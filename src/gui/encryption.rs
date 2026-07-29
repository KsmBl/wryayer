//! Moving apps into and out of VeraCrypt containers from the GUI, and managing
//! the master password store.
//!
//! ## Why the passwords are collected here
//!
//! The `wryayer` subprocesses these dialogs launch would normally prompt on a
//! terminal. There isn't one: the GUI streams them into a `TextView`, so a
//! child asking for a password would block forever against a pipe with nothing
//! to read. Every secret is therefore gathered up front and handed to the child
//! on stdin — the same `--encrypt-secrets-stdin` channel the TUI uses.
//!
//! Which secrets are *needed* is decided the same way as in the TUI: anything
//! already satisfied is skipped, so an authenticated sudo and a master store
//! already unlocked this boot mean the dialog asks for nothing at all.
//!
//! Unlike the TUI, which walks its prompts one screen at a time, a GUI can show
//! them together — so this is one form with only the fields that apply.

use std::rc::Rc;

use gtk4 as gtk;
use gtk::prelude::*;

use super::{op, Ctx};

/// Which passwords an operation still has to ask for.
#[derive(Clone, Copy, Default)]
struct Needs {
    sudo: bool,
    /// The store does not exist yet: ask twice and create it.
    master_new: bool,
    /// The store exists but is locked this boot.
    master_existing: bool,
    /// A container being created: ask twice.
    container_new: bool,
    /// An existing container that has to be opened.
    container_existing: bool,
}

impl Needs {
    fn nothing(&self) -> bool {
        !(self.sudo
            || self.master_new
            || self.master_existing
            || self.container_new
            || self.container_existing)
    }

    /// What a *new* container needs, given where its password will come from.
    fn for_new_container(use_master: bool, generate: bool) -> Self {
        let mut needs = Needs { sudo: !crate::veracrypt::sudo_is_primed(), ..Default::default() };
        if use_master {
            if !crate::secrets::exists() {
                needs.master_new = true;
            } else if !crate::secrets::is_unlocked() {
                needs.master_existing = true;
            }
        }
        // A generated password is never typed, so there is nothing to confirm.
        needs.container_new = !generate;
        needs
    }

    /// What opening an *existing* container needs.
    fn for_existing_container(app_name: &str) -> Self {
        let known = crate::secrets::open_cached()
            .ok()
            .flatten()
            .is_some_and(|s| s.get(app_name).is_some());
        Needs {
            sudo: !crate::veracrypt::sudo_is_primed(),
            container_existing: !known,
            ..Default::default()
        }
    }
}

/// The collected answers, in the shape the child expects on stdin.
#[derive(Default)]
struct Secrets {
    sudo: String,
    master: String,
    container: String,
}

impl Secrets {
    /// `key=value` lines. Empty values are omitted rather than sent blank,
    /// which the child would take as an empty password.
    fn payload(&self) -> String {
        let mut out = String::new();
        for (key, value) in
            [("sudo", &self.sudo), ("master", &self.master), ("container", &self.container)]
        {
            if !value.is_empty() {
                out.push_str(&format!("{key}={value}\n"));
            }
        }
        out
    }
}

/// A masked entry with a caption.
fn secret_entry(form: &gtk::Box, caption: &str, hint: Option<&str>) -> gtk::Entry {
    let label = gtk::Label::new(Some(caption));
    label.set_xalign(0.0);
    form.append(&label);

    let entry = gtk::Entry::new();
    entry.set_visibility(false);
    entry.set_activates_default(true);
    form.append(&entry);

    if let Some(hint) = hint {
        let l = gtk::Label::new(Some(hint));
        l.set_xalign(0.0);
        l.set_wrap(true);
        l.add_css_class("app-subtitle");
        form.append(&l);
    }
    entry
}

