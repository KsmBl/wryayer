use crate::manifest::{app_dir, list_all_apps, wryayer_root};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum TempMode {
    /// Share the host /tmp (default)
    System,
    /// Private in-memory tmpfs per launch — discarded on close
    Ramdisk,
    /// Persistent per-app temp under ~/.wryayer/<app>/.tmp/
    Local,
    /// Private per-instance temp with a UUID name — deleted on close
    Uuid,
}

/// Colour theme for the interactive TUI (a global appearance preference).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Theme {
    /// The original cool palette: cyan accent on a dark-blue selection.
    Default,
    /// A warm amber palette.
    Amber,
    /// A green-phosphor palette (green body text, not white).
    Matrix,
}

/// Structural layout for the TUI, independent of the colour theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layout {
    /// Horizontal tab strip on top, single-line borders, solid-arrow cursor.
    Default,
    /// Vertical tab sidebar on the left, double-line borders, prompt cursor.
    Sidebar,
    /// Horizontal tab strip along the bottom, rounded borders, chevron cursor.
    Bottom,
}

/// How to satisfy sandboxed apps that probe Avahi/zeroconf at startup.
#[derive(Debug, Clone, PartialEq)]
pub enum AvahiMode {
    /// Give the sandbox a private system bus with an in-process stub that owns
    /// org.freedesktop.Avahi, so avahi-client succeeds without touching the host
    /// or advertising anything on the LAN (default). Everything it uses lives
    /// under ~/.wryayer/<app>/.
    Stub,
    /// Best-effort start of the host avahi-daemon if it's installed but stopped.
    Host,
    /// Do nothing; apps that probe Avahi print a harmless "Daemon not running".
    Off,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LocalDelete {
    /// Keep temp dir across restarts
    Never,
    /// Wipe temp on launch when no other instance of the app is running
    OnStart,
    /// Wipe temp when this instance closes
    OnClose,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub temp_mode: TempMode,
    pub temp_delete: LocalDelete,
    /// Allow outgoing network access inside bwrap (default: true)
    pub network: bool,
    /// How to answer sandboxed apps that probe Avahi at startup (default: Stub)
    pub avahi: AvahiMode,
    /// Allow access to /dev/video* camera devices (default: true)
    pub camera: bool,
    /// Allow ALSA capture devices + PipeWire/PulseAudio mic (default: true)
    pub microphone: bool,
    /// Allow ALSA playback + PipeWire/PulseAudio audio output (default: true)
    pub audio: bool,
    /// Host directories bind-mounted read-write inside the sandbox (default: none)
    pub shared_dirs: Vec<String>,
    /// Override /etc/hostname and $HOSTNAME inside the sandbox
    pub spoof_hostname: Option<String>,
    /// Override $USER and $LOGNAME inside the sandbox
    pub spoof_username: Option<String>,
    /// Override /etc/machine-id — "random" generates a fresh UUID each launch
    pub spoof_machine_id: Option<String>,
    /// Path to a file to bind over /proc/cpuinfo inside the sandbox
    pub spoof_cpuinfo: Option<String>,
    /// Override /etc/os-release inside the sandbox — "sample" uses a generic ID; any other value is used as the OS name
    pub spoof_os: Option<String>,
    /// Detect the real terminal emulator and pass it into the sandbox via TERM_PROGRAM
    /// so tools like fastfetch report the correct terminal instead of "bwrap".
    pub spoof_terminal: bool,
    /// Report a fake system uptime (in seconds) inside the sandbox — fools
    /// fastfetch's "Uptime", `uptime`/`w`, and any `sysinfo(2)`/CLOCK_BOOTTIME
    /// reader. None = show the real uptime.
    pub spoof_uptime: Option<u64>,
    /// Maximum RAM the app may use, in KiB — enforced via systemd-run (None = no limit).
    /// Stored in KiB so limits can be set in KB/MB/GB with full precision.
    pub ram_limit: Option<u64>,
    /// Whether to create a ~/bin shortcut by default when installing (global only)
    pub create_shortcut: bool,
    /// Whether the TUI shows the "Install '<pkg>'?" confirmation before an
    /// install. When false the install starts immediately (global only).
    pub confirm_install: bool,
    /// Whether the TUI asks about the ~/bin shortcut before installing. When
    /// false it silently applies `create_shortcut` instead (global only).
    pub ask_shortcut: bool,
    /// Whether to delete the shared download/build cache (~/.cache/wryayer)
    /// after each successful install (global only). Off by default.
    pub clean_cache: bool,
    /// Colour theme for the TUI (global only).
    pub theme: Theme,
    /// Structural layout for the TUI, independent of the colour theme (global only).
    pub layout: Layout,
    /// Route the sandbox's D-Bus session through a filter that hides the host
    /// desktop portal, so file pickers run in-sandbox and only show shared
    /// dirs instead of leaking host paths (default: true)
    pub portal_filter: bool,
    /// Names of other installed wryayer apps exposed inside this app's sandbox
    /// as host-delegated launchers. When the sandboxed app runs e.g. `firefox
    /// <url>`, the command is forwarded out to the host and re-launched as
    /// `wryayer run firefox -- <url>` in Firefox's own container (default: none).
    pub bound_apps: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            temp_mode: TempMode::System,
            temp_delete: LocalDelete::OnStart,
            network: true,
            avahi: AvahiMode::Stub,
            camera: true,
            microphone: true,
            audio: true,
            shared_dirs: Vec::new(),
            spoof_hostname: None,
            spoof_username: None,
            spoof_machine_id: None,
            spoof_cpuinfo: None,
            spoof_os: None,
            spoof_terminal: false,
            spoof_uptime: None,
            ram_limit: None,
            create_shortcut: true,
            confirm_install: true,
            ask_shortcut: true,
            clean_cache: false,
            theme: Theme::Default,
            layout: Layout::Default,
            portal_filter: true,
            bound_apps: Vec::new(),
        }
    }
}

