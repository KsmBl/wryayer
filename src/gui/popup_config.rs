//! `wryayer popup configurator` — the popup's settings in a window.
//!
//! On the left, a to-scale drawing of the screens with a likeness of the
//! popup in the theme's colours: click a screen to open it there, drag the
//! popup to place it anywhere. On the right, every setting `popup.toml`
//! has. Below, the `wryayer popup …` command that opens it exactly so — to
//! copy into a key binding — and buttons to try it or save it as the default.
//!
//! The command names placement and size always, and the rest only where it
//! differs from `popup.toml`, so it stays short and still means the same
//! thing while that file does.

use std::cell::{Cell, RefCell};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;

use anyhow::Result;
use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::popup::theme::{self, Palette};
use crate::popup::{self as settings_mod, Length, LogPane, Overrides, Position, Settings};

const APP_ID: &str = "de.synthelicz.Wryayer.PopupConfigurator";

/// Always part of the command: without them it would not say where the
/// popup goes, which is what it is for.
const ALWAYS: &[&str] = &["monitor", "position", "margin", "width", "height", "animation"];

const POSITIONS: &[(&str, &str)] = &[
    ("center", "Center"),
    ("top", "Top"),
    ("bottom", "Bottom"),
    ("left", "Left"),
    ("right", "Right"),
    ("top-left", "Top left"),
    ("top-right", "Top right"),
    ("bottom-left", "Bottom left"),
    ("bottom-right", "Bottom right"),
    ("", "Custom (X, Y)"),
];

pub fn run() -> Result<()> {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        // A second configurator raises the first instead of competing with it.
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        build(app);
    });
    let code = app.run_with_args::<&str>(&[]);
    if code == glib::ExitCode::SUCCESS {
        Ok(())
    } else {
        anyhow::bail!("the configurator exited with a non-zero status")
    }
}

#[derive(Clone, Debug)]
struct Monitor {
    connector: String,
    label: String,
    rect: (i32, i32, i32, i32),
}

/// Where the last drawing put things, for turning clicks back into choices.
#[derive(Clone, Debug, Default)]
struct Layout {
    scale: f64,
    /// Widget coordinates of the layout's (0, 0).
    origin: (f64, f64),
    /// The layout's top-left in compositor coordinates.
    base: (i32, i32),
    popup: (f64, f64, f64, f64),
}

struct State {
    window: gtk::ApplicationWindow,
    monitors: Vec<Monitor>,
    /// `popup.toml` as it is on disk (the defaults if there is none yet).
    file_text: RefCell<String>,
    /// Every setting as the window has it now.
    values: RefCell<Overrides>,
    palette: RefCell<Palette>,
    layout: RefCell<Layout>,
    /// Set while the window updates its own controls, whose change handlers
    /// would otherwise take that for the user choosing something.
    syncing: Cell<bool>,
    /// The popup's top-left, monitor-relative, when a drag of it began.
    drag_from: Cell<Option<(f64, f64)>>,

    preview: gtk::DrawingArea,
    command: gtk::Entry,
    status: gtk::Label,
    monitor_dd: gtk::DropDown,
    position_dd: gtk::DropDown,
    x_spin: gtk::SpinButton,
    y_spin: gtk::SpinButton,
}

impl State {
    fn settings(&self) -> Result<Settings> {
        settings_mod::parse(&self.file_text.borrow(), &self.values.borrow())
    }

    fn value(&self, key: &str) -> String {
        self.values.borrow().get(key).unwrap_or_default().to_string()
    }

    fn set(&self, key: &str, value: impl Into<String>) {
        self.values.borrow_mut().set(key, value);
    }

    /// The monitor the popup opens on — the first one when that is left to
    /// the compositor, which is the best guess the preview can make.
    fn monitor(&self) -> Option<&Monitor> {
        let name = self.value("monitor");
        self.monitors
            .iter()
            .find(|m| m.connector == name)
            .or_else(|| self.monitors.first())
    }

