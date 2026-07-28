use crate::manifest::{
    app_dir, list_all_apps, now_rfc3339, read_manifest, write_manifest_to, AppMeta, Manifest,
    PackageEntry, PackageSource,
};
use crate::commands::install::{ensure_base_layout, ensure_owner_readable, regenerate_runtime_caches, run_ldconfig};
use crate::package::{
    build_aur, download_official, extract_package, resolve_full_dep_tree,
    satisfy_missing_sonames,
};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub fn run(app_name: Option<&str>, check_only: bool, full: bool) -> Result<()> {
    let manifests = match app_name {
        Some(name) => {
            // Naming an app explicitly is a request to update that app, so a
            // locked container is an error rather than something to skip.
            crate::commands::encrypt::require_unlocked(name, "update")?;
            vec![read_manifest(name).with_context(|| format!("'{name}' is not installed"))?]
        }
        None => list_all_apps()?,
    };

    if manifests.is_empty() {
        eprintln!("No apps installed.");
        return Ok(());
    }

    let mut has_updates = false;

    for manifest in &manifests {
        // Alias manifests don't own a filesystem tree — skip them; the target
        // app is updated separately when its own manifest is processed.
        if manifest.app.alias_of.is_some() {
            continue;
        }

        let name = &manifest.app.name;
        // A locked app's tree is inaccessible and its manifest is a stub with no
        // packages, so an update pass would compare against nothing. Skip it —
        // updating everything else shouldn't fail because one app is locked.
        if crate::veracrypt::is_locked(name) {
            eprintln!("  {name}: locked — skipping (unlock it with 'wryayer unlock {name}')");
            continue;
        }
        // When installed with --app-name, `name` is the user's custom label and
        // the real upstream package is in `pkg_name`. Version checks must query
        // the real package, not the custom name.
        let pkg = manifest.app.pkg_name.as_deref().unwrap_or(name);
        let main_pkg = manifest.packages.iter().find(|p| p.name == pkg);
        let current_version = main_pkg.map(|p| p.version.as_str()).unwrap_or("0");

        let latest_version = match main_pkg.map(|p| &p.source).unwrap_or(&PackageSource::Official) {
            PackageSource::Official => get_official_version(pkg)?,
            PackageSource::Aur => get_aur_version(pkg)?,
        };

        match latest_version {
            None => eprintln!("{name}: package not found, skipping"),
            Some(ref ver) if !is_newer(ver, current_version)? => {
                if check_only {
                    eprintln!("{name}: up to date ({current_version})");
                }
            }
            Some(ref ver) => {
                has_updates = true;
                if check_only {
                    eprintln!("{name}: update available  {current_version}  ->  {ver}");
                } else {
                    eprintln!("{name}: updating {current_version} -> {ver}");
                    reinstall(manifest, full)?;
                }
            }
        }
    }

    if check_only && !has_updates {
        eprintln!("All apps are up to date.");
    }

    Ok(())
}

