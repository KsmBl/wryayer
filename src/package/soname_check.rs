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
    satisfy_missing_sonames_impl(app_dir, cache_dir, None)
}

/// Like `satisfy_missing_sonames` but on the first iteration only scans the
/// given `seed_paths` (e.g. files just extracted by this install) instead of
/// the whole tree. Subsequent iterations widen to the full tree so transitive
/// deps of any auto-installed packages still get resolved.
///
/// Used after a merge install: only newly-extracted files can introduce
/// new soname requirements, so re-scanning a 400-package container tree is
/// wasted work for every install.
pub fn satisfy_missing_sonames_for(
    app_dir: &Path,
    cache_dir: &Path,
    seed_paths: &[PathBuf],
) -> Result<Vec<String>> {
    satisfy_missing_sonames_impl(app_dir, cache_dir, Some(seed_paths))
}

fn satisfy_missing_sonames_impl(
    app_dir: &Path,
    cache_dir: &Path,
    seed_paths: Option<&[PathBuf]>,
) -> Result<Vec<String>> {
    let mut installed: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut already_missing: HashSet<String> = HashSet::new();
    // Files to scan on the next iteration. Starts as the caller-supplied seed
    // (when None, the full tree is scanned), then narrows to only files added
    // by satisfy-loop installs so we never re-walk the full container.
    let mut next_scan: Option<Vec<PathBuf>> = seed_paths.map(|s| s.to_vec());

    loop {
        let missing = match &next_scan {
            Some(paths) => find_missing_sonames_in_paths(paths, app_dir)?,
            None => find_missing_sonames(app_dir)?,
        };
        // Drop sonames we already reported as unresolved last iteration — they
        // are still unresolvable and re-querying costs 8 pacman/apt forks each.
        let missing: Vec<String> = missing.into_iter()
            .filter(|s| !already_missing.contains(s))
            .collect();
        if missing.is_empty() {
            break;
        }

        let mut progress = false;
        let mut iter_new_paths: Vec<PathBuf> = Vec::new();
        for soname in &missing {
            match crate::distro::soname_owner(soname) {
                Ok(Some(pkg)) if !visited.contains(&pkg) => {
                    eprintln!("  installing {pkg} (provides {soname})...");
                    let path = download_official(&pkg, cache_dir)
                        .with_context(|| format!("failed to download {pkg}"))?;
                    extract_package(&path, app_dir)
                        .with_context(|| format!("failed to extract {pkg}"))?;
                    for rel in crate::distro::list_pkg_files(&path) {
                        iter_new_paths.push(app_dir.join(rel));
                    }
                    visited.insert(pkg.clone());
                    installed.push(pkg);
                    progress = true;
                }
                Ok(None) => {
                    already_missing.insert(soname.clone());
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
        next_scan = Some(iter_new_paths);
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

/// Like `find_missing_sonames` but only scans the given file paths instead
/// of walking the full `app_dir`. Useful right after a merge install when we
/// have the exact list of files written by the new packages. Soname presence
/// is still checked against the full `app_dir` lib tree — a NEEDED soname is
/// satisfied as long as it exists anywhere under usr/lib, even if it came
/// from an earlier install.
pub fn find_missing_sonames_in_paths(paths: &[PathBuf], app_dir: &Path) -> Result<Vec<String>> {
    let mut needed: HashSet<String> = HashSet::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        if let Ok(libs) = elf_needed(path) {
            needed.extend(libs);
        }
        if is_plugin_host(path) {
            needed.extend(collect_dlopen_sonames(path));
        }
    }
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
                if is_plugin_host(&path) {
                    needed.extend(collect_dlopen_sonames(&path));
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

/// True for files likely to dlopen other libraries at runtime: shared
/// libraries (.so / .so.N) and Node native modules (.node). Executables
/// occasionally do this too but the false-positive cost outweighs the
/// benefit, so we scope to plugin-loading hosts.
fn is_plugin_host(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    name.ends_with(".node") || name.contains(".so")
}

/// Scan an ELF file's string sections for `libfoo.so.N` patterns that look
/// like dlopen targets. These bypass ELF NEEDED entries because dlopen takes
/// its argument as a runtime string, so the linker can't record them.
/// Returns sonames found (no path prefix, version-suffixed only).
fn collect_dlopen_sonames(path: &Path) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let mut magic = [0u8; 4];
    if f.read_exact(&mut magic).is_err() || &magic != b"\x7fELF" {
        return vec![];
    }
    drop(f);
    let Ok(out) = Command::new("strings").args(["-a"]).arg(path).output() else { return vec![] };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut sonames: HashSet<String> = HashSet::new();
    for line in text.lines() {
        if is_versioned_soname(line) {
            sonames.insert(line.to_string());
        }
    }
    sonames.into_iter().collect()
}

/// True if `s` looks like a dlopen-style versioned soname such as
/// "libsndfile.so.1" or "libQt6Core.so.6". The version suffix is required
/// because plain "libfoo.so" strings are usually symlinks or messages, while
/// versioned forms are what apps actually pass to dlopen.
fn is_versioned_soname(s: &str) -> bool {
    if s.len() < 7 || s.len() > 64 || !s.starts_with("lib") {
        return false;
    }
    let Some(idx) = s.find(".so.") else { return false };
    let name = &s[3..idx];
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.'))
    {
        return false;
    }
    let version = &s[idx + 4..];
    !version.is_empty()
        && version
            .split('.')
            .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
}

pub(crate) fn soname_in_app(app_dir: &Path, soname: &str) -> bool {
    for root in &["usr/lib", "usr/lib64", "lib", "lib64"] {
        let dir = app_dir.join(root);
        if !dir.is_dir() {
            continue;
        }
        // Direct — Arch and Fedora /usr/lib64/
        if dir.join(soname).exists() {
            return true;
        }
        // One subdir deep — Debian multiarch (usr/lib/x86_64-linux-gnu/)
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && entry.path().join(soname).exists()
                {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_versioned_soname;
    #[test]
    fn accepts_typical_sonames() {
        assert!(is_versioned_soname("libsndfile.so.1"));
        assert!(is_versioned_soname("libQt6Core.so.6"));
        assert!(is_versioned_soname("libssl.so.3"));
        assert!(is_versioned_soname("libsndfile.so.1.0.37"));
        assert!(is_versioned_soname("libgcc_s.so.1"));
    }
    #[test]
    fn rejects_non_sonames() {
        assert!(!is_versioned_soname("libsndfile.so"));
        assert!(!is_versioned_soname("libsndfile"));
        assert!(!is_versioned_soname("libsndfile.so.x"));
        assert!(!is_versioned_soname("/usr/lib/libsndfile.so.1"));
        assert!(!is_versioned_soname("libsndfile.so.1 not found"));
        assert!(!is_versioned_soname("foo.so.1"));
        assert!(!is_versioned_soname(""));
    }
}
