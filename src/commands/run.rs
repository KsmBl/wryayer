use crate::config::{read_config, AppConfig, LocalDelete, TempMode};
use crate::manifest::{app_dir, read_manifest};
use anyhow::{bail, Context, Result};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let binary = format!("/usr/bin/{bin_name}");
    let app_root_str = app_root.to_string_lossy().into_owned();

    let (temp, cleanup) = prepare_temp(&config, &app_root)?;

    let mut cmd = bwrap_cmd(&app_root_str, &binary, args, &temp, &config);
    cmd.env("FONTCONFIG_CACHE", "/tmp/.wryayer-fc-cache");

    if let Some(cleanup_path) = cleanup {
        // on_close / uuid modes: run as child so we can clean up after exit
        let status = cmd.status().context("failed to run bwrap")?;
        let _ = std::fs::remove_dir_all(&cleanup_path);
        std::process::exit(status.code().unwrap_or(1));
    } else {
        let err = cmd.exec();
        bail!("failed to exec bwrap: {err}");
    }
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

    cmd.args(["--", binary]);
    cmd.args(args);
    cmd
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