fn reinstall(manifest: &crate::manifest::Manifest, full: bool) -> Result<()> {
    let app_name = &manifest.app.name;
    let bin_name = &manifest.app.main_binary;
    // The dependency tree is resolved from the real upstream package. When the
    // app was installed with --app-name, `app_name` is only the user's custom
    // label (and the app-dir name); the package to resolve is `pkg_name`.
    let pkg_name = manifest.app.pkg_name.as_deref().unwrap_or(app_name).to_string();

    // The dep-resolution cache never expires, so re-resolving would return the
    // versions recorded at first install and write them straight back into the
    // manifest — leaving the TUI showing the old version after a successful
    // update. Drop the cache for this app's whole tree so every package is
    // re-queried for its current version.
    let cached_names: Vec<String> = manifest.packages.iter().map(|p| p.name.clone()).collect();
    crate::package::deps::invalidate_dep_cache(&cached_names);
    crate::package::deps::invalidate_dep_cache(std::slice::from_ref(&pkg_name));

    eprintln!("Resolving dependencies for {app_name} ({pkg_name})...");
    let mut resolved = resolve_full_dep_tree(&pkg_name)?;

    // Merged-in child programs (installed with `--into <app>`) share this same
    // filesystem tree but are tracked by their own alias manifests; their
    // packages are NOT part of app_name's dependency tree.  Re-resolve each one
    // and fold it into the set, otherwise the wipe-and-extract below deletes
    // every child binary.  Any resolve failure bails *before* the wipe, so a
    // transient error can never leave the tree missing a child.
    let mut seen: std::collections::HashSet<String> =
        resolved.iter().map(|p| p.name.clone()).collect();
    let children: Vec<Manifest> = list_all_apps()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.app.alias_of.as_deref() == Some(app_name.as_str()))
        .collect();
    for child in &children {
        let root_pkg = child.app.pkg_name.as_deref().unwrap_or(child.app.name.as_str());
        let child_names: Vec<String> = child.packages.iter().map(|p| p.name.clone()).collect();
        crate::package::deps::invalidate_dep_cache(&child_names);
        crate::package::deps::invalidate_dep_cache(&[root_pkg.to_string()]);
        eprintln!("Resolving child program {} ({root_pkg})...", child.app.name);
        let child_tree = resolve_full_dep_tree(root_pkg)
            .with_context(|| format!("failed to resolve child program '{}'", child.app.name))?;
        for p in child_tree {
            if seen.insert(p.name.clone()) {
                resolved.push(p);
            }
        }
    }

    let app_dir = app_dir(app_name)?;

    // ── Delta planning ───────────────────────────────────────────────────────
    // Compare the freshly-resolved set against what's already installed (the
    // target's manifest plus every merged-in child's) so we re-download and
    // re-extract only the packages whose version actually changed, reusing the
    // rest straight from the live tree. A delta is only safe when no package
    // *disappeared*: an in-place overlay can't know which files a removed
    // package owned, so any removal — or `--full` — forces a clean full rebuild.
    let mut old_versions: std::collections::HashMap<String, String> = manifest
        .packages
        .iter()
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();
    for child in &children {
        for p in &child.packages {
            old_versions
                .entry(p.name.clone())
                .or_insert_with(|| p.version.clone());
        }
    }
    let resolved_nv: Vec<(String, String)> =
        resolved.iter().map(|p| (p.name.clone(), p.version.clone())).collect();
    let (delta, changed) = plan_delta(
        &old_versions,
        resolved_nv.iter().map(|(n, v)| (n.as_str(), v.as_str())),
        full,
        app_dir.exists(),
    );
    if delta {
        eprintln!(
            "Delta update: re-fetching {} of {} package(s); reusing the rest.",
            changed.len(),
            resolved.len()
        );
    }

    let home = std::env::var("HOME").context("HOME not set")?;
    let cache_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("pkg");
    let build_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("build");

    for pkg in &mut resolved {
        // Delta: unchanged packages are reused from the live tree, so neither
        // downloaded nor extracted.
        if delta && !changed.contains(&pkg.name) {
            continue;
        }
        match pkg.source {
            PackageSource::Official => {
                let path = download_official(&pkg.name, &cache_dir)
                    .with_context(|| format!("failed to download {}", pkg.name))?;
                pkg.pkg_path = Some(path);
            }
            PackageSource::Aur => {
                let path = build_aur(&pkg.name, &build_dir)
                    .with_context(|| format!("failed to build {}", pkg.name))?;
                pkg.pkg_path = Some(path);
            }
        }
    }

    // Any earlier update on this app that was killed mid-swap is finished or
    // rolled back here, before we build a new one, so we always start from a
    // consistent tree.
    recover_interrupted_update(app_name)?;

    // Crash-safe update: nothing destructive touches the live tree until the
    // new one is fully built.  Extract every package into a fresh staging tree
    // and stamp its manifest there; then swap it in with two atomic renames.
    // If the process is cancelled (Ctrl-C), killed, or the machine loses power
    // at ANY moment, recover_interrupted_update() on the next run either
    // completes the swap forward or restores the untouched old version — the
    // app can never be left half-wiped.
    let (staging, backup) = swap_paths(app_name)?;
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("failed to clear stale staging dir {}", staging.display()))?;
    }
    fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create staging dir {}", staging.display()))?;

    // Delta: seed the staging tree with a hard-linked clone of the current
    // package files (user data + manifest excluded — those are carried or
    // rewritten), then overlay only the changed packages below. `extract_package`
    // unlink-firsts, so a re-extracted file gets a fresh inode and the shared
    // clone/snapshot inodes are never mutated.
    if delta {
        eprintln!("Reusing unchanged files...");
        clone_package_tree(&app_dir, &staging)
            .with_context(|| "failed to seed the delta staging tree")?;
    }

    for pkg in &resolved {
        if delta && !changed.contains(&pkg.name) {
            continue;
        }
        let pkg_path = pkg.pkg_path.as_ref().unwrap();
        eprintln!("Extracting {}...", pkg.name);
        extract_package(pkg_path, &staging)
            .with_context(|| format!("failed to extract {}", pkg.name))?;
    }

    let packages: Vec<PackageEntry> = resolved
        .iter()
        .map(|p| PackageEntry {
            name: p.name.clone(),
            version: p.version.clone(),
            source: p.source.clone(),
        })
        .collect();

    let new_manifest = Manifest {
        app: AppMeta {
            name: app_name.clone(),
            main_binary: bin_name.clone(),
            installed_at: now_rfc3339(),
            launchers: manifest.app.launchers.clone(),
            alias_of: manifest.app.alias_of.clone(),
            display_name: manifest.app.display_name.clone(),
            pkg_name: manifest.app.pkg_name.clone(),
            wine_game: manifest.app.wine_game.clone(),
        },
        packages,
    };
    // Stamp the manifest into the staging tree so the dir swapped into place is
    // already complete.
    write_manifest_to(&staging, &new_manifest)?;

    // --- Point of no return: swap the staging tree in for the live one. ---
    // Each rename is atomic, and every interruption between them is understood
    // and healed by recover_interrupted_update():
    //   * after step 1, before step 2 -> old tree restored from backup
    //   * after step 2                 -> update finished forward from backup
    if app_dir.exists() {
        fs::rename(&app_dir, &backup) // 1. move the old tree aside
            .with_context(|| format!("failed to move old tree aside for {app_name}"))?;
    }
    fs::rename(&staging, &app_dir) // 2. move the new tree into place
        .with_context(|| format!("failed to swap in updated tree for {app_name}"))?;
    // The new tree ships only package files; carry the user's data (sandbox
    // home with browser profiles, per-app config, snapshots for rollback) over
    // from the old tree, then discard it.
    carry_over_user_data(&backup, &app_dir)?;
    let _ = fs::remove_dir_all(&backup);

    ensure_base_layout(&app_dir)
        .with_context(|| "failed to restore base filesystem symlinks")?;

    let fixed = ensure_owner_readable(&app_dir)
        .with_context(|| "failed to fix file permissions")?;
    if fixed > 0 {
        eprintln!("  Made {fixed} file(s) owner-readable.");
    }

    eprintln!("Checking for missing shared library dependencies...");
    match satisfy_missing_sonames(&app_dir, &cache_dir) {
        Ok(extra) if !extra.is_empty() => eprintln!("  Added: {}", extra.join(", ")),
        Ok(_) => {}
        Err(e) => eprintln!("  Warning: soname check failed: {e:#}"),
    }

    eprintln!("Building library cache...");
    run_ldconfig(&app_dir);
    regenerate_runtime_caches(&app_dir);

    eprintln!("Updated '{app_name}'.");
    Ok(())
}

