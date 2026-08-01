//! Shared scaffolding for the crate's own unit tests.
//!
//! ## Why there is exactly one lock
//!
//! `HOME` is process-global, `cargo test` runs every module's tests as threads
//! in a single binary, and most of this crate resolves its paths from `HOME`.
//! Each module used to serialise its own `HOME` juggling on its own mutex,
//! which is not enough: two independent locks let one module restore the real
//! `HOME` in the window between another module's `set_var` and the write that
//! followed it.
//!
//! That is not hypothetical. It overwrote a real `~/.wryayer/.passwords.vault`
//! with a test fixture, and the only symptom was the user's master password no
//! longer working. So every test that touches the environment now takes the one
//! lock, held for as long as the sandbox is in scope, and
//! [`crate::manifest::wryayer_root`] refuses outright to hand back a path
//! outside the temp directory while under test — a leak fails the test loudly
//! instead of quietly eating somebody's data.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// The one lock guarding process-global environment variables in tests.
///
/// A panicking test poisons the mutex; every caller re-establishes the state it
/// needs, so recovering the guard is correct rather than merely convenient.
///
/// Not reentrant. A helper called *by* a test must never take it — the test
/// owns the sandbox for its whole body.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A throwaway `HOME` that lasts until it is dropped.
///
/// Scope-shaped rather than closure-shaped because a test's *whole* body needs
/// the sandbox, not just the line that creates the fixture: a TUI test builds
/// an `App` and then feeds it keystrokes, and every one of those resolves paths
/// from `HOME` again.
pub struct TestHome {
    dir: tempfile::TempDir,
    saved: SavedEnv,
    // Dropped last, after the environment has been put back.
    _lock: MutexGuard<'static, ()>,
}

impl TestHome {
    /// The sandbox's `~/.wryayer`.
    pub fn root(&self) -> PathBuf {
        self.dir.path().join(".wryayer")
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        self.saved.restore();
    }
}

/// Point `HOME` (and the XDG directories derived from it) at a fresh scratch
/// directory containing an empty `.wryayer`, until the returned guard is
/// dropped.
pub fn test_home() -> TestHome {
    let lock = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let saved = SavedEnv::capture(&[
        "HOME",
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "WRYAYER_LAUNCHER_DIR",
        "WRYAYER_DESKTOP_DIR",
    ]);

    std::env::set_var("HOME", dir.path());
    std::env::set_var("XDG_RUNTIME_DIR", dir.path().join("run"));
    // Keeps the root-is-mounted marker out of the developer's real state dir.
    std::env::set_var("XDG_STATE_HOME", dir.path().join("state"));
    // Shortcuts and desktop entries are system-wide in normal use. Under test
    // they must never be: writing them would need root, and asking for root is
    // exactly the hang — or the damage — a test must not cause.
    std::env::set_var("WRYAYER_LAUNCHER_DIR", dir.path().join("bin"));
    std::env::set_var("WRYAYER_DESKTOP_DIR", dir.path().join("share/applications"));
    std::fs::create_dir_all(dir.path().join(".wryayer")).unwrap();

    TestHome { dir, saved, _lock: lock }
}

/// Closure form of [`test_home`], for tests that only need the sandbox around
/// one expression. Hands `f` the sandbox's `~/.wryayer`.
pub fn with_temp_home<T>(f: impl FnOnce(&Path) -> T) -> T {
    let home = test_home();
    f(&home.root())
}

/// The previous values of some environment variables, so they can be put back
/// exactly — including having been unset.
struct SavedEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl SavedEnv {
    fn capture(keys: &[&'static str]) -> Self {
        Self(keys.iter().map(|k| (*k, std::env::var_os(k))).collect())
    }

    fn restore(&self) {
        for (key, value) in &self.0 {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
