use crate::launcher::remove_launcher;
use crate::manifest::{app_dir, list_all_apps, read_manifest_or_marker};
use anyhow::{bail, Result};
use std::fs;

pub fn run_cascade(app_name: &str) -> Result<()> {
    let manifest = match read_manifest_or_marker(app_name) {
        Ok(m) => m,
        Err(_) => {
            eprintln!("'{app_name}' is not installed.");
            return Ok(());
        }
    };

    if manifest.app.alias_of.is_some() {
        return run(app_name);
    }

    let dependents: Vec<String> = list_all_apps()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.app.alias_of.as_deref() == Some(app_name))
        .map(|m| m.app.name)
        .collect();

    for alias in &dependents {
        eprintln!("Removing alias '{alias}'...");
        run(alias)?;
    }

    run(app_name)
}

pub fn run(app_name: &str) -> Result<()> {
    let manifest = match read_manifest_or_marker(app_name) {
        Ok(m) => m,
        Err(_) => {
            eprintln!("'{app_name}' is not installed.");
            return Ok(());
        }
    };

    // If this app is the target of any aliases (created via `install --into`),
    // removing it would silently break them. Force the user to clean up first.
    if manifest.app.alias_of.is_none() {
        let dependents: Vec<String> = list_all_apps()
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.app.alias_of.as_deref() == Some(app_name))
            .map(|m| m.app.name)
            .collect();
        if !dependents.is_empty() {
            bail!(
                "cannot remove '{app_name}': {} alias(es) point at it ({}). Remove those first.",
                dependents.len(),
                dependents.join(", ")
            );
        }
    }

    for launcher in &manifest.app.launchers {
        for path in remove_launcher(launcher)? {
            eprintln!("Removed launcher: {}", path.display());
        }
    }
    if let Err(e) = crate::desktop::remove(app_name) {
        eprintln!("warning: desktop entries not removed: {e:#}");
    }

    // An encrypted app's files are inside its container, not under the app dir.
    // Unmount first (removing a mount point would otherwise fail, and deleting
    // through a live mount would wipe the container's contents rather than the
    // container), then delete the container file itself.
    if crate::veracrypt::is_encrypted(app_name) {
        crate::veracrypt::dismount(app_name)?;
        let container = crate::veracrypt::container_path(app_name)?;
        fs::remove_file(&container)
            .map_err(|e| anyhow::anyhow!("failed to remove {}: {e}", container.display()))?;
        crate::veracrypt::remove_marker(app_name);
        eprintln!("Removed container: {}", container.display());

        // Drop the stored password, if the master store held one.
        if let Ok(Some(mut store)) = crate::secrets::open_cached() {
            if store.remove(app_name) {
                store.save()?;
            }
        }
    }

    let dir = app_dir(app_name)?;
    fs::remove_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("failed to remove {}: {e}", dir.display()))?;

    if manifest.app.alias_of.is_some() {
        eprintln!("Removed alias '{app_name}' (target tree left intact).");
    } else {
        eprintln!("Removed '{app_name}'.");
    }
    Ok(())
}
