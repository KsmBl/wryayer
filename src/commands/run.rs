use crate::commands::install::run_ldconfig;
use crate::config::{read_config, AppConfig, AvahiMode, LocalDelete, TempMode};
use crate::manifest::{app_dir, read_manifest};
use crate::package::{download_official, extract_package, find_missing_sonames_in};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const CPUINFO_SAMPLE: &str = "\
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

pub fn run(app_name: &str, bin: Option<&str>, args: &[String]) -> Result<()> {
    // Strip a leading "--" separator (e.g. `wryayer run firefox -- file.pdf`)
    let args = match args {
        [first, rest @ ..] if first == "--" => rest,
        other => other,
    };

    let manifest = read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    // Aliases (created by `install --into`) carry the binaries' real location
    // in `alias_of`. The alias has its own config and launchers list; the
    // filesystem tree (bwrap root) belongs to the target.
    let fs_root_name = manifest.app.alias_of.clone().unwrap_or_else(|| app_name.to_string());
    let app_root = app_dir(&fs_root_name)?;
    if !app_root.exists() {
        bail!(
            "app directory missing: {} (target of alias '{app_name}')",
            app_root.display(),
        );
    }
    let config = read_config(app_name)?;

    // Wine games override the normal binary lookup: they always launch
    // /usr/bin/wine with the .exe path prepended to the user's args, and
    // they need WINEPREFIX + chdir set on the bwrap command.
    let wine_ctx: Option<WineCtx> = manifest.app.wine_game.as_ref().map(|wg| {
        let chdir = std::path::Path::new(&wg.exe)
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        WineCtx {
            exe: wg.exe.clone(),
            prefix: wg.prefix.clone(),
            chdir,
        }
    });

    // bin override: must be one of the app's registered launchers, otherwise
    // anyone could trick `wryayer run` into invoking arbitrary binaries.
    // Wine games skip this — they have a fixed entry point (wine).
    let bin_name = if wine_ctx.is_some() {
        "wine".to_string()
    } else {
        match bin {
            None => manifest.app.main_binary.clone(),
            Some(b) => {
                if !manifest.app.launchers.iter().any(|l| l == b) {
                    bail!(
                        "binary '{b}' is not registered for {app_name} (launchers: {})",
                        manifest.app.launchers.join(", ")
                    );
                }
                b.to_string()
            }
        }
    };
    const BIN_DIRS: &[&str] = &["usr/bin", "usr/sbin", "bin", "sbin"];
    let binary = BIN_DIRS
        .iter()
        .find(|sub| app_root.join(sub).join(&bin_name).symlink_metadata().is_ok())
        .map(|sub| format!("/{sub}/{bin_name}"))
        .with_context(|| {
            if wine_ctx.is_some() {
                format!(
                    "wine binary not found in {} — install wine into the container first \
                     (wryayer install wine --into {})",
                    app_root.display(),
                    manifest.app.alias_of.as_deref().unwrap_or(app_name),
                )
            } else {
                format!(
                    "binary '{bin_name}' not found in usr/bin, usr/sbin, bin, or sbin inside {}",
                    app_root.display()
                )
            }
        })?;

    // AppImage packages ship a shell wrapper the extracted tree can't run (no
    // interpreter, no FUSE). Redirect the launch straight at the bundled
    // AppImage when that's the case. Wine games have their own fixed entry.
    let (binary, appimage_env) = if wine_ctx.is_none() {
        resolve_appimage_wrapper(&app_root, &binary).unwrap_or((binary, Vec::new()))
    } else {
        (binary, Vec::new())
    };

    let app_root_str = app_root.to_string_lossy().into_owned();

    // Pre-launch: repair any missing sonames in the sandbox home/ tree.
    // Catches second-and-later launches after a self-updating app (e.g.
    // Discord) already wrote its downloaded binary to home/.config/... during
    // a previous session.
    fix_home_sonames(&app_root);

    // Apps that probe zeroconf (Electron/Chromium, KDE, CUPS-linked, etc.) print
    // "Failed to connect to Avahi server: Daemon not running" when no Avahi is
    // reachable. The default `stub` mode answers them from a private in-sandbox
    // bus (set up in bwrap_cmd) with no host change; `host` starts the real
    // daemon; `off` leaves the harmless warning. Only the host path acts here.
    if config.network && config.avahi == AvahiMode::Host {
        ensure_avahi_daemon();
    }

    let (temp, cleanup) = prepare_temp(&config, &app_root)?;

    // For wine games, prepend the .exe path to the user-supplied args. The
    // user-supplied args become trailing wine args (typically empty, but the
    // /run subcommand accepts them).
    let effective_args: Vec<String> = match &wine_ctx {
        Some(w) => {
            let mut v = vec![w.exe.clone()];
            v.extend(args.iter().cloned());
            v
        }
        None => args.to_vec(),
    };

    let status = launch_bwrap(&app_root_str, &binary, &effective_args, &temp, &config, wine_ctx.as_ref(), &appimage_env)?;

    // Post-launch: if bwrap exited abnormally, the app may have written a new
    // self-updated ELF binary (e.g. Discord bootstrapping app-X.Y.Z/Discord)
    // that needs libraries not yet in the sandbox tree.  Run the home/ scan
    // again now that those binaries exist, install any missing packages, and
    // retry automatically so the user doesn't have to re-launch manually.
    let repaired = !status.success() && fix_home_sonames(&app_root);

    if let Some(cleanup_path) = cleanup {
        if repaired {
            let _ = launch_bwrap(&app_root_str, &binary, &effective_args, &temp, &config, wine_ctx.as_ref(), &appimage_env);
        }
        let _ = std::fs::remove_dir_all(&cleanup_path);
        std::process::exit(status.code().unwrap_or(1));
    } else if repaired {
        // Replace this process with a fresh bwrap so the retry gets a clean
        // exec() hand-off (correct signal disposition, no extra wryayer in the
        // process tree). The dbus proxy carries PR_SET_PDEATHSIG, so it dies
        // with the app.
        let (mut cmd, _, _dbus, _avahi) = bwrap_cmd(&app_root_str, &binary, &effective_args, &temp, &config, wine_ctx.as_ref(), &appimage_env);
        set_bwrap_env(&mut cmd);
        if let Some(mib) = config.ram_limit {
            if has_systemd_run() {
                cmd = wrap_with_ram_limit(cmd, mib);
            }
        }
        let err = cmd.exec();
        bail!("failed to exec bwrap: {err}");
    } else {
        std::process::exit(status.code().unwrap_or(0));
    }
}

