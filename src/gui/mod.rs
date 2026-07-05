//! Native GTK4 desktop front-end for wryayer — plain GTK (no libadwaita), built
//! like the TUI: a tab strip (Installed / Install / Import / Games / Space /
//! Settings) with ordinary buttons and lists.
//!
//! It reuses the same building blocks as the TUI: the manifest/config library
//! API for reading state and writing per-app settings, and `wryayer`
//! subprocesses (streamed into a console window) for anything that installs,
//! removes or mutates an app's files.

mod config;
mod install;
mod op;

use std::cell::RefCell;
use std::process::{Command, Stdio};
use std::rc::Rc;

use anyhow::Result;
use gtk4 as gtk;
use gtk::prelude::*;
use gtk::glib;

use crate::manifest::{list_all_apps, read_manifest, tree_order, write_manifest, Manifest};

/// A boxed "rebuild the app lists" closure behind shared mutable state so
/// callbacks can refresh after an operation.
pub type Refresh = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// Shared handles threaded through every screen.
#[derive(Clone)]
pub struct Ctx {
    pub window: gtk::ApplicationWindow,
    pub status: gtk::Label,
    pub refresh: Refresh,
}

impl Ctx {
    /// Put a message on the bottom status line.
    pub fn status(&self, msg: &str) {
        self.status.set_text(msg);
    }

    /// Rebuild the Installed and Games lists.
    pub fn refresh(&self) {
        if let Some(f) = self.refresh.borrow().as_ref() {
            f();
        }
    }
}

pub fn run() -> Result<()> {
    let app = gtk::Application::builder()
        .application_id("de.synthelicz.Wryayer")
        .build();
    app.connect_activate(build_ui);
    let code = app.run_with_args::<&str>(&[]);
    if code == glib::ExitCode::SUCCESS {
        Ok(())
    } else {
        anyhow::bail!("GUI exited with a non-zero status")
    }
}

fn build_ui(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("wryayer")
        .default_width(880)
        .default_height(620)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let notebook = gtk::Notebook::new();
    notebook.set_vexpand(true);
    root.append(&notebook);

    // Bottom status line (classic).
    let status = gtk::Label::new(Some("Ready."));
    status.set_xalign(0.0);
    status.set_margin_top(3);
    status.set_margin_bottom(3);
    status.set_margin_start(6);
    status.set_margin_end(6);
    let status_frame = gtk::Frame::new(None);
    status_frame.set_child(Some(&status));
    root.append(&status_frame);

    window.set_child(Some(&root));

    let ctx = Ctx {
        window: window.clone(),
        status,
        refresh: Rc::new(RefCell::new(None)),
    };

    // Build the six tabs.
    let (installed_tab, populate_installed) = build_app_list_tab(&ctx, false);
    let (games_tab, populate_games) = build_app_list_tab(&ctx, true);
    let install_tab = install::build_tab(&ctx);
    let import_tab = build_import_tab(&ctx);
    let space_tab = build_space_tab(&ctx);
    let settings_tab = config::build_settings_tab(&ctx);

    add_tab(&notebook, &installed_tab, "Installed");
    add_tab(&notebook, &install_tab, "Install");
    add_tab(&notebook, &import_tab, "Import");
    add_tab(&notebook, &games_tab, "Games");
    add_tab(&notebook, &space_tab, "Space");
    add_tab(&notebook, &settings_tab, "Settings");

    // Wire the refresh closure to repopulate both app lists.
    {
        let refresh = ctx.refresh.clone();
        *refresh.borrow_mut() = Some(Box::new(move || {
            populate_installed();
            populate_games();
        }));
    }
    ctx.refresh();

    window.present();
}

fn add_tab(notebook: &gtk::Notebook, child: &impl IsA<gtk::Widget>, label: &str) {
    notebook.append_page(child, Some(&gtk::Label::new(Some(label))));
}

// ── Installed / Games tabs ─────────────────────────────────────────────────────

