//! Hardware-identity spoofing for the sandbox: synthetic /proc/cpuinfo,
//! /proc/stat, /proc/meminfo, /sys CPU topology and DMI board identity, plus
//! the device/socket masks that hide audio hardware. Split out of the launcher
//! so run/mod.rs holds orchestration rather than these self-contained overlays.
use crate::config::AppConfig;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) const CPUINFO_SAMPLE: &str = "\
processor\t: 0\n\
vendor_id\t: GenuineIntel\n\
cpu family\t: 6\n\
model\t\t: 142\n\
model name\t: Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz\n\
stepping\t: 10\n\
cpu MHz\t\t: 1992.000\n\
cache size\t: 8192 KB\n\
physical id\t: 0\n\
siblings\t: 4\n\
core id\t\t: 0\n\
cpu cores\t: 4\n\
fpu\t\t: yes\n\
fpu_exception\t: yes\n\
cpuid level\t: 22\n\
wp\t\t: yes\n\
flags\t\t: fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat pse36 clflush mmx fxsr sse sse2 ss ht syscall nx lm constant_tsc nopl xtopology nonstop_tsc pni pclmulqdq ssse3 fma cx16 sse4_1 sse4_2 x2apic movbe popcnt aes xsave avx f16c rdrand lahf_lm avx2 bmi1 bmi2 erms xsaveopt\n\
bogomips\t: 3984.00\n\
clflush size\t: 64\n\
cache_alignment\t: 64\n\
address sizes\t: 39 bits physical, 48 bits virtual\n\
power management:\n";

/// Kernel-style /proc/meminfo body for a fixed MemTotal (kB) and current
/// MemFree (kB).  Buffers/Cached/SReclaimable/Shmem stay at zero so tools
/// that derive `used = total - free - buffers - cached - sreclaimable + shmem`
/// (free, htop) land on `total - free`, matching the cgroup's memory.current.
pub(super) fn format_meminfo(total_kb: u64, free_kb: u64) -> String {
    format!(
        "MemTotal:       {total_kb} kB\n\
         MemFree:        {free_kb} kB\n\
         MemAvailable:   {free_kb} kB\n\
         Buffers:             0 kB\n\
         Cached:              0 kB\n\
         SwapCached:          0 kB\n\
         Active:              0 kB\n\
         Inactive:            0 kB\n\
         SwapTotal:           0 kB\n\
         SwapFree:            0 kB\n\
         Shmem:               0 kB\n\
         Slab:                0 kB\n\
         SReclaimable:        0 kB\n\
         SUnreclaim:          0 kB\n"
    )
}