pub fn global_config_path() -> Result<PathBuf> {
    Ok(wryayer_root()?.join("defaults.ini"))
}

/// Read global default settings from ~/.wryayer/defaults.ini.
/// Falls back to AppConfig::default() if the file is absent or unreadable.
pub fn read_global_config() -> AppConfig {
    let path = match global_config_path() {
        Ok(p) => p,
        Err(_) => return AppConfig::default(),
    };
    if !path.exists() {
        return AppConfig::default();
    }
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return AppConfig::default(),
    };
    parse_ini(&content).unwrap_or_default()
}

pub fn write_global_config(config: &AppConfig) -> Result<()> {
    let path = global_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, format_ini(config))
        .with_context(|| format!("failed to write {}", path.display()))
}

pub fn config_path(app_name: &str) -> Result<PathBuf> {
    Ok(app_dir(app_name)?.join("config.ini"))
}

pub fn read_config(app_name: &str) -> Result<AppConfig> {
    let path = config_path(app_name)?;
    if !path.exists() {
        return Ok(read_global_config());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parse_ini(&content)
}

pub fn write_config(app_name: &str, config: &AppConfig) -> Result<()> {
    let path = config_path(app_name)?;
    fs::write(&path, format_ini(config))
        .with_context(|| format!("failed to write {}", path.display()))?;
    sync_container_aliases(app_name, config)?;
    Ok(())
}

/// If `root_name` is a container root (an installed app with one or more
/// aliases pointing at it), copy the container-shared subset of `root_config`
/// into each alias's config.ini.
///
/// Container-shared = everything in AppConfig except per-install behavior
/// flags (currently just `create_shortcut`, which is only used at install
/// time from the global config anyway). Per-alias config files are preserved
/// for any non-shared fields.
///
/// Silently no-ops if `root_name` is itself an alias, has no aliases, or the
/// manifest list can't be read — there's nothing to propagate in those cases.
fn sync_container_aliases(root_name: &str, root_config: &AppConfig) -> Result<()> {
    let manifests = match list_all_apps() {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    let is_root = manifests
        .iter()
        .any(|m| m.app.name == root_name && m.app.alias_of.is_none());
    if !is_root {
        return Ok(());
    }
    for m in &manifests {
        if m.app.alias_of.as_deref() != Some(root_name) {
            continue;
        }
        let alias = &m.app.name;
        let mut alias_cfg = read_config(alias).unwrap_or_default();
        alias_cfg.temp_mode        = root_config.temp_mode.clone();
        alias_cfg.temp_delete      = root_config.temp_delete.clone();
        alias_cfg.network          = root_config.network;
        alias_cfg.avahi            = root_config.avahi.clone();
        alias_cfg.camera           = root_config.camera;
        alias_cfg.microphone       = root_config.microphone;
        alias_cfg.audio            = root_config.audio;
        alias_cfg.shared_dirs      = root_config.shared_dirs.clone();
        alias_cfg.spoof_hostname   = root_config.spoof_hostname.clone();
        alias_cfg.spoof_username   = root_config.spoof_username.clone();
        alias_cfg.spoof_machine_id = root_config.spoof_machine_id.clone();
        alias_cfg.spoof_cpuinfo    = root_config.spoof_cpuinfo.clone();
        alias_cfg.spoof_os         = root_config.spoof_os.clone();
        alias_cfg.spoof_terminal   = root_config.spoof_terminal;
        alias_cfg.spoof_uptime     = root_config.spoof_uptime;
        alias_cfg.ram_limit        = root_config.ram_limit;
        alias_cfg.portal_filter    = root_config.portal_filter;
        alias_cfg.bound_apps       = root_config.bound_apps.clone();
        let alias_path = config_path(alias)?;
        fs::write(&alias_path, format_ini(&alias_cfg))
            .with_context(|| format!("failed to write {}", alias_path.display()))?;
    }
    Ok(())
}

pub fn parse_ini(content: &str) -> Result<AppConfig> {
    let mut config = AppConfig::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match (key.trim(), value.trim()) {
            ("mode", v) => {
                config.temp_mode = match v {
                    "system"  => TempMode::System,
                    "ramdisk" => TempMode::Ramdisk,
                    "local"   => TempMode::Local,
                    "uuid"    => TempMode::Uuid,
                    other     => bail!("unknown temp mode '{other}' — valid: system, ramdisk, local, uuid"),
                };
            }
            ("delete", v) => {
                config.temp_delete = match v {
                    "never"    => LocalDelete::Never,
                    "on_start" => LocalDelete::OnStart,
                    "on_close" => LocalDelete::OnClose,
                    other      => bail!("unknown delete policy '{other}' — valid: never, on_start, on_close"),
                };
            }
            ("network", v) => {
                config.network = parse_bool(v)
                    .map_err(|_| anyhow::anyhow!("unknown network value '{v}' — valid: on, off"))?;
            }
            ("avahi", v) => {
                config.avahi = match v {
                    "host" => AvahiMode::Host,
                    "off" | "false" | "0" | "no" => AvahiMode::Off,
                    _ => AvahiMode::Stub,
                };
            }
            ("camera", v) => {
                config.camera = parse_bool(v)
                    .map_err(|_| anyhow::anyhow!("unknown camera value '{v}' — valid: on, off"))?;
            }
            ("microphone", v) => {
                config.microphone = parse_bool(v)
                    .map_err(|_| anyhow::anyhow!("unknown microphone value '{v}' — valid: on, off"))?;
            }
            ("audio", v) => {
                config.audio = parse_bool(v)
                    .map_err(|_| anyhow::anyhow!("unknown audio value '{v}' — valid: on, off"))?;
            }
            ("share_dir", v) if !v.is_empty() => {
                config.shared_dirs.push(shellexpand::tilde(v).into_owned());
            }
            ("spoof_hostname", v) => {
                config.spoof_hostname = if v.is_empty() || v == "off" || v == "system" { None } else { Some(v.to_owned()) };
            }
            ("spoof_username", v) => {
                config.spoof_username = if v.is_empty() || v == "off" || v == "system" { None } else { Some(v.to_owned()) };
            }
            ("spoof_machine_id", v) => {
                config.spoof_machine_id = if v.is_empty() || v == "off" || v == "system" { None } else { Some(v.to_owned()) };
            }
            ("spoof_cpuinfo", v) => {
                config.spoof_cpuinfo = if v.is_empty() || v == "off" || v == "system" {
                    None
                } else if v == "sample" {
                    Some("sample".to_owned())
                } else if v == "custom" {
                    Some("custom".to_owned())
                } else {
                    Some(shellexpand::tilde(v).into_owned())
                };
            }
            ("spoof_os", v) => {
                config.spoof_os = if v.is_empty() || v == "off" || v == "system" { None } else { Some(v.to_owned()) };
            }
            ("spoof_terminal", v) => {
                config.spoof_terminal = matches!(v, "on" | "true" | "1");
            }
            ("spoof_uptime", v) => {
                config.spoof_uptime = if v.is_empty() || v == "off" || v == "system" { None } else { parse_uptime(v) };
            }
            ("ram_limit", v) => {
                config.ram_limit = parse_ram_limit(v);
            }
            ("create_shortcut", v) => {
                config.create_shortcut = !matches!(v, "off" | "false" | "0" | "no");
            }
            ("confirm_install", v) => {
                config.confirm_install = !matches!(v, "off" | "false" | "0" | "no");
            }
            ("ask_shortcut", v) => {
                config.ask_shortcut = !matches!(v, "off" | "false" | "0" | "no");
            }
            ("clean_cache", v) => {
                config.clean_cache = matches!(v, "on" | "true" | "1" | "yes");
            }
            ("theme", v) => {
                config.theme = match v {
                    "amber" => Theme::Amber,
                    "matrix" => Theme::Matrix,
                    _ => Theme::Default,
                };
            }
            ("layout", v) => {
                config.layout = match v {
                    "sidebar" => Layout::Sidebar,
                    "bottom" => Layout::Bottom,
                    _ => Layout::Default,
                };
            }
            ("portal_filter", v) => {
                config.portal_filter = !matches!(v, "off" | "false" | "0" | "no");
            }
            ("bind_app", v) if !v.is_empty() => {
                let name = v.to_owned();
                if !config.bound_apps.contains(&name) {
                    config.bound_apps.push(name);
                }
            }
            _ => {}
        }
    }
    Ok(config)
}

/// Parse a RAM-limit string into KiB, or None for "no limit".
///
/// Accepts a number with an optional unit — `K`/`KB`/`KiB`, `M`/`MB`/`MiB`,
/// `G`/`GB`/`GiB` (case-insensitive, 1024-based to match systemd). A bare number
/// is treated as MiB for backward compatibility with older config files. A
/// fractional value is allowed (e.g. `1.5G`). "none"/"off"/"0"/"" → None.
pub fn parse_ram_limit(v: &str) -> Option<u64> {
    let s = v.trim().to_lowercase();
    if s.is_empty() || matches!(s.as_str(), "0" | "off" | "none" | "no") {
        return None;
    }
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num_str, unit) = s.split_at(split);
    let num: f64 = num_str.parse().ok()?;
    let kib = match unit.trim() {
        "k" | "kb" | "kib" => num,
        "m" | "mb" | "mib" => num * 1024.0,
        "g" | "gb" | "gib" => num * 1024.0 * 1024.0,
        "" => num * 1024.0, // bare number = MiB (legacy configs)
        _ => return None,
    };
    let kib = kib.round() as u64;
    (kib > 0).then_some(kib)
}

