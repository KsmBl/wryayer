//! Per-app and global configuration — plain GTK forms writing the same INI the
//! CLI `wryayer config` command uses, via the library's `read_config`/`write_config`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::glib;

use super::{encryption, Ctx};
use crate::cpu::CPU_PROFILES;
use crate::config::{
    format_ram_limit, parse_ram_limit, random_hostname, random_username, read_config,
    read_global_config, write_config, write_global_config, AppConfig, AvahiMode, Layout,
    LocalDelete, PasswordSource, TempMode, Theme,
};

/// The Settings tab (global defaults).
pub fn build_settings_tab(ctx: &Ctx) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 4);
    form.set_margin_top(8);
    form.set_margin_bottom(8);
    form.set_margin_start(8);
    form.set_margin_end(8);
    let gather = build_form(&form, read_global_config(), true, None, ctx).gather;

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
    let Form { gather, save_manifest } = build_form(&form, cfg, false, Some(app_name), ctx);

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
        save.connect_clicked(move |_| {
            match write_config(&app_name, &gather()).and_then(|_| save_manifest()) {
                Ok(_) => {
                    ctx.status(&format!("Saved settings for {app_name}."));
                    win.close();
                }
                Err(e) => ctx.status(&format!("Failed to save: {e}")),
            }
        });
    }
}

// ── Form construction ──────────────────────────────────────────────────────────

/// A full-width button wired to an encryption action for `app_name`.
///
/// These act immediately rather than on Save: they rewrite the very tree
/// `config.ini` lives in, so batching them with the other settings would mean
/// saving into a directory that is about to be replaced.
fn action_button(
    form: &gtk::Box,
    ctx: &Ctx,
    label: &str,
    app_name: &str,
    action: fn(&Ctx, &str),
) {
    let button = gtk::Button::with_label(label);
    button.set_halign(gtk::Align::Start);
    button.set_margin_top(4);
    form.append(&button);

    let ctx = ctx.clone();
    let app_name = app_name.to_string();
    button.connect_clicked(move |_| action(&ctx, &app_name));
}

/// A full-width button wired to a store-wide action.
fn global_button(form: &gtk::Box, ctx: &Ctx, label: &str, action: fn(&Ctx)) {
    let button = gtk::Button::with_label(label);
    button.set_halign(gtk::Align::Start);
    button.set_margin_top(4);
    form.append(&button);

    let ctx = ctx.clone();
    button.connect_clicked(move |_| action(&ctx));
}

