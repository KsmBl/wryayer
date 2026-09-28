//! The popup's colours and font, taken from the desktop it opens on.
//!
//! Under tileWin they come from the active tileWin theme — the one its start
//! menu is drawn with — so the popup looks like part of the same desktop: the
//! header and footer gradients, the highlight colour, the border and the
//! corner radius are the start menu's, and the dark colour scheme swaps in the
//! theme's dark palette just as it does for the menus. Anywhere else the GTK
//! theme's own colours are used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// CSS values for every part of the popup. Colours may be any CSS colour or
/// `@name` GTK colour; the `*_bg` of the header and footer may be gradients.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    pub bg: String,
    pub fg: String,
    pub hl_bg: String,
    pub hl_fg: String,
    pub dim_fg: String,
    pub border: String,
    pub header_bg: String,
    pub header_fg: String,
    pub side_bg: String,
    pub side_fg: String,
    pub footer_bg: String,
    pub footer_fg: String,
    pub field_bg: String,
    pub field_fg: String,
    pub radius: u32,
    /// CSS `font-family` list, or None for the GTK default.
    pub font_family: Option<String>,
    pub font_size_pt: Option<f64>,
}

impl Palette {
    /// The GTK theme's own colours.
    pub fn gtk() -> Palette {
        Palette {
            bg: "@theme_base_color".into(),
            fg: "@theme_text_color".into(),
            hl_bg: "@theme_selected_bg_color".into(),
            hl_fg: "@theme_selected_fg_color".into(),
            dim_fg: "alpha(@theme_text_color, 0.6)".into(),
            border: "@borders".into(),
            header_bg: "@theme_bg_color".into(),
            header_fg: "@theme_fg_color".into(),
            side_bg: "mix(@theme_base_color, @theme_bg_color, 0.5)".into(),
            side_fg: "@theme_text_color".into(),
            footer_bg: "@theme_bg_color".into(),
            footer_fg: "alpha(@theme_fg_color, 0.75)".into(),
            field_bg: "@theme_base_color".into(),
            field_fg: "@theme_text_color".into(),
            radius: 8,
            font_family: None,
            font_size_pt: None,
        }
    }
}

/// Pick the palette `theme`/`scheme` (the popup settings) ask for.
///
/// `theme` is `auto`, `gtk`, `tilewin`, or a tileWin theme name; `scheme` is
/// `auto`, `light` or `dark`. A tileWin theme that cannot be found falls back
/// to GTK rather than to nothing.
pub fn resolve(theme: &str, scheme: &str) -> Palette {
    let under_tilewin = std::env::var("XDG_CURRENT_DESKTOP")
        .map(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("tilewin")))
        .unwrap_or(false);
    let name = match theme {
        "gtk" => return Palette::gtk(),
        "auto" if !under_tilewin => return Palette::gtk(),
        "auto" | "tilewin" => active_theme(),
        name => name.to_string(),
    };
    let dark = match scheme {
        "dark" => true,
        "light" => false,
        _ => tilewin_scheme_is_dark(),
    };
    theme_dir(&name)
        .and_then(|dir| load(&dir, 0))
        .map(|conf| from_tilewin(&conf, dark))
        .unwrap_or_else(Palette::gtk)
}

fn config_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join("tileWin")),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".config/tileWin")),
    }
}

fn first_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// The theme of the mode tileWin is in — the same lookup `tilewin-theme
/// current` does when it cannot ask the compositor.
fn active_theme() -> String {
    let Some(config) = config_dir() else { return "win10".into() };
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")));
    let mode = state
        .and_then(|s| first_line(&s.join("tileWin/mode")))
        .filter(|m| m == "tile")
        .unwrap_or_else(|| "window".into());
    first_line(&config.join(format!("theme-{mode}")))
        .or_else(|| first_line(&config.join("current-theme")))
        .unwrap_or_else(|| "win10".into())
}

fn tilewin_scheme_is_dark() -> bool {
    config_dir()
        .and_then(|c| first_line(&c.join("color-scheme")))
        .is_some_and(|s| s == "dark")
}

