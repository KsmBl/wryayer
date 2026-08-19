//! The Install tab: search repos + the AUR, tick any number of packages, and
//! install them all at once — plus the "import a Windows game" wizard.

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::glib;

use super::{confirm, encryption, op, text_prompt, Ctx};
use crate::child_output::ChildLine;
use crate::manifest::{list_all_apps, read_manifest};

struct PkgResult {
    name: String,
    source: &'static str,
    desc: String,
}

/// Currently ticked package names, in the order they were ticked.
type Selection = Rc<RefCell<Vec<String>>>;

pub fn build_tab(ctx: &Ctx) -> (gtk::Box, Rc<dyn Fn()>) {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.set_margin_top(6);
    vbox.set_margin_bottom(6);
    vbox.set_margin_start(6);
    vbox.set_margin_end(6);

    // Search row.
    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let search = gtk::SearchEntry::new();
    search.set_hexpand(true);
    search.set_placeholder_text(Some("Search official repos and the AUR…"));
    let search_btn = gtk::Button::with_label("Search");
    let spinner = gtk::Spinner::new();
    search_row.append(&search);
    search_row.append(&spinner);
    search_row.append(&search_btn);
    vbox.append(&search_row);

    // Manual add row.
    let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let add_entry = gtk::Entry::new();
    add_entry.set_hexpand(true);
    add_entry.set_placeholder_text(Some("…or type an exact package name and click Add"));
    let add_btn = gtk::Button::with_label("Add");
    add_row.append(&add_entry);
    add_row.append(&add_btn);
    vbox.append(&add_row);

    // Results.
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&list));
    vbox.append(&scroller);

    // Where the packages land. A merge target puts them in an existing app's
    // tree (`--into`), sharing everything already extracted there, and gives
    // each its own thin alias entry — the TUI asks the same question per
    // package; here it is one choice for the batch.
    let merge_targets: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    // Bottom bar.
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let target_model = gtk::StringList::new(&[]);
    let target = gtk::DropDown::new(Some(target_model.clone()), gtk::Expression::NONE);
    target.set_tooltip_text(Some(
        "Install each package as its own app, or merge it into an existing app's \
         tree — for plugins and tool bundles that belong with something already \
         installed.",
    ));
    let target_caption = gtk::Label::new(Some("Install as"));
    bottom.append(&target_caption);
    bottom.append(&target);

    // Filled now and again after every operation, since installing or removing
    // an app changes what there is to merge into. The chosen target is kept
    // across a rebuild when it is still installed.
    let refresh_targets: Rc<dyn Fn()> = {
        let merge_targets = merge_targets.clone();
        let target = target.clone();
        let target_model = target_model.clone();
        let target_caption = target_caption.clone();
        Rc::new(move || {
            let chosen = match target.selected() {
                0 => None,
                n => merge_targets.borrow().get((n - 1) as usize).cloned(),
            };
            let fresh: Vec<String> = list_all_apps()
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.app.alias_of.is_none() && m.app.wine_game.is_none())
                .map(|m| m.app.name)
                .collect();

            while target_model.n_items() > 0 {
                target_model.remove(0);
            }
            target_model.append("a new app of its own");
            for name in &fresh {
                target_model.append(&format!("into {name}"));
            }
            let restored = chosen
                .and_then(|c| fresh.iter().position(|n| *n == c))
                .map(|i| i as u32 + 1)
                .unwrap_or(0);
            target.set_selected(restored);

            // Nothing to merge into on a fresh machine — then the choice is not
            // a choice, and the row would only be in the way.
            let any = !fresh.is_empty();
            target.set_visible(any);
            target_caption.set_visible(any);
            *merge_targets.borrow_mut() = fresh;
        })
    };
    refresh_targets();
    let count_label = gtk::Label::new(Some("0 selected"));
    count_label.set_xalign(0.0);
    count_label.set_hexpand(true);
    let install_btn = gtk::Button::with_label("Install selected");
    install_btn.set_sensitive(false);

    // Whether to put the app on the PATH, starting from the Settings-tab
    // default. The TUI asks this per install; a checkbox says the same thing
    // without a dialog in the way of a batch.
    let shortcut_check = gtk::CheckButton::with_label("Shortcut");
    shortcut_check.set_active(crate::config::read_global_config().create_shortcut);
    shortcut_check.set_tooltip_text(Some(
        "Create /usr/bin/<name> (and a desktop entry) so the app can be started \
         by name. Without it the app is still installed — run it with \
         `wryayer run <name>`.",
    ));
    bottom.append(&shortcut_check);

    // Encryption is offered here rather than after the fact, mirroring the TUI's
    // install-time prompt: the container is sized from what the app actually
    // occupies, so making the choice now saves a whole second copy of the tree.
    let encrypt_check = gtk::CheckButton::with_label("Encrypt");
    encrypt_check.set_tooltip_text(Some(
        "Install into its own VeraCrypt container, mounted over the app's normal \
         directory. While locked its whole tree is unreadable, filenames included.",
    ));
    let encrypt_source = gtk::DropDown::from_strings(&[
        "password at every launch",
        "password in the master store",
        "generated password in the master store",
    ]);
    encrypt_source.set_selected(2);
    encrypt_source.set_sensitive(false);
    if crate::veracrypt::available() {
        bottom.append(&encrypt_check);
        bottom.append(&encrypt_source);
    }

    bottom.append(&count_label);
    bottom.append(&install_btn);
    vbox.append(&bottom);

    {
        let encrypt_source = encrypt_source.clone();
        encrypt_check.connect_toggled(move |c| encrypt_source.set_sensitive(c.is_active()));
    }

    let selection: Selection = Rc::new(RefCell::new(Vec::new()));

    let update_count: Rc<dyn Fn()> = {
        let selection = selection.clone();
        let count_label = count_label.clone();
        let install_btn = install_btn.clone();
        Rc::new(move || {
            let n = selection.borrow().len();
            count_label.set_text(&format!("{n} selected"));
            install_btn.set_sensitive(n > 0);
        })
    };

    // Manual add.
    {
        let selection = selection.clone();
        let list = list.clone();
        let update_count = update_count.clone();
        let add_entry2 = add_entry.clone();
        let do_add = Rc::new(move || {
            let name = add_entry2.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            if !selection.borrow().iter().any(|n| n == &name) {
                selection.borrow_mut().push(name.clone());
            }
            list.prepend(&result_row(
                &PkgResult { name, source: "manual", desc: String::new() },
                &selection,
                &update_count,
            ));
            add_entry2.set_text("");
            update_count();
        });
        let do_add2 = do_add.clone();
        add_btn.connect_clicked(move |_| do_add2());
        add_entry.connect_activate(move |_| do_add());
    }

    // Search.
    let run_search: Rc<dyn Fn()> = {
        let search = search.clone();
        let list = list.clone();
        let selection = selection.clone();
        let update_count = update_count.clone();
        let spinner = spinner.clone();
        Rc::new(move || {
            let query = search.text().trim().to_string();
            if query.len() < 2 {
                return;
            }
            spinner.start();
            let rx = search_packages(query);
            let list = list.clone();
            let selection = selection.clone();
            let update_count = update_count.clone();
            let spinner = spinner.clone();
            glib::timeout_add_local(Duration::from_millis(60), move || {
                match rx.try_recv() {
                    Ok(results) => {
                        while let Some(child) = list.first_child() {
                            list.remove(&child);
                        }
                        for r in &results {
                            list.append(&result_row(r, &selection, &update_count));
                        }
                        if results.is_empty() {
                            let l = gtk::Label::new(Some("No matches."));
                            l.set_margin_top(10);
                            l.set_margin_bottom(10);
                            list.append(&l);
                        }
                        spinner.stop();
                        glib::ControlFlow::Break
                    }
                    Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        spinner.stop();
                        glib::ControlFlow::Break
                    }
                }
            });
        })
    };
    {
        let run_search = run_search.clone();
        search_btn.connect_clicked(move |_| run_search());
    }
    {
        let run_search = run_search.clone();
        search.connect_activate(move |_| run_search());
    }

    // Install everything ticked.
    {
        let ctx = ctx.clone();
        let selection = selection.clone();
        let target = target.clone();
        let merge_targets = merge_targets.clone();
        let shortcut_check = shortcut_check.clone();
        install_btn.connect_clicked(move |_| {
            let names = selection.borrow().clone();
            if names.is_empty() {
                return;
            }
            // If any ticked package is already installed, resolve that first
            // (uninstall or install-a-copy) before touching the batch.
            if let Some(existing) = names.iter().find(|n| read_manifest(n).is_ok()) {
                already_installed_dialog(&ctx, existing);
                return;
            }
            let encrypt = encrypt_check.is_active();
            let (use_master, generate) = match encrypt_source.selected() {
                0 => (false, false),
                1 => (true, false),
                _ => (true, true),
            };
            let mut extra: Vec<String> = Vec::new();
            // The same flag the TUI's "no shortcut" answer passes: it keeps the
            // files without putting a command name on the PATH.
            if !shortcut_check.is_active() {
                extra.push("--keep-without-launcher".into());
            }
            if encrypt {
                extra.push("--encrypt".into());
                if use_master {
                    extra.push("--encrypt-master".into());
                }
                if generate {
                    extra.push("--encrypt-generate".into());
                }
            }
            // Row 0 is "a new app of its own"; the rest name a merge target.
            let into = match target.selected() {
                0 => None,
                n => merge_targets.borrow().get((n - 1) as usize).cloned(),
            };
            let jobs: Vec<(String, Vec<String>)> = names
                .iter()
                .map(|n| {
                    let mut args = vec!["install".to_string(), n.clone()];
                    if let Some(t) = &into {
                        args.extend(["--into".to_string(), t.clone()]);
                    }
                    args.extend(extra.iter().cloned());
                    (format!("install {n}"), args)
                })
                .collect();

            if !encrypt {
                op::run_jobs_answering(
                    &ctx.window,
                    "Install",
                    jobs,
                    None,
                    {
                        let ctx = ctx.clone();
                        move |_| ctx.refresh()
                    },
                    prompt_handler(&ctx),
                );
                return;
            }
            encryption::install_encrypted(&ctx, jobs, use_master, generate, names.len());
        });
    }

    (vbox, refresh_targets)
}