/// User data that lives inside an app tree but is NOT package-provided: the
/// sandbox home (browser profiles, font caches, GUI settings), the per-app
/// wryayer config, and snapshots (so a post-update rollback still reaches a
/// pre-update version). These are carried across an update rather than
/// re-extracted. The manifest is intentionally absent — the update writes a
/// fresh one into the staging tree.
const CARRY_OVER: &[&str] = &["home", "config.ini", crate::commands::snapshot::SNAP_DIR];

/// Reserved sibling paths used to apply an update atomically: `.<app>.wr-new`
/// is the staging tree, `.<app>.wr-old` is the old tree parked during the swap.
/// Both sit next to the app dir (same filesystem, so renames are atomic) and
/// are dot-prefixed so `list_all_apps` never mistakes them for apps.
fn swap_paths(app_name: &str) -> Result<(PathBuf, PathBuf)> {
    let base = app_dir(app_name)?;
    let parent = base
        .parent()
        .with_context(|| format!("app dir {} has no parent", base.display()))?
        .to_path_buf();
    Ok((
        parent.join(format!(".{app_name}.wr-new")),
        parent.join(format!(".{app_name}.wr-old")),
    ))
}

/// Decide which packages a delta update must re-fetch. Returns `(delta,
/// changed)`: `delta` is true when an incremental update is safe — not forced
/// full, the live tree exists, something is already installed, and **no** package
/// present before is missing now (a removal needs a clean rebuild since we can't
/// tell which files it owned). `changed` is the set of package names whose
/// version differs from what's installed, or that are brand new — exactly the
/// ones to re-download and re-extract.
fn plan_delta<'a>(
    old_versions: &std::collections::HashMap<String, String>,
    new_pkgs: impl Iterator<Item = (&'a str, &'a str)> + Clone,
    full: bool,
    tree_exists: bool,
) -> (bool, std::collections::HashSet<String>) {
    let new_names: std::collections::HashSet<&str> = new_pkgs.clone().map(|(n, _)| n).collect();
    let removed_any = old_versions.keys().any(|n| !new_names.contains(n.as_str()));
    let changed: std::collections::HashSet<String> = new_pkgs
        .filter(|(n, v)| old_versions.get(*n).map(|ov| ov != v).unwrap_or(true))
        .map(|(n, _)| n.to_string())
        .collect();
    let delta = !full && tree_exists && !old_versions.is_empty() && !removed_any;
    (delta, changed)
}

