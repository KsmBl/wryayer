// Tests for the alias-app feature: when a package is installed with
// `wryayer install <pkg> --into <target>`, a thin manifest is placed at
// ~/.wryayer/<pkg>/ that carries `alias_of = Some("<target>")`. The actual
// files live in the target's tree. These tests exercise the manifest serde,
// list_all_apps integration, and the remove flow without invoking pacman or
// bwrap.

use std::sync::Mutex;
use wryayer::commands::remove::{self, run_cascade};
use wryayer::launcher::create_launcher;
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

fn write_app(root: &std::path::Path, name: &str, alias_of: Option<&str>) {
    let dir = root.join(format!(".wryayer/{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    // Targets get a usr/bin/<main> file too so the tree looks realistic;
    // aliases stay empty apart from the manifest file.
    if alias_of.is_none() {
        std::fs::create_dir_all(dir.join("usr/bin")).unwrap();
        std::fs::write(dir.join("usr/bin").join(name), b"#!/bin/sh\n").unwrap();
    }
    let manifest = Manifest {
        app: AppMeta {
            name:         name.to_string(),
            main_binary:  name.to_string(),
            installed_at: "2026-05-17T00:00:00Z".to_string(),
            launchers:    vec![name.to_string()],
            alias_of:     alias_of.map(str::to_string),
            display_name: None,
            pkg_name:     None,
        },
        packages: vec![PackageEntry {
            name:    name.to_string(),
            version: "1.0-1".to_string(),
            source:  PackageSource::Official,
        }],
    };
    write_manifest(name, &manifest).unwrap();
}

// ── Serde behavior ────────────────────────────────────────────────────────────

#[test]
fn alias_of_round_trips_through_toml() {
    with_temp_home(|root| {
        std::fs::create_dir_all(root.join(".wryayer/hyfetch")).unwrap();
        write_app(root, "hyfetch", Some("fastfetch"));
        let loaded = read_manifest("hyfetch").unwrap();
        assert_eq!(loaded.app.alias_of.as_deref(), Some("fastfetch"));
    });
}

#[test]
fn alias_of_none_is_omitted_from_serialized_output() {
    // Cosmetic but important: standalone apps shouldn't get a noisy
    // `alias_of = ""` line. serde's skip_serializing_if handles this.
    with_temp_home(|root| {
        write_app(root, "firefox", None);
        let raw = std::fs::read_to_string(manifest_path("firefox").unwrap()).unwrap();
        assert!(
            !raw.contains("alias_of"),
            "alias_of=None should not appear in TOML; got:\n{raw}"
        );
    });
}

#[test]
fn alias_of_some_appears_in_serialized_output() {
    with_temp_home(|root| {
        write_app(root, "hyfetch", Some("fastfetch"));
        let raw = std::fs::read_to_string(manifest_path("hyfetch").unwrap()).unwrap();
        assert!(
            raw.contains("alias_of"),
            "alias_of=Some must appear in TOML; got:\n{raw}"
        );
        assert!(raw.contains("fastfetch"));
    });
}

#[test]
fn old_manifest_without_alias_of_still_parses() {
    // Backwards-compat: manifests written by older wryayer versions don't
    // mention alias_of at all. The serde default must keep them readable.
    with_temp_home(|root| {
        let dir = root.join(".wryayer/legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = r#"[app]
name = "legacy"
main_binary = "legacy"
installed_at = "2026-01-01T00:00:00Z"
launchers = ["legacy"]

[[packages]]
name = "legacy"
version = "1.0-1"
source = "official"
"#;
        std::fs::write(dir.join(".manifest.toml"), legacy).unwrap();
        let m = read_manifest("legacy").unwrap();
        assert_eq!(m.app.alias_of, None);
        assert_eq!(m.app.name, "legacy");
    });
}

// ── list_all_apps integration ─────────────────────────────────────────────────

#[test]
fn list_all_apps_includes_aliases_as_separate_entries() {
    with_temp_home(|root| {
        write_app(root, "fastfetch", None);
        write_app(root, "hyfetch", Some("fastfetch"));
        let apps = list_all_apps().unwrap();
        let names: Vec<&str> = apps.iter().map(|m| m.app.name.as_str()).collect();
        assert_eq!(names, ["fastfetch", "hyfetch"]); // sorted alphabetically

        let alias = apps.iter().find(|m| m.app.name == "hyfetch").unwrap();
        assert_eq!(alias.app.alias_of.as_deref(), Some("fastfetch"));
    });
}

// ── remove on alias ───────────────────────────────────────────────────────────

#[test]
fn remove_alias_deletes_alias_dir_only() {
    with_temp_home(|root| {
        write_app(root, "fastfetch", None);
        write_app(root, "hyfetch", Some("fastfetch"));
        // Pretend ~/bin/hyfetch was created by install; remove::run would
        // call remove_launcher on it. Create it via the real helper so the
        // safety header is present. Must use the alias name ("hyfetch"), not
        // the target ("fastfetch"), matching the fixed install behaviour.
        create_launcher("hyfetch", "hyfetch").unwrap();

        remove::run("hyfetch").unwrap();

        assert!(
            !root.join(".wryayer/hyfetch").exists(),
            "alias dir must be gone"
        );
        assert!(
            root.join(".wryayer/fastfetch/usr/bin/fastfetch").exists(),
            "target tree must be untouched"
        );
        assert!(
            !root.join("bin/hyfetch").exists(),
            "alias's launcher must be removed"
        );
    });
}

#[test]
fn remove_alias_does_not_touch_target_manifest() {
    with_temp_home(|root| {
        write_app(root, "fastfetch", None);
        write_app(root, "hyfetch", Some("fastfetch"));

        let before = std::fs::read_to_string(manifest_path("fastfetch").unwrap()).unwrap();
        remove::run("hyfetch").unwrap();
        let after = std::fs::read_to_string(manifest_path("fastfetch").unwrap()).unwrap();
        assert_eq!(before, after, "target's manifest must be byte-identical");
    });
}

// ── remove blocks when aliases depend on a target ─────────────────────────────

#[test]
fn remove_target_with_aliases_is_blocked() {
    with_temp_home(|root| {
        write_app(root, "fastfetch", None);
        write_app(root, "hyfetch", Some("fastfetch"));

        let err = remove::run("fastfetch").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("hyfetch"),
            "error must name the blocking alias; got: {msg}"
        );
        assert!(
            root.join(".wryayer/fastfetch").exists(),
            "target must NOT be deleted when removal is blocked"
        );
    });
}

#[test]
fn remove_target_with_multiple_aliases_lists_all_in_error() {
    with_temp_home(|root| {
        write_app(root, "fastfetch", None);
        write_app(root, "hyfetch", Some("fastfetch"));
        write_app(root, "neofetch-alt", Some("fastfetch"));

        let err = remove::run("fastfetch").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("hyfetch"));
        assert!(msg.contains("neofetch-alt"));
    });
}

#[test]
fn remove_target_succeeds_once_aliases_are_gone() {
    with_temp_home(|root| {
        write_app(root, "fastfetch", None);
        write_app(root, "hyfetch", Some("fastfetch"));
        // Pre-create launchers so remove::run doesn't choke.
        // Alias launcher uses alias name; target launcher uses target name.
        create_launcher("hyfetch", "hyfetch").unwrap();
        create_launcher("fastfetch", "fastfetch").unwrap();

        remove::run("hyfetch").unwrap();   // removes alias launcher + alias dir
        remove::run("fastfetch").unwrap(); // removes target launcher + target dir

        assert!(!root.join(".wryayer/fastfetch").exists());
        assert!(!root.join(".wryayer/hyfetch").exists());
    });
}

// ── run_cascade ───────────────────────────────────────────────────────────────

#[test]
fn run_cascade_removes_target_and_all_aliases() {
    with_temp_home(|root| {
        write_app(root, "fastfetch", None);
        write_app(root, "hyfetch", Some("fastfetch"));
        write_app(root, "neofetch-alt", Some("fastfetch"));
        create_launcher("fastfetch", "fastfetch").unwrap();
        create_launcher("hyfetch", "hyfetch").unwrap();
        create_launcher("neofetch-alt", "neofetch-alt").unwrap();

        run_cascade("fastfetch").unwrap();

        assert!(!root.join(".wryayer/fastfetch").exists(), "target dir must be removed");
        assert!(!root.join(".wryayer/hyfetch").exists(), "alias hyfetch dir must be removed");
        assert!(!root.join(".wryayer/neofetch-alt").exists(), "alias neofetch-alt dir must be removed");
        assert!(!root.join("bin/fastfetch").exists(), "target launcher must be removed");
        assert!(!root.join("bin/hyfetch").exists(), "alias launcher must be removed");
        assert!(!root.join("bin/neofetch-alt").exists(), "alias launcher must be removed");
    });
}

#[test]
fn run_cascade_on_alias_acts_like_plain_remove() {
    with_temp_home(|root| {
        write_app(root, "fastfetch", None);
        write_app(root, "hyfetch", Some("fastfetch"));
        create_launcher("hyfetch", "hyfetch").unwrap();

        run_cascade("hyfetch").unwrap();

        assert!(!root.join(".wryayer/hyfetch").exists(), "alias dir must be removed");
        assert!(root.join(".wryayer/fastfetch").exists(), "target dir must remain");
    });
}

// ── launcher content for aliases ──────────────────────────────────────────────

#[test]
fn alias_launcher_invokes_alias_not_target() {
    // Regression: install --into used to create the launcher with the target
    // app name, so ~/bin/hyfetch would exec `wryayer run cpufetch` and always
    // run cpufetch's main_binary regardless of which binary was requested.
    // The fix passes alias_name to create_launcher; verify the content here.
    with_temp_home(|root| {
        create_launcher("hyfetch", "hyfetch").unwrap();
        let content = std::fs::read_to_string(root.join("bin/hyfetch")).unwrap();
        assert!(
            content.contains(r#"run "hyfetch""#),
            "alias launcher must invoke `wryayer run hyfetch`; got:\n{content}",
        );
        assert!(
            !content.contains(r#"run "cpufetch""#),
            "alias launcher must not reference the target app name; got:\n{content}",
        );
    });
}

// ── remove of plain standalone app unaffected ─────────────────────────────────

#[test]
fn remove_standalone_app_with_no_aliases_works_normally() {
    with_temp_home(|root| {
        write_app(root, "firefox", None);
        create_launcher("firefox", "firefox").unwrap();

        remove::run("firefox").unwrap();
        assert!(!root.join(".wryayer/firefox").exists());
        assert!(!root.join("bin/firefox").exists());
    });
}