/// A handler for the questions an install's child asks — put to the user as a
/// dialog, and answered by re-running the same install with one more flag.
///
/// The TUI answers these the same way; both have to, because the child cannot
/// ask for itself: it has no terminal, so it prints the question and exits.
pub fn prompt_handler(ctx: &Ctx) -> Rc<dyn Fn(op::Prompt)> {
    let ctx = ctx.clone();
    Rc::new(move |prompt: op::Prompt| match prompt.line {
        ChildLine::NoLauncher { pkg, bins } => no_launcher_dialog(&ctx, &pkg, &bins, prompt.args),
        ChildLine::OutdatedPackages { pkg } => outdated_dialog(&ctx, &pkg, prompt.args),
        // Progress never reaches a handler — the console draws it.
        ChildLine::Progress(..) => {}
    })
}

/// The package installed nothing wryayer would call a launcher, so it kept
/// nothing. Offer to install it regardless.
fn no_launcher_dialog(ctx: &Ctx, pkg: &str, bins: &[String], args: Vec<String>) {
    let body = if bins.is_empty() {
        format!(
            "“{pkg}” installed no program at all, so there is nothing to run \
             it with and nothing was kept.\n\n\
             Install it anyway? Its files stay in their own tree — which is what a \
             library, a plugin or a data package is for — but no shortcut is created."
        )
    } else {
        format!(
            "“{pkg}” installed no program named after it, so nothing was \
             kept.\n\n\
             It does install: {}\n\n\
             Install it anyway (no shortcut), or cancel and install it again picking one \
             of those as the launcher.",
            bins.join(", ")
        )
    };
    let ctx2 = ctx.clone();
    let pkg = pkg.to_string();
    super::ask(ctx, &format!("No launcher in “{pkg}”"), &body, "Install anyway", move || {
        let mut args = args.clone();
        args.push("--keep-without-launcher".into());
        encryption::rerun_install(&ctx2, format!("Install — {pkg} (no launcher)"), args);
    });
}

