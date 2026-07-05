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

use super::{op, Ctx};

struct PkgResult {
    name: String,
    source: &'static str,
    desc: String,
}

/// Currently ticked package names, in the order they were ticked.
type Selection = Rc<RefCell<Vec<String>>>;

pub fn build_tab(ctx: &Ctx) -> gtk::Box {
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

    // Bottom bar.
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let count_label = gtk::Label::new(Some("0 selected"));
    count_label.set_xalign(0.0);
    count_label.set_hexpand(true);
    let install_btn = gtk::Button::with_label("Install selected");
    install_btn.set_sensitive(false);
    bottom.append(&count_label);
    bottom.append(&install_btn);
    vbox.append(&bottom);

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
        install_btn.connect_clicked(move |_| {
            let names = selection.borrow().clone();
            if names.is_empty() {
                return;
            }
            let jobs: Vec<(String, Vec<String>)> = names
                .iter()
                .map(|n| (format!("install {n}"), vec!["install".into(), n.clone()]))
                .collect();
            op::run_jobs(&ctx.window, "Install", jobs, {
                let ctx = ctx.clone();
                move |_| ctx.refresh()
            });
        });
    }

    vbox
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
