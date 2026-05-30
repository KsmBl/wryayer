use std::sync::Mutex;
use wryayer::commands::snapshot;
use wryayer::manifest::{write_manifest, AppMeta, Manifest, PackageEntry, PackageSource};

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn with_temp_home(f: impl FnOnce(&std::path::Path)) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let old = std::env::var("HOME").ok();
    std::env::set_var("HOME", tmp.path());
    f(tmp.path());
    match old {
        Some(h) => std::env::set_var("HOME", h),
        None    => std::env::remove_var("HOME"),
    }
}

fn install_fake_app(root: &std::path::Path, app: &str) {
    let app_dir = root.join(format!(".wryayer/{app}"));
    std::fs::create_dir_all(app_dir.join("usr/bin")).unwrap();
    std::fs::write(app_dir.join("usr/bin/main"), b"v1-content").unwrap();
    std::fs::write(app_dir.join("data.txt"), b"hello v1").unwrap();
    write_manifest(app, &Manifest {
        app: AppMeta {
            name:         app.to_string(),
            main_binary:  "main".into(),
            installed_at: "2026-05-16T00:00:00Z".into(),
            launchers:    vec!["main".into()],
            alias_of:     None,
            display_name: None,
            pkg_name:     None,
            wine_game:    None,
        },
        packages: vec![PackageEntry {
            name:    app.to_string(),
            version: "1.0-1".into(),
            source:  PackageSource::Official,
        }],
    }).unwrap();
}

// ── create / list / latest ────────────────────────────────────────────────────

#[test]
fn snapshot_create_returns_label_and_lists() {
    with_temp_home(|root| {
        install_fake_app(root, "testapp");
        let label = snapshot::create("testapp").unwrap();
        assert!(!label.is_empty());

        let labels = snapshot::labels("testapp").unwrap();
        assert_eq!(labels, vec![label.clone()]);
        assert_eq!(snapshot::latest("testapp").unwrap(), Some(label));
    });
}

#[test]
fn snapshot_files_share_inode_with_originals() {
    use std::os::unix::fs::MetadataExt;
    with_temp_home(|root| {
        install_fake_app(root, "testapp");
        let label = snapshot::create("testapp").unwrap();

        let original = root.join(".wryayer/testapp/usr/bin/main");
        let snap_copy = root.join(format!(".wryayer/testapp/.snapshots/{label}/usr/bin/main"));
        assert_eq!(
            std::fs::metadata(&original).unwrap().ino(),
            std::fs::metadata(&snap_copy).unwrap().ino(),
            "snapshot file must share inode with original (hard link)"
        );
    });
}

#[test]
fn snapshot_skips_dotsnapshots_dir() {
    with_temp_home(|root| {
        install_fake_app(root, "testapp");
        let l1 = snapshot::create("testapp").unwrap();
        // Sleep beyond 1s so the timestamp label differs
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let l2 = snapshot::create("testapp").unwrap();
        assert_ne!(l1, l2);

        // The second snapshot must not contain the first's snapshot subtree.
        let nested = root.join(format!(".wryayer/testapp/.snapshots/{l2}/.snapshots"));
        assert!(!nested.exists(), "snapshot must not recurse into .snapshots");
    });
}

#[test]
fn snapshot_latest_when_none_is_none() {
    with_temp_home(|root| {
        install_fake_app(root, "testapp");
        assert_eq!(snapshot::latest("testapp").unwrap(), None);
        assert!(snapshot::labels("testapp").unwrap().is_empty());
    });
}

#[test]
fn snapshot_unknown_app_errors() {
    with_temp_home(|_| {
        assert!(snapshot::create("nope").is_err());
    });
}

// ── rollback ──────────────────────────────────────────────────────────────────

/// Atomic replace via rename: matches how `tar --unlink-first` and pacman-style
/// install flows update files. In-place truncating writes would share the
/// inode with the snapshot's hard-link and silently corrupt the snapshot —
/// see extract.rs `--unlink-first` for the production protection.
fn atomic_write(path: &std::path::Path, content: &[u8]) {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, content).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[test]
fn rollback_restores_modified_file() {
    with_temp_home(|root| {
        install_fake_app(root, "testapp");
        let _label = snapshot::create("testapp").unwrap();

        // Mutate the live app via atomic replace (simulates an update)
        let live = root.join(".wryayer/testapp/data.txt");
        atomic_write(&live, b"hello v2-mutated");
        atomic_write(&root.join(".wryayer/testapp/usr/bin/main"), b"v2-content");
        std::fs::write(root.join(".wryayer/testapp/new-file.txt"), b"created after snap").unwrap();

        // Roll back to the latest snapshot
        snapshot::rollback("testapp", None).unwrap();

        assert_eq!(std::fs::read(&live).unwrap(), b"hello v1");
        assert_eq!(
            std::fs::read(root.join(".wryayer/testapp/usr/bin/main")).unwrap(),
            b"v1-content",
        );
        // Files created after the snapshot must be gone after rollback
        assert!(!root.join(".wryayer/testapp/new-file.txt").exists());
    });
}

#[test]
fn rollback_unknown_label_errors() {
    with_temp_home(|root| {
        install_fake_app(root, "testapp");
        snapshot::create("testapp").unwrap();
        assert!(snapshot::rollback("testapp", Some("does-not-exist")).is_err());
    });
}

#[test]
fn rollback_with_no_snapshots_errors() {
    with_temp_home(|root| {
        install_fake_app(root, "testapp");
        assert!(snapshot::rollback("testapp", None).is_err());
    });
}

#[test]
fn rollback_preserves_snapshots_directory() {
    with_temp_home(|root| {
        install_fake_app(root, "testapp");
        let label = snapshot::create("testapp").unwrap();
        std::fs::write(root.join(".wryayer/testapp/data.txt"), b"mutated").unwrap();

        snapshot::rollback("testapp", None).unwrap();

        // The snapshot we just rolled back to must still exist for future rollbacks
        assert!(root.join(format!(".wryayer/testapp/.snapshots/{label}")).is_dir());
    });
}
