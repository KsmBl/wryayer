//! Pure-function ASCII animations for konami-mode operations. Every animation
//! is fully described by its (kind, elapsed_ms, width, height, done, success)
//! parameters — there is no per-frame state — which keeps it deterministic
//! and unit-testable.

use ratatui::style::Color;

/// Which animation to play. Picked from the operation title in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anim {
    /// Install: ascending sparks that burst into coloured fireworks.
    Fireworks,
    /// Remove: a downpour of green characters that erode away the screen.
    Matrix,
    /// Export: a horizontal rainbow particle stream with trails.
    Stream,
    /// Snapshot/Generic: a radial rainbow explosion from the centre.
    Burst,
    /// Remove (always-on): a shockwave + debris bloom centred on the popup.
    Explosion,
}

impl Anim {
    /// Pick an animation kind from an operation title in konami mode.
    pub fn from_title(title: &str) -> Self {
        let lower = title.to_ascii_lowercase();
        if lower.starts_with("install") {
            Anim::Fireworks
        } else if lower.starts_with("remove") {
            Anim::Explosion
        } else if lower.starts_with("export") {
            Anim::Stream
        } else {
            Anim::Burst
        }
    }
}

/// A rectangular grid of (glyph, colour) cells. Empty cells are `(' ', Reset)`.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<(char, Color)>,
}

impl Frame {
    pub fn blank(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            cells: vec![(' ', Color::Reset); width as usize * height as usize],
        }
    }

    pub fn set(&mut self, x: i32, y: i32, ch: char, col: Color) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = y as usize * self.width as usize + x as usize;
        self.cells[idx] = (ch, col);
    }

    pub fn get(&self, x: u16, y: u16) -> (char, Color) {
        self.cells[y as usize * self.width as usize + x as usize]
    }

    /// True if at least one cell has a non-Reset colour.
    pub fn has_color(&self) -> bool {
        self.cells.iter().any(|(_, c)| !matches!(c, Color::Reset))
    }

    /// True if at least one cell has a non-blank glyph.
    pub fn has_content(&self) -> bool {
        self.cells.iter().any(|(c, _)| *c != ' ')
    }
}

/// Render an animation frame for the given parameters.
pub fn render(
    kind: Anim,
    width: u16,
    height: u16,
    elapsed_ms: u64,
    done: bool,
    success: bool,
) -> Frame {
    let mut f = Frame::blank(width, height);
    if width < 4 || height < 4 {
        return f;
    }
    match kind {
        Anim::Fireworks => draw_fireworks(&mut f, elapsed_ms, done, success),
        Anim::Matrix    => draw_matrix(&mut f, elapsed_ms, done, success),
        Anim::Stream    => draw_stream(&mut f, elapsed_ms, done, success),
        Anim::Burst     => draw_burst(&mut f, elapsed_ms, done, success),
        Anim::Explosion => draw_explosion(&mut f, elapsed_ms, done, success),
    }
    overlay_banner(&mut f, kind, done, success);
    f
}

// ── deterministic PRNG ───────────────────────────────────────────────────────