/// A download 404'd because the local package databases are older than the
/// mirror. Offer the refresh that fixes it.
fn outdated_dialog(ctx: &Ctx, pkg: &str, args: Vec<String>) {
    let body = format!(
        "“{pkg}” could not be downloaded: your local package databases are \
         older than the mirror, so they name versions that are no longer there.\n\n\
         Refreshing them needs root — the install runs `sudo pacman -Sy` first, then \
         tries again."
    );
    let ctx2 = ctx.clone();
    let pkg = pkg.to_string();
    super::ask(ctx, "Package databases are out of date", &body, "Refresh and retry", move || {
        let mut args = args.clone();
        args.push("--sync-db".into());
        encryption::rerun_install(&ctx2, format!("Install — {pkg} (refreshed sources)"), args);
    });
}

/// Shown when a package to install is already installed: offer to install a
/// second copy under a new name, or uninstall the existing one (mirrors the
/// TUI's already-installed prompt).
fn already_installed_dialog(ctx: &Ctx, pkg: &str) {
    let win = gtk::Window::builder()
        .title("Already installed")
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(400)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 10);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);

    let msg = gtk::Label::new(None);
    msg.set_xalign(0.0);
    msg.set_wrap(true);
    msg.set_markup(&format!(
        "<b>“{}” is already installed.</b>\nInstall a second copy under a new name, or uninstall the existing one?",
        gtk::glib::markup_escape_text(pkg)
    ));
    vbox.append(&msg);

    let copy_btn = gtk::Button::with_label("Install another copy…");
    let uninstall_btn = gtk::Button::with_label("Uninstall existing");
    uninstall_btn.add_css_class("destructive-action");
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    bar.append(&copy_btn);
    bar.append(&uninstall_btn);
    bar.append(&spacer);
    bar.append(&cancel);
    vbox.append(&bar);

    win.set_child(Some(&vbox));
    win.present();

    {
        let win = win.clone();
        cancel.connect_clicked(move |_| win.close());
    }
    {
        let ctx = ctx.clone();
        let win = win.clone();
        let pkg = pkg.to_string();
        copy_btn.connect_clicked(move |_| {
            win.close();
            let ctx2 = ctx.clone();
            let pkg2 = pkg.clone();
            text_prompt(&ctx, "Install another copy", "Name for the new copy:", &format!("{pkg}-2"), move |name| {
                let name = name.trim().to_string();
                if name.is_empty() {
                    return;
                }
                op::run_operation(&ctx2.window, "Install",
                    vec!["install".into(), pkg2.clone(), "--app-name".into(), name], {
                    let ctx = ctx2.clone();
                    move |_| ctx.refresh()
                });
            });
        });
    }
    {
        let ctx = ctx.clone();
        let win = win.clone();
        let pkg = pkg.to_string();
        uninstall_btn.connect_clicked(move |_| {
            win.close();
            let pkg2 = pkg.clone();
            confirm(&ctx, &format!("Uninstall “{pkg}”?"),
                "This deletes the existing app and its launchers.", true, {
                let ctx = ctx.clone();
                move || op::run_operation(&ctx.window, "Remove", vec!["remove".into(), pkg2.clone()], {
                    let ctx = ctx.clone();
                    move |_| ctx.refresh()
                })
            });
        });
    }
}

