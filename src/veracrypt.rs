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

/// Whether `app_name`'s container is currently mounted at its app directory,
/// i.e. whether its files are reachable right now.
pub fn is_mounted(app_name: &str) -> Result<bool> {
    let target = crate::manifest::app_dir(app_name)?;
    Ok(mounted_at(&list_mounted()?, &target.to_string_lossy()))
}

/// Whether any volume is mounted at `dir`.
fn mounted_at(volumes: &[MountedVolume], dir: &str) -> bool {
    volumes.iter().any(|v| v.mount_point.as_deref() == Some(dir))
}

/// The slot holding `container` open, mounted or not.
fn slot_for<'a>(volumes: &'a [MountedVolume], container: &str) -> Option<&'a MountedVolume> {
    volumes.iter().find(|v| v.volume == container)
}

/// The slot VeraCrypt holds open for `app_name`'s container, if any —
/// regardless of whether its filesystem is mounted anywhere.
///
/// These two states come apart. A volume can be attached to a device-mapper
/// node with no mount point at all: an unmount that did not release the slot, a
/// sandbox killed while it held the tree busy, a manual `umount`. Nothing is
/// readable in that state, so [`is_mounted`] rightly says no — but VeraCrypt
/// still refuses to mount a container it already has open, so asking it to
/// mount fails with "the volume is already mounted", which reads like a
/// contradiction and leaves the app unlaunchable until the slot is cleared by
/// hand.
pub fn attached(app_name: &str) -> Result<Option<MountedVolume>> {
    let container = container_path(app_name)?;
    Ok(slot_for(&list_mounted()?, &container.to_string_lossy()).cloned())
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

/// How full a container's filesystem is.
///
/// `used + available` is deliberately *not* `total`: ext4 reserves a slice for
/// root that an unprivileged process can never write into. Reporting a
/// container as 95% full while `df` says 100% would be worse than useless right
/// when the user needs to act, so the percentage below is computed the way `df`
/// computes its Use% — over what is actually reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// Bytes occupied by files.
    pub used: u64,
    /// Bytes an unprivileged process may still write.
    pub available: u64,
    /// The filesystem's nominal size, reserve included.
    pub total: u64,
}

impl Usage {
    /// Percentage of the reachable space that is in use, as `df` reports it.
    pub fn percent_used(&self) -> u64 {
        let reachable = self.used + self.available;
        if reachable == 0 {
            return 0;
        }
        // Round up, so "99%" never appears for a filesystem with nothing left.
        (self.used * 100).div_ceil(reachable)
    }
}

/// Space accounting for the filesystem mounted at `app_name`'s directory, or
/// `None` if it can't be queried.
///
/// Only meaningful once the container is mounted: `statvfs` on an unmounted
/// mount point describes whatever filesystem `~/.wryayer` sits on, which is a
/// plausible-looking wrong answer. Callers know their own mount state —
/// [`is_mounted`] costs a fork, so this doesn't pay for one on every call.
pub fn usage(app_name: &str) -> Option<Usage> {
    let dir = crate::manifest::app_dir(app_name).ok()?;
    let c_path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: c_path is a valid NUL-terminated string; statvfs only writes to sv.
    let sv = unsafe {
        let mut sv: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut sv) != 0 {
            return None;
        }
        sv
    };
    let frsize = sv.f_frsize as u64;
    Some(Usage {
        used: (sv.f_blocks as u64).saturating_sub(sv.f_bfree as u64) * frsize,
        available: sv.f_bavail as u64 * frsize,
        total: sv.f_blocks as u64 * frsize,
    })
}

/// Free bytes inside `app_name`'s mounted container. Same caveat as [`usage`]:
/// the caller is the one that knows whether the container is mounted.
pub fn free_space(app_name: &str) -> Option<u64> {
    usage(app_name).map(|u| u.available)
}

/// How full a container has to get before wryayer says so unprompted.
///
/// Growing a container costs a full copy of the volume, so the warning has to
/// arrive while there is still room to work — not at the point where the app
/// has already failed to write.
pub const FULL_WARN_PERCENT: u64 = 90;

