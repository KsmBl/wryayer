//! `wryayer relink` — rebuild the host-side entry points for installed apps.
//!
//! Shortcuts used to live in `~/bin` and desktop entries did not exist at all,
//! so apps installed before either change have no `/usr/bin` shortcut and are
//! invisible to the desktop. Nothing about the app itself needs touching to fix
//! that: the manifest already records which shortcuts it owns, and the tree
//! already carries its `.desktop` files. This walks them and writes the host
//! side again.
//!
//! Also the repair for a shortcut deleted by hand, or one that fell back to
//! `~/bin` because root was out of reach at install time.

use crate::desktop;
use crate::launcher::{self, create_launcher};
use crate::manifest::{list_all_apps, read_manifest_or_marker};
use anyhow::{Context, Result};

pub fn run(app_name: Option<&str>) -> Result<()> {
    let apps: Vec<String> = match app_name {
        Some(name) => vec![name.to_string()],
        None => list_all_apps()
            .context("failed to list installed apps")?
            .into_iter()
            .map(|m| m.app.name)
            .collect(),
    };

    if apps.is_empty() {
        eprintln!("No apps installed.");
        return Ok(());
    }

    let mut shortcuts = 0usize;
    let mut entries = 0usize;
    for app in &apps {
        let manifest = match read_manifest_or_marker(app) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("warning: skipping '{app}': {e:#}");
                continue;
            }
        };

        for launcher in &manifest.app.launchers {
            // Two apps can want the same command name — an alias records the
            // same launcher as the app it points at. Rebuilding every app in
            // one pass must not hand the name to whichever happens to come
            // last in the list.
            if let Some((path, owner)) = launcher::foreign_owner(app, launcher) {
                eprintln!(
                    "{app}: '{launcher}' left alone — {} belongs to '{owner}'",
                    path.display()
                );
                continue;
            }
            match create_launcher(app, launcher) {
                Ok(path) => {
                    eprintln!("{app}: {}", path.display());
                    shortcuts += 1;
                }
                Err(e) => eprintln!("warning: {app}: shortcut '{launcher}' not created: {e:#}"),
            }
        }

        // A locked app's .desktop files are inside its unmounted container.
        // Unlocking every app just to publish menu entries would be a poor
        // trade, so leave those until the app is next unlocked or reinstalled.
        if crate::veracrypt::is_locked(app) {
            eprintln!("{app}: locked — desktop entries left alone");
            continue;
        }
        match desktop::install(app) {
            Ok(published) => {
                for entry in &published {
                    eprintln!("{app}: {}", entry.path.display());
                }
                entries += published.len();
            }
            Err(e) => eprintln!("warning: {app}: desktop entries not registered: {e:#}"),
        }
    }

    eprintln!("\nDone: {shortcuts} shortcuts, {entries} desktop entries.");
    Ok(())
}
