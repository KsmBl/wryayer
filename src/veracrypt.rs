//! VeraCrypt container management.
//!
//! An encrypted app keeps its whole filesystem tree inside a VeraCrypt file
//! container instead of a plain directory. The container file lives at
//! `~/.wryayer/.containers/<app>.hc` and is mounted **over** the app's normal
//! directory `~/.wryayer/<app>/`, so every other subsystem (bwrap, update,
//! snapshot, dedup, …) keeps working against the same path it always used —
//! it simply sees an empty directory while the app is locked.
//!
//! ## Why shell out instead of using cryptsetup directly
//!
//! `cryptsetup` can open VeraCrypt volumes (`--type tcrypt --veracrypt`), but
//! creating them is VeraCrypt-only. Driving the official binary for every
//! operation keeps one implementation responsible for the on-disk format, so a
//! container wryayer made is a completely ordinary VeraCrypt volume the user can
//! open with the VeraCrypt GUI on any machine — nothing here is wryayer-specific.
//!
//! ## Root privileges
//!
//! Creating a container is unprivileged, but *mounting* one needs root on Linux
//! (loop device + `mount`), so VeraCrypt re-execs itself under `sudo`. That
//! means mount/unmount/format must run attached to a terminal where the user can
//! answer the sudo prompt. Callers that run under a TUI must suspend it first.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::manifest::wryayer_root;

/// Cipher for new containers.
///
/// Plain AES rather than a cascade (AES-Twofish, …): every current x86_64 and
/// arm64 CPU has an AES instruction set, so AES adds essentially no latency to
/// the app's disk I/O, while a cascade runs its second layer in software and
/// would slow every read the sandboxed app makes. A cascade also buys nothing
/// against any realistic attacker here — AES-256 is not the weak link, the
/// password is. Existing containers are unaffected; VeraCrypt stores the choice
/// in the volume header and detects it on mount.
const ENCRYPTION: &str = "AES";

/// Header key-derivation hash for new containers.
const HASH: &str = "SHA-512";

/// Filesystem created inside new containers.
///
/// ext4 is required, not cosmetic: app trees contain symlinks, executable bits,
/// and hard links (snapshots and `wryayer dedup` are built on hard links). A
/// FAT/exFAT container would silently break all three.
const FILESYSTEM: &str = "ext4";

/// Absolute path of the container file backing `app_name`.
pub fn container_path(app_name: &str) -> Result<PathBuf> {
    Ok(containers_dir()?.join(format!("{app_name}.hc")))
}

/// Directory holding every app's container file. Dot-prefixed so
/// [`crate::manifest::list_all_apps`] skips it — it is not an app.
pub fn containers_dir() -> Result<PathBuf> {
    Ok(wryayer_root()?.join(".containers"))
}

/// Whether the `veracrypt` binary is on PATH.
pub fn available() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("veracrypt").exists()))
        .unwrap_or(false)
}

/// Error message used whenever an operation needs VeraCrypt but can't find it.
pub fn missing_binary_error() -> anyhow::Error {
    anyhow::anyhow!(
        "veracrypt not found on PATH — install it first:\n    \
         Arch:   sudo pacman -S veracrypt\n    \
         Debian: sudo apt install veracrypt"
    )
}

/// A single row of `veracrypt --list`.
#[derive(Debug, Clone, PartialEq)]
pub struct MountedVolume {
    /// Path of the container file.
    pub volume: String,
    /// The `/dev/mapper/veracryptN` device.
    pub mapper: String,
    /// Where it is mounted, or `None` when the volume is opened but not mounted.
    pub mount_point: Option<String>,
}

/// Parse the output of `veracrypt --text --list`.
///
/// Each line looks like:
/// `1: /home/u/.wryayer/.containers/app.hc /dev/mapper/veracrypt1 /home/u/.wryayer/app`
/// A volume that is opened but not mounted has `-` in the mount-point column.
pub fn parse_list(output: &str) -> Vec<MountedVolume> {
    let mut out = Vec::new();
    for line in output.lines() {
        // Strip the leading "<slot>:" then split the three whitespace-separated
        // columns. Paths containing spaces would break this, but the container
        // paths wryayer creates never do.
        let Some((_slot, rest)) = line.split_once(':') else { continue };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }
        let mount_point = match fields.get(2) {
            Some(&"-") | None => None,
            Some(p) => Some((*p).to_string()),
        };
        out.push(MountedVolume {
            volume: fields[0].to_string(),
            mapper: fields[1].to_string(),
            mount_point,
        });
    }
    out
}

