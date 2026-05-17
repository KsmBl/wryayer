use crate::manifest::PackageSource;
use anyhow::{Context, Result};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub source: PackageSource,
    pub pkg_path: Option<PathBuf>,
}

pub fn resolve_full_dep_tree(root_pkg: &str) -> Result<Vec<ResolvedPackage>> {
    let mut visited: HashSet<String> = HashSet::new();
    // Queue holds (requested_name) — may differ from resolved name for virtual deps
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut result: Vec<ResolvedPackage> = Vec::new();

    queue.push_back(root_pkg.to_string());

    while !queue.is_empty() {
        let mut batch: Vec<String> = Vec::new();
        while let Some(pkg) = queue.pop_front() {
            if !visited.contains(&pkg) {
                batch.push(pkg);
            }
        }

        let mut aur_pending: Vec<String> = Vec::new();

        for pkg in batch {
            if visited.contains(&pkg) {
                continue;
            }

            match query_official(&pkg)? {
                Some((resolved_name, version, deps)) => {
                    // Mark both the virtual name and the resolved name as visited
                    visited.insert(pkg.clone());
                    visited.insert(resolved_name.clone());
                    for dep in &deps {
                        if !visited.contains(dep) {
                            queue.push_back(dep.clone());
                        }
                    }
                    result.push(ResolvedPackage {
                        name: resolved_name,
                        version,
                        source: PackageSource::Official,
                        pkg_path: None,
                    });
                }
                None => {
                    // Soname deps that didn't resolve via the provider lookup are
                    // true virtual provides — skip rather than hitting AUR
                    if is_soname_dep(&pkg) {
                        eprintln!("warning: cannot resolve soname dep '{pkg}', skipping");
                        visited.insert(pkg);
                    } else {
                        aur_pending.push(pkg);
                    }
                }
            }
        }

        if !aur_pending.is_empty() {
            let aur_results = query_aur_batch(&aur_pending)?;
            for pkg in &aur_pending {
                if visited.contains(pkg) {
                    continue;
                }
                match aur_results.get(pkg) {
                    Some(info) => {
                        visited.insert(pkg.clone());
                        for dep in &info.depends {
                            if !visited.contains(dep) {
                                queue.push_back(dep.clone());
                            }
                        }
                        result.push(ResolvedPackage {
                            name: pkg.clone(),
                            version: info.version.clone(),
                            source: PackageSource::Aur,
                            pkg_path: None,
                        });
                    }
                    None => {
                        eprintln!("warning: skipping virtual/unknown dependency '{pkg}'");
                        visited.insert(pkg.clone());
                    }
                }
            }
        }
    }

    Ok(result)
}

// Returns (resolved_package_name, version, deps).
// resolved_package_name may differ from pkg_name when pkg_name is a virtual dep
// (e.g. "jack" resolves to "jack2" or "pipewire-jack").
fn query_official(pkg_name: &str) -> Result<Option<(String, String, Vec<String>)>> {
    // Direct lookup first
    if let Some((ver, deps)) = pacman_si(pkg_name)? {
        return Ok(Some((pkg_name.to_string(), ver, deps)));
    }

    // Virtual provider fallback: ask pacman which real package satisfies this dep.
    // -dd skips dep/conflict checks — we're only interested in the provider name,
    // not whether a system-wide install would succeed.
    let output = Command::new("pacman")
        .args([
            "-Spdd",
            "--noconfirm",
            "--print-format",
            "%n",
            pkg_name,
        ])
        .output()
        .context("failed to spawn pacman -Spdd")?;

    if output.status.success() {
        let provider = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !provider.is_empty() && provider != pkg_name {
            if let Some((ver, deps)) = pacman_si(&provider)? {
                return Ok(Some((provider, ver, deps)));
            }
        }
    }

    // Soname fallback: find which installed package owns the .so file on the host.
    // pacman -Spdd returns nothing when the package is already installed, so we
    // look up the actual file instead: "libasound.so.2" -> pacman -Qqo /usr/lib/libasound.so.2
    if is_soname_dep(pkg_name) {
        if let Some(owner) = soname_owner(pkg_name)? {
            if let Some((ver, deps)) = pacman_si(&owner)? {
                return Ok(Some((owner, ver, deps)));
            }
        }
    }

    Ok(None)
}

