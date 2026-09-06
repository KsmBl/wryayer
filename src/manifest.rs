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
    #[cfg(test)]
    assert_sandboxed(&root);
    Ok(root)
}

/// Under test, refuse to resolve anything but a throwaway root.
///
/// `HOME` is process-global and the tests move it around; a gap in that
/// choreography once meant a test wrote its fixture over a real
/// `~/.wryayer/.passwords.vault`, and the only symptom was the user's master
/// password no longer working days later. Failing the test on the spot is the
/// only outcome that cannot be missed — see [`crate::test_support`].
#[cfg(test)]
fn assert_sandboxed(root: &Path) {
    let tmp = std::env::temp_dir();
    assert!(
        root.starts_with(&tmp),
        "test resolved the real wryayer root at {} — HOME is not sandboxed.\n\
         Wrap the test in crate::test_support::with_temp_home.",
        root.display(),
    );
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
mod sandbox_tests {
    /// The guard that would have caught the bug that overwrote a real store.
    ///
    /// A test that forgets its sandbox — or loses it to another thread's HOME
    /// juggling — must fail here, not write into the developer's home.
    #[test]
    fn resolving_a_root_outside_the_sandbox_fails_the_test() {
        let _guard = crate::test_support::env_lock();
        let saved = std::env::var_os("HOME");
        std::env::set_var("HOME", "/home/definitely-not-a-temp-dir");

        let outcome = std::panic::catch_unwind(|| super::wryayer_root().map(|_| ()));

        match saved {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let panic = outcome.expect_err("an unsandboxed root must panic, not be handed out");
        let msg = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .unwrap_or("");
        assert!(msg.contains("HOME is not sandboxed"), "unhelpful panic: {msg}");
    }
}
