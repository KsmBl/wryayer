// Crash-safety of `wryayer update`, in both the forms the swap takes: two
// atomic renames beside a plaintext app's tree, and a move of its top-level
// entries for an encrypted one, whose directory is a mount point that cannot be
// renamed. recover_interrupted_update() heals any interruption (Ctrl-C, kill,
// power loss) in either. These tests reproduce each possible on-disk state left
// by such an interruption and assert the tree ends up consistent — either fully
// the new version or fully the old one, never broken.

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

// ── The in-place swap, used when the tree is a container mount point ─────────
//
// There the app dir cannot be renamed, so the update moves top-level entries
// into `.wr-old` and then moves the staged ones in from `.wr-new`. Both halves
// leave entries spread across all three places, so `.wr-phase` records which
// half was running and recovery goes whichever way that says.

// Interrupted while parking the live tree: some entries are already in the
// backup, none of the new version has landed. Recovery must put them back.
#[test]
fn in_place_rolls_back_when_parking_was_interrupted() {
    with_temp_home(|home| {
        let app = home.join(".wryayer/foo");

        // Half the live tree parked, half still in place.
        write(&app.join("config.ini"), "user-config");
        write(&app.join("home/user/profile"), "browser-profile");
        write(&app.join(".wr-old/usr/bin/main"), "v1-binary");
        write(&app.join(".wr-old/.manifest.toml"), "version = 1");
        // The staged new tree is complete but must be thrown away.
        write(&app.join(".wr-new/usr/bin/main"), "v2-binary");
        write(&app.join(".wr-phase"), "parking");

        recover_interrupted_update("foo").unwrap();

        assert_eq!(std::fs::read_to_string(app.join("usr/bin/main")).unwrap(), "v1-binary");
        assert_eq!(std::fs::read_to_string(app.join(".manifest.toml")).unwrap(), "version = 1");
        assert_eq!(std::fs::read_to_string(app.join("config.ini")).unwrap(), "user-config");
        assert_eq!(
            std::fs::read_to_string(app.join("home/user/profile")).unwrap(),
            "browser-profile"
        );
        assert!(!app.join(".wr-old").exists());
        assert!(!app.join(".wr-new").exists());
        assert!(!app.join(".wr-phase").exists());
    });
}

// Interrupted while moving the new tree in: the old tree is fully parked, so
// recovery finishes forward and hands the user's data over.
#[test]
fn in_place_finishes_forward_when_the_new_tree_was_going_in() {
    with_temp_home(|home| {
        let app = home.join(".wryayer/foo");

        // Part of the new tree is in place, the rest still staged.
        write(&app.join("usr/bin/main"), "v2-binary");
        write(&app.join(".wr-new/.manifest.toml"), "version = 2");
        // Everything the app had, parked.
        write(&app.join(".wr-old/usr/bin/main"), "v1-binary");
        write(&app.join(".wr-old/.manifest.toml"), "version = 1");
        write(&app.join(".wr-old/config.ini"), "user-config");
        write(&app.join(".wr-old/home/user/profile"), "browser-profile");
        write(&app.join(".wr-old/.snapshots/snap1"), "rollback-point");
        write(&app.join(".wr-phase"), "installing");

        recover_interrupted_update("foo").unwrap();

        assert_eq!(std::fs::read_to_string(app.join("usr/bin/main")).unwrap(), "v2-binary");
        assert_eq!(std::fs::read_to_string(app.join(".manifest.toml")).unwrap(), "version = 2");
        assert_eq!(std::fs::read_to_string(app.join("config.ini")).unwrap(), "user-config");
        assert_eq!(
            std::fs::read_to_string(app.join("home/user/profile")).unwrap(),
            "browser-profile"
        );
        assert_eq!(std::fs::read_to_string(app.join(".snapshots/snap1")).unwrap(), "rollback-point");
        assert!(!app.join(".wr-old").exists());
        assert!(!app.join(".wr-new").exists());
        assert!(!app.join(".wr-phase").exists());
    });
}

// A marker that never got written (or was lost) must not be guessed at
// optimistically: with a backup present, recovery restores what was parked.
#[test]
fn in_place_rolls_back_without_a_readable_phase() {
    with_temp_home(|home| {
        let app = home.join(".wryayer/foo");
        write(&app.join(".wr-old/usr/bin/main"), "v1-binary");
        write(&app.join(".wr-new/usr/bin/main"), "v2-binary");

        recover_interrupted_update("foo").unwrap();

        assert_eq!(std::fs::read_to_string(app.join("usr/bin/main")).unwrap(), "v1-binary");
        assert!(!app.join(".wr-new").exists());
    });
}

// Interrupted before the live tree was touched at all: the staged tree is a
// half-built throwaway and the app is untouched.
#[test]
fn in_place_discards_staging_and_keeps_live_tree() {
    with_temp_home(|home| {
        let app = home.join(".wryayer/foo");
        make_old_tree(&app);
        write(&app.join(".wr-new/usr/bin/main"), "half-extracted");

        recover_interrupted_update("foo").unwrap();

        assert!(!app.join(".wr-new").exists());
        assert_eq!(std::fs::read_to_string(app.join("usr/bin/main")).unwrap(), "v1-binary");
        assert_eq!(std::fs::read_to_string(app.join("config.ini")).unwrap(), "user-config");
    });
}
