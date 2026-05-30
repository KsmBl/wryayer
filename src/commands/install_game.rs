use crate::config;
use crate::launcher::create_launcher;
use crate::manifest::{
    app_dir, now_rfc3339, read_manifest, write_manifest, AppMeta, Manifest, WineGame,
};
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

pub fn run(
    game_dir: &Path,
    container: &str,
    exe_override: Option<&str>,
    app_name_override: Option<&str>,
    delete_source: bool,
    skip_size_check: bool,
) -> Result<()> {
    if !game_dir.is_dir() {
        bail!("game path is not a directory: {}", game_dir.display());
    }
    let game_dir = game_dir.canonicalize()
        .with_context(|| format!("cannot resolve {}", game_dir.display()))?;

    let container_manifest = read_manifest(container)
        .with_context(|| format!("container '{container}' is not installed"))?;
    let container_root_name = container_manifest.app.alias_of
        .clone()
        .unwrap_or_else(|| container.to_string());
    let container_dir = app_dir(&container_root_name)?;
    let wine_path = ["usr/bin/wine", "bin/wine"]
        .iter()
        .map(|sub| container_dir.join(sub))
        .find(|p| p.symlink_metadata().is_ok());
    if wine_path.is_none() {
        bail!(
            "container '{container_root_name}' does not contain a wine binary.\n\
             Install wine into it first:  wryayer install wine --into {container_root_name}"
        );
    }

    let default_name = sanitize_name(
        game_dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("game"),
    );
    let alias_name = app_name_override
        .map(|s| s.to_string())
        .unwrap_or(default_name);
    if alias_name.is_empty() {
        bail!("could not derive a valid app name from the game directory; pass --app-name");
    }
    let alias_dir = app_dir(&alias_name)?;
    if alias_dir.join(".manifest.toml").exists() {
        bail!(
            "'{alias_name}' is already installed at ~/.wryayer/{alias_name}/. \
             Remove it first or pass --app-name."
        );
    }

    let exe_rel = match exe_override {
        Some(s) => {
            let p = game_dir.join(s);
            if !p.is_file() {
                bail!("--exe path not found inside game dir: {s}");
            }
            s.to_string()
        }
        None => match detect_main_exe(&game_dir, &alias_name)? {
            ExeChoice::One(rel) => rel,
            ExeChoice::Many(candidates) => prompt_exe_choice(&candidates)
                .context("no executable selected; re-run with --exe <relative-path>")?,
            ExeChoice::None => bail!(
                "no .exe files found anywhere under {}",
                game_dir.display()
            ),
        },
    };

    // Size + free-space check (unless user opted out). Copying a 50 GiB game
    // into a small partition would fail mid-copy and leave a half-written tree.
    let game_bytes = if skip_size_check {
        0
    } else {
        dir_size(&game_dir)
    };
    if !skip_size_check {
        let free = available_bytes(&container_dir).unwrap_or(u64::MAX);
        if free < game_bytes.saturating_add(64 * 1024 * 1024) {
            bail!(
                "not enough free space on container's filesystem.\n\
                 game size: {} MiB, free: {} MiB",
                game_bytes / 1_048_576,
                free / 1_048_576,
            );
        }
    }

    let games_root = container_dir.join("games");
    let dest = games_root.join(&alias_name);
    if dest.exists() {
        bail!(
            "game directory already exists in container: ~/.wryayer/{container_root_name}/games/{alias_name}/\n\
             Remove it manually if you want to re-import."
        );
    }
    fs::create_dir_all(&games_root)
        .with_context(|| format!("failed to create {}", games_root.display()))?;

    eprintln!(
        "Copying {} ({} MiB) into ~/.wryayer/{container_root_name}/games/{alias_name}/...",
        game_dir.display(),
        game_bytes / 1_048_576,
    );
    if let Err(e) = copy_tree(&game_dir, &dest, game_bytes) {
        let _ = fs::remove_dir_all(&dest);
        return Err(e).context("failed to copy game directory");
    }

    let exe_in_container = format!("/games/{alias_name}/{}", exe_rel.trim_start_matches('/'));
    let prefix_in_container = format!("/games/{alias_name}/.wineprefix");

    fs::create_dir_all(&alias_dir)
        .with_context(|| format!("failed to create alias dir {}", alias_dir.display()))?;
    let prefix_dir_host = dest.join(".wineprefix");
    fs::create_dir_all(&prefix_dir_host)
        .with_context(|| format!("failed to create wineprefix dir {}", prefix_dir_host.display()))?;

    let manifest = Manifest {
        app: AppMeta {
            name: alias_name.clone(),
            main_binary: "wine".into(),
            installed_at: now_rfc3339(),
            launchers: vec![alias_name.clone()],
            alias_of: Some(container_root_name.clone()),
            display_name: None,
            pkg_name: None,
            wine_game: Some(WineGame {
                exe: exe_in_container,
                prefix: prefix_in_container,
            }),
        },
        packages: vec![],
    };
    write_manifest(&alias_name, &manifest)
        .with_context(|| format!("failed to write manifest for {alias_name}"))?;

    // Seed the alias's config.ini from the container so the game inherits
    // its shared_dirs, audio/network toggles, spoof settings, etc.
    if let Ok(target_cfg_path) = config::config_path(&container_root_name) {
        if target_cfg_path.exists() {
            if let Ok(cfg) = config::read_config(&container_root_name) {
                let _ = config::write_config(&alias_name, &cfg);
            }
        }
    }

    let launcher_path = create_launcher(&alias_name, &alias_name)
        .with_context(|| format!("failed to create launcher for {alias_name}"))?;
    eprintln!("Created launcher: {}", launcher_path.display());

    if delete_source {
        eprintln!("Deleting source folder {}...", game_dir.display());
        if let Err(e) = fs::remove_dir_all(&game_dir) {
            eprintln!("warning: failed to delete source: {e:#}");
        }
    }

    eprintln!(
        "\nImported '{alias_name}' into wine container '{container_root_name}'.\n\
         Run with:  ~/bin/{alias_name}  or  wryayer run {alias_name}"
    );
    Ok(())
}