/// Build a synthetic `/proc/stat` exposing exactly `threads` logical CPUs, so
/// tools that count per-CPU lines (htop) report the spoofed number. The
/// aggregate `cpu` counters and every non-CPU line are copied from the real
/// file; each fake `cpuN` line reuses a real per-CPU sample so idle/used ratios
/// look plausible. The values are a static snapshot, so live per-core usage
/// bars read as flat — only the CPU *count* is spoofed.
/// Render a spoofed `/proc/stat` for a machine with `threads` logical CPUs.
///
/// The container's first N per-CPU lines mirror the host's real N cores 1:1, so
/// their busy/idle counters carry the host's actual usage; any surplus cores
/// (when spoofing *up*) cycle back through the real cores so every meter still
/// shows live activity. Because the counters are copied fresh from the host on
/// each call, a periodic rewrite makes tools like htop compute correct per-core
/// usage deltas.
pub(super) fn spoof_proc_stat(threads: u32) -> String {
    let real = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let mut agg: Option<String> = None;
    let mut per_cpu: Vec<String> = Vec::new(); // counter fields of real cpu0, cpu1, …
    let mut tail: Vec<String> = Vec::new();
    for line in real.lines() {
        if let Some(rest) = line.strip_prefix("cpu") {
            if rest.starts_with(|c: char| c.is_whitespace()) {
                agg = Some(line.to_string());
            } else if rest.starts_with(|c: char| c.is_ascii_digit()) {
                if let Some((_, v)) = line.split_once(|c: char| c.is_whitespace()) {
                    per_cpu.push(v.trim().to_string());
                }
            } else {
                tail.push(line.to_string());
            }
        } else {
            tail.push(line.to_string());
        }
    }
    let agg = agg.unwrap_or_else(|| "cpu  0 0 0 0 0 0 0 0 0 0".to_string());
    if per_cpu.is_empty() {
        per_cpu.push("0 0 0 0 0 0 0 0 0 0".to_string());
    }
    let realn = per_cpu.len();

    let mut out = String::new();
    out.push_str(&agg);
    out.push('\n');
    for i in 0..threads as usize {
        out.push_str(&format!("cpu{i} {}\n", per_cpu[i % realn]));
    }
    for line in &tail {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The number of logical CPUs the spoofed `/proc/cpuinfo` presents, or None when
/// no CPU spoofing is configured. It counts the actual `processor` blocks, so it
/// works uniformly for presets, custom CPUs, the raw-editor file, and a file
/// path — not just the values `topology_for` knows about.
pub(super) fn spoofed_thread_count(config: &AppConfig, spoof_dir: &Path) -> Option<u32> {
    let spec = config.spoof_cpuinfo.as_deref()?;
    let text = if spec == "sample" {
        CPUINFO_SAMPLE.to_string()
    } else if spec == "custom" {
        std::fs::read_to_string(spoof_dir.join("cpuinfo")).ok()?
    } else if let Some(t) = crate::cpu::cpuinfo_for(spec) {
        t
    } else {
        std::fs::read_to_string(spec).ok()?
    };
    let n = text.lines().filter(|l| l.starts_with("processor")).count() as u32;
    (n >= 1).then_some(n)
}

/// Rewrite the spoofed `/proc/stat` at `path` from the live host stats every
/// ~500 ms until `stop` is set, so per-CPU usage stays current in the sandbox.
pub(super) fn proc_stat_updater_loop(path: PathBuf, threads: u32, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::write(&path, spoof_proc_stat(threads));
        for _ in 0..5 {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// Count how many `cpuN` directories the host exposes under
/// `/sys/devices/system/cpu` (i.e. the real logical CPU count).
pub(super) fn host_cpu_dir_count() -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir("/sys/devices/system/cpu") {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("cpu") {
                if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                    n += 1;
                }
            }
        }
    }
    n.max(1)
}

/// Overlay `/sys/devices/system/cpu` so tools that count CPUs from the kernel's
/// sysfs see the spoofed number. `online`/`present`/`possible` are overridden
/// with the spoofed range (fixes sysconf/get_nprocs), and — because many tools
/// (fastfetch, some htop builds) count the `cpuN` *directories* — the directory
/// set is rebuilt to hold exactly `threads` of them. That needs a tmpfs, since
/// bwrap can't mkdir inside the read-only `/sys` bind, so the real entries
/// (cpufreq, cpuidle, real cpuN, …) are bound back on top of it.
pub(super) fn spoof_sys_cpu(cmd: &mut Command, spoof_dir: &Path, threads: u32) {
    let threads = threads.max(1) as usize;
    let real = host_cpu_dir_count();
    let base = "/sys/devices/system/cpu";

    // Only rebuild the directory set when the count actually differs.
    if threads != real {
        cmd.args(["--tmpfs", base]);
        // Re-bind every real entry back, except the surplus cpuN dirs when
        // spoofing *down*, and the online/present/possible files we override.
        if let Ok(rd) = std::fs::read_dir(base) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if matches!(name.as_ref(), "online" | "present" | "possible" | "offline") {
                    continue;
                }
                if let Some(rest) = name.strip_prefix("cpu") {
                    // Hide real CPUs beyond a spoofed-down count.
                    if !rest.is_empty()
                        && rest.bytes().all(|b| b.is_ascii_digit())
                        && rest.parse::<usize>().map(|n| n >= threads).unwrap_or(false)
                    {
                        continue;
                    }
                }
                let p = format!("{base}/{name}");
                cmd.args(["--ro-bind-try", &p, &p]);
            }
        }
        // Minimal fake cpuN dirs for the surplus when spoofing *up*. They mirror
        // a real CPU's cpufreq base_frequency so tools that group CPUs by
        // frequency (fastfetch's "core types") count all of them as one type
        // rather than only the real ones that carry cpufreq data.
        if threads > real {
            let fake = spoof_dir.join("fakecpu");
            let _ = std::fs::create_dir_all(fake.join("topology"));
            let _ = std::fs::create_dir_all(fake.join("cpufreq"));
            let _ = std::fs::write(fake.join("online"), "1\n");
            let _ = std::fs::write(fake.join("topology").join("core_id"), "0\n");
            let _ = std::fs::write(fake.join("topology").join("physical_package_id"), "0\n");
            // Copy the host's frequency figures so the fake CPUs look identical
            // to the real ones to frequency-grouping detectors.
            for f in ["base_frequency", "cpuinfo_max_freq", "cpuinfo_min_freq",
                      "scaling_max_freq", "scaling_min_freq", "scaling_cur_freq"] {
                if let Ok(v) = std::fs::read_to_string(format!("{base}/cpu0/cpufreq/{f}")) {
                    let _ = std::fs::write(fake.join("cpufreq").join(f), v);
                }
            }
            if let Some(fs) = fake.to_str() {
                for n in real..threads {
                    cmd.args(["--ro-bind", fs, &format!("{base}/cpu{n}")]);
                }
            }
        }

        // Frequency "policy" dirs: fastfetch (and similar) count CPUs by the
        // per-CPU cpufreq policies (each governs one CPU via affected_cpus), so
        // rebuild the policy set to the spoofed size too.
        let cpufreq = format!("{base}/cpufreq");
        if std::path::Path::new(&cpufreq).is_dir() {
            cmd.args(["--tmpfs", &cpufreq]);
            // Re-bind real entries, dropping surplus real policies when down.
            if let Ok(rd) = std::fs::read_dir(&cpufreq) {
                for e in rd.flatten() {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if let Some(rest) = name.strip_prefix("policy") {
                        if !rest.is_empty()
                            && rest.bytes().all(|b| b.is_ascii_digit())
                            && rest.parse::<usize>().map(|n| n >= threads).unwrap_or(false)
                        {
                            continue;
                        }
                    }
                    let p = format!("{cpufreq}/{name}");
                    cmd.args(["--ro-bind-try", &p, &p]);
                }
            }
            // One fake policy per surplus CPU, cloned from a real policy but with
            // its own affected/related CPU so detectors count them individually.
            if threads > real {
                let src = format!("{cpufreq}/policy0");
                const FREQ_FILES: &[&str] = &[
                    "base_frequency", "cpuinfo_max_freq", "cpuinfo_min_freq",
                    "scaling_max_freq", "scaling_min_freq", "scaling_cur_freq",
                    "scaling_governor", "scaling_driver",
                ];
                let snapshot: Vec<(&str, String)> = FREQ_FILES.iter()
                    .filter_map(|f| std::fs::read_to_string(format!("{src}/{f}")).ok().map(|v| (*f, v)))
                    .collect();
                let pol_root = spoof_dir.join("policies");
                for n in real..threads {
                    let d = pol_root.join(format!("policy{n}"));
                    if std::fs::create_dir_all(&d).is_err() {
                        continue;
                    }
                    let _ = std::fs::write(d.join("affected_cpus"), format!("{n}\n"));
                    let _ = std::fs::write(d.join("related_cpus"), format!("{n}\n"));
                    for (f, v) in &snapshot {
                        let _ = std::fs::write(d.join(f), v);
                    }
                    if let Some(s) = d.to_str() {
                        cmd.args(["--ro-bind", s, &format!("{cpufreq}/policy{n}")]);
                    }
                }
            }
        }
    }

    // Spoofed online/present/possible ranges (bound onto the tmpfs above, or
    // straight onto the real /sys when the count matched and no tmpfs was used).
    let range = if threads <= 1 { "0".to_string() } else { format!("0-{}", threads - 1) };
    for leaf in &["online", "present", "possible"] {
        let f = spoof_dir.join(format!("cpu-{leaf}"));
        if std::fs::write(&f, format!("{range}\n")).is_ok() {
            if let Some(s) = f.to_str() {
                cmd.args(["--ro-bind-try", s, &format!("{base}/{leaf}")]);
            }
        }
    }
}

