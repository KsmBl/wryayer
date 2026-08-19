//! Session/system-bus and desktop-portal plumbing for the sandbox: the filtered
//! xdg-dbus-proxy, the per-app Avahi stub, the cross-container portal listener,
//! and the avahi-daemon bring-up. Split out of the launcher for navigability.
use super::short_hash;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The statically-linked portal client (csrc/portal_client.c), embedded at
/// build time. Symlinked into a sandbox under each bound app's name so that
/// running e.g. `firefox <url>` there forwards the request to the host portal
/// listener. Empty when the build had no C compiler / no static libc, in which
/// case cross-container app binding is silently unavailable.
pub(super) const PORTAL_HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wryayer-portal"));

/// Start avahi-daemon if it's installed but not currently running, so sandboxed
/// apps that query Avahi over the system bus don't fail with "Daemon not
/// running". avahi-client prints that same message whether the daemon is absent
/// or the system bus is unreachable, so the only way to silence it is a live
/// daemon. Entirely best-effort: if the unit is missing, already active, or the
/// user's polkit rules don't permit an unprivileged start, we leave things as
/// they are and the harmless warning simply remains.
pub(super) fn ensure_avahi_daemon() {
    // Nothing to start if the service isn't installed.
    if !Path::new("/usr/lib/systemd/system/avahi-daemon.service").exists() {
        return;
    }
    // Skip if it's already running (the common case after the first launch).
    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", "avahi-daemon"])
        .status();
    if matches!(active, Ok(s) if s.success()) {
        return;
    }
    // Try an unprivileged start first (many desktops authorize this via polkit),
    // then fall back to a non-interactive sudo in case the user has cached
    // credentials. Both are best-effort; failures are ignored on purpose.
    let started = Command::new("systemctl")
        .args(["start", "avahi-daemon"])
        .status();
    if matches!(started, Ok(s) if s.success()) {
        return;
    }
    let _ = Command::new("sudo")
        .args(["-n", "systemctl", "start", "avahi-daemon"])
        .status();
}

/// Map a shared-dir path to the XDG role its basename represents, so a
/// synthetic user-dirs.dirs can list only the shared roles.  Unshared roles
/// disappear from the file-picker sidebar rather than appearing as broken
/// clickable shortcuts.
pub(super) fn xdg_role_for_dir(path: &str) -> Option<&'static str> {
    let name = std::path::Path::new(path).file_name()?.to_str()?;
    match name {
        "Desktop"   => Some("XDG_DESKTOP_DIR"),
        "Downloads" => Some("XDG_DOWNLOAD_DIR"),
        "Documents" => Some("XDG_DOCUMENTS_DIR"),
        "Music"     => Some("XDG_MUSIC_DIR"),
        "Pictures"  => Some("XDG_PICTURES_DIR"),
        "Videos"    => Some("XDG_VIDEOS_DIR"),
        "Templates" => Some("XDG_TEMPLATES_DIR"),
        "Public"    => Some("XDG_PUBLICSHARE_DIR"),
        _ => None,
    }
}

