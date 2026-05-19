use wryayer::config::{AppConfig, LocalDelete, TempMode};
use wryayer::tui::{
    apply_setting, cycle_setting, option_description, setting_current, setting_description,
    setting_options, setting_title,
};

// ── setting_options: shape of each row's choice list ─────────────────────────

#[test]
fn options_for_bool_rows_are_on_off() {
    for idx in 0..=3 {
        assert_eq!(setting_options(idx), vec!["on", "off"], "row {idx}");
    }
}

#[test]
fn options_for_temp_mode_has_four_choices() {
    assert_eq!(setting_options(4), vec!["system", "ramdisk", "local", "uuid"]);
}

#[test]
fn options_for_temp_delete_has_three_choices() {
    assert_eq!(setting_options(5), vec!["never", "on_start", "on_close"]);
}

#[test]
fn options_for_non_picker_rows_are_empty() {
    // CFG_SHARES (6) and CFG_SAVE (13) are handled by their own screens.
    assert!(setting_options(6).is_empty());
    assert!(setting_options(13).is_empty());
    assert!(setting_options(999).is_empty());
}

// ── setting_title: human-readable label per row ──────────────────────────────

#[test]
fn titles_match_known_rows() {
    assert_eq!(setting_title(0), "Network");
    assert_eq!(setting_title(1), "Camera");
    assert_eq!(setting_title(2), "Microphone");
    assert_eq!(setting_title(3), "Audio");
    assert_eq!(setting_title(4), "Temp mode");
    assert_eq!(setting_title(5), "Temp delete");
}

#[test]
fn title_for_unknown_row_falls_back() {
    assert_eq!(setting_title(99), "Option");
}

// ── setting_current: round-trips with the underlying enum ────────────────────

#[test]
fn current_index_matches_default_config() {
    let c = AppConfig::default();
    // Defaults: network/camera/mic/audio = on → index 0
    for idx in 0..=3 {
        assert_eq!(setting_current(&c, idx), 0, "row {idx} should be 'on'");
    }
    // Default temp_mode = System → index 0
    assert_eq!(setting_current(&c, 4), 0);
    // Default temp_delete = OnStart → index 1
    assert_eq!(setting_current(&c, 5), 1);
}

#[test]
fn current_index_reflects_off_values() {
    let mut c = AppConfig::default();
    c.network = false;
    c.camera = false;
    c.microphone = false;
    c.audio = false;
    for idx in 0..=3 {
        assert_eq!(setting_current(&c, idx), 1, "row {idx} should be 'off'");
    }
}

#[test]
fn current_index_for_temp_mode_each_variant() {
    let mut c = AppConfig::default();
    c.temp_mode = TempMode::System;  assert_eq!(setting_current(&c, 4), 0);
    c.temp_mode = TempMode::Ramdisk; assert_eq!(setting_current(&c, 4), 1);
    c.temp_mode = TempMode::Local;   assert_eq!(setting_current(&c, 4), 2);
    c.temp_mode = TempMode::Uuid;    assert_eq!(setting_current(&c, 4), 3);
}

#[test]
fn current_index_for_temp_delete_each_variant() {
    let mut c = AppConfig::default();
    c.temp_delete = LocalDelete::Never;   assert_eq!(setting_current(&c, 5), 0);
    c.temp_delete = LocalDelete::OnStart; assert_eq!(setting_current(&c, 5), 1);
    c.temp_delete = LocalDelete::OnClose; assert_eq!(setting_current(&c, 5), 2);
}

// ── apply_setting: writes the right field/variant ────────────────────────────

#[test]
fn apply_writes_each_bool_row() {
    let mut c = AppConfig::default();
    apply_setting(&mut c, 0, 1); assert!(!c.network);
    apply_setting(&mut c, 0, 0); assert!(c.network);
    apply_setting(&mut c, 1, 1); assert!(!c.camera);
    apply_setting(&mut c, 2, 1); assert!(!c.microphone);
    apply_setting(&mut c, 3, 1); assert!(!c.audio);
}

#[test]
fn apply_writes_each_temp_mode_variant() {
    let mut c = AppConfig::default();
    apply_setting(&mut c, 4, 3); assert_eq!(c.temp_mode, TempMode::Uuid);
    apply_setting(&mut c, 4, 2); assert_eq!(c.temp_mode, TempMode::Local);
    apply_setting(&mut c, 4, 1); assert_eq!(c.temp_mode, TempMode::Ramdisk);
    apply_setting(&mut c, 4, 0); assert_eq!(c.temp_mode, TempMode::System);
}

