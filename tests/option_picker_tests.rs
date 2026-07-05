// Tests intentionally build a Default config and tweak one field at a time.
#![allow(clippy::field_reassign_with_default)]
// These tests exercise the TUI; only build them when the tui feature is on.
#![cfg(feature = "tui")]

use wryayer::config::{AppConfig, AvahiMode, LocalDelete, TempMode};
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
    // CFG_SHARES (6) and CFG_SAVE (21) are handled by their own screens.
    // (14 = Avahi; 18 = Clean-cache; 19 = Theme; 20 = Layout.)
    assert!(setting_options(6).is_empty());
    assert!(setting_options(21).is_empty());
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

// ── Avahi row (shared row 14) ────────────────────────────────────────────────

#[test]
fn avahi_row_options_and_roundtrip() {
    let row = 14;
    assert_eq!(setting_options(row), vec!["stub", "host", "off"]);
    assert_eq!(setting_title(row), "Avahi mode");

    let mut cfg = AppConfig::default();
    // Default is Stub -> option index 0.
    assert_eq!(setting_current(&cfg, row), 0);
    for (choice, expected) in [(1, AvahiMode::Host), (2, AvahiMode::Off), (0, AvahiMode::Stub)] {
        apply_setting(&mut cfg, row, choice);
        assert_eq!(cfg.avahi, expected);
        assert_eq!(setting_current(&cfg, row), choice);
    }
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
    cycle_setting(&mut c, 21, -1); // CFG_SAVE — no options
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

// ── RAM limit row (index 13 = CFG_RAM_LIMIT) ──────────────────────────────────

#[test]
fn options_for_ram_limit_row_has_presets_plus_custom() {
    let opts = setting_options(13);
    assert_eq!(opts.len(), 7, "expected 6 presets + custom for RAM limit row");
    assert_eq!(opts[0], "none");
    assert_eq!(opts[1], "512 MB");
    assert_eq!(opts[5], "8 GB");
    assert_eq!(opts[6], "custom");
}

#[test]
fn title_for_ram_limit_row_is_correct() {
    assert_eq!(setting_title(13), "RAM limit");
}

#[test]
fn description_for_ram_limit_row_is_nonempty() {
    let d = setting_description(13);
    assert!(!d.is_empty());
    assert!(d.len() > 10);
}

#[test]
fn option_descriptions_for_ram_limit_all_nonempty() {
    for choice in 0..7 {
        let d = option_description(13, choice);
        assert!(!d.is_empty(), "choice {choice} must have a description");
        assert!(d.len() > 10, "choice {choice} description is too short: {d:?}");
    }
}

#[test]
fn option_descriptions_for_ram_limit_are_distinct() {
    let descs: Vec<&str> = (0..7).map(|c| option_description(13, c)).collect();
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
    assert_eq!(setting_current(&c, 13), 0);
}

#[test]
fn current_index_for_ram_limit_each_preset_value() {
    // Preset values are KiB.
    let cases = [(524288u64, 1), (1048576, 2), (2097152, 3), (4194304, 4), (8388608, 5)];
    for (kib, expected_idx) in cases {
        let c = AppConfig { ram_limit: Some(kib), ..AppConfig::default() };
        assert_eq!(setting_current(&c, 13), expected_idx, "{kib} KiB → index {expected_idx}");
    }
}

#[test]
fn current_index_for_ram_limit_non_preset_is_custom() {
    // Any value that isn't a preset maps to the "custom" index (6).
    let c = AppConfig { ram_limit: Some(99999), ..AppConfig::default() };
    assert_eq!(setting_current(&c, 13), 6);
}

// ── apply_setting for RAM limit ───────────────────────────────────────────────

#[test]
fn apply_ram_limit_choice_0_sets_none() {
    let mut c = AppConfig { ram_limit: Some(2097152), ..AppConfig::default() };
    apply_setting(&mut c, 13, 0);
    assert_eq!(c.ram_limit, None);
}

#[test]
fn apply_ram_limit_all_preset_choices() {
    // Preset values are KiB. "custom" (6) is a no-op here (it opens a text input).
    let expected: &[(usize, Option<u64>)] = &[
        (0, None),
        (1, Some(524288)),
        (2, Some(1048576)),
        (3, Some(2097152)),
        (4, Some(4194304)),
        (5, Some(8388608)),
    ];
    for &(choice, kib) in expected {
        let mut c = AppConfig::default();
        apply_setting(&mut c, 13, choice);
        assert_eq!(c.ram_limit, kib, "choice {choice} → {kib:?}");
    }
}

// ── cycle_setting for RAM limit ───────────────────────────────────────────────

#[test]
fn cycle_ram_limit_forward_then_back_is_identity() {
    let mut c = AppConfig { ram_limit: Some(2097152), ..AppConfig::default() }; // 2 GiB
    let before = setting_current(&c, 13);
    cycle_setting(&mut c, 13, 1);
    cycle_setting(&mut c, 13, -1);
    assert_eq!(setting_current(&c, 13), before);
}

#[test]
fn cycle_ram_limit_wraps_forward_past_custom_to_none() {
    // 8 GiB is the last preset; cycling forward skips "custom" and wraps to none.
    let mut c = AppConfig { ram_limit: Some(8388608), ..AppConfig::default() };
    cycle_setting(&mut c, 13, 1);
    assert_eq!(c.ram_limit, None, "8 GiB → (skip custom) → none");
}

#[test]
fn cycle_ram_limit_wraps_backward_past_custom_to_8gib() {
    // From none, cycling backward skips "custom" and lands on 8 GiB.
    let mut c = AppConfig { ram_limit: None, ..AppConfig::default() };
    cycle_setting(&mut c, 13, -1);
    assert_eq!(c.ram_limit, Some(8388608), "none → (skip custom) → 8 GiB");
}

#[test]
fn cycle_ram_limit_forward_steps_through_all_tiers() {
    let mut c = AppConfig { ram_limit: None, ..AppConfig::default() };
    cycle_setting(&mut c, 13, 1); assert_eq!(c.ram_limit, Some(524288));  // 512 MiB
    cycle_setting(&mut c, 13, 1); assert_eq!(c.ram_limit, Some(1048576)); // 1 GiB
    cycle_setting(&mut c, 13, 1); assert_eq!(c.ram_limit, Some(2097152)); // 2 GiB
    cycle_setting(&mut c, 13, 1); assert_eq!(c.ram_limit, Some(4194304)); // 4 GiB
    cycle_setting(&mut c, 13, 1); assert_eq!(c.ram_limit, Some(8388608)); // 8 GiB
    cycle_setting(&mut c, 13, 1); assert_eq!(c.ram_limit, None, "skip custom, wrap to none");
}

// ── Exhaustive cross-function consistency ────────────────────────────────────
//
// The settings rows are addressed by index across six functions
// (setting_options / _title / _description / option_description /
// setting_current / apply_setting) plus the UI. When a row is inserted every
// index shifts, and a single missed spot silently mis-maps a row. This test
// walks every picker row and asserts all six functions agree, turning that
// whole class of drift into a loud failure instead of a wrong-row-edits-wrong-
// setting bug.
#[test]
fn every_picker_row_is_cross_function_consistent() {
    use wryayer::tui::CFG_LEN;
    let base = AppConfig::default();
    for idx in 0..CFG_LEN {
        let opts = setting_options(idx);
        if opts.is_empty() {
            continue; // non-picker rows (shared dirs, save) drive their own screens
        }
        // A picker row must present a real title and description.
        assert_ne!(setting_title(idx), "Option", "row {idx}: picker row has no title");
        assert_ne!(
            setting_description(idx),
            "No description available.",
            "row {idx}: picker row has no description",
        );
        // The stored value must map to a real option index.
        let cur = setting_current(&base, idx);
        assert!(cur < opts.len(), "row {idx}: current index {cur} >= {} options", opts.len());

        for (choice, opt) in opts.iter().enumerate() {
            // Every choice must be documented.
            assert_ne!(
                option_description(idx, choice),
                "No description available.",
                "row {idx}: option {choice} ({opt}) is undocumented",
            );
            // 'input' / 'edit' / 'custom' choices defer to a text editor, so
            // apply_setting deliberately leaves the value unchanged — skip their
            // round-trip.
            if matches!(*opt, "input" | "edit" | "custom") {
                continue;
            }
            // apply_setting(idx, choice) then setting_current(idx) must return
            // `choice`: this is what fails if two functions map the same index to
            // different settings.
            let mut cfg = AppConfig::default();
            apply_setting(&mut cfg, idx, choice);
            assert_eq!(
                setting_current(&cfg, idx),
                choice,
                "row {idx} ('{}'): option {choice} ('{opt}') did not round-trip apply -> current",
                setting_title(idx),
            );
        }
    }
}
