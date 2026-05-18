use crate::manifest::{
    app_dir, list_all_apps, now_rfc3339, read_manifest, write_manifest, AppMeta, Manifest,
    PackageEntry, PackageSource,
};
use crate::commands::install::{ensure_base_layout, ensure_owner_readable, run_ldconfig};
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
        let main_pkg = manifest.packages.iter().find(|p| p.name == manifest.app.name);
        let current_version = main_pkg.map(|p| p.version.as_str()).unwrap_or("0");

        let latest_version = match main_pkg.map(|p| &p.source).unwrap_or(&PackageSource::Official) {
            PackageSource::Official => get_official_version(name)?,
            PackageSource::Aur => get_aur_version(name)?,
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

    eprintln!("Resolving dependencies for {app_name}...");
    let mut resolved = resolve_full_dep_tree(app_name)?;

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

    // Remove old files but keep the directory
    if app_dir.exists() {
        for entry in fs::read_dir(&app_dir)
            .with_context(|| format!("failed to read app dir {}", app_dir.display()))?
        {
            let entry = entry.context("failed to read entry")?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if file_name == ".manifest.toml" {
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

    eprintln!("Updated '{app_name}'.");
    Ok(())
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