/// Render a KiB RAM limit as the largest whole unit (GiB / MiB / KiB). The
/// result round-trips through [`parse_ram_limit`].
pub fn format_ram_limit(kib: u64) -> String {
    // Show the value in the largest unit that reads naturally and round-trips
    // exactly through parse_ram_limit — so "1.5 GB" stays "1.5 GB" instead of
    // being demoted to "1536 MB", while odd values (e.g. 500000 KB) keep their
    // own unit rather than turning into a fractional bigger one. Labelled KB/MB/GB
    // to match what the user types (1024-based, like systemd).
    for (div, unit) in [(1024u64 * 1024, "GB"), (1024u64, "MB")] {
        if kib >= div {
            let count = format!("{:.2}", kib as f64 / div as f64);
            let count = count.trim_end_matches('0').trim_end_matches('.');
            let label = format!("{count} {unit}");
            if parse_ram_limit(&label) == Some(kib) {
                return label;
            }
        }
    }
    format!("{kib} KB")
}

/// Parse a spoofed-uptime value into seconds. Accepts a compound duration made
/// of `w`/`d`/`h`/`m`/`s` parts (e.g. "3d4h", "1w 2d", "90m", "45"), where a
/// bare number is seconds. "0"/"off"/"none"/"" → None.
pub fn parse_uptime(v: &str) -> Option<u64> {
    let s = v.trim().to_lowercase();
    if s.is_empty() || matches!(s.as_str(), "0" | "off" | "none" | "no" | "system") {
        return None;
    }
    // Bare number = seconds.
    if let Ok(n) = s.parse::<u64>() {
        return (n > 0).then_some(n);
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else if let Some(mult) = match ch {
            'w' => Some(604800u64),
            'd' => Some(86400),
            'h' => Some(3600),
            'm' => Some(60),
            's' => Some(1),
            _ if ch.is_whitespace() => continue,
            _ => None,
        } {
            let n: u64 = num.parse().ok()?;
            total = total.checked_add(n.checked_mul(mult)?)?;
            num.clear();
        } else {
            return None; // unrecognised unit
        }
    }
    // A trailing number with no unit is treated as seconds.
    if let Ok(n) = num.parse::<u64>() {
        total = total.checked_add(n)?;
    }
    (total > 0).then_some(total)
}