/// Ask for whatever `needs` says is missing, then hand the answers to `on_ok`.
///
/// Validates what can be validated before the operation starts: a wrong sudo
/// password or a wrong master password would otherwise only surface once the
/// child was already several minutes into copying a container.
fn collect(ctx: &Ctx, title: &str, needs: Needs, on_ok: Rc<dyn Fn(Secrets)>) {
    if needs.nothing() {
        on_ok(Secrets::default());
        return;
    }

    let win = gtk::Window::builder()
        .title(title)
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(420)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 4);
    form.set_margin_top(10);
    form.set_margin_bottom(10);
    form.set_margin_start(10);
    form.set_margin_end(10);

    let sudo = needs.sudo.then(|| {
        secret_entry(&form, "Your sudo password", Some("VeraCrypt needs root to mount the container."))
    });
    let master_new = needs.master_new.then(|| {
        (
            secret_entry(
                &form,
                "New master password",
                Some("You'll type this once per boot to unlock stored passwords."),
            ),
            secret_entry(&form, "Repeat the master password", None),
        )
    });
    let master_existing = needs.master_existing.then(|| {
        secret_entry(
            &form,
            "Master password",
            Some("Unlocks the master password store for this boot."),
        )
    });
    let container_new = needs.container_new.then(|| {
        (
            secret_entry(
                &form,
                "New container password",
                Some("Opens this app's container."),
            ),
            secret_entry(&form, "Repeat the container password", None),
        )
    });
    let container_existing = needs.container_existing.then(|| {
        secret_entry(&form, "Container password", Some("The password this app's container was created with."))
    });

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("fill-critical");
    form.append(&error);
    outer.append(&form);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(6);
    bar.set_margin_end(6);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("Continue");
    ok.add_css_class("suggested-action");
    bar.append(&spacer);
    bar.append(&cancel);
    bar.append(&ok);
    outer.append(&bar);

    win.set_child(Some(&outer));
    win.present();

    {
        let win = win.clone();
        cancel.connect_clicked(move |_| win.close());
    }
    ok.connect_clicked(move |_| {
        let mut out = Secrets::default();

        if let Some(entry) = &sudo {
            let value = entry.text().to_string();
            // Checked now: a wrong one would otherwise surface as a veracrypt
            // failure long into the operation.
            if let Err(e) = crate::veracrypt::prime_sudo(&value) {
                error.set_text(&format!("{e:#}"));
                return;
            }
            out.sudo = value;
        }
        if let Some((first, second)) = &master_new {
            let value = first.text().to_string();
            if value.is_empty() {
                error.set_text("The master password must not be empty.");
                return;
            }
            if value != second.text() {
                error.set_text("The master passwords did not match.");
                return;
            }
            out.master = value;
        }
        if let Some(entry) = &master_existing {
            let value = entry.text().to_string();
            if let Err(e) = crate::secrets::open(&value) {
                error.set_text(&format!("{e:#}"));
                return;
            }
            out.master = value;
        }
        if let Some((first, second)) = &container_new {
            let value = first.text().to_string();
            if value.is_empty() {
                error.set_text("The container password must not be empty.");
                return;
            }
            if value != second.text() {
                error.set_text("The container passwords did not match.");
                return;
            }
            out.container = value;
        }
        if let Some(entry) = &container_existing {
            let value = entry.text().to_string();
            if value.is_empty() {
                error.set_text("The container password must not be empty.");
                return;
            }
            // Not verified here: checking it means mounting the container,
            // which needs root and is what the operation is about to do anyway.
            out.container = value;
        }

        win.close();
        on_ok(out);
    });
}

// ── Per-app operations ────────────────────────────────────────────────────────

/// Where a new container's password should come from.
const SOURCES: &[(&str, &str)] = &[
    ("Type it at every launch", "Nothing is stored on disk. Lose it and the container is gone."),
    ("Keep it in the master store", "Unlocked once per boot, then launches don't ask."),
    ("Generate one into the master store", "32 random characters you never have to type."),
];