enum ExeChoice {
    None,
    One(String),
    Many(Vec<(String, u64)>),
}

/// Walk the game dir for .exe files and rank them by likelihood of being the
/// main game executable. Returns a sorted list (best first).
fn detect_main_exe(game_dir: &Path, app_name: &str) -> Result<ExeChoice> {
    let mut found: Vec<(PathBuf, u64)> = Vec::new();
    collect_exes(game_dir, game_dir, &mut found, 0)?;

    let norm = |s: &str| s.to_lowercase().replace(['-', '_', '.', ' '], "");
    let app_norm = norm(app_name);

    let ranked: Vec<(String, u64, i32)> = found.into_iter()
        .map(|(p, sz)| {
            let rel = p.strip_prefix(game_dir).unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let stem = Path::new(&rel)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let stem_lc = stem.to_lowercase();
            let stem_norm = norm(&stem);

            let mut score: i32 = 0;
            // Strong negatives — installers, redistributables, helpers
            if stem_lc.starts_with("unins") { score -= 100; }
            if stem_lc.contains("setup") { score -= 60; }
            if stem_lc.contains("redist") || stem_lc.contains("vcredist") { score -= 80; }
            if stem_lc.contains("dxsetup") || stem_lc.starts_with("dx") { score -= 40; }
            if stem_lc.contains("dotnet") || stem_lc.contains("crashhandler") { score -= 40; }
            if stem_lc.contains("config") || stem_lc.contains("launcher") { score -= 10; }

            // Top-level beats deeply nested
            let depth = rel.matches('/').count() as i32;
            score -= depth * 5;

            // Bonus when the stem matches the app/folder name
            if stem_norm.contains(&app_norm) || app_norm.contains(&stem_norm) {
                score += 60;
            }

            // Bonus for large files (MiB scaled, capped)
            let mib = (sz / 1_048_576) as i32;
            score += mib.min(60);

            (rel, sz, score)
        })
        .collect();

    if ranked.is_empty() {
        return Ok(ExeChoice::None);
    }

    let mut sorted = ranked;
    sorted.sort_by(|a, b| b.2.cmp(&a.2));

    let best_score = sorted[0].2;
    let close_runners = sorted.iter()
        .take(8)
        .filter(|(_, _, s)| best_score - s < 20)
        .count();
    if close_runners <= 1 {
        return Ok(ExeChoice::One(sorted.into_iter().next().unwrap().0));
    }
    let candidates: Vec<(String, u64)> = sorted.into_iter()
        .take(10)
        .map(|(r, sz, _)| (r, sz))
        .collect();
    Ok(ExeChoice::Many(candidates))
}

fn collect_exes(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, u64)>, depth: usize) -> Result<()> {
    if depth > 6 {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            // Skip wine prefix dirs that may already exist from a previous import
            let name = entry.file_name();
            let name_lc = name.to_string_lossy().to_lowercase();
            if name_lc.starts_with('.') || name_lc == "drive_c" {
                continue;
            }
            collect_exes(root, &path, out, depth + 1)?;
        } else if ft.is_file() {
            let name = entry.file_name();
            let lc = name.to_string_lossy().to_lowercase();
            if lc.ends_with(".exe") {
                let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push((path, sz));
            }
        }
    }
    Ok(())
}