fn hash(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn frand(seed: u64) -> f32 {
    (hash(seed) as f32 / u64::MAX as f32).clamp(0.0, 1.0)
}

fn rainbow(t: f32) -> Color {
    // Hue cycle through 6 anchor colours
    let palette = [
        Color::Red, Color::Yellow, Color::Green,
        Color::Cyan, Color::Blue, Color::Magenta,
    ];
    let idx = (t.rem_euclid(1.0) * palette.len() as f32) as usize;
    palette[idx.min(palette.len() - 1)]
}

// ── Fireworks ────────────────────────────────────────────────────────────────

fn draw_fireworks(f: &mut Frame, elapsed_ms: u64, done: bool, success: bool) {
    let w = f.width as i32;
    let h = f.height as i32;
    // Spawn one firework every ~500ms; each lives 2.5s.
    const LIFETIME_MS: u64 = 2500;
    const SPAWN_INTERVAL_MS: u64 = 500;
    const BURST_AT_MS: u64 = 1200;

    // Walk back through the spawn timeline.
    for k in 0..6 {
        let spawn = elapsed_ms.saturating_sub(k * SPAWN_INTERVAL_MS);
        let age = elapsed_ms - spawn;
        if age > LIFETIME_MS { continue; }

        let seed = spawn / SPAWN_INTERVAL_MS;
        let launch_x = (frand(seed) * (w as f32 - 4.0) + 2.0) as i32;
        let apex_y = (frand(seed ^ 0xAA) * (h as f32 * 0.3) + 2.0) as i32;
        let hue = frand(seed ^ 0x33);

        if age < BURST_AT_MS {
            // Ascent: spark trails from bottom to apex
            let prog = age as f32 / BURST_AT_MS as f32;
            let y = ((h - 1) as f32 - prog * (h as f32 - apex_y as f32 - 1.0)) as i32;
            f.set(launch_x, y, '|', Color::White);
            f.set(launch_x, (y + 1).min(h - 1), '.', Color::Yellow);
        } else {
            // Burst: 16 radial particles, fading
            let burst_age = age - BURST_AT_MS;
            let burst_max = LIFETIME_MS - BURST_AT_MS;
            let t = burst_age as f32 / burst_max as f32;
            let radius = t * (w.min(h) as f32 * 0.35);
            let glyphs = ['*', '+', '·', 'o', '✦'];
            for i in 0..18 {
                let theta = (i as f32 / 18.0) * std::f32::consts::TAU;
                let x = launch_x as f32 + theta.cos() * radius;
                let y = apex_y as f32 + theta.sin() * radius * 0.55;
                let g = glyphs[(i + spawn as usize) % glyphs.len()];
                let col = rainbow(hue + t * 0.3);
                f.set(x as i32, y as i32, g, col);
            }
        }
    }

    if done {
        flood_border(f, if success { Color::Green } else { Color::Red });
    }
}

// ── Matrix dissolve ──────────────────────────────────────────────────────────

fn draw_matrix(f: &mut Frame, elapsed_ms: u64, done: bool, success: bool) {
    let w = f.width as usize;
    let h = f.height as usize;
    let glyphs: &[char] = &[
        '0','1','ﾊ','ﾐ','ﾋ','ｰ','ｳ','ｼ','ﾅ','ﾓ','ﾆ','ｻ','ﾜ','ﾂ','ｵ','ﾘ','ｱ','ﾎ','ﾃ',
    ];
    for col in 0..w {
        let speed = 40 + (hash(col as u64) % 80) as u64;
        let head_y = ((elapsed_ms / speed) % (h as u64 * 2)) as i32;
        let trail = 6 + (hash(col as u64 ^ 0xF00D) % 8) as i32;
        for d in 0..trail {
            let y = head_y - d;
            if y < 0 || y >= h as i32 { continue; }
            let g_idx = (hash(col as u64 ^ (y as u64) ^ (elapsed_ms / 80)) as usize) % glyphs.len();
            let g = glyphs[g_idx];
            let color = if d == 0 {
                Color::White
            } else if d < trail / 2 {
                Color::Green
            } else {
                Color::DarkGray
            };
            f.set(col as i32, y, g, color);
        }
    }

    if done {
        // Erode-from-center → cleared screen on success, red flash on failure
        let mid_x = w as i32 / 2;
        let mid_y = h as i32 / 2;
        let radius = ((elapsed_ms.saturating_sub(1) % 1000) as i32 / 50).min(w as i32);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let dx = x - mid_x;
                let dy = (y - mid_y) * 2; // compensate for char aspect ratio
                let r2 = dx * dx + dy * dy;
                if r2 < radius * radius {
                    let ch = if success { ' ' } else { 'X' };
                    let col = if success { Color::Reset } else { Color::Red };
                    f.set(x, y, ch, col);
                }
            }
        }
    }
}

// ── Particle stream ──────────────────────────────────────────────────────────

fn draw_stream(f: &mut Frame, elapsed_ms: u64, done: bool, success: bool) {
    let w = f.width as i32;
    let h = f.height as i32;
    // 40 particles, each with its own launch time + y-band, looping every 3s.
    const PARTICLES: u64 = 40;
    const LOOP_MS: u64 = 3000;

    for p in 0..PARTICLES {
        let phase = (elapsed_ms + p * 73) % LOOP_MS;
        let t = phase as f32 / LOOP_MS as f32;
        let y = (frand(p ^ 0xBEEF) * (h as f32 - 2.0)) as i32 + 1;
        let x = (t * (w as f32 + 8.0)) as i32 - 4;
        let hue = (t + p as f32 / PARTICLES as f32) % 1.0;
        for trail in 0..5 {
            let tx = x - trail;
            if tx < 0 || tx >= w { continue; }
            let g = ['━', '─', '·', '·', ' '][trail as usize];
            f.set(tx, y, g, rainbow(hue - trail as f32 * 0.04));
        }
    }

    if done {
        flood_border(f, if success { Color::Cyan } else { Color::Red });
    }
}

// ── Radial burst ─────────────────────────────────────────────────────────────

