use crate::manifest::{app_dir, list_all_apps};
use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const SKIP_DIRS: &[&str] = &["home", ".tmp", ".snapshots"];
const SKIP_FILES: &[&str] = &[".manifest.toml", "config.ini", ".instance.pid"];

/// One candidate file, identified the way the kernel identifies files: a
/// filesystem plus an inode number within it.
///
/// The inode alone is not an identity. Inode numbers are only unique per
/// filesystem, and every encrypted app lives on its own ext4 volume inside a
/// VeraCrypt container — so two unrelated files in two different apps routinely
/// share an inode number. Carrying the device alongside keeps "these are
/// already the same file" from being answered by coincidence.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FileRef {
    dev: u64,
    ino: u64,
    path: PathBuf,
}

impl FileRef {
    fn id(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

pub fn run(verbose: bool) -> Result<()> {
    let apps = list_all_apps()?;
    if apps.len() < 2 {
        eprintln!("need at least 2 installed apps to deduplicate");
        return Ok(());
    }

    eprintln!("Deduplicating shared files across {} apps...", apps.len());

    // size → candidate files of exactly that size
    let mut by_size: HashMap<u64, Vec<FileRef>> = HashMap::new();
    let mut total_files = 0u64;

    for app in &apps {
        let dir = app_dir(&app.app.name)?;
        collect_files(&dir, &mut by_size, &mut total_files);
    }

    eprintln!("  {total_files} files indexed");

    let mut links_created = 0u64;
    let mut bytes_saved = 0u64;
    let mut errors = 0u32;

    for (size, entries) in &by_size {
        if entries.len() < 2 {
            continue;
        }
        // If every entry is already the same file, nothing to do.
        if all_same_file(entries) {
            continue;
        }

        // Group by content hash.
        let mut by_hash: HashMap<u64, Vec<FileRef>> = HashMap::new();
        for file in entries {
            if let Ok(h) = hash_file(&file.path) {
                by_hash.entry(h).or_default().push(file.clone());
            }
        }

        for (_hash, mut group) in by_hash {
            if group.len() < 2 {
                continue;
            }
            // Lowest inode = canonical (oldest on disk, keeps the inode alive longest).
            group.sort_by_key(|f| f.ino);
            let canonical = group[0].clone();

            for dup in &group[1..] {
                if dup.id() == canonical.id() {
                    continue; // already hard-linked
                }
                // Verify byte-for-byte equality before linking: DefaultHasher can
                // collide, and a false match would silently corrupt a file.
                if !files_equal(&canonical.path, &dup.path) {
                    continue;
                }
                match atomic_hard_link(&canonical.path, &dup.path) {
                    Ok(()) => {
                        links_created += 1;
                        bytes_saved += size;
                        if verbose {
                            eprintln!("  linked {}", dup.path.display());
                        }
                    }
                    Err(e) => {
                        if verbose {
                            eprintln!("  warning: {}: {e:#}", dup.path.display());
                        }
                        errors += 1;
                    }
                }
            }
        }
    }

    if links_created == 0 && errors == 0 {
        eprintln!("  No duplicate files found.");
    } else {
        eprintln!(
            "  {} files hard-linked, {} recovered{}",
            links_created,
            format_bytes(bytes_saved),
            if errors > 0 { format!(" ({errors} skipped)") } else { String::new() },
        );
    }
    Ok(())
}

/// Whether every candidate is already one and the same file on disk, in which
/// case hashing and linking it has nothing left to do.
fn all_same_file(files: &[FileRef]) -> bool {
    match files.first() {
        Some(first) => files.iter().all(|f| f.id() == first.id()),
        None => true,
    }
}

// ── Disk-usage accounting ─────────────────────────────────────────────────────

/// Walk all installed apps once and return:
///   - per-app apparent size (each file in the app counted once)
///   - total apparent size (sum of per-app sizes)
///   - total actual size (each unique (dev, ino) pair counted only once,
///     so hard-linked files shared between apps are not double-counted)
pub fn all_du() -> Result<(HashMap<String, u64>, u64, u64)> {
    let apps = list_all_apps()?;
    let mut per_app: HashMap<String, u64> = HashMap::new();
    let mut total_apparent = 0u64;
    let mut total_actual = 0u64;
    let mut seen: HashSet<(u64, u64)> = HashSet::new(); // (dev, ino)

    for app in &apps {
        let dir = app_dir(&app.app.name)?;
        let mut app_apparent = 0u64;
        du_walk(&dir, &mut app_apparent, &mut total_actual, &mut seen);
        per_app.insert(app.app.name.clone(), app_apparent);
        total_apparent += app_apparent;
    }

    Ok((per_app, total_apparent, total_actual))
}

pub fn du_walk(dir: &Path, apparent: &mut u64, actual: &mut u64, seen: &mut HashSet<(u64, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let name = entry.file_name();
            if !SKIP_DIRS.iter().any(|&s| s == name.to_string_lossy().as_ref()) {
                du_walk(&entry.path(), apparent, actual, seen);
            }
        } else if meta.is_file() {
            let size = meta.len();
            *apparent += size;
            if seen.insert((meta.dev(), meta.ino())) {
                *actual += size;
            }
        }
    }
}

// ── File collection ───────────────────────────────────────────────────────────

fn collect_files(dir: &Path, by_size: &mut HashMap<u64, Vec<FileRef>>, count: &mut u64) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        // DirEntry::metadata() uses lstat on Unix — does not follow symlinks.
        let Ok(meta) = entry.metadata() else { continue };
        let path = entry.path();

