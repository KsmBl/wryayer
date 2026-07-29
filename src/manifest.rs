use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Manifest {
    pub app: AppMeta,
    pub packages: Vec<PackageEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AppMeta {
    pub name: String,
    pub main_binary: String,
    pub installed_at: String,
    pub launchers: Vec<String>,
    /// Set when this app is a thin alias whose binaries actually live inside
    /// another app's tree (created by `install --into <target>`). The alias
    /// dir holds just this manifest and its own config — no extracted files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_of: Option<String>,
    /// Custom display name shown in the TUI instead of the technical app name.
    /// Shown as "displayname [appname]" in the installed list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The original package name when it differs from the app folder name
    /// (i.e. when installed with --app-name). Used for version lookup and
    /// bracket display ("appname [pkgname]").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkg_name: Option<String>,
    /// Set when this app is a Windows game imported into a wine container.
    /// The `alias_of` field still points at the wine container (which owns
    /// the wine binary and shared library tree); `wine_game` adds the
    /// game-specific bits the launcher needs (.exe to launch, WINEPREFIX dir).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wine_game: Option<WineGame>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WineGame {
    /// Path inside the wine container's filesystem tree to the .exe to launch
    /// (e.g. "/games/nfsu2/Speed2.exe"). Resolved by wine at runtime.
    pub exe: String,
    /// Path inside the wine container's filesystem tree where the per-game
    /// WINEPREFIX lives (e.g. "/games/nfsu2/.wineprefix"). Created on first
    /// launch by wine itself.
    pub prefix: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PackageEntry {
    pub name: String,
    pub version: String,
    pub source: PackageSource,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PackageSource {
    Official,
    Aur,
}

pub fn wryayer_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    let root = PathBuf::from(&home).join(".wryayer");
    if let Some(problem) = root_problem(&home, &root) {
        anyhow::bail!("{problem}");
    }
    Ok(root)
}

// ── Is the root actually the root? ────────────────────────────────────────────
//
// `~/.wryayer` can be a mount point — an encrypted container holding every app,
// which is how wryayer is meant to be used when the whole install should be
// protected at rest. An unmounted mount point is an ordinary empty directory,
// indistinguishable from a fresh install, and everything below it still works:
// installs land underneath the mount point, a second master password store gets
// created there, and all of it disappears the moment the container is mounted.
// Worse, the shadow copy comes back on the next boot, so the master password
// the user knows is rejected by a store they never made.
//
// So: remember that the root was once a filesystem of its own, and refuse to
// touch it when it isn't one any more.

/// Name of the marker recording "this root lives on its own filesystem".
///
/// Kept in the XDG state directory rather than in `~/.wryayer`, because its
/// entire job is to be readable when `~/.wryayer` is not the right directory.
/// It records one bit and no app names, so it reveals nothing that
/// `~/.wryayer` existing doesn't already reveal.
fn root_fs_marker(home: &str) -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".local/state"));
    state.join("wryayer").join("root-is-mounted")
}

/// Whether `path` is the root of its own filesystem, i.e. something is mounted
/// there. `None` when it can't be determined.
fn is_own_filesystem(path: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let here = std::fs::metadata(path).ok()?.dev();
    let parent = std::fs::metadata(path.parent()?).ok()?.dev();
    Some(here != parent)
}

/// The verdict for a root, given what it looks like now and what it looked like
/// before. Split out from the filesystem so the rule itself can be tested.
///
/// The marker is one-way on purpose: once a root has been seen mounted, an
/// unmounted one is always an error rather than a new normal. Silently adopting
/// "well, it's a plain directory today" is exactly the behaviour that loses
/// data, and a container that fails to mount looks identical to one the user
/// has stopped using.
fn root_is_missing_its_filesystem(separate_now: Option<bool>, seen_mounted: bool) -> bool {
    matches!(separate_now, Some(false)) && seen_mounted
}

/// Check the root, and return the error text if it must not be used.
///
/// Cached for the process: this sits under every path lookup, and neither the
/// mount table nor the marker changes meaningfully mid-run.
fn root_problem(home: &str, root: &Path) -> Option<String> {
    use std::sync::OnceLock;
    static VERDICT: OnceLock<Option<String>> = OnceLock::new();
    VERDICT
        .get_or_init(|| {
            let marker = root_fs_marker(home);

            // The escape hatch, for someone who really has stopped using a
            // container. Clears the marker so it is a one-time thing.
            if std::env::var_os("WRYAYER_ALLOW_UNMOUNTED_ROOT").is_some() {
                let _ = std::fs::remove_file(&marker);
                return None;
            }
            // Nothing set up yet — there is no state to shadow.
            if !root.exists() {
                return None;
            }

            let separate_now = is_own_filesystem(root);
            if root_is_missing_its_filesystem(separate_now, marker.exists()) {
                return Some(unmounted_root_error(root));
            }
            if separate_now == Some(true) {
                remember_root_is_mounted(&marker);
            }
            None
        })
        .clone()
}

