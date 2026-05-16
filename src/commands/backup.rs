use crate::manifest::{app_dir, read_manifest};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
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

    eprintln!("Creating backup: {}", out_path.display());

    let file = fs::File::create(&out_path)
        .with_context(|| format!("failed to create {}", out_path.display()))?;
    let mut zip = ZipWriter::new(file);

    // The zip root is the app name — entries look like "discord/usr/bin/discord"
    let strip_from = app_dir
        .parent()
        .context("app dir has no parent")?
        .to_path_buf();

    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(app_dir.clone());
    let mut file_count = 0u64;

    while let Some(dir) = queue.pop_front() {
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read {}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
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
                file_count += 1;
            } else if ft.is_dir() {
                zip.add_directory(&rel, SimpleFileOptions::default())
                    .with_context(|| format!("failed to add directory {rel}"))?;
                queue.push_back(path);
            } else if ft.is_file() {
                let meta = fs::metadata(&path)?;
                let options = SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .unix_permissions(meta.mode());
                zip.start_file(&rel, options)
                    .with_context(|| format!("failed to start file {rel}"))?;
                let mut f = fs::File::open(&path)
                    .with_context(|| format!("failed to open {}", path.display()))?;
                io::copy(&mut f, &mut zip)
                    .with_context(|| format!("failed to write {rel}"))?;
                file_count += 1;
            }
        }
    }

    zip.finish().context("failed to finalise zip")?;

    let zip_size = fs::metadata(&out_path)?.len();
    eprintln!(
        "Backup complete: {} ({file_count} files, {:.1} MB)",
        out_path.display(),
        zip_size as f64 / 1_048_576.0
    );
    Ok(())
}