fn prompt_exe_choice(candidates: &[(String, u64)]) -> Option<String> {
    use std::io::IsTerminal as _;
    if !io::stdin().is_terminal() {
        // Non-interactive: emit machine-readable marker for the TUI/parent
        eprintln!(
            "PROMPT_GAME_EXE_CHOICE:{}",
            candidates.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join("|")
        );
        return None;
    }
    eprintln!("\nMultiple candidate .exe files found:");
    for (i, (rel, sz)) in candidates.iter().enumerate() {
        eprintln!("  {})  {rel}  ({} MiB)", i + 1, sz / 1_048_576);
    }
    eprint!("Pick one [1-{}] (or empty to cancel): ", candidates.len());
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return None;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<usize>().ok()
        .and_then(|n| if n >= 1 && n <= candidates.len() { Some(candidates[n - 1].0.clone()) } else { None })
}

fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = fs::read_dir(&p) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(md) = entry.metadata() {
                    total += md.len();
                }
            }
        }
    }
    total
}

fn available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    unsafe {
        let mut sv: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut sv) != 0 {
            return None;
        }
        Some(sv.f_bavail as u64 * sv.f_frsize as u64)
    }
}

/// Recursive copy that preserves symlinks and Unix permissions, and emits
/// `PROGRESS bytes/total` markers every 64 MiB so the TUI can render a bar.
fn copy_tree(src: &Path, dst: &Path, total: u64) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut copied: u64 = 0;
    let mut last_report: u64 = 0;
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(src.to_path_buf(), dst.to_path_buf())];

    while let Some((s, d)) = stack.pop() {
        fs::create_dir_all(&d).with_context(|| format!("mkdir {}", d.display()))?;
        if let Ok(meta) = fs::metadata(&s) {
            let _ = fs::set_permissions(&d, fs::Permissions::from_mode(meta.permissions().mode()));
        }
        for entry in fs::read_dir(&s).with_context(|| format!("read {}", s.display()))?.flatten() {
            let path = entry.path();
            let target = d.join(entry.file_name());
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                let link_target = fs::read_link(&path)
                    .with_context(|| format!("readlink {}", path.display()))?;
                let _ = fs::remove_file(&target);
                std::os::unix::fs::symlink(&link_target, &target)
                    .with_context(|| format!("symlink {}", target.display()))?;
            } else if ft.is_dir() {
                stack.push((path, target));
            } else if ft.is_file() {
                fs::copy(&path, &target)
                    .with_context(|| format!("copy {} -> {}", path.display(), target.display()))?;
                if let Ok(md) = entry.metadata() {
                    let _ = fs::set_permissions(&target, fs::Permissions::from_mode(md.permissions().mode()));
                    copied += md.len();
                }
                if total > 0 && copied - last_report >= 64 * 1024 * 1024 {
                    eprintln!("PROGRESS {copied}/{total}");
                    last_report = copied;
                }
            }
        }
    }

    if total > 0 {
        eprintln!("PROGRESS {total}/{total}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_name_drops_unsafe_chars() {
        assert_eq!(sanitize_name("NFS Underground 2"), "nfs-underground-2");
        assert_eq!(sanitize_name("Need_For-Speed.2"), "need_for-speed.2");
        assert_eq!(sanitize_name("  -- Game --  "), "game");
        assert_eq!(sanitize_name("§$%"), "");
    }

    fn touch(p: &std::path::Path, bytes: usize) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn detect_main_exe_prefers_game_over_uninstaller() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("Speed2.exe"), 5_000_000);
        touch(&root.join("unins000.exe"), 800_000);
        touch(&root.join("redist/vcredist.exe"), 4_000_000);
        match detect_main_exe(root, "nfsu2").unwrap() {
            ExeChoice::One(rel) => assert_eq!(rel, "Speed2.exe"),
            ExeChoice::Many(c) => assert_eq!(c[0].0, "Speed2.exe"),
            ExeChoice::None => panic!("expected detection"),
        }
    }

    #[test]
    fn detect_main_exe_returns_none_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        match detect_main_exe(tmp.path(), "x").unwrap() {
            ExeChoice::None => {}
            _ => panic!("expected ExeChoice::None"),
        }
    }

    #[test]
    fn detect_main_exe_bonus_when_stem_matches_app_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("launcher.exe"), 2_000_000);
        touch(&root.join("MyAwesomeGame.exe"), 2_100_000);
        match detect_main_exe(root, "myawesomegame").unwrap() {
            ExeChoice::One(rel) => assert_eq!(rel, "MyAwesomeGame.exe"),
            ExeChoice::Many(c) => assert_eq!(c[0].0, "MyAwesomeGame.exe"),
            ExeChoice::None => panic!("expected detection"),
        }
    }
}