/// Ask how to encrypt `app_name`, then do it.
pub fn encrypt_app(ctx: &Ctx, app_name: &str) {
    let win = gtk::Window::builder()
        .title(format!("Encrypt {app_name}"))
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(460)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    form.set_margin_top(10);
    form.set_margin_bottom(10);
    form.set_margin_start(10);
    form.set_margin_end(10);

    let blurb = gtk::Label::new(Some(
        "The whole app — binaries, config, profile — is copied into an encrypted \
         volume mounted over its normal directory. While locked, nothing in it is \
         readable, filenames included.\n\n\
         Copying a large app takes a while. The plaintext original is kept until \
         the copy is verified, so an interruption leaves the app exactly as it was.",
    ));
    blurb.set_xalign(0.0);
    blurb.set_wrap(true);
    form.append(&blurb);

    let heading = gtk::Label::new(None);
    heading.set_xalign(0.0);
    heading.set_markup("<b>Where should its password come from?</b>");
    heading.set_margin_top(8);
    form.append(&heading);

    let mut radios: Vec<gtk::CheckButton> = Vec::new();
    for (i, (label, hint)) in SOURCES.iter().enumerate() {
        let radio = gtk::CheckButton::with_label(label);
        if let Some(first) = radios.first() {
            radio.set_group(Some(first));
        }
        if i == 1 {
            radio.set_active(true); // the one most people want
        }
        form.append(&radio);
        let l = gtk::Label::new(Some(hint));
        l.set_xalign(0.0);
        l.set_margin_start(24);
        l.set_wrap(true);
        l.add_css_class("app-subtitle");
        form.append(&l);
        radios.push(radio);
    }
    outer.append(&form);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(6);
    bar.set_margin_end(6);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let go = gtk::Button::with_label("Encrypt");
    go.add_css_class("suggested-action");
    bar.append(&spacer);
    bar.append(&cancel);
    bar.append(&go);
    outer.append(&bar);

    win.set_child(Some(&outer));
    win.present();

    {
        let win = win.clone();
        cancel.connect_clicked(move |_| win.close());
    }
    {
        let ctx = ctx.clone();
        let app_name = app_name.to_string();
        let win = win.clone();
        go.connect_clicked(move |_| {
            let choice = radios.iter().position(|r| r.is_active()).unwrap_or(1);
            let (use_master, generate) = match choice {
                0 => (false, false),
                2 => (true, true),
                _ => (true, false),
            };
            win.close();

            let mut args = vec!["encrypt".to_string(), app_name.clone()];
            if use_master {
                args.push("--master".into());
            }
            if generate {
                args.push("--generate".into());
            }
            run_with_secrets(
                &ctx,
                &format!("Encrypt — {app_name}"),
                args,
                Needs::for_new_container(use_master, generate),
            );
        });
    }
}

/// Confirm, then move `app_name` back out of its container.
pub fn decrypt_app(ctx: &Ctx, app_name: &str) {
    let ctx2 = ctx.clone();
    let name = app_name.to_string();
    super::confirm(
        ctx,
        &format!("Remove encryption from '{name}'?"),
        "Its files are copied out of the container, and the container is deleted \
         along with any password stored for it.\n\n\
         The app keeps working — it is just no longer encrypted at rest.",
        false,
        move || {
            let args = vec!["decrypt".to_string(), name.clone()];
            run_with_secrets(
                &ctx2,
                &format!("Remove encryption — {name}"),
                args,
                Needs::for_existing_container(&name),
            );
        },
    );
}

/// Mount `app_name`'s container, so its files (and its settings) are reachable.
pub fn unlock_container(ctx: &Ctx, app_name: &str) {
    run_with_secrets(
        ctx,
        &format!("Unlock — {app_name}"),
        vec!["unlock".to_string(), app_name.to_string()],
        Needs::for_existing_container(app_name),
    );
}

/// Unmount `app_name`'s container so its files stop being readable.
///
/// Needs no password — only root, to unmount.
pub fn lock_container(ctx: &Ctx, app_name: &str) {
    run_with_secrets(
        ctx,
        &format!("Lock — {app_name}"),
        vec!["lock".to_string(), app_name.to_string()],
        Needs { sudo: !crate::veracrypt::sudo_is_primed(), ..Default::default() },
    );
}

/// Enlarge `app_name`'s container.
pub fn grow_container(ctx: &Ctx, app_name: &str) {
    let ctx2 = ctx.clone();
    let name = app_name.to_string();
    super::text_prompt(
        ctx,
        &format!("Grow {name}'s container"),
        "New size (e.g. 16G), or leave blank to re-size it the way a fresh \
         container would be for the data it holds:",
        "",
        move |value| {
            let mut args = vec!["grow".to_string(), name.clone()];
            let value = value.trim();
            if !value.is_empty() {
                args.push("--to".into());
                args.push(value.to_string());
            }
            run_with_secrets(
                &ctx2,
                &format!("Grow — {name}"),
                args,
                Needs::for_existing_container(&name),
            );
        },
    );
}

