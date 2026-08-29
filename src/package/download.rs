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

    ensure_build_deps(&clone_dir, pkg_name)?;

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

/// Install whatever a source build needs on the *host* before makepkg runs.
///
/// Both PKGBUILD arrays matter here, and for the same reason. `makedepends` is
/// the obvious one — cmake, ninja, a compiler. But `depends` is needed too: a
/// C++ package configures and links against its libraries\' headers, so a build
/// tree with none of them present fails at configure with a message about a
/// CMake package it cannot find, which reads like a broken PKGBUILD rather than
/// a missing library. `makepkg -s` installs both for exactly this reason;
/// wryayer passes `-d` and does it here instead, so it can say what it is
/// about to put on the host and ask first.
///
/// Only what `pacman -T` reports missing is touched, so a machine that already
/// has the libraries installs nothing.
///
/// A pacman that runs and fails is non-fatal — the function warns and returns
/// Ok, because the build may well cope without one of them. Having no way to
/// *reach* root is different, and stops the build here: a front-end child
/// cannot be asked for a password, so the alternative is compiling for several
/// minutes and then failing at configure time for a reason nothing in the log
/// connects back to a missing package. Instead it prints the
/// `PROMPT_BUILD_DEPS` marker, which the front-end turns into "authenticate and
/// retry".
fn ensure_build_deps(clone_dir: &Path, pkg_name: &str) -> Result<()> {
    let declared = pkgbuild_arrays(clone_dir, &["makedepends", "depends"]);
    if declared.is_empty() {
        // Could not parse the PKGBUILD, or it asks for nothing. Either way,
        // let makepkg report whatever the real problem turns out to be.
        return Ok(());
    }

    let missing = missing_on_host(&declared)?;
    if missing.is_empty() {
        return Ok(());
    }

    eprintln!("  Installing build dependencies for {pkg_name}: {}", missing.join(", "));

    // Ask before spending the build time, not after.
    if !crate::prompt::allowed() && !crate::veracrypt::sudo_is_primed() {
        println!("PROMPT_BUILD_DEPS:{pkg_name}:{}", missing.join(","));
        bail!(
            "'{pkg_name}' is built from source, and the build needs these packages \
             installed on the host first: {}\n\
             Installing them needs root, and this ran with no terminal to ask for a \
             password on.",
            missing.join(", ")
        );
    }

    let status = crate::prompt::sudo()
        .args(["pacman", "-S", "--needed", "--noconfirm"])
        .args(&missing)
        .status()
        .context("failed to run sudo pacman")?;

    if !status.success() {
        eprintln!(
            "  warning: could not install build dependencies ({}); build may fail",
            missing.join(", ")
        );
    }

    Ok(())
}

/// Source the PKGBUILD and read the named bash arrays out of it.
///
/// Version constraints are stripped, duplicates dropped, order kept — the
/// combined list is shown to the user, so it should read like the PKGBUILD.
fn pkgbuild_arrays(clone_dir: &Path, arrays: &[&str]) -> Vec<String> {
    let script = arrays
        .iter()
        .map(|name| format!("printf '%s\\n' \"${{{name}[@]}}\""))
        .collect::<Vec<_>>()
        .join("; ");
    let out = Command::new("bash")
        .args(["-c", &format!("source PKGBUILD && {{ {script}; }}")])
        .current_dir(clone_dir)
        .output();

    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }

    let mut seen = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let name = crate::package::deps::strip_version_constraint(line.trim());
        if !name.is_empty() && !seen.iter().any(|s| s == name) {
            seen.push(name.to_string());
        }
    }
    seen
}

/// Which of `packages` are not installed, according to `pacman -T`.
///
/// `pacman -T` exits 127 and prints each unsatisfied dependency; exit 0 means
/// they are all present.
fn missing_on_host(packages: &[String]) -> Result<Vec<String>> {
    let check = Command::new("pacman")
        .arg("-T")
        .args(packages)
        .output()
        .context("failed to run pacman -T")?;

    if check.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&check.stdout)
        .lines()
        .map(|l| crate::package::deps::strip_version_constraint(l.trim()).to_string())
        .filter(|l| !l.is_empty())
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a PKGBUILD into a scratch directory and read its arrays back.
    fn arrays_of(pkgbuild: &str, arrays: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().expect("a scratch directory");
        fs::write(dir.path().join("PKGBUILD"), pkgbuild).expect("PKGBUILD writes");
        pkgbuild_arrays(dir.path(), arrays)
    }

    #[test]
    fn both_arrays_are_read_and_comments_dropped() {
        // A source build needs the libraries in `depends` as much as the tools
        // in `makedepends`: it configures and links against their headers.
        let got = arrays_of(
            "makedepends=(\n  cmake\n  ninja # the generator\n)\ndepends=(\n  ada\n  openal\n)\n",
            &["makedepends", "depends"],
        );
        assert_eq!(got, ["cmake", "ninja", "ada", "openal"]);
    }

    #[test]
    fn version_constraints_are_stripped() {
        // `pacman -T` and `pacman -S` both want the bare name.
        let got = arrays_of("depends=(\n  'boost>=1.80'\n  qt6-base=6.7.0\n)\n", &["depends"]);
        assert_eq!(got, ["boost", "qt6-base"]);
    }

    #[test]
    fn a_package_named_in_both_arrays_is_listed_once() {
        let got = arrays_of(
            "makedepends=(git ninja)\ndepends=(git ada)\n",
            &["makedepends", "depends"],
        );
        assert_eq!(got, ["git", "ninja", "ada"]);
    }

    #[test]
    fn a_pkgbuild_declaring_neither_yields_nothing() {
        // Not an error: makepkg is left to report whatever the real problem is.
        assert!(arrays_of("pkgname=foo\npkgver=1\n", &["makedepends", "depends"]).is_empty());
    }

    #[test]
    fn a_pkgbuild_that_will_not_source_yields_nothing() {
        assert!(arrays_of("depends=(\n  unterminated\n", &["makedepends", "depends"]).is_empty());
    }
}