/// Overlay the sandbox's DMI/SMBIOS identity (`/sys/devices/virtual/dmi/id/*`)
/// so board-reading tools present `dmi` instead of the real mainboard. Only the
/// world-readable identity files are touched, and each is overlaid only if it
/// actually exists on the host — binding over a missing path in the read-only
/// `/sys` would abort the whole sandbox. `/sys/class/dmi/id` is a symlink to
/// this directory, so tools reaching either path see the fakes.
pub(super) fn spoof_dmi(cmd: &mut Command, spoof_dir: &Path, dmi: &crate::cpu::DmiInfo) {
    let dmi_dir = spoof_dir.join("dmi");
    let _ = std::fs::create_dir_all(&dmi_dir);
    let base = "/sys/devices/virtual/dmi/id";
    let fields = [
        ("sys_vendor", dmi.sys_vendor.as_str()),
        ("product_name", dmi.product_name.as_str()),
        ("product_version", dmi.product_version.as_str()),
        ("product_family", dmi.product_family.as_str()),
        ("board_vendor", dmi.board_vendor.as_str()),
        ("board_name", dmi.board_name.as_str()),
        ("board_version", dmi.board_version.as_str()),
    ];
    for (name, val) in fields {
        let dest = format!("{base}/{name}");
        // Overlay even empty values (to hide the real entry), but only where the
        // host already exposes the file.
        if !Path::new(&dest).exists() {
            continue;
        }
        let src = dmi_dir.join(name);
        // sysfs DMI files are newline-terminated.
        if std::fs::write(&src, format!("{val}\n")).is_ok() {
            if let Some(s) = src.to_str() {
                cmd.args(["--ro-bind", s, &dest]);
            }
        }
    }
}

