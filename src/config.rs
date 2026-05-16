use crate::manifest::app_dir;
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
    /// Allow access to /dev/video* camera devices (default: true)
    pub camera: bool,
    /// Allow ALSA capture devices + PipeWire/PulseAudio mic (default: true)
    pub microphone: bool,
    /// Allow ALSA playback + PipeWire/PulseAudio audio output (default: true)
    pub audio: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            temp_mode: TempMode::System,
            temp_delete: LocalDelete::OnStart,
            network: true,
            camera: true,
            microphone: true,
            audio: true,
        }
    }
}

pub fn config_path(app_name: &str) -> Result<PathBuf> {
    Ok(app_dir(app_name)?.join("config.ini"))
}

pub fn read_config(app_name: &str) -> Result<AppConfig> {
    let path = config_path(app_name)?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parse_ini(&content)
}

pub fn write_config(app_name: &str, config: &AppConfig) -> Result<()> {
    let path = config_path(app_name)?;
    fs::write(&path, format_ini(config))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn parse_ini(content: &str) -> Result<AppConfig> {
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
            _ => {}
        }
    }
    Ok(config)
}

fn parse_bool(v: &str) -> Result<bool, ()> {
    match v {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(()),
    }
}

fn format_ini(config: &AppConfig) -> String {
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
    format!(
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
         \n\
         [devices]\n\
         ; on = allow access, off = mask device inside sandbox\n\
         camera = {}\n\
         ; note: microphone off blocks ALSA capture; PipeWire/PulseAudio mic\n\
         ; is only fully blocked when audio is also off\n\
         microphone = {}\n\
         audio = {}\n",
        b(config.network),
        b(config.camera),
        b(config.microphone),
        b(config.audio),
    )
}