/// Every VeraCrypt volume currently open on the system.
pub fn list_mounted() -> Result<Vec<MountedVolume>> {
    if !available() {
        return Ok(Vec::new());
    }
    let out = Command::new("veracrypt")
        .args(["--text", "--list"])
        .stdin(Stdio::null())
        .output()
        .context("failed to run 'veracrypt --list'")?;
    // A completely empty volume list exits non-zero with "No volumes mounted",
    // which is a normal state rather than an error.
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
}

/// Whether `app_name`'s container is currently mounted at its app directory.
pub fn is_mounted(app_name: &str) -> Result<bool> {
    let target = crate::manifest::app_dir(app_name)?;
    let target = target.to_string_lossy().into_owned();
    Ok(list_mounted()?
        .iter()
        .any(|v| v.mount_point.as_deref() == Some(target.as_str())))
}

/// Legacy marker location: inside the app's own directory, where mounting the
/// container hid it. Still read so containers made before the move keep working.
pub const MARKER_FILE: &str = ".encrypted.toml";

/// The minimum an encrypted app must record outside its container so it can
/// still be listed, renamed, removed and *unlocked* while locked.
///
/// Deliberately not the full manifest: the installed package list stays inside
/// the container, so a locked app reveals nothing about what it is built from.
///
/// It lives next to the container file rather than inside the app directory,
/// because the app directory is a mount point — a marker there is hidden
/// whenever the container is mounted, which makes it unwritable exactly when
/// settings change. Keeping it outside means it is readable *and* writable in
/// both states.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Marker {
    pub name: String,
    pub main_binary: String,
    pub installed_at: String,
    #[serde(default)]
    pub launchers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkg_name: Option<String>,
    /// Mirror of the app's `password_source` setting.
    ///
    /// The real setting lives in `config.ini` *inside* the container, so it
    /// can't be consulted while locked — which is precisely when the unlock
    /// path needs to know whether to prompt or to read the master store. Kept
    /// in step by `config::write_config`.
    #[serde(default = "default_password_source")]
    pub password_source: String,
}

fn default_password_source() -> String {
    "prompt".to_string()
}

impl Marker {
    /// Capture the listing-relevant fields of a manifest.
    pub fn from_manifest(m: &crate::manifest::Manifest) -> Self {
        Self {
            name: m.app.name.clone(),
            main_binary: m.app.main_binary.clone(),
            installed_at: m.app.installed_at.clone(),
            launchers: m.app.launchers.clone(),
            alias_of: m.app.alias_of.clone(),
            display_name: m.app.display_name.clone(),
            pkg_name: m.app.pkg_name.clone(),
            password_source: default_password_source(),
        }
    }

    /// Rebuild a manifest stub for listing a locked app. `packages` is empty —
    /// the real list is inside the container.
    pub fn to_manifest(&self) -> crate::manifest::Manifest {
        crate::manifest::Manifest {
            app: crate::manifest::AppMeta {
                name: self.name.clone(),
                main_binary: self.main_binary.clone(),
                installed_at: self.installed_at.clone(),
                launchers: self.launchers.clone(),
                alias_of: self.alias_of.clone(),
                display_name: self.display_name.clone(),
                pkg_name: self.pkg_name.clone(),
                wine_game: None,
            },
            packages: Vec::new(),
        }
    }
}

/// Path of the locked-state marker for `app_name`, beside its container file.
pub fn marker_path(app_name: &str) -> Result<PathBuf> {
    Ok(containers_dir()?.join(format!("{app_name}.toml")))
}