/// Hard-link every package file from the live app tree `src` into the fresh
/// staging tree `dst`, so a delta update can overlay just the changed packages
/// on top. User data and the manifest are skipped: `home`, `config.ini` and
/// `.snapshots` are carried across the swap from the backup, and the manifest is
/// rewritten. Hard links are near-free and share inodes with the live tree (and
/// its snapshots); `extract_package` unlink-firsts, so overlaying a changed file
/// never mutates a shared inode.
fn clone_package_tree(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    const SKIP_TOP: &[&str] =
        &["home", "config.ini", crate::commands::snapshot::SNAP_DIR, ".manifest.toml"];

    fs::create_dir_all(dst)?;
    // (source dir, dest dir, is_top_level)
    let mut stack: Vec<(PathBuf, PathBuf, bool)> = vec![(src.to_path_buf(), dst.to_path_buf(), true)];
    while let Some((s, d, top)) = stack.pop() {
        for entry in fs::read_dir(&s).with_context(|| format!("failed to read {}", s.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let n: &str = &name_str;
            if top && SKIP_TOP.contains(&n) {
                continue;
            }
            let sp = entry.path();
            let dp = d.join(&name);
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                let target = fs::read_link(&sp)?;
                let _ = fs::remove_file(&dp);
                symlink(&target, &dp)
                    .with_context(|| format!("failed to symlink {}", dp.display()))?;
            } else if ft.is_dir() {
                fs::create_dir_all(&dp)?;
                if let Ok(m) = fs::metadata(&sp) {
                    let _ = fs::set_permissions(&dp, fs::Permissions::from_mode(m.mode()));
                }
                stack.push((sp, dp, false));
            } else if ft.is_file() {
                if dp.exists() {
                    let _ = fs::remove_file(&dp);
                }
                fs::hard_link(&sp, &dp)
                    .with_context(|| format!("failed to hard-link {}", dp.display()))?;
            }
        }
    }
    Ok(())
}