fn header(form: &gtk::Box, text: &str) {
    // A divider line above each section, so the groups read as separated bands.
    let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
    sep.set_margin_top(10);
    form.append(&sep);
    let l = gtk::Label::new(None);
    l.set_xalign(0.0);
    l.set_margin_top(6);
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

/// A modal field-by-field custom-CPU configurator (the GUI counterpart of the
/// TUI's configurator). Calls `on_ok` with the serialized `custom:…` value.
fn open_cpu_configurator(parent: &impl IsA<gtk::Window>, initial: crate::cpu::CustomCpu, on_ok: Rc<dyn Fn(String)>) {
    let win = gtk::Window::builder()
        .title("Configure custom CPU")
        .transient_for(parent)
        .modal(true)
        .default_width(440)
        .build();

    let form = gtk::Box::new(gtk::Orientation::Vertical, 4);
    form.set_margin_top(12);
    form.set_margin_bottom(12);
    form.set_margin_start(12);
    form.set_margin_end(12);

    let vendor = dropdown(&form, "Vendor", &["GenuineIntel (Intel)", "AuthenticAMD (AMD)"],
        if initial.vendor_id == "AuthenticAMD" { 1 } else { 0 });
    let name = entry(&form, "Model name", &initial.model_name);
    let family = entry(&form, "CPU family", &initial.family.to_string());
    let model = entry(&form, "Model", &initial.model.to_string());
    let stepping = entry(&form, "Stepping", &initial.stepping.to_string());
    let cores = entry(&form, "Cores", &initial.cores.to_string());
    let threads = entry(&form, "Threads", &initial.threads.to_string());
    let mhz = entry(&form, "CPU MHz", &initial.mhz.to_string());
    let cache = entry(&form, "Cache (KB)", &initial.cache_kb.to_string());
    let host = entry(&form, "Host (mainboard)", &initial.host);

    let hint = gtk::Label::new(Some(
        "Family: Intel Core = 6, AMD Zen 3/4 = 25. Threads = Cores for no SMT, 2× for SMT.\n\
         Host: mainboard shown as fastfetch 'Host:' (e.g. ASUS ROG STRIX X670E-E). Blank = auto.",
    ));
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    hint.set_margin_top(6);
    hint.add_css_class("dim-label");
    form.append(&hint);

    let btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    btns.set_margin_top(10);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    btns.append(&spacer);
    btns.append(&cancel);
    btns.append(&save);
    form.append(&btns);

    win.set_child(Some(&form));

    {
        let win = win.clone();
        cancel.connect_clicked(move |_| win.close());
    }
    {
        let win = win.clone();
        save.connect_clicked(move |_| {
            let num = |e: &gtk::Entry, d: u32| e.text().trim().parse::<u32>().unwrap_or(d);
            let cores_v = num(&cores, 1).max(1);
            let threads_v = num(&threads, cores_v).max(cores_v);
            let name_v = {
                let t = name.text().trim().to_string();
                if t.is_empty() { "Custom CPU".to_string() } else { t }
            };
            let cc = crate::cpu::CustomCpu {
                vendor_id: if vendor.selected() == 1 { "AuthenticAMD" } else { "GenuineIntel" }.to_string(),
                family: num(&family, 6),
                model: num(&model, 0),
                stepping: num(&stepping, 1),
                cores: cores_v,
                threads: threads_v,
                mhz: num(&mhz, 3000),
                cache_kb: num(&cache, 8192),
                model_name: name_v,
                host: host.text().trim().to_string(),
            };
            on_ok(cc.serialize());
            win.close();
        });
    }
    win.present();
}

/// What a built form hands back.
struct Form {
    /// Reconstructs an `AppConfig` from the widgets, carrying over anything not
    /// shown from the original.
    gather: Rc<dyn Fn() -> AppConfig>,
    /// Applies the edits that do not belong in `config.ini`. A wine game's exe
    /// and prefix live in the manifest — the launcher reads them from there —
    /// so they are saved alongside the settings rather than with them.
    save_manifest: Rc<dyn Fn() -> anyhow::Result<()>>,
}

/// Build the form widgets into `form` and return the pair above.
fn build_form(form: &gtk::Box, cfg: AppConfig, is_global: bool, app_name: Option<&str>, ctx: &Ctx) -> Form {
    // Only present for an app in an unlocked container.
    let mut encryption_widgets: Option<(gtk::DropDown, gtk::CheckButton)> = None;
    // Sections mirror the TUI: Hardware (CPU/RAM), Privacy (access), Environment
    // (identity/temp/OS). See `tui::SANDBOX_SECTIONS` for the canonical grouping.
    header(form, "Hardware settings");
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

    // Custom CPU built field-by-field (stored as a `custom:…` value), mirroring
    // the TUI configurator. Held in a cell so the dialog and `gather` share it.
    let custom_cpu: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(
        cfg.spoof_cpuinfo.as_deref().filter(|v| v.starts_with("custom:")).map(str::to_string),
    ));
    let cpu_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let cpu_caption = gtk::Label::new(Some("Custom CPU"));
    cpu_caption.set_xalign(0.0);
    cpu_caption.set_width_chars(22);
    cpu_row.append(&cpu_caption);
    let cpu_status = gtk::Label::new(None);
    cpu_status.set_xalign(0.0);
    cpu_status.set_hexpand(true);
    cpu_row.append(&cpu_status);
    let cpu_btn = gtk::Button::with_label("Configure…");
    cpu_row.append(&cpu_btn);
    form.append(&cpu_row);

    let refresh_cpu_status: Rc<dyn Fn()> = {
        let custom_cpu = custom_cpu.clone();
        let cpu_status = cpu_status.clone();
        Rc::new(move || {
            let txt = custom_cpu.borrow().as_deref()
                .and_then(crate::cpu::CustomCpu::parse)
                .map(|c| format!("{} · {}C/{}T", c.model_name, c.cores, c.threads))
                .unwrap_or_else(|| "not set".to_string());
            cpu_status.set_text(&txt);
        })
    };
    refresh_cpu_status();
    {
        let ctx = ctx.clone();
        let custom_cpu = custom_cpu.clone();
        let spoof_cpu = spoof_cpu.clone();
        let refresh = refresh_cpu_status.clone();
        cpu_btn.connect_clicked(move |_| {
            let initial = custom_cpu.borrow().as_deref()
                .and_then(crate::cpu::CustomCpu::parse)
                .unwrap_or_else(crate::cpu::CustomCpu::starter);
            let custom_cpu = custom_cpu.clone();
            let spoof_cpu = spoof_cpu.clone();
            let refresh = refresh.clone();
            let on_ok: Rc<dyn Fn(String)> = Rc::new(move |spec| {
                *custom_cpu.borrow_mut() = Some(spec);
                spoof_cpu.set_selected(0); // custom wins; clear the preset picker
                refresh();
            });
            open_cpu_configurator(&ctx.window, initial, on_ok);
        });
    }
    // Picking a preset clears any configured custom CPU so exactly one applies.
    {
        let custom_cpu = custom_cpu.clone();
        let refresh = refresh_cpu_status.clone();
        spoof_cpu.connect_selected_notify(move |d| {
            if d.selected() != 0 && custom_cpu.borrow().is_some() {
                *custom_cpu.borrow_mut() = None;
                refresh();
            }
        });
    }

    let cpu_custom_init = match cfg.spoof_cpuinfo.as_deref() {
        Some(v) if v != "sample" && !v.starts_with("preset:") && !v.starts_with("custom:") => v,
        _ => "",
    };
    let spoof_cpuinfo = entry(form, "…or custom cpuinfo file", cpu_custom_init);
    let ram = entry(form, "RAM limit (e.g. 2 GB)", &cfg.ram_limit.map(format_ram_limit).unwrap_or_default());

    header(form, "Privacy settings");
    let network = check(form, "Network access", cfg.network);
    let camera = check(form, "Camera (/dev/video*)", cfg.camera);
    let microphone = check(form, "Microphone", cfg.microphone);
    let audio = check(form, "Audio output", cfg.audio);
    let usb = check(form, "Show USB / removable drives (/run/media, /media, /mnt)", cfg.usb);
    let portal = check(form, "Portal filter (hide host desktop portal)", cfg.portal_filter);
    let avahi = dropdown(form, "Avahi / zeroconf",
        &["Private stub", "Use host daemon", "Off"],
        match cfg.avahi { AvahiMode::Stub => 0, AvahiMode::Host => 1, AvahiMode::Off => 2 });

    header(form, "Environment settings");
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
    let spoof_hostname = entry_random(form, "Hostname", cfg.spoof_hostname.as_deref().unwrap_or(""), random_hostname);
    let spoof_username = entry_random(form, "Username ($USER)", cfg.spoof_username.as_deref().unwrap_or(""), random_username);
    let spoof_machine_id = entry(form, "Machine ID / \"random\"", cfg.spoof_machine_id.as_deref().unwrap_or(""));
    let spoof_os = entry(form, "OS name", cfg.spoof_os.as_deref().unwrap_or(""));
    let spoof_uptime = entry(form, "Uptime (e.g. 3d4h, blank = real)",
        &cfg.spoof_uptime.map(crate::config::format_uptime).unwrap_or_default());
    let spoof_terminal = check(form, "Forward terminal identity (TERM_PROGRAM)", cfg.spoof_terminal);

    // Bound apps (per-app only): tick other installed apps to expose as
    // host-delegated launchers inside this app's sandbox.
    let mut bound_checks: Vec<(String, gtk::CheckButton)> = Vec::new();

    // Read once: the wine-game section and the encryption rows both ask about
    // the same manifest.
    let manifest = app_name.and_then(|n| crate::manifest::read_manifest(n).ok());
    let mut wine_entries: Option<(gtk::Entry, gtk::Entry)> = None;

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

        // ── Encryption ────────────────────────────────────────────────────
        // A plain app gets the one action that offers it; one already in a
        // container gets its settings and the way back out. An alias gets
        // neither: its files live in the target's tree, so sealing its own
        // directory would protect nothing.
        let name = app_name.unwrap_or("");
        let is_alias = manifest.as_ref().map(|m| m.app.alias_of.is_some()).unwrap_or(false);
        let states = crate::commands::encrypt::scan([name]);
        let enc_state = states.get(name).copied();

        if let Some(state) = enc_state {
            header(form, "Encryption");
            if state.locked {
                // config.ini lives inside the container, so while it is locked
                // there is nothing to read and nowhere safe to write.
                let l = gtk::Label::new(Some(
                    "This app's container is locked, so its settings are sealed \
                     inside it. Unlock it to change them.",
                ));
                l.set_xalign(0.0);
                l.set_wrap(true);
                form.append(&l);
                action_button(form, ctx, "Unlock container", name, |ctx, n| {
                    encryption::unlock_container(ctx, n)
                });
            } else {
                let source = dropdown(
                    form,
                    "Container password",
                    &["Ask at every launch", "From the master store"],
                    match cfg.password_source {
                        PasswordSource::Prompt => 0,
                        PasswordSource::Master => 1,
                    },
                );
                let lock_on_exit =
                    check(form, "Lock the container when the app exits", cfg.lock_on_exit);
                encryption_widgets = Some((source, lock_on_exit));

                action_button(form, ctx, "Lock container now", name, |ctx, n| {
                    encryption::lock_container(ctx, n)
                });
                action_button(form, ctx, "Grow container…", name, |ctx, n| {
                    encryption::grow_container(ctx, n)
                });
                action_button(form, ctx, "Remove encryption…", name, |ctx, n| {
                    encryption::decrypt_app(ctx, n)
                });
            }
        } else if !is_alias && crate::veracrypt::available() {
            header(form, "Encryption");
            let l = gtk::Label::new(Some(
                "This app is stored in a plain directory. Moving it into a \
                 VeraCrypt container keeps its whole tree — filenames included — \
                 unreadable while locked.",
            ));
            l.set_xalign(0.0);
            l.set_wrap(true);
            form.append(&l);
            action_button(form, ctx, "Encrypt this app…", name, |ctx, n| {
                encryption::encrypt_app(ctx, n)
            });
        }

        // A wine container's exe and prefix — the two things about it that are
        // worth changing after the import guessed them.
        if let Some(game) = manifest.as_ref().and_then(|m| m.app.wine_game.as_ref()) {
            header(form, "Wine game");
            let hint = gtk::Label::new(Some(
                "The .exe is relative to the game folder inside the container; the \
                 prefix is the WINEPREFIX the game runs against.",
            ));
            hint.set_xalign(0.0);
            hint.set_wrap(true);
            form.append(&hint);
            let exe = entry(form, "Game .exe", &game.exe);
            let prefix = entry(form, "WINEPREFIX", &game.prefix);
            wine_entries = Some((exe, prefix));
        }

        header(form, "Bound apps");
        let hint = gtk::Label::new(Some(
            "Ticked apps become launchers inside this app's sandbox — e.g. tick \
             firefox so links open in Firefox's own container.",
        ));
        hint.set_xalign(0.0);
        hint.set_wrap(true);
        form.append(&hint);

        let self_name = app_name.unwrap_or("");
        let mut names: Vec<String> = crate::manifest::list_all_apps()
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.app.name)
            .filter(|n| n != self_name)
            .collect();
        names.sort();
        if names.is_empty() {
            let l = gtk::Label::new(Some("No other apps installed to bind."));
            l.set_xalign(0.0);
            form.append(&l);
        }
        for n in names {
            let active = cfg.bound_apps.contains(&n);
            let c = check(form, &n, active);
            bound_checks.push((n, c));
        }
    }

    // Global-only groups.
    let global_widgets = if is_global {
        header(form, "Install behaviour (defaults)");
        let create_shortcut = check(form, "Create /usr/bin shortcut", cfg.create_shortcut);
        let confirm_install = check(form, "Confirm before installing", cfg.confirm_install);
        let ask_shortcut = check(form, "Ask about the shortcut each time", cfg.ask_shortcut);
        let clean_cache = check(form, "Clean cache after each install", cfg.clean_cache);

        // The master password store is global by nature — one store covers
        // every encrypted app — so it belongs here rather than in a per-app
        // dialog. Only offered when VeraCrypt is present: without it no app can
        // be encrypted and the store would have nothing to protect.
        if crate::veracrypt::available() {
            header(form, "Encryption");
            let state = gtk::Label::new(Some(&format!(
                "Master password store: {}",
                encryption::store_summary()
            )));
            state.set_xalign(0.0);
            form.append(&state);

            let store_exists = crate::secrets::exists();
            global_button(
                form,
                ctx,
                if store_exists { "Change master password…" } else { "Set a master password…" },
                encryption::master_password,
            );
            // Only meaningful once there is a store to reveal, forget or drop.
            if store_exists {
                global_button(form, ctx, "Show stored passwords…", encryption::show_stored_passwords);
                global_button(form, ctx, "Forget it for this boot", encryption::forget_master);
                global_button(form, ctx, "Delete the store…", encryption::reset_store);
            }
        }

        header(form, "TUI appearance");
        let theme = dropdown(form, "Theme", &["Default", "Amber", "Matrix"],
            match cfg.theme { Theme::Default => 0, Theme::Amber => 1, Theme::Matrix => 2 });
        let layout = dropdown(form, "Layout", &["Top tabs", "Sidebar", "Bottom tabs"],
            match cfg.layout { Layout::Default => 0, Layout::Sidebar => 1, Layout::Bottom => 2 });

        Some((create_shortcut, confirm_install, ask_shortcut, clean_cache, theme, layout))
    } else {
        None
    };

    let save_manifest: Rc<dyn Fn() -> anyhow::Result<()>> = {
        let name = app_name.map(str::to_string);
        let wine_entries = wine_entries.clone();
        Rc::new(move || {
            let (Some(name), Some((exe, prefix))) = (name.as_deref(), wine_entries.as_ref()) else {
                return Ok(());
            };
            // Re-read rather than reusing the copy taken when the form opened:
            // an operation may have rewritten the manifest while it was up.
            let mut m = crate::manifest::read_manifest(name)?;
            if let Some(game) = m.app.wine_game.as_mut() {
                game.exe = exe.text().trim().to_string();
                game.prefix = prefix.text().trim().to_string();
                crate::manifest::write_manifest(name, &m)?;
            }
            Ok(())
        })
    };

    let gather: Rc<dyn Fn() -> AppConfig> = Rc::new(move || {
        let mut c = cfg.clone();
        c.network = network.is_active();
        c.camera = camera.is_active();
        c.microphone = microphone.is_active();
        c.audio = audio.is_active();
        c.usb = usb.is_active();
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
            let file = spoof_cpuinfo.text().trim().to_string();
            if !file.is_empty() {
                Some(file) // an explicit cpuinfo file path wins
            } else if let Some(cc) = custom_cpu.borrow().clone() {
                Some(cc) // a configured custom CPU
            } else {
                match spoof_cpu.selected() {
                    0 => None,
                    n => CPU_PROFILES.get((n - 1) as usize).map(|p| format!("preset:{}", p.key)),
                }
            }
        };
        c.spoof_terminal = spoof_terminal.is_active();
        c.spoof_uptime = crate::config::parse_uptime(&spoof_uptime.text());
        c.ram_limit = parse_ram_limit(&ram.text());
        c.shared_dirs = shared_state.borrow().clone();

        // Bound apps only exist on the per-app form (global has no such section).
        if global_widgets.is_none() {
            c.bound_apps = bound_checks.iter()
                .filter(|(_, cb)| cb.is_active())
                .map(|(n, _)| n.clone())
                .collect();
        }

        if let Some((source, lock_on_exit)) = &encryption_widgets {
            c.password_source = match source.selected() {
                1 => PasswordSource::Master,
                _ => PasswordSource::Prompt,
            };
            c.lock_on_exit = lock_on_exit.is_active();
        }

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
    });

    Form { gather, save_manifest }
}
