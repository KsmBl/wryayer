//! Distro detection and per-distro package-manager operations.
//!
//! Everything that touches pacman/apt, tar/dpkg-deb, vercmp/dpkg is routed
//! through this module so the rest of the codebase stays distro-agnostic.

use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// ── Distro detection ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    /// Arch Linux and all derivatives: CachyOS, EndeavourOS, Garuda, Manjaro, …
    /// Package manager: pacman / yay; archive format: .pkg.tar.zst
    Arch,
    /// Debian, Ubuntu, and all derivatives: Mint, Pop!_OS, Kali, …
    /// Package manager: apt / dpkg; archive format: .deb
    Debian,
    /// Fedora, RHEL, CentOS, AlmaLinux, Rocky Linux, openSUSE, …
    /// Package manager: dnf / rpm; archive format: .rpm
    Fedora,
}

static CURRENT: OnceLock<Distro> = OnceLock::new();

pub fn current() -> Distro {
    *CURRENT.get_or_init(detect)
}

fn detect() -> Distro {
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        let mut id = String::new();
        let mut id_like = String::new();
        for line in content.lines() {
            if let Some(v) = line.strip_prefix("ID=") {
                id = v.trim_matches('"').to_lowercase();
            } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
                id_like = v.trim_matches('"').to_lowercase();
            }
        }
        // ID_LIKE wins over ID for derivatives (e.g. CachyOS: ID=cachyos ID_LIKE=arch)
        let combined = format!("{id_like} {id}");
        if combined.contains("debian") || combined.contains("ubuntu") {
            return Distro::Debian;
        }
        if combined.contains("arch") || combined.contains("manjaro") {
            return Distro::Arch;
        }
        if combined.contains("fedora") || combined.contains("rhel")
            || combined.contains("centos") || combined.contains("suse")
            || matches!(id.as_str(), "almalinux" | "rocky" | "fedora" | "rhel" | "centos")
        {
            return Distro::Fedora;
        }
    }
    // Fallback: check for package manager binaries
    if Path::new("/usr/bin/pacman").exists() {
        return Distro::Arch;
    }
    if Path::new("/usr/bin/apt-get").exists() {
        return Distro::Debian;
    }
    if Path::new("/usr/bin/dnf").exists() || Path::new("/usr/bin/dnf5").exists() {
        return Distro::Fedora;
    }
    Distro::Arch
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Direct package-info lookup — returns (version, deps) or None if not found.
/// Does not attempt virtual/provider resolution; call resolve_virtual for that.
pub fn query_pkg_info(pkg: &str) -> Result<Option<(String, Vec<String>)>> {
    match current() {
        Distro::Arch   => arch::query_info(pkg),
        Distro::Debian => debian::query_info(pkg),
        Distro::Fedora => fedora::query_info(pkg),
    }
}

/// Resolve a virtual/meta dependency to the name of a concrete package that
/// provides it. Returns None if no provider is found.
pub fn resolve_virtual(dep: &str) -> Result<Option<String>> {
    match current() {
        Distro::Arch   => arch::resolve_virtual(dep),
        Distro::Debian => debian::resolve_virtual(dep),
        Distro::Fedora => fedora::resolve_virtual(dep),
    }
}

/// Find which package installed on the *host* system provides the given soname.
/// Strips any `=VERSION` suffix before searching (e.g. "libreadline.so=8" → "libreadline.so").
///
/// Results are cached for the lifetime of the process. soname_owner is the
/// hot path during install — both positive and negative results spawn up to
/// 8 pacman/apt/dnf subprocesses each, and a single wine install would
/// otherwise repeat the same lookups across satisfy-loop iterations.
pub fn soname_owner(soname: &str) -> Result<Option<String>> {
    let filename = soname.split('=').next().unwrap_or(soname).to_string();
    if let Some(cached) = soname_cache_get(&filename) {
        return Ok(cached);
    }
    let result = match current() {
        Distro::Arch   => arch::soname_owner(&filename),
        Distro::Debian => debian::soname_owner(&filename),
        Distro::Fedora => fedora::soname_owner(&filename),
    }?;
    soname_cache_put(filename, result.clone());
    Ok(result)
}

fn soname_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<String>>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Option<String>>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn soname_cache_get(name: &str) -> Option<Option<String>> {
    soname_cache().lock().ok()?.get(name).cloned()
}

fn soname_cache_put(name: String, value: Option<String>) {
    if let Ok(mut g) = soname_cache().lock() {
        g.insert(name, value);
    }
}

/// Download a package archive to cache_dir and return its path.
///
/// Every backend authenticates the archive **before** it can be extracted, so a
/// tampered or MITM'd mirror can't drop arbitrary files into a sandbox root:
/// - **Arch** verifies the detached PGP `.sig` against the pacman keyring (`gpg`).
/// - **Fedora** verifies the `.rpm`'s signature against the rpm keyring (`rpmkeys`).
/// - **Debian** relies on apt's authenticated acquire (hashes tied to the signed
///   Release file) and rejects any package apt reports as unauthenticated.
///
/// AUR packages are the exception: they're built locally by `makepkg`, so there
/// is no repo signature to check (same trust model as running `yay`). Set
/// `WRYAYER_SKIP_SIG_VERIFY=1` to bypass verification for a private/unsigned repo.
pub fn download_pkg(pkg: &str, cache_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create cache dir {}", cache_dir.display()))?;
    match current() {
        Distro::Arch   => arch::download(pkg, cache_dir),
        Distro::Debian => debian::download(pkg, cache_dir),
        Distro::Fedora => fedora::download(pkg, cache_dir),
    }
}