    /// Size and monitor-relative top-left of the popup under `settings`.
    fn popup_rect(&self, settings: &Settings) -> Option<(i32, i32, i32, i32)> {
        let m = self.monitor()?;
        let (mx, my, mw, mh) = m.rect;
        let w = settings.width.resolve(mw).min(mw);
        let h = settings.height.resolve(mh).min(mh);
        let (x, y) = settings.position.place(m.rect, (w, h), settings.margin);
        Some((x - mx, y - my, w, h))
    }

    /// The command that opens the popup as configured.
    fn command_line(&self) -> Vec<String> {
        let saved = settings_mod::values_of(&self.file_text.borrow());
        let values = self.values.borrow();
        let mut chosen = Overrides::default();
        for key in settings_mod::CLI_KEYS {
            let value = values.get(key).unwrap_or_default();
            let differs = saved.get(key).unwrap_or_default() != value;
            // An empty monitor means "the compositor's choice", which is
            // also what leaving the flag out means.
            if (ALWAYS.contains(key) && !(*key == "monitor" && value.is_empty())) || differs {
                chosen.set(key, value);
            }
        }
        chosen.to_args()
    }

    /// Everything that follows from a changed setting.
    fn changed(&self) {
        let settings = self.settings();
        match &settings {
            Ok(s) => {
                *self.palette.borrow_mut() = theme::resolve(&s.theme, &s.scheme);
                let args = self.command_line();
                let text = std::iter::once("wryayer popup".to_string())
                    .chain(args.iter().map(|a| shell_quote(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                self.command.set_text(&text);
                self.command.remove_css_class("error");
                self.status.set_text("");
                self.sync_position(s);
            }
            Err(e) => {
                self.command.add_css_class("error");
                self.status.set_text(&format!("{e:#}"));
            }
        }
        self.preview.queue_draw();
    }

    /// Show the popup's actual corner in the X/Y fields, which are only
    /// editable for a custom position.
    fn sync_position(&self, settings: &Settings) {
        let Some((x, y, ..)) = self.popup_rect(settings) else { return };
        let custom = matches!(settings.position, Position::At(..));
        self.syncing.set(true);
        self.x_spin.set_value(x as f64);
        self.y_spin.set_value(y as f64);
        self.x_spin.set_sensitive(custom);
        self.y_spin.set_sensitive(custom);
        self.syncing.set(false);
    }

    fn select_monitor(&self, connector: &str) {
        self.set("monitor", connector);
        let index = self
            .monitors
            .iter()
            .position(|m| m.connector == connector)
            .map_or(0, |i| i + 1);
        self.syncing.set(true);
        self.monitor_dd.set_selected(index as u32);
        self.syncing.set(false);
        self.changed();
    }

    fn place_at(&self, x: i32, y: i32) {
        self.set("position", format!("{x},{y}"));
        self.syncing.set(true);
        self.position_dd.set_selected((POSITIONS.len() - 1) as u32);
        self.syncing.set(false);
        self.changed();
    }
}

fn build(app: &gtk::Application) {
    let display = gdk::Display::default().expect("GTK has a display once activated");
    let monitors = list_monitors(&display);
    let file_text = settings_mod::settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| settings_mod::DEFAULT_FILE.to_string());
    let values = settings_mod::values_of(&file_text);

    let css = gtk::CssProvider::new();
    css.load_from_data(
        ".wc-command { font-family: monospace; }
         .wc-group { font-weight: bold; margin-top: 10px; }
         .wc-hint { opacity: 0.7; font-size: 0.9em; }
         .wc-status { color: #d04040; }",
    );
    gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("wryayer popup — configurator")
        .default_width(1060)
        .default_height(680)
        .build();

    let preview = gtk::DrawingArea::new();
    preview.set_hexpand(true);
    preview.set_vexpand(true);
    preview.set_content_width(480);
    preview.set_content_height(320);

    let command = gtk::Entry::new();
    command.set_editable(false);
    command.set_hexpand(true);
    command.add_css_class("wc-command");
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.add_css_class("wc-status");

    let monitor_labels: Vec<String> = std::iter::once("The compositor's choice".to_string())
        .chain(monitors.iter().map(|m| m.label.clone()))
        .collect();
    let monitor_dd = gtk::DropDown::from_strings(&monitor_labels.iter().map(String::as_str).collect::<Vec<_>>());
    let position_dd =
        gtk::DropDown::from_strings(&POSITIONS.iter().map(|(_, label)| *label).collect::<Vec<_>>());
    let x_spin = gtk::SpinButton::with_range(0.0, 16384.0, 1.0);
    let y_spin = gtk::SpinButton::with_range(0.0, 16384.0, 1.0);

    let state = Rc::new(State {
        window: window.clone(),
        monitors,
        file_text: RefCell::new(file_text),
        values: RefCell::new(values),
        palette: RefCell::new(Palette::gtk()),
        layout: RefCell::new(Layout::default()),
        syncing: Cell::new(false),
        drag_from: Cell::new(None),
        preview: preview.clone(),
        command: command.clone(),
        status: status.clone(),
        monitor_dd: monitor_dd.clone(),
        position_dd: position_dd.clone(),
        x_spin: x_spin.clone(),
        y_spin: y_spin.clone(),
    });

    // ── settings column ─────────────────────────────────────────────────
    let grid = gtk::Grid::builder().column_spacing(10).row_spacing(6).build();
    grid.set_margin_start(12);
    grid.set_margin_end(12);
    grid.set_margin_top(6);
    grid.set_margin_bottom(12);
    let row = Cell::new(0);
    let add = |label: &str, widget: &gtk::Widget| {
        let l = gtk::Label::new(Some(label));
        l.set_xalign(0.0);
        grid.attach(&l, 0, row.get(), 1, 1);
        widget.set_hexpand(true);
        grid.attach(widget, 1, row.get(), 1, 1);
        row.set(row.get() + 1);
    };
    let group = |title: &str| {
        let l = gtk::Label::new(Some(title));
        l.set_xalign(0.0);
        l.add_css_class("wc-group");
        grid.attach(&l, 0, row.get(), 2, 1);
        row.set(row.get() + 1);
    };

    group("Placement");
    {
        let current = state.value("monitor");
        let index = state.monitors.iter().position(|m| m.connector == current).map_or(0, |i| i + 1);
        monitor_dd.set_selected(index as u32);
        let st = state.clone();
        monitor_dd.connect_selected_notify(move |dd| {
            if st.syncing.get() {
                return;
            }
            let connector = match dd.selected() as usize {
                0 => String::new(),
                i => st.monitors.get(i - 1).map(|m| m.connector.clone()).unwrap_or_default(),
            };
            st.select_monitor(&connector);
        });
    }
    add("Screen", monitor_dd.upcast_ref());
    {
        let current = state.value("position");
        let index = POSITIONS
            .iter()
            .position(|(v, _)| !v.is_empty() && *v == current)
            .unwrap_or(POSITIONS.len() - 1);
        position_dd.set_selected(index as u32);
        let st = state.clone();
        position_dd.connect_selected_notify(move |dd| {
            if st.syncing.get() {
                return;
            }
            match POSITIONS.get(dd.selected() as usize) {
                Some(("", _)) => {
                    // Custom starts where the popup is now, not at 0,0.
                    let (x, y) = (st.x_spin.value() as i32, st.y_spin.value() as i32);
                    st.set("position", format!("{x},{y}"));
                }
                Some((value, _)) => st.set("position", *value),
                None => return,
            }
            st.changed();
        });
    }
    add("Position", position_dd.upcast_ref());
    let xy = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    xy.append(&gtk::Label::new(Some("X")));
    xy.append(&x_spin);
    xy.append(&gtk::Label::new(Some("Y")));
    xy.append(&y_spin);
    for spin in [&x_spin, &y_spin] {
        let st = state.clone();
        spin.connect_value_changed(move |_| {
            if !st.syncing.get() {
                st.place_at(st.x_spin.value() as i32, st.y_spin.value() as i32);
            }
        });
    }
    add("Corner", xy.upcast_ref());
    add("Margin (px)", spin(&state, "margin", 0.0, 1000.0, 1.0).upcast_ref());

    group("Size");
    add("Width", length(&state, "width", true).upcast_ref());
    add("Height", length(&state, "height", false).upcast_ref());

    group("Animation");
    add(
        "Opening",
        dropdown(
            &state,
            "animation",
            &[
                ("auto", "Auto (from the edge it sits at)"),
                ("none", "None"),
                ("fade", "Fade"),
                ("slide-down", "Slide down"),
                ("slide-up", "Slide up"),
                ("slide-left", "Slide left"),
                ("slide-right", "Slide right"),
                ("swing-down", "Swing down"),
                ("swing-up", "Swing up"),
                ("swing-left", "Swing left"),
                ("swing-right", "Swing right"),
            ],
        )
        .upcast_ref(),
    );
    add("Duration (ms)", spin(&state, "animation_ms", 0.0, 2000.0, 10.0).upcast_ref());

    group("Look");
    let mut themes: Vec<(String, String)> = vec![
        ("auto".into(), "Auto (tileWin under tileWin, else GTK)".into()),
        ("gtk".into(), "GTK theme".into()),
        ("tilewin".into(), "Active tileWin theme".into()),
    ];
    themes.extend(theme::available().into_iter().map(|t| (t.clone(), format!("tileWin: {t}"))));
    let theme_options: Vec<(&str, &str)> = themes.iter().map(|(v, l)| (v.as_str(), l.as_str())).collect();
    add("Theme", dropdown(&state, "theme", &theme_options).upcast_ref());
    add(
        "Scheme",
        dropdown(&state, "scheme", &[("auto", "Follow the desktop"), ("light", "Light"), ("dark", "Dark")])
            .upcast_ref(),
    );
    add("Font", entry(&state, "font", "the theme's font, e.g. Noto Sans 11").upcast_ref());
    add("Icon size (px)", spin(&state, "icon_size", 12.0, 128.0, 1.0).upcast_ref());
    add(
        "App output",
        dropdown(&state, "log_pane", &[("right", "Beside the list"), ("bottom", "Below the list"), ("off", "Hidden")])
            .upcast_ref(),
    );

    group("Behaviour");
    add("Close after launching", switch(&state, "close_after_launch").upcast_ref());
    add("Close when focus is lost", switch(&state, "close_on_focus_loss").upcast_ref());
    add(
        "Keyboard (Wayland)",
        dropdown(&state, "keyboard", &[("exclusive", "Keep it until closed"), ("on-demand", "Let other windows take it")])
            .upcast_ref(),
    );
    add("Stay resident (file only)", switch(&state, "resident").upcast_ref());

    let settings_scroll = gtk::ScrolledWindow::new();
    settings_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    settings_scroll.set_child(Some(&grid));
    settings_scroll.set_min_content_width(400);

    // ── preview ─────────────────────────────────────────────────────────
    let preview_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    preview_box.set_margin_start(12);
    preview_box.set_margin_top(12);
    preview_box.append(&preview);
    let hint = gtk::Label::new(Some(
        "Drag the popup to place it anywhere. Click a screen to open it there.",
    ));
    hint.add_css_class("wc-hint");
    hint.set_xalign(0.0);
    preview_box.append(&hint);
    {
        let st = state.clone();
        preview.set_draw_func(move |_, cr, w, h| draw(&st, cr, w as f64, h as f64));
    }
    connect_drag(&state);

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    top.set_vexpand(true);
    top.append(&preview_box);
    top.append(&settings_scroll);

    // ── command and actions ─────────────────────────────────────────────
    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 6);
    bottom.set_margin_start(12);
    bottom.set_margin_end(12);
    bottom.set_margin_bottom(12);
    let command_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    command_row.append(&gtk::Label::new(Some("Command")));
    command_row.append(&command);
    let copy = gtk::Button::with_label("Copy");
    let try_it = gtk::Button::with_label("Try it");
    let save = gtk::Button::with_label("Save as default");
    save.set_tooltip_text(Some("Write these settings to ~/.wryayer/popup.toml, so a plain `wryayer popup` opens it this way"));
    let close = gtk::Button::with_label("Close");
    command_row.append(&copy);
    command_row.append(&try_it);
    command_row.append(&save);
    command_row.append(&close);
    bottom.append(&command_row);
    bottom.append(&status);
    {
        let st = state.clone();
        copy.connect_clicked(move |_| {
            st.window.clipboard().set_text(&st.command.text());
            st.status.set_text("Copied.");
        });
    }
    {
        let st = state.clone();
        try_it.connect_clicked(move |_| match st.settings() {
            Ok(_) => {
                if let Err(e) = spawn_popup(&st.command_line()) {
                    st.status.set_text(&format!("could not open the popup: {e}"));
                }
            }
            Err(e) => st.status.set_text(&format!("{e:#}")),
        });
    }
    {
        let st = state.clone();
        save.connect_clicked(move |_| save_defaults(&st));
    }
    {
        let window = window.clone();
        close.connect_clicked(move |_| window.close());
    }

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.append(&top);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&bottom);
    window.set_child(Some(&root));
    state.changed();
    window.present();
}

fn list_monitors(display: &gdk::Display) -> Vec<Monitor> {
    let model = display.monitors();
    (0..model.n_items())
        .filter_map(|i| model.item(i).and_downcast::<gdk::Monitor>())
        .map(|m| {
            let g = m.geometry();
            let connector = m.connector().map(|c| c.to_string()).unwrap_or_default();
            let model_name = m.model().map(|c| c.to_string()).unwrap_or_default();
            let label = format!(
                "{connector}{}{} — {}×{} at {},{}",
                if model_name.is_empty() { "" } else { " " },
                model_name,
                g.width(),
                g.height(),
                g.x(),
                g.y()
            );
            Monitor { connector, label, rect: (g.x(), g.y(), g.width(), g.height()) }
        })
        .collect()
}

// ── controls bound to one setting each ──────────────────────────────────────

fn dropdown(state: &Rc<State>, key: &'static str, options: &[(&str, &str)]) -> gtk::DropDown {
    let mut options: Vec<(String, String)> =
        options.iter().map(|(v, l)| (v.to_string(), l.to_string())).collect();
    // A value the list does not know — a theme since removed, say — is still
    // what the file says, and is shown as such rather than silently changed.
    let current = state.value(key);
    if !options.iter().any(|(v, _)| *v == current) {
        options.push((current.clone(), current.clone()));
    }
    let labels: Vec<&str> = options.iter().map(|(_, l)| l.as_str()).collect();
    let dd = gtk::DropDown::from_strings(&labels);
    dd.set_selected(options.iter().position(|(v, _)| *v == current).unwrap_or(0) as u32);
    let st = state.clone();
    dd.connect_selected_notify(move |dd| {
        if st.syncing.get() {
            return;
        }
        if let Some((value, _)) = options.get(dd.selected() as usize) {
            st.set(key, value.clone());
            st.changed();
        }
    });
    dd
}

fn spin(state: &Rc<State>, key: &'static str, min: f64, max: f64, step: f64) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(min, max, step);
    spin.set_value(state.value(key).parse().unwrap_or(min));
    let st = state.clone();
    spin.connect_value_changed(move |spin| {
        if !st.syncing.get() {
            st.set(key, (spin.value() as i64).to_string());
            st.changed();
        }
    });
    spin
}

fn switch(state: &Rc<State>, key: &'static str) -> gtk::Box {
    let switch = gtk::Switch::new();
    switch.set_active(state.value(key) == "true");
    let st = state.clone();
    switch.connect_active_notify(move |switch| {
        if !st.syncing.get() {
            st.set(key, if switch.is_active() { "true" } else { "false" });
            st.changed();
        }
    });
    // A Switch stretched over the column would be drawn stretched.
    let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    holder.append(&switch);
    holder
}

fn entry(state: &Rc<State>, key: &'static str, placeholder: &str) -> gtk::Entry {
    let entry = gtk::Entry::new();
    entry.set_text(&state.value(key));
    entry.set_placeholder_text(Some(placeholder));
    let st = state.clone();
    entry.connect_changed(move |entry| {
        if !st.syncing.get() {
            st.set(key, entry.text().to_string());
            st.changed();
        }
    });
    entry
}

/// A length: a number and whether it is pixels or a share of the screen.
/// Switching the unit keeps the popup the size it is.
fn length(state: &Rc<State>, key: &'static str, horizontal: bool) -> gtk::Box {
    let current = Length::parse(&state.value(key)).unwrap_or(Length::Px(if horizontal { 720 } else { 460 }));
    let number = gtk::SpinButton::with_range(1.0, 16384.0, 1.0);
    let unit = gtk::DropDown::from_strings(&["px", "% of the screen"]);
    match current {
        Length::Px(px) => number.set_value(px as f64),
        Length::Percent(p) => {
            unit.set_selected(1);
            number.set_range(1.0, 100.0);
            number.set_value(p);
        }
    }
    let write = {
        let st = state.clone();
        let (number, unit) = (number.clone(), unit.clone());
        move || {
            if st.syncing.get() {
                return;
            }
            let n = number.value().round() as i64;
            st.set(key, if unit.selected() == 1 { format!("{n}%") } else { n.to_string() });
            st.changed();
        }
    };
    {
        let write = write.clone();
        number.connect_value_changed(move |_| write());
    }
    {
        let st = state.clone();
        let number = number.clone();
        unit.connect_selected_notify(move |unit| {
            let screen = st
                .monitor()
                .map(|m| if horizontal { m.rect.2 } else { m.rect.3 })
                .unwrap_or(1920) as f64;
            let n = number.value();
            st.syncing.set(true);
            if unit.selected() == 1 {
                number.set_range(1.0, 100.0);
                number.set_value((n / screen * 100.0).round().clamp(1.0, 100.0));
            } else {
                number.set_range(1.0, 16384.0);
                number.set_value((n / 100.0 * screen).round());
            }
            st.syncing.set(false);
            write();
        });
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    number.set_hexpand(true);
    row.append(&number);
    row.append(&unit);
    row
}

// ── the preview ─────────────────────────────────────────────────────────────

fn draw(state: &State, cr: &gtk::cairo::Context, w: f64, h: f64) {
    let Some(settings) = state.settings().ok() else { return };
    if state.monitors.is_empty() {
        return;
    }
    // The whole screen layout, scaled to fit.
    let (x0, y0) = state.monitors.iter().fold((i32::MAX, i32::MAX), |(x, y), m| (x.min(m.rect.0), y.min(m.rect.1)));
    let (x1, y1) = state
        .monitors
        .iter()
        .fold((i32::MIN, i32::MIN), |(x, y), m| (x.max(m.rect.0 + m.rect.2), y.max(m.rect.1 + m.rect.3)));
    let pad = 16.0;
    let scale = ((w - 2.0 * pad) / (x1 - x0) as f64).min((h - 2.0 * pad) / (y1 - y0) as f64);
    let origin = (
        (w - (x1 - x0) as f64 * scale) / 2.0,
        (h - (y1 - y0) as f64 * scale) / 2.0,
    );
    let to_widget = |x: i32, y: i32| (origin.0 + (x - x0) as f64 * scale, origin.1 + (y - y0) as f64 * scale);

    let selected = state.monitor().map(|m| m.connector.clone());
    for m in &state.monitors {
        let (x, y) = to_widget(m.rect.0, m.rect.1);
        let (mw, mh) = (m.rect.2 as f64 * scale, m.rect.3 as f64 * scale);
        let chosen = Some(&m.connector) == selected.as_ref();
        rounded(cr, x, y, mw, mh, 4.0);
        if chosen {
            cr.set_source_rgb(0.20, 0.22, 0.26);
        } else {
            cr.set_source_rgb(0.13, 0.13, 0.14);
        }
        let _ = cr.fill_preserve();
        cr.set_line_width(if chosen { 2.0 } else { 1.0 });
        if chosen {
            cr.set_source_rgb(0.24, 0.55, 0.93);
        } else {
            cr.set_source_rgb(0.35, 0.35, 0.37);
        }
        let _ = cr.stroke();
        cr.set_source_rgb(0.75, 0.75, 0.78);
        cr.set_font_size(12.0);
        cr.move_to(x + 8.0, y + 18.0);
        let _ = cr.show_text(&m.connector);
    }

    let Some(monitor) = state.monitor() else { return };
    let Some((px, py, pw, ph)) = state.popup_rect(&settings) else { return };
    let (x, y) = to_widget(monitor.rect.0 + px, monitor.rect.1 + py);
    let (w, h) = (pw as f64 * scale, ph as f64 * scale);
    *state.layout.borrow_mut() = Layout { scale, origin, base: (x0, y0), popup: (x, y, w, h) };
    draw_popup(cr, &state.palette.borrow(), &settings, (x, y, w, h), scale);
}

/// A likeness of the popup in the theme's colours: header, list, the output
/// pane where the settings put it, footer.
fn draw_popup(cr: &gtk::cairo::Context, p: &Palette, s: &Settings, (x, y, w, h): (f64, f64, f64, f64), scale: f64) {
    let rgb = |css: &str, fallback: (f64, f64, f64)| theme::first_rgb(css).unwrap_or(fallback);
    let set = |c: (f64, f64, f64)| cr.set_source_rgb(c.0, c.1, c.2);
    let bg = rgb(&p.bg, (0.17, 0.17, 0.18));
    let header = rgb(&p.header_bg, (0.24, 0.24, 0.26));
    let footer = rgb(&p.footer_bg, header);
    let side = rgb(&p.side_bg, (0.14, 0.15, 0.17));
    let hl = rgb(&p.hl_bg, (0.21, 0.52, 0.89));
    let fg = rgb(&p.fg, (0.9, 0.9, 0.9));
    let border = rgb(&p.border, (0.4, 0.4, 0.42));

    let radius = (p.radius as f64 * scale).max(2.0);
    let bar = (38.0 * scale).clamp(4.0, h / 4.0);
    let foot = (26.0 * scale).clamp(3.0, h / 6.0);

    cr.save().ok();
    rounded(cr, x, y, w, h, radius);
    cr.clip();
    set(bg);
    cr.rectangle(x, y, w, h);
    let _ = cr.fill();

    // The list, and the output pane beside or below it.
    let body = (y + bar, h - bar - foot);
    let (list_w, list_h) = match s.log_pane {
        LogPane::Right => (w * 0.4, body.1),
        LogPane::Bottom => (w, body.1 * 0.6),
        LogPane::Off => (w, body.1),
    };
    match s.log_pane {
        LogPane::Right => {
            set(side);
            cr.rectangle(x + list_w, body.0, w - list_w, body.1);
            let _ = cr.fill();
        }
        LogPane::Bottom => {
            set(side);
            cr.rectangle(x, body.0 + list_h, w, body.1 - list_h);
            let _ = cr.fill();
        }
        LogPane::Off => {}
    }
    let row_h = ((s.icon_size as f64 + 14.0) * scale).max(3.0);
    let mut row_y = body.0;
    let mut first = true;
    while row_y + row_h <= body.0 + list_h {
        if first {
            set(hl);
            cr.rectangle(x, row_y, list_w, row_h);
            let _ = cr.fill();
            first = false;
        }
        cr.set_source_rgba(fg.0, fg.1, fg.2, 0.35);
        let icon = s.icon_size as f64 * scale;
        cr.rectangle(x + 6.0 * scale, row_y + (row_h - icon) / 2.0, icon, icon);
        let _ = cr.fill();
        cr.rectangle(x + 6.0 * scale + icon + 8.0 * scale, row_y + row_h / 2.0 - 1.0, list_w * 0.45, 2.0_f64.max(3.0 * scale));
        let _ = cr.fill();
        row_y += row_h;
    }

    set(header);
    cr.rectangle(x, y, w, bar);
    let _ = cr.fill();
    set(footer);
    cr.rectangle(x, y + h - foot, w, foot);
    let _ = cr.fill();
    cr.restore().ok();

    rounded(cr, x, y, w, h, radius);
    set(border);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
}

fn rounded(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// Dragging the popup places it; a click on a screen picks that screen.
fn connect_drag(state: &Rc<State>) {
    let drag = gtk::GestureDrag::new();
    {
        let st = state.clone();
        drag.connect_drag_begin(move |_, x, y| {
            let layout = st.layout.borrow().clone();
            let (px, py, pw, ph) = layout.popup;
            let inside = x >= px && x <= px + pw && y >= py && y <= py + ph;
            let from = inside
                .then(|| st.settings().ok().and_then(|s| st.popup_rect(&s)))
                .flatten()
                .map(|(rx, ry, ..)| (rx as f64, ry as f64));
            st.drag_from.set(from);
        });
    }
    {
        let st = state.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let Some((fx, fy)) = st.drag_from.get() else { return };
            let Ok(settings) = st.settings() else { return };
            let (Some(monitor), Some((_, _, pw, ph))) = (st.monitor(), st.popup_rect(&settings)) else { return };
            let scale = st.layout.borrow().scale.max(0.01);
            let x = (fx + dx / scale).round().clamp(0.0, (monitor.rect.2 - pw).max(0) as f64);
            let y = (fy + dy / scale).round().clamp(0.0, (monitor.rect.3 - ph).max(0) as f64);
            st.place_at(x as i32, y as i32);
        });
    }
    {
        let st = state.clone();
        drag.connect_drag_end(move |gesture, dx, dy| {
            let dragged_popup = st.drag_from.take().is_some();
            if dragged_popup || dx.abs() > 3.0 || dy.abs() > 3.0 {
                return;
            }
            let Some((sx, sy)) = gesture.start_point() else { return };
            let layout = st.layout.borrow().clone();
            let at = (
                layout.base.0 as f64 + (sx - layout.origin.0) / layout.scale.max(0.01),
                layout.base.1 as f64 + (sy - layout.origin.1) / layout.scale.max(0.01),
            );
            let hit = st.monitors.iter().find(|m| {
                let (x, y, w, h) = m.rect;
                at.0 >= x as f64 && at.0 < (x + w) as f64 && at.1 >= y as f64 && at.1 < (y + h) as f64
            });
            if let Some(m) = hit.cloned() {
                st.select_monitor(&m.connector);
            }
        });
    }
    state.preview.add_controller(drag);
}

// ── actions ─────────────────────────────────────────────────────────────────

fn save_defaults(state: &State) {
    let result = settings_mod::settings_path().and_then(|path| {
        let text = settings_mod::with_values(&state.file_text.borrow(), &state.values.borrow())?;
        std::fs::write(&path, &text)?;
        Ok((path, text))
    });
    match result {
        Ok((path, text)) => {
            *state.file_text.borrow_mut() = text;
            state.changed();
            state.status.set_text(&format!(
                "Saved to {} — a plain `wryayer popup` now opens it this way.",
                path.display()
            ));
        }
        Err(e) => state.status.set_text(&format!("could not save: {e:#}")),
    }
}

fn spawn_popup(args: &[String]) -> std::io::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    let mut cmd = Command::new(exe);
    cmd.arg("popup")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The popup may stay resident; it must not be tied to this window.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.,:/%+=@".contains(&b));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_leaves_plain_values_bare() {
        assert_eq!(shell_quote("top-right"), "top-right");
        assert_eq!(shell_quote("40%"), "40%");
        assert_eq!(shell_quote("12,300"), "12,300");
        assert_eq!(shell_quote("Noto Sans 11"), "'Noto Sans 11'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