/// One result row: a tick box + the package name/description.
fn result_row(pkg: &PkgResult, selection: &Selection, update_count: &Rc<dyn Fn()>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_margin_top(2);
    row.set_margin_bottom(2);
    row.set_margin_start(4);
    row.set_margin_end(4);

    let check = gtk::CheckButton::new();
    check.set_valign(gtk::Align::Center);
    check.set_active(selection.borrow().iter().any(|n| n == &pkg.name));
    row.append(&check);

    let text = gtk::Label::new(None);
    text.set_xalign(0.0);
    let sub = if pkg.desc.is_empty() {
        pkg.source.to_string()
    } else {
        format!("{} — {}", pkg.source, pkg.desc)
    };
    text.set_markup(&format!(
        "<b>{}</b>\n<small>{}</small>",
        glib::markup_escape_text(&pkg.name),
        glib::markup_escape_text(&sub)
    ));
    row.append(&text);

    let name = pkg.name.clone();
    let selection = selection.clone();
    let update_count = update_count.clone();
    check.connect_toggled(move |c| {
        let mut sel = selection.borrow_mut();
        if c.is_active() {
            if !sel.iter().any(|n| n == &name) {
                sel.push(name.clone());
            }
        } else {
            sel.retain(|n| n != &name);
        }
        drop(sel);
        update_count();
    });
    row
}

