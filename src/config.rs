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
    /// A green-phosphor terminal palette (green body text, not white).
    Matrix,
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
    /// Maximum RAM the app may use in MiB — enforced via systemd-run (None = no limit)
    pub ram_limit: Option<u64>,
    /// Spoof screen resolution reported by xrandr and via env vars — e.g. "1920x1080"
    pub spoof_resolution: Option<String>,
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
    /// Route the sandbox's D-Bus session through a filter that hides the host
    /// desktop portal, so file pickers run in-sandbox and only show shared
    /// dirs instead of leaking host paths (default: true)
    pub portal_filter: bool,
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
            ram_limit: None,
            spoof_resolution: None,
            create_shortcut: true,
            confirm_install: true,
            ask_shortcut: true,
            clean_cache: false,
            theme: Theme::Default,
            portal_filter: true,
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
        alias_cfg.ram_limit        = root_config.ram_limit;
        alias_cfg.spoof_resolution = root_config.spoof_resolution.clone();
        alias_cfg.portal_filter    = root_config.portal_filter;
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
            ("ram_limit", v) => {
                config.ram_limit = if v.is_empty() || v == "0" || v == "off" || v == "none" {
                    None
                } else {
                    v.parse::<u64>().ok().filter(|&n| n > 0)
                };
            }
            ("spoof_resolution", v) => {
                config.spoof_resolution = if v.is_empty() || v == "off" || v == "system" {
                    None
                } else {
                    Some(v.to_owned())
                };
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
            ("portal_filter", v) => {
                config.portal_filter = !matches!(v, "off" | "false" | "0" | "no");
            }
            _ => {}
        }
    }
    Ok(config)
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
        || config.spoof_terminal;
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
    }
    if let Some(mib) = config.ram_limit {
        s.push_str("\n[resources]\n");
        s.push_str("; Maximum RAM in MiB (RAM + swap). Enforced via systemd-run MemoryMax+MemorySwapMax.\n");
        s.push_str(&format!("ram_limit = {mib}\n"));
    }
    if let Some(ref res) = config.spoof_resolution {
        s.push_str("\n[spoof]\n");
        s.push_str("; Screen resolution to report via xrandr and env vars (e.g. 1920x1080)\n");
        s.push_str(&format!("spoof_resolution = {res}\n"));
    }
    if !config.create_shortcut || !config.confirm_install || !config.ask_shortcut || config.clean_cache || config.theme != Theme::Default || !config.portal_filter {
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
        if !config.portal_filter {
            s.push_str("; Hide the host desktop portal so file pickers only show shared dirs.\n");
            s.push_str("; Turn off if an app needs portal features (screen-share, portal file open).\n");
            s.push_str("portal_filter = off\n");
        }
    }
    s
}