/// Collect whatever `needs` requires, then stream the operation into a console.
fn run_with_secrets(ctx: &Ctx, title: &str, args: Vec<String>, needs: Needs) {
    let ctx2 = ctx.clone();
    let title = title.to_string();
    let dialog_title = title.clone();
    collect(
        ctx,
        &dialog_title,
        needs,
        Rc::new(move |secrets: Secrets| {
            let mut args = args.clone();
            args.push("--encrypt-secrets-stdin".into());
            let ctx3 = ctx2.clone();
            op::run_operation_with_stdin(
                &ctx2.window,
                &title,
                args,
                secrets.payload(),
                move |ok| {
                    ctx3.status(if ok { "Done." } else { "Failed — see the console." });
                    ctx3.refresh();
                },
            );
        }),
    );
}

// ── The master password store ─────────────────────────────────────────────────

/// Create the store, or change its password when one exists.
pub fn master_password(ctx: &Ctx) {
    let exists = crate::secrets::exists();
    let win = gtk::Window::builder()
        .title(if exists { "Change the master password" } else { "Set a master password" })
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(420)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 4);
    form.set_margin_top(10);
    form.set_margin_bottom(10);
    form.set_margin_start(10);
    form.set_margin_end(10);

    let current = exists.then(|| {
        secret_entry(&form, "Current master password", Some("Proves you may re-key the store."))
    });
    let first = secret_entry(
        &form,
        "New master password",
        Some("You'll type this once per boot to unlock stored passwords."),
    );
    let second = secret_entry(&form, "Repeat the new password", None);

    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("fill-critical");
    form.append(&error);
    outer.append(&form);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(6);
    bar.set_margin_end(6);
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label(if exists { "Change" } else { "Create" });
    ok.add_css_class("suggested-action");
    bar.append(&spacer);
    bar.append(&cancel);
    bar.append(&ok);
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
        ok.connect_clicked(move |_| {
            let new = first.text().to_string();
            if new.is_empty() {
                error.set_text("The master password must not be empty.");
                return;
            }
            if new != second.text() {
                error.set_text("The passwords did not match.");
                return;
            }
            let result = match &current {
                Some(entry) => crate::secrets::change_master(&entry.text(), &new),
                None => crate::secrets::init(&new),
            };
            match result {
                Ok(()) => {
                    ctx.status(if current.is_some() {
                        "Master password changed."
                    } else {
                        "Master password store created."
                    });
                    win.close();
                }
                Err(e) => error.set_text(&format!("{e:#}")),
            }
        });
    }
}

/// Reveal the stored container passwords, asking for the master password if the
/// store isn't already unlocked this boot.
///
/// A generated password is never shown when it is created, so this is the only
/// way to recover one.
pub fn show_stored_passwords(ctx: &Ctx) {
    match crate::secrets::open_cached() {
        Ok(Some(store)) => present_passwords(ctx, &store),
        Ok(None) => {
            let ctx2 = ctx.clone();
            collect(
                ctx,
                "Stored passwords",
                Needs { master_existing: true, ..Default::default() },
                Rc::new(move |secrets: Secrets| match crate::secrets::open(&secrets.master) {
                    Ok(store) => present_passwords(&ctx2, &store),
                    Err(e) => ctx2.status(&format!("{e:#}")),
                }),
            );
        }
        Err(e) => ctx.status(&format!("{e:#}")),
    }
}

