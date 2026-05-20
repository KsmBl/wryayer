use crate::commands::install::run_ldconfig;
use crate::config::{read_config, AppConfig, LocalDelete, TempMode};
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

    // bin override: must be one of the app's registered launchers, otherwise
    // anyone could trick `wryayer run` into invoking arbitrary binaries.
    let bin_name = match bin {
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
    };
    const BIN_DIRS: &[&str] = &["usr/bin", "usr/sbin", "bin", "sbin"];
    let binary = BIN_DIRS
        .iter()
        .find(|sub| app_root.join(sub).join(&bin_name).symlink_metadata().is_ok())
        .map(|sub| format!("/{sub}/{bin_name}"))
        .with_context(|| {
            format!(
                "binary '{bin_name}' not found in usr/bin, usr/sbin, bin, or sbin inside {}",
                app_root.display()
            )
        })?;
    let app_root_str = app_root.to_string_lossy().into_owned();

    // Pre-launch: repair any missing sonames in the sandbox home/ tree.
    // Catches second-and-later launches after a self-updating app (e.g.
    // Discord) already wrote its downloaded binary to home/.config/... during
    // a previous session.
    fix_home_sonames(&app_root);

    let (temp, cleanup) = prepare_temp(&config, &app_root)?;

    let status = launch_bwrap(&app_root_str, &binary, args, &temp, &config)?;

    // Post-launch: if bwrap exited abnormally, the app may have written a new
    // self-updated ELF binary (e.g. Discord bootstrapping app-X.Y.Z/Discord)
    // that needs libraries not yet in the sandbox tree.  Run the home/ scan
    // again now that those binaries exist, install any missing packages, and
    // retry automatically so the user doesn't have to re-launch manually.
    let repaired = !status.success() && fix_home_sonames(&app_root);

    if let Some(cleanup_path) = cleanup {
        if repaired {
            let _ = launch_bwrap(&app_root_str, &binary, args, &temp, &config);
        }
        let _ = std::fs::remove_dir_all(&cleanup_path);
        std::process::exit(status.code().unwrap_or(1));
    } else if repaired {
        // Replace this process with a fresh bwrap so the retry gets a clean
        // exec() hand-off (correct signal disposition, no extra wryayer in the
        // process tree).
        let mut cmd = bwrap_cmd(&app_root_str, &binary, args, &temp, &config);
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

fn launch_bwrap(
    app_root_str: &str,
    binary: &str,
    args: &[String],
    temp: &TempBind,
    config: &AppConfig,
) -> Result<ExitStatus> {
    let mut cmd = bwrap_cmd(app_root_str, binary, args, temp, config);
    set_bwrap_env(&mut cmd);
    if let Some(mib) = config.ram_limit {
        if has_systemd_run() {
            cmd = wrap_with_ram_limit(cmd, mib);
        } else {
            eprintln!("warning: systemd-run not found — running without RAM limit");
        }
    }
    cmd.status().context("failed to run bwrap")
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
/// systemd user service with a MemoryMax cgroup limit.  All program args
/// and env overrides are transferred; --wait makes systemd-run block until
/// the service exits and propagate its exit code.
pub fn wrap_with_ram_limit(inner: Command, mib: u64) -> Command {
    let mut outer = Command::new("systemd-run");
    outer.arg("--user")
         .arg("--wait")
         .arg("--quiet")
         .arg("-p").arg(format!("MemoryMax={mib}M"))
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

// ── bwrap command builder ─────────────────────────────────────────────────────

fn bwrap_cmd(app_root: &str, binary: &str, args: &[String], temp: &TempBind, config: &AppConfig) -> Command {
    let mut cmd = Command::new("bwrap");

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
    ] {
        cmd.args(["--ro-bind-try", p, p]);
    }

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

    if let Some(ref hostname) = config.spoof_hostname {
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
        } else {
            cmd.args(["--ro-bind-try", cpuinfo_path.as_str(), "/proc/cpuinfo"]);
        }
    }

    if let Some(ref os_val) = config.spoof_os {
        let content = match os_val.as_str() {
            "ubuntu" => "NAME=Ubuntu\nID=ubuntu\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\nVERSION_ID=24.04\nID_LIKE=debian\n".to_string(),
            "arch"   => "NAME=\"Arch Linux\"\nID=arch\nPRETTY_NAME=\"Arch Linux\"\nBUILD_ID=rolling\n".to_string(),
            "windows" => "NAME=\"Windows 11\"\nID=windows\nPRETTY_NAME=\"Windows 11\"\nVERSION_ID=11\n".to_string(),
            "arduinoide" => "NAME=ArduinoIDE\nID=arduinoide\nPRETTY_NAME=ArduinoIDE\nVERSION_ID=2.3\n".to_string(),
            custom => {
                let mut pretty = custom.to_string();
                pretty.get_mut(..1).map(|c| c.make_ascii_uppercase());
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

    // ── Terminal spoofing ─────────────────────────────────────────────────────────
    // Walk the outer process tree to find the real terminal emulator, then set
    // the env var that fastfetch/neofetch actually use to detect that terminal.
    // Each terminal has its own specific env var — TERM_PROGRAM is NOT a generic
    // solution (fastfetch only recognises it for ghostty and WezTerm).
    if config.spoof_terminal {
        if let Some(detected) = detect_terminal_name() {
            apply_terminal_env(&mut cmd, &detected);
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

        cmd.env("XDG_RUNTIME_DIR", isolated_rt);
    }

    cmd.args(["--", binary]);
    cmd.args(args);
    cmd
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

/// Set the env var(s) that fastfetch actually checks to identify each terminal.
/// Each terminal has its own detection scheme — there is no single generic var.
fn apply_terminal_env(cmd: &mut Command, comm: &str) {
    let current_term = std::env::var("TERM").unwrap_or_default();

    if comm == "kitty" {
        // fastfetch checks KITTY_WINDOW_ID (set by kitty; may be absent when
        // running through a launcher script that does exec).
        if std::env::var("KITTY_WINDOW_ID").is_err() {
            cmd.env("KITTY_WINDOW_ID", "1");
        }
    } else if comm == "foot" || comm == "footclient" {
        // fastfetch detects foot by checking whether $TERM starts with "foot".
        // foot can be configured to use "xterm-256color" instead, so we force it.
        if !current_term.starts_with("foot") {
            cmd.env("TERM", "foot");
        }
    } else if comm == "alacritty" {
        // fastfetch checks $TERM == "alacritty" (alacritty sets this by default).
        if current_term != "alacritty" {
            cmd.env("TERM", "alacritty");
        }
    } else if comm.starts_with("wezterm") {
        // fastfetch checks WEZTERM_PANE (always set by wezterm; may be absent
        // when exec'd through a launcher).
        if std::env::var("WEZTERM_PANE").is_err() {
            cmd.env("WEZTERM_PANE", "0");
        }
    } else if comm == "ghostty" {
        // fastfetch checks TERM_PROGRAM=ghostty.  ghostty sets this itself;
        // force it in case the launcher stripped it.
        cmd.env("TERM_PROGRAM", "ghostty");
    } else if comm.starts_with("gnome-terminal")
        || comm == "xfce4-terminal"
        || comm == "tilix"
        || comm == "mate-terminal"
    {
        // VTE-based terminals: fastfetch checks VTE_VERSION.
        if std::env::var("VTE_VERSION").is_err() {
            cmd.env("VTE_VERSION", "7400");
        }
    } else if comm == "konsole" {
        // fastfetch checks KONSOLE_VERSION.
        if std::env::var("KONSOLE_VERSION").is_err() {
            cmd.env("KONSOLE_VERSION", "220401");
        }
    }
    // For terminals that set their own distinctive env vars (WezTerm sets
    // TERM_PROGRAM, GNOME Terminal/VTE sets VTE_VERSION, etc.) the values are
    // already inherited from the outer environment — no override needed.
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
