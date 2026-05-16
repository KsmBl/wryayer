use crate::commands::dedup::{all_du, format_bytes};
use crate::manifest::list_all_apps;
use anyhow::Result;

pub fn run() -> Result<()> {
    let apps = list_all_apps()?;

    if apps.is_empty() {
        println!("No apps installed. Use `wryayer install <pkg>` to get started.");
        return Ok(());
    }

    let (sizes, total_apparent, total_actual) = all_du().unwrap_or_default();

    let name_width = apps.iter().map(|a| a.app.name.len()).max().unwrap_or(4).max(4);
    let ver_width = apps
        .iter()
        .flat_map(|a| a.packages.iter().filter(|p| p.name == a.app.name).map(|p| p.version.len()))
        .max()
        .unwrap_or(7)
        .max(7);
    let size_width = 9usize; // e.g. "1023.9 MB"

    let sep_width = name_width + ver_width + 19 + size_width + 10;

    println!(
        "{:<name_width$}  {:<ver_width$}  {:<19}  {:>size_width$}  launchers",
        "name", "version", "installed", "size",
    );
    println!("{}", "-".repeat(sep_width));

    for app in &apps {
        let version = app
            .packages
            .iter()
            .find(|p| p.name == app.app.name)
            .map(|p| p.version.as_str())
            .unwrap_or("?");
        let installed = app.app.installed_at.get(..19).unwrap_or(&app.app.installed_at);
        let size_str = sizes
            .get(&app.app.name)
            .map(|&b| format_bytes(b))
            .unwrap_or_else(|| "-".to_string());
        let launchers = app.app.launchers.join(", ");
        println!(
            "{:<name_width$}  {:<ver_width$}  {:<19}  {:>size_width$}  {}",
            app.app.name, version, installed, size_str, launchers
        );
    }

    if total_apparent > 0 {
        println!("{}", "-".repeat(sep_width));
        let savings = total_apparent.saturating_sub(total_actual);
        if savings > 0 {
            println!(
                "apparent: {}   on disk: {}   saves: {}",
                format_bytes(total_apparent),
                format_bytes(total_actual),
                format_bytes(savings),
            );
        } else {
            println!(
                "total: {}",
                format_bytes(total_apparent),
            );
        }
    }

    Ok(())
}