fn draw_burst(f: &mut Frame, elapsed_ms: u64, done: bool, success: bool) {
    let w = f.width as i32;
    let h = f.height as i32;
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let max_r = (w.min(h * 2) as f32) / 2.0;

    const ARMS: i32 = 12;
    let rotation = elapsed_ms as f32 / 800.0;

    for ring in 0..14 {
        let r = (((elapsed_ms / 60) + ring * 4) % 60) as f32 / 60.0 * max_r;
        for arm in 0..ARMS {
            let theta = arm as f32 / ARMS as f32 * std::f32::consts::TAU + rotation;
            let x = (cx + theta.cos() * r) as i32;
            let y = (cy + theta.sin() * r * 0.5) as i32;
            let g = ['*', '+', '·', '✦', '✺'][ring as usize % 5];
            let hue = (r / max_r + arm as f32 / ARMS as f32) % 1.0;
            f.set(x, y, g, rainbow(hue));
        }
    }

    if done {
        flood_border(f, if success { Color::Magenta } else { Color::Red });
    }
}

// ── Explosion (remove operation) ─────────────────────────────────────────────
//
// Layered radial bloom centred on the popup, designed to "consume" the area
// over ~1.5 s and then settle into drifting ash. Phases (driven by elapsed_ms):
//
//   0 –  200 ms : fuse — small flicker at centre
// 200 –  500 ms : flash — bright core fills + initial shockwave forms
// 500 – 1500 ms : bloom — shockwave expands, debris flies outward, colour cools
// 1500+    ms : ash drift — particles fall and fade
//
// `done`/`success` mostly affect the banner; the explosion itself plays out by
// elapsed time so a near-instant `rm -rf` still gets a satisfying show.
fn draw_explosion(f: &mut Frame, elapsed_ms: u64, done: bool, _success: bool) {
    let w = f.width as i32;
    let h = f.height as i32;
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let max_r = (w.min(h * 2) as f32) / 2.0;

    let t_ms = elapsed_ms;

    // ── Phase 1: fuse (centre flicker) ─────────────────────────────────────
    if t_ms < 200 {
        let n = 3 + (t_ms / 30) as i32;
        for i in 0..n {
            let theta = frand((t_ms / 40) ^ i as u64) * std::f32::consts::TAU;
            let r = frand((t_ms / 40) ^ (i as u64) ^ 0xCAFE) * 1.5;
            let x = cx + theta.cos() * r;
            let y = cy + theta.sin() * r * 0.5;
            let g = ['·', '*', '`'][i as usize % 3];
            f.set(x as i32, y as i32, g, Color::Yellow);
        }
        return;
    }

    // ── Phase 2 & 3: shockwave + flying debris ─────────────────────────────
    // Shockwave radius grows fast at first, then continues outward.
    let post = (t_ms.saturating_sub(200)) as f32;
    let shock_r = (post / 22.0).min(max_r * 1.4);

    // Bright core: filled at flash time, dwindles thereafter
    let core_alive = post < 350.0;
    if core_alive {
        let core_r = (post / 18.0).min(max_r * 0.45);
        let cr = core_r as i32;
        for dy in -cr..=cr {
            for dx in -cr..=cr {
                // squashed circle to compensate for terminal char aspect
                let r2 = (dx * dx) as f32 + (dy * dy * 4) as f32;
                if r2 < (core_r * core_r) {
                    let col = if r2 < (core_r * core_r * 0.25) {
                        Color::White
                    } else if r2 < (core_r * core_r * 0.6) {
                        Color::Yellow
                    } else {
                        Color::Rgb(255, 140, 0) // burnt orange
                    };
                    let glyph = ['█', '▓', '▒', '*', '+'][(dx.unsigned_abs() as usize
                        + dy.unsigned_abs() as usize
                        + (t_ms / 50) as usize)
                        % 5];
                    f.set(cx as i32 + dx, cy as i32 + dy, glyph, col);
                }
            }
        }
    }

    // Shockwave ring — bright outline, fading as it expands
    let ring_brightness = (1.0 - (shock_r / (max_r * 1.4))).clamp(0.0, 1.0);
    let ring_col = if ring_brightness > 0.7 {
        Color::White
    } else if ring_brightness > 0.4 {
        Color::Yellow
    } else if ring_brightness > 0.2 {
        Color::Rgb(255, 100, 30)
    } else {
        Color::DarkGray
    };
    if shock_r >= 1.0 {
        draw_ellipse_outline(f, cx, cy, shock_r, 0.5, '◜', ring_col);
        draw_ellipse_outline(f, cx, cy, shock_r * 0.92, 0.5, '◞', ring_col);
    }

    // Flying debris — 36 chunks, each with own angle + speed
    const DEBRIS: u64 = 36;
    for p in 0..DEBRIS {
        let angle = frand(p ^ 0xBEEF) * std::f32::consts::TAU;
        let speed = 0.018 + frand(p ^ 0xDEAD) * 0.025;
        let r = post * speed;
        if r < 0.5 || r > max_r * 1.6 {
            continue;
        }
        // y-squashed because terminal cells are ~2:1 tall
        let x = cx + angle.cos() * r;
        let y = cy + angle.sin() * r * 0.5;
        // Trail behind each debris piece
        for tr in 0..4 {
            let tail = (r - tr as f32 * 1.4).max(0.0);
            let tx = cx + angle.cos() * tail;
            let ty = cy + angle.sin() * tail * 0.5;
            let glyph_set = ['✦', '✺', '❉', '*', '+', '·', '°', '⋅'];
            let g = glyph_set[(p as usize + tr as usize + (t_ms / 70) as usize) % glyph_set.len()];
            // Cool the trail toward smoke colours
            let cooled = r / max_r + tr as f32 * 0.18;
            let col = if cooled < 0.25 {
                Color::White
            } else if cooled < 0.45 {
                Color::Yellow
            } else if cooled < 0.7 {
                Color::Rgb(255, 100, 30)
            } else if cooled < 0.95 {
                Color::Red
            } else {
                Color::DarkGray
            };
            f.set(tx as i32, ty as i32, g, col);
        }
        // Head of the piece — brighter
        f.set(x as i32, y as i32, '✦', Color::White);
    }

    // ── Phase 4: ash drift (after the bloom) ───────────────────────────────
    if t_ms > 1500 {
        let drift_t = (t_ms - 1500) as f32;
        for p in 0..20 {
            let seed = p as u64 + 0x4AC;
            let x0 = frand(seed) * w as f32;
            let fall = (drift_t / 80.0 + frand(seed ^ 0x77) * h as f32) % h as f32;
            let ch = ['·', '⋅', '°', '`', ','][(p as usize + (t_ms / 200) as usize) % 5];
            f.set(x0 as i32, fall as i32, ch, Color::DarkGray);
        }
    }

    if done {
        flood_border(f, Color::Red);
    }
}

