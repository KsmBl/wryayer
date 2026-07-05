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
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

/// Build a list-of-apps tab: a folding tree on the left (parents with their
/// `--into` children, expander arrows and tree lines) and a details panel on
/// the right. `games` selects wine-game containers; otherwise ordinary apps.
/// Returns the tab widget and a closure that repopulates it.
#[allow(deprecated)] // GtkTreeView gives the classic folding tree with lines.
fn build_app_list_tab(ctx: &Ctx, games: bool) -> (gtk::Box, Rc<dyn Fn()>) {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(6);
    vbox.set_margin_start(6);
    vbox.set_margin_end(6);

    // Collapse-to-parents toggle (Installed tab only).
    let compact = Rc::new(std::cell::Cell::new(false));
    let compact_check = gtk::CheckButton::with_label("Collapse tree (show parents only)");
    if !games {
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.append(&compact_check);
        vbox.append(&top);
    }

    // ── Tree (left) ─────────────────────────────────────────────────────
    // Columns: 0 = display markup, 1 = app name.
    let store = gtk::TreeStore::new(&[glib::Type::STRING, glib::Type::STRING]);
    let tree = gtk::TreeView::with_model(&store);
    tree.set_headers_visible(false);
    tree.set_enable_tree_lines(true);
    tree.set_show_expanders(true);
    tree.selection().set_mode(gtk::SelectionMode::Single);
    let cell = gtk::CellRendererText::new();
    let col = gtk::TreeViewColumn::new();
    col.pack_start(&cell, true);
    col.add_attribute(&cell, "markup", 0);
    tree.append_column(&col);

    let tree_scroll = gtk::ScrolledWindow::new();
    tree_scroll.set_min_content_width(240);
    tree_scroll.set_child(Some(&tree));

    // ── Details (right) ─────────────────────────────────────────────────
    let details = gtk::Box::new(gtk::Orientation::Vertical, 3);
    details.set_margin_top(8);
    details.set_margin_bottom(8);
    details.set_margin_start(10);
    details.set_margin_end(10);
    let det_scroll = gtk::ScrolledWindow::new();
    det_scroll.set_child(Some(&details));
    render_details(&details, "");

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&tree_scroll));
    paned.set_end_child(Some(&det_scroll));
    paned.set_position(300);
    paned.set_vexpand(true);
    vbox.append(&paned);

    // ── Button toolbar ──────────────────────────────────────────────────
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let run_btn = gtk::Button::with_label("Run");
    let update_btn = gtk::Button::with_label("Update");
    let config_btn = gtk::Button::with_label("Configure");
    let remove_btn = gtk::Button::with_label("Remove");
    bar.append(&run_btn);
    bar.append(&update_btn);
    bar.append(&config_btn);
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

    // Selected app name (empty placeholder rows count as no selection).
    let selected = {
        let tree = tree.clone();
        move || -> Option<String> {
            let (model, iter) = tree.selection().selected()?;
            let name = model.get::<String>(&iter, 1);
            (!name.is_empty()).then_some(name)
        }
    };

    // Selecting a row shows its details — clicking never runs the app.
    {
        let details = details.clone();
        tree.selection().connect_changed(move |sel| match sel.selected() {
            Some((model, iter)) => render_details(&details, &model.get::<String>(&iter, 1)),
            None => render_details(&details, ""),
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
        op::run_operation(&ctx.window, "Snapshot", vec!["snapshot".into(), name.into()], {
            let ctx = ctx.clone();
            move |_| ctx.refresh()
        });
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

    // Populate closure — builds the tree store.
    let populate: Rc<dyn Fn()> = {
        let store = store.clone();
        let tree = tree.clone();
        let details = details.clone();
        let compact = compact.clone();
        Rc::new(move || {
            store.clear();
            render_details(&details, "");

            // tree_order keeps each `--into` child directly after its parent, so
            // by the time a child is seen its parent iter already exists.
            let apps = list_all_apps().map(tree_order).unwrap_or_default();
            let mut parent_iters: std::collections::HashMap<String, gtk::TreeIter> =
                std::collections::HashMap::new();
            let mut any = false;

            for m in apps.iter().filter(|m| m.app.wine_game.is_some() == games) {
                any = true;
                let is_child = m
                    .app
                    .alias_of
                    .as_ref()
                    .map(|t| parent_iters.contains_key(t))
                    .unwrap_or(false);
                let markup = row_markup(m, is_child);
                let parent = m.app.alias_of.as_ref().and_then(|t| parent_iters.get(t)).cloned();
                let iter = store.append(parent.as_ref());
                store.set_value(&iter, 0, &markup.to_value());
                store.set_value(&iter, 1, &m.app.name.to_value());
                parent_iters.insert(m.app.name.clone(), iter);
            }

            if !any {
                let msg = if games { "No wine games imported." } else { "No apps installed." };
                let iter = store.append(None);
                store.set_value(&iter, 0, &format!("<i>{msg}</i>").to_value());
                store.set_value(&iter, 1, &String::new().to_value());
            }

            if compact.get() {
                tree.collapse_all();
            } else {
                tree.expand_all();
            }
        })
    };

    // Collapse/expand the whole tree.
    {
        let compact = compact.clone();
        let tree = tree.clone();
        compact_check.connect_toggled(move |c| {
            compact.set(c.is_active());
            if c.is_active() {
                tree.collapse_all();
            } else {
                tree.expand_all();
            }
        });
    }

    (vbox, populate)
}

/// One tree-row label: bold for a parent, dimmed for a child.
fn row_markup(m: &Manifest, is_child: bool) -> String {
    let title = match &m.app.display_name {
        Some(d) => format!("{d}  [{}]", m.app.name),
        None => match &m.app.pkg_name {
            Some(p) => format!("{}  [{p}]", m.app.name),
            None => m.app.name.clone(),
        },
    };
    let esc = glib::markup_escape_text(&title);
    if is_child {
        format!("<span alpha='75%'>{esc}</span>")
    } else {
        format!("<b>{esc}</b>")
    }
}

/// Rebuild the right-hand details panel for `name` (empty = nothing selected).
fn render_details(container: &gtk::Box, name: &str) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    if name.is_empty() {
        let l = gtk::Label::new(Some("No app selected."));
        l.set_xalign(0.0);
        l.add_css_class("dim-label");
        container.append(&l);
        return;
    }
    let Ok(m) = read_manifest(name) else {
        let l = gtk::Label::new(Some("No app selected."));
        l.set_xalign(0.0);
        container.append(&l);
        return;
    };

    let title = match &m.app.display_name {
        Some(d) => format!("{d}  [{}]", m.app.name),
        None => m.app.name.clone(),
    };
    let head = gtk::Label::new(None);
    head.set_xalign(0.0);
    head.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&title)));
    head.set_wrap(true);
    container.append(&head);

    let real_pkg = m.app.pkg_name.as_deref().unwrap_or(&m.app.name);
    let ver = m
        .packages
        .iter()
        .find(|p| p.name == real_pkg)
        .map(|p| p.version.as_str())
        .unwrap_or("?");
    detail_line(container, "Version", ver);
    detail_line(container, "Installed", m.app.installed_at.get(..10).unwrap_or(&m.app.installed_at));
    let launchers = if m.app.launchers.is_empty() { "none".to_string() } else { m.app.launchers.join(", ") };
    detail_line(container, "Launchers", &launchers);
    if let Some(g) = &m.app.wine_game {
        detail_line(container, "Wine exe", &g.exe);
    }

    // Size — computed off-thread so a big app can't freeze the panel.
    let size_lbl = detail_line(container, "Size", "computing…");
    dir_bytes_async(name, &size_lbl);

    // Snapshots.
    let snaps = list_snapshots(name);
    detail_header(container, &format!("Snapshots ({})", snaps.len()));
    if snaps.is_empty() {
        detail_item(container, "none");
    } else {
        for s in &snaps {
            detail_item(container, s);
        }
    }

    // Packages.
    detail_header(container, &format!("Packages ({})", m.packages.len()));
    for p in &m.packages {
        detail_item(container, &format!("{}  {}", p.name, p.version));
    }
}