/// Start avahi-daemon if it's installed but not currently running, so sandboxed
/// apps that query Avahi over the system bus don't fail with "Daemon not
/// running". avahi-client prints that same message whether the daemon is absent
/// or the system bus is unreachable, so the only way to silence it is a live
/// daemon. Entirely best-effort: if the unit is missing, already active, or the
/// user's polkit rules don't permit an unprivileged start, we leave things as
/// they are and the harmless warning simply remains.
fn ensure_avahi_daemon() {
    // Nothing to start if the service isn't installed.
    if !Path::new("/usr/lib/systemd/system/avahi-daemon.service").exists() {
        return;
    }
    // Skip if it's already running (the common case after the first launch).
    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", "avahi-daemon"])
        .status();
    if matches!(active, Ok(s) if s.success()) {
        return;
    }
    // Try an unprivileged start first (many desktops authorize this via polkit),
    // then fall back to a non-interactive sudo in case the user has cached
    // credentials. Both are best-effort; failures are ignored on purpose.
    let started = Command::new("systemctl")
        .args(["start", "avahi-daemon"])
        .status();
    if matches!(started, Ok(s) if s.success()) {
        return;
    }
    let _ = Command::new("sudo")
        .args(["-n", "systemctl", "start", "avahi-daemon"])
        .status();
}

struct WineCtx {
    /// Absolute path inside the sandbox to the .exe wine should launch.
    exe: String,
    /// Absolute path inside the sandbox where WINEPREFIX should be set.
    prefix: String,
    /// Optional --chdir target inside the sandbox (usually the .exe's dir).
    chdir: Option<String>,
}

fn launch_bwrap(
    app_root_str: &str,
    binary: &str,
    args: &[String],
    temp: &TempBind,
    config: &AppConfig,
    wine: Option<&WineCtx>,
    appimage_env: &[(String, String)],
) -> Result<ExitStatus> {
    let (mut cmd, spoof_dir, mut dbus_proxy, mut avahi_stub) = bwrap_cmd(app_root_str, binary, args, temp, config, wine, appimage_env);
    set_bwrap_env(&mut cmd);
    let ram_mib = if let Some(mib) = config.ram_limit {
        if has_systemd_run() {
            cmd = wrap_with_ram_limit(cmd, mib);
            Some(mib)
        } else {
            eprintln!("warning: systemd-run not found — running without RAM limit");
            None
        }
    } else {
        None
    };

    // When systemd-run wraps the command we can track the scope's cgroup
    // memory.current and rewrite the sandbox's /proc/meminfo so MemFree
    // shrinks with real usage.  Needs a live parent, so it's a spawn+wait
    // pair rather than a blocking status() call.
    let meminfo_path = ram_mib.map(|_| Path::new(app_root_str).join(".spoof").join("meminfo"));
    let mut child = cmd.spawn().context("failed to run bwrap")?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let updater = match (ram_mib, meminfo_path) {
        (Some(mib), Some(path)) if path.exists() => {
            let pid = child.id();
            let stop_clone = stop.clone();
            Some(std::thread::spawn(move || {
                meminfo_updater_loop(pid, mib, path, stop_clone);
            }))
        }
        _ => None,
    };
    let status = child.wait().context("failed to wait for bwrap")?;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(u) = updater {
        let _ = u.join();
    }
    // The dbus proxy has no self-terminate flag; stop it now that the app is gone.
    if let Some(ref mut child) = dbus_proxy {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Tear down the Avahi stub (and, via PDEATHSIG, its private dbus-daemon).
    if let Some(ref mut child) = avahi_stub {
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(dir) = spoof_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(status)
}

fn set_bwrap_env(cmd: &mut Command) {
    cmd.env("FONTCONFIG_CACHE", "/tmp/.wryayer-fc-cache");
    // Chromium-based renderers (QtWebEngine, Electron) crash inside bwrap because
    // their subprocesses cannot set up their own namespace sandboxes.
    // --no-sandbox: skip Chromium's internal sandbox
    // --no-zygote:  skip the zygote fork relay (also tries sandbox setup)
    // --in-process-gpu: run GPU code in the browser process instead of a
    //   separate GPU subprocess; the GPU subprocess fails to init EGL because
    //   the sandbox app root may not have mesa, while the main browser process
    //   already has working OpenGL (confirmed by Qt's OpenGL init log).
    cmd.env("QTWEBENGINE_CHROMIUM_FLAGS", "--no-sandbox --no-zygote --in-process-gpu --single-process --disable-gpu");
    cmd.env("ELECTRON_DISABLE_SANDBOX", "1");
}

// ── Temp handling ─────────────────────────────────────────────────────────────

enum TempBind {
    System,
    Tmpfs,
    Dir(PathBuf),
}

/// Returns the temp binding and, if cleanup is needed, the path to remove on exit.
fn prepare_temp(config: &AppConfig, app_root: &Path) -> Result<(TempBind, Option<PathBuf>)> {
    match config.temp_mode {
        TempMode::System => Ok((TempBind::System, None)),

        TempMode::Ramdisk => Ok((TempBind::Tmpfs, None)),

        TempMode::Local => {
            let tmp_dir = app_root.join(".tmp");
            match config.temp_delete {
                LocalDelete::Never => {
                    std::fs::create_dir_all(&tmp_dir)
                        .context("failed to create local temp dir")?;
                    Ok((TempBind::Dir(tmp_dir), None))
                }
                LocalDelete::OnStart => {
                    let pid_file = app_root.join(".instance.pid");
                    if no_other_instance(&pid_file) {
                        let _ = std::fs::remove_dir_all(&tmp_dir);
                    }
                    std::fs::create_dir_all(&tmp_dir)
                        .context("failed to create local temp dir")?;
                    // Write PID before exec — exec replaces the process image but
                    // keeps the same PID, so the file stays valid for bwrap's lifetime.
                    let _ = std::fs::write(&pid_file, std::process::id().to_string());
                    Ok((TempBind::Dir(tmp_dir), None))
                }
                LocalDelete::OnClose => {
                    std::fs::create_dir_all(&tmp_dir)
                        .context("failed to create local temp dir")?;
                    Ok((TempBind::Dir(tmp_dir.clone()), Some(tmp_dir)))
                }
            }
        }

        TempMode::Uuid => {
            let uuid = kernel_uuid();
            let tmp_dir = app_root.join(".tmp").join(&uuid);
            std::fs::create_dir_all(&tmp_dir)
                .context("failed to create uuid temp dir")?;
            Ok((TempBind::Dir(tmp_dir.clone()), Some(tmp_dir)))
        }
    }
}

pub fn no_other_instance(pid_file: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(pid_file) else {
        return true;
    };
    let Ok(pid) = content.trim().parse::<u32>() else {
        return true;
    };
    !Path::new(&format!("/proc/{pid}")).exists()
}

fn kernel_uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .unwrap_or_default()
        .trim()
        .to_string()
}

// ── RAM limit helpers ─────────────────────────────────────────────────────────

pub fn has_systemd_run() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("systemd-run").exists()))
        .unwrap_or(false)
}

