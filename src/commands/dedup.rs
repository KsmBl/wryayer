use crate::manifest::{app_dir, list_all_apps};
use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
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
    let mut unshared_files = 0u64;
    let mut unshared_bytes = 0u64;

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
            // Confirm byte-for-byte equality against one reference file before
            // anything else: DefaultHasher can collide, and a false match would
            // silently corrupt a file. Doing it here rather than per link also
            // keeps the cross-filesystem accounting below honest.
            group.sort_by_key(|f| (f.dev, f.ino));
            let reference = group[0].clone();
            group.retain(|f| f.id() == reference.id() || files_equal(&reference.path, &f.path));
            if group.len() < 2 {
                continue;
            }

            // A hard link cannot cross a filesystem boundary, and every
            // encrypted app is its own ext4 volume inside its own container.
            // Linking is therefore only ever attempted within one filesystem;
            // trying across them would fail with EXDEV on every single file.
            let buckets = by_device(group);
            // Every filesystem past the first keeps its own full copy of the
            // content. That is space dedup can see but can never reclaim, so
            // it is reported apart from the savings rather than hidden.
            if buckets.len() > 1 {
                let extra = buckets.len() as u64 - 1;
                unshared_files += extra;
                unshared_bytes += size * extra;
            }

            for bucket in buckets.into_values() {
                // Lowest inode = canonical (oldest on disk, keeps the inode
                // alive longest).
                let canonical = bucket[0].clone();

                for dup in &bucket[1..] {
                    if dup.id() == canonical.id() {
                        continue; // already hard-linked
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
    }

    for line in summary_lines(links_created, bytes_saved, errors, unshared_files, unshared_bytes) {
        eprintln!("  {line}");
    }
    Ok(())
}

/// The closing report, as the lines to print (without indentation).
fn summary_lines(
    links: u64,
    saved: u64,
    errors: u32,
    unshared_files: u64,
    unshared_bytes: u64,
) -> Vec<String> {
    let mut out = Vec::new();
    if links > 0 || errors > 0 {
        out.push(format!(
            "{} files hard-linked, {} recovered{}",
            links,
            format_bytes(saved),
            if errors > 0 { format!(" ({errors} skipped)") } else { String::new() },
        ));
    }
    if unshared_bytes > 0 {
        // Not an error and not something the user can act on by re-running:
        // it is the standing cost of keeping apps in separate containers.
        out.push(format!(
            "{} in {unshared_files} files stays duplicated across container \
             boundaries — hard links cannot span filesystems",
            format_bytes(unshared_bytes),
        ));
    }
    if out.is_empty() {
        out.push("No duplicate files found.".to_string());
    }
    out
}

/// Split content-identical files into one bucket per filesystem, each sorted so
/// the lowest inode comes first.
///
/// Buckets are the unit hard-linking works on: within one there is a single
/// canonical inode every other name can point at; between them nothing can be
/// shared at all.
fn by_device(files: Vec<FileRef>) -> BTreeMap<u64, Vec<FileRef>> {
    let mut buckets: BTreeMap<u64, Vec<FileRef>> = BTreeMap::new();
    for file in files {
        buckets.entry(file.dev).or_default().push(file);
    }
    for bucket in buckets.values_mut() {
        bucket.sort_by_key(|f| f.ino);
    }
    buckets
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

    #[test]
    fn files_are_bucketed_per_filesystem() {
        // Two plain apps on the host filesystem plus two encrypted ones, each
        // its own volume: only the first pair can ever be linked together.
        let buckets = by_device(vec![
            file(1, 30, "/plain-a/lib.so"),
            file(1, 20, "/plain-b/lib.so"),
            file(7, 11, "/enc-a/lib.so"),
            file(9, 11, "/enc-b/lib.so"),
        ]);

        assert_eq!(buckets.len(), 3, "one bucket per filesystem");
        assert_eq!(buckets[&1].len(), 2);
        assert_eq!(buckets[&7].len(), 1);
        assert_eq!(buckets[&9].len(), 1);
    }

    #[test]
    fn the_lowest_inode_leads_each_bucket() {
        // The canonical file is whichever the bucket lists first, so the sort
        // is what makes "keep the oldest inode" true.
        let buckets = by_device(vec![
            file(1, 50, "/a/lib.so"),
            file(1, 9, "/b/lib.so"),
            file(1, 33, "/c/lib.so"),
        ]);
        assert_eq!(buckets[&1].iter().map(|f| f.ino).collect::<Vec<_>>(), vec![9, 33, 50]);
    }

    #[test]
    fn nothing_found_is_still_reported() {
        assert_eq!(summary_lines(0, 0, 0, 0, 0), vec!["No duplicate files found."]);
    }

    #[test]
    fn cross_container_duplicates_are_not_reported_as_nothing_found() {
        // The whole point: an encrypted app duplicating a plain one used to
        // read as "No duplicate files found", which is the opposite of true.
        let lines = summary_lines(0, 0, 0, 3, 6 * 1024 * 1024);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("6.0 MiB"), "{}", lines[0]);
        assert!(lines[0].contains("3 files"), "{}", lines[0]);
        assert!(!lines[0].contains("No duplicate"));
    }

    #[test]
    fn savings_and_unshareable_space_are_reported_separately() {
        let lines = summary_lines(4, 1024, 0, 2, 2048);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("4 files hard-linked"), "{}", lines[0]);
        assert!(lines[1].contains("container boundaries"), "{}", lines[1]);
    }

    #[test]
    fn genuine_failures_still_show_a_skipped_count() {
        // EXDEV no longer lands here, so a non-zero count now means something
        // actually went wrong and is worth surfacing.
        let lines = summary_lines(1, 512, 2, 0, 0);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("(2 skipped)"), "{}", lines[0]);
    }

    #[test]
    fn a_single_file_bucket_has_no_duplicates_to_link() {
        // The linking loop skips bucket[0] and iterates bucket[1..]; a lone
        // file must not panic or be linked to itself.
        let buckets = by_device(vec![file(4, 1, "/only/lib.so")]);
        assert!(buckets[&4][1..].is_empty());
    }
}
