//! `wryayer popup` — a keyboard launcher for installed apps.
//!
//! Type to search, arrows to pick, Enter to launch, Esc to close. The app
//! under the cursor shows what it has printed while running, read from the
//! log `wryayer run` keeps for launches without a terminal (see `app_log`).
//! Running `wryayer popup` again while it is open closes it, so a single key
//! binding toggles it.
//!
//! Placing a window is the part that differs by desktop, since a Wayland
//! client may not position its own windows:
//!
//! * **Wayland with the layer-shell protocol** (sway, tileWin, Hyprland, KDE):
//!   through `gtk4-layer-shell`, loaded at runtime — it is not a build
//!   dependency, and the popup works without it. The library has to be loaded
//!   before GTK connects to the compositor, which only `LD_PRELOAD` can
//!   arrange from here, so the popup re-executes itself once with it set.
//! * **sway-compatible compositors without it**: an ordinary window, floated
//!   and moved into place over the compositor's IPC socket.
//! * **X11**: an undecorated window moved with `XMoveWindow`.
//! * Anywhere else it opens where the compositor puts it.
//!
//! The opening animation is a `GtkRevealer` inside a transparent window of
//! the final size, so it looks the same on all of them and moves nothing.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_ulong, c_void, CString};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::time::Duration;

use anyhow::Result;
use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::popup::theme::{self, Palette};
use crate::popup::{self as settings_mod, Animation, LogPane, Overrides, Position, Settings};

const APP_ID: &str = "de.synthelicz.Wryayer.Popup";

/// Set in the re-executed process, so it knows the library is loaded — and
/// what `LD_PRELOAD` was before, for the apps it launches.
const PRELOADED: &str = "WRYAYER_POPUP_PRELOADED";
const ORIGINAL_PRELOAD: &str = "WRYAYER_POPUP_ORIGINAL_PRELOAD";

/// How much of a log the pane reads; the rest is scrolled away anyway.
const LOG_TAIL_BYTES: u64 = 96 * 1024;
const LOG_MAX_LINES: usize = 600;

pub fn run(overrides: Overrides) -> Result<()> {
    preload_layer_shell();
    // Nothing here can answer a terminal prompt, and neither can what it
    // starts: the apps run detached.
    crate::prompt::forbid_here();
    let settings = settings_mod::load(&overrides)?;

    let app = gtk::Application::builder().application_id(APP_ID).build();
    let open: Rc<RefCell<Option<Rc<Ui>>>> = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        // A second `wryayer popup` lands here, in the one already open.
        let current = open.borrow().clone();
        match current {
            Some(ui) => ui.close(),
            None => *open.borrow_mut() = Some(Ui::build(app, settings.clone())),
        }
    });
    let code = app.run_with_args::<&str>(&[]);
    if code == glib::ExitCode::SUCCESS {
        Ok(())
    } else {
        anyhow::bail!("the popup exited with a non-zero status")
    }
}

// ── the list ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Item {
    name: String,
    title: String,
    /// Shown under the title: the app name when the title is something else.
    detail: String,
    /// An absolute path, or a name for the icon theme.
    icon: Option<String>,
    fs_root: String,
    running: usize,
    locked: bool,
}

/// Every app that has something to launch.
fn load_items() -> Vec<Item> {
    let running = crate::commands::run::running_instances();
    let apps = crate::manifest::list_all_apps().unwrap_or_default();
    apps.into_iter()
        .filter(|m| !m.app.main_binary.is_empty() || m.app.wine_game.is_some())
        .map(|m| {
            let fs_root = m.app.alias_of.clone().unwrap_or_else(|| m.app.name.clone());
            let locked = crate::veracrypt::is_locked(&fs_root);
            let (packaged, icon) = if locked {
                (None, None)
            } else {
                crate::desktop::presentation(&m.app.name)
                    .map_or((None, None), |(name, icon)| (Some(name), icon))
            };
            // A name the user gave it beats the one the package gives it.
            let title = m.app.display_name.clone().or(packaged).unwrap_or_else(|| m.app.name.clone());
            let detail = if title.eq_ignore_ascii_case(&m.app.name) {
                m.app.pkg_name.clone().unwrap_or_default()
            } else {
                m.app.name.clone()
            };
            Item {
                running: running.get(&m.app.name).copied().unwrap_or(0),
                icon: icon.or_else(|| m.app.wine_game.as_ref().map(|_| "wine".to_string())),
                name: m.app.name,
                title,
                detail,
                fs_root,
                locked,
            }
        })
        .collect()
}

/// Indices of the items matching `query`, best first. With no query the
/// running apps come first — they are the ones with something to show.
fn arrange(items: &[Item], query: &str) -> Vec<usize> {
    let mut scored: Vec<(u32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let fields = [item.title.as_str(), item.name.as_str(), item.detail.as_str()];
            settings_mod::score(query, &fields).map(|s| (s, i))
        })
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        let (a, b) = (&items[*a], &items[*b]);
        sb.cmp(sa)
            .then_with(|| (b.running > 0).cmp(&(a.running > 0)))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}

// ── the window ──────────────────────────────────────────────────────────────

