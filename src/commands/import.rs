use crate::launcher::create_launcher;
use crate::manifest::read_manifest;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use zip::ZipArchive;

pub fn run(zip_path: &Path) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let wryayer_dir = PathBuf::from(&home).join(".wryayer");
    fs::create_dir_all(&wryayer_dir).context("failed to create ~/.wryayer")?;

    let file = fs::File::open(zip_path)
        .with_context(|| format!("failed to open {}", zip_path.display()))?;
    let mut archive = ZipArchive::new(file).context("failed to read zip")?;

    if archive.is_empty() {
        bail!("zip is empty");
    }

    // Detect app name from the first entry's top-level path component
    let app_name = {
        let first = archive.by_index(0).context("failed to read first zip entry")?;
        first
            .enclosed_name()
            .and_then(|p| p.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()))
            .context("cannot determine app name from zip")?
    };

    eprintln!("Importing '{app_name}' from {}", zip_path.display());

    let dest = wryayer_dir.join(&app_name);
    if dest.exists() {
        eprintln!(
            "Warning: '{}' already exists — existing files will be overwritten",
            dest.display()
        );
    }

    let mut file_count = 0u64;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("failed to read zip entry")?;

        // Guard against path traversal
        let enclosed = entry
            .enclosed_name()
            .context("zip entry has unsafe path")?
            .to_path_buf();

        let out_path = wryayer_dir.join(&enclosed);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .with_context(|| format!("failed to create dir {}", out_path.display()))?;
        } else if entry.is_symlink() {
            // Symlink target is stored as the file content
            let mut target = String::new();
            io::Read::read_to_string(&mut entry, &mut target)
                .context("failed to read symlink target")?;
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let _ = fs::remove_file(&out_path);
            std::os::unix::fs::symlink(target.trim(), &out_path)
                .with_context(|| format!("failed to create symlink {}", out_path.display()))?;
            file_count += 1;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = fs::File::create(&out_path)
                .with_context(|| format!("failed to create {}", out_path.display()))?;
            io::copy(&mut entry, &mut f)
                .with_context(|| format!("failed to write {}", out_path.display()))?;
            // Restore Unix permissions
            if let Some(mode) = entry.unix_mode() {
                fs::set_permissions(&out_path, fs::Permissions::from_mode(mode))
                    .with_context(|| format!("failed to set permissions on {}", out_path.display()))?;
            }
            file_count += 1;
        }
    }

    // The sandbox home lives at `home/<username>`, where <username> is the
    // basename of $HOME at launch. A tree exported from another machine carries
    // *that* user's name (e.g. `home/whisper`); on a differently-named account
    // the sandbox would look at `home/<thisuser>` and find nothing, so the
    // browser profile and settings would appear lost. Rename the imported home
    // to this machine's username so the profile is found regardless of account.
    remap_sandbox_home(&dest)?;

    // Recreate launchers in ~/bin/ — the export only contains ~/.wryayer/<app>/
    // so launchers are never in the zip and must be reconstructed from the manifest.
    let manifest = read_manifest(&app_name)
        .with_context(|| format!("failed to read imported manifest for '{app_name}'"))?;
    for launcher_name in &manifest.app.launchers {
        let launcher_path = create_launcher(&manifest.app.name, launcher_name)
            .with_context(|| format!("failed to create launcher for '{launcher_name}'"))?;
        eprintln!("Created launcher: {}", launcher_path.display());
    }

    eprintln!("Imported '{app_name}' to {} ({file_count} files)", dest.display());
    Ok(())
}

/// Return the current user's name — the basename of `$HOME`, matching how the
/// sandbox home dir is named at install/run time.
fn current_username() -> Result<String> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(home
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("user")
        .to_string())
}

/// Rename an imported `home/<exporter>` sandbox-home dir to `home/<thisuser>` so
/// a tree exported on another machine (under a different account) still finds
/// its profile. No-op when the home already carries this user's name, or when
/// the layout is ambiguous (not exactly one user subdir).
fn remap_sandbox_home(app_dir: &Path) -> Result<()> {
    remap_sandbox_home_to(app_dir, &current_username()?)
}

fn remap_sandbox_home_to(app_dir: &Path, username: &str) -> Result<()> {
    let home_dir = app_dir.join("home");
    if !home_dir.is_dir() {
        return Ok(());
    }
    let target = home_dir.join(username);
    if target.exists() {
        // Already this user's home (same-account import) — nothing to do.
        return Ok(());
    }
    // Collect the real (non-symlink) user subdirs. A sandbox home has exactly
    // one; anything else is unexpected, so leave it untouched rather than guess.
    let subdirs: Vec<PathBuf> = fs::read_dir(&home_dir)
        .with_context(|| format!("failed to read {}", home_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.is_symlink())
        .collect();
    if subdirs.len() != 1 {
        return Ok(());
    }
    let old = &subdirs[0];
    fs::rename(old, &target)
        .with_context(|| format!("failed to remap sandbox home to '{username}'"))?;
    let old_name = old.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    eprintln!("Remapped sandbox home: {old_name} -> {username}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree exported under another account (`home/whisper`) must have its home
    /// renamed to this machine's user so the profile is found after import.
    #[test]
    fn remap_renames_foreign_home_to_current_user() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("vivaldi");
        fs::create_dir_all(app.join("home/whisper/.config/vivaldi")).unwrap();
        fs::write(app.join("home/whisper/.config/vivaldi/Prefs"), b"settings").unwrap();

        remap_sandbox_home_to(&app, "alice").unwrap();

        assert!(!app.join("home/whisper").exists());
        let prefs = app.join("home/alice/.config/vivaldi/Prefs");
        assert!(prefs.is_file(), "profile not found under the new username");
        assert_eq!(fs::read(&prefs).unwrap(), b"settings");
    }

    /// Same-account import: the home already matches, so it's left untouched.
    #[test]
    fn remap_noop_when_home_already_current_user() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("vivaldi");
        fs::create_dir_all(app.join("home/alice")).unwrap();
        fs::write(app.join("home/alice/marker"), b"x").unwrap();

        remap_sandbox_home_to(&app, "alice").unwrap();

        assert!(app.join("home/alice/marker").is_file());
    }

    /// Ambiguous layouts (not exactly one user subdir) are left alone.
    #[test]
    fn remap_noop_when_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("vivaldi");
        fs::create_dir_all(app.join("home/whisper")).unwrap();
        fs::create_dir_all(app.join("home/bob")).unwrap();

        remap_sandbox_home_to(&app, "alice").unwrap();

        assert!(app.join("home/whisper").exists());
        assert!(app.join("home/bob").exists());
        assert!(!app.join("home/alice").exists());
    }
}