/// Build a list-of-apps tab. `games` selects wine-game containers; otherwise
/// ordinary apps. Returns the tab widget and a closure that repopulates it.
fn build_app_list_tab(ctx: &Ctx, games: bool) -> (gtk::Box, Rc<dyn Fn()>) {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(6);
    vbox.set_margin_start(6);
    vbox.set_margin_end(6);

    // Collapse the parent/child (`--into`) tree down to just the parents.
    let compact = Rc::new(std::cell::Cell::new(false));
    let compact_check = gtk::CheckButton::with_label("Compact tree (show parents only)");
    if !games {
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.append(&compact_check);
        vbox.append(&top);
    }

    let listbox = gtk::ListBox::new();
    listbox.set_selection_mode(gtk::SelectionMode::Single);
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&listbox));
    vbox.append(&scroller);

    let names: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    // Button toolbar.
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let run_btn = gtk::Button::with_label("Run");
    let update_btn = gtk::Button::with_label("Update");
    let config_btn = gtk::Button::with_label("Configure");
    let remove_btn = gtk::Button::with_label("Remove");
    bar.append(&run_btn);
    bar.append(&update_btn);
    bar.append(&config_btn);
    // Non-game extras.
    let rename_btn = gtk::Button::with_label("Rename");
    let snapshot_btn = gtk::Button::with_label("Snapshot");
    let rollback_btn = gtk::Button::with_label("Roll back");
    let export_btn = gtk::Button::with_label("Export");
    if !games {
        bar.append(&rename_btn);
        bar.append(&snapshot_btn);
        bar.append(&rollback_btn);
        bar.append(&export_btn);
    }
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    bar.append(&spacer);
    bar.append(&remove_btn);
    vbox.append(&bar);

    // Helper to read the selected app name.
    let selected = {
        let listbox = listbox.clone();
        let names = names.clone();
        move || -> Option<String> {
            let idx = listbox.selected_row()?.index();
            names.borrow().get(idx as usize).cloned()
        }
    };

    // Double-click a row to run it.
    {
        let ctx = ctx.clone();
        let names = names.clone();
        listbox.connect_row_activated(move |_, row| {
            if let Some(name) = names.borrow().get(row.index() as usize) {
                launch_detached(&ctx, name);
            }
        });
    }

    macro_rules! act {
        ($btn:expr, $body:expr) => {{
            let ctx = ctx.clone();
            let selected = selected.clone();
            let f = $body;
            $btn.connect_clicked(move |_| match selected() {
                Some(name) => f(&ctx, &name),
                None => ctx.status("Select an app first."),
            });
        }};
    }

    act!(run_btn, |ctx: &Ctx, name: &str| launch_detached(ctx, name));
    act!(update_btn, |ctx: &Ctx, name: &str| {
        op::run_operation(&ctx.window, "Update", vec!["update".into(), name.into()], {
            let ctx = ctx.clone();
            move |_| ctx.refresh()
        });
    });
    act!(config_btn, |ctx: &Ctx, name: &str| config::open(ctx, name));
    act!(rename_btn, |ctx: &Ctx, name: &str| rename_app(ctx, name));
    act!(snapshot_btn, |ctx: &Ctx, name: &str| {
        op::run_operation(&ctx.window, "Snapshot", vec!["snapshot".into(), name.into()], |_| {});
    });
    act!(rollback_btn, |ctx: &Ctx, name: &str| {
        let name = name.to_string();
        confirm(ctx, "Roll back to latest snapshot?",
            "The app's files are restored to the most recent snapshot.", true, {
            let ctx = ctx.clone();
            move || op::run_operation(&ctx.window, "Rollback", vec!["rollback".into(), name.clone()], {
                let ctx = ctx.clone();
                move |_| ctx.refresh()
            })
        });
    });
    act!(export_btn, |ctx: &Ctx, name: &str| export_app(ctx, name));
    act!(remove_btn, |ctx: &Ctx, name: &str| {
        let name = name.to_string();
        confirm(ctx, &format!("Remove “{name}”?"),
            "This deletes the app's isolated directory and its launcher shortcuts.", true, {
            let ctx = ctx.clone();
            move || op::run_operation(&ctx.window, "Remove", vec!["remove".into(), name.clone()], {
                let ctx = ctx.clone();
                move |_| ctx.refresh()
            })
        });
    });

    // Populate closure.
    let populate: Rc<dyn Fn()> = {
        let listbox = listbox.clone();
        let names = names.clone();
        let compact = compact.clone();
        Rc::new(move || {
            while let Some(child) = listbox.first_child() {
                listbox.remove(&child);
            }
            names.borrow_mut().clear();

            // tree_order keeps each `--into` child directly after its parent.
            let apps = list_all_apps().map(tree_order).unwrap_or_default();
            let show_children = !compact.get();
            let filtered: Vec<&Manifest> = apps
                .iter()
                .filter(|m| m.app.wine_game.is_some() == games)
                .filter(|m| show_children || m.app.alias_of.is_none())
                .collect();

            if filtered.is_empty() {
                let msg = if games { "No wine games imported." } else { "No apps installed." };
                let l = gtk::Label::new(Some(msg));
                l.set_margin_top(12);
                l.set_margin_bottom(12);
                listbox.append(&l);
                listbox.set_selection_mode(gtk::SelectionMode::None);
                return;
            }
            listbox.set_selection_mode(gtk::SelectionMode::Single);

            for (idx, m) in filtered.iter().enumerate() {
                names.borrow_mut().push(m.app.name.clone());
                // A child (alias) gets a tree connector; the last child of a
                // parent gets the corner glyph.
                let connector = if let Some(target) = &m.app.alias_of {
                    let is_last = filtered
                        .get(idx + 1)
                        .map(|n| n.app.alias_of.as_deref() != Some(target.as_str()))
                        .unwrap_or(true);
                    if is_last { "└─ " } else { "├─ " }
                } else {
                    ""
                };
                listbox.append(&app_row_widget(m, connector));
            }
        })
    };

    // Toggling compact mode re-renders the list.
    {
        let compact = compact.clone();
        let populate = populate.clone();
        compact_check.connect_toggled(move |c| {
            compact.set(c.is_active());
            populate();
        });
    }

    (vbox, populate)
}