/// Move each user-data item from `from` into `to`. When `to` doesn't have the
/// item, it's a fast same-filesystem rename that moves atomically. When `to`
/// *does* already have it — e.g. the `filesystem` package ships an empty `home/`
/// skeleton that lands in the freshly-extracted tree — the old data is **merged
/// in, winning over** the new tree's entry, so a package-provided empty
/// directory can never shadow (and then get the real profile deleted with the
/// discarded backup). Package trees never own `home`/`config.ini`/`.snapshots`,
/// so any pre-existing `to` entry is only ever an empty skeleton. Safe to re-run
/// after an interruption: already-moved entries are simply gone from `from`.
fn carry_over_user_data(from: &Path, to: &Path) -> Result<()> {
    for item in CARRY_OVER {
        let src = from.join(item);
        let dst = to.join(item);
        if !src.exists() {
            continue;
        }
        if !dst.exists() {
            fs::rename(&src, &dst)
                .with_context(|| format!("failed to carry over '{item}' during update"))?;
            continue;
        }
        merge_preferring_src(&src, &dst)
            .with_context(|| format!("failed to carry over '{item}' during update"))?;
    }
    Ok(())
}

/// Recursively move every entry from `src` into `dst`, with `src` (the old user
/// data) winning on any collision: matching directories are merged, and a file,
/// symlink, or dir in `src` replaces whatever empty skeleton `dst` held. Leaves
/// `src` empty so the backup tree can be removed cleanly afterwards.
fn merge_preferring_src(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let s = src.join(&name);
        let d = dst.join(&name);
        let s_is_dir = fs::symlink_metadata(&s)
            .with_context(|| format!("failed to stat {}", s.display()))?
            .file_type()
            .is_dir();
        let d_meta = fs::symlink_metadata(&d).ok();
        // Two real directories: recurse so existing (empty) subdirs are kept and
        // the old entries fill them in.
        if s_is_dir && d_meta.as_ref().is_some_and(|m| m.file_type().is_dir()) {
            merge_preferring_src(&s, &d)?;
            continue;
        }
        // Otherwise the old entry wins outright — clear any skeleton in the way.
        if let Some(m) = d_meta {
            if m.file_type().is_dir() {
                fs::remove_dir_all(&d)
                    .with_context(|| format!("failed to clear {}", d.display()))?;
            } else {
                fs::remove_file(&d)
                    .with_context(|| format!("failed to clear {}", d.display()))?;
            }
        }
        fs::rename(&s, &d).with_context(|| format!("failed to move {}", s.display()))?;
    }
    // `src` is now empty; drop it so the backup removal has nothing left to do.
    fs::remove_dir(src).ok();
    Ok(())
}

/// Finish or roll back an update that was interrupted between the swap renames,
/// so a cancelled / killed / power-cut update can never leave a broken tree.
/// Idempotent and safe to call before any update or launch:
///   * backup present, app dir gone  -> restore the untouched old version
///   * backup present, app dir there  -> new tree is in; carry data + drop old
///   * only staging present           -> junk from a pre-swap abort; discard it
pub fn recover_interrupted_update(app_name: &str) -> Result<()> {
    let app_dir = app_dir(app_name)?;
    let (staging, backup) = swap_paths(app_name)?;

    if backup.exists() {
        if app_dir.exists() {
            // The new tree was already swapped in; complete the data hand-off.
            carry_over_user_data(&backup, &app_dir)?;
            let _ = fs::remove_dir_all(&backup);
        } else {
            // The old tree was parked but the new one never landed; put it back.
            fs::rename(&backup, &app_dir).with_context(|| {
                format!("failed to restore '{app_name}' after an interrupted update")
            })?;
        }
    }
    // A leftover staging tree (with no backup) means we were interrupted before
    // touching the live tree — it's a half-built throwaway.
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    Ok(())
}