fn theme_dir(name: &str) -> Option<PathBuf> {
    // A name is a directory name, never a path.
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return None;
    }
    let data = std::env::var_os("TILEWIN_DATADIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    let bases = config_dir()
        .map(|c| c.join("themes"))
        .into_iter()
        .chain(data.map(|d| d.join("themes")))
        .chain(["/usr/local/share/tileWin/themes", "/usr/share/tileWin/themes"].map(PathBuf::from));
    bases
        .map(|base| base.join(name))
        .find(|dir| dir.join("theme.conf").is_file())
}

/// A theme file flattened to `block.sub.key → value`, its `inherit`ed theme
/// underneath it.
fn load(dir: &Path, depth: u32) -> Option<HashMap<String, String>> {
    let text = std::fs::read_to_string(dir.join("theme.conf")).ok()?;
    let own = parse_conf(&text);
    let mut conf = match own.get("inherit") {
        Some(parent) if depth < 8 => theme_dir(parent)
            .and_then(|d| load(&d, depth + 1))
            .unwrap_or_default(),
        _ => HashMap::new(),
    };
    conf.extend(own);
    Some(conf)
}

/// Read tileWin's sway-style theme syntax: `key value` statements ending at a
/// newline or `;`, grouped in `name { … }` blocks that may nest or sit on one
/// line. Values may be quoted. Lines starting with `#` are comments — a `#`
/// anywhere else is a colour.
pub fn parse_conf(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut path: Vec<String> = Vec::new();
    let mut words: Vec<String> = Vec::new();

    let flush = |words: &mut Vec<String>, path: &[String], out: &mut HashMap<String, String>| {
        if let Some((key, rest)) = words.split_first() {
            let mut full = path.join(".");
            if !full.is_empty() {
                full.push('.');
            }
            full.push_str(key);
            out.insert(full, rest.join(" "));
        }
        words.clear();
    };

    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let mut chars = line.chars().peekable();
        let mut word = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    for q in chars.by_ref() {
                        if q == '"' {
                            break;
                        }
                        word.push(q);
                    }
                    words.push(std::mem::take(&mut word));
                }
                '{' => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    path.push(words.first().cloned().unwrap_or_default());
                    words.clear();
                }
                '}' => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    flush(&mut words, &path, &mut out);
                    path.pop();
                }
                ';' => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    flush(&mut words, &path, &mut out);
                }
                c if c.is_whitespace() => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                c => word.push(c),
            }
        }
        if !word.is_empty() {
            words.push(word);
        }
        flush(&mut words, &path, &mut out);
    }
    out
}

/// The popup is a relative of tileWin's start menu, so that is where most of
/// its colours come from; the plain menu fills in what the start menu leaves
/// unsaid.
fn from_tilewin(conf: &HashMap<String, String>, dark: bool) -> Palette {
    // Any dark value beats every light one: a theme whose dark palette only
    // covers the plain menu must not show the start menu's light colours.
    let get = |keys: &[&str]| -> Option<String> {
        let dark_value = || keys.iter().find_map(|k| conf.get(&format!("dark.{k}")));
        let light_value = || keys.iter().find_map(|k| conf.get(*k));
        dark.then(dark_value).flatten().or_else(light_value).cloned()
    };
    let colour = |keys: &[&str], fallback: &str| {
        get(keys).and_then(|v| css_colour(&v)).unwrap_or_else(|| fallback.to_string())
    };
    let fill = |keys: &[&str], fallback: &str| {
        get(keys).and_then(|v| css_fill(&v)).unwrap_or_else(|| fallback.to_string())
    };

    let bg = colour(&["startmenu.left_bg", "menu.bg"], "@theme_base_color");
    let fg = colour(&["startmenu.left_fg", "menu.fg"], "@theme_text_color");
    let (font_family, font_size_pt) = get(&["menu.font", "panel.font"])
        .map(|f| parse_font(&f))
        .unwrap_or((None, None));

    Palette {
        hl_bg: colour(&["startmenu.hl_bg", "menu.hl_bg"], "@theme_selected_bg_color"),
        hl_fg: colour(&["startmenu.hl_fg", "menu.hl_fg"], "@theme_selected_fg_color"),
        dim_fg: colour(&["menu.disabled_fg"], "alpha(currentColor, 0.6)"),
        border: colour(&["startmenu.border", "menu.border"], "@borders"),
        header_bg: fill(&["startmenu.header_gradient", "startmenu.header_bg"], &bg),
        header_fg: colour(&["startmenu.header_fg"], &fg),
        side_bg: colour(&["startmenu.right_bg", "menu.sidebar"], &bg),
        side_fg: colour(&["startmenu.right_fg"], &fg),
        footer_bg: fill(&["startmenu.footer_gradient", "startmenu.footer_bg"], &bg),
        footer_fg: colour(&["startmenu.footer_fg"], &fg),
        field_bg: colour(&["menu.field_bg"], &bg),
        field_fg: colour(&["menu.field_fg"], &fg),
        radius: get(&["startmenu.radius", "menu.radius"])
            .and_then(|r| r.parse().ok())
            .unwrap_or(6),
        font_family,
        font_size_pt,
        bg,
        fg,
    }
}

