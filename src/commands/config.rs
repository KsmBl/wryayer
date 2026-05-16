use crate::config::{config_path, read_config, write_config, AppConfig, LocalDelete, TempMode};
use crate::manifest::read_manifest;
use anyhow::{bail, Context, Result};

pub fn run(
    app_name: &str,
    temp_mode: Option<&str>,
    temp_delete: Option<&str>,
    network: Option<&str>,
    camera: Option<&str>,
    microphone: Option<&str>,
    audio: Option<&str>,
) -> Result<()> {
    read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    let mut config = read_config(app_name)?;
    let changed = [temp_mode, temp_delete, network, camera, microphone, audio]
        .iter()
        .any(Option::is_some);

    if let Some(mode) = temp_mode {
        config.temp_mode = match mode {
            "system"  => TempMode::System,
            "ramdisk" => TempMode::Ramdisk,
            "local"   => TempMode::Local,
            "uuid"    => TempMode::Uuid,
            other => bail!("unknown temp mode '{other}'\n  valid: system, ramdisk, local, uuid"),
        };
    }

    if let Some(policy) = temp_delete {
        config.temp_delete = match policy {
            "never"    => LocalDelete::Never,
            "on_start" => LocalDelete::OnStart,
            "on_close" => LocalDelete::OnClose,
            other => bail!("unknown delete policy '{other}'\n  valid: never, on_start, on_close"),
        };
    }

    for (val, field, name) in [
        (network,    &mut config.network,    "network"),
        (camera,     &mut config.camera,     "camera"),
        (microphone, &mut config.microphone, "microphone"),
        (audio,      &mut config.audio,      "audio"),
    ] {
        if let Some(v) = val {
            *field = match v {
                "on"  | "true"  | "1" => true,
                "off" | "false" | "0" => false,
                other => bail!("unknown {name} value '{other}'\n  valid: on, off"),
            };
        }
    }

    if changed {
        write_config(app_name, &config)?;
        eprintln!("Saved to {}", config_path(app_name)?.display());
    }

    print_config(app_name, &config);
    Ok(())
}

fn print_config(app_name: &str, config: &AppConfig) {
    let b = |v: bool| if v { "on" } else { "off" };
    let mode = match config.temp_mode {
        TempMode::System  => "system",
        TempMode::Ramdisk => "ramdisk",
        TempMode::Local   => "local",
        TempMode::Uuid    => "uuid",
    };
    eprintln!("[{app_name}]");
    eprintln!("  temp.mode   = {mode}");
    if matches!(config.temp_mode, TempMode::Local) {
        let delete = match config.temp_delete {
            LocalDelete::Never   => "never",
            LocalDelete::OnStart => "on_start",
            LocalDelete::OnClose => "on_close",
        };
        eprintln!("  temp.delete = {delete}");
    }
    eprintln!("  network     = {}", b(config.network));
    eprintln!("  camera      = {}", b(config.camera));
    eprintln!("  microphone  = {}", b(config.microphone));
    if !config.microphone && config.audio {
        eprintln!("  ! microphone off only blocks ALSA capture devices.");
        eprintln!("    Apps using PipeWire or PulseAudio can still access");
        eprintln!("    the mic. Set audio = off to block it completely.");
    }
    eprintln!("  audio       = {}", b(config.audio));
}
