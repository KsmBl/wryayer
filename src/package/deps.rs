use crate::distro::{self, Distro};
use crate::manifest::PackageSource;
use anyhow::{Context, Result};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

// ── Dep cache ──────────────────────────────────────────────────────────────────

/// How long a cached dependency resolution stays fresh. After this the entry is
/// treated as a miss and re-queried, so a package cached long ago can't keep
/// resolving to a stale version on a fresh install (the update path already
/// invalidates explicitly; this makes plain installs self-healing too).
const DEP_CACHE_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PkgDepCache {
    source: String,   // "official" or "aur"
    resolved: String, // canonical name (may differ for virtual providers)
    version: String,
    deps: Vec<String>,
    // Unix seconds when this entry was written. `default` (0) makes pre-TTL
    // cache files parse and immediately read as expired, so they re-query once.
    #[serde(default)]
    cached_at: u64,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn dep_cache_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".cache").join("wryayer").join("deps");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn dep_cache_path(pkg_name: &str) -> Option<PathBuf> {
    let safe: String = pkg_name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    Some(dep_cache_dir()?.join(format!("{safe}.toml")))
}

/// Drop cached dependency resolutions for the named packages so the next
/// resolve re-queries the package manager (and AUR) for current versions.
/// The dep cache never expires, so without this an update would re-resolve to
/// the version recorded at first install and write that stale value back into
/// the manifest — even though the freshly downloaded/built package is newer.
pub fn invalidate_dep_cache(pkg_names: &[String]) {
    for name in pkg_names {
        if let Some(path) = dep_cache_path(name) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn read_dep_cache(pkg_name: &str) -> Option<PkgDepCache> {
    let content = std::fs::read_to_string(dep_cache_path(pkg_name)?).ok()?;
    let entry: PkgDepCache = toml::from_str(&content).ok()?;
    // Expired entries are reported as a miss so the caller re-queries.
    if now_secs().saturating_sub(entry.cached_at) > DEP_CACHE_TTL_SECS {
        return None;
    }
    Some(entry)
}

fn write_dep_cache(pkg_name: &str, entry: &PkgDepCache) {
    if let Some(path) = dep_cache_path(pkg_name) {
        // Stamp every write with the current time regardless of what the caller
        // put in `cached_at`, so the TTL measures write time.
        let stamped = PkgDepCache { cached_at: now_secs(), ..entry.clone() };
        if let Ok(content) = toml::to_string_pretty(&stamped) {
            let _ = std::fs::write(path, content);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub source: PackageSource,
    pub pkg_path: Option<PathBuf>,
}

pub fn resolve_full_dep_tree(root_pkg: &str) -> Result<Vec<ResolvedPackage>> {
    let mut visited: HashSet<String> = HashSet::new();
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
            if distro::current() == Distro::Arch {
                let mut uncached: Vec<String> = Vec::new();
                let mut aur_results: std::collections::HashMap<String, AurInfo> =
                    std::collections::HashMap::new();

                for pkg in &aur_pending {
                    if let Some(c) = read_dep_cache(pkg) {
                        if c.source == "aur" {
                            aur_results.insert(pkg.clone(), AurInfo { version: c.version, depends: c.deps });
                            continue;
                        }
                    }
                    uncached.push(pkg.clone());
                }

                if !uncached.is_empty() {
                    let fetched = query_aur_batch(&uncached)?;
                    for (name, info) in &fetched {
                        write_dep_cache(name, &PkgDepCache {
                            source: "aur".into(),
                            resolved: name.clone(),
                            version: info.version.clone(),
                            deps: info.depends.clone(),
                            cached_at: 0, // stamped by write_dep_cache
                        });
                        aur_results.insert(name.clone(), AurInfo {
                            version: info.version.clone(),
                            depends: info.depends.clone(),
                        });
                    }
                }

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
            } else {
                // Non-Arch distros have no AUR equivalent; warn and skip.
                for pkg in &aur_pending {
                    eprintln!("warning: '{pkg}' not found in package repos, skipping");
                    visited.insert(pkg.clone());
                }
            }
        }
    }

    Ok(result)
}

/// Resolve a package name through the official repos, following virtual
/// providers and soname deps as needed.
/// Returns (resolved_name, version, deps) or None if not found anywhere.
fn query_official(pkg_name: &str) -> Result<Option<(String, String, Vec<String>)>> {
    if let Some(c) = read_dep_cache(pkg_name) {
        if c.source == "official" {
            return Ok(Some((c.resolved, c.version, c.deps)));
        }
    }

    // 1. Direct lookup
    if let Some((ver, deps)) = distro::query_pkg_info(pkg_name)? {
        write_dep_cache(pkg_name, &PkgDepCache {
            source: "official".into(),
            resolved: pkg_name.to_string(),
            version: ver.clone(),
            deps: deps.clone(),
            cached_at: 0, // stamped by write_dep_cache
        });
        return Ok(Some((pkg_name.to_string(), ver, deps)));
    }

    // 2. Virtual provider fallback
    if let Some(provider) = distro::resolve_virtual(pkg_name)? {
        if !provider.is_empty() && provider != pkg_name {
            if let Some((ver, deps)) = distro::query_pkg_info(&provider)? {
                write_dep_cache(pkg_name, &PkgDepCache {
                    source: "official".into(),
                    resolved: provider.clone(),
                    version: ver.clone(),
                    deps: deps.clone(),
                    cached_at: 0, // stamped by write_dep_cache
                });
                return Ok(Some((provider, ver, deps)));
            }
        }
    }

    // 3. Soname fallback: find which installed package on the host owns the .so
    if is_soname_dep(pkg_name) {
        if let Some(owner) = distro::soname_owner(pkg_name)? {
            if let Some((ver, deps)) = distro::query_pkg_info(&owner)? {
                write_dep_cache(pkg_name, &PkgDepCache {
                    source: "official".into(),
                    resolved: owner.clone(),
                    version: ver.clone(),
                    deps: deps.clone(),
                    cached_at: 0, // stamped by write_dep_cache
                });
                return Ok(Some((owner, ver, deps)));
            }
        }
    }

    Ok(None)
}

/// Thin wrapper kept for backwards compat and test accessibility.
pub fn soname_owner(soname: &str) -> Result<Option<String>> {
    distro::soname_owner(soname)
}

// ── Text parsers (Arch pacman output) — kept pub for integration tests ─────────

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

/// Soname virtual provides like "libreadline.so" or "libreadline.so=8"
/// are not real installable package names.
pub fn is_soname_dep(name: &str) -> bool {
    name.contains(".so")
        && (name.ends_with(".so") || name.contains(".so=") || name.contains(".so."))
}

// ── AUR batch query (Arch-only) ───────────────────────────────────────────────

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
