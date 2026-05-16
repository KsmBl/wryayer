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

    if archive.len() == 0 {
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

    eprintln!("Imported '{app_name}' to {} ({file_count} files)", dest.display());
    Ok(())
}
