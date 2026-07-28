use crate::config::{config_path, read_config, write_config, AppConfig, LocalDelete, TempMode};
use crate::manifest::read_manifest;
use anyhow::{bail, Context, Result};
use std::path::Path;

#[allow(clippy::too_many_arguments)] // one optional per config field; a struct would just move the noise
pub fn run(
    app_name: &str,
    temp_mode: Option<&str>,
    temp_delete: Option<&str>,
    network: Option<&str>,
    camera: Option<&str>,
    microphone: Option<&str>,
    audio: Option<&str>,
    usb: Option<&str>,
    spoof_hostname: Option<&str>,
    spoof_username: Option<&str>,
    spoof_machine_id: Option<&str>,
    spoof_cpuinfo: Option<&str>,
    spoof_os: Option<&str>,
    spoof_terminal: Option<&str>,
    spoof_uptime: Option<&str>,
    ram_limit: Option<&str>,
) -> Result<()> {
    read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    let mut config = read_config(app_name)?;
    let changed = [
        temp_mode, temp_delete, network, camera, microphone, audio, usb,
        spoof_hostname, spoof_username, spoof_machine_id, spoof_cpuinfo, spoof_os,
        spoof_terminal, spoof_uptime, ram_limit,
    ]
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
        (usb,        &mut config.usb,        "usb"),
    ] {
        if let Some(v) = val {
            *field = match v {
                "on"  | "true"  | "1" => true,
                "off" | "false" | "0" => false,
                other => bail!("unknown {name} value '{other}'\n  valid: on, off"),
            };
        }
    }

    let set_spoof = |val: Option<&str>| -> Option<Option<String>> {
        val.map(|v| if v == "off" || v == "system" || v.is_empty() { None } else { Some(v.to_owned()) })
    };

    if let Some(v) = set_spoof(spoof_hostname)   { config.spoof_hostname   = v; }
    if let Some(v) = set_spoof(spoof_username)   { config.spoof_username   = v; }
    if let Some(v) = set_spoof(spoof_machine_id) { config.spoof_machine_id = v; }
    if let Some(v) = set_spoof(spoof_cpuinfo)    { config.spoof_cpuinfo    = v; }
    if let Some(v) = set_spoof(spoof_os)         { config.spoof_os         = v; }

    if let Some(v) = spoof_terminal {
        config.spoof_terminal = match v {
            "on" | "true" | "1" => true,
            "off" | "false" | "0" => false,
            other => bail!("unknown spoof_terminal value '{other}'\n  valid: on, off"),
        };
    }

    if let Some(v) = spoof_uptime {
        if matches!(v, "system" | "off" | "0" | "") {
            config.spoof_uptime = None;
        } else {
            match crate::config::parse_uptime(v) {
                Some(secs) => config.spoof_uptime = Some(secs),
                None => bail!("invalid spoof_uptime '{v}' — expected a duration like 3d4h, 90m, bare seconds, or 'system'"),
            }
        }
    }

    if let Some(v) = ram_limit {
        if matches!(v, "none" | "off" | "0" | "") {
            config.ram_limit = None;
        } else {
            match crate::config::parse_ram_limit(v) {
                Some(kib) => config.ram_limit = Some(kib),
                None => bail!("invalid ram_limit '{v}' — expected a size like 512MB, 2GB, 500000KB, or 'none'"),
            }
        }
    }

    if changed {
        write_config(app_name, &config)?;
        eprintln!("Saved to {}", config_path(app_name)?.display());
    }

    print_config(app_name, &config);
    Ok(())
}

pub fn share_add(app_name: &str, raw_path: &str) -> Result<()> {
    read_manifest(app_name).with_context(|| format!("'{app_name}' is not installed"))?;
    let path = shellexpand::tilde(raw_path).into_owned();
    if !Path::new(&path).is_dir() {
        bail!("not a directory: {path}");
    }
    let mut config = read_config(app_name)?;
    if config.shared_dirs.contains(&path) {
        eprintln!("already shared: {path}");
    } else {
        config.shared_dirs.push(path.clone());
        write_config(app_name, &config)?;
        eprintln!("added shared dir: {path}");
    }
    Ok(())
}