struct LogWidgets {
    title: gtk::Label,
    state: gtk::Label,
    view: gtk::TextView,
    scroll: gtk::ScrolledWindow,
}

struct Ui {
    settings: Settings,
    window: gtk::ApplicationWindow,
    revealer: gtk::Revealer,
    entry: gtk::SearchEntry,
    list: gtk::ListBox,
    list_scroll: gtk::ScrolledWindow,
    log: Option<LogWidgets>,
    count: gtk::Label,
    items: RefCell<Vec<Item>>,
    /// Row index → index into `items`.
    shown: RefCell<Vec<usize>>,
    /// The query `shown` was arranged for.
    arranged_for: RefCell<String>,
    /// What the log pane last showed, so the timer only redraws on change.
    log_key: RefCell<Option<(String, Option<(u64, std::time::SystemTime)>, usize)>>,
    textures: RefCell<HashMap<String, Option<gdk::Texture>>>,
    closing: Cell<bool>,
    /// Whether the window has had focus yet — losing it only counts after.
    was_active: Cell<bool>,
}

impl Ui {
    fn build(app: &gtk::Application, settings: Settings) -> Rc<Ui> {
        let display = gdk::Display::default().expect("GTK has a display once activated");
        let monitor = pick_monitor(&display, &settings.monitor);
        let area = monitor
            .as_ref()
            .map(|m| {
                let g = m.geometry();
                (g.x(), g.y(), g.width(), g.height())
            })
            .unwrap_or((0, 0, 1920, 1080));
        let width = settings.width.resolve(area.2).min(area.2);
        let height = settings.height.resolve(area.3).min(area.3);

        let palette = theme::resolve(&settings.theme, &settings.scheme);
        if settings.theme == "gtk" || palette == Palette::gtk() {
            if let Some(gtk_settings) = gtk::Settings::default() {
                match settings.scheme.as_str() {
                    "dark" => gtk_settings.set_gtk_application_prefer_dark_theme(true),
                    "light" => gtk_settings.set_gtk_application_prefer_dark_theme(false),
                    _ => {}
                }
            }
        }
        load_css(&display, &palette, &settings);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("wryayer")
            .decorated(false)
            .resizable(false)
            .default_width(width)
            .default_height(height)
            .build();
        window.add_css_class("wryayer-popup");
        window.set_size_request(width, height);

        // ── header: title and search ────────────────────────────────────
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        header.add_css_class("wp-header");
        let title = gtk::Label::new(Some("wryayer"));
        title.add_css_class("wp-title");
        let entry = gtk::SearchEntry::new();
        entry.set_hexpand(true);
        entry.set_search_delay(0);
        entry.set_placeholder_text(Some("Type to search apps…"));
        entry.add_css_class("wp-search");
        header.append(&title);
        header.append(&entry);

        // ── the app list ────────────────────────────────────────────────
        let list = gtk::ListBox::new();
        list.add_css_class("wp-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(false);
        list.set_focusable(false);
        let empty = gtk::Label::new(Some("No app matches."));
        empty.add_css_class("wp-detail");
        empty.set_margin_top(16);
        list.set_placeholder(Some(&empty));
        let list_scroll = gtk::ScrolledWindow::new();
        list_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        list_scroll.set_child(Some(&list));
        list_scroll.set_vexpand(true);

        // ── the log pane ────────────────────────────────────────────────
        let log = (settings.log_pane != LogPane::Off).then(|| {
            let title = gtk::Label::new(None);
            title.set_xalign(0.0);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let state = gtk::Label::new(None);
            state.set_xalign(0.0);
            state.set_wrap(true);
            state.add_css_class("wp-state");
            let view = gtk::TextView::new();
            view.set_editable(false);
            view.set_cursor_visible(false);
            view.set_monospace(true);
            view.set_wrap_mode(gtk::WrapMode::WordChar);
            view.set_focusable(false);
            view.set_left_margin(10);
            view.set_right_margin(10);
            view.set_bottom_margin(8);
            view.add_css_class("wp-log");
            let scroll = gtk::ScrolledWindow::new();
            scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
            scroll.set_child(Some(&view));
            scroll.set_vexpand(true);
            LogWidgets { title, state, view, scroll }
        });

        let body = match settings.log_pane {
            LogPane::Bottom => gtk::Box::new(gtk::Orientation::Vertical, 0),
            _ => gtk::Box::new(gtk::Orientation::Horizontal, 0),
        };
        body.set_vexpand(true);
        body.append(&list_scroll);
        if let Some(log) = &log {
            let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
            side.add_css_class("wp-side");
            let head = gtk::Box::new(gtk::Orientation::Vertical, 2);
            head.add_css_class("wp-side-head");
            head.append(&log.title);
            head.append(&log.state);
            side.append(&head);
            side.append(&log.scroll);
            match settings.log_pane {
                LogPane::Bottom => {
                    side.add_css_class("bottom");
                    side.set_size_request(-1, height * 2 / 5);
                    list_scroll.set_vexpand(true);
                }
                _ => {
                    list_scroll.set_size_request(width * 2 / 5, -1);
                    side.set_hexpand(true);
                }
            }
            body.append(&side);
        } else {
            list_scroll.set_hexpand(true);
        }

        // ── footer: keys and counts ─────────────────────────────────────
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        footer.add_css_class("wp-footer");
        let hints = gtk::Label::new(Some(if log.is_some() {
            "↑↓ select   Enter launch   Shift+PgUp/PgDn scroll log   Esc close"
        } else {
            "↑↓ select   Enter launch   Esc close"
        }));
        hints.set_xalign(0.0);
        hints.set_hexpand(true);
        hints.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let count = gtk::Label::new(None);
        footer.append(&hints);
        footer.append(&count);

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("wp-frame");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_size_request(width, height);
        frame.append(&header);
        frame.append(&body);
        frame.append(&footer);

        let revealer = gtk::Revealer::builder()
            .transition_type(transition(settings.animation))
            .transition_duration(settings.animation_ms)
            .child(&frame)
            .build();
        // The revealer is only as big as the part already revealed, and has
        // to sit against the side the popup unfolds from.
        let (halign, valign) = match settings.animation {
            Animation::SlideUp | Animation::SwingUp => (gtk::Align::Fill, gtk::Align::End),
            Animation::SlideLeft | Animation::SwingLeft => (gtk::Align::End, gtk::Align::Fill),
            Animation::SlideRight | Animation::SwingRight => (gtk::Align::Start, gtk::Align::Fill),
            _ => (gtk::Align::Fill, gtk::Align::Start),
        };
        revealer.set_halign(halign);
        revealer.set_valign(valign);
        window.set_child(Some(&revealer));

        let ui = Rc::new(Ui {
            settings,
            window,
            revealer,
            entry,
            list,
            list_scroll,
            log,
            count,
            items: RefCell::new(load_items()),
            shown: RefCell::new(Vec::new()),
            arranged_for: RefCell::new(String::new()),
            log_key: RefCell::new(None),
            textures: RefCell::new(HashMap::new()),
            closing: Cell::new(false),
            was_active: Cell::new(false),
        });
        ui.refill(None);
        ui.connect();

        let placement = place(&ui.window, &ui.settings, monitor.as_ref(), area, (width, height));
        {
            let ui_weak = Rc::downgrade(&ui);
            ui.window.connect_map(move |_| {
                let ui_weak = ui_weak.clone();
                let placement = placement.clone();
                glib::idle_add_local_once(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.after_map(placement);
                    }
                });
            });
        }
        ui.window.present();
        ui.entry.grab_focus();
        ui
    }

    /// Put the window where it belongs, then let it unfold.
    fn after_map(&self, placement: Placement) {
        match placement {
            Placement::X11 { x, y } => x11_move(&self.window, x, y),
            Placement::Sway { command } => {
                // The compositor may not know the window by the time GTK
                // reports it mapped; ask again for a moment.
                let revealer = self.revealer.clone();
                let tries = Cell::new(0);
                glib::timeout_add_local(Duration::from_millis(15), move || {
                    tries.set(tries.get() + 1);
                    let placed = sway_command(&command).is_some_and(|r| r.contains("\"success\": true") || r.contains("\"success\":true"));
                    if placed || tries.get() > 20 {
                        revealer.set_reveal_child(true);
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                });
                return;
            }
            Placement::LayerShell | Placement::Default => {}
        }
        self.revealer.set_reveal_child(true);
    }

    fn connect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let with = move |f: fn(&Ui)| {
            let weak = weak.clone();
            move || {
                if let Some(ui) = weak.upgrade() {
                    f(&ui);
                }
            }
        };

        let changed = with(|ui| ui.refill(None));
        self.entry.connect_search_changed(move |_| changed());
        let launch = with(|ui| ui.launch());
        self.entry.connect_activate(move |_| launch());

        let selected = with(|ui| ui.show_log(true));
        self.list.connect_row_selected(move |_, _| selected());
        let activated = with(|ui| ui.launch());
        self.list.connect_row_activated(move |_, _| activated());

        // Keys are taken before the search entry sees them, so the arrows
        // move the selection instead of the text cursor.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(ui) = weak.upgrade() else { return glib::Propagation::Proceed };
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            match key {
                gdk::Key::Escape => ui.close(),
                gdk::Key::Up | gdk::Key::KP_Up => ui.move_selection(-1),
                gdk::Key::Down | gdk::Key::KP_Down => ui.move_selection(1),
                gdk::Key::Page_Up | gdk::Key::KP_Page_Up if shift => ui.scroll_log(-1.0),
                gdk::Key::Page_Down | gdk::Key::KP_Page_Down if shift => ui.scroll_log(1.0),
                gdk::Key::Page_Up | gdk::Key::KP_Page_Up => ui.move_selection(-8),
                gdk::Key::Page_Down | gdk::Key::KP_Page_Down => ui.move_selection(8),
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => ui.launch(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);

        // Clicking another window: the popup has done its job.
        let weak = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |window| {
            let Some(ui) = weak.upgrade() else { return };
            if window.is_active() {
                ui.was_active.set(true);
            } else if ui.was_active.get() && ui.closes_on_focus_loss() {
                ui.close();
            }
        });

        let weak = Rc::downgrade(self);
        self.revealer.connect_child_revealed_notify(move |revealer| {
            if let Some(ui) = weak.upgrade() {
                if ui.closing.get() && !revealer.is_child_revealed() {
                    ui.window.close();
                }
            }
        });

        // Apps start and stop, and running ones keep writing.
        let weak: Weak<Ui> = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(1000), move || {
            let Some(ui) = weak.upgrade() else { return glib::ControlFlow::Break };
            if ui.closing.get() {
                return glib::ControlFlow::Break;
            }
            ui.refresh_running();
            ui.show_log(false);
            glib::ControlFlow::Continue
        });
    }

    fn closes_on_focus_loss(&self) -> bool {
        // An exclusive keyboard grab never lets focus go; with one there is
        // nothing to lose, and losing it spuriously would close the popup.
        self.settings.close_on_focus_loss
            && !(layer_shell_active(&self.window) && self.settings.keyboard_exclusive)
    }

    fn close(&self) {
        if self.closing.replace(true) {
            return;
        }
        if self.settings.animation == Animation::None || !self.revealer.is_child_revealed() {
            self.window.close();
            return;
        }
        self.revealer.set_reveal_child(false);
        // In case the animation never reports its end (animations disabled
        // desktop-wide finish instantly, but a hidden window may not animate).
        let window = self.window.clone();
        glib::timeout_add_local_once(
            Duration::from_millis(self.settings.animation_ms as u64 + 400),
            move || window.close(),
        );
    }

    // ── list ────────────────────────────────────────────────────────────

    /// GTK reports a changed query on its next main-loop turn, so an Enter
    /// typed right behind the last letter arrives first; acting on the list
    /// as it stood would launch whatever the shorter query had on top.
    fn catch_up(&self) {
        if *self.arranged_for.borrow() != self.entry.text().as_str() {
            self.refill(None);
        }
    }

    fn selected_item(&self) -> Option<Item> {
        let row = self.list.selected_row()?;
        let index = *self.shown.borrow().get(row.index() as usize)?;
        self.items.borrow().get(index).cloned()
    }

    /// Rebuild the rows for the current query, keeping `keep` selected when
    /// it is still listed. A new query passes None and starts from its best
    /// match; only a refresh keeps the selection.
    fn refill(&self, keep: Option<String>) {
        let query = self.entry.text().to_string();
        let items = self.items.borrow();
        let order = arrange(&items, &query);

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut select = 0;
        for (row_index, &i) in order.iter().enumerate() {
            let item = &items[i];
            if keep.as_deref() == Some(item.name.as_str()) {
                select = row_index;
            }
            let row = gtk::ListBoxRow::new();
            row.set_focusable(false);
            row.set_child(Some(&self.row_widget(item)));
            self.list.append(&row);
        }
        let running = items.iter().filter(|i| i.running > 0).count();
        self.count.set_text(&match (order.len(), items.len(), running) {
            (shown, all, 0) if shown == all => format!("{all} apps"),
            (shown, all, 0) => format!("{shown} of {all} apps"),
            (shown, all, running) if shown == all => format!("{all} apps · {running} running"),
            (shown, all, running) => format!("{shown} of {all} apps · {running} running"),
        });
        drop(items);
        *self.shown.borrow_mut() = order;
        *self.arranged_for.borrow_mut() = query;

        match self.list.row_at_index(select as i32) {
            Some(row) => {
                self.list.select_row(Some(&row));
                self.scroll_to(&row);
            }
            None => self.show_log(true),
        }
    }

    fn row_widget(&self, item: &Item) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.append(&self.icon(item));

        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        let title = gtk::Label::new(Some(&item.title));
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.add_css_class("wp-name");
        text.append(&title);
        if !item.detail.is_empty() {
            let detail = gtk::Label::new(Some(&item.detail));
            detail.set_xalign(0.0);
            detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
            detail.add_css_class("wp-detail");
            text.append(&detail);
        }
        row.append(&text);

        let badge = match (item.locked, item.running) {
            (true, _) => Some("🔒".to_string()),
            (false, 0) => None,
            (false, 1) => Some("▶".to_string()),
            (false, n) => Some(format!("▶ {n}")),
        };
        if let Some(badge) = badge {
            let label = gtk::Label::new(Some(&badge));
            label.add_css_class("wp-badge");
            if item.running > 0 {
                label.set_tooltip_text(Some("running"));
            }
            row.append(&label);
        }
        row
    }

    fn icon(&self, item: &Item) -> gtk::Image {
        let size = self.settings.icon_size;
        let fallback = || {
            let image = gtk::Image::from_icon_name("application-x-executable");
            image.set_pixel_size(size);
            image
        };
        let Some(icon) = item.icon.as_deref() else { return fallback() };
        let image = if icon.starts_with('/') {
            let texture = self
                .textures
                .borrow_mut()
                .entry(icon.to_string())
                .or_insert_with(|| gdk::Texture::from_filename(icon).ok())
                .clone();
            match texture {
                Some(texture) => gtk::Image::from_paintable(Some(&texture)),
                None => return fallback(),
            }
        } else {
            let themed = gdk::Display::default()
                .map(|d| gtk::IconTheme::for_display(&d).has_icon(icon))
                .unwrap_or(false);
            if !themed {
                return fallback();
            }
            gtk::Image::from_icon_name(icon)
        };
        image.set_pixel_size(size);
        image
    }

    fn move_selection(&self, delta: i32) {
        self.catch_up();
        let len = self.shown.borrow().len() as i32;
        if len == 0 {
            return;
        }
        let current = self.list.selected_row().map(|r| r.index()).unwrap_or(-1);
        let next = (current + delta).clamp(0, len - 1);
        if let Some(row) = self.list.row_at_index(next) {
            self.list.select_row(Some(&row));
            self.scroll_to(&row);
        }
    }

    /// Scroll just enough to show `row`.
    fn scroll_to(&self, row: &gtk::ListBoxRow) {
        let list = self.list.clone();
        let row = row.clone();
        let adj = self.list_scroll.vadjustment();
        // Rows added a moment ago have no position until the next layout.
        glib::idle_add_local_once(move || {
            let Some(bounds) = row.compute_bounds(&list) else { return };
            let (top, bottom) = (bounds.y() as f64, (bounds.y() + bounds.height()) as f64);
            if top < adj.value() {
                adj.set_value(top);
            } else if bottom > adj.value() + adj.page_size() {
                adj.set_value(bottom - adj.page_size());
            }
        });
    }

    fn refresh_running(&self) {
        let running = crate::commands::run::running_instances();
        let changed = {
            let mut items = self.items.borrow_mut();
            let mut changed = false;
            for item in items.iter_mut() {
                let now = running.get(&item.name).copied().unwrap_or(0);
                changed |= item.running != now;
                item.running = now;
            }
            changed
        };
        if changed {
            self.refill(self.selected_item().map(|i| i.name));
        }
    }

    // ── log pane ────────────────────────────────────────────────────────

    /// Show the selected app's output. `force` redraws even when nothing has
    /// changed — for a new selection, which also starts at the bottom.
    fn show_log(&self, force: bool) {
        let Some(log) = &self.log else { return };
        let Some(item) = self.selected_item() else {
            log.title.set_text("");
            log.state.set_text("");
            log.view.buffer().set_text("");
            *self.log_key.borrow_mut() = None;
            return;
        };

        let key = (item.name.clone(), crate::app_log::stamp(&item.name), item.running);
        if !force && self.log_key.borrow().as_ref() == Some(&key) {
            return;
        }
        *self.log_key.borrow_mut() = Some(key);

        let mut lines = crate::app_log::tail(&item.name, LOG_TAIL_BYTES).unwrap_or_default();
        if lines.len() > LOG_MAX_LINES {
            lines.drain(..lines.len() - LOG_MAX_LINES);
        }
        let writer_alive = crate::app_log::started_pid(&lines)
            .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());

        let running = match item.running {
            0 => String::new(),
            1 => "running".to_string(),
            n => format!("{n} instances running"),
        };
        let (state, current) = if item.locked {
            (format!("🔒 locked — unlock it first: wryayer unlock {}", item.fs_root), false)
        } else if item.running > 0 && writer_alive {
            (running, true)
        } else if item.running > 0 {
            let earlier = if lines.is_empty() { "" } else { " Below: an earlier run." };
            (format!("{running} — started from a terminal, so its output is there.{earlier}"), false)
        } else if lines.is_empty() {
            ("not running · no output recorded yet".to_string(), false)
        } else {
            ("not running · output of its last run".to_string(), false)
        };

        let title = if item.detail.is_empty() {
            format!("<b>{}</b>", glib::markup_escape_text(&item.title))
        } else {
            format!(
                "<b>{}</b>  <span alpha='60%'>{}</span>",
                glib::markup_escape_text(&item.title),
                glib::markup_escape_text(&item.detail)
            )
        };
        log.title.set_markup(&title);
        log.state.set_text(&state);
        if current {
            log.view.remove_css_class("wp-stale");
        } else {
            log.view.add_css_class("wp-stale");
        }

        let adj = log.scroll.vadjustment();
        let at_bottom = adj.value() >= adj.upper() - adj.page_size() - 4.0;
        log.view.buffer().set_text(&lines.join("\n"));
        if force || at_bottom {
            // Following the output, like a terminal — unless the user has
            // scrolled back to read something.
            glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
        }
    }

    fn scroll_log(&self, pages: f64) {
        if let Some(log) = &self.log {
            let adj = log.scroll.vadjustment();
            adj.set_value(adj.value() + pages * adj.page_size() * 0.9);
        }
    }

    // ── launching ───────────────────────────────────────────────────────

    fn launch(&self) {
        self.catch_up();
        let Some(item) = self.selected_item() else { return };
        if item.locked {
            // Unlocking wants a password and root, and nothing here can ask
            // for either; the pane already says how.
            self.show_log(true);
            self.window.error_bell();
            return;
        }
        match spawn_run(&item.name) {
            Ok(()) if self.settings.close_after_launch => self.close(),
            Ok(()) => {}
            Err(e) => {
                if let Some(log) = &self.log {
                    log.state.set_text(&format!("could not start: {e}"));
                }
            }
        }
    }
}

