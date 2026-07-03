use crate::manifest::{
    app_dir, list_all_apps, now_rfc3339, read_manifest, write_manifest, AppMeta, Manifest,
    PackageEntry, PackageSource,
};
use crate::commands::install::{ensure_base_layout, ensure_owner_readable, regenerate_runtime_caches, run_ldconfig};
use crate::package::{
    build_aur, download_official, extract_package, resolve_full_dep_tree,
    satisfy_missing_sonames,
};
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

pub fn run(app_name: Option<&str>, check_only: bool) -> Result<()> {
    let manifests = match app_name {
        Some(name) => vec![read_manifest(name)
            .with_context(|| format!("'{name}' is not installed"))?],
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
                    reinstall(manifest)?;
                }
            }
        }
    }

    if check_only && !has_updates {
        eprintln!("All apps are up to date.");
    }

    Ok(())
}

fn reinstall(manifest: &crate::manifest::Manifest) -> Result<()> {
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

    let home = std::env::var("HOME").context("HOME not set")?;
    let cache_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("pkg");
    let build_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("build");

    for pkg in &mut resolved {
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

    let app_dir = app_dir(app_name)?;

    // Remove old package-provided files but keep user data: the sandbox home
    // (browser profiles, font caches, GUI app settings), the per-app wryayer
    // config, the install manifest, and all snapshots (so a post-update
    // rollback still returns to a pre-update version).  Without this, every
    // update wipes the user's Firefox profile and every saved snapshot.
    const PRESERVE: &[&str] =
        &[".manifest.toml", "config.ini", "home", crate::commands::snapshot::SNAP_DIR];
    if app_dir.exists() {
        for entry in fs::read_dir(&app_dir)
            .with_context(|| format!("failed to read app dir {}", app_dir.display()))?
        {
            let entry = entry.context("failed to read entry")?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if PRESERVE.contains(&file_name) {
                continue;
            }
            if path.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            } else {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
        }
    }

    for pkg in &resolved {
        let pkg_path = pkg.pkg_path.as_ref().unwrap();
        eprintln!("Extracting {}...", pkg.name);
        extract_package(pkg_path, &app_dir)
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
    write_manifest(app_name, &new_manifest)?;

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