/// Write the locked-state marker. Safe in either state — it lives outside the
/// mount point.
pub fn write_marker(app_name: &str, marker: &Marker) -> Result<()> {
    let path = marker_path(app_name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(marker).context("failed to serialize the app marker")?;
    std::fs::write(&path, text)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Read the locked-state marker for `app_name`.
///
/// Falls back to the legacy in-app-directory location so containers created
/// before the marker moved still list and unlock correctly.
pub fn read_marker(app_name: &str) -> Option<Marker> {
    let read = |p: PathBuf| -> Option<Marker> {
        toml::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };
    marker_path(app_name)
        .ok()
        .and_then(read)
        .or_else(|| crate::manifest::app_dir(app_name).ok().and_then(|d| read(d.join(MARKER_FILE))))
}

/// Update just the recorded password source, leaving the rest of the marker
/// alone. No-op for apps that aren't encrypted.
pub fn set_marker_password_source(app_name: &str, source: &str) -> Result<()> {
    if !is_encrypted(app_name) {
        return Ok(());
    }
    let Some(mut marker) = read_marker(app_name) else {
        return Ok(());
    };
    if marker.password_source == source {
        return Ok(());
    }
    marker.password_source = source.to_string();
    write_marker(app_name, &marker)
}

/// Delete the marker (used when an app stops being encrypted, or is removed).
pub fn remove_marker(app_name: &str) {
    if let Ok(p) = marker_path(app_name) {
        let _ = std::fs::remove_file(p);
    }
    if let Ok(d) = crate::manifest::app_dir(app_name) {
        let _ = std::fs::remove_file(d.join(MARKER_FILE));
    }
}

/// Whether `app_name` is stored in an encrypted container at all.
///
/// Keyed on the container file rather than the marker, so it stays true while
/// the app is unlocked and the marker is hidden behind the mount.
pub fn is_encrypted(app_name: &str) -> bool {
    container_path(app_name).map(|p| p.exists()).unwrap_or(false)
}

/// Whether `app_name` is encrypted *and* currently locked (container not
/// mounted, so its tree is inaccessible).
pub fn is_locked(app_name: &str) -> bool {
    is_encrypted(app_name) && !is_mounted(app_name).unwrap_or(false)
}

/// Round `bytes` up to the next whole multiple of `unit`.
fn round_up(bytes: u64, unit: u64) -> u64 {
    bytes.div_ceil(unit) * unit
}

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Pick a container size for an app tree currently using `used_bytes`.
///
/// The container is a fixed-size file, so this trades two opposing risks: too
/// small and the app hits ENOSPC mid-run (browser profiles and caches grow a
/// lot), too large and the file wastes disk that can never be reclaimed.
///
/// The rule is `used + headroom`, where headroom is half the current tree
/// clamped to 512 MiB…2 GiB, plus a fixed allowance for ext4 metadata (journal,
/// inode tables and the 5% reserved-blocks default), rounded up to 128 MiB:
///
/// | tree    | container |
/// |---------|-----------|
/// | empty   | 640 MiB   |
/// | 50 MiB  | 768 MiB   |
/// | 500 MiB | 1.25 GiB  |
/// | 2 GiB   | 3.25 GiB  |
/// | 10 GiB  | 12.5 GiB  |
///
/// Small apps get generous room to grow because it costs little in absolute
/// terms; large apps get proportionally less because doubling a 10 GiB game is
/// far more expensive than doubling a 50 MiB utility. The minimum headroom sets
/// an effective floor of 640 MiB, comfortably above what ext4 needs for its own
/// structures on an otherwise empty volume.
pub fn recommended_size(used_bytes: u64) -> u64 {
    let headroom = (used_bytes / 2).clamp(512 * MIB, 2 * GIB);
    // ext4 overhead: journal (up to 128 MiB) + inode tables + the 5% reserve.
    let overhead = 128 * MIB + used_bytes / 20;
    round_up(used_bytes + headroom + overhead, 128 * MIB)
}

/// Total bytes consumed by a directory tree, following no symlinks and counting
/// each hard-linked inode once (so snapshots don't inflate the estimate).
pub fn tree_size(dir: &Path) -> u64 {
    use std::collections::HashSet;
    use std::os::unix::fs::MetadataExt;

    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total = 0u64;
    let mut queue = vec![dir.to_path_buf()];
    while let Some(d) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let Ok(md) = entry.metadata() else { continue };
            if md.is_dir() {
                queue.push(entry.path());
            } else if md.is_file() {
                // Count a multiply-linked inode only the first time it is seen.
                if md.nlink() > 1 && !seen.insert((md.dev(), md.ino())) {
                    continue;
                }
                total += md.len();
            }
        }
    }
    total
}

