//! `wryayer clean` — remove the shared download/build/dependency cache.
//!
//! The cache lives at `~/.cache/wryayer` (outside `~/.wryayer`), so its
//! contents — package tarballs, AUR build dirs, and resolved dependency lists —
//! reveal which apps have been installed. Wiping it leaves no such record
//! outside the (optionally encrypted) `~/.wryayer` container. Installs can do
//! this automatically via the `clean_cache` setting; this command does it on
//! demand.

use anyhow::Result;
use std::path::PathBuf;

/// Path to the shared cache dir, or None if HOME is unset.
fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cache").join("wryayer"))
}

/// Delete `~/.cache/wryayer` if it exists. Best-effort: logs but never panics.
/// Shared with the post-install auto-clean path.
pub fn clean_cache() {
    let Some(cache) = cache_dir() else { return };
    if cache.exists() {
        match std::fs::remove_dir_all(&cache) {
            Ok(()) => eprintln!("Cleaned cache: {}", cache.display()),
            Err(e) => eprintln!("warning: failed to clean cache {}: {e:#}", cache.display()),
        }
    }
}

/// CLI entry point for `wryayer clean`.
pub fn run() -> Result<()> {
    match cache_dir() {
        Some(cache) if cache.exists() => clean_cache(),
        Some(cache) => eprintln!("Cache already empty: {} does not exist.", cache.display()),
        None => eprintln!("warning: HOME not set — nothing to clean."),
    }
    Ok(())
}
