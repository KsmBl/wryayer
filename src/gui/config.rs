//! Per-app and global configuration — plain GTK forms writing the same INI the
//! CLI `wryayer config` command uses, via the library's `read_config`/`write_config`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::glib;

use super::Ctx;
use crate::cpu::CPU_PROFILES;
use crate::config::{
    format_ram_limit, parse_ram_limit, random_hostname, random_username, read_config,
    read_global_config, write_config, write_global_config, AppConfig, AvahiMode, Layout,
    LocalDelete, TempMode, Theme,
};

/// The Settings tab (global defaults).
pub fn build_settings_tab(ctx: &Ctx) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 4);
    form.set_margin_top(8);
    form.set_margin_bottom(8);
    form.set_margin_start(8);
    form.set_margin_end(8);
    let gather = build_form(&form, read_global_config(), true, ctx);

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&form));
    outer.append(&scroller);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(6);
    bar.set_margin_end(6);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let save = gtk::Button::with_label("Save global settings");
    bar.append(&spacer);
    bar.append(&save);
    outer.append(&bar);

    let ctx = ctx.clone();
    save.connect_clicked(move |_| match write_global_config(&gather()) {
        Ok(_) => ctx.status("Global settings saved."),
        Err(e) => ctx.status(&format!("Failed to save: {e}")),
    });

    outer
}

/// Open the per-app configuration window.
pub fn open(ctx: &Ctx, app_name: &str) {
    let cfg = read_config(app_name).unwrap_or_default();

    let win = gtk::Window::builder()
        .title(format!("Configure {app_name}"))
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(480)
        .default_height(560)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 4);
    form.set_margin_top(8);
    form.set_margin_bottom(8);
    form.set_margin_start(8);
    form.set_margin_end(8);
    let gather = build_form(&form, cfg, false, ctx);

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&form));
    outer.append(&scroller);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(6);
    bar.set_margin_end(6);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    bar.append(&spacer);
    bar.append(&cancel);
    bar.append(&save);
    outer.append(&bar);

    win.set_child(Some(&outer));
    win.present();

    {
        let win = win.clone();
        cancel.connect_clicked(move |_| win.close());
    }
    {
        let ctx = ctx.clone();
        let win = win.clone();
        let app_name = app_name.to_string();
        save.connect_clicked(move |_| match write_config(&app_name, &gather()) {
            Ok(_) => {
                ctx.status(&format!("Saved settings for {app_name}."));
                win.close();
            }
            Err(e) => ctx.status(&format!("Failed to save: {e}")),
        });
    }
}

// ── Form construction ──────────────────────────────────────────────────────────

fn header(form: &gtk::Box, text: &str) {
    let l = gtk::Label::new(None);
    l.set_xalign(0.0);
    l.set_margin_top(8);
    l.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(text)));
    form.append(&l);
}

/// A labelled row: fixed-width caption on the left, widget on the right.
fn labelled(form: &gtk::Box, caption: &str, widget: &impl IsA<gtk::Widget>) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let l = gtk::Label::new(Some(caption));
    l.set_xalign(0.0);
    l.set_width_chars(22);
    row.append(&l);
    widget.set_hexpand(true);
    row.append(widget);
    form.append(&row);
}

fn check(form: &gtk::Box, label: &str, active: bool) -> gtk::CheckButton {
    let c = gtk::CheckButton::with_label(label);
    c.set_active(active);
    form.append(&c);
    c
}

fn dropdown(form: &gtk::Box, caption: &str, options: &[&str], selected: u32) -> gtk::DropDown {
    let d = gtk::DropDown::from_strings(options);
    d.set_selected(selected);
    labelled(form, caption, &d);
    d
}

fn entry(form: &gtk::Box, caption: &str, value: &str) -> gtk::Entry {
    let e = gtk::Entry::new();
    e.set_text(value);
    labelled(form, caption, &e);
    e
}

/// A labelled entry with a "Random" button that fills it with a freshly
/// generated value. The value is stored verbatim as a custom string, so it
/// never changes on its own — only when the button is clicked again.
fn entry_random(form: &gtk::Box, caption: &str, value: &str, gen: fn() -> String) -> gtk::Entry {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let l = gtk::Label::new(Some(caption));
    l.set_xalign(0.0);
    l.set_width_chars(22);
    row.append(&l);

    let e = gtk::Entry::new();
    e.set_text(value);
    e.set_hexpand(true);
    row.append(&e);

    let b = gtk::Button::with_label("Random");
    b.set_tooltip_text(Some(
        "Fill with a random value. It is saved as-is and never changes until you click Random again.",
    ));
    let e2 = e.clone();
    b.connect_clicked(move |_| e2.set_text(&gen()));
    row.append(&b);

    form.append(&row);
    e
}

