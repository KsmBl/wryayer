use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn download_official(pkg_name: &str, cache_dir: &Path) -> Result<PathBuf> {
    crate::distro::download_pkg(pkg_name, cache_dir)
        .with_context(|| format!("failed to download '{pkg_name}'"))
}

pub fn build_aur(pkg_name: &str, build_dir: &Path) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let yay_cache = PathBuf::from(&home)
        .join(".cache")
        .join("yay")
        .join(pkg_name);

    if yay_cache.exists() {
        if let Some(cached) = find_pkg_tarball(&yay_cache)? {
            eprintln!("  Using cached AUR package for {pkg_name}");
            return Ok(cached);
        }
    }

    fs::create_dir_all(build_dir)
        .with_context(|| format!("failed to create build dir {}", build_dir.display()))?;

    let clone_dir = build_dir.join(pkg_name);
    let aur_url = format!("https://aur.archlinux.org/{pkg_name}.git");

    if clone_dir.exists() {
        eprintln!("  Pulling {pkg_name} from AUR...");
        let status = Command::new("git")
            .args(["pull"])
            .current_dir(&clone_dir)
            .status()
            .context("failed to run git pull")?;
        if !status.success() {
            bail!("git pull failed for {pkg_name}");
        }
    } else {
        eprintln!("  Cloning {pkg_name} from AUR...");
        let status = Command::new("git")
            .args(["clone", &aur_url, clone_dir.to_str().unwrap()])
            .status()
            .context("failed to run git clone")?;
        if !status.success() {
            bail!("git clone failed for {aur_url}");
        }
    }

    eprintln!("  Building {pkg_name} with makepkg...");
    let output = Command::new("makepkg")
        .args(["-d", "--noconfirm", "--noprogressbar", "--skippgpcheck"])
        .current_dir(&clone_dir)
        .output()
        .context("failed to run makepkg")?;

    if !output.status.success() {
        bail!(
            "makepkg failed for {pkg_name}:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    find_pkg_tarball(&clone_dir)?
        .with_context(|| format!("no .pkg.tar.zst found after building {pkg_name}"))
}

fn find_pkg_tarball(dir: &Path) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(dir)
        .with_context(|| format!("failed to read dir {}", dir.display()))?
    {
        let entry = entry.context("failed to read directory entry")?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.ends_with(".pkg.tar.zst") && !name.ends_with("-debug.pkg.tar.zst") {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}
