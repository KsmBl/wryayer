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
    //
    // In merge mode, follow one hop of the alias chain: if the user passes
    // --into fastfetch and fastfetch is itself an alias of cpufetch, the real
    // filesystem tree is cpufetch's. Extracting into the alias dir would leave
    // the binary in a near-empty directory without the shared library tree.
    // alias_name: always the folder the manifest lives in (= user's chosen app name, or pkg name)
    let alias_name = app_name.unwrap_or(pkg_name).to_string();
    // target_name: where the package files actually live on disk.
    //   fresh mode → same as alias_name (must match per the comment above)
    //   merge mode → follow the --into chain to find the real filesystem root
    let target_name = if let Some(into_name) = into {
        let resolved = read_manifest(into_name)
            .ok()
            .and_then(|m| m.app.alias_of)
            .unwrap_or_else(|| into_name.to_string());
        resolved
    } else {
        alias_name.clone()
    };
    let target_dir = app_dir(&target_name)?;
    let alias_dir = app_dir(&alias_name)?;

    // Multi-launcher: if user passed --bin-names, use that list verbatim;
    // otherwise create a single launcher named after the package.
    let bin_names_explicit = !bin_names.is_empty();
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
    let mut keep_without_launcher = false;
    let target_name_owned = target_name.clone();
    let alias_name_owned = alias_name.clone();
    let bin_names_for_closure = bin_names.clone();
    let bin_names_explicit_for_closure = bin_names_explicit;
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
        //
        // Use symlink_metadata (lstat) rather than exists (stat) so that
        // symlinks with absolute targets (e.g. /usr/bin/vivaldi-stable ->
        // /opt/vivaldi/vivaldi) are found correctly: exists() follows the link
        // and resolves the absolute path against the HOST filesystem where the
        // target doesn't live, returning false even though the link is present.
        // Inside the bwrap sandbox the absolute path resolves correctly because
        // the app dir is mounted as /.
        let bin_dirs = ["usr/bin", "usr/sbin", "bin", "sbin"];
        let exists_in_bins = |name: &str| {
            bin_dirs.iter().any(|sub| target_dir.join(sub).join(name).symlink_metadata().is_ok())
        };

        let mut resolved_bin_names: Vec<String> = Vec::with_capacity(bin_names_for_closure.len());
        for bin in &bin_names_for_closure {
            if exists_in_bins(bin) {
                resolved_bin_names.push(bin.clone());
                continue;
            }

            // When --bin-names was not given, try to auto-detect the real binary
            // from the package's .desktop file before giving up. Many AUR packages
            // (e.g. visual-studio-code-bin → code, google-chrome-stable → google-chrome)
            // install under a name that differs from the AUR package name.
            if !bin_names_explicit_for_closure {
                if let Some(detected) = auto_detect_binary(&target_dir) {
                    eprintln!(
                        "  auto-detected binary '{detected}' \
                         (package does not install a binary named '{bin}')"
                    );
                    resolved_bin_names.push(detected);
                    continue;
                }
            }

            // Collect what IS in the binary dirs to help the user pick the
            // right name for --bin-names.
            let mut available: Vec<String> = bin_dirs
                .iter()
                .flat_map(|sub| {
                    std::fs::read_dir(target_dir.join(sub))
                        .ok()
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                })
                .collect();
            available.sort();
            available.dedup();

            if !bin_names_explicit_for_closure
                && prompt_keep_without_launcher(&alias_name_owned, &available)
            {
                keep_without_launcher = true;
                break;
            }

            let hint = if available.is_empty() {
                String::new()
            } else {
                format!("\n  available binaries: {}", available.join(", "))
            };
            bail!(
                "binary '{bin}' not found in usr/bin, usr/sbin, bin, or sbin — \
                 re-run with --bin-names <name>{hint}"
            );
        }

        // In merge mode the launcher must use alias_name so it calls
        // `wryayer run <alias>`, which reads the alias manifest and follows
        // alias_of to find the right filesystem tree and main_binary.
        // Using target_name here would call `wryayer run cpufetch` for every
        // merged binary regardless of which binary was actually requested.
        if !keep_without_launcher {
            let launcher_app = if merge_mode { &alias_name_owned } else { &target_name_owned };
            for bin in &resolved_bin_names {
                if created_launchers.contains(bin) {
                    continue;
                }
                let launcher_path = create_launcher(launcher_app, bin)
                    .with_context(|| format!("failed to create launcher for {bin}"))?;
                created_launchers.push(bin.to_string());
                eprintln!("Created launcher: {}", launcher_path.display());
            }
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
                    main_binary: resolved_bin_names.first().cloned().unwrap_or_default(),
                    installed_at: now_rfc3339(),
                    launchers: resolved_bin_names.clone(),
                    alias_of: Some(target_name_owned.clone()),
                    display_name: None,
                },
                packages: new_packages,
            };
            write_manifest(&alias_name_owned, &alias_manifest)?;
        } else {
            // Fresh install: single manifest at alias_name (= target_name).
            let manifest = Manifest {
                app: AppMeta {
                    name: alias_name_owned.clone(),
                    main_binary: resolved_bin_names.first().cloned().unwrap_or_default(),
                    installed_at: now_rfc3339(),
                    launchers: created_launchers.clone(),
                    alias_of: None,
                    display_name: None,
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
    if created_launchers.is_empty() {
        eprintln!("No launcher created — files are at ~/.wryayer/{alias_name}/");
        eprintln!("To add a launcher later, re-install with --bin-names <name>.");
    } else {
        eprintln!(
            "Run with: {}  or  wryayer run {alias_name}",
            created_launchers
                .iter()
                .map(|b| format!("~/bin/{b}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

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

    // Create a per-app home directory so browsers and GUI apps can store their
    // profiles across launches.  The bwrap sandbox binds app_dir as its root,
    // so app_dir/home/<username>/ is visible as /home/<username>/ inside —
    // matching the inherited $HOME env var without touching the real home.
    if let Ok(home_val) = std::env::var("HOME") {
        let username = home_val.trim_end_matches('/').rsplit('/').next().unwrap_or("user");
        let sandbox_home = app_dir.join("home").join(username);
        if !sandbox_home.exists() {
            fs::create_dir_all(&sandbox_home)
                .with_context(|| format!("failed to create sandbox home {}", sandbox_home.display()))?;
        }
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

/// When no binary could be found or auto-detected, prompt the user (if stdin is
/// a terminal) to choose between keeping the installed files without a launcher
/// or cleaning everything up. Returns true if the user wants to keep.
fn prompt_keep_without_launcher(pkg_name: &str, available: &[String]) -> bool {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};

    if !std::io::stdin().is_terminal() {
        return false;
    }

    eprintln!();
    eprintln!("No launcher binary found for '{pkg_name}'.");
    if available.is_empty() {
        eprintln!("  No executables found in usr/bin, usr/sbin, bin, or sbin.");
        eprintln!("  This package may install data/library files only (e.g. coreutils).");
    } else {
        eprintln!("  Available binaries: {}", available.join(", "));
        eprintln!("  Re-install with --bin-names <name> to create a launcher for one of them.");
    }
    eprintln!();
    eprintln!("  k  Keep installed files without a launcher");
    eprintln!("  c  Clean up (remove everything)");
    eprint!("Choice [k/c]: ");
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "k" | "keep")
}

/// Scan the app's .desktop files for an Exec= entry whose basename exists in
/// the binary dirs. Used when the package name doesn't match the installed
/// binary name (e.g. visual-studio-code-bin installs as `code`).
fn auto_detect_binary(target_dir: &Path) -> Option<String> {
    let bin_dirs = ["usr/bin", "usr/sbin", "bin", "sbin"];
    let exists_in_bins = |name: &str| {
        bin_dirs.iter().any(|sub| target_dir.join(sub).join(name).symlink_metadata().is_ok())
    };

    let apps_dir = target_dir.join("usr/share/applications");
    let Ok(entries) = std::fs::read_dir(&apps_dir) else { return None };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        for line in content.lines() {
            let line = line.trim();
            if !line.starts_with("Exec=") {
                continue;
            }
            // Exec=/usr/share/code/code --unity-launch %F  →  basename = "code"
            let exec_val = line.trim_start_matches("Exec=");
            let cmd = exec_val.split_whitespace().next().unwrap_or("");
            let bin_name = std::path::Path::new(cmd)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if bin_name.is_empty() || bin_name.starts_with('%') {
                continue;
            }
            if exists_in_bins(bin_name) {
                return Some(bin_name.to_string());
            }
        }
    }
    None
}