/// Render a seconds uptime as a compact duration (e.g. "3d4h", "90m", "45s")
/// that round-trips through [`parse_uptime`].
pub fn format_uptime(mut secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    let mut out = String::new();
    for (div, unit) in [(604800u64, 'w'), (86400, 'd'), (3600, 'h'), (60, 'm'), (1, 's')] {
        if secs >= div {
            out.push_str(&format!("{}{unit}", secs / div));
            secs %= div;
        }
    }
    out
}

/// Cheap non-cryptographic randomness — enough to seed a plausible hostname or
/// username. Mixes the clock, the pid and a monotonic counter so successive
/// calls differ even within the same nanosecond.
fn spoof_rng() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let mut x = t
        ^ (std::process::id() as u64).rotate_left(17)
        ^ c.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn spoof_pick<'a>(items: &[&'a str]) -> &'a str {
    items[(spoof_rng() as usize) % items.len()]
}

/// A random but realistic-looking hostname, e.g. `desktop-a3f9c1`. Returned as a
/// fixed custom string — it only changes when regenerated, never per launch.
pub fn random_hostname() -> String {
    let prefix = spoof_pick(&["pc", "desktop", "host", "arch", "box", "node", "lab", "workstation"]);
    format!("{prefix}-{:06x}", spoof_rng() & 0xff_ffff)
}

