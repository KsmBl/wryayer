use crate::manifest::{app_dir, read_manifest};
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Subdirectory inside each app holding all snapshots for that app.
/// Walked-around when creating new snapshots so snapshots don't snapshot themselves.
pub const SNAP_DIR: &str = ".snapshots";

fn snapshots_dir(app_name: &str) -> Result<PathBuf> {
    Ok(app_dir(app_name)?.join(SNAP_DIR))
}

fn timestamp_label() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Create a hard-linked snapshot of `app_name`. Returns the label used.
pub fn create(app_name: &str) -> Result<String> {
    read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    let app_root = app_dir(app_name)?;
    let snap_root = snapshots_dir(app_name)?;
    let label = timestamp_label();
    let snap_path = snap_root.join(&label);

    if snap_path.exists() {
        bail!("snapshot '{label}' already exists for {app_name}");
    }

    eprintln!("Creating snapshot {app_name}/{label}...");
    fs::create_dir_all(&snap_path)
        .with_context(|| format!("failed to create {}", snap_path.display()))?;

    let mut file_count = 0u64;
    hardlink_tree(&app_root, &snap_path, &app_root, &mut file_count)?;

    eprintln!("Snapshot complete: {} ({file_count} files)", snap_path.display());
    Ok(label)
}

/// Roll `app_name` back to `snapshot` (or the latest if None).
/// The current state is replaced by hard-linking the snapshot's files back over the app dir.
pub fn rollback(app_name: &str, snapshot: Option<&str>) -> Result<()> {
    read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    let app_root = app_dir(app_name)?;
    let snap_root = snapshots_dir(app_name)?;

    let label = match snapshot {
        Some(s) => s.to_string(),
        None => latest(app_name)?
            .with_context(|| format!("no snapshots for {app_name}"))?,
    };
    let snap_path = snap_root.join(&label);
    if !snap_path.is_dir() {
        bail!("snapshot '{label}' not found at {}", snap_path.display());
    }

    // Move current state aside so rollback is atomic from the user's POV
    let staging = app_root.with_file_name(format!(".{app_name}.rollback-staging"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create staging {}", staging.display()))?;

    eprintln!("Rolling back {app_name} to {label}...");

    // Re-snapshot the live tree into staging (cheap, hardlink only) so we
    // can restore if something goes wrong mid-rollback.
    let mut current_files = 0u64;
    hardlink_tree(&app_root, &staging, &app_root, &mut current_files)?;

    // Wipe everything in app_root *except* the .snapshots dir
    for entry in fs::read_dir(&app_root)
        .with_context(|| format!("failed to read {}", app_root.display()))?
        .flatten()
    {
        if entry.file_name() == SNAP_DIR {
            continue;
        }
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
    }

    // Materialise snapshot back over app_root via hardlinks
    let mut restored = 0u64;
    hardlink_tree(&snap_path, &app_root, &app_root, &mut restored)?;

    // Successful — drop the staging dir
    let _ = fs::remove_dir_all(&staging);

    eprintln!("Rollback complete: {restored} files restored from {label}");
    Ok(())
}

/// List snapshots for `app_name`, newest first.
pub fn list(app_name: &str) -> Result<()> {
    let snaps = labels(app_name)?;
    if snaps.is_empty() {
        eprintln!("No snapshots for {app_name}");
        return Ok(());
    }
    println!("Snapshots for {app_name}:");
    for s in snaps {
        println!("  {s}");
    }
    Ok(())
}

/// Delete all snapshots for `app_name` beyond the `keep` most recent.
pub fn prune(app_name: &str, keep: usize) -> Result<()> {
    read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    let snaps = labels(app_name)?;
    if snaps.len() <= keep {
        eprintln!(
            "Nothing to prune — {app_name} has {} snapshot(s), keeping {keep}",
            snaps.len()
        );
        return Ok(());
    }

    let snap_root = snapshots_dir(app_name)?;
    for label in &snaps[keep..] {
        let path = snap_root.join(label);
        fs::remove_dir_all(&path)
            .with_context(|| format!("failed to remove snapshot {}", path.display()))?;
        eprintln!("Deleted snapshot {app_name}/{label}");
    }
    eprintln!("Kept {} most recent snapshot(s).", keep);
    Ok(())
}

pub fn latest(app_name: &str) -> Result<Option<String>> {
    Ok(labels(app_name)?.into_iter().next())
}

pub fn labels(app_name: &str) -> Result<Vec<String>> {
    let snap_root = snapshots_dir(app_name)?;
    let mut out: Vec<String> = vec![];
    let Ok(rd) = fs::read_dir(&snap_root) else {
        return Ok(out);
    };
    for entry in rd.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    // Lexical sort gives newest-first when labels are YYYYMMDD-HHMMSS
    out.sort_by(|a, b| b.cmp(a));
    Ok(out)
}

/// Recursively recreate the tree at `src` under `dst`, hard-linking regular
/// files and re-creating symlinks. The `app_root_skip` is the live app root
/// whose `.snapshots` subdirectory must be skipped so snapshots don't recurse
/// into themselves.
fn hardlink_tree(
    src: &Path,
    dst: &Path,
    app_root_skip: &Path,
    file_count: &mut u64,
) -> Result<()> {
    let mut queue: VecDeque<(PathBuf, PathBuf)> = VecDeque::new();
    queue.push_back((src.to_path_buf(), dst.to_path_buf()));

    while let Some((s, d)) = queue.pop_front() {
        for entry in fs::read_dir(&s)
            .with_context(|| format!("failed to read {}", s.display()))?
            .flatten()
        {
            let p = entry.path();
            // Skip the snapshots dir when walking the live app root
            if p == app_root_skip.join(SNAP_DIR) {
                continue;
            }
            let name = entry.file_name();
            let target = d.join(&name);
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                let link_target = fs::read_link(&p)?;
                if target.symlink_metadata().is_ok() {
                    let _ = fs::remove_file(&target);
                }
                symlink(link_target, &target)
                    .with_context(|| format!("failed to symlink {}", target.display()))?;
                *file_count += 1;
            } else if ft.is_dir() {
                fs::create_dir_all(&target)
                    .with_context(|| format!("failed to create {}", target.display()))?;
                if let Ok(meta) = fs::metadata(&p) {
                    let _ = fs::set_permissions(&target, fs::Permissions::from_mode(meta.mode()));
                }
                queue.push_back((p, target));
            } else if ft.is_file() {
                if target.exists() {
                    let _ = fs::remove_file(&target);
                }
                fs::hard_link(&p, &target)
                    .with_context(|| format!("failed to hard-link {}", target.display()))?;
                *file_count += 1;
            }
        }
    }
    Ok(())
}
