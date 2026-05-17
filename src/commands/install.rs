use crate::launcher::{create_launcher, remove_launcher};
use crate::manifest::{
    app_dir, now_rfc3339, read_manifest, write_manifest, AppMeta, Manifest, PackageEntry,
    PackageSource,
};
use crate::package::{
    build_aur, download_official, extract_package, resolve_full_dep_tree,
    satisfy_missing_sonames,
};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub fn run(
    pkg_name: &str,
    app_name: Option<&str>,
    bin_names: &[String],
    into: Option<&str>,
) -> Result<()> {
    let merge_mode = into.is_some();

    // target_name: where the package's files actually live on disk.
    //   - fresh mode: same as alias_name
    //   - merge mode: the existing app the user passed in --into
    // alias_name: the dir under ~/.wryayer/ that holds this install's manifest.
    //   - fresh mode: the only dir that exists for this app
    //   - merge mode: a thin alias dir holding a manifest with alias_of = target_name
    let target_name = into.unwrap_or(pkg_name).to_string();
    let alias_name = app_name.unwrap_or(pkg_name).to_string();
    let target_dir = app_dir(&target_name)?;
    let alias_dir = app_dir(&alias_name)?;

    // Multi-launcher: if user passed --bin-names, use that list verbatim;
    // otherwise create a single launcher named after the package.
    let bin_names: Vec<String> = if bin_names.is_empty() {
        vec![pkg_name.to_string()]
    } else {
        bin_names.to_vec()
    };

    // In merge mode the target app must already exist; the alias dir must not.
    // In fresh mode the alias dir must not exist.
    let target_manifest = if merge_mode {
        if alias_name == target_name {
            bail!("--app-name cannot match --into target ('{target_name}'); pick a different alias name");
        }
        if alias_dir.join(".manifest.toml").exists() {
            bail!(
                "'{alias_name}' is already installed at ~/.wryayer/{alias_name}/. Remove it first or pass --app-name."
            );
        }
        Some(
            read_manifest(&target_name)
                .with_context(|| format!("--into target '{target_name}' is not installed"))?,
        )
    } else {
        if alias_dir.join(".manifest.toml").exists() {
            eprintln!(
                "'{alias_name}' is already installed. Use `wryayer update {alias_name}` to update, or pass --into {alias_name} to merge."
            );
            return Ok(());
        }
        None
    };

    eprintln!("Resolving dependencies for {pkg_name}...");
    let mut resolved = resolve_full_dep_tree(pkg_name)?;

    // In merge mode skip packages that are already present in the target.
    if let Some(m) = &target_manifest {
        let already: std::collections::HashSet<&str> =
            m.packages.iter().map(|p| p.name.as_str()).collect();
        resolved.retain(|p| !already.contains(p.name.as_str()));
    }

    // In fresh mode a totally-resolved-away result means the headline pkg is
    // bogus — bail. In merge mode it's fine: the binary is already present in
    // the target's tree and we still want to create the alias + launcher.
    if resolved.is_empty() && !merge_mode {
        eprintln!("Nothing to install — all packages already present.");
        return Ok(());
    }

    if !resolved.is_empty() {
        eprintln!(
            "Will install {} package(s): {}",
            resolved.len(),
            resolved
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        eprintln!("All dependencies already present in {target_name}.");
    }

    let home = std::env::var("HOME").context("HOME not set")?;
    let cache_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("pkg");
    let build_dir = PathBuf::from(&home).join(".cache").join("wryayer").join("build");

    for pkg in &mut resolved {
        match pkg.source {
            PackageSource::Official => {
                eprintln!("[official] {}", pkg.name);
                let path = download_official(&pkg.name, &cache_dir)
                    .with_context(|| format!("failed to download {}", pkg.name))?;
                pkg.pkg_path = Some(path);
            }
            PackageSource::Aur => {
                eprintln!("[aur]      {}", pkg.name);
                let path = build_aur(&pkg.name, &build_dir)
                    .with_context(|| format!("failed to build AUR package {}", pkg.name))?;
                pkg.pkg_path = Some(path);
            }
        }
    }

    fs::create_dir_all(&target_dir)
        .with_context(|| format!("failed to create app dir {}", target_dir.display()))?;
    if merge_mode {
        fs::create_dir_all(&alias_dir)
            .with_context(|| format!("failed to create alias dir {}", alias_dir.display()))?;
    }

    let mut created_launchers: Vec<String> = vec![];
    let target_name_owned = target_name.clone();
    let alias_name_owned = alias_name.clone();
    let bin_names_for_closure = bin_names.clone();
    let target_manifest_for_closure = target_manifest.clone();

    let result: Result<()> = (|| {
        for pkg in &resolved {
            let pkg_path = pkg.pkg_path.as_ref().unwrap();
            eprintln!("Extracting {}...", pkg.name);
            extract_package(pkg_path, &target_dir)
                .with_context(|| format!("failed to extract {}", pkg.name))?;
        }

        let schemas_dir = target_dir.join("usr/share/glib-2.0/schemas");
        if schemas_dir.exists() {
            let _ = std::process::Command::new("glib-compile-schemas")
                .arg(&schemas_dir)
                .status();
        }

        eprintln!("Checking for missing shared library dependencies...");
        match satisfy_missing_sonames(&target_dir, &cache_dir) {
            Ok(extra) if !extra.is_empty() => eprintln!("  Added: {}", extra.join(", ")),
            Ok(_) => {}
            Err(e) => eprintln!("  Warning: soname check failed: {e:#}"),
        }

        ensure_base_layout(&target_dir)
            .with_context(|| "failed to create base filesystem symlinks")?;

        let fixed = ensure_owner_readable(&target_dir)
            .with_context(|| "failed to fix file permissions")?;
        if fixed > 0 {
            eprintln!("Made {fixed} file(s) owner-readable (setuid helpers lose the suid bit during user-mode extract).");
        }

        eprintln!("Building library cache...");
        run_ldconfig(&target_dir);

        // Verify each requested launcher actually maps to a real binary in the
        // target's tree (file location is the same for fresh and merge modes).
        for bin in &bin_names_for_closure {
            let binary = target_dir.join("usr/bin").join(bin);
            if !binary.exists() {
                bail!(
                    "binary '{bin}' not found at {} after install — check --bin-names",
                    binary.display()
                );
            }
        }

        // Launchers always point at target_name (where the binary lives), even
        // for aliases — the launcher script bind-mounts the target's tree.
        for bin in &bin_names_for_closure {
            if created_launchers.contains(bin) {
                continue;
            }
            let launcher_path = create_launcher(&target_name_owned, bin)
                .with_context(|| format!("failed to create launcher for {bin}"))?;
            created_launchers.push(bin.to_string());
            eprintln!("Created launcher: {}", launcher_path.display());
        }

        let new_packages: Vec<PackageEntry> = resolved
            .iter()
            .map(|p| PackageEntry {
                name: p.name.clone(),
                version: p.version.clone(),
                source: p.source.clone(),
            })
            .collect();

        if merge_mode {
            // ── Update target's manifest: append new packages so dedup/health
            //    checks see the full set of files in its tree. Launchers stay
            //    on the alias — the target manifest only tracks its own.
            if let Some(t) = &target_manifest_for_closure {
                let mut updated = t.clone();
                updated.packages.extend(new_packages.clone());
                write_manifest(&target_name_owned, &updated)?;
            }

            // ── Write the alias manifest (its own dir, points at target).
            let alias_manifest = Manifest {
                app: AppMeta {
                    name: alias_name_owned.clone(),
                    main_binary: bin_names_for_closure[0].clone(),
                    installed_at: now_rfc3339(),
                    launchers: bin_names_for_closure.clone(),
                    alias_of: Some(target_name_owned.clone()),
                },
                packages: new_packages,
            };
            write_manifest(&alias_name_owned, &alias_manifest)?;
        } else {
            // Fresh install: single manifest at alias_name (= target_name).
            let manifest = Manifest {
                app: AppMeta {
                    name: alias_name_owned.clone(),
                    main_binary: bin_names_for_closure[0].clone(),
                    installed_at: now_rfc3339(),
                    launchers: created_launchers.clone(),
                    alias_of: None,
                },
                packages: new_packages,
            };
            write_manifest(&alias_name_owned, &manifest)?;
        }
        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("Installation failed, cleaning up...");
        for bin in &created_launchers {
            let _ = remove_launcher(bin);
        }
        if merge_mode {
            // Leave target alone; only undo the alias dir.
            let _ = fs::remove_dir_all(&alias_dir);
        } else {
            let _ = fs::remove_dir_all(&target_dir);
        }
        return Err(e);
    }

    if merge_mode {
        eprintln!("\nMerged '{pkg_name}' into ~/.wryayer/{target_name}/");
        eprintln!("Alias manifest: ~/.wryayer/{alias_name}/.manifest.toml");
    } else {
        eprintln!("\nInstalled '{alias_name}' to ~/.wryayer/{alias_name}/");
    }
    eprintln!(
        "Run with: {} or  wryayer run {alias_name}",
        bin_names
            .iter()
            .map(|b| format!("~/bin/{b}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    if let Err(e) = super::dedup::run(false) {
        eprintln!("warning: dedup failed: {e:#}");
    }

    Ok(())
}

/// Recreate the well-known top-level symlinks that the `filesystem` package
/// provides on every Arch system. Without these, dynamically-linked binaries
/// fail with `execvp: No such file or directory` because their PT_INTERP
/// points to `/lib64/ld-linux-x86-64.so.2` while the actual loader sits at
/// `/usr/lib/ld-linux-x86-64.so.2`. `filesystem` is only pulled in transitively
/// when another package depends on it, so apps like `hyfetch` (which declares
/// no deps) end up missing these symlinks entirely.
pub fn ensure_base_layout(app_dir: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let links: &[(&str, &str)] = &[
        ("lib",      "usr/lib"),
        ("lib64",    "usr/lib"),
        ("bin",      "usr/bin"),
        ("sbin",     "usr/bin"),
        ("usr/sbin", "bin"),
    ];

    for (path, target) in links {
        let full = app_dir.join(path);
        if full.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        symlink(target, &full)
            .with_context(|| format!("failed to symlink {} -> {target}", full.display()))?;
    }
    Ok(())
}

/// Pacman packages occasionally ship files with restrictive modes that assume
/// a setuid-root install (e.g. dbus-daemon-launch-helper at `---x--x---`).
/// When we extract as a regular user, the setuid bit is dropped but the
/// restrictive mode stays, leaving us with files we own but can't read.
/// Walk the tree and add `u+r` to any regular file the user owns where it's
/// missing. Returns the number of files modified.
pub fn ensure_owner_readable(app_dir: &Path) -> Result<u64> {
    use std::collections::VecDeque;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let my_uid = unsafe { libc::geteuid() };
    let mut fixed = 0u64;
    let mut queue: VecDeque<std::path::PathBuf> = VecDeque::new();
    queue.push_back(app_dir.to_path_buf());

    while let Some(dir) = queue.pop_front() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                // Owner-traverse bit too — otherwise we can't walk in.
                if let Ok(meta) = entry.metadata() {
                    if meta.uid() == my_uid && (meta.mode() & 0o500) != 0o500 {
                        let mut perms = meta.permissions();
                        perms.set_mode(meta.mode() | 0o500);
                        let _ = fs::set_permissions(&path, perms);
                    }
                }
                queue.push_back(path);
                continue;
            }
            if !ft.is_file() { continue; }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.uid() != my_uid { continue; }
            let mode = meta.mode();
            if mode & 0o400 == 0 {
                let mut perms = meta.permissions();
                perms.set_mode(mode | 0o400);
                if fs::set_permissions(&path, perms).is_ok() {
                    fixed += 1;
                }
            }
        }
    }
    Ok(fixed)
}

pub fn run_ldconfig(app_dir: &Path) {
    match std::process::Command::new("ldconfig")
        .arg("-r")
        .arg(app_dir)
        .status()
    {
        Ok(s) if !s.success() => eprintln!("  warning: ldconfig exited with {s}"),
        Err(_) => eprintln!("  warning: ldconfig not found, skipping cache build"),
        _ => {}
    }
}
