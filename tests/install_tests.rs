use wryayer::commands::install::ensure_base_layout;

// ── ensure_base_layout ────────────────────────────────────────────────────────

#[test]
fn ensure_base_layout_creates_all_filesystem_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_base_layout(tmp.path()).unwrap();

    let expected: &[(&str, &str)] = &[
        ("lib",      "usr/lib"),
        ("lib64",    "usr/lib"),
        ("bin",      "usr/bin"),
        ("sbin",     "usr/bin"),
        ("usr/sbin", "bin"),
    ];
    for (path, want_target) in expected {
        let full = tmp.path().join(path);
        let target = std::fs::read_link(&full)
            .unwrap_or_else(|e| panic!("missing symlink {path}: {e}"));
        assert_eq!(target.to_string_lossy(), *want_target);
    }
}

#[test]
fn ensure_base_layout_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_base_layout(tmp.path()).unwrap();
    // Second call must succeed and not error on existing symlinks
    ensure_base_layout(tmp.path()).unwrap();
    // exists() follows symlinks; the target dir is empty here so use symlink_metadata
    assert!(tmp.path().join("lib64").symlink_metadata().is_ok());
}

#[test]
fn ensure_base_layout_preserves_existing_real_directory() {
    let tmp = tempfile::tempdir().unwrap();
    // Pre-existing real /lib directory must not be replaced
    std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
    std::fs::write(tmp.path().join("lib/sentinel"), b"keepme").unwrap();

    ensure_base_layout(tmp.path()).unwrap();

    assert!(tmp.path().join("lib/sentinel").exists(),
        "real /lib must not be clobbered by symlink creation");
    // The other expected symlinks are still created
    assert!(tmp.path().join("lib64").symlink_metadata().is_ok());
}

#[test]
fn ensure_base_layout_creates_parent_for_usr_sbin() {
    let tmp = tempfile::tempdir().unwrap();
    // usr/ doesn't exist yet — function must create it for usr/sbin -> bin
    ensure_base_layout(tmp.path()).unwrap();
    assert!(tmp.path().join("usr").is_dir());
    let target = std::fs::read_link(tmp.path().join("usr/sbin")).unwrap();
    assert_eq!(target.to_string_lossy(), "bin");
}

#[test]
fn ensure_base_layout_creates_sandbox_home_dir() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_base_layout(tmp.path()).unwrap();

    if let Ok(home_val) = std::env::var("HOME") {
        let username = home_val.trim_end_matches('/').rsplit('/').next().unwrap_or("user");
        let sandbox_home = tmp.path().join("home").join(username);
        assert!(sandbox_home.is_dir(),
            "home/{username} should be a directory so browsers can store profiles");
    }
}

#[test]
fn ensure_base_layout_home_dir_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_base_layout(tmp.path()).unwrap();
    // Second call must not error even though home/<username> already exists.
    ensure_base_layout(tmp.path()).unwrap();
}