/// Locate the cgroup memory.current file for `pid` (a systemd-run --scope
/// child).  systemd-run moves itself into the new scope only after talking to
/// systemd, so the process spends a brief window in the parent's cgroup.
/// Accept only a cgroup whose `memory.max` equals the configured limit — that
/// unambiguously identifies our scope and rejects the outer session's cgroup.
pub(super) fn find_scope_memory_current(pid: u32, kib: u64) -> Option<PathBuf> {
    let want_max = kib.saturating_mul(1024);
    for _ in 0..40 {
        if let Ok(content) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("0::") {
                    let dir = Path::new("/sys/fs/cgroup")
                        .join(rest.trim().trim_start_matches('/'));
                    let max = std::fs::read_to_string(dir.join("memory.max"))
                        .ok()
                        .and_then(|s| s.trim().parse::<u64>().ok());
                    if max == Some(want_max) {
                        let mem = dir.join("memory.current");
                        if mem.exists() {
                            return Some(mem);
                        }
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

/// Poll the cgroup's memory.current and rewrite the bind-mounted meminfo file
/// so sandboxed tools see MemFree shrink as the app allocates.  Exits when
/// `stop` is set or the memory file disappears (scope ended).
pub(super) fn meminfo_updater_loop(
    pid: u32,
    kib: u64,
    meminfo_path: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let Some(mem_current) = find_scope_memory_current(pid, kib) else { return };
    let total_kb = kib; // /proc/meminfo counts in KiB, which is our unit
    while !stop.load(Ordering::Relaxed) {
        let Ok(s) = std::fs::read_to_string(&mem_current) else { break };
        if let Ok(used_bytes) = s.trim().parse::<u64>() {
            let used_kb = used_bytes / 1024;
            let free_kb = total_kb.saturating_sub(used_kb);
            let _ = std::fs::write(&meminfo_path, format_meminfo(total_kb, free_kb));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Bind /dev/null over every file in `dir` whose name starts with `prefix`.
pub(super) fn mask_dev_prefix(cmd: &mut Command, dir: &str, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) {
            let path = format!("{dir}/{name}");
            cmd.args(["--bind", "/dev/null", &path]);
        }
    }
}

/// Bind /dev/null over ALSA sound devices.
/// If `capture_suffix` is Some('c'), only mask capture devices (pcmC*D*c).
pub(super) fn mask_snd_devices(cmd: &mut Command, capture_only: Option<char>) {
    let Ok(entries) = std::fs::read_dir("/dev/snd") else { return };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let matches = match capture_only {
            Some(suffix) => name.ends_with(suffix),
            None => true,
        };
        if matches {
            let path = format!("/dev/snd/{name}");
            cmd.args(["--bind", "/dev/null", &path]);
        }
    }
}

/// Bind /dev/null over PipeWire and PulseAudio sockets in XDG_RUNTIME_DIR.
pub(super) fn mask_audio_sockets(cmd: &mut Command) {
    let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") else { return };
    for name in &["pipewire-0", "pipewire-0.lock", "pulse/native"] {
        let path = format!("{xdg}/{name}");
        if std::path::Path::new(&path).exists() {
            cmd.args(["--bind", "/dev/null", &path]);
        }
    }
}