/// Extract a package archive (returned by download_pkg) into dest_dir.
pub fn extract_pkg(pkg_path: &Path, dest_dir: &Path) -> Result<()> {
    match current() {
        Distro::Arch   => arch::extract(pkg_path, dest_dir),
        Distro::Debian => debian::extract(pkg_path, dest_dir),
        Distro::Fedora => fedora::extract(pkg_path, dest_dir),
    }
}

/// List the regular file paths inside a package archive.
/// Used for snapshot-safe pre-unlinking before extraction.
/// Returns an empty vec on error so the caller can fall through to extraction.
pub fn list_pkg_files(pkg_path: &Path) -> Vec<String> {
    let r = match current() {
        Distro::Arch   => arch::list_files(pkg_path),
        Distro::Debian => debian::list_files(pkg_path),
        Distro::Fedora => fedora::list_files(pkg_path),
    };
    r.unwrap_or_default()
}

/// Get the latest available version of a package from the distro repos.
pub fn pkg_latest_version(pkg: &str) -> Result<Option<String>> {
    match current() {
        Distro::Arch   => arch::latest_version(pkg),
        Distro::Debian => debian::latest_version(pkg),
        Distro::Fedora => fedora::latest_version(pkg),
    }
}

/// Search available packages matching `query`. Returns a list of package names.
/// Returns `(package_name, repo)` pairs for display. Repo is the source
/// repository name (e.g. "core", "aur", "extra") or None when not applicable.
/// Results are deduplicated by package name (first occurrence wins).
pub fn pkg_search(query: &str) -> Vec<(String, Option<String>)> {
    match current() {
        Distro::Arch   => arch::search(query),
        Distro::Debian => debian::search(query).into_iter().map(|n| (n, None)).collect(),
        Distro::Fedora => fedora::search(query).into_iter().map(|n| (n, None)).collect(),
    }
}

/// Return true if candidate is strictly newer than current_ver.
pub fn version_is_newer(candidate: &str, current_ver: &str) -> Result<bool> {
    if candidate == current_ver {
        return Ok(false);
    }
    match current() {
        Distro::Arch   => arch::version_newer(candidate, current_ver),
        Distro::Debian => debian::version_newer(candidate, current_ver),
        Distro::Fedora => fedora::version_newer(candidate, current_ver),
    }
}

// ── Signature-verification verdicts (pure, unit-tested) ───────────────────────

/// True when `rpmkeys --checksig` reports a package as signed by a trusted key.
/// A good run exits 0 and prints `…: digests signatures OK`; plain `digests OK`
/// (no "signatures") means unsigned, and anything else means a bad signature.
fn rpm_checksig_ok(success: bool, stdout: &str) -> bool {
    success && stdout.to_lowercase().contains("signatures ok")
}

/// True when `apt-get download` warns that a package could not be authenticated
/// against a signed repository (an untrusted repo). apt still downloads such a
/// package, so wryayer treats this as a verification failure.
fn apt_reports_unauthenticated(stderr: &str) -> bool {
    stderr.to_lowercase().contains("cannot be authenticated")
}

// ── Shared HTTP downloader ────────────────────────────────────────────────────

