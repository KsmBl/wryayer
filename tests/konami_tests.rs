// These tests exercise the TUI; only build them when the tui feature is on.
#![cfg(feature = "tui")]

use crossterm::event::KeyCode;
use ratatui::style::Color;
use wryayer::tui::konami::{self, Anim, Frame};
use wryayer::tui::{konami_advance, konami_status_for_toggle, parse_progress, KONAMI};

// ── parse_progress (PROGRESS n/total) ────────────────────────────────────────

#[test]
fn parse_progress_valid() {
    assert_eq!(parse_progress("PROGRESS 42/100"), Some((42, 100)));
    assert_eq!(parse_progress("PROGRESS 0/1"), Some((0, 1)));
}

#[test]
fn parse_progress_garbage_returns_none() {
    assert_eq!(parse_progress("not a progress line"), None);
    assert_eq!(parse_progress("PROGRESS abc/100"), None);
    assert_eq!(parse_progress("PROGRESS 100"), None);
    assert_eq!(parse_progress(""), None);
}

#[test]
fn parse_progress_trims_whitespace_around_numbers() {
    // The parser calls trim() on both sides of '/'; verify it actually works.
    assert_eq!(parse_progress("PROGRESS 42 / 100"), Some((42, 100)));
    assert_eq!(parse_progress("PROGRESS  0 / 1"),   Some((0, 1)));
}

// ── konami sequence FSM ──────────────────────────────────────────────────────

#[test]
fn konami_full_sequence_triggers() {
    let mut state = 0usize;
    let mut triggered = false;
    for &k in KONAMI {
        if konami_advance(&mut state, k) { triggered = true; }
    }
    assert!(triggered, "complete sequence must trigger");
    assert_eq!(state, 0, "state must reset after trigger");
}

#[test]
fn konami_wrong_key_resets_state() {
    let mut state = 0usize;
    konami_advance(&mut state, KeyCode::Up);
    konami_advance(&mut state, KeyCode::Up);
    assert_eq!(state, 2);
    konami_advance(&mut state, KeyCode::Char('x'));
    assert_eq!(state, 0, "wrong key resets the FSM");
}

#[test]
fn konami_wrong_key_that_is_also_start_advances_to_one() {
    let mut state = 5usize; // mid-sequence
    konami_advance(&mut state, KeyCode::Up);
    assert_eq!(state, 1, "Up at wrong index resets, but Up is also start → state=1");
}

#[test]
fn konami_case_insensitive_for_ba() {
    let mut state = 0usize;
    // Manually advance through arrows, then assert B/A accept either case
    for k in &KONAMI[..8] {
        konami_advance(&mut state, *k);
    }
    assert_eq!(state, 8);
    assert!(!konami_advance(&mut state, KeyCode::Char('B'))); // capital B accepted
    assert!(konami_advance(&mut state, KeyCode::Char('A')));  // capital A finishes
}

#[test]
fn konami_partial_sequence_does_not_trigger() {
    let mut state = 0usize;
    let mut triggered = false;
    for &k in &KONAMI[..KONAMI.len() - 1] {
        if konami_advance(&mut state, k) { triggered = true; }
    }
    assert!(!triggered);
    assert_ne!(state, 0);
}

// ── Frame primitives ─────────────────────────────────────────────────────────

#[test]
fn frame_blank_is_correct_size() {
    let f = Frame::blank(80, 24);
    assert_eq!(f.cells.len(), 80 * 24);
    assert!(!f.has_content());
    assert!(!f.has_color());
}

#[test]
fn frame_set_writes_cell_and_ignores_out_of_bounds() {
    let mut f = Frame::blank(10, 4);
    f.set(5, 2, 'X', Color::Red);
    assert_eq!(f.get(5, 2), ('X', Color::Red));
    // Out of bounds — must not panic and must not write
    f.set(-1, 2, 'Z', Color::Red);
    f.set(2, -1, 'Z', Color::Red);
    f.set(10, 2, 'Z', Color::Red);
    f.set(2, 4,  'Z', Color::Red);
    // Only the original write remains
    let written = f.cells.iter().filter(|(c, _)| *c != ' ').count();
    assert_eq!(written, 1);
}

// ── render dispatcher ────────────────────────────────────────────────────────

#[test]
fn render_respects_bounds() {
    for kind in [Anim::Fireworks, Anim::Matrix, Anim::Stream, Anim::Burst] {
        let f = konami::render(kind, 80, 24, 500, false, false);
        assert_eq!(f.width, 80);
        assert_eq!(f.height, 24);
        assert_eq!(f.cells.len(), 80 * 24);
    }
}

#[test]
fn render_produces_content_and_color_when_running() {
    for kind in [Anim::Fireworks, Anim::Matrix, Anim::Stream, Anim::Burst] {
        let f = konami::render(kind, 80, 24, 1500, false, false);
        assert!(f.has_content(), "{kind:?} must draw something at t=1500ms");
        assert!(f.has_color(),   "{kind:?} must use colour at t=1500ms");
    }
}

#[test]
fn render_evolves_over_time() {
    // Same animation at two different times produces different frames.
    for kind in [Anim::Fireworks, Anim::Matrix, Anim::Stream, Anim::Burst] {
        let f1 = konami::render(kind, 60, 20, 500, false, false);
        let f2 = konami::render(kind, 60, 20, 2500, false, false);
        assert_ne!(f1.cells, f2.cells, "{kind:?} must change between t=500 and t=2500");
    }
}

#[test]
fn render_handles_tiny_terminals() {
    // 3x3 is below the minimum — must not panic
    let f = konami::render(Anim::Burst, 3, 3, 1000, false, false);
    assert_eq!(f.cells.len(), 9);
}