/// Check every installed (non-alias) app for a newer package version without
/// modifying anything.  Returns app name -> latest available version for the
/// apps that have an update.  Network- and pacman-bound, so callers should run
/// it off any UI thread.
pub fn check_all_updates() -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(manifests) = list_all_apps() else { return out };
    for manifest in &manifests {
        if manifest.app.alias_of.is_some() {
            continue;
        }
        let name = &manifest.app.name;
        // Query the real upstream package (pkg_name) for custom --app-name
        // installs, but key the result on the app's name so the TUI can match
        // the dot to the list entry.
        let pkg = manifest.app.pkg_name.as_deref().unwrap_or(name);
        let main_pkg = manifest.packages.iter().find(|p| p.name == pkg);
        let current = main_pkg.map(|p| p.version.as_str()).unwrap_or("0");
        let latest = match main_pkg.map(|p| &p.source).unwrap_or(&PackageSource::Official) {
            PackageSource::Official => get_official_version(pkg),
            PackageSource::Aur => get_aur_version(pkg),
        };
        if let Ok(Some(ver)) = latest {
            if is_newer(&ver, current).unwrap_or(false) {
                out.insert(name.clone(), ver);
            }
        }
    }
    out
}

fn get_official_version(pkg_name: &str) -> Result<Option<String>> {
    crate::distro::pkg_latest_version(pkg_name)
}