// Draw the outline of an ellipse (squashed circle to compensate for terminal
// cell aspect ratio) with given character. y_scale should be ~0.5 for typical
// monospace fonts where a cell is ~2× tall as wide.
fn draw_ellipse_outline(
    f: &mut Frame,
    cx: f32,
    cy: f32,
    r: f32,
    y_scale: f32,
    glyph: char,
    col: Color,
) {
    let steps = (r * 6.0).max(16.0) as i32;
    for i in 0..steps {
        let theta = i as f32 / steps as f32 * std::f32::consts::TAU;
        let x = cx + theta.cos() * r;
        let y = cy + theta.sin() * r * y_scale;
        f.set(x as i32, y as i32, glyph, col);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn flood_border(f: &mut Frame, col: Color) {
    let w = f.width as i32;
    let h = f.height as i32;
    for x in 0..w {
        f.set(x, 0, '═', col);
        f.set(x, h - 1, '═', col);
    }
    for y in 0..h {
        f.set(0, y, '║', col);
        f.set(w - 1, y, '║', col);
    }
    f.set(0,     0,     '╔', col);
    f.set(w - 1, 0,     '╗', col);
    f.set(0,     h - 1, '╚', col);
    f.set(w - 1, h - 1, '╝', col);
}

fn overlay_banner(f: &mut Frame, kind: Anim, done: bool, success: bool) {
    let h = f.height as i32;
    let w = f.width as i32;
    if h < 5 || w < 20 { return; }

    let text = if done {
        if success { "✦ SUCCESS ✦" } else { "✗ FAILED ✗" }
    } else {
        match kind {
            Anim::Fireworks => "✦ INSTALLING ✦",
            Anim::Matrix    => "✦ DELETING ✦",
            Anim::Stream    => "✦ EXPORTING ✦",
            Anim::Burst     => "✦ WORKING ✦",
            Anim::Explosion => "💥 OBLITERATING 💥",
        }
    };
    let col = if done {
        if success { Color::Green } else { Color::Red }
    } else {
        Color::White
    };
    let y = h / 2;
    let start_x = (w - text.chars().count() as i32) / 2;
    for (i, ch) in text.chars().enumerate() {
        f.set(start_x + i as i32, y, ch, col);
    }
}