/// Wrap `inner` (a fully-constructed bwrap command) inside a transient
/// systemd scope unit with a MemoryMax cgroup limit.
///
/// `--scope` is used instead of a service unit because scope mode makes
/// systemd-run exec() the target directly (no fork).  The child process
/// therefore inherits the real PTY from the calling shell, so bash and other
/// interactive programs get a proper terminal and full job control.  A service
/// unit separates the process from the TTY and requires --pipe / --setenv
/// workarounds that still don't give the child a real PTY.
pub fn wrap_with_ram_limit(inner: Command, kib: u64) -> Command {
    let mut outer = Command::new("systemd-run");
    outer.arg("--user")
         .arg("--scope")
         .arg("--quiet")
         .arg("-p").arg(format!("MemoryMax={kib}K"))
         .arg("-p").arg("MemorySwapMax=0")
         .arg("--");
    outer.arg(inner.get_program());
    for arg in inner.get_args() {
        outer.arg(arg);
    }
    for (k, v) in inner.get_envs() {
        match v {
            Some(val) => { outer.env(k, val); }
            None      => { outer.env_remove(k); }
        }
    }
    outer
}

/// Kernel-style /proc/meminfo body for a fixed MemTotal (kB) and current
/// MemFree (kB).  Buffers/Cached/SReclaimable/Shmem stay at zero so tools
/// that derive `used = total - free - buffers - cached - sreclaimable + shmem`
/// (free, htop) land on `total - free`, matching the cgroup's memory.current.
fn format_meminfo(total_kb: u64, free_kb: u64) -> String {
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

/// Locate the cgroup memory.current file for `pid` (a systemd-run --scope
/// child).  systemd-run moves itself into the new scope only after talking to
/// systemd, so the process spends a brief window in the parent's cgroup.
/// Accept only a cgroup whose `memory.max` equals the configured limit — that
/// unambiguously identifies our scope and rejects the outer session's cgroup.
fn find_scope_memory_current(pid: u32, kib: u64) -> Option<PathBuf> {
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
fn meminfo_updater_loop(
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

/// Map a shared-dir path to the XDG role its basename represents, so a
/// synthetic user-dirs.dirs can list only the shared roles.  Unshared roles
/// disappear from the file-picker sidebar rather than appearing as broken
/// clickable shortcuts.
fn xdg_role_for_dir(path: &str) -> Option<&'static str> {
    let name = std::path::Path::new(path).file_name()?.to_str()?;
    match name {
        "Desktop"   => Some("XDG_DESKTOP_DIR"),
        "Downloads" => Some("XDG_DOWNLOAD_DIR"),
        "Documents" => Some("XDG_DOCUMENTS_DIR"),
        "Music"     => Some("XDG_MUSIC_DIR"),
        "Pictures"  => Some("XDG_PICTURES_DIR"),
        "Videos"    => Some("XDG_VIDEOS_DIR"),
        "Templates" => Some("XDG_TEMPLATES_DIR"),
        "Public"    => Some("XDG_PUBLICSHARE_DIR"),
        _ => None,
    }
}

/// Spawn an xdg-dbus-proxy that mirrors the host session bus at `socket_path`
/// with the desktop portal filtered out.  In `--filter` mode the proxy's
/// default policy makes every name invisible, so we allow-list the session
/// services sandboxed GUI apps commonly use and simply never grant the portal
/// names — leaving `org.freedesktop.portal.*` unreachable.  With no visible
/// portal, GTK/Qt/Firefox/Chromium fall back to their in-sandbox file choosers,
/// which honour the XDG overlays and can only browse mounted dirs.
///
/// The proxy is given PR_SET_PDEATHSIG so it dies with its parent even on the
/// exec() retry path where nobody is left to kill it explicitly.
fn spawn_dbus_proxy(host_bus: &str, socket_path: &str) -> Option<std::process::Child> {
    // A stale socket from a previous run would make the proxy's bind() fail.
    let _ = std::fs::remove_file(socket_path);

    let mut proxy = Command::new("xdg-dbus-proxy");
    proxy.arg(host_bus).arg(socket_path).arg("--filter");
    for name in &[
        "org.freedesktop.Notifications",         // desktop notifications
        "org.freedesktop.secrets",               // keyring (saved passwords)
        "org.freedesktop.ScreenSaver",           // inhibit idle during playback
        "org.freedesktop.PowerManagement",       // ditto, older spec
        "org.freedesktop.FileManager1",          // "show in file manager"
        "org.a11y.Bus",                          // accessibility bridge
        "org.kde.StatusNotifierWatcher",         // tray icons
        "org.freedesktop.StatusNotifierWatcher",
        "ca.desrt.dconf",                        // GSettings/dconf backend
        "org.gtk.vfs.*",                         // GVFS mounts
    ] {
        proxy.arg(format!("--talk={name}"));
    }
    // Apps register their own MPRIS name to expose media controls.
    proxy.arg("--own=org.mpris.MediaPlayer2.*");
    // Steam's pressure-vessel launcher-service owns names under its own
    // namespace so it can place launched games into the right runtime; without
    // this it crash-loops ("Unable to acquire bus name …") and Steam disables
    // it, breaking game launches. These are Steam's own names, not the portal.
    proxy.arg("--own=com.steampowered.*");

    proxy
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        proxy.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }

    let mut child = proxy.spawn().ok()?;

    // Wait (up to ~1s) for the proxy socket to appear before bwrap binds it.
    for _ in 0..40 {
        if std::path::Path::new(socket_path).exists() {
            return Some(child);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    // Never came up — don't point the sandbox at a dead bus.
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Bring up the per-sandbox Avahi stub (see `avahi_stub.rs`): a private system
/// bus plus an in-process owner of `org.freedesktop.Avahi`, so avahi-client apps
/// don't fail with "Daemon not running" — without starting the host daemon or
/// putting anything on the network.  Returns the managed child (which owns the
/// dbus-daemon) and the host path of the bus socket, or None if it didn't come
/// up in time.
///
/// The bus socket, its dbus-daemon config, and the readiness marker all live in
/// the app's own `.spoof` dir under `~/.wryayer`, so nothing identifying is
/// written outside the container.  The child carries PR_SET_PDEATHSIG so it (and
/// its dbus-daemon) die with the sandbox even on the exec() retry path.
fn spawn_avahi_stub(spoof_dir: &Path) -> Option<(std::process::Child, String)> {
    // AF_UNIX socket paths are capped at ~108 bytes. A deeply nested app dir can
    // overflow that, and dbus-daemon then silently fails to bind, leaving the
    // stub disabled. When the in-container path is too long, fall back to a short
    // hashed name in the runtime dir (tmpfs — the name is a hash and the file is
    // ephemeral, so nothing identifying persists outside ~/.wryayer).
    let mut sock = spoof_dir.join(".avahi-bus");
    if sock.as_os_str().len() > 100 {
        let rt = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
        sock = PathBuf::from(rt).join(format!(".wrav-{:x}", short_hash(spoof_dir)));
    }
    let conf = spoof_dir.join(".avahi-bus.conf");
    let sock_str = sock.to_str()?.to_string();
    let conf_str = conf.to_str()?.to_string();
    let ready = format!("{sock_str}.ready");

    // A leftover socket makes dbus-daemon's bind() fail; a leftover marker would
    // make us treat the bus as up before it is.
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&ready);

    let config = format!(
        "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
           \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
         <busconfig>\n\
         \x20 <type>system</type>\n\
         \x20 <listen>unix:path={sock_str}</listen>\n\
         \x20 <auth>EXTERNAL</auth>\n\
         \x20 <policy context=\"default\">\n\
         \x20   <allow user=\"*\"/>\n\
         \x20   <allow own=\"*\"/>\n\
         \x20   <allow send_type=\"method_call\"/>\n\
         \x20   <allow send_type=\"method_return\"/>\n\
         \x20   <allow send_type=\"error\"/>\n\
         \x20   <allow send_type=\"signal\"/>\n\
         \x20   <allow send_requested_reply=\"true\"/>\n\
         \x20   <allow receive_requested_reply=\"true\"/>\n\
         \x20   <allow receive_type=\"method_call\"/>\n\
         \x20   <allow receive_type=\"method_return\"/>\n\
         \x20   <allow receive_type=\"error\"/>\n\
         \x20   <allow receive_type=\"signal\"/>\n\
         \x20 </policy>\n\
         </busconfig>\n"
    );
    if std::fs::write(&conf, config).is_err() {
        return None;
    }

    let exe = std::env::current_exe().ok()?;
    let mut c = Command::new(exe);
    c.arg("avahi-stub").arg(&sock_str).arg(&conf_str);
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = c.spawn().ok()?;

    // Wait (up to ~3 s) for the stub to actually own the name — it writes the
    // marker only after RequestName returns — so the app never races an
    // unowned bus and sees a spurious "Daemon not running".
    for _ in 0..200 {
        if Path::new(&ready).exists() {
            return Some((child, sock_str));
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// A short, stable hash of a path — used to name a per-app socket without
/// exposing the app name.
fn short_hash(p: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// AUR `*-appimage` packages ship a tiny `#!/bin/sh` wrapper in `usr/bin` plus
/// the real `.AppImage` under `/opt`, but declare no shell (or FUSE)
/// dependency — so the extracted tree has no interpreter for the wrapper
/// (`execvp` → ENOENT, the "No such file or directory" launch failure) and no
/// FUSE for the AppImage to self-mount.
///
/// When the resolved launcher is such an unrunnable wrapper, point the launch
/// at the AppImage directly (a static-pie ELF that needs neither a shell nor,
/// with `APPIMAGE_EXTRACT_AND_RUN` set, FUSE) and carry over any `export
/// VAR=VALUE` the wrapper set. Returns `None` when `binary` isn't a shebang
/// wrapper whose interpreter is missing from the tree — i.e. leave normal
/// launchers untouched.
fn resolve_appimage_wrapper(app_root: &Path, binary: &str) -> Option<(String, Vec<(String, String)>)> {
    let wrapper = app_root.join(binary.trim_start_matches('/'));
    let bytes = std::fs::read(&wrapper).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let shebang = text.strip_prefix("#!")?.lines().next()?;
    let interp = shebang.split_whitespace().next().unwrap_or("");
    // If the interpreter is present inside the tree the wrapper runs fine as-is.
    if interp.is_empty()
        || app_root
            .join(interp.trim_start_matches('/'))
            .symlink_metadata()
            .is_ok()
    {
        return None;
    }

    // Interpreter missing: pull the AppImage path and any exported env out of
    // the wrapper, falling back to a tree scan if the script doesn't name it.
    let mut env = Vec::new();
    let mut appimage = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some((k, v)) = rest.split_once('=') {
                let v = v.trim().trim_matches(['"', '\'']);
                // Skip values that reference other shell vars — we can't expand
                // them here and a literal `$FOO` would only mislead the app.
                if !v.contains('$') {
                    env.push((k.trim().to_string(), v.to_string()));
                }
            }
        }
        for tok in line.split_whitespace() {
            let tok = tok.trim_matches(['"', '\'']);
            if tok.ends_with(".AppImage") {
                appimage = Some(tok.to_string());
            }
        }
    }

    let appimage = match appimage {
        Some(p) if app_root.join(p.trim_start_matches('/')).is_file() => p,
        _ => find_appimage_in_tree(app_root)?,
    };
    let sandbox_path = format!("/{}", appimage.trim_start_matches('/').trim_start_matches("./"));
    Some((sandbox_path, env))
}

/// Recursively locate the first `*.AppImage` under the app tree (bounded depth
/// — these packages drop it in `/opt/<App>/…`). Used as a fallback when the
/// wrapper script doesn't spell out the AppImage path.
fn find_appimage_in_tree(app_root: &Path) -> Option<String> {
    fn walk(dir: &Path, root: &Path, depth: u32) -> Option<String> {
        if depth > 6 {
            return None;
        }
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            let ft = entry.file_type().ok()?;
            if ft.is_file() && path.extension().is_some_and(|e| e == "AppImage") {
                let rel = path.strip_prefix(root).ok()?;
                return Some(format!("/{}", rel.to_string_lossy()));
            }
            if ft.is_dir() {
                if let Some(hit) = walk(&path, root, depth + 1) {
                    return Some(hit);
                }
            }
        }
        None
    }
    walk(app_root, app_root, 0)
}

// ── bwrap command builder ─────────────────────────────────────────────────────


fn bwrap_cmd(app_root: &str, binary: &str, args: &[String], temp: &TempBind, config: &AppConfig, wine: Option<&WineCtx>, appimage_env: &[(String, String)]) -> (Command, Option<PathBuf>, Option<std::process::Child>, Option<std::process::Child>) {
    // Terminal spoofing: exec bwrap through a symlink named after the detected
    // terminal. Linux sets task->comm from the exec basename, so fastfetch's
    // process-tree walk sees the terminal name instead of "bwrap".
    let (bwrap_exe, term_spoof_dir): (PathBuf, Option<PathBuf>) = if config.spoof_terminal {
        match make_bwrap_spoof_exe() {
            Some((link, dir)) => (link, Some(dir)),
            None => (resolve_bwrap_path(), None),
        }
    } else {
        (PathBuf::from("bwrap"), None)
    };

    let mut cmd = Command::new(&bwrap_exe);

    cmd.args(["--bind", app_root, "/"]);
    cmd.args(["--dev-bind", "/dev", "/dev"]);
    cmd.args(["--proc", "/proc"]);
    cmd.args(["--ro-bind", "/sys", "/sys"]);

    match temp {
        TempBind::System  => { cmd.args(["--bind",  "/tmp", "/tmp"]); }
        TempBind::Tmpfs   => { cmd.args(["--tmpfs", "/tmp"]); }
        TempBind::Dir(d)  => { cmd.args(["--bind",  d.to_str().unwrap_or("/tmp"), "/tmp"]); }
    }

    cmd.args(["--bind", "/run", "/run"]);

    // No home binding by default — apps are fully isolated from the user's home.
    // Each entry in shared_dirs is bind-mounted read-write inside the sandbox.
    for dir in &config.shared_dirs {
        if std::path::Path::new(dir.as_str()).is_dir() {
            cmd.args(["--bind", dir.as_str(), dir.as_str()]);
        }
    }

    // /etc — networking, locale, identity, TLS
    for p in &[
        "/etc/resolv.conf", "/etc/hosts",      "/etc/localtime",
        "/etc/locale.conf", "/etc/machine-id", "/etc/nsswitch.conf",
        "/etc/passwd",      "/etc/group",       "/etc/ssl/certs",
        // On Arch Linux every file in /etc/ssl/certs/ is a symlink into
        // /etc/ca-certificates/extracted/ — bind the real tree so they resolve.
        "/etc/ca-certificates",
    ] {
        cmd.args(["--ro-bind-try", p, p]);
    }

    // Python's requests/certifi resolves its CA bundle through a symlink chain
    // that may escape the sandbox root.  Point all SSL env vars at the system
    // bundle; with /etc/ca-certificates bound above, the symlink resolves correctly.
    cmd.args(["--setenv", "SSL_CERT_FILE",      "/etc/ssl/certs/ca-certificates.crt"]);
    cmd.args(["--setenv", "REQUESTS_CA_BUNDLE", "/etc/ssl/certs/ca-certificates.crt"]);
    cmd.args(["--setenv", "CURL_CA_BUNDLE",     "/etc/ssl/certs/ca-certificates.crt"]);

    // Help Qt find its platform plugins when the sandbox root lacks the
    // compiled-in prefix; the host's plugin tree is already bound above.
    cmd.args(["--setenv", "QT_QPA_PLATFORM_PLUGIN_PATH", "/usr/lib/qt6/plugins/platforms"]);

    // Font directories — required by Chromium/NW.js/Electron/Qt renderers.
    // Without these, fontconfig finds no fonts and the renderer crashes with
    // FATAL:font_cache.cc Check failed: false + SEGV_MAPERR.
    for p in &["/usr/share/fonts", "/etc/fonts", "/usr/share/fontconfig"] {
        cmd.args(["--ro-bind-try", p, p]);
    }

    // Locale data — the compiled locale archive is generated by locale-gen on
    // the host, not shipped inside packages.  Without it the app's glibc falls
    // back to the "C" locale (ANSI_X3.4-1968), causing Qt/GTK locale warnings
    // and broken locale-sensitive formatting.
    for p in &["/usr/lib/locale", "/usr/share/locale"] {
        cmd.args(["--ro-bind-try", p, p]);
    }

    // Qt platform plugins — the sandbox root may not contain qt6-wayland even
    // when the host has it installed.  Bind the host plugin tree so Qt can find
    // whichever platform backend QT_QPA_PLATFORM requests.
    cmd.args(["--ro-bind-try", "/usr/lib/qt6/plugins", "/usr/lib/qt6/plugins"]);

    if !config.network {
        cmd.arg("--unshare-net");
    }

    // ── Device masking (later binds override the --dev-bind /dev /dev above) ──

    if !config.camera {
        mask_dev_prefix(&mut cmd, "/dev", "video");
        mask_dev_prefix(&mut cmd, "/dev", "media");
    }

    // audio=off blocks everything; microphone=off (with audio=on) blocks only
    // ALSA capture devices (PipeWire/PulseAudio mic is not separately blockable)
    if !config.audio {
        mask_snd_devices(&mut cmd, None);
        mask_audio_sockets(&mut cmd);
    } else if !config.microphone {
        // Only mask ALSA capture devices (names end in 'c', e.g. pcmC0D0c)
        mask_snd_devices(&mut cmd, Some('c'));
    }

    // ── Identity spoofing ─────────────────────────────────────────────────────────
    let spoof_dir = std::path::Path::new(app_root).join(".spoof");
    let _ = std::fs::create_dir_all(&spoof_dir);

    // ── Avahi stub ────────────────────────────────────────────────────────────────
    // Give the sandbox a private system bus answering org.freedesktop.Avahi so
    // avahi-client apps don't print "Daemon not running", without starting the
    // host daemon.  This --bind lands after the `--bind /run /run` above, so it
    // overrides the host system-bus socket the sandbox would otherwise inherit.
    let mut avahi_stub_child: Option<std::process::Child> = None;
    if config.network && config.avahi == AvahiMode::Stub {
        if let Some((child, sock)) = spawn_avahi_stub(&spoof_dir) {
            avahi_stub_child = Some(child);
            cmd.args(["--bind-try", &sock, "/run/dbus/system_bus_socket"]);
            cmd.args(["--setenv", "DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/run/dbus/system_bus_socket"]);
        }
    }

    if let Some(ref hostname) = config.spoof_hostname {
        // --unshare-uts gives the sandbox its own UTS namespace; --hostname sets
        // the kernel hostname within it.  gethostname() (used by fastfetch, bash's
        // \h prompt, etc.) reads from the kernel UTS namespace, NOT /etc/hostname,
        // so only this approach makes the spoof visible to those programs.
        cmd.args(["--unshare-uts", "--hostname", hostname.as_str()]);
        let hf = spoof_dir.join("hostname");
        let _ = std::fs::write(&hf, format!("{hostname}\n"));
        if let Some(s) = hf.to_str() {
            cmd.args(["--ro-bind-try", s, "/etc/hostname"]);
        }
        cmd.env("HOSTNAME", hostname);
    }

    if let Some(ref username) = config.spoof_username {
        cmd.env("USER", username);
        cmd.env("LOGNAME", username);

        // whoami(1) and getpwuid(3) read /etc/passwd directly — env vars alone
        // are not enough.  Patch /etc/passwd and /etc/group so that the current
        // UID/GID entries carry the spoofed name.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let uid_s = uid.to_string();
        let gid_s = gid.to_string();

        // Determine real username from passwd so we can also fix the group file.
        let real_name: String = std::fs::read_to_string("/etc/passwd")
            .unwrap_or_default()
            .lines()
            .find_map(|l| {
                let f: Vec<&str> = l.splitn(7, ':').collect();
                if f.len() >= 3 && f[2] == uid_s { Some(f[0].to_string()) } else { None }
            })
            .unwrap_or_default();

        if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
            let patched: String = passwd
                .lines()
                .map(|l| {
                    let mut f: Vec<&str> = l.splitn(7, ':').collect();
                    // Replace the username field for our UID; keep home dir as-is
                    // so $HOME still resolves correctly inside the sandbox.
                    if f.len() >= 3 && f[2] == uid_s {
                        f[0] = username.as_str();
                    }
                    f.join(":")
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            let pf = spoof_dir.join("passwd");
            if std::fs::write(&pf, patched).is_ok() {
                if let Some(s) = pf.to_str() {
                    cmd.args(["--ro-bind", s, "/etc/passwd"]);
                }
            }
        }

        if let Ok(group) = std::fs::read_to_string("/etc/group") {
            let patched: String = group
                .lines()
                .map(|l| {
                    let mut f: Vec<&str> = l.splitn(4, ':').collect();
                    if f.len() >= 3 {
                        // Rename the primary group (same GID) and any group
                        // whose name matches the real username (common on Linux).
                        let is_primary = f[2] == gid_s;
                        let is_user_group = !real_name.is_empty() && f[0] == real_name;
                        if is_primary || is_user_group {
                            f[0] = username.as_str();
                        }
                    }
                    f.join(":")
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            let gf = spoof_dir.join("group");
            if std::fs::write(&gf, patched).is_ok() {
                if let Some(s) = gf.to_str() {
                    cmd.args(["--ro-bind", s, "/etc/group"]);
                }
            }
        }
    }

    if let Some(ref machine_id) = config.spoof_machine_id {
        let id = if machine_id == "random" {
            kernel_uuid().replace('-', "")
        } else {
            machine_id.clone()
        };
        let mf = spoof_dir.join("machine-id");
        let _ = std::fs::write(&mf, format!("{id}\n"));
        if let Some(s) = mf.to_str() {
            cmd.args(["--ro-bind-try", s, "/etc/machine-id"]);
        }
    }

    if let Some(ref cpuinfo_path) = config.spoof_cpuinfo {
        if cpuinfo_path == "sample" {
            let cf = spoof_dir.join("cpuinfo");
            let _ = std::fs::write(&cf, CPUINFO_SAMPLE);
            if let Some(s) = cf.to_str() {
                cmd.args(["--ro-bind-try", s, "/proc/cpuinfo"]);
            }
        } else if cpuinfo_path == "custom" {
            // User-edited file written by the TUI's editor session.
            let cf = spoof_dir.join("cpuinfo");
            if cf.exists() {
                if let Some(s) = cf.to_str() {
                    cmd.args(["--ro-bind", s, "/proc/cpuinfo"]);
                }
            }
        } else if let Some(text) = crate::cpu::cpuinfo_for(cpuinfo_path) {
            // A built-in CPU preset ("preset:<key>") — render and bind it.
            let cf = spoof_dir.join("cpuinfo");
            let _ = std::fs::write(&cf, text);
            if let Some(s) = cf.to_str() {
                cmd.args(["--ro-bind-try", s, "/proc/cpuinfo"]);
            }
        } else {
            cmd.args(["--ro-bind-try", cpuinfo_path.as_str(), "/proc/cpuinfo"]);
        }
    }

    // ── RAM limit: fake /proc/meminfo ────────────────────────────────────────
    //
    // MemoryMax on the cgroup caps allocations, but /proc/meminfo is not
    // namespaced by the kernel — htop, free, and `sysinfo(2)`-based tools
    // still report host RAM.  Overlay a synthetic meminfo whose MemTotal is
    // the limit so the app sees the enforced ceiling as its total memory.
    // The initial contents assume zero usage; launch_bwrap starts an updater
    // thread that rewrites this file with the cgroup's live memory.current so
    // MemFree/MemAvailable shrink as the app allocates.
    let meminfo_file = spoof_dir.join("meminfo");
    if let Some(kib) = config.ram_limit {
        let mf = &meminfo_file;
        if std::fs::write(mf, format_meminfo(kib, kib)).is_ok() {
            if let Some(s) = mf.to_str() {
                cmd.args(["--ro-bind", s, "/proc/meminfo"]);
            }
        }
    } else {
        // No limit this launch: remove any overlay left behind by a previous
        // ram-limited run so the TUI doesn't keep reporting a phantom RAM cap.
        let _ = std::fs::remove_file(&meminfo_file);
    }

    // ── XDG file-picker filtering ────────────────────────────────────────────
    //
    // GTK/Qt file choosers build their "Places" sidebar from
    // $HOME/.config/user-dirs.dirs (via g_get_user_special_dir), and file
    // managers pull sidebar entries from gtk-*/bookmarks and recently-used.xbel.
    // Without an overlay these files can leak host paths — Pictures, Music,
    // Videos — even though the sandbox itself doesn't bind them, so the picker
    // shows clickable shortcuts that fail with ENOENT when clicked.  Write a
    // synthetic user-dirs.dirs listing only shared_dirs whose basename matches
    // an XDG role, and blank the other sources.
    if let Ok(home) = std::env::var("HOME") {
        let mut ud = String::from("# generated by wryayer\n");
        for dir in &config.shared_dirs {
            if let Some(role) = xdg_role_for_dir(dir) {
                ud.push_str(&format!("{role}=\"{dir}\"\n"));
            }
        }
        let udf = spoof_dir.join("user-dirs.dirs");
        if std::fs::write(&udf, &ud).is_ok() {
            if let Some(s) = udf.to_str() {
                let target = format!("{home}/.config/user-dirs.dirs");
                cmd.args(["--ro-bind", s, &target]);
            }
        }

        // Blank system-wide XDG defaults so glib doesn't invent role→path
        // fallbacks when the per-user file omits a role.
        let udd = spoof_dir.join("user-dirs.defaults");
        if std::fs::write(&udd, "").is_ok() {
            if let Some(s) = udd.to_str() {
                cmd.args(["--ro-bind-try", s, "/etc/xdg/user-dirs.defaults"]);
            }
        }

        // Empty bookmarks — clears pinned entries in GTK 2/3/4 file dialogs.
        let gb = spoof_dir.join("gtk-bookmarks");
        if std::fs::write(&gb, "").is_ok() {
            if let Some(s) = gb.to_str() {
                for v in &["gtk-2.0", "gtk-3.0", "gtk-4.0"] {
                    cmd.args(["--ro-bind-try", s, &format!("{home}/.config/{v}/bookmarks")]);
                }
            }
        }

        // Empty recently-used.xbel — otherwise host recent-file entries appear
        // as broken clickable shortcuts in the picker's "Recent" section.
        let rr = spoof_dir.join("recently-used.xbel");
        let stub = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xbel version=\"1.0\"/>\n";
        if std::fs::write(&rr, stub).is_ok() {
            if let Some(s) = rr.to_str() {
                cmd.args(["--ro-bind-try", s, &format!("{home}/.local/share/recently-used.xbel")]);
            }
        }

        // Disable the desktop-portal file chooser: it runs on the host and
        // would bypass every overlay above by returning host paths.  GTK 3
        // honours GTK_USE_PORTAL=0; Firefox uses GTK's chooser under the hood.
        cmd.args(["--setenv", "GTK_USE_PORTAL", "0"]);
    }

    if let Some(ref os_val) = config.spoof_os {
        let content = match os_val.as_str() {
            "ubuntu" => "NAME=Ubuntu\nID=ubuntu\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\nVERSION_ID=24.04\nID_LIKE=debian\n".to_string(),
            "arch"   => "NAME=\"Arch Linux\"\nID=arch\nPRETTY_NAME=\"Arch Linux\"\nBUILD_ID=rolling\n".to_string(),
            "windows" => "NAME=\"Windows 11\"\nID=windows\nPRETTY_NAME=\"Windows 11\"\nVERSION_ID=11\n".to_string(),
            "arduinoide" => "NAME=ArduinoIDE\nID=arduinoide\nPRETTY_NAME=ArduinoIDE\nVERSION_ID=2.3\n".to_string(),
            custom => {
                let mut pretty = custom.to_string();
                if let Some(c) = pretty.get_mut(..1) { c.make_ascii_uppercase() }
                format!("NAME={pretty}\nID={custom}\nPRETTY_NAME={pretty}\nVERSION_ID=1.0\n")
            }
        };
        let of = spoof_dir.join("os-release");
        let _ = std::fs::write(&of, content);
        if let Some(s) = of.to_str() {
            cmd.args(["--ro-bind-try", s, "/etc/os-release"]);
            cmd.args(["--ro-bind-try", s, "/usr/lib/os-release"]);
        }
    }


    // ── Isolated XDG_RUNTIME_DIR ───────────────────────────────────────────────
    //
    // Electron/Qt apps (VS Code, Discord, …) store per-instance IPC sockets in
    // $XDG_RUNTIME_DIR.  If the sandbox shares the host's /run/user/<uid>, a
    // sandboxed app finds the host app's socket, hands off to the running host
    // process ("OK Pleased to meet you"), and exits without opening a window.
    //
    // Fix: give every sandbox a private subdirectory inside the host's runtime
    // dir.  The directory persists across runs so the app can reconnect to its
    // own previous server, but it is invisible to host or other sandbox sockets.
    //
    // Audio and Wayland are re-pointed explicitly so they keep working after the
    // runtime-dir change.
    let mut dbus_proxy_child: Option<std::process::Child> = None;
    {
        let host_rt = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));

        let app_name = std::path::Path::new(app_root)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app");
        let isolated_rt = format!("{host_rt}/.wryayer-{app_name}");
        let _ = std::fs::create_dir_all(&isolated_rt);
        let _ = std::fs::set_permissions(
            &isolated_rt,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        );

        // Convert relative WAYLAND_DISPLAY to absolute so Wayland keeps working
        // after XDG_RUNTIME_DIR changes.
        let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        if !wayland.is_empty() && !wayland.starts_with('/') {
            cmd.env("WAYLAND_DISPLAY", format!("{host_rt}/{wayland}"));
        }

        // Re-point audio sockets explicitly (only when audio is enabled; when
        // audio=off, mask_audio_sockets already handles blocking them and not
        // setting these keeps the masking effective).
        if config.audio {
            let pulse = format!("{host_rt}/pulse/native");
            if std::path::Path::new(&pulse).exists() {
                cmd.env("PULSE_SERVER", format!("unix:{pulse}"));
            }
            let pw = format!("{host_rt}/pipewire-0");
            if std::path::Path::new(&pw).exists() {
                cmd.env("PIPEWIRE_REMOTE", pw);
            }
        }

        // Use bwrap's --setenv rather than cmd.env() so XDG_RUNTIME_DIR is only
        // overridden inside the sandbox.  cmd.env() would also set it on the bwrap
        // process itself; when wrapped with systemd-run --scope, systemd-run
        // inherits that env and then cannot find the user bus socket under the
        // isolated path, causing "Failed to connect to user scope bus".
        cmd.args(["--setenv", "XDG_RUNTIME_DIR", &isolated_rt]);

        // ── Session-bus portal filter ─────────────────────────────────────────
        //
        // The sandbox otherwise reaches the host desktop portal through the
        // session bus (bound via /run), so Firefox/Chromium open the host-side
        // portal file chooser — which shows the user's whole home and hands back
        // paths that aren't mounted, producing errors.  Route D-Bus through
        // xdg-dbus-proxy with the portal filtered out; apps then use their own
        // in-sandbox choosers, which only see shared dirs.
        //
        // Uses --setenv (not cmd.env) for the same reason as XDG_RUNTIME_DIR:
        // the systemd-run --scope wrapper must keep the real user bus to create
        // the scope, so the filtered address may only exist inside the sandbox.
        //
        // The proxy socket lives under the isolated runtime dir, but the address
        // we advertise is the *standard* `$host_runtime/bus`, with the proxy
        // bind-mounted over it inside the sandbox. Apps that nest their own
        // container (Steam's pressure-vessel/RunImage) tmpfs `/run`, rebind the
        // bus to the canonical `/run/user/<uid>/bus`, and reset XDG_RUNTIME_DIR —
        // but they inherit DBUS_SESSION_BUS_ADDRESS unchanged. Pointing it at a
        // path under the private runtime dir leaves it dangling in the nested
        // namespace ("Can't find source path …/.wryayer-<app>/bus"); the
        // canonical path survives that remapping.
        if config.portal_filter {
            let host_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS")
                .unwrap_or_else(|_| format!("unix:path={host_rt}/bus"));
            let proxy_sock = format!("{isolated_rt}/bus");
            if let Some(child) = spawn_dbus_proxy(&host_bus, &proxy_sock) {
                dbus_proxy_child = Some(child);
                let canonical_bus = format!("{host_rt}/bus");
                // Shadow the host session bus with our filtered proxy (sandbox
                // only). Lands after `--bind /run /run`, so it wins.
                cmd.args(["--bind", &proxy_sock, &canonical_bus]);
                cmd.args([
                    "--setenv",
                    "DBUS_SESSION_BUS_ADDRESS",
                    &format!("unix:path={canonical_bus}"),
                ]);
            }
        }
    }

    // ── Wine game ─────────────────────────────────────────────────────────────
    if let Some(w) = wine {
        cmd.args(["--setenv", "WINEPREFIX", &w.prefix]);
        // Quiet wine's default debug spam — games don't need it and it can
        // flood the terminal at hundreds of MB/sec on some titles.
        cmd.args(["--setenv", "WINEDEBUG", "-all"]);
        // Chromium-style sandbox flags don't apply here; clear them so wine
        // children inherit a clean env.
        cmd.args(["--unsetenv", "QTWEBENGINE_CHROMIUM_FLAGS"]);
        cmd.args(["--unsetenv", "ELECTRON_DISABLE_SANDBOX"]);
        if let Some(ref chdir) = w.chdir {
            cmd.args(["--chdir", chdir]);
        }
    }

    // AppImages self-mount through FUSE by default, which isn't present in the
    // sandbox tree; extract-and-run makes them unpack into $TMPDIR instead.
    // Set unconditionally — it's ignored by non-AppImage binaries.
    cmd.args(["--setenv", "APPIMAGE_EXTRACT_AND_RUN", "1"]);
    // Env exported by an AppImage wrapper we bypassed (e.g. `HOME=/opt/Steam`).
    // Applied last so it wins over any earlier --setenv, matching the wrapper.
    for (k, v) in appimage_env {
        cmd.args(["--setenv", k, v]);
    }

    cmd.args(["--", binary]);
    cmd.args(args);
    (cmd, term_spoof_dir, dbus_proxy_child, avahi_stub_child)
}

/// Scan the sandbox's `home/` subtree for ELF files with missing soname
/// dependencies and install the owning packages if any are found.  Returns
/// true if at least one package was installed (caller may retry the app).
///
/// Unlike the install-time soname check, this does NOT skip hidden directories
/// so it correctly reaches binaries in `home/hawky/.config/discord/`.
fn fix_home_sonames(app_root: &Path) -> bool {
    let home_subdir = app_root.join("home");
    if !home_subdir.is_dir() {
        return false;
    }
    let missing = match find_missing_sonames_in(&home_subdir, app_root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("warning: soname check failed: {e:#}");
            return false;
        }
    };
    if missing.is_empty() {
        return false;
    }
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return false,
    };
    let cache_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("pkg");
    let _ = std::fs::create_dir_all(&cache_dir);

    let mut visited: HashSet<String> = HashSet::new();
    let mut any_installed = false;

    for soname in &missing {
        match crate::distro::soname_owner(soname) {
            Ok(Some(pkg)) if !visited.contains(&pkg) => {
                eprintln!("  installing {pkg} (provides {soname})...");
                match download_official(&pkg, &cache_dir) {
                    Ok(path) => match extract_package(&path, app_root) {
                        Ok(()) => {
                            visited.insert(pkg);
                            any_installed = true;
                        }
                        Err(e) => eprintln!("  warning: failed to extract {pkg}: {e:#}"),
                    },
                    Err(e) => eprintln!("  warning: failed to download {pkg}: {e:#}"),
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => eprintln!("  warning: no package found for {soname}"),
            Err(e) => eprintln!("  warning: soname lookup for {soname}: {e:#}"),
        }
    }

    if any_installed {
        run_ldconfig(app_root);
    }
    any_installed
}

// ── Terminal detection helpers ────────────────────────────────────────────────

/// Walk /proc upward from the current process to find the name of the running
/// terminal emulator. Returns the comm name of the first recognised terminal
/// ancestor, or None if nothing was found within 32 hops.
fn detect_terminal_name() -> Option<String> {
    const KNOWN: &[&str] = &[
        "kitty", "foot", "footclient", "alacritty", "wezterm", "wezterm-gui",
        "ghostty", "gnome-terminal", "gnome-terminal-", "konsole", "xterm",
        "urxvt", "rxvt", "st", "xfce4-terminal", "tilix", "termite",
        "sakura", "lxterminal", "mate-terminal", "terminator",
    ];

    let mut pid = unsafe { libc::getpid() };
    for _ in 0..32 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap_or_default();
        let ppid: i32 = status.lines()
            .find_map(|l| {
                l.strip_prefix("PPid:").map(|v| v.trim().parse().ok())
            })
            .flatten()
            .unwrap_or(0);
        if ppid <= 1 { break; }
        let comm = std::fs::read_to_string(format!("/proc/{ppid}/comm"))
            .unwrap_or_default();
        let comm = comm.trim();
        for &t in KNOWN {
            if comm == t || comm.starts_with(t) {
                return Some(comm.to_string());
            }
        }
        pid = ppid;
    }
    None
}

/// Find the absolute path to bwrap by searching PATH.
fn resolve_bwrap_path() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p).find_map(|d| {
                let b = d.join("bwrap");
                if b.exists() { Some(b) } else { None }
            })
        })
        .unwrap_or_else(|| PathBuf::from("/usr/bin/bwrap"))
}