/// Record that the root was seen on its own filesystem. Best-effort: failing to
/// write the marker must never stop wryayer from working.
fn remember_root_is_mounted(marker: &Path) {
    if marker.exists() {
        return;
    }
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        marker,
        "# wryayer saw ~/.wryayer on its own filesystem (a mounted container).\n\
         # While this file exists, wryayer refuses to run with it unmounted,\n\
         # so nothing is written underneath the mount point.\n\
         # Delete it, or set WRYAYER_ALLOW_UNMOUNTED_ROOT=1 once, to stop that.\n",
    );
}

fn unmounted_root_error(root: &Path) -> String {
    format!(
        "{} is not mounted.\n\n\
         wryayer has seen this directory on its own filesystem before — you keep your \
         apps in an encrypted container. Right now it is an ordinary directory on the \
         same filesystem as your home, which means the container has not been mounted \
         yet.\n\n\
         Continuing would write underneath the mount point: installs, and a second \
         master password store with a different password, all of which vanish the \
         moment the container is mounted and come back the next time it is not. \
         Mount it first, for example:\n    \
         veracrypt --mount <container> {}\n\n\
         If you have deliberately stopped using a container, run once with:\n    \
         WRYAYER_ALLOW_UNMOUNTED_ROOT=1 wryayer <command>",
        root.display(),
        root.display(),
    )
}

pub fn app_dir(app_name: &str) -> Result<PathBuf> {
    Ok(wryayer_root()?.join(app_name))
}

pub fn manifest_path(app_name: &str) -> Result<PathBuf> {
    Ok(app_dir(app_name)?.join(".manifest.toml"))
}

pub fn read_manifest(app_name: &str) -> Result<Manifest> {
    let path = manifest_path(app_name)?;
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read manifest at {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("failed to parse manifest for {app_name}"))
}

/// Read an app's manifest, falling back to the locked-state marker when the app
/// is encrypted and currently locked (its real manifest is inside the
/// unmounted container).
///
/// The fallback manifest has an empty package list, so it is only suitable for
/// operations that need the app's identity and launchers — listing and removal —
/// never for anything that inspects or rewrites the installed packages.
pub fn read_manifest_or_marker(app_name: &str) -> Result<Manifest> {
    match read_manifest(app_name) {
        Ok(m) => Ok(m),
        Err(e) => match crate::veracrypt::read_marker(app_name) {
            Some(marker) => Ok(marker.to_manifest()),
            None => Err(e),
        },
    }
}

pub fn write_manifest(app_name: &str, manifest: &Manifest) -> Result<()> {
    write_manifest_to(&app_dir(app_name)?, manifest)
}

/// Write the manifest into an arbitrary app-tree directory (used to stamp a
/// staging tree before it is atomically swapped into place). Writes to a temp
/// file and renames, so a reader never sees a half-written manifest.
pub fn write_manifest_to(dir: &Path, manifest: &Manifest) -> Result<()> {
    let path = dir.join(".manifest.toml");
    let tmp_path = path.with_extension("toml.tmp");
    let content =
        toml::to_string_pretty(manifest).context("failed to serialize manifest to TOML")?;
    fs::write(&tmp_path, &content)
        .with_context(|| format!("failed to write manifest tmp file at {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "failed to rename manifest tmp to {}",
            path.display()
        )
    })?;
    Ok(())
}

pub fn list_all_apps() -> Result<Vec<Manifest>> {
    let root = wryayer_root()?;
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut manifests = vec![];
    for entry in fs::read_dir(&root)
        .with_context(|| format!("failed to read wryayer root {}", root.display()))?
    {
        let entry = entry.context("failed to read directory entry")?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let app_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // Dot-prefixed dirs are never apps: they're reserved scratch trees such
        // as an update's staging/backup (see commands::update). A valid package
        // name never starts with '.', so skipping them can't hide a real app,
        // and it keeps an in-flight update invisible to listings and the TUI.
        if app_name.starts_with('.') {
            continue;
        }
        // An encrypted app that is currently locked has its container
        // unmounted, so the directory shows only the .encrypted.toml marker and
        // no manifest. Rebuild a listing stub from the marker so the app stays
        // visible (and removable) instead of silently disappearing while locked.
        let has_manifest = manifest_path(&app_name).map(|p| p.exists()).unwrap_or(false);
        if !has_manifest {
            if let Some(marker) = crate::veracrypt::read_marker(&app_name) {
                manifests.push(marker.to_manifest());
                continue;
            }
            // Otherwise: a partial install in progress (install.rs creates the
            // app dir before it writes the manifest) or a leftover. Skip it
            // silently — warning here spams the install log for the very app
            // being installed. Only a manifest that exists but won't parse is a
            // real problem worth flagging.
            continue;
        }
        match read_manifest(&app_name) {
            Ok(m) => manifests.push(m),
            Err(e) => eprintln!("warning: skipping '{}': {e:#}", app_name),
        }
    }
    manifests.sort_by(|a, b| a.app.name.cmp(&b.app.name));
    Ok(manifests)
}