fn get_aur_version(pkg_name: &str) -> Result<Option<String>> {
    let url = format!("https://aur.archlinux.org/rpc/v5/info?arg[]={pkg_name}");
    let client = reqwest::blocking::Client::new();
    let json: serde_json::Value = client
        .get(&url)
        .send()
        .context("failed to query AUR RPC")?
        .json()
        .context("failed to parse AUR RPC JSON")?;
    let version = json
        .get("results")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|pkg| pkg.get("Version"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Ok(version)
}

fn is_newer(candidate: &str, current: &str) -> Result<bool> {
    crate::distro::version_is_newer(candidate, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn old_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    #[test]
    fn delta_refetches_only_changed_and_new_packages() {
        let old = old_map(&[("app", "1.0"), ("liba", "2.0"), ("libb", "3.0")]);
        let new = [("app", "1.1"), ("liba", "2.0"), ("libb", "3.0"), ("libc", "1.0")];
        let (delta, changed) =
            plan_delta(&old, new.iter().map(|(n, v)| (*n, *v)), false, true);
        assert!(delta);
        // app bumped, libc is new; liba/libb unchanged.
        assert_eq!(changed.len(), 2);
        assert!(changed.contains("app"));
        assert!(changed.contains("libc"));
        assert!(!changed.contains("liba"));
    }

    #[test]
    fn delta_disabled_when_a_package_was_removed() {
        // libb disappeared from the new set — an overlay can't clean its files.
        let old = old_map(&[("app", "1.0"), ("libb", "3.0")]);
        let new = [("app", "1.1")];
        let (delta, _changed) =
            plan_delta(&old, new.iter().map(|(n, v)| (*n, *v)), false, true);
        assert!(!delta);
    }

    #[test]
    fn delta_disabled_by_full_flag_and_missing_tree() {
        let old = old_map(&[("app", "1.0")]);
        let new = [("app", "1.1")];
        // --full forces a clean rebuild.
        assert!(!plan_delta(&old, new.iter().map(|(n, v)| (*n, *v)), true, true).0);
        // no live tree yet → nothing to reuse.
        assert!(!plan_delta(&old, new.iter().map(|(n, v)| (*n, *v)), false, false).0);
        // nothing installed → nothing to reuse.
        assert!(!plan_delta(&HashMap::new(), new.iter().map(|(n, v)| (*n, *v)), false, true).0);
    }

    #[test]
    fn clone_package_tree_hardlinks_files_and_skips_user_data() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("live");
        let dst = tmp.path().join("staging");
        fs::create_dir_all(src.join("usr/bin")).unwrap();
        fs::write(src.join("usr/bin/app"), b"binary").unwrap();
        std::os::unix::fs::symlink("usr/bin", src.join("bin")).unwrap();
        // User data + manifest that must NOT be cloned (they're carried/rewritten).
        fs::create_dir_all(src.join("home/whisper")).unwrap();
        fs::write(src.join("home/whisper/profile"), b"x").unwrap();
        fs::write(src.join("config.ini"), b"cfg").unwrap();
        fs::create_dir_all(src.join(".snapshots/snap1")).unwrap();
        fs::write(src.join(".manifest.toml"), b"old").unwrap();

        clone_package_tree(&src, &dst).unwrap();

        // Package files cloned as hard links (same inode = near-free).
        let a = fs::metadata(src.join("usr/bin/app")).unwrap();
        let b = fs::metadata(dst.join("usr/bin/app")).unwrap();
        assert_eq!(a.ino(), b.ino(), "cloned file should be a hard link");
        assert!(dst.join("bin").symlink_metadata().unwrap().file_type().is_symlink());
        // User data + manifest skipped.
        assert!(!dst.join("home").exists());
        assert!(!dst.join("config.ini").exists());
        assert!(!dst.join(".snapshots").exists());
        assert!(!dst.join(".manifest.toml").exists());
    }

    /// Regression: the `filesystem` package ships an empty `home/`, so the
    /// freshly-extracted tree already has `home/` when carry-over runs. The old
    /// user profile must be merged in, not silently dropped with the backup.
    #[test]
    fn carry_over_merges_old_home_over_package_skeleton() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("old");
        let newtree = tmp.path().join("new");

        // Old tree: real browser profile + per-app config + a snapshot.
        fs::create_dir_all(backup.join("home/whisper/.config/vivaldi")).unwrap();
        fs::write(backup.join("home/whisper/.config/vivaldi/Prefs"), b"my settings").unwrap();
        fs::write(backup.join("config.ini"), b"[temp]\n").unwrap();
        fs::create_dir_all(backup.join(".snapshots/snap1")).unwrap();

        // New tree: only the empty `home/` skeleton the filesystem package ships.
        fs::create_dir_all(newtree.join("home")).unwrap();

        carry_over_user_data(&backup, &newtree).unwrap();

        let prefs = newtree.join("home/whisper/.config/vivaldi/Prefs");
        assert!(prefs.is_file(), "profile lost during carry-over");
        assert_eq!(fs::read(&prefs).unwrap(), b"my settings");
        assert!(newtree.join("config.ini").is_file());
        assert!(newtree.join(".snapshots/snap1").is_dir());
        // Everything was moved out of the backup, so dropping it loses nothing.
        assert!(!backup.join("home/whisper").exists());
    }

    /// The fast path (new tree lacks the item entirely) still moves it wholesale.
    #[test]
    fn carry_over_moves_when_new_tree_has_no_home() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("old");
        let newtree = tmp.path().join("new");
        fs::create_dir_all(backup.join("home/whisper")).unwrap();
        fs::write(backup.join("home/whisper/f"), b"x").unwrap();
        fs::create_dir_all(&newtree).unwrap();

        carry_over_user_data(&backup, &newtree).unwrap();

        assert_eq!(fs::read(newtree.join("home/whisper/f")).unwrap(), b"x");
    }

    /// Old files win over any colliding entry the new skeleton happened to carry.
    #[test]
    fn merge_prefers_old_file_over_new() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("old");
        let newtree = tmp.path().join("new");
        fs::create_dir_all(backup.join("home/whisper")).unwrap();
        fs::write(backup.join("home/whisper/Prefs"), b"real").unwrap();
        // New tree ships a stub file at the same path.
        fs::create_dir_all(newtree.join("home/whisper")).unwrap();
        fs::write(newtree.join("home/whisper/Prefs"), b"stub").unwrap();

        carry_over_user_data(&backup, &newtree).unwrap();

        assert_eq!(fs::read(newtree.join("home/whisper/Prefs")).unwrap(), b"real");
    }
}
