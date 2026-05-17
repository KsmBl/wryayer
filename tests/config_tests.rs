use wryayer::config::*;

// ── parse_bool — 3 equivalence classes ───────────────────────────────────────

#[test]
fn parse_bool_truthy_ec() {
    for v in ["on", "true", "1"] {
        assert_eq!(parse_bool(v), Ok(true), "expected true for '{v}'");
    }
}

#[test]
fn parse_bool_falsy_ec() {
    for v in ["off", "false", "0"] {
        assert_eq!(parse_bool(v), Ok(false), "expected false for '{v}'");
    }
}

#[test]
fn parse_bool_invalid_ec() {
    // Any unrecognised string → Err (representative sample of the invalid class)
    for v in ["yes", "no", "enabled", "1.0", "True", "ON", "OFF", ""] {
        assert!(parse_bool(v).is_err(), "expected Err for '{v}'");
    }
}

// ── parse_ini — structural / ignored lines ────────────────────────────────────

#[test]
fn parse_ini_empty_yields_defaults() {
    let cfg = parse_ini("").unwrap();
    let def = AppConfig::default();
    assert_eq!(cfg.temp_mode,   def.temp_mode);
    assert_eq!(cfg.temp_delete, def.temp_delete);
    assert_eq!(cfg.network,     def.network);
    assert_eq!(cfg.shared_dirs, def.shared_dirs);
}

#[test]
fn parse_ini_comments_and_sections_ignored() {
    let s = "[temp]\n# comment\n; another comment\n[devices]\n";
    let cfg = parse_ini(s).unwrap();
    assert_eq!(cfg.temp_mode, TempMode::System);
    assert!(cfg.network);
}

#[test]
fn parse_ini_lines_without_equals_ignored() {
    let cfg = parse_ini("not a key value pair\n").unwrap();
    assert!(cfg.network);
}

#[test]
fn parse_ini_whitespace_around_equals_trimmed() {
    let cfg = parse_ini("  mode  =  ramdisk  \n").unwrap();
    assert_eq!(cfg.temp_mode, TempMode::Ramdisk);
}

// ── parse_ini — temp mode — 4 ECs + error ─────────────────────────────────────

#[test]
fn parse_ini_all_temp_modes() {
    for (s, expected) in [
        ("mode = system",  TempMode::System),
        ("mode = ramdisk", TempMode::Ramdisk),
        ("mode = local",   TempMode::Local),
        ("mode = uuid",    TempMode::Uuid),
    ] {
        assert_eq!(parse_ini(s).unwrap().temp_mode, expected, "mode '{s}'");
    }
}

#[test]
fn parse_ini_unknown_mode_is_error() {
    assert!(parse_ini("mode = garbage").is_err());
}

// ── parse_ini — delete policy — 3 ECs + error ────────────────────────────────

#[test]
fn parse_ini_all_delete_policies() {
    for (s, expected) in [
        ("delete = never",    LocalDelete::Never),
        ("delete = on_start", LocalDelete::OnStart),
        ("delete = on_close", LocalDelete::OnClose),
    ] {
        assert_eq!(parse_ini(s).unwrap().temp_delete, expected, "delete '{s}'");
    }
}

#[test]
fn parse_ini_unknown_delete_is_error() {
    assert!(parse_ini("delete = always").is_err());
}

// ── parse_ini — bool fields ───────────────────────────────────────────────────

#[test]
fn parse_ini_bool_fields_all_off() {
    let s = "network = off\ncamera = off\nmicrophone = off\naudio = off\n";
    let cfg = parse_ini(s).unwrap();
    assert!(!cfg.network);
    assert!(!cfg.camera);
    assert!(!cfg.microphone);
    assert!(!cfg.audio);
}

#[test]
fn parse_ini_invalid_bool_is_error() {
    // Representative of invalid bool value for any bool key
    assert!(parse_ini("network = yes").is_err());
    assert!(parse_ini("camera = maybe").is_err());
}

// ── parse_ini — shared_dirs ───────────────────────────────────────────────────

#[test]
fn parse_ini_multiple_share_dirs() {
    let s = "share_dir = /tmp/a\nshare_dir = /tmp/b\nshare_dir = /tmp/c\n";
    let cfg = parse_ini(s).unwrap();
    assert_eq!(cfg.shared_dirs, vec!["/tmp/a", "/tmp/b", "/tmp/c"]);
}

#[test]
fn parse_ini_empty_share_dir_skipped() {
    // Boundary: value is empty string — must not push an empty entry
    let cfg = parse_ini("share_dir = \n").unwrap();
    assert!(cfg.shared_dirs.is_empty());
}

#[test]
fn parse_ini_unknown_keys_ignored() {
    let cfg = parse_ini("brightness = high\nfoo = bar\n").unwrap();
    assert!(cfg.shared_dirs.is_empty());
    assert!(cfg.network); // default unchanged
}

// ── format_ini ────────────────────────────────────────────────────────────────

#[test]
fn format_ini_omits_share_section_when_empty() {
    let ini = format_ini(&AppConfig::default());
    assert!(!ini.contains("[share]"));
    assert!(!ini.contains("share_dir"));
}

#[test]
fn format_ini_includes_share_section_when_non_empty() {
    let cfg = AppConfig {
        shared_dirs: vec!["/home/user/docs".to_string()],
        ..AppConfig::default()
    };
    let ini = format_ini(&cfg);
    assert!(ini.contains("[share]"));
    assert!(ini.contains("share_dir = /home/user/docs"));
}

// ── round-trip: format_ini → parse_ini ───────────────────────────────────────

#[test]
fn round_trip_default_config() {
    let original = AppConfig::default();
    let parsed = parse_ini(&format_ini(&original)).unwrap();
    assert_eq!(parsed.temp_mode,   original.temp_mode);
    assert_eq!(parsed.temp_delete, original.temp_delete);
    assert_eq!(parsed.network,     original.network);
    assert_eq!(parsed.camera,      original.camera);
    assert_eq!(parsed.microphone,  original.microphone);
    assert_eq!(parsed.audio,       original.audio);
    assert_eq!(parsed.shared_dirs, original.shared_dirs);
}

#[test]
fn round_trip_all_non_default_values() {
    let original = AppConfig {
        temp_mode:   TempMode::Ramdisk,
        temp_delete: LocalDelete::OnClose,
        network:     false,
        camera:      false,
        microphone:  false,
        audio:       false,
        shared_dirs: vec!["/tmp/foo".to_string(), "/opt/bar".to_string()],
    };
    let parsed = parse_ini(&format_ini(&original)).unwrap();
    assert_eq!(parsed.temp_mode,   TempMode::Ramdisk);
    assert_eq!(parsed.temp_delete, LocalDelete::OnClose);
    assert!(!parsed.network);
    assert!(!parsed.camera);
    assert!(!parsed.microphone);
    assert!(!parsed.audio);
    assert_eq!(parsed.shared_dirs, vec!["/tmp/foo", "/opt/bar"]);
}