/// How much a compressed package archive is assumed to expand to on disk.
///
/// Deliberately generous — running out of space *inside* a container mid-way
/// through an extraction is far more painful than briefly over-reserving, since
/// growing afterwards costs a full copy of the volume.
const ARCHIVE_EXPANSION: u64 = 4;

/// Free space always kept spare inside a container, so an app has somewhere to
/// write at runtime even straight after a big install.
const FREE_SPACE_MARGIN: u64 = 512 * MIB;

/// Bytes needed inside a container to safely unpack `archive_bytes` of packages.
pub fn space_needed_for(archive_bytes: u64) -> u64 {
    archive_bytes * ARCHIVE_EXPANSION + FREE_SPACE_MARGIN
}

/// Keeps an encrypted container from filling up while packages are added to it.
///
/// A single up-front check is not enough: the soname-satisfy loop discovers and
/// extracts further packages *after* the initial set is sized, and those can be
/// far larger than the package the user asked for (a missing `libGL` pulls in
/// the whole graphics driver). Handing this guard down means every extraction
/// reserves its own space first.
pub struct SpaceGuard<'a> {
    pub app: &'a str,
    pub password: &'a str,
}

impl SpaceGuard<'_> {
    /// Make room for `archive_bytes` about to be unpacked, growing the
    /// container if it would otherwise run out.
    pub fn reserve(&self, archive_bytes: u64) -> Result<()> {
        ensure_room_for(self.app, archive_bytes, self.password)
    }
}

/// Grow `app_name`'s container so it can hold `archive_bytes` of new packages.
///
/// A no-op when there is already room. VeraCrypt cannot resize a volume in
/// place, so growing means creating a larger container, copying the contents
/// across and swapping the files — expensive, hence the generous headroom used
/// when containers are first created.
///
/// The old container is only deleted once the new one holds a verified copy, so
/// an interruption leaves the original intact.
pub fn ensure_room_for(app_name: &str, archive_bytes: u64, password: &str) -> Result<()> {
    let needed = space_needed_for(archive_bytes);
    let Some(free) = free_space(app_name) else {
        // Not mounted, or an unqueryable filesystem: nothing sensible to do.
        return Ok(());
    };
    if free >= needed {
        return Ok(());
    }

    let container = container_path(app_name)?;
    let current = std::fs::metadata(&container)
        .with_context(|| format!("failed to stat {}", container.display()))?
        .len();
    // Add what's missing, plus the same headroom rule used at creation, so a
    // series of merge installs doesn't grow the volume once per install.
    let shortfall = needed - free;
    let new_size = round_up(current + shortfall + shortfall / 2, 128 * MIB);

    println!(
        "Container for '{app_name}' needs more room ({} free, {} required) — growing to {}.",
        human(free),
        human(needed),
        human(new_size)
    );
    println!("This copies the whole container and may take a while.");

    grow(app_name, new_size, password)
}

/// Rebuild `app_name`'s container at `new_size`, preserving its contents.
pub fn grow(app_name: &str, new_size: u64, password: &str) -> Result<()> {
    let container = container_path(app_name)?;
    let bigger = container.with_extension("hc.growing");
    let mount_point = crate::manifest::app_dir(app_name)?;
    let staging_mount = containers_dir()?.join(format!(".{app_name}.grow-mnt"));

    // Start clean: a leftover from an interrupted growth is worthless, since the
    // original container was never touched.
    let _ = std::fs::remove_file(&bigger);
    let _ = std::fs::remove_dir_all(&staging_mount);

    create(&bigger, new_size, password)?;
    std::fs::create_dir_all(&staging_mount)
        .with_context(|| format!("failed to create {}", staging_mount.display()))?;

    let result = (|| -> Result<()> {
        mount_at(&bigger, &staging_mount, password)?;
        crate::commands::encrypt::copy_tree_public(&mount_point, &staging_mount)?;
        Ok(())
    })();

    // Whatever happened, unmount the new volume before touching the files.
    let _ = dismount_path(&bigger);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&bigger);
        let _ = std::fs::remove_dir_all(&staging_mount);
        return Err(e);
    }

    // The copy is complete and unmounted: swap the containers. The app's own
    // volume has to come down first so its file can be replaced.
    dismount(app_name)?;
    std::fs::rename(&bigger, &container).with_context(|| {
        format!(
            "failed to replace {} with the grown container at {}",
            container.display(),
            bigger.display()
        )
    })?;
    let _ = std::fs::remove_dir_all(&staging_mount);

    // Put it back the way we found it: mounted.
    mount(app_name, password)?;
    println!("Container for '{app_name}' grown to {}.", human(new_size));
    Ok(())
}

