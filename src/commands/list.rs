use crate::manifest::list_all_apps;
use anyhow::Result;

pub fn run() -> Result<()> {
    let apps = list_all_apps()?;

    if apps.is_empty() {
        println!("No apps installed. Use `wryayer install <pkg>` to get started.");
        return Ok(());
    }

    let name_width = apps.iter().map(|a| a.app.name.len()).max().unwrap_or(4).max(4);
    let ver_width = apps
        .iter()
        .flat_map(|a| a.packages.iter().filter(|p| p.name == a.app.name).map(|p| p.version.len()))
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<name_width$}  {:<ver_width$}  {:<19}  launchers",
        "name", "version", "installed",
    );
    println!("{}", "-".repeat(name_width + ver_width + 30));

    for app in &apps {
        let version = app
            .packages
            .iter()
            .find(|p| p.name == app.app.name)
            .map(|p| p.version.as_str())
            .unwrap_or("?");
        let installed = app.app.installed_at.get(..19).unwrap_or(&app.app.installed_at);
        let launchers = app.app.launchers.join(", ");
        println!(
            "{:<name_width$}  {:<ver_width$}  {:<19}  {}",
            app.app.name, version, installed, launchers
        );
    }

    Ok(())
}