#[test]
fn apply_writes_each_temp_delete_variant() {
    let mut c = AppConfig::default();
    apply_setting(&mut c, 5, 0); assert_eq!(c.temp_delete, LocalDelete::Never);
    apply_setting(&mut c, 5, 2); assert_eq!(c.temp_delete, LocalDelete::OnClose);
    apply_setting(&mut c, 5, 1); assert_eq!(c.temp_delete, LocalDelete::OnStart);
}

#[test]
fn apply_out_of_range_is_silent_noop() {
    let mut c = AppConfig::default();
    let before = c.clone();
    apply_setting(&mut c, 4, 99); // bogus choice for temp_mode
    apply_setting(&mut c, 99, 0); // bogus row
    assert_eq!(c.temp_mode, before.temp_mode);
    assert_eq!(c.temp_delete, before.temp_delete);
    assert_eq!(c.network, before.network);
}

// ── cycle_setting: forward and inverse round-trip ────────────────────────────
//
// The user requested Left as the inverse of Right. Strongest guarantee we can
// assert: cycling forward once then back once returns to the original value.

#[test]
fn cycle_forward_then_back_is_identity_for_bool_rows() {
    for idx in 0..=3 {
        let mut c = AppConfig::default();
        let before = setting_current(&c, idx);
        cycle_setting(&mut c, idx, 1);
        cycle_setting(&mut c, idx, -1);
        assert_eq!(setting_current(&c, idx), before, "row {idx} round-trip failed");
    }
}

#[test]
fn cycle_forward_then_back_is_identity_for_temp_mode() {
    let mut c = AppConfig::default();
    for start in [TempMode::System, TempMode::Ramdisk, TempMode::Local, TempMode::Uuid] {
        c.temp_mode = start.clone();
        cycle_setting(&mut c, 4, 1);
        cycle_setting(&mut c, 4, -1);
        assert_eq!(c.temp_mode, start);
    }
}

#[test]
fn cycle_forward_then_back_is_identity_for_temp_delete() {
    let mut c = AppConfig::default();
    for start in [LocalDelete::Never, LocalDelete::OnStart, LocalDelete::OnClose] {
        c.temp_delete = start.clone();
        cycle_setting(&mut c, 5, 1);
        cycle_setting(&mut c, 5, -1);
        assert_eq!(c.temp_delete, start);
    }
}

#[test]
fn cycle_forward_wraps_at_end_for_temp_mode() {
    let mut c = AppConfig::default();
    c.temp_mode = TempMode::Uuid; // last option (index 3)
    cycle_setting(&mut c, 4, 1);
    assert_eq!(c.temp_mode, TempMode::System, "Uuid → System (wrap)");
}

#[test]
fn cycle_backward_wraps_at_start_for_temp_mode() {
    let mut c = AppConfig::default();
    c.temp_mode = TempMode::System; // first option (index 0)
    cycle_setting(&mut c, 4, -1);
    assert_eq!(c.temp_mode, TempMode::Uuid, "System → Uuid (wrap)");
}

#[test]
fn cycle_backward_steps_through_temp_mode_in_reverse() {
    let mut c = AppConfig::default();
    c.temp_mode = TempMode::Uuid;
    cycle_setting(&mut c, 4, -1); assert_eq!(c.temp_mode, TempMode::Local);
    cycle_setting(&mut c, 4, -1); assert_eq!(c.temp_mode, TempMode::Ramdisk);
    cycle_setting(&mut c, 4, -1); assert_eq!(c.temp_mode, TempMode::System);
    cycle_setting(&mut c, 4, -1); assert_eq!(c.temp_mode, TempMode::Uuid); // wrap
}

#[test]
fn cycle_on_empty_options_is_noop() {
    let mut c = AppConfig::default();
    let before = c.clone();
    cycle_setting(&mut c, 6, 1);  // CFG_SHARES — no options
    cycle_setting(&mut c, 13, -1); // CFG_SAVE — no options
    assert_eq!(c.network, before.network);
    assert_eq!(c.temp_mode, before.temp_mode);
    assert_eq!(c.temp_delete, before.temp_delete);
}

// ── setting_description: each known row has a non-empty description ───────────