/// Whether sudo can currently run without asking for a password.
///
/// Used to tell an "authenticate first" situation apart from a real failure,
/// and by the TUI to decide whether it needs to ask for the sudo password
/// before starting a container operation.
pub fn sudo_is_primed() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Cache sudo credentials using `password`, so later container operations run
/// without prompting.
///
/// Feeds `sudo -S -v`, which reads the password from stdin instead of the
/// terminal. This is what lets the TUI collect the password in an overlay and
/// then run the install as a normal piped operation.
pub fn prime_sudo(password: &str) -> Result<()> {
    let mut child = Command::new("sudo")
        .args(["-S", "-v"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // Swallow the "[sudo] password for …" prompt; the caller already asked.
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run sudo")?;
    child
        .stdin
        .as_mut()
        .context("failed to open sudo stdin")?
        .write_all(format!("{password}\n").as_bytes())
        .context("failed to send the sudo password")?;
    let status = child.wait().context("failed to wait for sudo")?;
    if !status.success() {
        bail!("sudo authentication failed — wrong password?");
    }
    Ok(())
}

/// Run a veracrypt subcommand under sudo, feeding the *volume* password on
/// stdin.
///
/// Two separate secrets are in play and they must not collide. VeraCrypt needs
/// root to set up a loop device, and its own escalation path can't be used
/// here: with `--non-interactive` it has no way to ask for an admin password
/// and simply fails, while without it, it would prompt on a terminal that a
/// TUI-spawned process doesn't have. So wryayer invokes `sudo` itself — sudo
/// reads its password from `/dev/tty` (or from a cached ticket, see
/// [`prime_sudo`]), leaving stdin free for the volume password.
///
/// The volume password goes through stdin rather than `--password=` so it never
/// appears in `/proc/<pid>/cmdline`, where any process on the system could read
/// it.
fn run_with_password(args: &[&str], password: &str, what: &str) -> Result<()> {
    let mut child = Command::new("sudo")
        .arg("veracrypt")
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run veracrypt to {what}"))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open veracrypt stdin")?;
        // VeraCrypt reads one newline-terminated line per password prompt.
        stdin
            .write_all(format!("{password}\n").as_bytes())
            .with_context(|| format!("failed to send password to veracrypt to {what}"))?;
    }

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for veracrypt to {what}"))?;
    if !status.success() {
        // The overwhelmingly common cause is sudo having nothing to work with:
        // no cached ticket and no terminal to prompt on.
        if !sudo_is_primed() {
            bail!(
                "veracrypt failed to {what} — could not get root.\n\
                 Container operations need sudo. Run this from a terminal, or \
                 authenticate first with:\n    sudo -v"
            );
        }
        bail!(
            "veracrypt failed to {what} (exit {})",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// Give `path` back to the invoking user after a root-run veracrypt created it.
fn chown_to_user(path: &Path) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let status = Command::new("sudo")
        .arg("chown")
        .arg(format!("{uid}:{gid}"))
        .arg(path)
        .status()
        .context("failed to run sudo chown")?;
    if !status.success() {
        bail!("failed to take ownership of {}", path.display());
    }
    Ok(())
}

/// Create a new container file of `size_bytes` at `path`, formatted with ext4.
///
/// Formatting runs `mkfs` on a loop device, so this prompts for sudo.
pub fn create(path: &Path, size_bytes: u64, password: &str) -> Result<()> {
    if !available() {
        return Err(missing_binary_error());
    }
    if path.exists() {
        bail!("container already exists: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let size = size_bytes.to_string();
    let path_str = path.to_string_lossy().into_owned();
    run_with_password(
        &[
            "--text",
            "--create",
            &path_str,
            "--size",
            &size,
            "--encryption",
            ENCRYPTION,
            "--hash",
            HASH,
            "--filesystem",
            FILESYSTEM,
            "--volume-type",
            "normal",
            "--pim",
            "0",
            "--keyfiles",
            "",
            // Without an explicit random source VeraCrypt's text mode asks the
            // user to type random keystrokes to seed its pool. The kernel CSPRNG
            // is a better source and needs no interaction.
            "--random-source",
            "/dev/urandom",
            "--stdin",
            "--non-interactive",
            // A new file container has no meaningful previous contents to wipe,
            // so skip the full overwrite pass; it would write the container's
            // entire size to disk for no security benefit.
            "--quick",
        ],
        password,
        "create the container",
    )
    .inspect_err(|_| {
        // A failed create can leave a partial file behind; drop it so a retry
        // isn't blocked by the "container already exists" check above. It may
        // be root-owned, so remove it with the same privileges that made it.
        if path.exists() {
            let _ = Command::new("sudo").arg("rm").arg("-f").arg(path).status();
        }
    })?;

    // veracrypt ran as root, so the container file belongs to root and the user
    // couldn't even open it. Hand it back.
    chown_to_user(path)
}

/// Mount `app_name`'s container over its app directory.
///
/// Prompts for sudo. No-op if it is already mounted there.
pub fn mount(app_name: &str, password: &str) -> Result<()> {
    if !available() {
        return Err(missing_binary_error());
    }
    if is_mounted(app_name)? {
        return Ok(());
    }
    let container = container_path(app_name)?;
    if !container.exists() {
        bail!(
            "no container for '{app_name}' at {}",
            container.display()
        );
    }
    let mount_point = crate::manifest::app_dir(app_name)?;
    // The mount point must exist before mounting. It normally does — it holds
    // the .encrypted.toml marker that records the app while it is locked.
    std::fs::create_dir_all(&mount_point)
        .with_context(|| format!("failed to create mount point {}", mount_point.display()))?;

    let container_str = container.to_string_lossy().into_owned();
    let mount_str = mount_point.to_string_lossy().into_owned();
    run_with_password(
        &[
            "--text",
            "--mount",
            &container_str,
            &mount_str,
            "--pim",
            "0",
            "--keyfiles",
            "",
            "--protect-hidden",
            "no",
            "--stdin",
            "--non-interactive",
        ],
        password,
        &format!("mount '{app_name}'"),
    )?;

    ensure_owner_writable(&mount_point)?;
    Ok(())
}

/// Unmount `app_name`'s container. Prompts for sudo. No-op if not mounted.
pub fn dismount(app_name: &str) -> Result<()> {
    if !available() || !is_mounted(app_name)? {
        return Ok(());
    }
    let container = container_path(app_name)?;
    let container_str = container.to_string_lossy().into_owned();
    // Unmounting needs root for the same reason mounting does.
    let status = Command::new("sudo")
        .arg("veracrypt")
        .args(["--text", "--dismount", &container_str, "--non-interactive"])
        .status()
        .with_context(|| format!("failed to run veracrypt to unmount '{app_name}'"))?;
    if !status.success() {
        if !sudo_is_primed() {
            bail!(
                "failed to unmount '{app_name}' — could not get root. Run this from a \
                 terminal, or authenticate first with:\n    sudo -v"
            );
        }
        bail!(
            "failed to unmount '{app_name}' — is a program still using files inside it? \
             Close the app and try again"
        );
    }
    Ok(())
}

/// Make sure the mounted filesystem is writable by the current user.
///
/// `mkfs.ext4` runs as root, so a freshly created container's root directory is
/// owned by root and the unprivileged user cannot write into it. ext4 has no
/// `uid=` mount option (unlike FAT/NTFS), so ownership has to be corrected once,
/// on the empty filesystem, with a single chown.
fn ensure_owner_writable(mount_point: &Path) -> Result<()> {
    // Cheapest possible probe: try to create and remove a file.
    let probe = mount_point.join(".wryayer-write-probe");
    let writable = match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(e) if e.kind() != std::io::ErrorKind::PermissionDenied => {
            return Err(e).with_context(|| {
                format!("failed to write inside container at {}", mount_point.display())
            })
        }
        Err(_) => false,
    };

    if !writable {
        chown_to_user(mount_point).with_context(|| {
            format!(
                "the container is mounted at {} but is not writable",
                mount_point.display()
            )
        })?;
    }
    // Checked on every mount, not just the first: the mount point itself keeps
    // its ownership across remounts, so a container created before this was
    // handled would otherwise never get its lost+found fixed.
    take_ownership_of_lost_found(mount_point);
    Ok(())
}

/// Hand ext4's `lost+found` to the invoking user.
///
/// `mkfs.ext4` creates it as root with mode 0700, which leaves one unreadable
/// directory inside an otherwise user-owned tree. That breaks every consumer
/// that walks the app tree — `export` aborts its whole archive on the failed
/// `read_dir`, and size accounting silently under-reports. Everything else
/// assumes the tree is fully owned by the user, so restore that invariant here
/// rather than teaching each walker about this one directory.
///
/// Best-effort: a container that somehow lacks it is fine, and failing to chown
/// it must not block an otherwise good mount.
fn take_ownership_of_lost_found(mount_point: &Path) {
    let lf = mount_point.join("lost+found");
    if !lf.exists() {
        return;
    }
    // Already ours (a container mounted before) — nothing to do.
    if std::fs::read_dir(&lf).is_ok() {
        return;
    }
    let _ = chown_to_user(&lf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_mounted_volume_line() {
        let out = "1: /home/u/.wryayer/.containers/app.hc /dev/mapper/veracrypt1 /home/u/.wryayer/app\n";
        let v = parse_list(out);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].volume, "/home/u/.wryayer/.containers/app.hc");
        assert_eq!(v[0].mapper, "/dev/mapper/veracrypt1");
        assert_eq!(v[0].mount_point.as_deref(), Some("/home/u/.wryayer/app"));
    }

    #[test]
    fn treats_dash_mount_point_as_not_mounted() {
        let v = parse_list("2: /vol/x.hc /dev/mapper/veracrypt2 -\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].mount_point, None);
    }

    #[test]
    fn parses_multiple_volumes_and_ignores_noise() {
        let out = "\
1: /a.hc /dev/mapper/veracrypt1 /mnt/a
garbage without a colon
2: /b.hc /dev/mapper/veracrypt2 /mnt/b
";
        let v = parse_list(out);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].mount_point.as_deref(), Some("/mnt/a"));
        assert_eq!(v[1].volume, "/b.hc");
    }

    #[test]
    fn empty_list_output_yields_no_volumes() {
        assert!(parse_list("").is_empty());
        assert!(parse_list("No volumes mounted.\n").is_empty());
    }

    #[test]
    fn size_never_drops_below_the_floor() {
        // Minimum headroom (512 MiB) plus ext4 overhead (128 MiB).
        assert_eq!(recommended_size(0), 640 * MIB);
        // Anything non-empty rounds up to the next 128 MiB step.
        assert_eq!(recommended_size(1024), 768 * MIB);
        assert!(recommended_size(u64::from(u32::MAX)) >= 640 * MIB);
    }

    #[test]
    fn size_leaves_room_to_grow() {
        // A tree always fits with room to spare, at every scale.
        for used in [10 * MIB, 100 * MIB, 500 * MIB, 2 * GIB, 10 * GIB, 50 * GIB] {
            let size = recommended_size(used);
            assert!(size > used, "container {size} must exceed tree {used}");
            assert!(
                size - used >= 512 * MIB,
                "tree {used} got only {} headroom",
                size - used
            );
        }
    }

    #[test]
    fn size_stays_proportionate_for_large_trees() {
        // Headroom is capped, so a big app doesn't get a wildly oversized file.
        let used = 20 * GIB;
        let size = recommended_size(used);
        assert!(
            size < used * 2,
            "container {size} is more than double the tree {used}"
        );
    }

    #[test]
    fn size_is_rounded_to_whole_units() {
        for used in [0, 1, 12345, 999 * MIB, 3 * GIB] {
            assert_eq!(recommended_size(used) % (128 * MIB), 0);
        }
    }
}
