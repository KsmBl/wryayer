use crate::commands::install::{ensure_base_layout, ensure_owner_readable, regenerate_runtime_caches, run_ldconfig};
use crate::manifest::{app_dir, read_manifest};
use crate::package::{find_missing_sonames, satisfy_missing_sonames};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn run(app_name: &str) -> Result<()> {
    crate::commands::encrypt::require_unlocked(app_name, "repair")?;
    read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    let app_dir = app_dir(app_name)?;
    let home = std::env::var("HOME").context("HOME not set")?;
    let cache_dir = PathBuf::from(&home).join(".cache/wryayer/pkg");

    eprintln!("Restoring base filesystem layout...");
    ensure_base_layout(&app_dir)
        .with_context(|| "failed to create base filesystem symlinks")?;

    eprintln!("Fixing owner-readability on extracted files...");
    let fixed = ensure_owner_readable(&app_dir)
        .with_context(|| "failed to fix file permissions")?;
    if fixed > 0 {
        eprintln!("  Repaired permissions on {fixed} file(s)");
    }

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
        eprintln!("Rebuilding library cache...");
        run_ldconfig(&app_dir);
    }

    regenerate_runtime_caches(&app_dir);

    Ok(())
}