#[test]
fn description_for_each_picker_row_is_nonempty() {
    for idx in 0..=6 {
        let d = setting_description(idx);
        assert!(!d.is_empty(), "row {idx} should have a description");
        assert!(d.len() > 10, "row {idx} description is suspiciously short: {d:?}");
    }
}

#[test]
fn description_for_unknown_row_has_fallback() {
    let d = setting_description(999);
    assert!(!d.is_empty(), "fallback description must not be empty");
}

#[test]
fn descriptions_are_distinct() {
    let descs: Vec<&str> = (0..=6).map(setting_description).collect();
    for i in 0..descs.len() {
        for j in (i + 1)..descs.len() {
            assert_ne!(descs[i], descs[j], "rows {i} and {j} share the same description");
        }
    }
}

// ── option_description: per-choice descriptions ───────────────────────────────

#[test]
fn option_description_boolean_rows_lead_with_on_or_off() {
    // Rows 0–3 are boolean (on/off). The convention is that choice 0 starts
    // with "on" and choice 1 starts with "off" so the popup title matches.
    for idx in 0..=3 {
        assert!(
            option_description(idx, 0).starts_with("on"),
            "row {idx} choice 0 should start with 'on', got: {:?}",
            option_description(idx, 0),
        );
        assert!(
            option_description(idx, 1).starts_with("off"),
            "row {idx} choice 1 should start with 'off', got: {:?}",
            option_description(idx, 1),
        );
    }
}

#[test]
fn option_description_all_known_choices_are_nonempty() {
    for idx in 0..=5 {
        let opts = setting_options(idx);
        for (choice, opt) in opts.iter().enumerate() {
            let d = option_description(idx, choice);
            assert!(!d.is_empty(), "row {idx} choice {choice} ({opt}) has empty description");
            assert!(d.len() > 10, "row {idx} choice {choice} ({opt}) description is too short: {d:?}");
        }
    }
}

#[test]
fn option_description_contains_option_name_for_temp_mode() {
    assert!(option_description(4, 0).contains("system"),  "system choice should mention 'system'");
    assert!(option_description(4, 1).contains("ramdisk"), "ramdisk choice should mention 'ramdisk'");
    assert!(option_description(4, 2).contains("local"),   "local choice should mention 'local'");
    assert!(option_description(4, 3).contains("uuid"),    "uuid choice should mention 'uuid'");
}

#[test]
fn option_description_contains_option_name_for_temp_delete() {
    assert!(option_description(5, 0).contains("never"),    "never choice should mention 'never'");
    assert!(option_description(5, 1).contains("on_start"), "on_start choice should mention 'on_start'");
    assert!(option_description(5, 2).contains("on_close"), "on_close choice should mention 'on_close'");
}

#[test]
fn option_description_fallback_for_unknown_pair() {
    let d = option_description(99, 99);
    assert!(!d.is_empty(), "fallback must not be empty");
}

#[test]
fn option_descriptions_within_same_setting_are_distinct() {
    for idx in 0..=5 {
        let opts = setting_options(idx);
        let descs: Vec<&str> = (0..opts.len()).map(|c| option_description(idx, c)).collect();
        for i in 0..descs.len() {
            for j in (i + 1)..descs.len() {
                assert_ne!(
                    descs[i], descs[j],
                    "row {idx} choices {i} and {j} share the same option description",
                );
            }
        }
    }
}

// ── RAM limit row (index 12 = CFG_RAM_LIMIT) ──────────────────────────────────

#[test]
fn options_for_ram_limit_row_has_six_choices() {
    let opts = setting_options(12);
    assert_eq!(opts.len(), 6, "expected 6 choices for RAM limit row");
    assert_eq!(opts[0], "none");
    assert_eq!(opts[1], "512 MiB");
    assert_eq!(opts[5], "8 GiB");
}

#[test]
fn title_for_ram_limit_row_is_correct() {
    assert_eq!(setting_title(12), "RAM limit");
}

#[test]
fn description_for_ram_limit_row_is_nonempty() {
    let d = setting_description(12);
    assert!(!d.is_empty());
    assert!(d.len() > 10);
}

#[test]
fn option_descriptions_for_ram_limit_all_nonempty() {
    for choice in 0..6 {
        let d = option_description(12, choice);
        assert!(!d.is_empty(), "choice {choice} must have a description");
        assert!(d.len() > 10, "choice {choice} description is too short: {d:?}");
    }
}

