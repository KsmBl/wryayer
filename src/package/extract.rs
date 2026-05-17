use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub fn extract_package(pkg_path: &Path, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create dest dir {}", dest_dir.display()))?;

    // Pre-unlink regular files that this archive would overwrite.
    // This prevents tar/dpkg-deb from truncating files in place, which would
    // silently corrupt hard-linked snapshots by mutating the shared inode.
    unlink_conflicting_files(pkg_path, dest_dir);

    crate::distro::extract_pkg(pkg_path, dest_dir)
        .with_context(|| format!("failed to extract {}", pkg_path.display()))
}

fn unlink_conflicting_files(pkg_path: &Path, dest_dir: &Path) {
    for entry in crate::distro::list_pkg_files(pkg_path) {
        let path = dest_dir.join(&entry);
        if let Ok(meta) = path.symlink_metadata() {
            if meta.file_type().is_file() {
                let _ = fs::remove_file(&path);
            }
        }
    }
}
