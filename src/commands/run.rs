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