fn app_row_widget(m: &Manifest, connector: &str) -> gtk::Box {
    let is_child = !connector.is_empty();
    let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
    row.set_margin_top(4);
    row.set_margin_bottom(4);
    // Indent children so the tree structure reads at a glance.
    row.set_margin_start(if is_child { 22 } else { 4 });
    row.set_margin_end(4);

    let title = match &m.app.display_name {
        Some(d) => format!("{d}  [{}]", m.app.name),
        None => m.app.name.clone(),
    };
    let name_lbl = gtk::Label::new(None);
    name_lbl.set_xalign(0.0);
    // The connector glyph is dimmed; a child's name is dimmed too, like the TUI.
    let name_markup = if is_child {
        format!(
            "<span alpha='55%'>{}</span><span alpha='75%'>{}</span>",
            glib::markup_escape_text(connector),
            glib::markup_escape_text(&title)
        )
    } else {
        format!("<b>{}</b>", glib::markup_escape_text(&title))
    };
    name_lbl.set_markup(&name_markup);
    row.append(&name_lbl);

    let mut info: Vec<String> = m
        .packages
        .iter()
        .map(|p| format!("{} {}", p.name, p.version))
        .collect();
    if m.app.alias_of.is_some() {
        info.push("alias".into());
    }
    if !info.is_empty() {
        let info_lbl = gtk::Label::new(None);
        info_lbl.set_xalign(0.0);
        info_lbl.set_markup(&format!(
            "<small>{}</small>",
            glib::markup_escape_text(&info.join("  ·  "))
        ));
        row.append(&info_lbl);
    }
    row
}

// ── Import tab ─────────────────────────────────────────────────────────────────

fn build_import_tab(ctx: &Ctx) -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

    let lbl = gtk::Label::new(Some("Import an app or a Windows game into its own sandbox."));
    lbl.set_xalign(0.0);
    vbox.append(&lbl);

    let zip_btn = gtk::Button::with_label("Import app from export (.zip)…");
    zip_btn.set_halign(gtk::Align::Start);
    let game_btn = gtk::Button::with_label("Import Windows game (folder)…");
    game_btn.set_halign(gtk::Align::Start);
    vbox.append(&zip_btn);
    vbox.append(&game_btn);

    {
        let ctx = ctx.clone();
        zip_btn.connect_clicked(move |_| import_app_zip(&ctx));
    }
    {
        let ctx = ctx.clone();
        game_btn.connect_clicked(move |_| install::open_game_wizard(&ctx));
    }
    vbox
}

// ── Space tab ──────────────────────────────────────────────────────────────────

fn build_space_tab(ctx: &Ctx) -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

    let lbl = gtk::Label::new(Some("Reclaim disk space."));
    lbl.set_xalign(0.0);
    vbox.append(&lbl);

    let dedup_btn = gtk::Button::with_label("Deduplicate storage (hard-link identical files)");
    dedup_btn.set_halign(gtk::Align::Start);
    let clean_btn = gtk::Button::with_label("Clean download cache (~/.cache/wryayer)");
    clean_btn.set_halign(gtk::Align::Start);
    vbox.append(&dedup_btn);
    vbox.append(&clean_btn);

    {
        let ctx = ctx.clone();
        dedup_btn.connect_clicked(move |_| {
            op::run_operation(&ctx.window, "Deduplicate", vec!["dedup".into()], {
                let ctx = ctx.clone();
                move |_| ctx.refresh()
            });
        });
    }
    {
        let ctx = ctx.clone();
        clean_btn.connect_clicked(move |_| {
            confirm(&ctx, "Clean download cache?",
                "This deletes ~/.cache/wryayer. Installed apps are unaffected.", false, {
                let ctx = ctx.clone();
                move || op::run_operation(&ctx.window, "Clean cache", vec!["clean".into()], |_| {})
            });
        });
    }
    vbox
}

// ── App actions ────────────────────────────────────────────────────────────────

/// Launch an installed app detached — it runs independently of the GUI.
fn launch_detached(ctx: &Ctx, name: &str) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    match Command::new(&exe)
        .arg("run")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => ctx.status(&format!("Launched {name}.")),
        Err(e) => ctx.status(&format!("Failed to launch {name}: {e}")),
    }
}