/// `#rgb`, `#rrggbb` or `#rrggbbaa` as CSS; names and `rgb()` pass through.
pub fn css_colour(value: &str) -> Option<String> {
    let v = value.trim();
    let Some(hex) = v.strip_prefix('#') else {
        return (!v.is_empty() && !v.contains(':')).then(|| v.to_string());
    };
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        3 | 6 => Some(format!("#{hex}")),
        8 => {
            let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
            Some(format!(
                "rgba({}, {}, {}, {:.3})",
                byte(0),
                byte(2),
                byte(4),
                byte(6) as f64 / 255.0
            ))
        }
        _ => None,
    }
}

/// A colour, or tileWin's `"0:#aaa 0.5:#bbb 1:#ccc"` gradient as a CSS
/// top-to-bottom `linear-gradient`.
pub fn css_fill(value: &str) -> Option<String> {
    if !value.contains(':') {
        return css_colour(value);
    }
    let stops: Option<Vec<String>> = value
        .split_whitespace()
        .map(|stop| {
            let (at, colour) = stop.split_once(':')?;
            let at: f64 = at.parse().ok()?;
            Some(format!("{} {}%", css_colour(colour)?, (at * 100.0).round()))
        })
        .collect();
    let stops = stops?;
    match stops.len() {
        0 => None,
        1 => stops[0].split_whitespace().next().map(str::to_string),
        _ => Some(format!("linear-gradient(to bottom, {})", stops.join(", "))),
    }
}

