use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use wryayer::commands::dedup::*;

// ── format_bytes — 4 equivalence classes + boundary values ───────────────────

#[test]
fn format_bytes_zero() {
    assert_eq!(format_bytes(0), "0 B");
}

#[test]
fn format_bytes_bytes_ec() {
    // EC1: 0 ≤ n < 1024 → "N B"  (boundaries: 1 and 1023)
    assert_eq!(format_bytes(1), "1 B");
    assert_eq!(format_bytes(1023), "1023 B");
}

#[test]
fn format_bytes_kib_ec() {
    // EC2: 1024 ≤ n < 1 048 576 → "N.N KiB"  (boundaries: 1024 and 1048575)
    assert_eq!(format_bytes(1024), "1.0 KiB");
    assert_eq!(format_bytes(1536), "1.5 KiB");
    assert!(format_bytes(1024 * 1024 - 1).ends_with("KiB"));
}

#[test]
fn format_bytes_mib_ec() {
    // EC3: 1 MiB ≤ n < 1 GiB → "N.N MiB"  (boundaries: 1 MiB and 1 GiB-1)
    assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
    assert_eq!(format_bytes(512 * 1024 * 1024), "512.0 MiB");
    assert!(format_bytes(1024u64 * 1024 * 1024 - 1).ends_with("MiB"));
}

#[test]
fn format_bytes_gib_ec() {
    // EC4: n ≥ 1 GiB → "N.NN GiB"  (boundaries: exactly 1 GiB and 2 GiB)
    assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GiB");
    assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
}

// ── du_walk — SKIP_DIRS filtering ────────────────────────────────────────────

#[test]
fn du_walk_skips_home_dir() {
    // "home" is in SKIP_DIRS
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("home")).unwrap();
    std::fs::write(tmp.path().join("home/secret.txt"), b"12345").unwrap(); // 5 bytes — must not be counted
    std::fs::write(tmp.path().join("visible.txt"), b"hi").unwrap();       // 2 bytes — must be counted

    let (mut apparent, mut actual) = (0u64, 0u64);
    du_walk(tmp.path(), &mut apparent, &mut actual, &mut HashSet::new());

    assert_eq!(apparent, 2);
}

#[test]
fn du_walk_skips_tmp_dir() {
    // ".tmp" is also in SKIP_DIRS
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".tmp")).unwrap();
    std::fs::write(tmp.path().join(".tmp/junk"), b"xxxxx").unwrap();
    std::fs::write(tmp.path().join("real.bin"), b"ab").unwrap();

    let (mut apparent, mut actual) = (0u64, 0u64);
    du_walk(tmp.path(), &mut apparent, &mut actual, &mut HashSet::new());

    assert_eq!(apparent, 2);
}

// ── du_walk — hard-link accounting ───────────────────────────────────────────

#[test]
fn du_walk_apparent_double_counts_hard_links_actual_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file_a.bin"), b"shared content!").unwrap(); // 15 bytes
    std::fs::hard_link(
        tmp.path().join("file_a.bin"),
        tmp.path().join("file_b.bin"),
    ).unwrap();

    let (mut apparent, mut actual) = (0u64, 0u64);
    du_walk(tmp.path(), &mut apparent, &mut actual, &mut HashSet::new());

    assert_eq!(apparent, 30, "apparent must count both hard-link copies");
    assert_eq!(actual,   15, "actual must count unique inode only once");
}

#[test]
fn du_walk_unique_files_have_equal_apparent_and_actual() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"aaa").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"bb").unwrap();

    let (mut apparent, mut actual) = (0u64, 0u64);
    du_walk(tmp.path(), &mut apparent, &mut actual, &mut HashSet::new());

    assert_eq!(apparent, 5);
    assert_eq!(actual,   5, "no hard links → apparent == actual");
}

#[test]
fn du_walk_empty_dir_yields_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut apparent, mut actual) = (0u64, 0u64);
    du_walk(tmp.path(), &mut apparent, &mut actual, &mut HashSet::new());
    assert_eq!(apparent, 0);
    assert_eq!(actual,   0);
}

// ── atomic_hard_link ──────────────────────────────────────────────────────────

#[test]
fn atomic_hard_link_dup_shares_inode_with_canonical() {
    let tmp = tempfile::tempdir().unwrap();
    let canonical = tmp.path().join("canonical.bin");
    let dup       = tmp.path().join("dup.bin");
    std::fs::write(&canonical, b"canonical data").unwrap();
    std::fs::write(&dup,       b"old dup data").unwrap();

    atomic_hard_link(&canonical, &dup).unwrap();

    let ino_c = std::fs::metadata(&canonical).unwrap().ino();
    let ino_d = std::fs::metadata(&dup).unwrap().ino();
    assert_eq!(ino_c, ino_d, "dup must share inode with canonical");
    assert_eq!(std::fs::read(&dup).unwrap(), b"canonical data");
}

#[test]
fn atomic_hard_link_leaves_no_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let canonical = tmp.path().join("canonical.bin");
    let dup       = tmp.path().join("dup.bin");
    std::fs::write(&canonical, b"data").unwrap();
    std::fs::write(&dup,       b"old").unwrap();

    atomic_hard_link(&canonical, &dup).unwrap();

    assert!(!tmp.path().join("dup.bin.wry_dedup").exists(), "temp file must be cleaned up");
}