/// Start `wryayer run <app>` in its own session, so it outlives the popup.
///
/// Its output goes nowhere a terminal would show, which is exactly what makes
/// `wryayer run` keep it in the app's log.
fn spawn_run(app: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    let mut cmd = Command::new(exe);
    cmd.arg("run")
        .arg(app)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::prompt::forbid_prompts(&mut cmd);
    // The layer-shell library was preloaded for the popup, not for apps.
    cmd.env_remove(PRELOADED).env_remove(ORIGINAL_PRELOAD);
    if std::env::var_os(PRELOADED).is_some() {
        match std::env::var_os(ORIGINAL_PRELOAD).filter(|p| !p.is_empty()) {
            Some(original) => cmd.env("LD_PRELOAD", original),
            None => cmd.env_remove("LD_PRELOAD"),
        };
    }
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

fn transition(animation: Animation) -> gtk::RevealerTransitionType {
    use gtk::RevealerTransitionType as T;
    match animation {
        Animation::None => T::None,
        Animation::Fade => T::Crossfade,
        Animation::SlideDown => T::SlideDown,
        Animation::SlideUp => T::SlideUp,
        Animation::SlideLeft => T::SlideLeft,
        Animation::SlideRight => T::SlideRight,
        Animation::SwingDown => T::SwingDown,
        Animation::SwingUp => T::SwingUp,
        Animation::SwingLeft => T::SwingLeft,
        Animation::SwingRight => T::SwingRight,
    }
}

fn pick_monitor(display: &gdk::Display, name: &str) -> Option<gdk::Monitor> {
    let monitors = display.monitors();
    let all: Vec<gdk::Monitor> = (0..monitors.n_items())
        .filter_map(|i| monitors.item(i).and_downcast::<gdk::Monitor>())
        .collect();
    if !name.is_empty() {
        if let Some(m) = all.iter().find(|m| m.connector().as_deref() == Some(name)) {
            return Some(m.clone());
        }
    }
    all.into_iter().next()
}

// ── styling ─────────────────────────────────────────────────────────────────

fn load_css(display: &gdk::Display, p: &Palette, s: &Settings) {
    let (family, size) = if s.font.trim().is_empty() {
        (p.font_family.clone(), p.font_size_pt)
    } else {
        theme::parse_font(&s.font)
    };
    let mut font = String::new();
    if let Some(family) = family {
        font.push_str(&format!("font-family: {family};"));
    }
    if let Some(size) = size {
        // Themes size their fonts for menus; a launcher is read at a glance.
        font.push_str(&format!("font-size: {:.1}pt;", size.max(9.0)));
    }
    let r = p.radius;
    let inner = r.saturating_sub(3).max(2);
    let css = format!(
        "
        window.wryayer-popup {{ background: none; box-shadow: none; }}
        .wp-frame {{
            background: {bg}; color: {fg};
            border: 1px solid {border}; border-radius: {r}px;
            {font}
        }}
        .wp-frame scrolledwindow, .wp-frame viewport, .wp-frame list {{ background: transparent; }}
        .wp-header {{ background: {header_bg}; color: {header_fg}; padding: 9px 12px; }}
        .wp-title {{ font-weight: bold; font-size: 1.2em; }}
        .wp-search {{
            background: {field_bg}; color: {field_fg};
            border: 1px solid {border}; border-radius: {inner}px;
            box-shadow: none; outline: none; min-height: 28px;
        }}
        .wp-search text {{ color: {field_fg}; }}
        .wp-search image {{ color: {field_fg}; opacity: 0.7; }}
        .wp-list {{ color: {fg}; }}
        .wp-list > row {{ padding: 5px 10px; border-radius: 0; color: {fg}; background: transparent; }}
        .wp-list > row:hover {{ background: alpha({hl_bg}, 0.18); }}
        .wp-list > row:selected {{ background: {hl_bg}; color: {hl_fg}; }}
        .wp-list > row:selected .wp-detail {{ color: {hl_fg}; opacity: 0.8; }}
        .wp-name {{ font-size: 1.05em; }}
        .wp-detail {{ color: {dim}; font-size: 0.85em; }}
        .wp-badge {{ font-size: 0.9em; }}
        .wp-side {{ background: {side_bg}; color: {side_fg}; border-left: 1px solid {border}; }}
        .wp-side.bottom {{ border-left: none; border-top: 1px solid {border}; }}
        .wp-side-head {{ padding: 8px 10px 6px 10px; }}
        .wp-state {{ font-size: 0.85em; opacity: 0.8; }}
        .wp-log, .wp-log text {{ background: transparent; color: {side_fg}; font-size: 0.85em; }}
        .wp-log.wp-stale text {{ color: alpha({side_fg}, 0.6); }}
        .wp-footer {{ background: {footer_bg}; color: {footer_fg}; padding: 5px 12px; font-size: 0.9em; }}
        ",
        bg = p.bg,
        fg = p.fg,
        border = p.border,
        header_bg = p.header_bg,
        header_fg = p.header_fg,
        field_bg = p.field_bg,
        field_fg = p.field_fg,
        hl_bg = p.hl_bg,
        hl_fg = p.hl_fg,
        dim = p.dim_fg,
        side_bg = p.side_bg,
        side_fg = p.side_fg,
        footer_bg = p.footer_bg,
        footer_fg = p.footer_fg,
    );
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&css);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

// ── placing the window ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Placement {
    LayerShell,
    Sway { command: String },
    X11 { x: i32, y: i32 },
    Default,
}