#[test]
fn render_success_uses_green_border() {
    let f = konami::render(Anim::Fireworks, 80, 24, 5000, true, true);
    let border_has_green = f.cells.iter().any(|(_, c)| matches!(c, Color::Green));
    assert!(border_has_green, "success state must include green");
}

#[test]
fn render_failure_uses_red() {
    let f = konami::render(Anim::Burst, 80, 24, 5000, true, false);
    let has_red = f.cells.iter().any(|(_, c)| matches!(c, Color::Red));
    assert!(has_red, "failure state must include red");
}

// ── from_title routing ───────────────────────────────────────────────────────

#[test]
fn anim_from_title_routes_correctly() {
    assert_eq!(Anim::from_title("Install — firefox"),  Anim::Fireworks);
    assert_eq!(Anim::from_title("Remove — firefox"),   Anim::Explosion);
    assert_eq!(Anim::from_title("Export — firefox"),   Anim::Stream);
    assert_eq!(Anim::from_title("Snapshot — firefox"), Anim::Burst);
    assert_eq!(Anim::from_title("Something else"),     Anim::Burst);
}

#[test]
fn anim_from_title_remove_is_case_insensitive() {
    assert_eq!(Anim::from_title("Remove — x"), Anim::Explosion);
    assert_eq!(Anim::from_title("remove — x"), Anim::Explosion);
    assert_eq!(Anim::from_title("REMOVE — x"), Anim::Explosion);
}

// ── konami_status_for_toggle (statusbar dedup fix) ───────────────────────────
//
// The statusbar already renders a dedicated `★ konami mode` chip from the
// `app.konami_mode` flag. If we ALSO write that text into `app.status` the
// chip appears twice. Regression guard: activating must return empty, only
// deactivating produces user-visible text.

#[test]
fn konami_status_is_empty_when_activating() {
    assert_eq!(konami_status_for_toggle(true), "");
}

#[test]
fn konami_status_announces_when_deactivating() {
    let s = konami_status_for_toggle(false);
    assert_eq!(s, "konami mode off");
}

#[test]
fn konami_status_never_duplicates_chip_text() {
    // Whatever activating writes, it must not be the same text the statusbar
    // chip renders — otherwise both appear simultaneously.
    let chip = "★ konami mode";
    assert_ne!(konami_status_for_toggle(true), chip);
    assert!(!konami_status_for_toggle(true).contains(chip));
}

// ── Explosion animation ──────────────────────────────────────────────────────

#[test]
fn explosion_fuse_phase_paints_centre_only() {
    // 0–200 ms is the fuse phase — sparse glyphs near the centre, nothing
    // out at the corners. Use a generous canvas so we can clearly assert
    // that the corners stayed blank.
    let f = konami::render(Anim::Explosion, 80, 24, 50, false, false);
    let (corner_ch, corner_col) = f.get(0, 0);
    assert_eq!(corner_ch, ' ');
    assert!(matches!(corner_col, Color::Reset));

    // Centre should have at least one non-blank cell
    let cx = 40;
    let cy = 12;
    let mut any_painted = false;
    for dy in -2..=2 {
        for dx in -2..=2 {
            let (ch, _) = f.get((cx + dx) as u16, (cy + dy) as u16);
            if ch != ' ' {
                any_painted = true;
            }
        }
    }
    assert!(any_painted, "fuse phase should paint at least one centre cell");
}

#[test]
fn explosion_flash_phase_has_bright_core() {
    // ~300 ms in: the white-hot core should be filled
    let f = konami::render(Anim::Explosion, 80, 24, 300, false, false);
    let white_cells = f.cells.iter().filter(|(_, c)| matches!(c, Color::White)).count();
    assert!(
        white_cells > 5,
        "flash phase should fill the core with bright cells; got {white_cells}"
    );
}

#[test]
fn explosion_bloom_phase_reaches_outward() {
    // ~1 s in: shockwave + debris should have spread outward from centre.
    let f = konami::render(Anim::Explosion, 80, 24, 1000, false, false);
    let cx = 40;
    let cy = 12;
    // Look at cells outside a small central exclusion zone
    let mut far_painted = 0;
    for y in 0..24u16 {
        for x in 0..80u16 {
            let dx = x as i32 - cx;
            let dy = y as i32 - cy;
            if dx.abs() < 8 && dy.abs() < 4 {
                continue;
            }
            let (ch, _) = f.get(x, y);
            if ch != ' ' {
                far_painted += 1;
            }
        }
    }
    assert!(
        far_painted > 0,
        "bloom phase should paint cells well away from the centre"
    );
}

#[test]
fn explosion_render_is_deterministic() {
    let a = konami::render(Anim::Explosion, 60, 20, 750, false, false);
    let b = konami::render(Anim::Explosion, 60, 20, 750, false, false);
    assert_eq!(a.cells, b.cells, "render must be a pure function of inputs");
}

#[test]
fn explosion_handles_tiny_terminal_without_panic() {
    // Below the 4x4 floor — must not panic; the early-return in render
    // gives back a blank frame of the correct dimensions.
    let f = konami::render(Anim::Explosion, 3, 3, 800, false, false);
    assert_eq!(f.cells.len(), 9);
}

#[test]
fn explosion_done_flag_paints_red_border() {
    let f = konami::render(Anim::Explosion, 60, 20, 2000, true, true);
    let has_red = f.cells.iter().any(|(_, c)| matches!(c, Color::Red));
    assert!(has_red, "done state should add a red border accent");
}