/// Build the form widgets into `form` and return a closure that reconstructs an
/// `AppConfig` from them (carrying over anything not shown from the original).
fn build_form(form: &gtk::Box, cfg: AppConfig, is_global: bool, ctx: &Ctx) -> Rc<dyn Fn() -> AppConfig> {
    header(form, "Sandbox");
    let network = check(form, "Network access", cfg.network);
    let camera = check(form, "Camera (/dev/video*)", cfg.camera);
    let microphone = check(form, "Microphone", cfg.microphone);
    let audio = check(form, "Audio output", cfg.audio);
    let portal = check(form, "Portal filter (hide host desktop portal)", cfg.portal_filter);

    header(form, "Temporary files & discovery");
    let temp_mode = dropdown(form, "Temp mode",
        &["Share host /tmp", "Private RAM disk", "Persistent per-app", "Per-instance UUID"],
        match cfg.temp_mode {
            TempMode::System => 0, TempMode::Ramdisk => 1, TempMode::Local => 2, TempMode::Uuid => 3,
        });
    let temp_delete = dropdown(form, "Temp cleanup (local)",
        &["Never", "On launch", "On close"],
        match cfg.temp_delete {
            LocalDelete::Never => 0, LocalDelete::OnStart => 1, LocalDelete::OnClose => 2,
        });
    let avahi = dropdown(form, "Avahi / zeroconf",
        &["Private stub", "Use host daemon", "Off"],
        match cfg.avahi { AvahiMode::Stub => 0, AvahiMode::Host => 1, AvahiMode::Off => 2 });

    header(form, "Identity");
    let spoof_hostname = entry_random(form, "Hostname", cfg.spoof_hostname.as_deref().unwrap_or(""), random_hostname);
    let spoof_username = entry_random(form, "Username ($USER)", cfg.spoof_username.as_deref().unwrap_or(""), random_username);
    let spoof_machine_id = entry(form, "Machine ID / \"random\"", cfg.spoof_machine_id.as_deref().unwrap_or(""));
    let spoof_os = entry(form, "OS name", cfg.spoof_os.as_deref().unwrap_or(""));
    // CPU spoof: a preset picker, plus an optional custom cpuinfo file path.
    let mut cpu_labels: Vec<&str> = vec!["Real CPU"];
    cpu_labels.extend(CPU_PROFILES.iter().map(|p| p.label));
    let cpu_sel = match cfg.spoof_cpuinfo.as_deref() {
        None => 0,
        Some(v) => v
            .strip_prefix("preset:")
            .and_then(|k| CPU_PROFILES.iter().position(|p| p.key == k))
            .map(|p| (p + 1) as u32)
            .unwrap_or(0),
    };
    let spoof_cpu = dropdown(form, "Spoof CPU", &cpu_labels, cpu_sel);
    let cpu_custom_init = match cfg.spoof_cpuinfo.as_deref() {
        Some(v) if v != "sample" && !v.starts_with("preset:") => v,
        _ => "",
    };
    let spoof_cpuinfo = entry(form, "…or custom cpuinfo file", cpu_custom_init);
    let spoof_terminal = check(form, "Forward terminal identity (TERM_PROGRAM)", cfg.spoof_terminal);

    header(form, "Resources");
    let ram = entry(form, "RAM limit (e.g. 2 GB)", &cfg.ram_limit.map(format_ram_limit).unwrap_or_default());

    // Shared dirs (per-app only).
    let shared_state: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(cfg.shared_dirs.clone()));
    if !is_global {
        header(form, "Shared directories");
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        let frame = gtk::Frame::new(None);
        frame.set_child(Some(&list));
        form.append(&frame);

        let rebuild: Rc<dyn Fn()> = {
            let list = list.clone();
            let shared_state = shared_state.clone();
            Rc::new(move || {
                while let Some(c) = list.first_child() {
                    list.remove(&c);
                }
                for d in shared_state.borrow().iter() {
                    let l = gtk::Label::new(Some(d));
                    l.set_xalign(0.0);
                    list.append(&l);
                }
            })
        };
        rebuild();

        let btns = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let add = gtk::Button::with_label("Add folder…");
        let remove = gtk::Button::with_label("Remove selected");
        btns.append(&add);
        btns.append(&remove);
        form.append(&btns);

        {
            let ctx = ctx.clone();
            let shared_state = shared_state.clone();
            let rebuild = rebuild.clone();
            add.connect_clicked(move |_| {
                let dialog = gtk::FileDialog::builder().title("Share a folder").build();
                let shared_state = shared_state.clone();
                let rebuild = rebuild.clone();
                dialog.select_folder(Some(&ctx.window.clone()), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(folder) = res {
                        if let Some(p) = folder.path() {
                            let p = p.to_string_lossy().to_string();
                            if !shared_state.borrow().iter().any(|d| d == &p) {
                                shared_state.borrow_mut().push(p);
                                rebuild();
                            }
                        }
                    }
                });
            });
        }
        {
            let shared_state = shared_state.clone();
            let rebuild = rebuild.clone();
            let list = list.clone();
            remove.connect_clicked(move |_| {
                if let Some(row) = list.selected_row() {
                    let i = row.index() as usize;
                    if i < shared_state.borrow().len() {
                        shared_state.borrow_mut().remove(i);
                        rebuild();
                    }
                }
            });
        }
    }

    // Global-only groups.
    let global_widgets = if is_global {
        header(form, "Install behaviour (defaults)");
        let create_shortcut = check(form, "Create ~/bin shortcut", cfg.create_shortcut);
        let confirm_install = check(form, "Confirm before installing", cfg.confirm_install);
        let ask_shortcut = check(form, "Ask about the shortcut each time", cfg.ask_shortcut);
        let clean_cache = check(form, "Clean cache after each install", cfg.clean_cache);

        header(form, "TUI appearance");
        let theme = dropdown(form, "Theme", &["Default", "Amber", "Matrix"],
            match cfg.theme { Theme::Default => 0, Theme::Amber => 1, Theme::Matrix => 2 });
        let layout = dropdown(form, "Layout", &["Top tabs", "Sidebar", "Bottom tabs"],
            match cfg.layout { Layout::Default => 0, Layout::Sidebar => 1, Layout::Bottom => 2 });

        Some((create_shortcut, confirm_install, ask_shortcut, clean_cache, theme, layout))
    } else {
        None
    };

    Rc::new(move || {
        let mut c = cfg.clone();
        c.network = network.is_active();
        c.camera = camera.is_active();
        c.microphone = microphone.is_active();
        c.audio = audio.is_active();
        c.portal_filter = portal.is_active();
        c.temp_mode = match temp_mode.selected() {
            1 => TempMode::Ramdisk, 2 => TempMode::Local, 3 => TempMode::Uuid, _ => TempMode::System,
        };
        c.temp_delete = match temp_delete.selected() {
            0 => LocalDelete::Never, 2 => LocalDelete::OnClose, _ => LocalDelete::OnStart,
        };
        c.avahi = match avahi.selected() {
            1 => AvahiMode::Host, 2 => AvahiMode::Off, _ => AvahiMode::Stub,
        };
        let opt = |e: &gtk::Entry| {
            let t = e.text().trim().to_string();
            (!t.is_empty()).then_some(t)
        };
        c.spoof_hostname = opt(&spoof_hostname);
        c.spoof_username = opt(&spoof_username);
        c.spoof_machine_id = opt(&spoof_machine_id);
        c.spoof_os = opt(&spoof_os);
        // A custom path wins; otherwise use the CPU-preset dropdown selection.
        c.spoof_cpuinfo = {
            let custom = spoof_cpuinfo.text().trim().to_string();
            if !custom.is_empty() {
                Some(custom)
            } else {
                match spoof_cpu.selected() {
                    0 => None,
                    n => CPU_PROFILES.get((n - 1) as usize).map(|p| format!("preset:{}", p.key)),
                }
            }
        };
        c.spoof_terminal = spoof_terminal.is_active();
        c.ram_limit = parse_ram_limit(&ram.text());
        c.shared_dirs = shared_state.borrow().clone();

        if let Some((cs, ci, as_, cc, theme, layout)) = &global_widgets {
            c.create_shortcut = cs.is_active();
            c.confirm_install = ci.is_active();
            c.ask_shortcut = as_.is_active();
            c.clean_cache = cc.is_active();
            c.theme = match theme.selected() {
                1 => Theme::Amber, 2 => Theme::Matrix, _ => Theme::Default,
            };
            c.layout = match layout.selected() {
                1 => Layout::Sidebar, 2 => Layout::Bottom, _ => Layout::Default,
            };
        }
        c
    })
}
