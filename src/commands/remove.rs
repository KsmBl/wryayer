use crate::launcher::remove_launcher;
use crate::manifest::{app_dir, read_manifest};
use anyhow::Result;
use std::fs;

pub fn run(app_name: &str) -> Result<()> {
    let manifest = match read_manifest(app_name) {
        Ok(m) => m,
        Err(_) => {
            eprintln!("'{app_name}' is not installed.");
            return Ok(());
        }
    };

    for launcher in &manifest.app.launchers {
        remove_launcher(launcher)?;
        eprintln!("Removed launcher: ~/bin/{launcher}");
    }

    let dir = app_dir(app_name)?;
    fs::remove_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("failed to remove {}: {e}", dir.display()))?;

    eprintln!("Removed '{app_name}'.");
    Ok(())
}
