//! The launcher popup's settings, search and placement — everything about it
//! that does not need a display. The window itself is `gui::popup`.
//!
//! Settings live in `~/.wryayer/popup.toml`, written with every option and
//! its explanation the first time the popup opens, so there is always a file
//! to edit rather than a list of keys to remember.

pub mod theme;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// Where on the screen the popup opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// Exact coordinates of the top-left corner, relative to the monitor.
    At(i32, i32),
}

impl Position {
    pub fn parse(s: &str) -> Option<Position> {
        if let Some((x, y)) = s.split_once(',') {
            return Some(Position::At(x.trim().parse().ok()?, y.trim().parse().ok()?));
        }
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "center" | "centre" | "middle" => Position::Center,
            "top" => Position::Top,
            "bottom" => Position::Bottom,
            "left" => Position::Left,
            "right" => Position::Right,
            "top-left" => Position::TopLeft,
            "top-right" => Position::TopRight,
            "bottom-left" => Position::BottomLeft,
            "bottom-right" => Position::BottomRight,
            _ => return None,
        })
    }

    /// The screen edges the popup sits against: (left, right, top, bottom).
    /// Coordinates count as the top-left corner; the center touches nothing.
    pub fn edges(self) -> (bool, bool, bool, bool) {
        use Position::*;
        let left = matches!(self, Left | TopLeft | BottomLeft | At(..));
        let right = matches!(self, Right | TopRight | BottomRight);
        let top = matches!(self, Top | TopLeft | TopRight | At(..));
        let bottom = matches!(self, Bottom | BottomLeft | BottomRight);
        (left, right, top, bottom)
    }

    /// Top-left corner of a `w`×`h` popup on a monitor at `(mx, my)` sized
    /// `mw`×`mh`, kept `margin` pixels off every edge it sits against.
    pub fn place(self, (mx, my, mw, mh): (i32, i32, i32, i32), (w, h): (i32, i32), margin: i32) -> (i32, i32) {
        if let Position::At(x, y) = self {
            return (mx + x, my + y);
        }
        let (left, right, top, bottom) = self.edges();
        let x = if left {
            margin
        } else if right {
            mw - w - margin
        } else {
            (mw - w) / 2
        };
        let y = if top {
            margin
        } else if bottom {
            mh - h - margin
        } else {
            (mh - h) / 2
        };
        (mx + x.max(0), my + y.max(0))
    }

    /// The animation `auto` stands for: slide in from the edge the popup sits
    /// at, fade in when it sits at none.
    fn natural_animation(self) -> Animation {
        match self.edges() {
            (_, _, true, false) => Animation::SlideDown,
            (_, _, false, true) => Animation::SlideUp,
            (true, false, false, false) => Animation::SlideRight,
            (false, true, false, false) => Animation::SlideLeft,
            _ => Animation::Fade,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Animation {
    None,
    Fade,
    SlideDown,
    SlideUp,
    SlideLeft,
    SlideRight,
    SwingDown,
    SwingUp,
    SwingLeft,
    SwingRight,
}

impl Animation {
    /// `auto` is resolved against `position`.
    pub fn parse(s: &str, position: Position) -> Option<Animation> {
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "auto" | "" => position.natural_animation(),
            "none" | "off" => Animation::None,
            "fade" | "crossfade" => Animation::Fade,
            "slide-down" => Animation::SlideDown,
            "slide-up" => Animation::SlideUp,
            "slide-left" => Animation::SlideLeft,
            "slide-right" => Animation::SlideRight,
            "swing-down" => Animation::SwingDown,
            "swing-up" => Animation::SwingUp,
            "swing-left" => Animation::SwingLeft,
            "swing-right" => Animation::SwingRight,
            _ => return None,
        })
    }
}

/// A length: pixels, or a share of the monitor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Px(i32),
    Percent(f64),
}

impl Length {
    pub fn parse(s: &str) -> Option<Length> {
        let s = s.trim();
        if let Some(p) = s.strip_suffix('%') {
            let p: f64 = p.trim().parse().ok()?;
            return (p > 0.0 && p <= 100.0).then_some(Length::Percent(p));
        }
        let px: i32 = s.strip_suffix("px").unwrap_or(s).trim().parse().ok()?;
        (px > 0).then_some(Length::Px(px))
    }

