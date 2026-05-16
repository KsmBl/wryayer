use crate::config::{read_config, AppConfig, LocalDelete, TempMode};
use crate::manifest::{app_dir, read_manifest};
use anyhow::{bail, Context, Result};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(app_name: &str, args: &[String]) -> Result<()> {
    let manifest = read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;
    let config = read_config(app_name)?;

    let app_root = app_dir(app_name)?;
    if !app_root.exists() {
        bail!("app directory missing: {}", app_root.display());
    }

    let binary = format!("/usr/bin/{}", manifest.app.main_binary);
    let app_root_str = app_root.to_string_lossy().into_owned();

    let (temp, cleanup) = prepare_temp(&config, &app_root)?;

    let mut cmd = bwrap_cmd(&app_root_str, &binary, args, &temp);
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

fn no_other_instance(pid_file: &Path) -> bool {
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

fn bwrap_cmd(app_root: &str, binary: &str, args: &[String], temp: &TempBind) -> Command {
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

    cmd.args(["--bind", "/run",  "/run"]);
    cmd.args(["--bind", "/home", "/home"]);

    // /etc — networking, locale, identity, TLS
    for p in &[
        "/etc/resolv.conf", "/etc/hosts",      "/etc/localtime",
        "/etc/locale.conf", "/etc/machine-id", "/etc/nsswitch.conf",
        "/etc/passwd",      "/etc/group",       "/etc/ssl/certs",
    ] {
        cmd.args(["--ro-bind-try", p, p]);
    }

    cmd.args(["--", binary]);
    cmd.args(args);
    cmd
}
