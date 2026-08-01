//! The `<app>` shortcut that stands in for a sandboxed app.
//!
//! Shortcuts are installed system-wide, in `/usr/bin`, so everything that
//! resolves a program through the standard PATH treats a sandboxed app exactly
//! as it would a natively packaged one: a login shell, a desktop menu entry, a
//! file manager handing off a document, another application opening a link.
//! `~/bin` — where shortcuts used to live — is on none of those PATHs unless
//! the user's shell profile puts it there, which is why wryayer apps were
//! effectively invisible outside an interactive terminal.
//!
//! Writing to `/usr/bin` needs root. We write directly when we already have it,
//! borrow it through sudo when sudo can be had without hanging on a password
//! prompt nobody can answer, and otherwise fall back to the old `~/bin`
//! location with a warning — an install is not worth failing over a shortcut.
//! Removal looks in both places, so shortcuts written either way are cleaned up.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where shortcuts go unless `WRYAYER_LAUNCHER_DIR` says otherwise.
pub const SYSTEM_LAUNCHER_DIR: &str = "/usr/bin";

/// First line of every generated shortcut after the shebang. Removal refuses to
/// delete a file that does not carry it, so a hand-written `/usr/bin/firefox`
/// is never collateral damage.
const MARKER: &str = "# wryayer managed launcher";

/// The directory new shortcuts are installed into.
///
/// `WRYAYER_LAUNCHER_DIR` overrides the default — for the test suite, and for
/// anyone who would rather keep shortcuts in their own home directory than
/// hand wryayer root for every install.
pub fn launchers_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("WRYAYER_LAUNCHER_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    Ok(PathBuf::from(SYSTEM_LAUNCHER_DIR))
}

/// The per-user shortcut directory: where shortcuts lived before they became
/// system-wide, and where they still land when root is out of reach.
pub fn user_launchers_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("bin"))
}

/// Every directory a shortcut may be sitting in, most preferred first, with the
/// per-user fallback included only when it is a genuinely different place.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = launchers_dir() {
        dirs.push(dir);
    }
    if let Ok(dir) = user_launchers_dir() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Where `binary_name`'s shortcut actually is, if a wryayer-managed one exists.
///
/// Used by the desktop-entry writer, which has to point `Exec=` at the real
/// path rather than assume the shortcut made it into `/usr/bin`.
pub fn launcher_path(binary_name: &str) -> Option<PathBuf> {
    candidate_dirs().into_iter().map(|d| d.join(binary_name)).find(|p| is_ours(p))
}

pub fn create_launcher(app_name: &str, binary_name: &str) -> Result<PathBuf> {
    let dir = launchers_dir()?;
    let path = dir.join(binary_name);
    let content = launcher_content(app_name);

    match install_shortcut(&path, &content) {
        Ok(()) => {
            // A stale per-user shortcut of the same name would shadow the
            // system one for anybody whose PATH still lists ~/bin first.
            if let Ok(user_dir) = user_launchers_dir() {
                if user_dir != dir {
                    let _ = remove_shortcut(&user_dir.join(binary_name));
                }
            }
            Ok(path)
        }
        Err(e) => {
            let fallback_dir = user_launchers_dir()?;
            if fallback_dir == dir {
                return Err(e);
            }
            let fallback = fallback_dir.join(binary_name);
            eprintln!("warning: could not install the shortcut in {}: {e}", dir.display());
            eprintln!("         using {} instead — it is only on the PATH of your", fallback.display());
            eprintln!("         interactive shell, so desktop menus will not find it.");
            eprintln!("         To promote it later:");
            eprintln!("           sudo install -m 755 {} {}", fallback.display(), path.display());
            install_shortcut(&fallback, &content)?;
            Ok(fallback)
        }
    }
}

/// Delete `binary_name`'s shortcut wherever it is, and report what was removed.
///
/// Files that do not carry the wryayer marker are left alone with a warning.
pub fn remove_launcher(binary_name: &str) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for dir in candidate_dirs() {
        let path = dir.join(binary_name);
        if !path.exists() {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read launcher at {}", path.display()))?;
        if !content.contains(MARKER) {
            eprintln!(
                "warning: skipping {} — does not look like a wryayer launcher",
                path.display()
            );
            continue;
        }
        remove_shortcut(&path)?;
        removed.push(path);
    }
    Ok(removed)
}

