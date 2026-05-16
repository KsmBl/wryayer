use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn download_official(pkg_name: &str, cache_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create cache dir {}", cache_dir.display()))?;

    // Ask pacman for the download URL.
    // -dd skips all dep checks so the output is exactly one URL: the package itself.
    // Without -dd, uninstalled deps would also appear in the output.
    let output = Command::new("pacman")
        .args(["-Spdd", "--noconfirm", pkg_name])
        .output()
        .context("failed to spawn pacman -Spdd")?;

    if !output.status.success() {
        bail!(
            "pacman cannot find '{pkg_name}':\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // pacman may emit warnings/notices to stdout — filter for actual URL lines only
    let url = stdout
        .lines()
        .find(|l| {
            let t = l.trim();
            t.starts_with("https://") || t.starts_with("http://") || t.starts_with("file://")
        })
        .with_context(|| {
            format!(
                "pacman produced no download URL for '{pkg_name}' \
                 (stdout: {})",
                stdout.trim()
            )
        })?
        .trim();

    // Local file path (package already in pacman's cache)
    if url.starts_with("file://") {
        let path = PathBuf::from(url.trim_start_matches("file://"));
        if !path.exists() {
            bail!("local cache file missing: {}", path.display());
        }
        return Ok(path);
    }

    let filename = url
        .rsplit('/')
        .next()
        .with_context(|| format!("cannot parse filename from URL: {url}"))?;

    // Check wryayer's own download cache
    let dest = cache_dir.join(filename);
    if dest.exists() {
        return Ok(dest);
    }

    eprintln!("  Downloading {pkg_name}...");
    download_url(url, &dest).with_context(|| {
        format!(
            "failed to download '{pkg_name}'\n  \
             If you see a 404 error, your package databases are out of date.\n  \
             Fix with: sudo pacman -Sy"
        )
    })?;
    Ok(dest)
}

fn download_url(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::new();
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("HTTP request failed for {url}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "mirror returned 404 for {url}\n  \
             Your package databases are out of date — run: sudo pacman -Sy"
        );
    }
    if !response.status().is_success() {
        bail!("server returned {} for {url}", response.status());
    }

    let content_length = response.content_length().unwrap_or(0);
    let pb = ProgressBar::new(content_length);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  [{bar:40}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );

    let mut file =
        fs::File::create(dest).with_context(|| format!("failed to create {}", dest.display()))?;

    let mut buf = [0u8; 65536];
    loop {
        use std::io::Read;
        let n = response
            .read(&mut buf)
            .context("failed to read response body")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("failed to write downloaded data")?;
        pb.inc(n as u64);
    }
    pb.finish_and_clear();
    Ok(())
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