/// Set where an encrypted app's container password comes from.
///
/// A dedicated function rather than another parameter on `run`: the value needs
/// validating, and it only means anything for an app that is actually stored in
/// a VeraCrypt container — worth saying out loud instead of silently recording a
/// setting that will never be consulted.
pub fn password_source(app_name: &str, value: &str) -> Result<()> {
    read_manifest(app_name).with_context(|| format!("'{app_name}' is not installed"))?;
    let source = match value {
        "prompt" => crate::config::PasswordSource::Prompt,
        "master" => crate::config::PasswordSource::Master,
        other => bail!("unknown password source '{other}' — valid: prompt, master"),
    };
    if source == crate::config::PasswordSource::Master && !crate::secrets::exists() {
        bail!(
            "no master password store yet — create one first:\n    wryayer master init"
        );
    }

    let mut config = read_config(app_name)?;
    config.password_source = source;
    write_config(app_name, &config)?;
    eprintln!("{app_name}: password_source = {value}");

    if !crate::veracrypt::is_encrypted(app_name) {
        eprintln!(
            "note: '{app_name}' is not stored in an encrypted container, so this has no effect \
             yet. Encrypt it with:\n    wryayer encrypt {app_name}"
        );
    } else if source == crate::config::PasswordSource::Master
        && crate::secrets::open_cached()
            .ok()
            .flatten()
            .is_none_or(|s| s.get(app_name).is_none())
    {
        eprintln!(
            "note: the master store has no password for '{app_name}' yet — add it with:\n    \
             wryayer master set {app_name}"
        );
    }
    Ok(())
}

/// Set whether an encrypted app's container is unmounted when the app exits.
pub fn lock_on_exit(app_name: &str, value: &str) -> Result<()> {
    read_manifest(app_name).with_context(|| format!("'{app_name}' is not installed"))?;
    let on = crate::config::parse_bool(value)
        .map_err(|_| anyhow::anyhow!("unknown value '{value}' — valid: on, off"))?;
    let mut config = read_config(app_name)?;
    config.lock_on_exit = on;
    write_config(app_name, &config)?;
    eprintln!("{app_name}: lock_on_exit = {}", if on { "on" } else { "off" });
    if !crate::veracrypt::is_encrypted(app_name) {
        eprintln!("note: '{app_name}' is not stored in an encrypted container, so this has no effect yet.");
    }
    Ok(())
}

pub fn share_remove(app_name: &str, raw_path: &str) -> Result<()> {
    read_manifest(app_name).with_context(|| format!("'{app_name}' is not installed"))?;
    let path = shellexpand::tilde(raw_path).into_owned();
    let mut config = read_config(app_name)?;
    let before = config.shared_dirs.len();
    config.shared_dirs.retain(|d| d != &path);
    if config.shared_dirs.len() < before {
        write_config(app_name, &config)?;
        eprintln!("removed shared dir: {path}");
    } else {
        eprintln!("not found: {path}");
    }
    Ok(())
}

pub fn share_list(app_name: &str) -> Result<()> {
    read_manifest(app_name).with_context(|| format!("'{app_name}' is not installed"))?;
    let config = read_config(app_name)?;
    if config.shared_dirs.is_empty() {
        eprintln!("[{app_name}] no shared directories");
    } else {
        for d in &config.shared_dirs {
            println!("{d}");
        }
    }
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
    eprintln!("  usb         = {}", b(config.usb));
    if config.shared_dirs.is_empty() {
        eprintln!("  shared dirs = (none)");
    } else {
        eprintln!("  shared dirs:");
        for d in &config.shared_dirs {
            eprintln!("    {d}");
        }
    }
    fn spoof_str(v: &Option<String>) -> &str { v.as_deref().unwrap_or("off") }
    if config.spoof_hostname.is_some()
        || config.spoof_username.is_some()
        || config.spoof_machine_id.is_some()
        || config.spoof_cpuinfo.is_some()
        || config.spoof_os.is_some()
        || config.spoof_terminal
        || config.spoof_uptime.is_some()
    {
        eprintln!("  spoof:");
        eprintln!("    hostname   = {}", spoof_str(&config.spoof_hostname));
        eprintln!("    username   = {}", spoof_str(&config.spoof_username));
        eprintln!("    machine-id = {}", spoof_str(&config.spoof_machine_id));
        eprintln!("    cpuinfo    = {}", spoof_str(&config.spoof_cpuinfo));
        eprintln!("    os-release = {}", spoof_str(&config.spoof_os));
        eprintln!("    terminal   = {}", b(config.spoof_terminal));
        eprintln!("    uptime     = {}", config.spoof_uptime.map(crate::config::format_uptime).unwrap_or_else(|| "off".into()));
    }
    match config.ram_limit {
        None      => eprintln!("  ram_limit   = none"),
        Some(kib) => eprintln!("  ram_limit   = {}", crate::config::format_ram_limit(kib)),
    }
}
