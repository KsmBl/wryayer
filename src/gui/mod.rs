//! Native GTK4 desktop front-end for wryayer — plain GTK (no libadwaita), built
//! like the TUI: a tab strip (Installed / Install / Import / Games / Space /
//! Settings) with ordinary buttons and lists.
//!
//! It reuses the same building blocks as the TUI: the manifest/config library
//! API for reading state and writing per-app settings, and `wryayer`
//! subprocesses (streamed into a console window) for anything that installs,
//! removes or mutates an app's files.

mod config;
mod encryption;
mod install;
mod op;

use std::cell::RefCell;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use gtk4 as gtk;
use gtk::prelude::*;
use gtk::glib;

use crate::commands::encrypt::AppEncryption;
use crate::manifest::{list_all_apps, read_manifest, tree_order, write_manifest, Manifest};

/// A boxed "rebuild the app lists" closure behind shared mutable state so
/// callbacks can refresh after an operation.
pub type Refresh = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// What the manifests cannot say: which apps have a newer version waiting, and
/// how many sandboxes of each are running right now.
///
/// Both answers cost something to get — one is a network round trip per app,
/// the other a walk of `/proc` — so they are gathered on their own cadence and
/// read from here by whatever draws next, exactly as the TUI does.
#[derive(Default)]
pub struct Live {
    /// App name → the version an update would install.
    pub updates: HashMap<String, String>,
    /// App name → how many of its sandboxes are up.
    pub instances: HashMap<String, usize>,
}

/// Shared handles threaded through every screen.
#[derive(Clone)]
pub struct Ctx {
    pub window: gtk::ApplicationWindow,
    pub status: gtk::Label,
    /// Re-read everything and rebuild the lists — what an operation calls when
    /// it has changed what is installed.
    pub refresh: Refresh,
    /// Rebuild the lists from what is already known. Used by the timers, which
    /// have just gathered the one fact that changed and must not start the
    /// whole cycle over.
    pub repopulate: Refresh,
    pub live: Rc<RefCell<Live>>,
}

impl Ctx {
    /// Put a message on the bottom status line.
    pub fn status(&self, msg: &str) {
        self.status.set_text(msg);
    }

    /// Re-read the world and rebuild the Installed and Games lists.
    pub fn refresh(&self) {
        if let Some(f) = self.refresh.borrow().as_ref() {
            f();
        }
    }

    /// Rebuild the lists from what is already known, without re-checking.
    pub fn repopulate(&self) {
        if let Some(f) = self.repopulate.borrow().as_ref() {
            f();
        }
    }
}

pub fn run() -> Result<()> {
    // The GUI has no terminal to ask on — its children's output goes to a
    // TextView, and its own calls into `commands` have nowhere to print. Every
    // password is collected in a dialog instead; see `encryption`.
    crate::prompt::forbid_here();
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
    load_css();

    // Checked before anything else draws: everything below resolves its paths
    // through the root, so if it is unusable the whole window would otherwise
    // come up looking like a fresh, empty install.
    let root_problem = crate::manifest::wryayer_root().err().map(|e| format!("{e:#}"));

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("wryayer")
        .default_width(900)
        .default_height(660)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);

    // Header strip.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.add_css_class("app-header");
    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_markup("<b>wryayer</b>");
    title.add_css_class("app-title");
    let subtitle = gtk::Label::new(Some("isolated per-app sandboxes"));
    subtitle.add_css_class("app-subtitle");
    header.append(&title);
    header.append(&subtitle);
    root.append(&header);

    let notebook = gtk::Notebook::new();
    notebook.set_vexpand(true);
    root.append(&notebook);

    // Bottom status line (classic).
    let status = gtk::Label::new(Some("Ready."));
    status.set_xalign(0.0);
    status.add_css_class("statusline");
    root.append(&status);

    window.set_child(Some(&root));

    let ctx = Ctx {
        window: window.clone(),
        status,
        refresh: Rc::new(RefCell::new(None)),
        repopulate: Rc::new(RefCell::new(None)),
        live: Rc::new(RefCell::new(Live::default())),
    };

    // Build the six tabs.
    let (installed_tab, populate_installed) = build_app_list_tab(&ctx, false);
    let (games_tab, populate_games) = build_app_list_tab(&ctx, true);
    let (install_tab, refresh_install_targets) = install::build_tab(&ctx);
    let import_tab = build_import_tab(&ctx);
    let space_tab = build_space_tab(&ctx);
    let settings_tab = config::build_settings_tab(&ctx);

    add_tab(&notebook, &installed_tab, "Installed");
    add_tab(&notebook, &install_tab, "Install");
    add_tab(&notebook, &import_tab, "Import");
    add_tab(&notebook, &games_tab, "Games");
    add_tab(&notebook, &space_tab, "Space");
    add_tab(&notebook, &settings_tab, "Settings");

    // Wire the refresh closure to repopulate both app lists. Anything that
    // rebuilt them changed what is installed, so the update check runs again:
    // an app just updated must lose its marker, a new one may arrive with hers.
    {
        let installed = populate_installed.clone();
        let games = populate_games.clone();
        *ctx.repopulate.borrow_mut() = Some(Box::new(move || {
            installed();
            games();
        }));
    }
    {
        let ctx2 = ctx.clone();
        *ctx.refresh.borrow_mut() = Some(Box::new(move || {
            ctx2.live.borrow_mut().instances = crate::commands::run::running_instances();
            ctx2.repopulate();
            refresh_install_targets();
            check_for_updates(&ctx2);
        }));
    }
    ctx.refresh();
    watch_running_instances(&ctx);

    window.present();

    if let Some(problem) = root_problem {
        ctx.status("wryayer cannot use ~/.wryayer — see the dialog.");
        report_root_problem(&window, &problem);
    }
}

