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

    ensure_makedepends(&clone_dir, pkg_name)?;

    eprintln!("  Building {pkg_name} with makepkg...");
    let output = run_makepkg(&clone_dir, false)?;

    if !output.status.success() {
        // Combine stdout + stderr — some build tools (yarn, gulp) write errors
        // to stdout; makepkg itself writes to stderr.
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        );

        // nw-builder 3.8.3 sets `rq.proxy = true` on its request object which
        // forces a proxy tunnel even when no proxy is configured, causing
        // ECONNRESET / "Unable to download NWjs".  Patch the broken line and
        // retry with --noextract so the existing node_modules (and our fix)
        // survive the second makepkg run.
        if combined.contains("Unable to download NWjs")
            || combined.contains("tunneling socket could not be established")
            || combined.contains("Error building NW apps")
        {
            eprintln!("  Detected nw-builder proxy bug — patching and retrying...");
            patch_nw_builder_downloader(&clone_dir);
            let retry = run_makepkg(&clone_dir, true)?;
            if !retry.status.success() {
                bail!(
                    "makepkg failed for {pkg_name} (after nw-builder patch):\n{}\n{}",
                    String::from_utf8_lossy(&retry.stderr),
                    String::from_utf8_lossy(&retry.stdout),
                );
            }
        } else {
            bail!("makepkg failed for {pkg_name}:\n{combined}");
        }
    }

    find_pkg_tarball(&clone_dir)?
        .with_context(|| format!("no .pkg.tar.zst found after building {pkg_name}"))
}

/// Source the PKGBUILD in the given directory, extract its `makedepends`
/// array, find which are missing on the host with `pacman -T`, and install
/// them via `sudo pacman -S --needed --noconfirm` before makepkg runs.
/// Failures are non-fatal — the function warns and returns Ok so the build
/// can still succeed if the PKGBUILD itself handles the absence gracefully.
fn ensure_makedepends(clone_dir: &Path, pkg_name: &str) -> Result<()> {
    // Source the PKGBUILD and print every makedepends entry, one per line.
    let out = Command::new("bash")
        .args(["-c", "source PKGBUILD && printf '%s\\n' \"${makedepends[@]}\""])
        .current_dir(clone_dir)
        .output();

    let makedepends: Vec<String> = match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| crate::package::deps::strip_version_constraint(l).to_string())
                .collect()
        }
        _ => return Ok(()), // can't parse PKGBUILD — skip, let makepkg report the real error
    };

    if makedepends.is_empty() {
        return Ok(());
    }

    // `pacman -T` exits 127 and prints each unsatisfied dep; exit 0 = all present.
    let check = Command::new("pacman")
        .arg("-T")
        .args(&makedepends)
        .output()
        .context("failed to run pacman -T")?;

    if check.status.success() {
        return Ok(()); // all satisfied
    }

    let missing: Vec<String> = String::from_utf8_lossy(&check.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| crate::package::deps::strip_version_constraint(l.trim()).to_string())
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    eprintln!(
        "  Installing makedepends for {pkg_name}: {}",
        missing.join(", ")
    );

    let status = Command::new("sudo")
        .args(["pacman", "-S", "--needed", "--noconfirm"])
        .args(&missing)
        .status()
        .context("failed to run sudo pacman")?;

    if !status.success() {
        eprintln!(
            "  warning: could not install makedepends ({}); build may fail",
            missing.join(", ")
        );
    }

    Ok(())
}

fn run_makepkg(clone_dir: &Path, noextract: bool) -> Result<std::process::Output> {
    // `-f` overwrites any package left over from a previous build in this same
    // clone dir; without it makepkg aborts with "A package has already been
    // built" whenever we rebuild (e.g. on update or after a partial run).
    let mut args = vec!["-df", "--noconfirm", "--noprogressbar", "--skippgpcheck"];
    if noextract {
        // --noextract skips re-cloning/resetting the source tree and skips
        // prepare(), so any patches we applied to node_modules survive.
        args.push("--noextract");
    }
    Command::new("makepkg")
        .args(&args)
        .current_dir(clone_dir)
        .output()
        .context("failed to run makepkg")
}

/// Walk `build_root/src/` looking for every `nw-builder/lib/downloader.cjs`
/// and replace the broken `rq.proxy = true` with `rq.proxy = false`.
/// nw-builder 3.8.3 sets proxy=true unconditionally, causing it to try to
/// open a tunnelled connection even when no proxy exists — ECONNRESET ensues.
fn patch_nw_builder_downloader(build_root: &Path) {
    let src_dir = build_root.join("src");
    let Ok(walker) = fs::read_dir(&src_dir) else { return };
    for project_dir in walker.flatten() {
        let target = project_dir.path()
            .join("node_modules")
            .join("nw-builder")
            .join("lib")
            .join("downloader.cjs");
        if !target.exists() {
            continue;
        }
        let Ok(content) = fs::read_to_string(&target) else { continue };
        if content.contains("rq.proxy = true") {
            let patched = content.replace("rq.proxy = true", "rq.proxy = false");
            if fs::write(&target, patched).is_ok() {
                eprintln!("  Patched nw-builder proxy bug in {}", target.display());
            }
        }
    }
}

fn find_pkg_tarball(dir: &Path) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(dir)
        .with_context(|| format!("failed to read dir {}", dir.display()))?
    {
        let entry = entry.context("failed to read directory entry")?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            // Skip the split debug package makepkg emits when a PKGBUILD builds
            // with debug symbols. It's named `<pkgbase>-debug-<ver>-<arch>.pkg.
            // tar.zst`, so the `-debug-` marker sits in the middle, not at the
            // end — matching only a trailing `-debug` misses it and we'd extract
            // the symbols-only package instead of the real one.
            if name.ends_with(".pkg.tar.zst") && !name.contains("-debug-") {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}