/// Mount an arbitrary container file at an arbitrary directory.
fn mount_at(container: &Path, mount_point: &Path, password: &str) -> Result<()> {
    let c = container.to_string_lossy().into_owned();
    let m = mount_point.to_string_lossy().into_owned();
    run_with_password(
        &[
            "--text", "--mount", &c, &m, "--pim", "0", "--keyfiles", "",
            "--protect-hidden", "no", "--stdin", "--non-interactive",
        ],
        password,
        "mount the new container",
    )?;
    ensure_owner_writable(mount_point)
}

/// Unmount by container path (used for volumes not tied to an app directory).
fn dismount_path(container: &Path) -> Result<()> {
    let c = container.to_string_lossy().into_owned();
    let status = crate::prompt::sudo()
        .arg("veracrypt")
        .args(["--text", "--dismount", &c, "--non-interactive"])
        .status()
        .context("failed to run veracrypt to unmount")?;
    if !status.success() {
        bail!("failed to unmount {}", container.display());
    }
    Ok(())
}

/// Compact human-readable byte count, for the messages above.
fn human(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else {
        format!("{} MB", bytes / MIB)
    }
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

/// How often to refresh the sudo ticket. Comfortably under any usable
/// `timestamp_timeout`, including the short ones some systems set.
const SUDO_REFRESH: std::time::Duration = std::time::Duration::from_secs(60);

/// Hold the cached sudo credentials open for as long as this process runs.
///
/// sudo forgets an authentication after a few minutes. Installing an app into a
/// new container easily outlives that: the password is collected up front, then
/// hundreds of packages are downloaded and extracted, and only afterwards does
/// the container get created and mounted. By then the ticket has lapsed, so
/// sudo asks again — on `/dev/tty`, which for a TUI- or GUI-spawned child is
/// either absent or owned by the front-end. The prompt cannot be answered and
/// the install fails at the last step, after all the work.
///
/// `sudo -n -v` refreshes the ticket without needing the password again, so a
/// slow tick is enough to keep it from ever expiring mid-operation.
///
/// Deliberately not started by [`prime_sudo`] itself. It belongs to the process
/// that is *doing* the work and exits when that work is done — the install
/// child, not the TUI. A front-end that started it would hold root open for as
/// long as the user left the interface running, which is precisely what
/// `timestamp_timeout` exists to prevent.
pub fn keep_sudo_alive() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return; // already ticking
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(SUDO_REFRESH);
        // -n so a ticket that lapsed anyway fails here quietly, rather than
        // blocking this thread forever on a prompt nobody is watching.
        let _ = Command::new("sudo")
            .args(["-n", "-v"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
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
    let mut child = crate::prompt::sudo()
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
    let status = crate::prompt::sudo()
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
            let _ = crate::prompt::sudo().arg("rm").arg("-f").arg(path).status();
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
    // Clear a slot left open without a mount point before asking VeraCrypt for
    // a new one — it would otherwise refuse, having the container open already.
    if let Some(stale) = attached(app_name)? {
        eprintln!(
            "note: '{app_name}' was still attached at {} without being mounted — releasing it",
            stale.mapper
        );
        dismount_path(&container).with_context(|| {
            format!("failed to release the stale volume slot for '{app_name}'")
        })?;
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
    // Keyed on the slot rather than on the mount point: a volume left attached
    // with nothing mounted is exactly the state that needs clearing, and
    // guarding on `is_mounted` made this a no-op precisely then.
    if !available() || attached(app_name)?.is_none() {
        return Ok(());
    }
    let container = container_path(app_name)?;
    let container_str = container.to_string_lossy().into_owned();
    // Unmounting needs root for the same reason mounting does.
    let status = crate::prompt::sudo()
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
    fn fill_is_measured_against_reachable_space_not_nominal_size() {
        // ext4 reserves a slice for root. A container 100% full as far as the
        // app is concerned still has that reserve free, and reporting 95% at
        // the moment writes start failing would be actively misleading.
        let u = Usage { used: 950, available: 0, total: 1000 };
        assert_eq!(u.percent_used(), 100);
    }

    #[test]
    fn fill_rounds_up_so_full_never_reads_as_nearly_full() {
        // 1 byte left out of a million must not round down to 99%.
        let u = Usage { used: 999_999, available: 1, total: 1_000_000 };
        assert_eq!(u.percent_used(), 100);
    }

    #[test]
    fn an_empty_container_is_zero_percent() {
        let u = Usage { used: 0, available: 1024, total: 1024 };
        assert_eq!(u.percent_used(), 0);
    }

    #[test]
    fn a_container_with_no_reachable_space_does_not_divide_by_zero() {
        let u = Usage { used: 0, available: 0, total: 0 };
        assert_eq!(u.percent_used(), 0);
    }

    #[test]
    fn half_full_is_half_full() {
        let u = Usage { used: 512, available: 512, total: 1024 };
        assert_eq!(u.percent_used(), 50);
    }

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

    /// Real `veracrypt --list` output from the failure: a container still held
    /// open on a device-mapper node, with nothing mounted anywhere.
    const HALF_ATTACHED: &str = "\
1: /home/u/.llms/root.dat /dev/mapper/veracrypt1 /home/u/.wryayer
3: /home/u/.wryayer/.containers/app.hc /dev/mapper/veracrypt3 -
";

    #[test]
    fn a_volume_attached_without_a_mount_point_is_not_mounted() {
        let v = parse_list(HALF_ATTACHED);
        assert!(!mounted_at(&v, "/home/u/.wryayer/app"), "nothing is readable there");
    }

    #[test]
    fn a_volume_attached_without_a_mount_point_still_holds_its_slot() {
        // The whole bug: the slot exists, so VeraCrypt refuses a fresh mount,
        // while every mount-point test says the app is locked. Releasing it has
        // to key on this, not on the mount point.
        let v = parse_list(HALF_ATTACHED);
        let slot = slot_for(&v, "/home/u/.wryayer/.containers/app.hc")
            .expect("the container is open and must be found");
        assert_eq!(slot.mapper, "/dev/mapper/veracrypt3");
        assert_eq!(slot.mount_point, None);
    }

    #[test]
    fn a_container_that_was_never_opened_holds_no_slot() {
        let v = parse_list(HALF_ATTACHED);
        assert!(slot_for(&v, "/home/u/.wryayer/.containers/other.hc").is_none());
    }

    #[test]
    fn a_properly_mounted_container_is_both_mounted_and_attached() {
        let v = parse_list(
            "1: /home/u/.wryayer/.containers/app.hc /dev/mapper/veracrypt1 /home/u/.wryayer/app\n",
        );
        assert!(mounted_at(&v, "/home/u/.wryayer/app"));
        assert!(slot_for(&v, "/home/u/.wryayer/.containers/app.hc").is_some());
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
    fn space_needed_covers_expansion_and_a_margin() {
        // Archives expand when unpacked, and an app needs somewhere to write
        // afterwards — so the requirement must exceed the archive size by a lot.
        let archives = 500 * MIB;
        let needed = space_needed_for(archives);
        assert!(needed > archives * 3, "{needed} is not enough for {archives}");
        // Even a trivial install reserves the runtime margin.
        assert!(space_needed_for(0) >= 512 * MIB);
    }

    #[test]
    fn size_is_rounded_to_whole_units() {
        for used in [0, 1, 12345, 999 * MIB, 3 * GIB] {
            assert_eq!(recommended_size(used) % (128 * MIB), 0);
        }
    }
}