fn display_kind(window: &gtk::ApplicationWindow) -> &'static str {
    match WidgetExt::display(window).type_().name() {
        "GdkWaylandDisplay" => "wayland",
        "GdkX11Display" => "x11",
        _ => "other",
    }
}

/// Arrange for the window to open at its place. Called before it is shown —
/// a layer surface has to be set up before the window is realized.
fn place(
    window: &gtk::ApplicationWindow,
    settings: &Settings,
    monitor: Option<&gdk::Monitor>,
    area: (i32, i32, i32, i32),
    size: (i32, i32),
) -> Placement {
    let (x, y) = settings.position.place(area, size, settings.margin);
    match display_kind(window) {
        "wayland" => {
            if let Some(shell) = LayerShell::get() {
                shell.setup(window, settings, monitor.filter(|_| !settings.monitor.is_empty()));
                return Placement::LayerShell;
            }
            if std::env::var_os("SWAYSOCK").is_some() {
                let (w, h) = size;
                return Placement::Sway {
                    command: format!(
                        "[pid={}] floating enable, border none, sticky enable, \
                         resize set width {w} px height {h} px, move absolute position {x} {y}, focus",
                        std::process::id()
                    ),
                };
            }
            Placement::Default
        }
        "x11" => Placement::X11 { x, y },
        _ => Placement::Default,
    }
}