pub(crate) fn download_url(url: &str, dest: &Path) -> Result<()> {
    use std::io::Read;

    // Without timeouts a mirror that accepts the connection then stalls wedges
    // the whole install forever — the classic "stuck at Checking for missing
    // shared library dependencies..." hang, where the soname loop is fetching a
    // dependency and the socket never returns. `connect_timeout` fails fast on a
    // dead mirror (so the mirrorlist fallback can try the next one); `timeout`
    // here is a per-operation read/write socket timeout, not a total deadline,
    // so a large package that keeps making progress still completes — only a
    // stall longer than the window aborts.
    let client = reqwest::blocking::Client::builder()
        .user_agent("curl/7.88.1")
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("failed to build HTTP client")?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("HTTP request failed for {url}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("mirror returned 404 for {url}");
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

    let mut file = std::fs::File::create(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;

    let mut buf = [0u8; 65536];
    loop {
        let n = response.read(&mut buf).context("failed to read response body")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("failed to write downloaded data")?;
        pb.inc(n as u64);
    }
    pb.finish_and_clear();
    Ok(())
}

// ── Arch / pacman / makepkg ───────────────────────────────────────────────────

mod arch {
    use super::*;
    use crate::package::deps::{parse_pacman_depends, parse_pacman_field};

    pub fn query_info(pkg: &str) -> Result<Option<(String, Vec<String>)>> {
        let output = Command::new("pacman")
            .args(["-Si", pkg])
            .env("LANG", "C")
            .env("LC_ALL", "C")
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

    pub fn resolve_virtual(dep: &str) -> Result<Option<String>> {
        let output = Command::new("pacman")
            .args(["-Spdd", "--noconfirm", "--print-format", "%n", dep])
            .output()
            .context("failed to spawn pacman -Spdd")?;
        if !output.status.success() {
            return Ok(None);
        }
        let provider = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if provider.is_empty() || provider == dep {
            return Ok(None);
        }
        Ok(Some(provider))
    }

    pub fn soname_owner(filename: &str) -> Result<Option<String>> {
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

        // Fallback: query the file database so we can find the owning package
        // even when it isn't installed on the host (e.g. alsa-lib on a
        // PipeWire-only system).  Requires `pacman -Fy` to have been run at
        // least once, which is standard on any up-to-date Arch install.
        for dir in ["/usr/lib", "/usr/lib64", "/lib", "/lib64"] {
            let path = format!("{dir}/{filename}");
            let Ok(out) = Command::new("pacman")
                .args(["-F", &path])
                .env("LANG", "C")
                .env("LC_ALL", "C")
                .output()
            else {
                continue;
            };
            if !out.status.success() {
                continue;
            }
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                // pacman -F output: "<file> is owned by <repo>/<pkg> <version>"
                let line = line.trim();
                let Some((_, after_owned)) = line.split_once(" is owned by ") else { continue };
                let Some((_, pkg_part)) = after_owned.split_once('/') else { continue };
                let pkg = pkg_part.split_whitespace().next().unwrap_or("").to_string();
                if !pkg.is_empty() {
                    return Ok(Some(pkg));
                }
            }
        }

        Ok(None)
    }

    pub fn download(pkg: &str, cache_dir: &Path) -> Result<PathBuf> {
        download_impl(pkg, cache_dir, true)
    }

    fn download_impl(pkg: &str, cache_dir: &Path, allow_refresh: bool) -> Result<PathBuf> {
        let output = Command::new("pacman")
            .args(["-Spdd", "--noconfirm", pkg])
            .output()
            .context("failed to spawn pacman -Spdd")?;

        if !output.status.success() {
            bail!(
                "pacman cannot find '{pkg}':\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let url = stdout
            .lines()
            .find(|l| {
                let t = l.trim();
                t.starts_with("https://") || t.starts_with("http://") || t.starts_with("file://")
            })
            .with_context(|| format!("pacman produced no download URL for '{pkg}' (stdout: {})", stdout.trim()))?
            .trim();

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
        let dest = cache_dir.join(filename);
        if dest.exists() {
            return verified(pkg, url, dest);
        }

        eprintln!("  Downloading {pkg}...");
        let primary_err = match download_url(url, &dest) {
            Ok(()) => return verified(pkg, url, dest),
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                e
            }
        };

        let is_404 = format!("{primary_err:#}").contains("404");

        // On transient failures (not 404 — that means the package doesn't exist at
        // that path on this mirror) try every other mirror from the mirrorlist files.
        if !is_404 {
            for fallback_url in mirror_fallback_urls(url, filename) {
                eprintln!("  Retrying with fallback mirror...");
                if download_url(&fallback_url, &dest).is_ok() {
                    return verified(pkg, &fallback_url, dest);
                }
                let _ = std::fs::remove_file(&dest);
            }
        }

        // 404 means the local package database is stale: the cached URL points to
        // a version that's no longer on the mirror. Offer to refresh and retry.
        if is_404 && allow_refresh {
            let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) != 0 };
            if is_tty {
                eprint!(
                    "\n  Package databases may be out of date (got 404 for '{pkg}').\n  \
                     Update sources with 'sudo pacman -Sy'? [Y/n]: "
                );
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let mut ans = String::new();
                let _ = std::io::stdin().read_line(&mut ans);
                let ans = ans.trim().to_ascii_lowercase();
                if ans.is_empty() || ans == "y" || ans == "yes" {
                    eprintln!("  Syncing package databases...");
                    let ok = crate::prompt::sudo()
                        .args(["pacman", "-Sy"])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if ok {
                        return download_impl(pkg, cache_dir, false);
                    }
                    eprintln!("  warning: sudo pacman -Sy failed");
                }
            } else {
                // Non-interactive (TUI subprocess): emit a marker so the TUI can
                // show a popup and relaunch the install with --sync-db.
                eprintln!("PROMPT_OUTDATED_PACKAGES:{pkg}");
            }
        }

        Err(primary_err).with_context(|| format!("failed to download '{pkg}'"))
    }

    /// Authenticate a downloaded package against the pacman keyring and return
    /// its path, or delete it and error out. wryayer fetches packages straight
    /// from a mirror URL, bypassing pacman's own signature check, so without
    /// this a compromised or MITM'd mirror could drop arbitrary files into the
    /// sandbox root. Set WRYAYER_SKIP_SIG_VERIFY=1 to bypass (e.g. a private
    /// unsigned repo).
    fn verified(pkg: &str, url: &str, dest: PathBuf) -> Result<PathBuf> {
        if std::env::var_os("WRYAYER_SKIP_SIG_VERIFY").is_some() {
            return Ok(dest);
        }
        match verify_signature(url, &dest) {
            Ok(()) => Ok(dest),
            Err(e) => {
                // A file that fails verification is untrustworthy — drop it so a
                // later run re-downloads instead of reusing the bad cache entry.
                let _ = std::fs::remove_file(&dest);
                Err(e).with_context(|| format!("refusing to install unverified package '{pkg}'"))
            }
        }
    }

    /// Download `<pkg_url>.sig` and verify the detached PGP signature against the
    /// pacman keyring. Official Arch (and CachyOS) packages are always signed and
    /// the mirror serves the `.sig` alongside the package.
    fn verify_signature(pkg_url: &str, pkg_path: &Path) -> Result<()> {
        const KEYRING: &str = "/etc/pacman.d/gnupg";
        if !Path::new(KEYRING).is_dir() {
            bail!("pacman keyring {KEYRING} not found; cannot verify package signature");
        }
        let sig_path = PathBuf::from(format!("{}.sig", pkg_path.display()));
        download_url(&format!("{pkg_url}.sig"), &sig_path)
            .with_context(|| format!("failed to download signature for {}", pkg_path.display()))?;

        let out = Command::new("gpg")
            .args(["--homedir", KEYRING, "--status-fd", "1", "--verify"])
            .arg(&sig_path)
            .arg(pkg_path)
            .output();
        let _ = std::fs::remove_file(&sig_path);
        let out = out.context("failed to run gpg (is gnupg installed?)")?;

        // gpg exits 0 and prints a VALIDSIG status line only when the signature
        // is good AND made by a key already in the pacman keyring. Benign
        // warnings (unsafe homedir ownership, unwritable trustdb) go to stderr
        // and don't affect the verdict.
        let status = String::from_utf8_lossy(&out.stdout);
        if out.status.success() && status.contains("VALIDSIG") {
            return Ok(());
        }
        bail!(
            "package signature invalid or not signed by a trusted key:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    /// Return alternative download URLs for `filename` by substituting the same
    /// arch/repo values into every `Server =` line found in `/etc/pacman.d/*mirrorlist*`.
    ///
    /// Handles two URL layouts:
    ///   Standard Arch: …/<repo>/os/<arch>/<filename>
    ///   CachyOS style: …/<arch_v3>/<repo>/<filename>
    fn mirror_fallback_urls(failed_url: &str, filename: &str) -> Vec<String> {
        let parts: Vec<&str> = failed_url.split('/').collect();
        let n = parts.len();
        if n < 4 {
            return vec![];
        }
        let p2 = parts[n - 2];
        let p3 = parts[n - 3];

        // Detect layout from the path components immediately before the filename.
        //   Standard Arch: …/repo/os/arch/filename  → p3 == "os"
        //   CachyOS style: …/arch_v3/repo/filename  → p3 != "os"
        let (arch_v3, base_arch, repo): (&str, &str, &str) = if p3 == "os" {
            let arch = p2;
            let Some(repo) = parts.get(n.wrapping_sub(4)).copied() else {
                return vec![];
            };
            (arch, strip_v_suffix(arch), repo)
        } else {
            (p3, strip_v_suffix(p3), p2)
        };

        let mut urls = vec![];
        for server in read_mirrorlist_servers() {
            let expanded = server
                .replace("$arch_v3", arch_v3)
                .replace("$arch", base_arch)
                .replace("$repo", repo);
            let url = format!("{}/{}", expanded.trim_end_matches('/'), filename);
            if url != failed_url && !urls.contains(&url) {
                urls.push(url);
            }
        }
        urls
    }

    /// Strip a `_v<digits>` version suffix from an arch component, e.g.
    /// `"x86_64_v3"` → `"x86_64"`, `"x86_64"` → `"x86_64"`.
    fn strip_v_suffix(s: &str) -> &str {
        if let Some(idx) = s.rfind("_v") {
            let suffix = &s[idx + 2..];
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                return &s[..idx];
            }
        }
        s
    }

    /// Read every `Server =` line from files whose name contains "mirrorlist"
    /// under `/etc/pacman.d/`.
    fn read_mirrorlist_servers() -> Vec<String> {
        let mut servers = vec![];
        let pacman_d = Path::new("/etc/pacman.d");
        let Ok(entries) = std::fs::read_dir(pacman_d) else {
            return servers;
        };
        let mut sorted: Vec<_> = entries.flatten().collect();
        sorted.sort_by_key(|e| e.file_name());
        for entry in sorted {
            let fname = entry.file_name().to_string_lossy().into_owned();
            if !fname.contains("mirrorlist") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in content.lines() {
                if let Some(server) = line.trim().strip_prefix("Server = ") {
                    if !server.is_empty() {
                        servers.push(server.to_string());
                    }
                }
            }
        }
        servers
    }

    pub fn extract(pkg_path: &Path, dest_dir: &Path) -> Result<()> {
        let pkg = pkg_path.to_str().context("pkg path not valid UTF-8")?;
        let dest = dest_dir.to_str().context("dest path not valid UTF-8")?;
        let output = Command::new("tar")
            .args([
                "--zstd", "-xf", pkg, "-C", dest,
                "--exclude=.PKGINFO", "--exclude=.BUILDINFO",
                "--exclude=.MTREE", "--exclude=.INSTALL",
            ])
            .output()
            .context("failed to spawn tar")?;
        if !output.status.success() {
            bail!(
                "tar extraction failed for {}:\n{}",
                pkg_path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    pub fn list_files(pkg_path: &Path) -> Result<Vec<String>> {
        let pkg = pkg_path.to_str().context("pkg path not valid UTF-8")?;
        let output = Command::new("tar")
            .args([
                "--zstd", "-tf", pkg,
                "--exclude=.PKGINFO", "--exclude=.BUILDINFO",
                "--exclude=.MTREE", "--exclude=.INSTALL",
            ])
            .output()
            .context("failed to list tar contents")?;
        if !output.status.success() {
            return Ok(vec![]);
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty() && !l.ends_with('/'))
            .map(str::to_string)
            .collect())
    }

    pub fn latest_version(pkg: &str) -> Result<Option<String>> {
        let output = Command::new("pacman")
            .args(["-Si", pkg])
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
            .context("failed to spawn pacman -Si")?;
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_pacman_field(&stdout, "Version"))
    }

    pub fn version_newer(candidate: &str, current: &str) -> Result<bool> {
        let output = Command::new("vercmp")
            .args([candidate, current])
            .output()
            .context("failed to run vercmp")?;
        if !output.status.success() {
            bail!("vercmp failed:\n{}", String::from_utf8_lossy(&output.stderr));
        }
        let result: i32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .context("vercmp output was not an integer")?;
        Ok(result > 0)
    }

    pub fn search(query: &str) -> Vec<(String, Option<String>)> {
        // Official repos — use full `pacman -Ss` output (not -q) so we get
        // the "repo/pkgname version" prefix. This lets us:
        //   a) show the repo in the TUI for context
        //   b) deduplicate packages that appear in multiple repos (e.g. core/bash
        //      and cachyos-core-v3/bash on CachyOS) — keep the first occurrence,
        //      which is what pacman would choose anyway.
        let out = Command::new("pacman")
            .args(["-Ss", query])
            .output()
            .unwrap_or_else(|_| std::process::Output {
                status: unsafe { std::mem::zeroed() },
                stdout: vec![],
                stderr: vec![],
            });

        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut results: Vec<(String, Option<String>)> = vec![];

        for line in stdout.lines() {
            // Package header lines: "repo/pkgname version [flags]"
            // Description lines start with whitespace — skip.
            if line.starts_with(|c: char| c.is_whitespace()) {
                continue;
            }
            if let Some((repo_name, _rest)) = line.split_once(' ') {
                if let Some((repo, name)) = repo_name.split_once('/') {
                    if !seen.contains(name) {
                        seen.insert(name.to_string());
                        results.push((name.to_string(), Some(repo.to_string())));
                    }
                }
            }
        }

        // AUR — RPC search, annotate with "aur", skip packages already in official results.
        let aur = search_aur(query)
            .into_iter()
            .filter(|n| !seen.contains(n.as_str()))
            .map(|n| (n, Some("aur".to_string())))
            .collect::<Vec<_>>();

        results.extend(aur);
        results
    }

    fn search_aur(query: &str) -> Vec<String> {
        let url = format!(
            "https://aur.archlinux.org/rpc/v5/search/{}?by=name-desc",
            query
        );
        let client = match reqwest::blocking::Client::builder()
            .user_agent("curl/7.88.1")
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let Ok(resp) = client.get(&url).send() else { return vec![] };
        let Ok(json) = resp.json::<serde_json::Value>() else { return vec![] };
        json.get("results")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        p.get("Name")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ── Debian / apt / dpkg ───────────────────────────────────────────────────────

mod debian {
    use super::*;

    pub fn query_info(pkg: &str) -> Result<Option<(String, Vec<String>)>> {
        let output = Command::new("apt-cache")
            .args(["show", "--no-all-versions", pkg])
            .output()
            .context("failed to spawn apt-cache show")?;
        if !output.status.success() || output.stdout.is_empty() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let version = parse_field(&stdout, "Version").unwrap_or_default();
        if version.is_empty() {
            return Ok(None);
        }
        Ok(Some((version, parse_depends(&stdout))))
    }

    pub fn resolve_virtual(dep: &str) -> Result<Option<String>> {
        // apt-cache showpkg lists reverse providers under "Reverse Provides:"
        let output = Command::new("apt-cache")
            .args(["showpkg", dep])
            .output()
            .context("failed to spawn apt-cache showpkg")?;
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut in_section = false;
        for line in stdout.lines() {
            if line.starts_with("Reverse Provides:") {
                in_section = true;
                continue;
            }
            if in_section {
                if line.trim().is_empty() || line.ends_with(':') {
                    break;
                }
                // Lines look like: "libcurl4 7.88.1-10+deb12u5"
                let provider = line.split_whitespace().next().unwrap_or("").to_string();
                if !provider.is_empty() {
                    return Ok(Some(provider));
                }
            }
        }
        Ok(None)
    }

    pub fn soname_owner(filename: &str) -> Result<Option<String>> {
        let triplet = arch_triplet();
        let paths = [
            format!("/usr/lib/{triplet}/{filename}"),
            format!("/usr/lib/{filename}"),
            format!("/lib/{triplet}/{filename}"),
            format!("/lib/{filename}"),
        ];
        for path in &paths {
            if !Path::new(path).exists() {
                continue;
            }
            let out = Command::new("dpkg")
                .args(["-S", path])
                .output()
                .context("failed to spawn dpkg -S")?;
            if out.status.success() {
                // Output: "libfoo2:amd64: /usr/lib/x86_64-linux-gnu/libfoo.so.2"
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(first) = text.lines().next() {
                    // Strip arch suffix: "libfoo2:amd64" → "libfoo2"
                    let pkg = first.split(':').next().unwrap_or("").trim().to_string();
                    if !pkg.is_empty() {
                        return Ok(Some(pkg));
                    }
                }
            }
        }

        // Fallback: apt-file searches all repos, including packages not installed on
        // the host (same class of fix as pacman -F for Arch).  Requires apt-file to be
        // installed and its database updated (`sudo apt-file update`).
        if let Ok(out) = Command::new("apt-file")
            .args(["search", "--regexp", &format!("/{filename}$")])
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    // "libfoo2: /usr/lib/x86_64-linux-gnu/libfoo.so.2"
                    if let Some((pkg_part, _)) = line.split_once(':') {
                        let pkg = pkg_part.trim().to_string();
                        if !pkg.is_empty() {
                            return Ok(Some(pkg));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    pub fn download(pkg: &str, cache_dir: &Path) -> Result<PathBuf> {
        if let Some(cached) = find_deb(cache_dir, pkg)? {
            return Ok(cached);
        }
        eprintln!("  Downloading {pkg}...");
        // apt-get download writes the .deb to the current directory. Its acquire
        // system verifies each package's hash against the repo's Packages index,
        // which is itself covered by the signed Release file — so a package that
        // downloads cleanly from a normal `apt update`'d setup is authenticated
        // by the apt trust chain. The one hole is an *untrusted* repo, where apt
        // downloads anyway but prints "cannot be authenticated"; treat that as a
        // hard failure (like the Arch/Fedora backends) unless the skip var is set.
        let output = Command::new("apt-get")
            .args(["download", pkg])
            .current_dir(cache_dir)
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
            .context("failed to spawn apt-get download")?;
        if !output.status.success() {
            bail!(
                "apt-get download failed for '{pkg}':\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if std::env::var_os("WRYAYER_SKIP_SIG_VERIFY").is_none()
            && super::apt_reports_unauthenticated(&stderr)
        {
            bail!(
                "refusing to install unverified package '{pkg}': apt could not \
                 authenticate it against a signed repository:\n{}",
                stderr.trim()
            );
        }
        find_deb(cache_dir, pkg)?
            .with_context(|| format!("no .deb found in cache after downloading '{pkg}'"))
    }

    pub fn extract(pkg_path: &Path, dest_dir: &Path) -> Result<()> {
        let pkg = pkg_path.to_str().context("pkg path not valid UTF-8")?;
        let dest = dest_dir.to_str().context("dest path not valid UTF-8")?;
        // dpkg-deb -x unpacks the data section of the .deb into dest_dir,
        // preserving the same usr/bin/, usr/lib/ hierarchy we rely on.
        let output = Command::new("dpkg-deb")
            .args(["-x", pkg, dest])
            .output()
            .context("failed to spawn dpkg-deb")?;
        if !output.status.success() {
            bail!(
                "dpkg-deb extraction failed for {}:\n{}",
                pkg_path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    pub fn list_files(pkg_path: &Path) -> Result<Vec<String>> {
        let pkg = pkg_path.to_str().context("pkg path not valid UTF-8")?;
        // dpkg-deb -c lists the file table.  Output columns:
        //   <perms> <owner>  <size>  <date>  ./<path>
        let output = Command::new("dpkg-deb")
            .args(["-c", pkg])
            .output()
            .context("failed to list deb contents")?;
        if !output.status.success() {
            return Ok(vec![]);
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let path = line.split_whitespace().last()?;
                let path = path.strip_prefix("./").unwrap_or(path);
                if path.ends_with('/') || path.is_empty() {
                    None
                } else {
                    Some(path.to_string())
                }
            })
            .collect())
    }

    pub fn latest_version(pkg: &str) -> Result<Option<String>> {
        let output = Command::new("apt-cache")
            .args(["policy", pkg])
            .output()
            .context("failed to spawn apt-cache policy")?;
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // "  Candidate: 7.88.1-10+deb12u5"
        for line in stdout.lines() {
            if let Some(ver) = line.trim().strip_prefix("Candidate:") {
                let ver = ver.trim();
                if ver != "(none)" && !ver.is_empty() {
                    return Ok(Some(ver.to_string()));
                }
            }
        }
        Ok(None)
    }

    pub fn version_newer(candidate: &str, current: &str) -> Result<bool> {
        // dpkg --compare-versions exits 0 when the comparison holds
        let output = Command::new("dpkg")
            .args(["--compare-versions", candidate, "gt", current])
            .output()
            .context("failed to run dpkg --compare-versions")?;
        Ok(output.status.success())
    }

    pub fn search(query: &str) -> Vec<String> {
        // apt-cache search outputs "pkg - description" one per line
        let Ok(out) = Command::new("apt-cache").args(["search", "--names-only", query]).output() else {
            return vec![];
        };
        let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.split_once(" - ").map(|(name, _)| name.to_string()))
            .collect();
        names.sort_unstable();
        names
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn find_deb(dir: &Path, pkg: &str) -> Result<Option<PathBuf>> {
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("failed to read cache dir {}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("{pkg}_")) && name.ends_with(".deb") {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    fn arch_triplet() -> String {
        // dpkg --print-architecture gives the dpkg arch name (e.g. "amd64").
        // Map to the GNU triplet used for multiarch library paths.
        let arch = Command::new("dpkg")
            .args(["--print-architecture"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        match arch.trim() {
            "amd64" => "x86_64-linux-gnu".to_string(),
            "arm64" => "aarch64-linux-gnu".to_string(),
            "armhf" => "arm-linux-gnueabihf".to_string(),
            "i386"  => "i386-linux-gnu".to_string(),
            other   => format!("{other}-linux-gnu"),
        }
    }

    fn parse_field(stdout: &str, field: &str) -> Option<String> {
        for line in stdout.lines() {
            if let Some(val) = line.strip_prefix(&format!("{field}: ")) {
                return Some(val.trim().to_string());
            }
        }
        None
    }

    fn parse_depends(stdout: &str) -> Vec<String> {
        let mut deps = vec![];
        for line in stdout.lines() {
            let dep_str = if let Some(s) = line.strip_prefix("Depends: ") {
                s
            } else if let Some(s) = line.strip_prefix("Pre-Depends: ") {
                s
            } else {
                continue;
            };
            // Comma-separated groups; within each group | means OR (take first).
            for group in dep_str.split(',') {
                let first = group.split('|').next().unwrap_or("").trim();
                // Strip version constraint: "libc6 (>= 2.17)" → "libc6"
                let name = first.split_whitespace().next().unwrap_or("").trim().to_string();
                if !name.is_empty() {
                    deps.push(name);
                }
            }
        }
        deps
    }
}

// ── Fedora / dnf / rpm ────────────────────────────────────────────────────────
//
// Covers Fedora, RHEL, CentOS, AlmaLinux, Rocky Linux, openSUSE, and any
// other RPM-based distro with dnf (dnf4 or dnf5) and rpm installed.
//
// Dependency resolution notes:
//   RPM requires can be soname virtuals (libfoo.so.2()(64bit)), file deps
//   (/usr/bin/sh), or capability deps (pkgconfig(glib-2.0)).  We filter these
//   down to bare package-name-like entries so the dep resolver only queues
//   things it can actually look up.  Soname gaps are caught at install time by
//   satisfy_missing_sonames.

mod fedora {
    use super::*;
    use std::process::Stdio;

    pub fn query_info(pkg: &str) -> Result<Option<(String, Vec<String>)>> {
        // dnf repoquery is available in dnf-plugins-core (dnf4) or built-in (dnf5).
        let ver_out = Command::new("dnf")
            .args(["repoquery", "--quiet", "--queryformat", "%{version}", pkg])
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
            .context("failed to spawn dnf repoquery")?;

        if !ver_out.status.success() || ver_out.stdout.is_empty() {
            return Ok(None);
        }
        let version = String::from_utf8_lossy(&ver_out.stdout)
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        if version.is_empty() {
            return Ok(None);
        }

        let dep_out = Command::new("dnf")
            .args(["repoquery", "--quiet", "--requires", pkg])
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
            .context("failed to spawn dnf repoquery --requires")?;

        let deps = if dep_out.status.success() {
            String::from_utf8_lossy(&dep_out.stdout)
                .lines()
                .filter_map(|line| {
                    let dep = line.trim();
                    if dep.is_empty() {
                        return None;
                    }
                    // Skip soname virtuals (.so), file deps (/), capability deps (()
                    if dep.contains(".so") || dep.starts_with('/') || dep.contains('(') {
                        return None;
                    }
                    // Strip version constraint: "foo >= 1.0" → "foo"
                    let name = dep.split_whitespace().next().unwrap_or("").trim();
                    if name.is_empty() { None } else { Some(name.to_string()) }
                })
                .collect()
        } else {
            vec![]
        };

        Ok(Some((version, deps)))
    }

    pub fn resolve_virtual(dep: &str) -> Result<Option<String>> {
        let out = Command::new("dnf")
            .args(["repoquery", "--quiet", "--queryformat", "%{name}", "--whatprovides", dep])
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
            .context("failed to spawn dnf repoquery --whatprovides")?;
        if !out.status.success() {
            return Ok(None);
        }
        let provider = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        if provider.is_empty() || provider == dep {
            return Ok(None);
        }
        Ok(Some(provider))
    }

    pub fn soname_owner(filename: &str) -> Result<Option<String>> {
        // First: check installed packages via rpm -qf
        for dir in ["/usr/lib64", "/usr/lib", "/lib64", "/lib"] {
            let path = format!("{dir}/{filename}");
            if !Path::new(&path).exists() {
                continue;
            }
            let out = Command::new("rpm")
                .args(["-qf", "--queryformat", "%{NAME}", &path])
                .output()
                .context("failed to spawn rpm -qf")?;
            if out.status.success() {
                let pkg = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !pkg.is_empty() && !pkg.starts_with("error:") {
                    return Ok(Some(pkg));
                }
            }
        }

        // Fallback: search all repos via dnf repoquery --file
        // Works even when the owning package is not installed on the host.
        let pattern = format!("*/{filename}");
        if let Ok(out) = Command::new("dnf")
            .args(["repoquery", "--quiet", "--queryformat", "%{name}", "--file", &pattern])
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
        {
            if out.status.success() {
                let pkg = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !pkg.is_empty() {
                    return Ok(Some(pkg));
                }
            }
        }

        Ok(None)
    }

    pub fn download(pkg: &str, cache_dir: &Path) -> Result<PathBuf> {
        if let Some(cached) = find_rpm(cache_dir, pkg)? {
            return verified(pkg, cached);
        }
        eprintln!("  Downloading {pkg}...");
        let output = Command::new("dnf")
            .args([
                "download",
                "--quiet",
                "--destdir",
                cache_dir.to_str().unwrap_or("."),
                pkg,
            ])
            .output()
            .context("failed to spawn dnf download")?;
        if !output.status.success() {
            bail!(
                "dnf download failed for '{pkg}':\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let path = find_rpm(cache_dir, pkg)?
            .with_context(|| format!("no .rpm found in cache after downloading '{pkg}'"))?;
        verified(pkg, path)
    }

    /// Verify a downloaded `.rpm`'s GPG signature against the host rpm keyring
    /// with `rpmkeys --checksig`. `dnf download` fetches the package but does not
    /// check its signature, so — like the Arch backend — wryayer verifies before
    /// extraction so a tampered or MITM'd mirror can't drop arbitrary files into
    /// a sandbox root. A good result reads `…: digests signatures OK`, meaning the
    /// package is signed by a key already in the rpm keyring. Set
    /// `WRYAYER_SKIP_SIG_VERIFY=1` to bypass (e.g. an unsigned third-party repo).
    fn verified(pkg: &str, path: PathBuf) -> Result<PathBuf> {
        if std::env::var_os("WRYAYER_SKIP_SIG_VERIFY").is_some() {
            return Ok(path);
        }
        let run = |bin: &str| {
            Command::new(bin)
                .arg("--checksig")
                .arg(&path)
                .env("LANG", "C")
                .env("LC_ALL", "C")
                .output()
        };
        // rpmkeys is the modern entry point; fall back to `rpm --checksig`.
        let out = match run("rpmkeys") {
            Ok(o) => o,
            Err(_) => run("rpm").context("failed to run rpmkeys/rpm --checksig (is rpm installed?)")?,
        };
        let report = String::from_utf8_lossy(&out.stdout);
        if super::rpm_checksig_ok(out.status.success(), &report) {
            return Ok(path);
        }
        let _ = std::fs::remove_file(&path);
        bail!(
            "refusing to install unverified package '{pkg}': rpm signature invalid, \
             unsigned, or signed by a key not in the rpm keyring:\n{}",
            report.trim()
        );
    }

    pub fn extract(pkg_path: &Path, dest_dir: &Path) -> Result<()> {
        let rpm2cpio = Command::new("rpm2cpio")
            .arg(pkg_path)
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to spawn rpm2cpio")?;

        let output = Command::new("cpio")
            .args(["--extract", "--make-directories", "--preserve-modification-time", "--quiet"])
            .stdin(rpm2cpio.stdout.unwrap())
            .current_dir(dest_dir)
            .output()
            .context("failed to spawn cpio")?;

        if !output.status.success() {
            bail!(
                "cpio extraction failed for {}:\n{}",
                pkg_path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    pub fn list_files(pkg_path: &Path) -> Result<Vec<String>> {
        let rpm2cpio = Command::new("rpm2cpio")
            .arg(pkg_path)
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to spawn rpm2cpio")?;

        let output = Command::new("cpio")
            .args(["--list", "--quiet"])
            .stdin(rpm2cpio.stdout.unwrap())
            .output()
            .context("failed to spawn cpio --list")?;

        if !output.status.success() {
            return Ok(vec![]);
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| {
                let path = l.trim().strip_prefix("./").unwrap_or(l.trim());
                if path.is_empty() || path.ends_with('/') {
                    None
                } else {
                    Some(path.to_string())
                }
            })
            .collect())
    }

    pub fn latest_version(pkg: &str) -> Result<Option<String>> {
        let out = Command::new("dnf")
            .args(["repoquery", "--quiet", "--queryformat", "%{version}", pkg])
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output()
            .context("failed to spawn dnf repoquery")?;
        if !out.status.success() {
            return Ok(None);
        }
        let ver = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        if ver.is_empty() { Ok(None) } else { Ok(Some(ver)) }
    }

    pub fn version_newer(candidate: &str, current: &str) -> Result<bool> {
        // rpm.vercmp is accessible via the Lua interpreter built into rpm --eval.
        let script = format!(
            "%{{lua:print(rpm.vercmp(\"{}\", \"{}\") > 0 and \"1\" or \"0\")}}",
            candidate.replace('"', ""),
            current.replace('"', ""),
        );
        let out = Command::new("rpm")
            .args(["--eval", &script])
            .output()
            .context("failed to run rpm --eval")?;
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "1")
    }

    pub fn search(query: &str) -> Vec<String> {
        let pattern = format!("*{query}*");
        let Ok(out) = Command::new("dnf")
            .args(["repoquery", "--quiet", "--queryformat", "%{name}", &pattern])
            .output()
        else {
            return vec![];
        };
        let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    fn find_rpm(dir: &Path, pkg: &str) -> Result<Option<PathBuf>> {
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("failed to read cache dir {}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("{pkg}-")) && name.ends_with(".rpm") {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod verify_tests {
    use super::{apt_reports_unauthenticated, rpm_checksig_ok};

    #[test]
    fn rpm_signed_by_trusted_key_passes() {
        assert!(rpm_checksig_ok(true, "firefox-130.rpm: digests signatures OK"));
        assert!(rpm_checksig_ok(true, "foo.rpm: digests SIGNATURES OK\n"));
    }

    #[test]
    fn rpm_unsigned_or_bad_fails() {
        // Unsigned: digests OK but no "signatures".
        assert!(!rpm_checksig_ok(true, "foo.rpm: digests OK"));
        // Bad signature: non-zero exit.
        assert!(!rpm_checksig_ok(false, "foo.rpm: digests SIGNATURES NOT OK"));
        // Missing key still counts as not-OK even if the word appears negated.
        assert!(!rpm_checksig_ok(false, "foo.rpm: digests signatures OK"));
        assert!(!rpm_checksig_ok(true, ""));
    }

    #[test]
    fn apt_unauthenticated_is_detected() {
        assert!(apt_reports_unauthenticated(
            "WARNING: The following packages cannot be authenticated!\n  foo\n"
        ));
        assert!(apt_reports_unauthenticated("E: Some packages Cannot Be Authenticated"));
    }

    #[test]
    fn apt_clean_output_is_authenticated() {
        assert!(!apt_reports_unauthenticated("Get:1 http://deb.debian.org foo 1.0\nFetched 1 B\n"));
        assert!(!apt_reports_unauthenticated(""));
    }
}