fn present_passwords(ctx: &Ctx, store: &crate::secrets::Store) {
    let apps = store.apps();
    let body = if apps.is_empty() {
        "No container passwords are stored.".to_string()
    } else {
        apps.iter()
            .map(|a| format!("{a}\n    {}", store.get(a).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let win = gtk::Window::builder()
        .title("Stored container passwords")
        .transient_for(&ctx.window)
        .modal(true)
        .default_width(460)
        .default_height(320)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.set_margin_top(10);
    outer.set_margin_bottom(10);
    outer.set_margin_start(10);
    outer.set_margin_end(10);

    let text = gtk::TextView::new();
    text.set_editable(false);
    text.set_monospace(true);
    text.buffer().set_text(&body);
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&text));
    outer.append(&scroller);

    let close = gtk::Button::with_label("Close");
    outer.append(&close);
    win.set_child(Some(&outer));
    win.present();

    let win2 = win.clone();
    close.connect_clicked(move |_| win2.close());
}

/// Drop this boot's cached key, so the master password is needed again.
pub fn forget_master(ctx: &Ctx) {
    match crate::secrets::lock() {
        Ok(()) => ctx.status("Master password forgotten — it will be needed again."),
        Err(e) => ctx.status(&format!("{e:#}")),
    }
}

/// Delete the store outright: the way out of one nobody can open.
pub fn reset_store(ctx: &Ctx) {
    // The library refuses when apps still depend on it, so ask it first and put
    // that in the confirmation rather than discovering it after the click.
    let stranded = crate::commands::encrypt::apps_relying_on_the_store().unwrap_or_default();
    let body = if stranded.is_empty() {
        "The store and every password in it is deleted. Nothing else is affected."
            .to_string()
    } else {
        format!(
            "These apps open from the store and would be left unopenable: {}\n\n\
             Their container passwords live only here — deleting it deletes the \
             only copy. Show them first if you have not written them down.",
            stranded.join(", ")
        )
    };

    let ctx2 = ctx.clone();
    super::confirm(ctx, "Delete the master password store?", &body, true, move || {
        match crate::commands::encrypt::master_reset(true) {
            Ok(()) => ctx2.status("Master password store deleted."),
            Err(e) => ctx2.status(&format!("{e:#}")),
        }
    });
}

/// A one-line summary of the store for the Settings tab.
pub fn store_summary() -> String {
    if !crate::secrets::exists() {
        return "not set up".to_string();
    }
    if crate::secrets::is_unlocked() {
        "set · unlocked this boot".to_string()
    } else {
        "set · locked".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_the_key_value_form_the_child_parses() {
        let s = Secrets {
            sudo: "s".into(),
            master: "m".into(),
            container: "c".into(),
        };
        assert_eq!(s.payload(), "sudo=s\nmaster=m\ncontainer=c\n");
    }

    #[test]
    fn empty_answers_are_omitted_rather_than_sent_blank() {
        // The child splits on the first '=' and takes the rest verbatim, so a
        // "master=" line would set the master password to the empty string
        // instead of leaving it unset.
        let s = Secrets { container: "only-this".into(), ..Default::default() };
        assert_eq!(s.payload(), "container=only-this\n");
    }

    #[test]
    fn a_password_containing_an_equals_sign_survives() {
        // split_once('=') takes the first separator, so everything after it —
        // including further '=' — is the value.
        let s = Secrets { container: "a=b=c".into(), ..Default::default() };
        assert_eq!(s.payload(), "container=a=b=c\n");
        let payload = s.payload();
        let (key, value) = payload.trim_end().split_once('=').unwrap();
        assert_eq!((key, value), ("container", "a=b=c"));
    }

    #[test]
    fn nothing_to_ask_is_recognised() {
        assert!(Needs::default().nothing());
        assert!(!Needs { sudo: true, ..Default::default() }.nothing());
        assert!(!Needs { container_existing: true, ..Default::default() }.nothing());
    }

    #[test]
    fn a_generated_password_is_never_asked_for() {
        let _home = crate::test_support::test_home();
        // Nothing to type and nothing to confirm — that is the whole point of
        // generating it.
        assert!(!Needs::for_new_container(true, true).container_new);
        assert!(Needs::for_new_container(true, false).container_new);
    }

    #[test]
    fn a_first_container_has_to_create_the_master_store() {
        let _home = crate::test_support::test_home();
        // Fresh sandbox: no store yet, so it must be created rather than opened.
        let needs = Needs::for_new_container(true, true);
        assert!(needs.master_new);
        assert!(!needs.master_existing);
    }

    #[test]
    fn a_prompt_source_container_never_touches_the_master_store() {
        let _home = crate::test_support::test_home();
        let needs = Needs::for_new_container(false, false);
        assert!(!needs.master_new && !needs.master_existing);
    }

    #[test]
    fn opening_a_container_asks_for_its_password_when_the_store_has_none() {
        let _home = crate::test_support::test_home();
        assert!(Needs::for_existing_container("vault").container_existing);
    }
}