/// Search official repos (pacman) and the AUR (RPC) off the main thread.
fn search_packages(query: String) -> mpsc::Receiver<Vec<PkgResult>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut results: Vec<PkgResult> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        if let Ok(out) = Command::new("pacman").args(["-Ssq", &query]).output() {
            for name in String::from_utf8_lossy(&out.stdout).lines().take(50) {
                let name = name.trim().to_string();
                if !name.is_empty() && seen.insert(name.clone()) {
                    results.push(PkgResult { name, source: "repo", desc: String::new() });
                }
            }
        }

        let url = format!(
            "https://aur.archlinux.org/rpc/v5/search/{}?by=name-desc",
            urlencode(&query)
        );
        if let Ok(resp) = reqwest::blocking::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(8))
            .send()
        {
            if let Ok(json) = resp.json::<serde_json::Value>() {
                if let Some(arr) = json.get("results").and_then(|r| r.as_array()) {
                    for item in arr.iter().take(60) {
                        let name = item
                            .get("Name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if name.is_empty() || !seen.insert(name.clone()) {
                            continue;
                        }
                        let desc = item
                            .get("Description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        results.push(PkgResult { name, source: "AUR", desc });
                    }
                }
            }
        }

        let _ = tx.send(results);
    });
    rx
}

/// Minimal percent-encoding for the query path segment.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── Windows-game import wizard ─────────────────────────────────────────────────

pub fn open_game_wizard(ctx: &Ctx) {
    let dialog = gtk::FileDialog::builder()
        .title("Select the game folder")
        .build();
    let ctx = ctx.clone();
    dialog.select_folder(Some(&ctx.window.clone()), gtk::gio::Cancellable::NONE, move |res| {
        if let Ok(folder) = res {
            if let Some(path) = folder.path() {
                game_details_window(&ctx, path);
            }
        }
    });
}

fn game_details_window(ctx: &Ctx, path: std::path::PathBuf) {
    let default_name = path
        .file_name()
        .map(|s| sanitize(&s.to_string_lossy()))
        .unwrap_or_else(|| "game".into());

    let win = gtk::Window::builder()
        .title("Import Windows game")
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(460)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_top(10);
    vbox.set_margin_bottom(10);
    vbox.set_margin_start(10);
    vbox.set_margin_end(10);

    let path_lbl = gtk::Label::new(None);
    path_lbl.set_xalign(0.0);
    path_lbl.set_markup(&format!("<small>{}</small>", glib::markup_escape_text(&path.to_string_lossy())));
    vbox.append(&path_lbl);

    let name_lbl = gtk::Label::new(Some("Container name:"));
    name_lbl.set_xalign(0.0);
    vbox.append(&name_lbl);
    let name_entry = gtk::Entry::new();
    name_entry.set_text(&default_name);
    vbox.append(&name_entry);

    let exe_lbl = gtk::Label::new(Some("Main .exe (optional — auto-detected if blank):"));
    exe_lbl.set_xalign(0.0);
    vbox.append(&exe_lbl);
    let exe_entry = gtk::Entry::new();
    vbox.append(&exe_entry);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let import = gtk::Button::with_label("Import game");
    bar.append(&spacer);
    bar.append(&cancel);
    bar.append(&import);
    vbox.append(&bar);

    win.set_child(Some(&vbox));
    win.present();

    {
        let win = win.clone();
        cancel.connect_clicked(move |_| win.close());
    }
    {
        let ctx = ctx.clone();
        let win = win.clone();
        import.connect_clicked(move |_| {
            let mut args: Vec<String> = vec!["install-game".into(), path.to_string_lossy().into()];
            let name = name_entry.text().trim().to_string();
            if !name.is_empty() {
                args.push("--app-name".into());
                args.push(name);
            }
            let exe = exe_entry.text().trim().to_string();
            if !exe.is_empty() {
                args.push("--exe".into());
                args.push(exe);
            }
            win.close();
            op::run_operation(&ctx.window, "Import game", args, {
                let ctx = ctx.clone();
                move |_| ctx.refresh()
            });
        });
    }
}

fn sanitize(s: &str) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}
