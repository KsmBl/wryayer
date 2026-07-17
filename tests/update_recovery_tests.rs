// Crash-safety of `wryayer update`: an update applies the new tree with two
// atomic renames, and recover_interrupted_update() heals any interruption
// (Ctrl-C, kill, power loss) between them. These tests reproduce each possible
// on-disk state left by such an interruption and assert the tree ends up
// consistent — either fully the new version or fully the old one, never broken.

use std::path::Path;
use std::sync::Mutex;
use wryayer::commands::update::recover_interrupted_update;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn with_temp_home(f: impl FnOnce(&Path)) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let old = std::env::var("HOME").ok();
    std::env::set_var("HOME", tmp.path());
    f(tmp.path());
    match old {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

// Old tree with package files AND the user data an update must preserve.
fn make_old_tree(dir: &Path) {
    write(&dir.join("usr/bin/main"), "v1-binary");
    write(&dir.join(".manifest.toml"), "version = 1");
    write(&dir.join("config.ini"), "user-config");
    write(&dir.join("home/user/profile"), "browser-profile");
    write(&dir.join(".snapshots/snap1"), "rollback-point");
}

// New tree as it exists in the freshly-swapped-in app dir: only package files
// plus the new manifest — no user data yet (that gets carried over).
fn make_new_tree(dir: &Path) {
    write(&dir.join("usr/bin/main"), "v2-binary");
    write(&dir.join(".manifest.toml"), "version = 2");
}

// Interrupted after the old tree was parked as backup but before the new tree
// landed: recovery must restore the untouched old version.
#[test]
fn rolls_back_when_new_tree_never_landed() {
    with_temp_home(|home| {
        let root = home.join(".wryayer");
        let app = root.join("foo");
        let backup = root.join(".foo.wr-old");

        make_old_tree(&backup); // old tree parked aside
        // app dir absent — the swap-in rename never ran.

        recover_interrupted_update("foo").unwrap();

        assert!(!backup.exists(), "backup should be consumed");
        assert_eq!(std::fs::read_to_string(app.join("usr/bin/main")).unwrap(), "v1-binary");
        assert_eq!(std::fs::read_to_string(app.join("config.ini")).unwrap(), "user-config");
        assert_eq!(
            std::fs::read_to_string(app.join("home/user/profile")).unwrap(),
            "browser-profile"
        );
    });
}

// Interrupted after the new tree was swapped in but before user data was
// carried over: recovery must finish forward — new package files, old user data.
#[test]
fn finishes_forward_when_new_tree_is_in_place() {
    with_temp_home(|home| {
        let root = home.join(".wryayer");
        let app = root.join("foo");
        let backup = root.join(".foo.wr-old");

        make_new_tree(&app); // new tree already in place, no user data
        make_old_tree(&backup); // old tree still holds the user data

        recover_interrupted_update("foo").unwrap();

        assert!(!backup.exists(), "backup should be consumed");
        // Package files and manifest are the new version...
        assert_eq!(std::fs::read_to_string(app.join("usr/bin/main")).unwrap(), "v2-binary");
        assert_eq!(std::fs::read_to_string(app.join(".manifest.toml")).unwrap(), "version = 2");
        // ...and the user data survived the update.
        assert_eq!(std::fs::read_to_string(app.join("config.ini")).unwrap(), "user-config");
        assert_eq!(
            std::fs::read_to_string(app.join("home/user/profile")).unwrap(),
            "browser-profile"
        );
        assert_eq!(std::fs::read_to_string(app.join(".snapshots/snap1")).unwrap(), "rollback-point");
    });
}

// Interrupted before anything destructive happened (still extracting into
// staging): the live tree is the intact old version and the half-built staging
// is junk to discard.
#[test]
fn discards_staging_and_keeps_live_tree() {
    with_temp_home(|home| {
        let root = home.join(".wryayer");
        let app = root.join("foo");
        let staging = root.join(".foo.wr-new");

        make_old_tree(&app); // live tree untouched
        write(&staging.join("usr/bin/main"), "half-extracted"); // partial staging

        recover_interrupted_update("foo").unwrap();

        assert!(!staging.exists(), "staging junk should be removed");
        assert_eq!(std::fs::read_to_string(app.join("usr/bin/main")).unwrap(), "v1-binary");
        assert_eq!(std::fs::read_to_string(app.join("config.ini")).unwrap(), "user-config");
    });
}

// Recovery must be a no-op on a normal, fully-applied tree (no swap dirs).
#[test]
fn no_op_when_nothing_was_interrupted() {
    with_temp_home(|home| {
        let root = home.join(".wryayer");
        let app = root.join("foo");
        make_old_tree(&app);

        recover_interrupted_update("foo").unwrap();

        assert_eq!(std::fs::read_to_string(app.join("usr/bin/main")).unwrap(), "v1-binary");
    });
}
