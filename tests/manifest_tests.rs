use std::sync::Mutex;
use wryayer::manifest::*;

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

fn sample_manifest(name: &str) -> Manifest {
    Manifest {
        app: AppMeta {
            name:         name.to_string(),
            main_binary:  name.to_string(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            launchers:    vec![name.to_string()],
            alias_of:     None,
            display_name: None,
            pkg_name:     None,
        },
        packages: vec![PackageEntry {
            name:    name.to_string(),
            version: "1.0-1".to_string(),
            source:  PackageSource::Official,
        }],
    }
}

// ── write + read round-trip ───────────────────────────────────────────────────

#[test]
fn write_then_read_preserves_all_fields() {
    with_temp_home(|root| {
        std::fs::create_dir_all(root.join(".wryayer/testapp")).unwrap();
        let m = sample_manifest("testapp");
        write_manifest("testapp", &m).unwrap();
        let loaded = read_manifest("testapp").unwrap();
        assert_eq!(loaded.app.name,             "testapp");
        assert_eq!(loaded.app.main_binary,      "testapp");
        assert_eq!(loaded.app.launchers,        vec!["testapp"]);
        assert_eq!(loaded.packages[0].version,  "1.0-1");
        assert_eq!(loaded.packages[0].source,   PackageSource::Official);
    });
}

#[test]
fn write_manifest_is_atomic_no_tmp_file_left() {
    with_temp_home(|root| {
        std::fs::create_dir_all(root.join(".wryayer/testapp")).unwrap();
        write_manifest("testapp", &sample_manifest("testapp")).unwrap();
        let tmp = manifest_path("testapp").unwrap().with_extension("toml.tmp");
        assert!(!tmp.exists(), ".tmp file must not remain after successful write");
    });
}

// ── read_manifest error paths ─────────────────────────────────────────────────

#[test]
fn read_manifest_missing_app_returns_err() {
    with_temp_home(|_| {
        assert!(read_manifest("doesnotexist").is_err());
    });
}

#[test]
fn read_manifest_invalid_toml_returns_err() {
    with_temp_home(|root| {
        let dir = root.join(".wryayer/broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".manifest.toml"), b"not valid toml [[[").unwrap();
        assert!(read_manifest("broken").is_err());
    });
}

// ── list_all_apps ─────────────────────────────────────────────────────────────

#[test]
fn list_all_apps_empty_when_root_missing() {
    with_temp_home(|_| {
        assert!(list_all_apps().unwrap().is_empty());
    });
}

#[test]
fn list_all_apps_sorted_by_name() {
    with_temp_home(|root| {
        for name in ["vlc", "firefox", "alacritty"] {
            std::fs::create_dir_all(root.join(format!(".wryayer/{name}"))).unwrap();
            write_manifest(name, &sample_manifest(name)).unwrap();
        }
        let apps = list_all_apps().unwrap();
        let names: Vec<&str> = apps.iter().map(|m| m.app.name.as_str()).collect();
        assert_eq!(names, ["alacritty", "firefox", "vlc"]);
    });
}

#[test]
fn list_all_apps_skips_dirs_without_manifest() {
    with_temp_home(|root| {
        // Orphan dir with no manifest
        std::fs::create_dir_all(root.join(".wryayer/orphan")).unwrap();
        // Corrupt manifest
        let bad = root.join(".wryayer/corrupt");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(".manifest.toml"), b"[garbage").unwrap();
        // One valid app
        std::fs::create_dir_all(root.join(".wryayer/good")).unwrap();
        write_manifest("good", &sample_manifest("good")).unwrap();

        let apps = list_all_apps().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].app.name, "good");
    });
}

#[test]
fn list_all_apps_skips_non_directory_entries() {
    with_temp_home(|root| {
        let wryayer_root = root.join(".wryayer");
        std::fs::create_dir_all(&wryayer_root).unwrap();
        std::fs::write(wryayer_root.join("somefile.txt"), b"hello").unwrap();
        std::fs::create_dir_all(wryayer_root.join("good")).unwrap();
        write_manifest("good", &sample_manifest("good")).unwrap();

        let apps = list_all_apps().unwrap();
        assert_eq!(apps.len(), 1);
    });
}

// ── now_rfc3339 ───────────────────────────────────────────────────────────────

#[test]
fn now_rfc3339_is_parseable() {
    let s = now_rfc3339();
    assert!(
        chrono::DateTime::parse_from_rfc3339(&s).is_ok(),
        "not valid RFC 3339: {s}"
    );
}