/// Rename an app's display name (a manifest edit, done in-process).
fn rename_app(ctx: &Ctx, name: &str) {
    let manifest = match read_manifest(name) {
        Ok(m) => m,
        Err(e) => {
            ctx.status(&format!("Cannot read manifest: {e}"));
            return;
        }
    };
    let initial = manifest.app.display_name.clone().unwrap_or_default();
    let ctx2 = ctx.clone();
    let name = name.to_string();
    text_prompt(ctx, "Rename app", "Display name (blank to clear):", &initial, move |text| {
        let mut manifest = match read_manifest(&name) {
            Ok(m) => m,
            Err(e) => {
                ctx2.status(&format!("Cannot read manifest: {e}"));
                return;
            }
        };
        let text = text.trim().to_string();
        manifest.app.display_name = (!text.is_empty()).then_some(text);
        match write_manifest(&name, &manifest) {
            Ok(_) => {
                ctx2.status("Renamed.");
                ctx2.refresh();
            }
            Err(e) => ctx2.status(&format!("Failed to save: {e}")),
        }
    });
}

/// Export an app to a zip chosen with a native save dialog.
fn export_app(ctx: &Ctx, name: &str) {
    let today = chrono::Local::now().format("%Y-%m-%d");
    let dialog = gtk::FileDialog::builder()
        .title(format!("Export {name}"))
        .initial_name(format!("{name}-{today}.zip"))
        .build();
    let ctx = ctx.clone();
    let name = name.to_string();
    dialog.save(Some(&ctx.window.clone()), gtk::gio::Cancellable::NONE, move |res| {
        if let Ok(file) = res {
            if let Some(path) = file.path() {
                op::run_operation(
                    &ctx.window,
                    "Export",
                    vec!["export".into(), name.clone(), "-o".into(), path.to_string_lossy().into()],
                    |_| {},
                );
            }
        }
    });
}

/// Import an app from a wryayer export zip chosen with a native open dialog.
fn import_app_zip(ctx: &Ctx) {
    let filter = gtk::FileFilter::new();
    filter.add_pattern("*.zip");
    filter.set_name(Some("wryayer export (*.zip)"));
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let dialog = gtk::FileDialog::builder()
        .title("Import app")
        .filters(&filters)
        .build();
    let ctx = ctx.clone();
    dialog.open(Some(&ctx.window.clone()), gtk::gio::Cancellable::NONE, move |res| {
        if let Ok(file) = res {
            if let Some(path) = file.path() {
                op::run_operation(
                    &ctx.window,
                    "Import",
                    vec!["import".into(), path.to_string_lossy().into()],
                    {
                        let ctx = ctx.clone();
                        move |_| ctx.refresh()
                    },
                );
            }
        }
    });
}

// ── Shared dialogs ─────────────────────────────────────────────────────────────

/// A yes/no confirmation using the plain GTK alert dialog.
pub fn confirm<F>(ctx: &Ctx, title: &str, body: &str, danger: bool, on_confirm: F)
where
    F: Fn() + 'static,
{
    let action = if danger { "Remove" } else { "OK" };
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(title)
        .detail(body)
        .buttons(["Cancel", action])
        .cancel_button(0)
        .default_button(1)
        .build();
    dialog.choose(Some(&ctx.window), gtk::gio::Cancellable::NONE, move |res| {
        if res == Ok(1) {
            on_confirm();
        }
    });
}

/// A single-line text-entry prompt window with OK/Cancel.
pub fn text_prompt<F>(ctx: &Ctx, title: &str, label: &str, initial: &str, on_ok: F)
where
    F: Fn(String) + 'static,
{
    let win = gtk::Window::builder()
        .title(title)
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(360)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(10);
    vbox.set_margin_bottom(10);
    vbox.set_margin_start(10);
    vbox.set_margin_end(10);

    let lbl = gtk::Label::new(Some(label));
    lbl.set_xalign(0.0);
    vbox.append(&lbl);

    let entry = gtk::Entry::new();
    entry.set_text(initial);
    entry.set_activates_default(true);
    vbox.append(&entry);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("OK");
    bar.append(&spacer);
    bar.append(&cancel);
    bar.append(&ok);
    vbox.append(&bar);

    win.set_child(Some(&vbox));
    win.set_default_widget(Some(&ok));

    {
        let win = win.clone();
        cancel.connect_clicked(move |_| win.close());
    }
    {
        let win = win.clone();
        let entry = entry.clone();
        let on_ok = Rc::new(on_ok);
        ok.connect_clicked(move |_| {
            on_ok(entry.text().to_string());
            win.close();
        });
    }
    win.present();
}