/// Create a symlink `/tmp/.wryayer-spoof-<pid>/<term>` → bwrap and return
/// `(symlink_path, dir_to_clean_up)`.  The symlink name sets task->comm in the
/// exec'd process — fastfetch reads comm, not env vars, for terminal detection.
fn make_bwrap_spoof_exe() -> Option<(PathBuf, PathBuf)> {
    let term = detect_terminal_name()?;
    let bwrap = resolve_bwrap_path();
    let dir = PathBuf::from(format!("/tmp/.wryayer-spoof-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let link = dir.join(&term);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&bwrap, &link).ok()?;
    Some((link, dir))
}

/// Bind /dev/null over every file in `dir` whose name starts with `prefix`.
fn mask_dev_prefix(cmd: &mut Command, dir: &str, prefix: &str) {
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
fn mask_snd_devices(cmd: &mut Command, capture_only: Option<char>) {
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
fn mask_audio_sockets(cmd: &mut Command) {
    let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") else { return };
    for name in &["pipewire-0", "pipewire-0.lock", "pulse/native"] {
        let path = format!("{xdg}/{name}");
        if std::path::Path::new(&path).exists() {
            cmd.args(["--bind", "/dev/null", &path]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway app tree under a unique temp dir; caller populates it.
    fn tmp_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wryayer-run-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("usr/bin")).unwrap();
        dir
    }

    #[test]
    fn appimage_wrapper_without_interpreter_redirects_to_appimage() {
        let root = tmp_tree("noshell");
        std::fs::create_dir_all(root.join("opt/Steam/appimage")).unwrap();
        std::fs::write(root.join("opt/Steam/appimage/Steam.AppImage"), b"ELF").unwrap();
        std::fs::write(
            root.join("usr/bin/steam"),
            "#!/bin/sh\nexport HOME=/opt/Steam\nexec /opt/Steam/appimage/Steam.AppImage\n",
        )
        .unwrap();
        // No /bin/sh (or usr/bin/sh) in the tree → wrapper is unrunnable.

        let (bin, env) = resolve_appimage_wrapper(&root, "/usr/bin/steam").expect("redirect");
        assert_eq!(bin, "/opt/Steam/appimage/Steam.AppImage");
        assert_eq!(env, vec![("HOME".to_string(), "/opt/Steam".to_string())]);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wrapper_with_interpreter_present_is_left_alone() {
        let root = tmp_tree("hasshell");
        std::fs::write(root.join("usr/bin/sh"), b"binary").unwrap();
        std::os::unix::fs::symlink("usr/bin", root.join("bin")).unwrap();
        std::fs::write(root.join("usr/bin/app"), "#!/bin/sh\nexec /usr/bin/real\n").unwrap();

        // /bin/sh resolves through the bin→usr/bin symlink, so don't redirect.
        assert!(resolve_appimage_wrapper(&root, "/usr/bin/app").is_none());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn falls_back_to_tree_scan_when_wrapper_omits_path() {
        let root = tmp_tree("scan");
        std::fs::create_dir_all(root.join("opt/foo")).unwrap();
        std::fs::write(root.join("opt/foo/Foo.AppImage"), b"ELF").unwrap();
        // Wrapper launches via a variable, so no literal *.AppImage token.
        std::fs::write(
            root.join("usr/bin/foo"),
            "#!/bin/sh\nAI=/opt/foo/Foo.AppImage\nexec \"$AI\"\n",
        )
        .unwrap();

        let (bin, _) = resolve_appimage_wrapper(&root, "/usr/bin/foo").expect("scan redirect");
        assert_eq!(bin, "/opt/foo/Foo.AppImage");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn plain_elf_launcher_is_not_touched() {
        let root = tmp_tree("elf");
        std::fs::write(root.join("usr/bin/app"), b"\x7fELF binary").unwrap();
        assert!(resolve_appimage_wrapper(&root, "/usr/bin/app").is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
