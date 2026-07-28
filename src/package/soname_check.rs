use crate::package::{download_official, extract_package};
use anyhow::{Context, Result};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often a long-running phase reports what it is doing.
///
/// The soname pass has two phases that can run for a long time with nothing to
/// say — walking a 400-package tree parsing every ELF, and querying the package
/// manager for each missing soname (several forks per query). Without output the
/// TUI just sits on "Checking for missing shared library dependencies…" and
/// looks hung. Fast enough to feel live, slow enough not to flood the TUI's log
/// channel with thousands of lines.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(400);

/// Rate-limited progress reporter for a phase with no natural line output.
struct Ticker {
    last: Instant,
}

impl Ticker {
    fn new() -> Self {
        // Start in the past so the first tick fires immediately: the user should
        // see the phase begin, not wait out the first interval.
        Self { last: Instant::now() - PROGRESS_INTERVAL }
    }

    /// Print `msg()` if enough time has passed. The closure means the message is
    /// only formatted when it will actually be shown.
    fn tick(&mut self, msg: impl FnOnce() -> String) {
        if self.last.elapsed() >= PROGRESS_INTERVAL {
            eprintln!("{}", msg());
            self.last = Instant::now();
        }
    }
}

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
    // Per-soname progress/warnings are a wall of output on big apps (Steam et al).
    // Off by default: collapse to a one-line summary, full detail behind the env.
    let verbose = std::env::var_os("WRYAYER_VERBOSE").is_some();
    let mut installed: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut unresolved: HashSet<String> = HashSet::new();
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
            .filter(|s| !unresolved.contains(s))
            .collect();
        if missing.is_empty() {
            break;
        }

        let mut progress = false;
        let mut iter_new_paths: Vec<PathBuf> = Vec::new();
        let total = missing.len();
        for (i, soname) in missing.iter().enumerate() {
            // Not throttled: each lookup shells out to the package manager
            // several times and can take a second or more, so one line per
            // soname *is* the natural pace — and it names exactly what is being
            // resolved right now.
            eprintln!("  [{}/{total}] looking up {soname}", i + 1);
            match crate::distro::soname_owner(soname) {
                Ok(Some(pkg)) if !visited.contains(&pkg) => {
                    // Worth showing unconditionally: it is bounded by the number
                    // of packages actually installed, and it is the one part of
                    // this phase that changes the app.
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
                    unresolved.insert(soname.clone());
                }
                Ok(_) => {}
                Err(e) => {
                    if verbose {
                        eprintln!("  warning: soname lookup for {soname}: {e:#}");
                    }
                    unresolved.insert(soname.clone());
                }
            }
        }

        if !progress {
            for soname in &missing {
                unresolved.insert(soname.clone());
            }
            break;
        }
        next_scan = Some(iter_new_paths);
    }

    if !installed.is_empty() || !unresolved.is_empty() {
        let mut msg = format!("  sonames: installed {} package(s)", installed.len());
        if !unresolved.is_empty() {
            let mut names: Vec<&str> = unresolved.iter().map(String::as_str).collect();
            names.sort_unstable();
            if verbose {
                msg.push_str(&format!(", {} unresolved: {}", names.len(), names.join(", ")));
            } else {
                msg.push_str(&format!(
                    ", {} unresolved (set WRYAYER_VERBOSE=1 for the list)",
                    names.len()
                ));
            }
        }
        eprintln!("{msg}");
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
    let mut ticker = Ticker::new();
    let total = paths.len();
    for (i, path) in paths.iter().enumerate() {
        if !path.is_file() {
            continue;
        }
        ticker.tick(|| format!("  scanning new files ({}/{total})", i + 1));
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

    let mut scanned: u64 = 0;
    let mut ticker = Ticker::new();

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
                scanned += 1;
                // Name the directory rather than the file: on a big tree the
                // filename changes far too fast to read, while the directory
                // shows real progress through the app.
                ticker.tick(|| {
                    let where_ = dir
                        .strip_prefix(start_dir)
                        .unwrap_or(&dir)
                        .to_string_lossy()
                        .into_owned();
                    let where_ = if where_.is_empty() { ".".into() } else { where_ };
                    format!("  scanning {where_} ({scanned} files read)")
                });
                if let Ok(libs) = elf_needed(&path) {
                    needed.extend(libs);
                }
                if is_plugin_host(&path) {
                    needed.extend(collect_dlopen_sonames(&path));
                }
            }
        }
    }

    if scanned > 0 {
        eprintln!("  scanned {scanned} files, {} distinct libraries required", needed.len());
    }
    Ok(needed)
}


fn elf_needed(path: &Path) -> Result<Vec<String>> {
    let Ok(data) = std::fs::read(path) else { return Ok(vec![]) };
    if !data.starts_with(b"\x7fELF") {
        return Ok(vec![]);
    }
    // goblin reads the DT_NEEDED entries directly from the dynamic section.
    // A parse error (truncated/exotic ELF) is treated as "no deps" — same as
    // the old readelf path, which produced nothing on such files.
    match goblin::elf::Elf::parse(&data) {
        Ok(elf) => Ok(elf.libraries.iter().map(|s| s.to_string()).collect()),
        Err(_) => Ok(vec![]),
    }
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
    let Ok(data) = std::fs::read(path) else { return vec![] };
    if !data.starts_with(b"\x7fELF") {
        return vec![];
    }
    // Replicate `strings -a`: every maximal run of printable ASCII bytes (space
    // included, matching GNU strings' isprint runs) is a candidate; keep only
    // the ones shaped like a versioned dlopen soname.
    let mut sonames: HashSet<String> = HashSet::new();
    let mut run_start = 0usize;
    for i in 0..=data.len() {
        let printable = i < data.len() && matches!(data[i], 0x20..=0x7e);
        if printable {
            continue;
        }
        if i > run_start {
            if let Ok(tok) = std::str::from_utf8(&data[run_start..i]) {
                if is_versioned_soname(tok) {
                    sonames.insert(tok.to_string());
                }
            }
        }
        run_start = i + 1;
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