// Layer shell, through gtk4-layer-shell loaded at runtime.

const LAYER_OVERLAY: c_int = 3;
const EDGE_LEFT: c_int = 0;
const EDGE_RIGHT: c_int = 1;
const EDGE_TOP: c_int = 2;
const EDGE_BOTTOM: c_int = 3;
const KEYBOARD_EXCLUSIVE: c_int = 1;
const KEYBOARD_ON_DEMAND: c_int = 2;

const LAYER_SHELL_LIBS: &[&str] = &[
    "/usr/lib/libgtk4-layer-shell.so.0",
    "/usr/lib64/libgtk4-layer-shell.so.0",
    "/usr/local/lib/libgtk4-layer-shell.so.0",
    "/usr/local/lib64/libgtk4-layer-shell.so.0",
    "/usr/lib/x86_64-linux-gnu/libgtk4-layer-shell.so.0",
    "/usr/lib/aarch64-linux-gnu/libgtk4-layer-shell.so.0",
];

/// Re-execute with gtk4-layer-shell preloaded, when this is a Wayland session
/// and the library is installed. Returns only when there is nothing to do or
/// the exec failed — the popup then carries on without it.
fn preload_layer_shell() {
    if std::env::var_os(PRELOADED).is_some() {
        return;
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
    let forced_x11 = std::env::var("GDK_BACKEND").is_ok_and(|b| b.starts_with("x11"));
    if !wayland || forced_x11 {
        return;
    }
    let Some(lib) = LAYER_SHELL_LIBS.iter().find(|p| Path::new(p).exists()) else { return };
    let Ok(exe) = std::env::current_exe() else { return };
    let original = std::env::var("LD_PRELOAD").unwrap_or_default();
    let preload = if original.is_empty() { lib.to_string() } else { format!("{lib}:{original}") };
    let err = Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("LD_PRELOAD", preload)
        .env(PRELOADED, "1")
        .env(ORIGINAL_PRELOAD, original)
        .exec();
    eprintln!("warning: could not restart with {lib} preloaded: {err}");
}

struct LayerShell {
    is_supported: unsafe extern "C" fn() -> c_int,
    init_for_window: unsafe extern "C" fn(*mut c_void),
    is_layer_window: unsafe extern "C" fn(*mut c_void) -> c_int,
    set_namespace: unsafe extern "C" fn(*mut c_void, *const c_char),
    set_layer: unsafe extern "C" fn(*mut c_void, c_int),
    set_anchor: unsafe extern "C" fn(*mut c_void, c_int, c_int),
    set_margin: unsafe extern "C" fn(*mut c_void, c_int, c_int),
    set_keyboard_mode: unsafe extern "C" fn(*mut c_void, c_int),
    set_monitor: unsafe extern "C" fn(*mut c_void, *mut c_void),
}

impl LayerShell {
    /// The library's entry points, if it was preloaded and the compositor
    /// speaks the protocol.
    fn get() -> Option<LayerShell> {
        let handle = unsafe {
            libc::dlopen(c"libgtk4-layer-shell.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD)
        };
        if handle.is_null() {
            return None;
        }
        macro_rules! sym {
            ($name:literal) => {{
                let name = CString::new($name).ok()?;
                let ptr = unsafe { libc::dlsym(handle, name.as_ptr()) };
                if ptr.is_null() {
                    return None;
                }
                unsafe { std::mem::transmute::<*mut c_void, _>(ptr) }
            }};
        }
        let shell = LayerShell {
            is_supported: sym!("gtk_layer_is_supported"),
            init_for_window: sym!("gtk_layer_init_for_window"),
            is_layer_window: sym!("gtk_layer_is_layer_window"),
            set_namespace: sym!("gtk_layer_set_namespace"),
            set_layer: sym!("gtk_layer_set_layer"),
            set_anchor: sym!("gtk_layer_set_anchor"),
            set_margin: sym!("gtk_layer_set_margin"),
            set_keyboard_mode: sym!("gtk_layer_set_keyboard_mode"),
            set_monitor: sym!("gtk_layer_set_monitor"),
        };
        (unsafe { (shell.is_supported)() } != 0).then_some(shell)
    }

    fn setup(&self, window: &gtk::ApplicationWindow, settings: &Settings, monitor: Option<&gdk::Monitor>) {
        let w = window.upcast_ref::<gtk::Window>().as_ptr() as *mut c_void;
        let (left, right, top, bottom) = settings.position.edges();
        let (margin_x, margin_y) = match settings.position {
            Position::At(x, y) => (x, y),
            _ => (settings.margin, settings.margin),
        };
        unsafe {
            (self.init_for_window)(w);
            (self.set_namespace)(w, c"wryayer-popup".as_ptr());
            (self.set_layer)(w, LAYER_OVERLAY);
            for (edge, on, margin) in [
                (EDGE_LEFT, left, margin_x),
                (EDGE_RIGHT, right, margin_x),
                (EDGE_TOP, top, margin_y),
                (EDGE_BOTTOM, bottom, margin_y),
            ] {
                (self.set_anchor)(w, edge, on as c_int);
                (self.set_margin)(w, edge, if on { margin } else { 0 });
            }
            (self.set_keyboard_mode)(
                w,
                if settings.keyboard_exclusive { KEYBOARD_EXCLUSIVE } else { KEYBOARD_ON_DEMAND },
            );
            if let Some(monitor) = monitor {
                (self.set_monitor)(w, monitor.as_ptr() as *mut c_void);
            }
        }
    }
}

fn layer_shell_active(window: &gtk::ApplicationWindow) -> bool {
    LayerShell::get().is_some_and(|shell| unsafe {
        (shell.is_layer_window)(window.upcast_ref::<gtk::Window>().as_ptr() as *mut c_void) != 0
    })
}

/// Send one command over the sway-compatible IPC socket and return the reply.
fn sway_command(command: &str) -> Option<String> {
    use std::io::{Read, Write};
    let socket = std::env::var_os("SWAYSOCK")?;
    let mut stream = std::os::unix::net::UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    let mut message = b"i3-ipc".to_vec();
    message.extend((command.len() as u32).to_ne_bytes());
    message.extend(0u32.to_ne_bytes()); // RUN_COMMAND
    message.extend(command.as_bytes());
    stream.write_all(&message).ok()?;
    let mut head = [0u8; 14];
    stream.read_exact(&mut head).ok()?;
    let len = u32::from_ne_bytes(head[6..10].try_into().ok()?) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

/// Move a mapped X11 window. GDK 4 has no call for it, so Xlib is asked
/// directly; both it and GDK's X11 backend are already loaded when this runs.
fn x11_move(window: &gtk::ApplicationWindow, x: i32, y: i32) {
    let Some(surface) = window.surface() else { return };
    let display = WidgetExt::display(window);
    unsafe {
        let sym = |name: &str| {
            let name = CString::new(name).ok()?;
            let ptr = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
            (!ptr.is_null()).then_some(ptr)
        };
        let (Some(get_xid), Some(get_xdisplay), Some(move_window), Some(flush)) = (
            sym("gdk_x11_surface_get_xid"),
            sym("gdk_x11_display_get_xdisplay"),
            sym("XMoveWindow"),
            sym("XFlush"),
        ) else {
            return;
        };
        let get_xid: unsafe extern "C" fn(*mut c_void) -> c_ulong = std::mem::transmute(get_xid);
        let get_xdisplay: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
            std::mem::transmute(get_xdisplay);
        let move_window: unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_int) -> c_int =
            std::mem::transmute(move_window);
        let flush: unsafe extern "C" fn(*mut c_void) -> c_int = std::mem::transmute(flush);

        let xdisplay = get_xdisplay(display.as_ptr() as *mut c_void);
        let xid = get_xid(surface.as_ptr() as *mut c_void);
        if !xdisplay.is_null() && xid != 0 {
            move_window(xdisplay, xid, x, y);
            flush(xdisplay);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, title: &str, running: usize) -> Item {
        Item {
            name: name.into(),
            title: title.into(),
            detail: String::new(),
            icon: None,
            fs_root: name.into(),
            running,
            locked: false,
        }
    }

    #[test]
    fn with_no_query_running_apps_come_first() {
        let items = vec![item("alpha", "Alpha", 0), item("zulu", "Zulu", 1), item("mike", "Mike", 0)];
        let order: Vec<&str> = arrange(&items, "").iter().map(|&i| items[i].name.as_str()).collect();
        assert_eq!(order, ["zulu", "alpha", "mike"]);
    }

    #[test]
    fn a_query_orders_by_how_well_it_matches() {
        let items = vec![item("bonfire", "Bonfire", 1), item("firefox", "Firefox", 0)];
        let order: Vec<&str> = arrange(&items, "fire").iter().map(|&i| items[i].name.as_str()).collect();
        assert_eq!(order, ["firefox", "bonfire"]);
    }
}