/// Re-order a flat app list into tree order: each root app is immediately
/// followed by its aliases (sorted by name), so callers can iterate once
/// and detect tree structure via `alias_of`.  Orphan aliases (whose target
/// is absent) are appended at the end.
pub fn tree_order(apps: Vec<Manifest>) -> Vec<Manifest> {
    let mut by_target: std::collections::HashMap<String, Vec<Manifest>> =
        std::collections::HashMap::new();
    let mut roots: Vec<Manifest> = Vec::new();

    for app in apps {
        if let Some(ref target) = app.app.alias_of {
            by_target.entry(target.clone()).or_default().push(app);
        } else {
            roots.push(app);
        }
    }
    for children in by_target.values_mut() {
        children.sort_by(|a, b| a.app.name.cmp(&b.app.name));
    }

    let mut result = Vec::new();
    for root in roots {
        let children = by_target.remove(&root.app.name).unwrap_or_default();
        result.push(root);
        result.extend(children);
    }
    for (_, orphans) in by_target {
        result.extend(orphans);
    }
    result
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod root_fs_tests {
    use super::*;

    #[test]
    fn a_root_that_lost_its_filesystem_is_refused() {
        // The whole bug: the container isn't mounted, so ~/.wryayer is a plain
        // directory again and everything written there is a shadow copy.
        assert!(root_is_missing_its_filesystem(Some(false), true));
    }

    #[test]
    fn a_mounted_root_is_fine() {
        assert!(!root_is_missing_its_filesystem(Some(true), true));
    }

    #[test]
    fn a_plain_directory_setup_is_left_alone() {
        // Never seen mounted: this user simply doesn't encrypt their root, and
        // must not start getting errors.
        assert!(!root_is_missing_its_filesystem(Some(false), false));
    }

    #[test]
    fn an_unreadable_root_is_not_treated_as_unmounted() {
        // Refusing to run because a stat failed would turn an unrelated
        // permissions problem into "your container is missing".
        assert!(!root_is_missing_its_filesystem(None, true));
    }

    #[test]
    fn the_marker_lives_outside_the_root_it_describes() {
        // Kept inside ~/.wryayer it would be sealed in the very container whose
        // absence it exists to detect.
        let marker = root_fs_marker("/home/someone");
        assert!(
            !marker.starts_with("/home/someone/.wryayer"),
            "marker must not live in the root: {}",
            marker.display()
        );
        assert!(marker.starts_with("/home/someone/.local/state"), "{}", marker.display());
    }

    #[test]
    fn the_marker_honours_xdg_state_home() {
        let old = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", "/somewhere/state");
        let marker = root_fs_marker("/home/someone");
        match old {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        assert!(marker.starts_with("/somewhere/state"), "{}", marker.display());
    }

    #[test]
    fn writing_the_marker_is_idempotent_and_self_describing() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("state/wryayer/root-is-mounted");
        remember_root_is_mounted(&marker);
        let first = std::fs::read_to_string(&marker).unwrap();
        remember_root_is_mounted(&marker);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), first);
        // Someone finding this file must be able to tell what it does.
        assert!(first.contains("WRYAYER_ALLOW_UNMOUNTED_ROOT"), "{first}");
    }

    #[test]
    fn a_separate_mount_is_recognised_as_its_own_filesystem() {
        // /proc is always a mount of its own; / never is a mount within itself.
        assert_eq!(is_own_filesystem(Path::new("/proc")), Some(true));
        assert_eq!(is_own_filesystem(Path::new("/etc")), Some(false));
    }

    #[test]
    fn the_error_says_what_to_do_about_it() {
        let msg = unmounted_root_error(Path::new("/home/u/.wryayer"));
        assert!(msg.contains("is not mounted"), "{msg}");
        assert!(msg.contains("veracrypt --mount"), "{msg}");
        assert!(msg.contains("WRYAYER_ALLOW_UNMOUNTED_ROOT=1"), "{msg}");
    }
}
