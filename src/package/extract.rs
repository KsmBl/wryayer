use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

const TAR_EXCLUDES: &[&str] = &[
    "--exclude=.PKGINFO",
    "--exclude=.BUILDINFO",
    "--exclude=.MTREE",
    "--exclude=.INSTALL",
];

pub fn extract_package(pkg_path: &Path, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create dest dir {}", dest_dir.display()))?;

    // Pre-unlink any regular files in `dest_dir` that this tarball would
    // overwrite. Default tar behaviour truncates files in place, which shares
    // the inode with hardlink-based snapshots and silently corrupts them.
    // Unlinking first forces the new extraction to create a fresh inode.
    //
    // Only regular files are touched — directory entries are merged naturally
    // by tar (any other approach breaks installs that drop into shared paths
    // like usr/bin/), and symlinks are unlinked by tar itself on overwrite.
    unlink_conflicting_files(pkg_path, dest_dir)?;

    let pkg = pkg_path.to_str().context("pkg path is not valid UTF-8")?;
    let dest = dest_dir.to_str().context("dest path is not valid UTF-8")?;

    let mut cmd = Command::new("tar");
    cmd.args(["--zstd", "-xf", pkg, "-C", dest]);
    cmd.args(TAR_EXCLUDES);
    let output = cmd.output().context("failed to spawn tar")?;

    if !output.status.success() {
        bail!(
            "tar extraction failed for {}:\n{}",
            pkg_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

fn unlink_conflicting_files(pkg_path: &Path, dest_dir: &Path) -> Result<()> {
    let pkg = pkg_path.to_str().context("pkg path is not valid UTF-8")?;
    let mut cmd = Command::new("tar");
    cmd.args(["--zstd", "-tf", pkg]);
    cmd.args(TAR_EXCLUDES);
    let output = cmd.output().context("failed to list tar contents")?;

    if !output.status.success() {
        // Listing failed — fall through to extract; tar will report the same
        // error there. We don't want listing failures to mask the real issue.
        return Ok(());
    }

    for entry in String::from_utf8_lossy(&output.stdout).lines() {
        // Directory entries end with '/' in tar listings — skip them.
        if entry.is_empty() || entry.ends_with('/') {
            continue;
        }
        let path = dest_dir.join(entry);
        // Only unlink existing regular files (not symlinks, not dirs).
        if let Ok(meta) = path.symlink_metadata() {
            if meta.file_type().is_file() {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}