/// Whether `path` is a shortcut this program wrote.
fn is_ours(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|c| c.contains(MARKER))
}

fn launcher_content(app_name: &str) -> String {
    format!(
        r#"#!/bin/bash
{MARKER} for {app_name}
exec {exe} run "{app_name}" "$@"
"#,
        exe = shell_quote(&wryayer_exe())
    )
}

/// The wryayer binary a shortcut should call.
///
/// An absolute path, so a shortcut started by a desktop menu or a file manager
/// works even though the session PATH never included wherever wryayer was
/// installed. Falls back to a bare `wryayer` when the running executable is not
/// wryayer itself — under `cargo test`, most obviously.
fn wryayer_exe() -> String {
    std::env::current_exe()
        .ok()
        .filter(|p| p.file_name().is_some_and(|n| n == "wryayer"))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "wryayer".to_string())
}

fn shell_quote(s: &str) -> String {
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

// ── privileged file operations ───────────────────────────────────────────────

/// Write an executable shortcut at `path`, escalating only if we have to.
fn install_shortcut(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(path, content) {
        Ok(()) => {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                .with_context(|| format!("failed to chmod launcher at {}", path.display()))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            sudo_write(path, content, 0o755)
        }
        Err(e) => Err(e).with_context(|| format!("failed to write launcher at {}", path.display())),
    }
}

fn remove_shortcut(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => sudo_remove(path),
        Err(e) => Err(e).with_context(|| format!("failed to remove launcher at {}", path.display())),
    }
}

/// Delete a root-owned file. Shared with the desktop-entry writer, which puts
/// its files in `/usr/share/applications` under the same conditions.
pub fn sudo_remove(path: &Path) -> Result<()> {
    let status = sudo()
        .with_context(|| format!("root is needed to remove {}", path.display()))?
        .args(["rm", "-f"])
        .arg(path)
        .status()
        .context("failed to run sudo")?;
    if !status.success() {
        bail!("sudo rm -f {} failed", path.display());
    }
    Ok(())
}

/// Stage `content` in a private temp file and have root copy it into place.
///
/// `install(1)` sets the mode as it copies, so the file is never briefly
/// present with the wrong permissions, and never briefly present but empty.
pub fn sudo_write(path: &Path, content: &str, mode: u32) -> Result<()> {
    let mut sudo = sudo().with_context(|| {
        format!(
            "root is needed to write {}, and sudo cannot ask for a password here.\n       \
             Run this from a terminal, or set WRYAYER_LAUNCHER_DIR to a directory you own",
            path.display()
        )
    })?;

    let staged = std::env::temp_dir().join(format!(
        "wryayer-staged-{}-{}",
        std::process::id(),
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&staged, content)
        .with_context(|| format!("failed to stage {}", staged.display()))?;

    let status = sudo
        .args(["install", "-m", &format!("{mode:o}")])
        .arg(&staged)
        .arg(path)
        .status()
        .context("failed to run sudo");
    let _ = fs::remove_file(&staged);

    if !status?.success() {
        bail!("sudo install -m {mode:o} … {} failed", path.display());
    }
    Ok(())
}

/// A sudo command that will not hang, or `None` when there is no way to
/// authenticate.
///
/// Cached credentials are used silently. Otherwise a password is needed, and
/// asking for one is only viable with a terminal attached — the TUI and GUI run
/// installs as piped children, where an interactive sudo would block forever on
/// a prompt the user never sees.
fn sudo() -> Option<Command> {
    if crate::veracrypt::sudo_is_primed() {
        let mut cmd = Command::new("sudo");
        cmd.arg("-n").stdin(Stdio::null());
        return Some(cmd);
    }
    if std::io::stdin().is_terminal() {
        return Some(Command::new("sudo"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_calls_wryayer_with_the_app_name() {
        let content = launcher_content("firefox");
        assert!(content.starts_with("#!/bin/bash\n"));
        assert!(content.contains(MARKER));
        assert!(content.contains(r#"run "firefox" "$@""#));
    }

    #[test]
    fn shell_quote_leaves_ordinary_paths_alone() {
        assert_eq!(shell_quote("/usr/bin/wryayer"), "/usr/bin/wryayer");
        assert_eq!(shell_quote("/home/a b/wryayer"), "'/home/a b/wryayer'");
        assert_eq!(shell_quote("/o'x/wryayer"), r"'/o'\''x/wryayer'");
    }
}