    pub fn resolve(self, of: i32) -> i32 {
        match self {
            Length::Px(px) => px,
            Length::Percent(p) => (of as f64 * p / 100.0).round() as i32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogPane {
    Right,
    Bottom,
    Off,
}

/// Everything `popup.toml` can say, resolved and checked.
#[derive(Debug, Clone)]
pub struct Settings {
    pub position: Position,
    pub margin: i32,
    pub width: Length,
    pub height: Length,
    pub animation: Animation,
    pub animation_ms: u32,
    pub log_pane: LogPane,
    pub theme: String,
    pub scheme: String,
    pub font: String,
    pub monitor: String,
    pub icon_size: i32,
    pub close_after_launch: bool,
    pub close_on_focus_loss: bool,
    pub keyboard_exclusive: bool,
}

/// The file as written — every key optional, numbers accepted where strings
/// are expected (`width = 720` as well as `width = "40%"`).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    position: Option<String>,
    margin: Option<i32>,
    width: Option<toml::Value>,
    height: Option<toml::Value>,
    animation: Option<String>,
    animation_ms: Option<u32>,
    log_pane: Option<String>,
    theme: Option<String>,
    scheme: Option<String>,
    font: Option<String>,
    monitor: Option<String>,
    icon_size: Option<i32>,
    close_after_launch: Option<bool>,
    close_on_focus_loss: Option<bool>,
    keyboard: Option<String>,
}

/// What `wryayer popup` was told on the command line; each overrides the file.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub position: Option<String>,
    pub width: Option<String>,
    pub height: Option<String>,
    pub animation: Option<String>,
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(crate::manifest::wryayer_root()?.join("popup.toml"))
}

/// Read the settings, writing the commented defaults first if there is no
/// file yet.
pub fn load(overrides: &Overrides) -> Result<Settings> {
    let path = settings_path()?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(&path, DEFAULT_FILE)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    parse(&text, overrides).with_context(|| format!("in {}", path.display()))
}

pub fn parse(text: &str, overrides: &Overrides) -> Result<Settings> {
    let raw: Raw = toml::from_str(text)?;

    let position_text = overrides.position.clone().or(raw.position).unwrap_or_else(|| "center".into());
    let position = Position::parse(&position_text).with_context(|| {
        format!(
            "position '{position_text}' — use center, top, bottom, left, right, top-left, \
             top-right, bottom-left, bottom-right, or \"X,Y\""
        )
    })?;

    let length = |cli: &Option<String>, file: Option<toml::Value>, key: &str, default: &str| -> Result<Length> {
        let text = match (cli, file) {
            (Some(s), _) => s.clone(),
            (None, Some(toml::Value::Integer(n))) => n.to_string(),
            (None, Some(toml::Value::String(s))) => s,
            (None, Some(other)) => anyhow::bail!("{key} = {other} — use pixels (720) or a percentage (\"40%\")"),
            (None, None) => default.to_string(),
        };
        Length::parse(&text)
            .with_context(|| format!("{key} '{text}' — use pixels (720) or a percentage (\"40%\")"))
    };
    let width = length(&overrides.width, raw.width, "width", "720")?;
    let height = length(&overrides.height, raw.height, "height", "460")?;

    let animation_text = overrides.animation.clone().or(raw.animation).unwrap_or_else(|| "auto".into());
    let animation = Animation::parse(&animation_text, position).with_context(|| {
        format!(
            "animation '{animation_text}' — use auto, none, fade, slide-down, slide-up, \
             slide-left, slide-right, swing-down, swing-up, swing-left or swing-right"
        )
    })?;

    let log_pane = match raw.log_pane.as_deref().unwrap_or("right") {
        "right" => LogPane::Right,
        "bottom" => LogPane::Bottom,
        "off" | "none" => LogPane::Off,
        other => anyhow::bail!("log_pane '{other}' — use right, bottom or off"),
    };
    let keyboard_exclusive = match raw.keyboard.as_deref().unwrap_or("exclusive") {
        "exclusive" => true,
        "on-demand" | "on_demand" => false,
        other => anyhow::bail!("keyboard '{other}' — use exclusive or on-demand"),
    };
    let scheme = raw.scheme.unwrap_or_else(|| "auto".into());
    if !matches!(scheme.as_str(), "auto" | "light" | "dark") {
        anyhow::bail!("scheme '{scheme}' — use auto, light or dark");
    }

    Ok(Settings {
        position,
        margin: raw.margin.unwrap_or(48).max(0),
        width,
        height,
        animation,
        animation_ms: raw.animation_ms.unwrap_or(180).min(2000),
        log_pane,
        theme: raw.theme.unwrap_or_else(|| "auto".into()),
        scheme,
        font: raw.font.unwrap_or_default(),
        monitor: raw.monitor.unwrap_or_default(),
        icon_size: raw.icon_size.unwrap_or(24).clamp(12, 128),
        close_after_launch: raw.close_after_launch.unwrap_or(true),
        close_on_focus_loss: raw.close_on_focus_loss.unwrap_or(true),
        keyboard_exclusive,
    })
}

/// Written the first time the popup opens.
pub const DEFAULT_FILE: &str = r#"# wryayer launcher popup — opened with `wryayer popup`.
# Running `wryayer popup` again while it is open closes it, so one key binding
# toggles it. In tileWin/sway, for example:
#     bindsym Mod1+F2 exec wryayer popup

# Where it opens: center, top, bottom, left, right, top-left, top-right,
# bottom-left, bottom-right — or the exact top-left corner as "X,Y".
position = "center"

# Distance in pixels from the screen edge(s) it sits against.
margin = 48

# Size: pixels, or a share of the screen ("40%").
width = 720
height = 460

# How it appears: auto, none, fade, slide-down, slide-up, slide-left,
# slide-right, swing-down, swing-up, swing-left, swing-right.
# auto slides in from the edge it sits at, and fades in when centered.
animation = "auto"
animation_ms = 180

# Where the output of the highlighted app is shown: right, bottom, or off.
log_pane = "right"

# Colours and fonts: auto (tileWin's theme under tileWin, the GTK theme
# elsewhere), gtk, tilewin, or a tileWin theme by name ("win7", "win95", …).
theme = "auto"
# light, dark, or auto (follow the desktop).
scheme = "auto"
# Font override, e.g. "Noto Sans 11". Empty = the theme's.
font = ""

# Monitor to open on, by connector name (e.g. "DP-1"). Empty = the current one.
monitor = ""

icon_size = 24
close_after_launch = true

# Wayland: "exclusive" keeps the keyboard until the popup closes; "on-demand"
# lets other windows take it, and with close_on_focus_loss the popup then
# closes. On X11 the popup always closes when another window is focused, if
# close_on_focus_loss is set.
keyboard = "exclusive"
close_on_focus_loss = true
"#;

// ── search ─────────────────────────────────────────────────────────────────

/// How well `query` matches an app described by `fields` (display name first,
/// then the names it is known by). Higher is better; None is no match.
///
/// Every word of the query has to match some field. Within a field a whole
/// match beats a prefix, a prefix beats the start of a later word, and that
/// beats the letters merely appearing in order — which is still accepted, so
/// "tbird" finds Thunderbird.
pub fn score(query: &str, fields: &[&str]) -> Option<u32> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Some(0);
    }
    let fields: Vec<String> = fields.iter().map(|f| f.to_lowercase()).collect();
    let mut total = 0;
    for word in &words {
        let best = fields
            .iter()
            .enumerate()
            .filter_map(|(i, field)| {
                // The display name is what the user reads, so it wins ties.
                word_score(word, field).map(|s| s + if i == 0 { 5 } else { 0 })
            })
            .max()?;
        total += best;
    }
    Some(total)
}