/// Ask every app whether it has a newer version, off the main thread, and
/// redraw the lists when the answer arrives.
///
/// One check covers every app — it is a single `check_all_updates` — and it can
/// take seconds against a slow mirror, which is exactly why it does not run on
/// the thread painting the window.
fn check_for_updates(ctx: &Ctx) {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(crate::commands::update::check_all_updates());
    });

    let ctx = ctx.clone();
    glib::timeout_add_local(Duration::from_millis(250), move || match rx.try_recv() {
        Ok(updates) => {
            let changed = ctx.live.borrow().updates != updates;
            ctx.live.borrow_mut().updates = updates;
            // Repopulating from here would re-enter the refresh closure that
            // started this check, and with it another check; only the lists are
            // rebuilt, not the whole cycle.
            if changed {
                ctx.repopulate();
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

/// Keep the running-instance counts live, at the cadence the TUI uses.
///
/// A sandbox starting or exiting is not something wryayer is told about — the
/// count comes from walking `/proc` — so it is re-read on a timer, and the
/// lists are only rebuilt when the answer actually changed.
fn watch_running_instances(ctx: &Ctx) {
    let ctx = ctx.clone();
    glib::timeout_add_local(Duration::from_secs(2), move || {
        let fresh = crate::commands::run::running_instances();
        let changed = ctx.live.borrow().instances != fresh;
        if changed {
            ctx.live.borrow_mut().instances = fresh;
            ctx.repopulate();
        }
        glib::ControlFlow::Continue
    });
}

/// Put an unusable root in front of the user, in full.
///
/// The message explains what to do about it and is several paragraphs long, so
/// it gets a window rather than the one-line status bar.
fn report_root_problem(window: &gtk::ApplicationWindow, problem: &str) {
    let win = gtk::Window::builder()
        .title("wryayer cannot use ~/.wryayer")
        .transient_for(window)
        .modal(true)
        .default_width(560)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let label = gtk::Label::new(Some(problem));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_selectable(true); // the message contains a command to copy
    outer.append(&label);

    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    outer.append(&close);

    win.set_child(Some(&outer));
    win.present();

    let win2 = win.clone();
    close.connect_clicked(move |_| win2.close());
}

fn add_tab(notebook: &gtk::Notebook, child: &impl IsA<gtk::Widget>, label: &str) {
    notebook.append_page(child, Some(&gtk::Label::new(Some(label))));
}

/// A small, theme-aware stylesheet — tidier spacing, a header strip, a real
/// status bar and roomier list rows, without abandoning the plain-GTK look.
fn load_css() {
    const CSS: &str = "
        .app-header {
            padding: 8px 12px;
            background-color: alpha(@theme_fg_color, 0.05);
            border-bottom: 1px solid alpha(@theme_fg_color, 0.12);
        }
        .app-title { font-size: 15px; }
        .app-subtitle { color: alpha(@theme_fg_color, 0.55); font-size: 11px; }
        .statusline {
            padding: 5px 12px;
            background-color: alpha(@theme_fg_color, 0.05);
            border-top: 1px solid alpha(@theme_fg_color, 0.12);
            font-size: 12px;
        }
        notebook > header > tabs > tab { padding: 6px 14px; }
        notebook > header > tabs > tab:checked { font-weight: bold; }
        button { padding: 5px 12px; }
        list > row { padding: 2px 4px; }
        list > row:selected { border-radius: 4px; }
        /* Container fill, matching the TUI's amber-then-red escalation.
           Spelled out rather than relying on GTK's .warning/.error, so the
           meaning survives a theme that styles those differently. */
        .fill-warn { color: #d08420; }
        .fill-critical { color: #cc3333; font-weight: bold; }
    ";
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
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
    render_details(&details, "", &ctx.live.borrow());

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
    let check_btn = gtk::Button::with_label("Check updates");
    let update_all_btn = gtk::Button::with_label("Update all");
    let config_btn = gtk::Button::with_label("Configure");
    let remove_btn = gtk::Button::with_label("Remove");
    bar.append(&run_btn);
    bar.append(&update_btn);
    if !games {
        bar.append(&check_btn);
        bar.append(&update_all_btn);
    }
    bar.append(&config_btn);
    let rename_btn = gtk::Button::with_label("Rename");
    let into_btn = gtk::Button::with_label("Install into…");
    let shortcut_btn = gtk::Button::with_label("Shortcut");
    let snapshot_btn = gtk::Button::with_label("Snapshot");
    let rollback_btn = gtk::Button::with_label("Snapshots…");
    let export_btn = gtk::Button::with_label("Export");
    if !games {
        bar.append(&rename_btn);
        bar.append(&shortcut_btn);
        bar.append(&into_btn);
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
        let live = ctx.live.clone();
        tree.selection().connect_changed(move |sel| match sel.selected() {
            Some((model, iter)) => {
                render_details(&details, &model.get::<String>(&iter, 1), &live.borrow())
            }
            None => render_details(&details, "", &live.borrow()),
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
    act!(update_btn, |ctx: &Ctx, name: &str| encryption::update_app(ctx, name));
    act!(check_btn, |ctx: &Ctx, name: &str| {
        op::run_operation(&ctx.window, "Check updates", vec!["update".into(), name.into(), "--check".into()], {
            let ctx = ctx.clone();
            move |_| ctx.refresh()
        });
    });
    {
        // "Update all" needs no selection — update every out-of-date app.
        let ctx = ctx.clone();
        update_all_btn.connect_clicked(move |_| {
            let ctx2 = ctx.clone();
            confirm(&ctx, "Update all apps?", "Re-resolves and updates every installed app that has a newer version.", false, move || {
                let apps: Vec<String> = list_all_apps()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| m.app.name)
                    .collect();
                encryption::update_all(&ctx2, &apps);
            });
        });
    }
    act!(config_btn, |ctx: &Ctx, name: &str| config::open(ctx, name));
    act!(rename_btn, |ctx: &Ctx, name: &str| rename_app(ctx, name));
    act!(into_btn, |ctx: &Ctx, name: &str| {
        // Merge a package into this app's tree (`install <pkg> --into <name>`).
        let target = name.to_string();
        let ctx_op = ctx.clone();
        text_prompt(ctx, &format!("Install into “{target}”"), "Package to add:", "", move |pkg| {
            let pkg = pkg.trim().to_string();
            if pkg.is_empty() {
                return;
            }
            let args = vec!["install".into(), pkg, "--into".into(), target.clone()];
            op::run_jobs_answering(
                &ctx_op.window,
                "Install into",
                vec![(String::new(), args)],
                None,
                {
                    let ctx = ctx_op.clone();
                    move |_| ctx.refresh()
                },
                install::prompt_handler(&ctx_op),
            );
        });
    });
    act!(shortcut_btn, |ctx: &Ctx, name: &str| make_shortcut(ctx, name));
    act!(snapshot_btn, |ctx: &Ctx, name: &str| {
        op::run_operation(&ctx.window, "Snapshot", vec!["snapshot".into(), name.into()], {
            let ctx = ctx.clone();
            move |_| ctx.refresh()
        });
    });
    act!(rollback_btn, |ctx: &Ctx, name: &str| open_snapshots(ctx, name));
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
        let live = ctx.live.clone();
        Rc::new(move || {
            store.clear();
            render_details(&details, "", &live.borrow());

            // tree_order keeps each `--into` child directly after its parent, so
            // by the time a child is seen its parent iter already exists.
            // An unusable root — the usual cause being ~/.wryayer living in a
            // container that has not been mounted yet — must say so. Swallowed,
            // it reads as "no apps installed", which is both wrong and alarming.
            let apps = match list_all_apps().map(tree_order) {
                Ok(apps) => apps,
                Err(e) => {
                    let iter = store.append(None);
                    let msg = glib::markup_escape_text(&format!("{e:#}"));
                    let first = msg.lines().next().unwrap_or("wryayer cannot read its apps");
                    store.set_value(&iter, 0, &format!("<i>{first}</i>").to_value());
                    store.set_value(&iter, 1, &String::new().to_value());
                    return;
                }
            };
            let mut parent_iters: std::collections::HashMap<String, gtk::TreeIter> =
                std::collections::HashMap::new();
            let mut any = false;

            // One `veracrypt --list` for the whole rebuild, not one per row.
            let encryption =
                crate::commands::encrypt::scan(apps.iter().map(|m| m.app.name.as_str()));

            for m in apps.iter().filter(|m| m.app.wine_game.is_some() == games) {
                any = true;
                let is_child = m
                    .app
                    .alias_of
                    .as_ref()
                    .map(|t| parent_iters.contains_key(t))
                    .unwrap_or(false);
                let markup =
                    row_markup(m, is_child, encryption.get(&m.app.name).copied(), &live.borrow());
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

/// One tree-row label: bold for a parent, dimmed for a child, followed by what
/// is true of the app right now — a dot when an update is waiting, a count when
/// sandboxes of it are running. Same vocabulary as the TUI's list.
fn row_markup(m: &Manifest, is_child: bool, enc: Option<AppEncryption>, live: &Live) -> String {
    let title = match &m.app.display_name {
        Some(d) => format!("{d}  [{}]", m.app.name),
        None => match &m.app.pkg_name {
            Some(p) => format!("{}  [{p}]", m.app.name),
            None => m.app.name.clone(),
        },
    };
    let esc = glib::markup_escape_text(&title);
    let name = if is_child {
        format!("<span alpha='75%'>{esc}</span>")
    } else {
        format!("<b>{esc}</b>")
    };
    let mut markers = String::new();
    if let Some(state) = enc {
        markers.push_str(&encryption_badges(state));
    }
    if live.updates.contains_key(&m.app.name) {
        markers.push('●');
    }
    match live.instances.get(&m.app.name).copied().unwrap_or(0) {
        0 => {}
        n => markers.push_str(&format!(" {n}▶")),
    }
    if markers.is_empty() {
        name
    } else {
        format!("{name}  <span alpha='70%'>{markers}</span>")
    }
}

/// The list markers for an encrypted app: a padlock for the container's current
/// state, and a key when wryayer can open it without asking.
///
/// Two glyphs rather than one because they answer different questions. The
/// padlock is transient — it flips every time the app is launched and closed —
/// while the key reflects a setting, and is what tells the user whether the next
/// launch will stop for a password. Same vocabulary as the TUI.
fn encryption_badges(state: AppEncryption) -> String {
    let lock = if state.locked { "🔒" } else { "🔓" };
    if state.master {
        format!("{lock}🔑")
    } else {
        lock.to_string()
    }
}

/// Rebuild the right-hand details panel for `name` (empty = nothing selected).
fn render_details(container: &gtk::Box, name: &str, live: &Live) {
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
    if let Some(newer) = live.updates.get(name) {
        let line = detail_line(container, "Update", &format!("{newer} available"));
        line.add_css_class("fill-warn");
    }
    detail_line(container, "Installed", m.app.installed_at.get(..10).unwrap_or(&m.app.installed_at));
    let launchers = if m.app.launchers.is_empty() { "none".to_string() } else { m.app.launchers.join(", ") };
    detail_line(container, "Launchers", &launchers);
    if let Some(g) = &m.app.wine_game {
        detail_line(container, "Wine exe", &g.exe);
    }

    // What is running right now, and — for a ram-limited sandbox — how close to
    // its cap it is. The overlay only exists while such a sandbox is up, so a
    // reading doubles as proof the limit is in force.
    if let Some(n) = live.instances.get(name).copied().filter(|n| *n > 0) {
        let fs_root = m.app.alias_of.as_deref().unwrap_or(name);
        let ram = crate::commands::run::sandbox_ram(fs_root)
            .map(|(used, total)| format!("   RAM {used} / {total} MiB"))
            .unwrap_or_default();
        detail_line(container, "Running", &format!("{n} instance(s){ram}"));
    }

    // Size — computed off-thread so a big app can't freeze the panel.
    let size_lbl = detail_line(container, "Size", "computing…");
    dir_bytes_async(name, &size_lbl);

    render_encryption_details(container, name);

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

/// Encryption lines for an app stored in a container; nothing at all for one
/// that isn't.
///
/// Spelled out rather than left to the list's padlock, because "locked" and
/// "asks for a password" are separate facts and a badge only has room to hint
/// at both.
fn render_encryption_details(container: &gtk::Box, name: &str) {
    let states = crate::commands::encrypt::scan([name]);
    let Some(state) = states.get(name).copied() else { return };

    let lock = if state.locked { "🔒 locked" } else { "🔓 unlocked" };
    let source = if state.master {
        "🔑 opens from the master store"
    } else {
        "asks for a password"
    };
    detail_line(container, "Encrypted", &format!("{lock}   {source}"));

    // Only readable while the container is open: statvfs on an unmounted mount
    // point describes the host filesystem, which is a plausible wrong answer.
    if let Some(usage) = state.fill {
        let pct = usage.percent_used();
        let line = detail_line(
            container,
            "Container",
            &format!(
                "{} / {} ({pct}%)",
                format_bytes(usage.used),
                format_bytes(usage.used + usage.available)
            ),
        );
        if pct >= crate::veracrypt::FULL_WARN_PERCENT {
            line.add_css_class("fill-critical");
            detail_item(
                container,
                &format!("nearly full — grow it from Settings, or: wryayer grow {name}"),
            );
        } else if pct >= 75 {
            line.add_css_class("fill-warn");
        }
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

    vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // ── Moving the whole setup ──────────────────────────────────────────
    // Not a backup: a list of what is installed and how it is configured,
    // which is what a *different* machine can act on. Its packages come from
    // whatever package manager that machine has.
    let setup_lbl = gtk::Label::new(Some(
        "Take this machine's app list — and every app's settings — to another          machine, even one with a different package manager.",
    ));
    setup_lbl.set_xalign(0.0);
    setup_lbl.set_wrap(true);
    vbox.append(&setup_lbl);

    let export_setup_btn = gtk::Button::with_label("Export setup list (.toml)…");
    export_setup_btn.set_halign(gtk::Align::Start);
    let import_setup_btn = gtk::Button::with_label("Recreate from a setup list…");
    import_setup_btn.set_halign(gtk::Align::Start);
    vbox.append(&export_setup_btn);
    vbox.append(&import_setup_btn);

    {
        let ctx = ctx.clone();
        export_setup_btn.connect_clicked(move |_| export_setup_list(&ctx));
    }
    {
        let ctx = ctx.clone();
        import_setup_btn.connect_clicked(move |_| import_setup_list(&ctx));
    }
    vbox
}

/// Write the list of installed apps and their settings to a file.
fn export_setup_list(ctx: &Ctx) {
    let today = chrono::Local::now().format("%Y-%m-%d");
    let dialog = gtk::FileDialog::builder()
        .title("Export setup list")
        .initial_name(format!("wryayer-setup-{today}.toml"))
        .build();
    let ctx = ctx.clone();
    dialog.save(Some(&ctx.window.clone()), gtk::gio::Cancellable::NONE, move |res| {
        if let Ok(file) = res {
            if let Some(path) = file.path() {
                op::run_operation(
                    &ctx.window,
                    "Export setup list",
                    vec!["setup".into(), "export".into(), "-o".into(), path.to_string_lossy().into()],
                    |_| {},
                );
            }
        }
    });
}

/// Install what a setup list names, and apply the settings it records.
///
/// The plan is shown first — as a dry run in the same console the install will
/// use — because a list from another machine can name packages this one spells
/// differently, and that is worth seeing before anything is downloaded.
fn import_setup_list(ctx: &Ctx) {
    let filter = gtk::FileFilter::new();
    filter.add_pattern("*.toml");
    filter.set_name(Some("wryayer setup list (*.toml)"));
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let dialog = gtk::FileDialog::builder()
        .title("Recreate from a setup list")
        .filters(&filters)
        .build();
    let ctx = ctx.clone();
    dialog.open(Some(&ctx.window.clone()), gtk::gio::Cancellable::NONE, move |res| {
        let Ok(file) = res else { return };
        let Some(path) = file.path() else { return };
        let path = path.to_string_lossy().into_owned();

        let ctx2 = ctx.clone();
        let path2 = path.clone();
        op::run_operation(
            &ctx.window,
            "Setup list — what it would do",
            vec!["setup".into(), "import".into(), path.clone(), "--dry-run".into()],
            move |ok| {
                if !ok {
                    return;
                }
                let ctx3 = ctx2.clone();
                let path3 = path2.clone();
                ask(
                    &ctx2,
                    "Install everything on that list?",
                    "The console above shows what it would do. Packages are downloaded \
                     and installed from this machine's package manager; anything it \
                     cannot install is reported at the end.",
                    "Install",
                    move || {
                        let ctx = ctx3.clone();
                        op::run_operation(
                            &ctx3.window,
                            "Recreate setup",
                            vec!["setup".into(), "import".into(), path3.clone()],
                            move |_| ctx.refresh(),
                        );
                    },
                );
            },
        );
    });
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

    vbox.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // ── Per-app disk usage ──────────────────────────────────────────────
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let usage_lbl = gtk::Label::new(None);
    usage_lbl.set_xalign(0.0);
    usage_lbl.set_hexpand(true);
    usage_lbl.set_markup("<b>Disk usage</b>");
    let refresh_btn = gtk::Button::with_label("Refresh");
    head.append(&usage_lbl);
    head.append(&refresh_btn);
    vbox.append(&head);

    let total_lbl = gtk::Label::new(Some("Computing…"));
    total_lbl.set_xalign(0.0);
    vbox.append(&total_lbl);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&list));
    vbox.append(&scroller);

    let populate: Rc<dyn Fn()> = {
        let list = list.clone();
        let total_lbl = total_lbl.clone();
        Rc::new(move || {
            while let Some(c) = list.first_child() {
                list.remove(&c);
            }
            total_lbl.set_text("Computing…");
            let rx = spawn_usage();
            let list = list.clone();
            let total_lbl = total_lbl.clone();
            glib::timeout_add_local(Duration::from_millis(80), move || match rx.try_recv() {
                Ok(rows) => {
                    let total: u64 = rows.iter().map(|(_, b)| *b).sum();
                    total_lbl.set_markup(&format!("Total: <b>{}</b>", format_bytes(total)));
                    for (name, bytes) in &rows {
                        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                        row.set_margin_top(2);
                        row.set_margin_bottom(2);
                        row.set_margin_start(4);
                        row.set_margin_end(4);
                        let n = gtk::Label::new(Some(name));
                        n.set_xalign(0.0);
                        n.set_hexpand(true);
                        let s = gtk::Label::new(Some(&format_bytes(*bytes)));
                        s.set_xalign(1.0);
                        row.append(&n);
                        row.append(&s);
                        list.append(&row);
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
            });
        })
    };
    populate();
    {
        let populate = populate.clone();
        refresh_btn.connect_clicked(move |_| populate());
    }

    vbox
}

/// Compute each installed app's on-disk size with `du`, off the main thread.
/// Returns rows sorted largest-first.
fn spawn_usage() -> mpsc::Receiver<Vec<(String, u64)>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut rows: Vec<(String, u64)> = list_all_apps()
            .unwrap_or_default()
            .into_iter()
            .map(|m| {
                let path = format!("{home}/.wryayer/{}", m.app.name);
                let bytes = Command::new("du")
                    .args(["-sb", &path])
                    .output()
                    .ok()
                    .and_then(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .split_whitespace()
                            .next()?
                            .parse::<u64>()
                            .ok()
                    })
                    .unwrap_or(0);
                (m.app.name, bytes)
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        let _ = tx.send(rows);
    });
    rx
}

// ── App actions ────────────────────────────────────────────────────────────────

/// Launch an installed app detached — it runs independently of the GUI.
///
/// A locked app is unlocked through the dialog first. `wryayer run` would mount
/// the container itself, but it is started detached with its output discarded,
/// so a password prompt in there would be a launch that silently never happens.
fn launch_detached(ctx: &Ctx, name: &str) {
    if crate::veracrypt::is_locked(name) {
        let ctx2 = ctx.clone();
        let name = name.to_string();
        encryption::unlock_then(
            ctx,
            &name.clone(),
            Rc::new(move || spawn_run(&ctx2, &name)),
        );
        return;
    }
    spawn_run(ctx, name);
}

/// Start `wryayer run <name>` and forget about it.
fn spawn_run(ctx: &Ctx, name: &str) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    match crate::prompt::forbid_prompts(&mut Command::new(&exe))
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

/// Write an app's `/usr/bin` shortcut and desktop entry, after showing exactly
/// which paths that touches.
///
/// The plan is worth showing because two of its outcomes surprise people: a
/// command name another app already owns is left alone, and so is one that
/// belongs to the system — a sandboxed `bash` must not become `/usr/bin/bash`.
fn make_shortcut(ctx: &Ctx, name: &str) {
    let plan = crate::launcher::shortcut_plan(name);
    if let Some(problem) = plan.problem {
        ctx.status(&problem);
        return;
    }

    let mut body = String::new();
    if !plan.creates.is_empty() {
        body.push_str("Creates:\n");
        for path in &plan.creates {
            body.push_str(&format!("  {path}\n"));
        }
        body.push_str("plus a desktop entry, if the app ships one — so menus and other \
                       applications can reach it.\n");
    }
    if !plan.skips.is_empty() {
        body.push_str("\nLeft alone:\n");
        for skip in &plan.skips {
            body.push_str(&format!("  {skip}\n"));
        }
    }
    body.push_str("\nWriting to a system directory needs your sudo password.");

    let ctx2 = ctx.clone();
    let name = name.to_string();
    ask(ctx, &format!("Create the shortcut for “{name}”?"), &body, "Create", move || {
        encryption::run_as_root(
            &ctx2,
            &format!("Shortcut — {name}"),
            vec!["relink".into(), name.clone()],
            "Shortcuts and desktop entries live in system directories.",
        );
    });
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

/// A snapshot-management window: pick a snapshot to roll back to (or delete).
fn open_snapshots(ctx: &Ctx, name: &str) {
    let win = gtk::Window::builder()
        .title(format!("Snapshots — {name}"))
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(420)
        .default_height(360)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(10);
    vbox.set_margin_bottom(10);
    vbox.set_margin_start(10);
    vbox.set_margin_end(10);

    let heading = gtk::Label::new(None);
    heading.set_xalign(0.0);
    heading.set_markup("<b>Select a snapshot</b>");
    vbox.append(&heading);

    let listbox = gtk::ListBox::new();
    listbox.set_selection_mode(gtk::SelectionMode::Single);
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&listbox));
    vbox.append(&scroller);

    let labels: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let refill = {
        let listbox = listbox.clone();
        let labels = labels.clone();
        let name = name.to_string();
        Rc::new(move || {
            while let Some(c) = listbox.first_child() {
                listbox.remove(&c);
            }
            let snaps = crate::commands::snapshot::labels(&name).unwrap_or_default();
            *labels.borrow_mut() = snaps.clone();
            if snaps.is_empty() {
                let l = gtk::Label::new(Some("No snapshots yet."));
                l.set_margin_top(12);
                l.set_margin_bottom(12);
                listbox.append(&l);
                listbox.set_selection_mode(gtk::SelectionMode::None);
            } else {
                listbox.set_selection_mode(gtk::SelectionMode::Single);
                for s in &snaps {
                    let l = gtk::Label::new(Some(s));
                    l.set_xalign(0.0);
                    l.set_margin_top(3);
                    l.set_margin_bottom(3);
                    l.set_margin_start(4);
                    listbox.append(&l);
                }
            }
        })
    };
    refill();

    let selected = {
        let listbox = listbox.clone();
        let labels = labels.clone();
        move || -> Option<String> {
            let i = listbox.selected_row()?.index();
            labels.borrow().get(i as usize).cloned()
        }
    };

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let rollback = gtk::Button::with_label("Roll back to selected");
    let delete = gtk::Button::with_label("Delete selected");
    delete.add_css_class("destructive-action");
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let close = gtk::Button::with_label("Close");
    bar.append(&rollback);
    bar.append(&delete);
    bar.append(&spacer);
    bar.append(&close);
    vbox.append(&bar);

    win.set_child(Some(&vbox));
    win.present();

    {
        let win = win.clone();
        close.connect_clicked(move |_| win.close());
    }
    {
        let ctx = ctx.clone();
        let selected = selected.clone();
        let name = name.to_string();
        let win = win.clone();
        rollback.connect_clicked(move |_| {
            let Some(label) = selected() else {
                ctx.status("Select a snapshot first.");
                return;
            };
            let ctx2 = ctx.clone();
            let name2 = name.clone();
            let win2 = win.clone();
            confirm(&ctx, &format!("Roll back to {label}?"),
                "The app's files are restored to this snapshot.", true, move || {
                win2.close();
                op::run_operation(&ctx2.window, "Rollback",
                    vec!["rollback".into(), name2.clone(), label.clone()], {
                    let ctx = ctx2.clone();
                    move |_| ctx.refresh()
                });
            });
        });
    }
    {
        let ctx = ctx.clone();
        let selected = selected.clone();
        let refill = refill.clone();
        let name = name.to_string();
        delete.connect_clicked(move |_| {
            let Some(label) = selected() else {
                ctx.status("Select a snapshot first.");
                return;
            };
            let ctx2 = ctx.clone();
            let refill2 = refill.clone();
            let name2 = name.clone();
            confirm(&ctx, &format!("Delete snapshot {label}?"),
                "This permanently removes this snapshot. The app itself is unaffected.", true, move || {
                match crate::commands::snapshot::delete(&name2, &label) {
                    Ok(_) => {
                        ctx2.status(&format!("Deleted snapshot {label}."));
                        refill2();
                    }
                    Err(e) => ctx2.status(&format!("Delete failed: {e}")),
                }
            });
        });
    }
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
    ask(ctx, title, body, if danger { "Remove" } else { "OK" }, on_confirm);
}

/// As [`confirm`], but the accepting button says what it will do — for the
/// questions a child asked, where "OK" would not say which of two courses is
/// being taken.
pub fn ask<F>(ctx: &Ctx, title: &str, body: &str, action: &str, on_confirm: F)
where
    F: Fn() + 'static,
{
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

#[cfg(test)]
mod badge_tests {
    use super::*;

    fn state(locked: bool, master: bool) -> AppEncryption {
        AppEncryption { locked, master, fill: None }
    }

    #[test]
    fn the_badges_match_the_vocabulary_the_tui_uses() {
        // Two front-ends teaching two different symbol sets would be worse than
        // either teaching none.
        assert_eq!(encryption_badges(state(true, false)), "🔒");
        assert_eq!(encryption_badges(state(false, false)), "🔓");
        assert_eq!(encryption_badges(state(true, true)), "🔒🔑");
        assert_eq!(encryption_badges(state(false, true)), "🔓🔑");
    }

    fn manifest(name: &str) -> Manifest {
        Manifest {
            app: crate::manifest::AppMeta {
                name: name.into(),
                main_binary: name.into(),
                installed_at: "2026-01-01".into(),
                launchers: vec![],
                alias_of: None,
                display_name: None,
                pkg_name: None,
                wine_game: None,
            },
            packages: vec![],
        }
    }

    #[test]
    fn a_plain_app_gets_no_badge_at_all() {
        let markup = row_markup(&manifest("plain"), false, None, &Live::default());
        assert!(!markup.contains('🔒'), "{markup}");
        assert!(!markup.contains('🔓'), "{markup}");
        assert!(markup.contains("plain"), "{markup}");
    }

    #[test]
    fn an_app_with_an_update_is_marked() {
        let live = Live {
            updates: HashMap::from([("plain".to_string(), "2.0-1".to_string())]),
            ..Default::default()
        };
        assert!(row_markup(&manifest("plain"), false, None, &live).contains('●'));
        // …and one without it is not.
        assert!(!row_markup(&manifest("other"), false, None, &live).contains('●'));
    }

    #[test]
    fn running_sandboxes_are_counted_on_the_row() {
        let live = Live {
            instances: HashMap::from([("plain".to_string(), 2)]),
            ..Default::default()
        };
        assert!(row_markup(&manifest("plain"), false, None, &live).contains("2▶"));
        // Zero is not worth a marker.
        let idle = Live { instances: HashMap::from([("plain".to_string(), 0)]), ..Default::default() };
        assert!(!row_markup(&manifest("plain"), false, None, &idle).contains('▶'));
    }
}
