use crate::package::{download_official, extract_package};
use anyhow::{Context, Result};
use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Walk `app_dir`, find every ELF NEEDED soname that is absent from the tree,
/// and extract the owning package for each one. Loops until convergence so that
/// transitive deps of newly added packages are also satisfied.
/// Returns the list of package names that were installed.
pub fn satisfy_missing_sonames(app_dir: &Path, cache_dir: &Path) -> Result<Vec<String>> {
    let mut installed: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    loop {
        let missing = find_missing_sonames(app_dir)?;
        if missing.is_empty() {
            break;
        }

        let mut progress = false;
        for soname in &missing {
            match crate::distro::soname_owner(soname) {
                Ok(Some(pkg)) if !visited.contains(&pkg) => {
                    eprintln!("  installing {pkg} (provides {soname})...");
                    let path = download_official(&pkg, cache_dir)
                        .with_context(|| format!("failed to download {pkg}"))?;
                    extract_package(&path, app_dir)
                        .with_context(|| format!("failed to extract {pkg}"))?;
                    visited.insert(pkg.clone());
                    installed.push(pkg);
                    progress = true;
                }
                Ok(_) => {}
                Err(e) => eprintln!("  warning: soname lookup for {soname}: {e:#}"),
            }
        }

        if !progress {
            for soname in &missing {
                eprintln!("  warning: no package found for {soname}");
            }
            break;
        }
    }

    Ok(installed)
}

pub fn find_missing_sonames(app_dir: &Path) -> Result<Vec<String>> {
    let needed = collect_needed(app_dir)?;
    Ok(needed
        .into_iter()
        .filter(|s| !soname_in_app(app_dir, s))
        .collect())
}

/// Like `find_missing_sonames` but only scans `scan_dir` for ELF files.
/// Soname presence is still checked against the full `app_dir` lib tree.
///
/// Unlike `find_missing_sonames`, this does NOT skip hidden directories
/// (e.g. `.config`). User-writable sandbox home dirs routinely place real
/// app binaries inside hidden subdirectories like `~/.config/discord/`.
pub fn find_missing_sonames_in(scan_dir: &Path, app_dir: &Path) -> Result<Vec<String>> {
    let needed = collect_needed_including_hidden(scan_dir)?;
    Ok(needed
        .into_iter()
        .filter(|s| !soname_in_app(app_dir, s))
        .collect())
}

/// Walk `start_dir` recursively, skipping directories whose name starts with
/// `.` (package metadata dirs, wryayer control dirs such as `.tmp`, `.spoof`).
fn collect_needed(start_dir: &Path) -> Result<HashSet<String>> {
    collect_needed_impl(start_dir, true)
}

/// Walk `start_dir` recursively without skipping hidden directories.
/// Used when scanning the sandbox `home/` tree where real app binaries live
/// inside hidden dirs like `.config/`.
fn collect_needed_including_hidden(start_dir: &Path) -> Result<HashSet<String>> {
    collect_needed_impl(start_dir, false)
}

fn collect_needed_impl(start_dir: &Path, skip_hidden: bool) -> Result<HashSet<String>> {
    let mut needed: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(start_dir.to_path_buf());

    while let Some(dir) = queue.pop_front() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                let n = entry.file_name();
                if !skip_hidden || !n.to_string_lossy().starts_with('.') {
                    queue.push_back(path);
                }
            } else if ft.is_file() {
                if let Ok(libs) = elf_needed(&path) {
                    needed.extend(libs);
                }
            }
        }
    }

    Ok(needed)
}

fn elf_needed(path: &Path) -> Result<Vec<String>> {
    let mut f = std::fs::File::open(path)?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    if &magic != b"\x7fELF" {
        return Ok(vec![]);
    }
    drop(f);

    let out = Command::new("readelf")
        .args(["-d", &path.to_string_lossy()])
        .output()
        .context("readelf failed")?;

    let mut libs = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.contains("(NEEDED)") {
            if let (Some(s), Some(e)) = (line.find('['), line.rfind(']')) {
                libs.push(line[s + 1..e].to_string());
            }
        }
    }
    Ok(libs)
}

pub(crate) fn soname_in_app(app_dir: &Path, soname: &str) -> bool {
    ["usr/lib", "usr/lib64", "lib", "lib64"]
        .iter()
        .any(|sub| app_dir.join(sub).join(soname).exists())
}
