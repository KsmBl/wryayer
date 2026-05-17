use crate::launcher::remove_launcher;
use crate::manifest::{app_dir, list_all_apps, read_manifest};
use anyhow::{bail, Result};
use std::fs;

pub fn run(app_name: &str) -> Result<()> {
    let manifest = match read_manifest(app_name) {
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
        remove_launcher(launcher)?;
        eprintln!("Removed launcher: ~/bin/{launcher}");
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
