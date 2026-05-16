use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn extract_package(pkg_path: &Path, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create dest dir {}", dest_dir.display()))?;

    let output = Command::new("tar")
        .args([
            "--zstd",
            "-xf",
            pkg_path.to_str().context("pkg path is not valid UTF-8")?,
            "-C",
            dest_dir.to_str().context("dest path is not valid UTF-8")?,
            "--exclude=.PKGINFO",
            "--exclude=.BUILDINFO",
            "--exclude=.MTREE",
            "--exclude=.INSTALL",
        ])
        .output()
        .context("failed to spawn tar")?;

    if !output.status.success() {
        bail!(
            "tar extraction failed for {}:\n{}",
            pkg_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}
