use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Manifest {
    pub app: AppMeta,
    pub packages: Vec<PackageEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AppMeta {
    pub name: String,
    pub main_binary: String,
    pub installed_at: String,
    pub launchers: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PackageEntry {
    pub name: String,
    pub version: String,
    pub source: PackageSource,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PackageSource {
    Official,
    Aur,
}

pub fn wryayer_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".wryayer"))
}

pub fn app_dir(app_name: &str) -> Result<PathBuf> {
    Ok(wryayer_root()?.join(app_name))
}

pub fn manifest_path(app_name: &str) -> Result<PathBuf> {
    Ok(app_dir(app_name)?.join(".manifest.toml"))
}

pub fn read_manifest(app_name: &str) -> Result<Manifest> {
    let path = manifest_path(app_name)?;
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read manifest at {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("failed to parse manifest for {app_name}"))
}

pub fn write_manifest(app_name: &str, manifest: &Manifest) -> Result<()> {
    let path = manifest_path(app_name)?;
    let tmp_path = path.with_extension("toml.tmp");
    let content =
        toml::to_string_pretty(manifest).context("failed to serialize manifest to TOML")?;
    fs::write(&tmp_path, &content)
        .with_context(|| format!("failed to write manifest tmp file at {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "failed to rename manifest tmp to {}",
            path.display()
        )
    })?;
    Ok(())
}

pub fn list_all_apps() -> Result<Vec<Manifest>> {
    let root = wryayer_root()?;
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut manifests = vec![];
    for entry in fs::read_dir(&root)
        .with_context(|| format!("failed to read wryayer root {}", root.display()))?
    {
        let entry = entry.context("failed to read directory entry")?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let app_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        match read_manifest(&app_name) {
            Ok(m) => manifests.push(m),
            Err(_) => continue,
        }
    }
    manifests.sort_by(|a, b| a.app.name.cmp(&b.app.name));
    Ok(manifests)
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}
