use crate::commands::snapshot::SNAP_DIR;
use crate::manifest::{app_dir, read_manifest};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;
use zip::ZipWriter;

pub fn run(app_name: &str, output: Option<&PathBuf>) -> Result<()> {
    read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    let app_dir = app_dir(app_name)?;

    let default_name = format!(
        "{}-{}.zip",
        app_name,
        chrono::Local::now().format("%Y-%m-%d")
    );
    let out_path = match output {
        Some(p) => p.clone(),
        None => PathBuf::from(&default_name),
    };

    eprintln!("Creating export: {}", out_path.display());

    // Pre-walk to count entries so we can emit real progress markers.
    let total_entries = count_entries(&app_dir);
    eprintln!("PROGRESS 0/{total_entries}");

    let file = fs::File::create(&out_path)
        .with_context(|| format!("failed to create {}", out_path.display()))?;
    let mut zip = ZipWriter::new(file);

    let strip_from = app_dir
        .parent()
        .context("app dir has no parent")?
        .to_path_buf();

    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(app_dir.clone());
    let mut written = 0u64;
    let mut skipped = 0u64;
    // Throttle PROGRESS lines: at most ~200 updates over the full run so the
    // TUI's mpsc channel doesn't get spammed for trees with 50k+ files.
    let stride = (total_entries / 200).max(1);

    while let Some(dir) = queue.pop_front() {
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read {}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            // Skip the snapshots subtree — those are wryayer-internal
            if path.file_name().map(|n| n == SNAP_DIR).unwrap_or(false) {
                continue;
            }

            let rel = path
                .strip_prefix(&strip_from)
                .context("path outside app dir")?
                .to_string_lossy()
                .into_owned();

            let ft = entry.file_type()?;

            if ft.is_symlink() {
                let target = fs::read_link(&path)?;
                zip.add_symlink(
                    &rel,
                    target.to_string_lossy(),
                    SimpleFileOptions::default(),
                )
                .with_context(|| format!("failed to add symlink {rel}"))?;
                written += 1;
            } else if ft.is_dir() {
                zip.add_directory(&rel, SimpleFileOptions::default())
                    .with_context(|| format!("failed to add directory {rel}"))?;
                queue.push_back(path);
                written += 1;
            } else if ft.is_file() {
                let meta = fs::metadata(&path)?;
                // Some packages ship files at restrictive modes that survived
                // extraction (e.g. setuid helpers stripped to ---x--x---).
                // Skip-with-warning so one bad file doesn't fail the whole
                // export; `wryayer repair <app>` retroactively fixes these.
                let mut f = match fs::File::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("warning: skipping {rel}: {e}");
                        skipped += 1;
                        continue;
                    }
                };
                let options = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .unix_permissions(meta.mode());
                zip.start_file(&rel, options)
                    .with_context(|| format!("failed to start file {rel}"))?;
                if let Err(e) = io::copy(&mut f, &mut zip) {
                    eprintln!("warning: short read on {rel}: {e}");
                    skipped += 1;
                }
                written += 1;
            }

            if written % stride == 0 {
                eprintln!("PROGRESS {written}/{total_entries}");
            }
        }
    }

    zip.finish().context("failed to finalise zip")?;
    eprintln!("PROGRESS {total_entries}/{total_entries}");

    let zip_size = fs::metadata(&out_path)?.len();
    eprintln!(
        "Export complete: {} ({written} files, {:.1} MB){}",
        out_path.display(),
        zip_size as f64 / 1_048_576.0,
        if skipped > 0 { format!(" — skipped {skipped} unreadable file(s); run `wryayer repair {app_name}` to fix") } else { String::new() },
    );
    Ok(())
}

/// Count files + dirs + symlinks under `root`, skipping the snapshots subtree.
fn count_entries(root: &Path) -> u64 {
    let mut count = 0u64;
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(root.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.file_name().map(|n| n == SNAP_DIR).unwrap_or(false) {
                continue;
            }
            count += 1;
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() && !ft.is_symlink() {
                    queue.push_back(p);
                }
            }
        }
    }
    count
}
