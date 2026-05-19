use crate::commands::dedup::{all_du, format_bytes};
use crate::manifest::{list_all_apps, Manifest};
use anyhow::Result;
use std::collections::HashMap;

pub fn run() -> Result<()> {
    let all_apps = list_all_apps()?;

    if all_apps.is_empty() {
        println!("No apps installed. Use `wryayer install <pkg>` to get started.");
        return Ok(());
    }

    let (sizes, total_apparent, total_actual) = all_du().unwrap_or_default();

    // Partition into root apps and aliases; build target→children map.
    let mut by_target: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();

    for (i, app) in all_apps.iter().enumerate() {
        if let Some(ref target) = app.app.alias_of {
            by_target.entry(target.clone()).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    roots.sort_by_key(|&i| &all_apps[i].app.name);
    for children in by_target.values_mut() {
        children.sort_by_key(|&i| &all_apps[i].app.name);
    }

    // Column widths.  Alias rows are prefixed with "  ├── " / "  └── " (6 chars).
    const PREFIX: usize = 6;
    let name_width = all_apps
        .iter()
        .map(|a| a.app.name.len() + if a.app.alias_of.is_some() { PREFIX } else { 0 })
        .max()
        .unwrap_or(4)
        .max(4);
    let ver_width = all_apps
        .iter()
        .flat_map(|a| a.packages.iter().filter(|p| p.name == a.app.name).map(|p| p.version.len()))
        .max()
        .unwrap_or(7)
        .max(7);
    let size_width = 9usize;
    let sep_width = name_width + ver_width + 19 + size_width + 10;

    println!(
        "{:<name_width$}  {:<ver_width$}  {:<19}  {:>size_width$}  launchers",
        "name", "version", "installed", "size",
    );
    println!("{}", "-".repeat(sep_width));

    let print_row = |app: &Manifest, prefix: &str| {
        let display = format!("{prefix}{}", app.app.name);
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
            display, version, installed, size_str, launchers
        );
    };

    let empty: Vec<usize> = vec![];
    for &root_idx in &roots {
        print_row(&all_apps[root_idx], "");

        let children = by_target.get(&all_apps[root_idx].app.name).unwrap_or(&empty);
        for (n, &child_idx) in children.iter().enumerate() {
            let connector = if n + 1 == children.len() { "  └── " } else { "  ├── " };
            print_row(&all_apps[child_idx], connector);
        }
    }

    // Orphan aliases whose target was removed — show them flat.
    let printed: std::collections::HashSet<usize> = roots
        .iter()
        .copied()
        .chain(by_target.values().flatten().copied())
        .collect();
    for (i, app) in all_apps.iter().enumerate() {
        if !printed.contains(&i) {
            print_row(app, "");
        }
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
            println!("total: {}", format_bytes(total_apparent));
        }
    }

    Ok(())
}