/// A Pango-style font description — `"Tahoma, Noto Sans Bold 8"` — as a CSS
/// family list and a point size. Style words belong to the description, not
/// the family, and are dropped.
pub fn parse_font(desc: &str) -> (Option<String>, Option<f64>) {
    let mut words: Vec<&str> = desc.split_whitespace().collect();
    let size = words.last().and_then(|w| w.parse::<f64>().ok());
    if size.is_some() {
        words.pop();
    }
    const STYLE: &[&str] = &["bold", "italic", "oblique", "light", "medium", "semibold", "heavy", "regular"];
    let families: Vec<String> = words
        .join(" ")
        .split(',')
        .map(|f| {
            f.split_whitespace()
                .filter(|w| !STYLE.contains(&w.to_ascii_lowercase().as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|f| !f.is_empty())
        .map(|f| format!("\"{f}\""))
        .collect();
    let families = (!families.is_empty()).then(|| families.join(", "));
    (families, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XP: &str = r##"
# tileWin theme: Windows XP (Luna blue)
name "Windows XP"
menu {
	font "Tahoma, Noto Sans 8"
	bg #ffffff
	fg #000000
	hl_bg #316ac5
	disabled_fg #aca899
	border #8a867a
}
startmenu {
	radius 7
	header_gradient "0:#1868ce 0.1:#3a8ef3 1:#1e5bc0"
	left_bg #ffffff
	left_fg #000000
	right_bg #d3e5fa
	border #1c52b8
}
alttab {
	bg #1b56c8f0
}
dark {
	menu {
		bg #2b2b2b
		fg #f0f0f0
	}
	tooltip { bg #2b2b2b; fg #f0f0f0; border #808080 }
}
"##;

    #[test]
    fn blocks_nest_and_colours_are_not_comments() {
        let conf = parse_conf(XP);
        assert_eq!(conf.get("name").map(String::as_str), Some("Windows XP"));
        assert_eq!(conf.get("menu.bg").map(String::as_str), Some("#ffffff"));
        assert_eq!(conf.get("dark.menu.fg").map(String::as_str), Some("#f0f0f0"));
        assert_eq!(conf.get("startmenu.header_gradient").map(String::as_str),
                   Some("0:#1868ce 0.1:#3a8ef3 1:#1e5bc0"));
    }

    #[test]
    fn a_one_line_block_splits_at_semicolons() {
        let conf = parse_conf(XP);
        assert_eq!(conf.get("dark.tooltip.bg").map(String::as_str), Some("#2b2b2b"));
        assert_eq!(conf.get("dark.tooltip.border").map(String::as_str), Some("#808080"));
    }

    #[test]
    fn the_start_menu_colours_the_popup() {
        let p = from_tilewin(&parse_conf(XP), false);
        assert_eq!(p.header_bg, "linear-gradient(to bottom, #1868ce 0%, #3a8ef3 10%, #1e5bc0 100%)");
        assert_eq!(p.side_bg, "#d3e5fa");
        assert_eq!(p.border, "#1c52b8");
        assert_eq!(p.radius, 7);
        assert_eq!(p.font_family.as_deref(), Some("\"Tahoma\", \"Noto Sans\""));
        assert_eq!(p.font_size_pt, Some(8.0));
    }

    #[test]
    fn the_dark_scheme_takes_the_dark_palette_where_it_has_one() {
        let conf = parse_conf(XP);
        // The start menu's own light list colour gives way to the dark menu's…
        let dark = from_tilewin(&conf, true);
        assert_eq!(dark.fg, "#f0f0f0");
        // …while what the dark palette leaves alone stays.
        assert_eq!(dark.hl_bg, "#316ac5");
        assert_eq!(from_tilewin(&conf, false).fg, "#000000");
    }

    #[test]
    fn eight_digit_colours_carry_their_alpha() {
        assert_eq!(css_colour("#1b56c8f0").as_deref(), Some("rgba(27, 86, 200, 0.941)"));
        assert_eq!(css_colour("#abc").as_deref(), Some("#abc"));
        assert_eq!(css_colour("#zzzzzz"), None);
    }

    #[test]
    fn a_pango_description_becomes_a_css_family_list() {
        assert_eq!(
            parse_font("Trebuchet MS, Noto Sans Bold Italic 12"),
            (Some("\"Trebuchet MS\", \"Noto Sans\"".into()), Some(12.0))
        );
        assert_eq!(parse_font("Monospace"), (Some("\"Monospace\"".into()), None));
    }

    #[test]
    fn an_inherited_theme_fills_in_what_the_child_leaves_out() {
        let _home = crate::test_support::test_home();
        let themes = PathBuf::from(std::env::var("HOME").unwrap()).join(".config/tileWin/themes");
        std::fs::create_dir_all(themes.join("base")).unwrap();
        std::fs::create_dir_all(themes.join("mine")).unwrap();
        std::fs::write(themes.join("base/theme.conf"), "menu {\n bg #111111\n fg #eeeeee\n}\n").unwrap();
        std::fs::write(themes.join("mine/theme.conf"), "inherit base\nmenu { fg #ff0000 }\n").unwrap();

        let p = from_tilewin(&load(&theme_dir("mine").unwrap(), 0).unwrap(), false);
        assert_eq!(p.bg, "#111111");
        assert_eq!(p.fg, "#ff0000");
    }

    #[test]
    fn a_theme_name_cannot_reach_outside_the_theme_directories() {
        assert_eq!(theme_dir("../../etc"), None);
        assert_eq!(theme_dir(".hidden"), None);
    }
}
