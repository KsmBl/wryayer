use crate::launcher::{create_launcher, remove_launcher};
use crate::manifest::{
    app_dir, now_rfc3339, write_manifest, AppMeta, Manifest, PackageEntry, PackageSource,
};
use crate::package::{
    build_aur, download_official, extract_package, resolve_full_dep_tree,
    satisfy_missing_sonames,
};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub fn run(pkg_name: &str, app_name: Option<&str>, bin_name: Option<&str>) -> Result<()> {
    let app_name = app_name.unwrap_or(pkg_name);
    let bin_name = bin_name.unwrap_or(pkg_name);

    let app_dir = app_dir(app_name)?;
    if app_dir.join(".manifest.toml").exists() {
        eprintln!("'{app_name}' is already installed. Use `wryayer update {app_name}` to update.");
        return Ok(());
    }

    eprintln!("Resolving dependencies for {pkg_name}...");
    let resolved = resolve_full_dep_tree(pkg_name)?;

    eprintln!(
        "Will install {} package(s): {}",
        resolved.len(),
        resolved
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let home = std::env::var("HOME").context("HOME not set")?;
    let cache_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("pkg");
    let build_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("build");

    let mut resolved = resolved;
    for pkg in &mut resolved {
        match pkg.source {
            PackageSource::Official => {
                eprintln!("[official] {}", pkg.name);
                let path = download_official(&pkg.name, &cache_dir)
                    .with_context(|| format!("failed to download {}", pkg.name))?;
                pkg.pkg_path = Some(path);
            }
            PackageSource::Aur => {
                eprintln!("[aur]      {}", pkg.name);
                let path = build_aur(&pkg.name, &build_dir)
                    .with_context(|| format!("failed to build AUR package {}", pkg.name))?;
                pkg.pkg_path = Some(path);
            }
        }
    }

    fs::create_dir_all(&app_dir)
        .with_context(|| format!("failed to create app dir {}", app_dir.display()))?;

    let mut created_launchers: Vec<String> = vec![];
    let result: Result<()> = (|| {
        for pkg in &resolved {
            let pkg_path = pkg.pkg_path.as_ref().unwrap();
            eprintln!("Extracting {}...", pkg.name);
            extract_package(pkg_path, &app_dir)
                .with_context(|| format!("failed to extract {}", pkg.name))?;
        }

        // Compile GLib schemas — pacman hooks do this at install time,
        // but our tar extraction skips hooks.
        let schemas_dir = app_dir.join("usr/share/glib-2.0/schemas");
        if schemas_dir.exists() {
            let _ = std::process::Command::new("glib-compile-schemas")
                .arg(&schemas_dir)
                .status();
        }

        eprintln!("Checking for missing shared library dependencies...");
        match satisfy_missing_sonames(&app_dir, &cache_dir) {
            Ok(extra) if !extra.is_empty() => eprintln!("  Added: {}", extra.join(", ")),
            Ok(_) => {}
            Err(e) => eprintln!("  Warning: soname check failed: {e:#}"),
        }

        eprintln!("Building library cache...");
        run_ldconfig(&app_dir);

        let launcher_path = create_launcher(app_name, bin_name)
            .with_context(|| format!("failed to create launcher for {bin_name}"))?;
        created_launchers.push(bin_name.to_string());
        eprintln!("Created launcher: {}", launcher_path.display());

        let packages: Vec<PackageEntry> = resolved
            .iter()
            .map(|p| PackageEntry {
                name: p.name.clone(),
                version: p.version.clone(),
                source: p.source.clone(),
            })
            .collect();

        let manifest = Manifest {
            app: AppMeta {
                name: app_name.to_string(),
                main_binary: bin_name.to_string(),
                installed_at: now_rfc3339(),
                launchers: created_launchers.clone(),
            },
            packages,
        };
        write_manifest(app_name, &manifest)?;
        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("Installation failed, cleaning up...");
        for launcher in &created_launchers {
            let _ = remove_launcher(launcher);
        }
        let _ = fs::remove_dir_all(&app_dir);
        return Err(e);
    }

    eprintln!(
        "\nInstalled '{}' to ~/.wryayer/{}/",
        app_name, app_name
    );
    eprintln!("Run with: ~/bin/{bin_name}  or  wryayer run {app_name}");
    Ok(())
}

pub fn run_ldconfig(app_dir: &Path) {
    match std::process::Command::new("ldconfig")
        .arg("-r")
        .arg(app_dir)
        .status()
    {
        Ok(s) if !s.success() => eprintln!("  warning: ldconfig exited with {s}"),
        Err(_) => eprintln!("  warning: ldconfig not found, skipping cache build"),
        _ => {}
    }
}