/// Spawn an xdg-dbus-proxy that mirrors the host session bus at `socket_path`
/// with the desktop portal filtered out.  In `--filter` mode the proxy's
/// default policy makes every name invisible, so we allow-list the session
/// services sandboxed GUI apps commonly use and simply never grant the portal
/// names — leaving `org.freedesktop.portal.*` unreachable.  With no visible
/// portal, GTK/Qt/Firefox/Chromium fall back to their in-sandbox file choosers,
/// which honour the XDG overlays and can only browse mounted dirs.
///
/// The proxy is given PR_SET_PDEATHSIG so it dies with its parent even on the
/// exec() retry path where nobody is left to kill it explicitly.
pub(super) fn spawn_dbus_proxy(host_bus: &str, socket_path: &str) -> Option<std::process::Child> {
    // A stale socket from a previous run would make the proxy's bind() fail.
    let _ = std::fs::remove_file(socket_path);

    let mut proxy = Command::new("xdg-dbus-proxy");
    proxy.arg(host_bus).arg(socket_path).arg("--filter");
    for name in &[
        "org.freedesktop.Notifications",         // desktop notifications
        "org.freedesktop.secrets",               // keyring (saved passwords)
        "org.freedesktop.ScreenSaver",           // inhibit idle during playback
        "org.freedesktop.PowerManagement",       // ditto, older spec
        "org.freedesktop.FileManager1",          // "show in file manager"
        "org.a11y.Bus",                          // accessibility bridge
        "org.kde.StatusNotifierWatcher",         // tray icons
        "org.freedesktop.StatusNotifierWatcher",
        "ca.desrt.dconf",                        // GSettings/dconf backend
        "org.gtk.vfs.*",                         // GVFS mounts
    ] {
        proxy.arg(format!("--talk={name}"));
    }
    // Apps register their own MPRIS name to expose media controls.
    proxy.arg("--own=org.mpris.MediaPlayer2.*");
    // Firefox (and Thunderbird) remote to an already-running instance over the
    // session bus via `org.mozilla.<app>.<profile-hash>`. A bound app opening a
    // link spawns a second `wryayer run firefox`; sharing the same profile and
    // this bus name lets it hand the URL to the running browser as a new tab
    // instead of colliding with the profile lock ("Firefox is already running,
    // but is not responding"). Without this the `--filter` proxy hides the name
    // and remoting silently fails. Owning implies talk, so the second instance
    // can also reach the name the first one owns on the real bus.
    proxy.arg("--own=org.mozilla.*");
    // Steam's pressure-vessel launcher-service owns names under its own
    // namespace so it can place launched games into the right runtime; without
    // this it crash-loops ("Unable to acquire bus name …") and Steam disables
    // it, breaking game launches. These are Steam's own names, not the portal.
    proxy.arg("--own=com.steampowered.*");

    proxy
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        proxy.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }

    let mut child = proxy.spawn().ok()?;

    // Wait (up to ~1s) for the proxy socket to appear before bwrap binds it.
    for _ in 0..40 {
        if std::path::Path::new(socket_path).exists() {
            return Some(child);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    // Never came up — don't point the sandbox at a dead bus.
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Bring up the per-sandbox Avahi stub (see `avahi_stub.rs`): a private system
/// bus plus an in-process owner of `org.freedesktop.Avahi`, so avahi-client apps
/// don't fail with "Daemon not running" — without starting the host daemon or
/// putting anything on the network.  Returns the managed child (which owns the
/// dbus-daemon) and the host path of the bus socket, or None if it didn't come
/// up in time.
///
/// The bus socket, its dbus-daemon config, and the readiness marker all live in
/// the app's own `.spoof` dir under `~/.wryayer`, so nothing identifying is
/// written outside the container.  The child carries PR_SET_PDEATHSIG so it (and
/// its dbus-daemon) die with the sandbox even on the exec() retry path.
pub(super) fn spawn_avahi_stub(spoof_dir: &Path) -> Option<(std::process::Child, String)> {
    // AF_UNIX socket paths are capped at ~108 bytes. A deeply nested app dir can
    // overflow that, and dbus-daemon then silently fails to bind, leaving the
    // stub disabled. When the in-container path is too long, fall back to a short
    // hashed name in the runtime dir (tmpfs — the name is a hash and the file is
    // ephemeral, so nothing identifying persists outside ~/.wryayer).
    let mut sock = spoof_dir.join(".avahi-bus");
    if sock.as_os_str().len() > 100 {
        let rt = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
        sock = PathBuf::from(rt).join(format!(".wrav-{:x}", short_hash(spoof_dir)));
    }
    let conf = spoof_dir.join(".avahi-bus.conf");
    let sock_str = sock.to_str()?.to_string();
    let conf_str = conf.to_str()?.to_string();
    let ready = format!("{sock_str}.ready");

    // A leftover socket makes dbus-daemon's bind() fail; a leftover marker would
    // make us treat the bus as up before it is.
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&ready);

    let config = format!(
        "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
           \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
         <busconfig>\n\
         \x20 <type>system</type>\n\
         \x20 <listen>unix:path={sock_str}</listen>\n\
         \x20 <auth>EXTERNAL</auth>\n\
         \x20 <policy context=\"default\">\n\
         \x20   <allow user=\"*\"/>\n\
         \x20   <allow own=\"*\"/>\n\
         \x20   <allow send_type=\"method_call\"/>\n\
         \x20   <allow send_type=\"method_return\"/>\n\
         \x20   <allow send_type=\"error\"/>\n\
         \x20   <allow send_type=\"signal\"/>\n\
         \x20   <allow send_requested_reply=\"true\"/>\n\
         \x20   <allow receive_requested_reply=\"true\"/>\n\
         \x20   <allow receive_type=\"method_call\"/>\n\
         \x20   <allow receive_type=\"method_return\"/>\n\
         \x20   <allow receive_type=\"error\"/>\n\
         \x20   <allow receive_type=\"signal\"/>\n\
         \x20 </policy>\n\
         </busconfig>\n"
    );
    if std::fs::write(&conf, config).is_err() {
        return None;
    }

    let exe = std::env::current_exe().ok()?;
    let mut c = Command::new(exe);
    c.arg("avahi-stub").arg(&sock_str).arg(&conf_str);
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = c.spawn().ok()?;

    // Wait (up to ~3 s) for the stub to actually own the name — it writes the
    // marker only after RequestName returns — so the app never races an
    // unowned bus and sees a spurious "Daemon not running".
    for _ in 0..200 {
        if Path::new(&ready).exists() {
            return Some((child, sock_str));
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Pick which bound app should handle generic URL/file openers (`xdg-open`,
/// `x-www-browser`, …). Prefer a bound app whose name looks like a web browser;
/// otherwise fall back to the first bound app. None when nothing is bound.
pub(super) fn pick_open_app(bound: &[String]) -> Option<&str> {
    const BROWSER_HINTS: &[&str] = &[
        "firefox", "librewolf", "waterfox", "chrom", "chromium", "brave",
        "vivaldi", "opera", "edge", "epiphany", "falkon", "qutebrowser",
        "midori", "zen", "tor-browser", "torbrowser", "min",
    ];
    bound.iter()
        .find(|a| {
            let low = a.to_ascii_lowercase();
            BROWSER_HINTS.iter().any(|h| low.contains(h))
        })
        .or_else(|| bound.first())
        .map(String::as_str)
}

/// Bring up the host-side portal listener for cross-container app binding.
/// The socket lives under the app's isolated runtime dir (bind-mounted through
/// /run, so the same absolute path is valid inside the sandbox). Returns the
/// managed child — carrying PR_SET_PDEATHSIG so it dies with the sandbox even
/// on the exec() retry path — and the socket path, or None if it didn't come up.
pub(super) fn spawn_portal_listener(
    isolated_rt: &str,
    allowed: &[String],
) -> Option<(std::process::Child, String)> {
    let sock = format!("{isolated_rt}/portal.sock");
    let ready = format!("{sock}.ready");
    // Stale files would make bind() fail or fake an early readiness signal.
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&ready);

    let exe = std::env::current_exe().ok()?;
    let mut c = Command::new(exe);
    c.arg("portal-listener").arg(&sock).arg(allowed.join(","));
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = c.spawn().ok()?;

    // Wait (up to ~2 s) for the listener to create the socket before the sandbox
    // app can try to connect through the helper.
    for _ in 0..80 {
        if Path::new(&ready).exists() {
            return Some((child, sock));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Where the portal shims live inside a sandbox: one symlink per bound app,
/// all pointing at the static helper, on a private tmpfs at the front of PATH.
pub(super) const SHIM_DIR: &str = "/.wryayer-bin";

/// Where the generated desktop entries for the bound apps are mounted inside a
/// sandbox — an XDG data directory holding nothing but `applications/`.
pub(super) const SHARE_DIR: &str = "/.wryayer-share";

/// Write the desktop-entry tree that makes bound apps *findable* inside the
/// sandbox, and return its host path.
///
/// The shims on PATH only help an app that runs `firefox` by name. Everything
/// else — Thunderbird opening a link, a file manager's "Open with", any
/// GIO/GTK `g_app_info_launch_default_for_uri` — looks for a `.desktop` file
/// claiming the MIME type instead, and finds none inside a container that only
/// holds one app. So generate one entry per bound app, each claiming what that
/// app's own package declares it can open, plus the `mimeapps.list` naming the
/// link handler as the default.
///
/// Lives in the app's `.spoof` dir like the other generated files, so nothing
/// identifying is written outside `~/.wryayer`. Returns None if it cannot be
/// written, in which case the caller simply leaves it out.
pub(super) fn write_bound_app_entries(
    spoof_dir: &Path,
    app_name: &str,
    bound: &[String],
    open_app: Option<&str>,
) -> Option<String> {
    // The link handler goes first so it wins any MIME type two bound apps both
    // claim — it is the one the user pointed at links.
    let ordered = open_app
        .into_iter()
        .chain(bound.iter().map(String::as_str).filter(|a| Some(*a) != open_app));

    let mut apps = Vec::new();
    for name in ordered {
        let mut app = crate::desktop::bound_app(name);
        // A designated link handler that claims no scheme — because its
        // container is locked, or it ships no entry — still has to take links,
        // exactly as it already does for xdg-open.
        if Some(name) == open_app && !app.handles_links() {
            app.mime_types
                .extend(crate::desktop::LINK_MIME_TYPES.iter().map(|m| m.to_string()));
        }
        apps.push(app);
    }

    // What this app handles itself stays its own: a bound app is offered for
    // those types but never made the default for them.
    let reserved = crate::desktop::declared(app_name)
        .map(|(_, mimes)| mimes)
        .unwrap_or_default();

    // Rebuilt from scratch every run: an app unbound since the last one must
    // not keep answering for links.
    let dir = spoof_dir.join("portal-share");
    let _ = std::fs::remove_dir_all(&dir);
    for (rel, content) in crate::desktop::sandbox_entries(&apps, SHIM_DIR, &reserved) {
        let path = dir.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        std::fs::write(&path, content).ok()?;
    }
    Some(dir.to_str()?.to_string())
}

/// Prepend `first` to a colon-separated XDG search path, keeping what the host
/// set and making sure `required` (the in-container defaults) are on it — the
/// host's own entries can point at directories no sandbox has.
pub(super) fn xdg_path(var: &str, first: &str, required: &[&str]) -> String {
    let mut dirs = vec![first.to_string()];
    let inherited = std::env::var(var).unwrap_or_default();
    for dir in inherited.split(':').filter(|d| !d.is_empty()) {
        if !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_string());
        }
    }
    for dir in required {
        if !dirs.iter().any(|d| d == dir) {
            dirs.push((*dir).to_string());
        }
    }
    dirs.join(":")
}