/// A random but realistic-looking username, e.g. `max47`.
pub fn random_username() -> String {
    let name = spoof_pick(&["alex", "sam", "max", "lee", "kai", "noah", "mia", "ivy", "leo", "zoe", "user"]);
    format!("{name}{:02}", spoof_rng() % 100)
}

#[allow(clippy::result_unit_err)] // callers only care whether it parsed; the unit err is the signal
pub fn parse_bool(v: &str) -> Result<bool, ()> {
    match v {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(()),
    }
}

pub fn format_ini(config: &AppConfig) -> String {
    let mode = match config.temp_mode {
        TempMode::System  => "system",
        TempMode::Ramdisk => "ramdisk",
        TempMode::Local   => "local",
        TempMode::Uuid    => "uuid",
    };
    let delete = match config.temp_delete {
        LocalDelete::Never   => "never",
        LocalDelete::OnStart => "on_start",
        LocalDelete::OnClose => "on_close",
    };
    let b = |v: bool| if v { "on" } else { "off" };
    let mut s = format!(
        "[temp]\n\
         ; ramdisk = private in-memory tmpfs, discarded on close\n\
         ; local   = persistent per-app dir ~/.wryayer/<app>/.tmp/\n\
         ; system  = share the host /tmp (default)\n\
         ; uuid    = private per-instance dir (deleted on close)\n\
         mode = {mode}\n\
         \n\
         ; Only applies when mode = local\n\
         ; never    = keep temp across restarts\n\
         ; on_start = wipe on launch when no other instance is running\n\
         ; on_close = wipe when this instance exits\n\
         delete = {delete}\n\
         \n\
         [network]\n\
         ; on = allow internet access (default), off = block all network\n\
         network = {}\n\
         ; avahi = how to answer apps that probe Avahi/zeroconf at startup:\n\
         ;   stub = private in-sandbox stub bus (no host change, no LAN broadcast)\n\
         ;   host = start the host avahi-daemon if installed but stopped\n\
         ;   off  = leave the harmless \"Daemon not running\" warning as-is\n\
         avahi = {}\n\
         \n\
         [devices]\n\
         ; on = allow access, off = mask device inside sandbox\n\
         camera = {}\n\
         ; note: microphone off blocks ALSA capture; PipeWire/PulseAudio mic\n\
         ; is only fully blocked when audio is also off\n\
         microphone = {}\n\
         audio = {}\n",
        b(config.network),
        match config.avahi {
            AvahiMode::Stub => "stub",
            AvahiMode::Host => "host",
            AvahiMode::Off  => "off",
        },
        b(config.camera),
        b(config.microphone),
        b(config.audio),
    );
    if !config.shared_dirs.is_empty() {
        s.push_str("\n[share]\n");
        s.push_str("; Host directories bind-mounted read-write inside the sandbox\n");
        for dir in &config.shared_dirs {
            s.push_str(&format!("share_dir = {dir}\n"));
        }
    }
    let has_spoof = config.spoof_hostname.is_some()
        || config.spoof_username.is_some()
        || config.spoof_machine_id.is_some()
        || config.spoof_cpuinfo.is_some()
        || config.spoof_os.is_some()
        || config.spoof_terminal
        || config.spoof_uptime.is_some();
    if has_spoof {
        s.push_str("\n[spoof]\n");
        s.push_str("; spoof_machine_id = random  — fresh UUID on every launch\n");
        if let Some(ref v) = config.spoof_hostname {
            s.push_str(&format!("spoof_hostname = {v}\n"));
        }
        if let Some(ref v) = config.spoof_username {
            s.push_str(&format!("spoof_username = {v}\n"));
        }
        if let Some(ref v) = config.spoof_machine_id {
            s.push_str(&format!("spoof_machine_id = {v}\n"));
        }
        if let Some(ref v) = config.spoof_cpuinfo {
            s.push_str(&format!("spoof_cpuinfo = {v}\n"));
        }
        if let Some(ref v) = config.spoof_os {
            s.push_str(&format!("spoof_os = {v}\n"));
        }
        if config.spoof_terminal {
            s.push_str("spoof_terminal = on\n");
        }
        if let Some(secs) = config.spoof_uptime {
            s.push_str("; spoof_uptime accepts a duration (e.g. 3d4h, 90m) or bare seconds\n");
            s.push_str(&format!("spoof_uptime = {}\n", format_uptime(secs)));
        }
    }
    if let Some(kib) = config.ram_limit {
        s.push_str("\n[resources]\n");
        s.push_str("; Maximum RAM (RAM + swap). Enforced via systemd-run MemoryMax+MemorySwapMax.\n");
        s.push_str("; Accepts a unit: KB / MB / GB (e.g. 512MB, 2GB). A bare number means MiB.\n");
        s.push_str(&format!("ram_limit = {}\n", format_ram_limit(kib)));
    }
    if !config.bound_apps.is_empty() {
        s.push_str("\n[bind]\n");
        s.push_str("; Other installed wryayer apps exposed inside this sandbox as\n");
        s.push_str("; host-delegated launchers. Running e.g. `firefox <url>` from this\n");
        s.push_str("; app forwards to the host and re-launches `wryayer run firefox`.\n");
        for app in &config.bound_apps {
            s.push_str(&format!("bind_app = {app}\n"));
        }
    }
    if !config.create_shortcut || !config.confirm_install || !config.ask_shortcut || config.clean_cache || config.theme != Theme::Default || config.layout != Layout::Default || !config.portal_filter {
        s.push_str("\n[behavior]\n");
        if !config.create_shortcut {
            s.push_str("; Create ~/bin/<name> shortcut by default when installing apps\n");
            s.push_str("create_shortcut = off\n");
        }
        if !config.confirm_install {
            s.push_str("; Ask 'Install <pkg>?' before installing from the TUI\n");
            s.push_str("confirm_install = off\n");
        }
        if !config.ask_shortcut {
            s.push_str("; Ask whether to create a ~/bin shortcut before installing\n");
            s.push_str("ask_shortcut = off\n");
        }
        if config.clean_cache {
            s.push_str("; Delete the shared download/build cache (~/.cache/wryayer) after each install\n");
            s.push_str("clean_cache = on\n");
        }
        if config.theme != Theme::Default {
            s.push_str("; TUI colour theme: default | amber | matrix\n");
            let name = match config.theme {
                Theme::Amber => "amber",
                Theme::Matrix => "matrix",
                Theme::Default => "default",
            };
            s.push_str(&format!("theme = {name}\n"));
        }
        if config.layout != Layout::Default {
            s.push_str("; TUI layout: default (top tabs) | sidebar (left tabs) | bottom (bottom tabs)\n");
            let name = match config.layout {
                Layout::Sidebar => "sidebar",
                Layout::Bottom => "bottom",
                Layout::Default => "default",
            };
            s.push_str(&format!("layout = {name}\n"));
        }
        if !config.portal_filter {
            s.push_str("; Hide the host desktop portal so file pickers only show shared dirs.\n");
            s.push_str("; Turn off if an app needs portal features (screen-share, portal file open).\n");
            s.push_str("portal_filter = off\n");
        }
    }
    s
}