        if meta.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !SKIP_DIRS.iter().any(|&s| s == name.as_ref()) {
                collect_files(&path, by_size, count);
            }
            continue;
        }

        if !meta.is_file() {
            continue; // skip symlinks and special files
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_FILES.iter().any(|&s| s == name.as_ref()) {
            continue;
        }

        let size = meta.len();
        if size == 0 {
            continue;
        }

        by_size
            .entry(size)
            .or_default()
            .push(FileRef { dev: meta.dev(), ino: meta.ino(), path });
        *count += 1;
    }
}

// ── Hashing ───────────────────────────────────────────────────────────────────

fn hash_file(path: &Path) -> Result<u64, std::io::Error> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = DefaultHasher::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        // Include the chunk length in the hash so that differently-split
        // reads of identical content always produce the same value.
        n.hash(&mut hasher);
        buf[..n].hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn files_equal(a: &Path, b: &Path) -> bool {
    let Ok(fa) = std::fs::File::open(a) else { return false };
    let Ok(fb) = std::fs::File::open(b) else { return false };
    let mut ra = BufReader::new(fa);
    let mut rb = BufReader::new(fb);
    loop {
        let ba = ra.fill_buf().unwrap_or(&[]);
        let bb = rb.fill_buf().unwrap_or(&[]);
        let len = ba.len().min(bb.len());
        if len == 0 {
            return ba.len() == bb.len();
        }
        if ba[..len] != bb[..len] {
            return false;
        }
        ra.consume(len);
        rb.consume(len);
    }
}

// ── Hard-link replacement ─────────────────────────────────────────────────────

/// Atomically replace `dup` with a hard link to `canonical` using a
/// sibling temp file + rename so the path is never absent.
pub fn atomic_hard_link(canonical: &Path, dup: &Path) -> Result<()> {
    let mut tmp_name = dup.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".wry_dedup");
    let tmp = dup.parent().unwrap_or(Path::new(".")).join(tmp_name);

    let _ = std::fs::remove_file(&tmp);
    std::fs::hard_link(canonical, &tmp).map_err(|e| {
        anyhow::anyhow!("hard_link {} → {}: {e}", canonical.display(), tmp.display())
    })?;
    std::fs::rename(&tmp, dup).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("rename {} → {}: {e}", tmp.display(), dup.display())
    })?;
    Ok(())
}

// ── Formatting ────────────────────────────────────────────────────────────────

pub fn format_bytes(n: u64) -> String {
    const K: u64 = 1024;
    if n < K {
        format!("{n} B")
    } else if n < K * K {
        format!("{:.1} KiB", n as f64 / K as f64)
    } else if n < K * K * K {
        format!("{:.1} MiB", n as f64 / (K * K) as f64)
    } else {
        format!("{:.2} GiB", n as f64 / (K * K * K) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dev: u64, ino: u64, path: &str) -> FileRef {
        FileRef { dev, ino, path: PathBuf::from(path) }
    }

    #[test]
    fn identical_inodes_on_different_devices_are_different_files() {
        // Each encrypted app is its own ext4 volume, so inode 12 in one app has
        // nothing to do with inode 12 in another. Treating them as one file
        // would skip a pair that genuinely could be deduplicated.
        let files = vec![file(1, 12, "/a/lib.so"), file(2, 12, "/b/lib.so")];
        assert!(!all_same_file(&files));
    }

    #[test]
    fn the_same_inode_on_one_device_is_one_file() {
        let files = vec![file(1, 12, "/a/lib.so"), file(1, 12, "/a/also-lib.so")];
        assert!(all_same_file(&files));
    }
}