fn word_score(word: &str, field: &str) -> Option<u32> {
    if field == word {
        return Some(1000);
    }
    if field.starts_with(word) {
        return Some(800);
    }
    let boundary = field
        .match_indices(word)
        .any(|(i, _)| field[..i].ends_with([' ', '-', '_', '.', '/']));
    if boundary {
        return Some(600);
    }
    if field.contains(word) {
        return Some(400);
    }
    // Letters in order, scored down by how spread out they are.
    let mut chars = field.char_indices();
    let mut first = None;
    let mut last = 0;
    for wc in word.chars() {
        let (i, _) = chars.by_ref().find(|&(_, c)| c == wc)?;
        first.get_or_insert(i);
        last = i;
    }
    let spread = (last - first.unwrap_or(0)) as u32;
    Some(200u32.saturating_sub(spread * 4).max(10))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Settings {
        parse(DEFAULT_FILE, &Overrides::default()).unwrap()
    }

    #[test]
    fn the_file_written_on_first_open_parses_to_the_defaults() {
        let s = defaults();
        assert_eq!(s.position, Position::Center);
        assert_eq!(s.width, Length::Px(720));
        assert_eq!(s.height, Length::Px(460));
        assert_eq!(s.animation, Animation::Fade, "auto fades when centered");
        assert_eq!(s.log_pane, LogPane::Right);
        assert!(s.keyboard_exclusive);
    }

    #[test]
    fn an_empty_file_means_the_defaults_too() {
        let s = parse("", &Overrides::default()).unwrap();
        assert_eq!(s.position, Position::Center);
        assert_eq!(s.margin, 48);
    }

    #[test]
    fn the_command_line_beats_the_file() {
        let s = parse(
            "position = \"top\"\nwidth = 500\n",
            &Overrides {
                position: Some("bottom-right".into()),
                width: Some("50%".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.position, Position::BottomRight);
        assert_eq!(s.width, Length::Percent(50.0));
    }

    #[test]
    fn a_mistyped_setting_is_named_rather_than_ignored() {
        let err = parse("position = \"nowhere\"", &Overrides::default()).unwrap_err();
        assert!(format!("{err:#}").contains("nowhere"), "{err:#}");
        // An unknown key is most likely a typo of a known one.
        assert!(parse("postion = \"top\"", &Overrides::default()).is_err());
    }

    #[test]
    fn auto_slides_in_from_the_edge_the_popup_sits_at() {
        let auto = |p| Animation::parse("auto", p).unwrap();
        assert_eq!(auto(Position::Top), Animation::SlideDown);
        assert_eq!(auto(Position::BottomLeft), Animation::SlideUp);
        assert_eq!(auto(Position::Left), Animation::SlideRight);
        assert_eq!(auto(Position::Right), Animation::SlideLeft);
        assert_eq!(auto(Position::Center), Animation::Fade);
    }

    #[test]
    fn placement_keeps_the_margin_off_the_edges_it_sits_at() {
        let monitor = (1920, 0, 1920, 1080); // the second of two screens
        let size = (700, 400);
        assert_eq!(Position::Center.place(monitor, size, 40), (1920 + 610, 340));
        assert_eq!(Position::TopLeft.place(monitor, size, 40), (1920 + 40, 40));
        assert_eq!(Position::BottomRight.place(monitor, size, 40), (1920 + 1180, 640));
        assert_eq!(Position::Bottom.place(monitor, size, 40), (1920 + 610, 640));
        assert_eq!(Position::At(10, 20).place(monitor, size, 40), (1930, 20));
    }

    #[test]
    fn coordinates_are_a_position_too() {
        assert_eq!(Position::parse(" 100 , 250 "), Some(Position::At(100, 250)));
        assert_eq!(Position::parse("top_right"), Some(Position::TopRight));
        assert_eq!(Position::parse("12,x"), None);
    }

    #[test]
    fn lengths_are_pixels_or_a_share_of_the_screen() {
        assert_eq!(Length::parse("720"), Some(Length::Px(720)));
        assert_eq!(Length::parse("720px"), Some(Length::Px(720)));
        assert_eq!(Length::parse("40%").map(|l| l.resolve(1920)), Some(768));
        assert_eq!(Length::parse("0"), None);
        assert_eq!(Length::parse("150%"), None);
    }

    #[test]
    fn a_name_that_starts_with_the_query_ranks_above_one_containing_it() {
        let fire = score("fire", &["Firefox", "firefox"]).unwrap();
        let bonfire = score("fire", &["Bonfire", "bonfire"]).unwrap();
        assert!(fire > bonfire);
    }

    #[test]
    fn letters_in_order_still_find_the_app() {
        assert!(score("tbird", &["Thunderbird", "thunderbird"]).is_some());
        assert!(score("xyz", &["Thunderbird", "thunderbird"]).is_none());
    }

    #[test]
    fn every_word_must_match_but_any_field_will_do() {
        // "mail" is only in the package name, "thunder" only in the title.
        assert!(score("thunder mail", &["Thunderbird", "tb", "thunderbird-mail"]).is_some());
        assert!(score("thunder nope", &["Thunderbird", "tb"]).is_none());
    }

    #[test]
    fn an_empty_query_matches_everything_equally() {
        assert_eq!(score("  ", &["Anything"]), Some(0));
    }
}
