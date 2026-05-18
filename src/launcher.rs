use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub fn launchers_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("bin"))
}

pub fn create_launcher(app_name: &str, binary_name: &str) -> Result<PathBuf> {
    let dir = launchers_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create launchers dir {}", dir.display()))?;

    let path = dir.join(binary_name);
    let content = launcher_content(app_name);
    fs::write(&path, &content)
        .with_context(|| format!("failed to write launcher at {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to chmod launcher at {}", path.display()))?;
    Ok(path)
}

pub fn remove_launcher(binary_name: &str) -> Result<()> {
    let path = launchers_dir()?.join(binary_name);
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read launcher at {}", path.display()))?;
    if !content.contains("# wryayer managed launcher") {
        eprintln!(
            "warning: skipping {} — does not look like a wryayer launcher",
            path.display()
        );
        return Ok(());
    }
    fs::remove_file(&path)
        .with_context(|| format!("failed to remove launcher at {}", path.display()))?;
    Ok(())
}

fn launcher_content(app_name: &str) -> String {
    format!(
        r#"#!/bin/bash
# wryayer managed launcher for {app_name}
exec wryayer run "{app_name}" "$@"
"#
    )
}