pub fn soname_owner(soname: &str) -> Result<Option<String>> {
    // Strip =VERSION suffix (e.g. "libreadline.so=8-64" -> "libreadline.so")
    let filename = soname.split('=').next().unwrap_or(soname);
    for dir in ["/usr/lib", "/usr/lib64", "/lib", "/lib64"] {
        let path = format!("{dir}/{filename}");
        let out = Command::new("pacman")
            .args(["-Qqo", &path])
            .output()
            .context("failed to spawn pacman -Qqo")?;
        if out.status.success() {
            let pkg = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !pkg.is_empty() {
                return Ok(Some(pkg));
            }
        }
    }
    Ok(None)
}

fn pacman_si(pkg_name: &str) -> Result<Option<(String, Vec<String>)>> {
    let output = Command::new("pacman")
        .args(["-Si", pkg_name])
        .output()
        .context("failed to spawn pacman -Si")?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = parse_pacman_field(&stdout, "Version").unwrap_or_default();
    let deps = parse_pacman_depends(&stdout);
    Ok(Some((version, deps)))
}

pub fn parse_pacman_field(stdout: &str, field: &str) -> Option<String> {
    for line in stdout.lines() {
        if line.starts_with(field) && line.contains(':') {
            let value = line.splitn(2, ':').nth(1)?.trim().to_string();
            return Some(value);
        }
    }
    None
}

pub fn parse_pacman_depends(stdout: &str) -> Vec<String> {
    for line in stdout.lines() {
        if line.starts_with("Depends On") {
            let value = match line.splitn(2, ':').nth(1) {
                Some(v) => v.trim(),
                None => return vec![],
            };
            if value == "None" {
                return vec![];
            }
            return value
                .split_whitespace()
                .map(|s| strip_version_constraint(s).to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    vec![]
}

pub fn strip_version_constraint(dep: &str) -> &str {
    dep.split(|c| matches!(c, '>' | '<' | '=' | '!'))
        .next()
        .unwrap_or(dep)
}

// Soname virtual provides like "libreadline.so" or "libreadline.so=8"
// are not real installable package names — skip them entirely.
pub fn is_soname_dep(name: &str) -> bool {
    name.contains(".so") && (name.ends_with(".so") || name.contains(".so=") || name.contains(".so."))
}

struct AurInfo {
    version: String,
    depends: Vec<String>,
}

const AUR_BATCH_SIZE: usize = 20;

fn query_aur_batch(names: &[String]) -> Result<std::collections::HashMap<String, AurInfo>> {
    if names.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let client = reqwest::blocking::Client::new();
    let mut map = std::collections::HashMap::new();

    for chunk in names.chunks(AUR_BATCH_SIZE) {
        let mut url = String::from("https://aur.archlinux.org/rpc/v5/info?");
        for (i, name) in chunk.iter().enumerate() {
            if i > 0 {
                url.push('&');
            }
            url.push_str("arg[]=");
            url.push_str(name);
        }

        let response = client
            .get(&url)
            .send()
            .context("failed to query AUR RPC")?;

        let json: serde_json::Value = response
            .json()
            .context("failed to parse AUR RPC response as JSON")?;

        if let Some(results) = json.get("results").and_then(serde_json::Value::as_array) {
            for pkg in results {
                let name = match pkg.get("Name").and_then(serde_json::Value::as_str) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                let version = pkg
                    .get("Version")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let depends = pkg
                    .get("Depends")
                    .and_then(serde_json::Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(|s| strip_version_constraint(s).to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                map.insert(name, AurInfo { version, depends });
            }
        }
    }

    Ok(map)
}
