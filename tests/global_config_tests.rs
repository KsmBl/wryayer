use wryayer::config::{
    format_ini, parse_ini, read_global_config, write_global_config, AppConfig, TempMode,
};

fn with_temp_home<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::TempDir::new().unwrap();
    let old = std::env::var_os("HOME");
    std::env::set_var("HOME", tmp.path());
    f(tmp.path());
    match old {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}

// ── read_global_config falls back gracefully ─────────────────────────────────

#[test]
fn read_global_config_returns_default_when_no_file() {
    with_temp_home(|_root| {
        let cfg = read_global_config();
        let default = AppConfig::default();
        assert_eq!(cfg.network, default.network);
        assert_eq!(cfg.temp_mode, default.temp_mode);
        assert_eq!(cfg.ram_limit, default.ram_limit);
        assert_eq!(cfg.keyboard_layout, default.keyboard_layout);
    });
}

// ── write_global_config + read_global_config round-trip ─────────────────────

#[test]
fn write_then_read_global_config_round_trips() {
    with_temp_home(|_root| {
        let mut cfg = AppConfig::default();
        cfg.network = false;
        cfg.temp_mode = TempMode::Ramdisk;
        cfg.ram_limit = Some(2048);
        cfg.keyboard_layout = Some("de".to_string());

        write_global_config(&cfg).unwrap();
        let loaded = read_global_config();

        assert!(!loaded.network);
        assert_eq!(loaded.temp_mode, TempMode::Ramdisk);
        assert_eq!(loaded.ram_limit, Some(2048));
        assert_eq!(loaded.keyboard_layout.as_deref(), Some("de"));
    });
}

// ── keyboard_layout round-trips through format_ini / parse_ini ──────────────

#[test]
fn keyboard_layout_none_survives_round_trip() {
    let cfg = AppConfig { keyboard_layout: None, ..AppConfig::default() };
    let parsed = parse_ini(&format_ini(&cfg)).unwrap();
    assert_eq!(parsed.keyboard_layout, None);
}

#[test]
fn keyboard_layout_us_survives_round_trip() {
    let cfg = AppConfig { keyboard_layout: Some("us".to_string()), ..AppConfig::default() };
    let parsed = parse_ini(&format_ini(&cfg)).unwrap();
    assert_eq!(parsed.keyboard_layout.as_deref(), Some("us"));
}

#[test]
fn keyboard_layout_de_survives_round_trip() {
    let cfg = AppConfig { keyboard_layout: Some("de".to_string()), ..AppConfig::default() };
    let parsed = parse_ini(&format_ini(&cfg)).unwrap();
    assert_eq!(parsed.keyboard_layout.as_deref(), Some("de"));
}

#[test]
fn keyboard_layout_colemak_survives_round_trip() {
    let cfg = AppConfig { keyboard_layout: Some("colemak".to_string()), ..AppConfig::default() };
    let parsed = parse_ini(&format_ini(&cfg)).unwrap();
    assert_eq!(parsed.keyboard_layout.as_deref(), Some("colemak"));
}

#[test]
fn keyboard_layout_dvorak_survives_round_trip() {
    let cfg = AppConfig { keyboard_layout: Some("dvorak".to_string()), ..AppConfig::default() };
    let parsed = parse_ini(&format_ini(&cfg)).unwrap();
    assert_eq!(parsed.keyboard_layout.as_deref(), Some("dvorak"));
}

#[test]
fn keyboard_layout_off_alias_parses_to_none() {
    let parsed = parse_ini("keyboard_layout = off").unwrap();
    assert_eq!(parsed.keyboard_layout, None);
}

#[test]
fn keyboard_layout_system_alias_parses_to_none() {
    let parsed = parse_ini("keyboard_layout = system").unwrap();
    assert_eq!(parsed.keyboard_layout, None);
}

#[test]
fn keyboard_layout_empty_alias_parses_to_none() {
    let parsed = parse_ini("keyboard_layout = ").unwrap();
    assert_eq!(parsed.keyboard_layout, None);
}