/// A `Key:  value` line; returns the value label so callers can update it.
fn detail_line(container: &gtk::Box, key: &str, value: &str) -> gtk::Label {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let k = gtk::Label::new(None);
    k.set_xalign(0.0);
    k.set_width_chars(11);
    k.set_markup(&format!("<span alpha='55%'>{}</span>", glib::markup_escape_text(key)));
    let v = gtk::Label::new(Some(value));
    v.set_xalign(0.0);
    v.set_wrap(true);
    v.set_selectable(true);
    v.set_hexpand(true);
    row.append(&k);
    row.append(&v);
    container.append(&row);
    v
}

fn detail_header(container: &gtk::Box, text: &str) {
    let l = gtk::Label::new(None);
    l.set_xalign(0.0);
    l.set_margin_top(8);
    l.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(text)));
    container.append(&l);
}

fn detail_item(container: &gtk::Box, text: &str) {
    let l = gtk::Label::new(None);
    l.set_xalign(0.0);
    l.set_margin_start(10);
    l.set_selectable(true);
    l.set_markup(&format!("<span alpha='80%'>{}</span>", glib::markup_escape_text(text)));
    container.append(&l);
}

/// Snapshot labels for an app (newest first), read straight from disk.
fn list_snapshots(name: &str) -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = format!("{home}/.wryayer/{name}/.snapshots");
    let mut labels: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    labels.sort_by(|a, b| b.cmp(a));
    labels
}

/// Compute an app directory's size with `du` off-thread and update `label`.
fn dir_bytes_async(name: &str, label: &gtk::Label) {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = format!("{home}/.wryayer/{name}");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let bytes = Command::new("du")
            .args(["-sb", &path])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next()?.parse::<u64>().ok());
        let _ = tx.send(bytes);
    });
    let label = label.clone();
    glib::timeout_add_local(Duration::from_millis(80), move || match rx.try_recv() {
        Ok(bytes) => {
            label.set_text(&bytes.map(format_bytes).unwrap_or_else(|| "—".into()));
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

fn format_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
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
