use crate::manifest::{app_dir, read_manifest};
use crate::package::{find_missing_sonames, satisfy_missing_sonames};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn run(app_name: &str) -> Result<()> {
    read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    let app_dir = app_dir(app_name)?;
    let home = std::env::var("HOME").context("HOME not set")?;
    let cache_dir = PathBuf::from(&home).join(".cache/wryayer/pkg");

    eprintln!("Scanning {app_name} for missing shared library dependencies...");

    let missing = find_missing_sonames(&app_dir)?;
    if missing.is_empty() {
        eprintln!("No missing libraries found — {app_name} looks healthy.");
        return Ok(());
    }

    eprintln!("Missing: {}", missing.join(", "));

    let installed = satisfy_missing_sonames(&app_dir, &cache_dir)?;
    if installed.is_empty() {
        eprintln!("Could not resolve any missing libraries automatically.");
    } else {
        eprintln!("Repaired {app_name}: installed {}", installed.join(", "));
    }

    Ok(())
}