#[test]
fn option_descriptions_for_ram_limit_are_distinct() {
    let descs: Vec<&str> = (0..6).map(|c| option_description(12, c)).collect();
    for i in 0..descs.len() {
        for j in (i + 1)..descs.len() {
            assert_ne!(descs[i], descs[j], "choices {i} and {j} share a description");
        }
    }
}

// ── setting_current for RAM limit ─────────────────────────────────────────────

#[test]
fn current_index_for_ram_limit_none_is_0() {
    let c = AppConfig::default();
    assert_eq!(c.ram_limit, None);
    assert_eq!(setting_current(&c, 12), 0);
}

#[test]
fn current_index_for_ram_limit_each_mib_value() {
    let cases = [(512u64, 1), (1024, 2), (2048, 3), (4096, 4), (8192, 5)];
    for (mib, expected_idx) in cases {
        let c = AppConfig { ram_limit: Some(mib), ..AppConfig::default() };
        assert_eq!(setting_current(&c, 12), expected_idx, "{mib} MiB → index {expected_idx}");
    }
}

#[test]
fn current_index_for_ram_limit_clamped_for_unusual_values() {
    // Values not in the preset list fall back to the nearest tier ≥ the value.
    // The important invariant is that the index stays in [0, 5].
    let c = AppConfig { ram_limit: Some(99999), ..AppConfig::default() };
    let idx = setting_current(&c, 12);
    assert!(idx <= 5, "index must be within the option list, got {idx}");
}

// ── apply_setting for RAM limit ───────────────────────────────────────────────

#[test]
fn apply_ram_limit_choice_0_sets_none() {
    let mut c = AppConfig { ram_limit: Some(2048), ..AppConfig::default() };
    apply_setting(&mut c, 12, 0);
    assert_eq!(c.ram_limit, None);
}

#[test]
fn apply_ram_limit_all_preset_choices() {
    let expected: &[(usize, Option<u64>)] = &[
        (0, None),
        (1, Some(512)),
        (2, Some(1024)),
        (3, Some(2048)),
        (4, Some(4096)),
        (5, Some(8192)),
    ];
    for &(choice, mib) in expected {
        let mut c = AppConfig::default();
        apply_setting(&mut c, 12, choice);
        assert_eq!(c.ram_limit, mib, "choice {choice} → {mib:?}");
    }
}

// ── cycle_setting for RAM limit ───────────────────────────────────────────────

#[test]
fn cycle_ram_limit_forward_then_back_is_identity() {
    let mut c = AppConfig { ram_limit: Some(2048), ..AppConfig::default() };
    let before = setting_current(&c, 12);
    cycle_setting(&mut c, 12, 1);
    cycle_setting(&mut c, 12, -1);
    assert_eq!(setting_current(&c, 12), before);
}

#[test]
fn cycle_ram_limit_wraps_forward_at_end() {
    // index 5 = 8192 MiB is the last choice; cycling forward wraps to 0 (none)
    let mut c = AppConfig { ram_limit: Some(8192), ..AppConfig::default() };
    cycle_setting(&mut c, 12, 1);
    assert_eq!(c.ram_limit, None, "8192 MiB → wrap → none");
}

#[test]
fn cycle_ram_limit_wraps_backward_at_start() {
    // index 0 = none; cycling backward wraps to 5 (8192 MiB)
    let mut c = AppConfig { ram_limit: None, ..AppConfig::default() };
    cycle_setting(&mut c, 12, -1);
    assert_eq!(c.ram_limit, Some(8192), "none → wrap back → 8192 MiB");
}

#[test]
fn cycle_ram_limit_forward_steps_through_all_tiers() {
    let mut c = AppConfig { ram_limit: None, ..AppConfig::default() };
    cycle_setting(&mut c, 12, 1); assert_eq!(c.ram_limit, Some(512));
    cycle_setting(&mut c, 12, 1); assert_eq!(c.ram_limit, Some(1024));
    cycle_setting(&mut c, 12, 1); assert_eq!(c.ram_limit, Some(2048));
    cycle_setting(&mut c, 12, 1); assert_eq!(c.ram_limit, Some(4096));
    cycle_setting(&mut c, 12, 1); assert_eq!(c.ram_limit, Some(8192));
    cycle_setting(&mut c, 12, 1); assert_eq!(c.ram_limit, None, "wrap back to none");
}
