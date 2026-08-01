use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use wryayer::launcher::*;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn with_temp_home(f: impl FnOnce(&std::path::Path)) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let old = std::env::var("HOME").ok();
    let old_dir = std::env::var("WRYAYER_LAUNCHER_DIR").ok();
    std::env::set_var("HOME", tmp.path());
    // Shortcuts are system-wide in normal use. A test must never go near
    // /usr/bin: writing there needs root, and a test that asks for root either
    // hangs waiting for a password or damages the machine it runs on.
    std::env::set_var("WRYAYER_LAUNCHER_DIR", tmp.path().join("bin"));
    std::fs::create_dir_all(tmp.path().join("bin")).unwrap();
    f(tmp.path());
    match old {
        Some(h) => std::env::set_var("HOME", h),
        None    => std::env::remove_var("HOME"),
    }
    match old_dir {
        Some(d) => std::env::set_var("WRYAYER_LAUNCHER_DIR", d),
        None    => std::env::remove_var("WRYAYER_LAUNCHER_DIR"),
    }
}

// ── create_launcher ───────────────────────────────────────────────────────────

#[test]
fn create_launcher_returns_correct_path() {
    with_temp_home(|root| {
        let path = create_launcher("firefox", "firefox").unwrap();
        assert_eq!(path, root.join("bin/firefox"));
    });
}

#[test]
fn create_launcher_file_is_executable() {
    with_temp_home(|_| {
        let path = create_launcher("myapp", "myapp").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "launcher must be executable, mode={mode:o}");
    });
}

#[test]
fn create_launcher_content_has_shebang_and_marker() {
    with_temp_home(|_| {
        let path = create_launcher("firefox", "firefox").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("#!/bin/bash"), "must have bash shebang");
        assert!(content.contains("# wryayer managed launcher"), "must have identity marker");
    });
}

#[test]
fn create_launcher_references_app_name() {
    with_temp_home(|_| {
        let path = create_launcher("firefox", "firefox").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("firefox"), "app name must appear in launcher");
    });
}

#[test]
fn create_launcher_forwards_all_args() {
    with_temp_home(|_| {
        let path = create_launcher("myapp", "myapp").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(r#""$@""#), "must forward all arguments");
    });
}

#[test]
fn create_launcher_binary_name_differs_from_app_name() {
    with_temp_home(|root| {
        let path = create_launcher("python312", "python3.12").unwrap();
        assert_eq!(path, root.join("bin/python3.12"), "launcher must use binary name");
        let content = std::fs::read_to_string(&path).unwrap();
        // The run command must use the app dir name, not the binary name
        assert!(content.contains(r#"run "python312""#));
    });
}

// ── remove_launcher ───────────────────────────────────────────────────────────

#[test]
fn remove_launcher_missing_file_is_ok() {
    with_temp_home(|_| {
        assert!(remove_launcher("no-such-app").is_ok());
    });
}

#[test]
fn remove_launcher_skips_non_wryayer_file() {
    with_temp_home(|root| {
        let path = root.join("bin/myapp");
        std::fs::write(&path, "#!/bin/bash\necho hello\n").unwrap();
        assert!(remove_launcher("myapp").is_ok());
        assert!(path.exists(), "non-wryayer file must not be deleted");
    });
}

#[test]
fn remove_launcher_deletes_wryayer_launcher() {
    with_temp_home(|_| {
        let path = create_launcher("myapp", "myapp").unwrap();
        assert!(path.exists());
        remove_launcher("myapp").unwrap();
        assert!(!path.exists(), "wryayer launcher must be deleted");
    });
}

#[test]
fn remove_launcher_is_idempotent() {
    with_temp_home(|_| {
        let path = create_launcher("myapp", "myapp").unwrap();
        remove_launcher("myapp").unwrap();
        assert!(!path.exists());
        // Second call must also succeed (file already gone)
        assert!(remove_launcher("myapp").is_ok());
    });
}
