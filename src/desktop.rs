//! Host desktop integration: making a sandboxed app reachable the way a
//! natively installed one is.
//!
//! A PATH shortcut is enough for a shell, but not for a desktop. When another
//! application opens a link it does not run `firefox` — it asks the desktop
//! which application handles `x-scheme-handler/https`, and the answer comes
//! from `.desktop` entries in `/usr/share/applications` plus the user's
//! `mimeapps.list`. An app installed into a wryayer container ships its own
//! `.desktop` files, but they live inside the container where nothing on the
//! host will ever look at them, and their `Exec=` points at a path that only
//! exists inside the sandbox.
//!
//! So this module copies them out and rewrites them: `Exec=` and `TryExec=`
//! become the app's `/usr/bin` shortcut, `Icon=` becomes an absolute path into
//! the app's tree, and everything else — the name, the categories, and above
//! all the `MimeType=` list the package itself declares — is carried over
//! untouched. The result is an entry the desktop cannot tell apart from a
//! packaged one, so the app shows up in menus, in "Open with", and (once
//! [`set_default`] has run) as the handler that other applications hand links
//! to.
//!
//! Entries carry an `X-Wryayer-App=` key. It is what makes removal safe: only
//! entries stamped with the app being removed are deleted.

use crate::launcher;
use crate::manifest::{app_dir, read_manifest};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where desktop entries go unless `WRYAYER_DESKTOP_DIR` says otherwise.
pub const SYSTEM_DESKTOP_DIR: &str = "/usr/share/applications";

/// The key stamping an entry as ours, and naming the app that owns it.
const OWNER_KEY: &str = "X-Wryayer-App";

/// One registered desktop entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// The MIME types and URL schemes the entry declares it can open.
    pub mime_types: Vec<String>,
}

impl Entry {
    /// The entry's `.desktop` file name — what `mimeapps.list` refers to it by.
    pub fn id(&self) -> String {
        self.path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    /// Whether the entry claims any URL scheme, i.e. whether making it a
    /// default handler would let other applications open links with it.
    pub fn handles_links(&self) -> bool {
        self.mime_types.iter().any(|m| m.starts_with("x-scheme-handler/"))
    }
}

pub fn entries_dir() -> PathBuf {
    match std::env::var_os("WRYAYER_DESKTOP_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(SYSTEM_DESKTOP_DIR),
    }
}

// ── registering ──────────────────────────────────────────────────────────────

/// Publish host desktop entries for every `.desktop` file `app_name` ships that
/// belongs to one of its shortcuts.
///
/// Returns what was registered. An app with no desktop files of its own — a
/// command-line tool, most of them — registers nothing and that is not an
/// error.
pub fn install(app_name: &str) -> Result<Vec<Entry>> {
    let manifest = read_manifest(app_name)
        .with_context(|| format!("'{app_name}' is not installed"))?;

    // An alias owns its shortcuts but not its files: the tree, and therefore
    // the packaged desktop files, belong to the app it points at.
    let fs_root = manifest.app.alias_of.clone().unwrap_or_else(|| app_name.to_string());
    let tree = app_dir(&fs_root)?;

    let mut launchers = manifest.app.launchers.clone();
    if !manifest.app.main_binary.is_empty() && !launchers.contains(&manifest.app.main_binary) {
        launchers.push(manifest.app.main_binary.clone());
    }

    // Replacing rather than merging: a re-register after an update must not
    // leave behind an entry for a desktop file the new version dropped.
    remove(app_name)?;

    let mut registered = Vec::new();
    for (source, content) in packaged_entries(&tree) {
        let Some(binary) = exec_binary(&content) else { continue };
        if !launchers.contains(&binary) {
            continue;
        }
        // Point at where the shortcut really is — it may have fallen back to
        // ~/bin — and skip the entry entirely if no shortcut was ever made,
        // since the entry would only produce a "command not found".
        let Some(shortcut) = launcher::launcher_path(&binary) else { continue };

        let rewritten = rewrite(&content, &shortcut.to_string_lossy(), &tree, app_name);
        let stem = source.file_stem().unwrap_or_default().to_string_lossy();
        let path = entries_dir().join(format!("wryayer-{app_name}-{stem}.desktop"));
        write_entry(&path, &rewritten)
            .with_context(|| format!("failed to write desktop entry {}", path.display()))?;
        registered.push(Entry { mime_types: mime_types(&rewritten), path });
    }

    if !registered.is_empty() {
        refresh_database();
    }
    Ok(registered)
}

/// Delete every host desktop entry belonging to `app_name`.
pub fn remove(app_name: &str) -> Result<()> {
    let mut removed = false;
    for (path, content) in our_entries() {
        if owner(&content).as_deref() == Some(app_name) {
            remove_entry(&path)
                .with_context(|| format!("failed to remove desktop entry {}", path.display()))?;
            removed = true;
        }
    }
    if removed {
        refresh_database();
    }
    Ok(())
}

/// The entries currently registered for `app_name`.
pub fn installed(app_name: &str) -> Vec<Entry> {
    our_entries()
        .into_iter()
        .filter(|(_, content)| owner(content).as_deref() == Some(app_name))
        .map(|(path, content)| Entry { mime_types: mime_types(&content), path })
        .collect()
}

// ── default handlers ─────────────────────────────────────────────────────────

/// Make `app_name` the default handler for every MIME type and URL scheme its
/// registered entries declare, and report what it now handles.
///
/// This is per-user state (`mimeapps.list`), which is where the desktop
/// specification puts it — a default browser is a preference, not a property of
/// the installed system.
pub fn set_default(app_name: &str) -> Result<Vec<String>> {
    let entries = installed(app_name);
    if entries.is_empty() {
        anyhow::bail!(
            "'{app_name}' has no registered desktop entries — nothing to hand links to.\n       \
             Only apps that ship a .desktop file declaring MimeType= can be a default handler."
        );
    }

    let mut assignments: Vec<(String, String)> = Vec::new();
    for entry in &entries {
        let id = entry.id();
        for mime in &entry.mime_types {
            if !assignments.iter().any(|(m, _)| m == mime) {
                assignments.push((mime.clone(), id.clone()));
            }
        }
    }
    if assignments.is_empty() {
        anyhow::bail!("'{app_name}' declares no MIME types, so it cannot be a default handler");
    }

    let path = mimeapps_path()?;
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = with_defaults(&current, &assignments);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, updated)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(assignments.into_iter().map(|(mime, _)| mime).collect())
}

fn mimeapps_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("mimeapps.list"));
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/mimeapps.list"))
}

/// Set `assignments` in the `[Default Applications]` group of a `mimeapps.list`,
/// leaving every other group, comment and ordering intact.
fn with_defaults(current: &str, assignments: &[(String, String)]) -> String {
    const GROUP: &str = "[Default Applications]";

    let mut out: Vec<String> = Vec::new();
    let mut in_group = false;
    let mut seen_group = false;
    let mut written = Vec::new();

    for line in current.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Leaving the group: everything not already overwritten goes in at
            // the end of it, so the keys stay together.
            if in_group {
                for (mime, id) in assignments {
                    if !written.contains(mime) {
                        out.push(format!("{mime}={id}"));
                        written.push(mime.clone());
                    }
                }
            }
            in_group = trimmed == GROUP;
            seen_group |= in_group;
            out.push(line.to_string());
            continue;
        }
        if in_group {
            if let Some((key, _)) = trimmed.split_once('=') {
                if let Some((mime, id)) = assignments.iter().find(|(m, _)| m == key.trim()) {
                    out.push(format!("{mime}={id}"));
                    written.push(mime.clone());
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }

    if in_group || !seen_group {
        if !seen_group {
            if !out.is_empty() && !out.last().is_some_and(|l| l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push(GROUP.to_string());
        }
        for (mime, id) in assignments {
            if !written.contains(mime) {
                out.push(format!("{mime}={id}"));
            }
        }
    }

    let mut text = out.join("\n");
    text.push('\n');
    text
}

// ── reading and rewriting entries ────────────────────────────────────────────

/// The `.desktop` files an app ships, as (path, contents).
fn packaged_entries(tree: &Path) -> Vec<(PathBuf, String)> {
    let dir = tree.join("usr/share/applications");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut found: Vec<(PathBuf, String)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("desktop"))
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|c| (p, c)))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Every entry in the host directory that wryayer wrote.
fn our_entries() -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(entries_dir()) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("desktop"))
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|c| (p, c)))
        .filter(|(_, c)| c.contains(&format!("{OWNER_KEY}=")))
        .collect()
}

fn owner(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|l| l.trim().strip_prefix(&format!("{OWNER_KEY}=")))
        .map(|v| v.trim().to_string())
}

fn mime_types(content: &str) -> Vec<String> {
    content
        .lines()
        .find_map(|l| l.trim().strip_prefix("MimeType="))
        .map(|v| v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// The program a desktop entry runs, as a bare binary name.
fn exec_binary(content: &str) -> Option<String> {
    // TryExec is the spec's canonical pointer at the real binary; Exec often
    // wraps it in `env`, a shell, or a vendor script.
    for key in ["TryExec=", "Exec="] {
        for line in content.lines() {
            let Some(value) = line.trim().strip_prefix(key) else { continue };
            if let Some(program) = exec_program(value) {
                return Path::new(&program)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// The program token of an `Exec=` value, skipping an `env VAR=value …` prefix.
fn exec_program(value: &str) -> Option<String> {
    let mut tokens = value.split_whitespace();
    let mut program = tokens.next()?;
    if Path::new(program).file_name().is_some_and(|n| n == "env") {
        for token in tokens {
            if !token.contains('=') {
                program = token;
                break;
            }
        }
    }
    (!program.is_empty() && !program.contains('=')).then(|| program.to_string())
}

/// Rewrite a packaged entry so the host can act on it.
fn rewrite(content: &str, shortcut: &str, tree: &Path, app_name: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let declares_mime = content.lines().any(|l| l.trim().starts_with("MimeType="));

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Exec=") {
            out.push(format!("Exec={}", rewrite_exec(value, shortcut, declares_mime)));
        } else if trimmed.starts_with("TryExec=") {
            out.push(format!("TryExec={shortcut}"));
        } else if let Some(value) = trimmed.strip_prefix("Icon=") {
            out.push(format!("Icon={}", resolve_icon(value.trim(), tree)));
        } else if trimmed.starts_with("DBusActivatable=") {
            // The host has no service file for the sandboxed app, so leaving
            // this true makes launchers try an activation that always fails.
            out.push("DBusActivatable=false".to_string());
        } else {
            out.push(line.to_string());
        }

        // Stamp ownership into the main group, right where it is easy to see.
        if trimmed == "[Desktop Entry]" {
            out.push(format!("{OWNER_KEY}={app_name}"));
        }
    }

    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Replace the program in an `Exec=` value, keeping its arguments and field
/// codes.
///
/// An entry that declares MIME types but carries no field code has no way to
/// receive the file or URL it was chosen for, so one is added.
fn rewrite_exec(value: &str, shortcut: &str, declares_mime: bool) -> String {
    let mut tokens: Vec<&str> = value.split_whitespace().collect();

    // Drop the `env VAR=value …` prefix along with the program it wraps: the
    // shortcut sets up the sandbox's environment itself, and host variables
    // would not survive into it anyway.
    let mut start = 0;
    if tokens.first().is_some_and(|t| Path::new(t).file_name().is_some_and(|n| n == "env")) {
        start = 1;
        while tokens.get(start).is_some_and(|t| t.contains('=')) {
            start += 1;
        }
    }
    if start < tokens.len() {
        start += 1; // the program itself
    }
    tokens.drain(..start.min(tokens.len()));

    let has_field_code = tokens.iter().any(|t| matches!(*t, "%f" | "%F" | "%u" | "%U"));
    let mut out = vec![shortcut.to_string()];
    out.extend(tokens.iter().map(|t| t.to_string()));
    if declares_mime && !has_field_code {
        out.push("%U".to_string());
    }
    out.join(" ")
}

/// Turn an icon name into an absolute path inside the app's tree.
///
/// Icon themes are looked up in host directories, and the app's icons are not
/// in any of them; an absolute path is what the spec offers instead. A name
/// that cannot be resolved is left as-is — the host theme may well have it.
fn resolve_icon(name: &str, tree: &Path) -> String {
    if name.is_empty() || name.starts_with('/') {
        return name.to_string();
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for ext in ["png", "svg", "xpm"] {
        candidates.push(tree.join(format!("usr/share/pixmaps/{name}.{ext}")));
    }
    // Prefer the largest raster size available, then scalable.
    for size in ["scalable", "512x512", "256x256", "128x128", "64x64", "48x48", "32x32"] {
        for ext in ["svg", "png"] {
            candidates.push(tree.join(format!(
                "usr/share/icons/hicolor/{size}/apps/{name}.{ext}"
            )));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| name.to_string())
}

// ── privileged file operations ───────────────────────────────────────────────

fn write_entry(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(path, content) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            crate::launcher::sudo_write(path, content, 0o644)
        }
        Err(e) => Err(e.into()),
    }
}

fn remove_entry(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            crate::launcher::sudo_remove(path)
        }
        Err(e) => Err(e.into()),
    }
}

/// Rebuild the MIME cache so the new entries are found without a re-login.
///
/// Best-effort: desktops rescan on their own eventually, and the entry is
/// already usable from a file manager's "Open with" either way.
fn refresh_database() {
    let dir = entries_dir();
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIREFOX: &str = "\
[Desktop Entry]
Version=1.0
Name=Firefox
Exec=/usr/lib/firefox/firefox %u
TryExec=firefox
Icon=firefox
Terminal=false
Type=Application
MimeType=text/html;x-scheme-handler/http;x-scheme-handler/https;
Categories=Network;WebBrowser;
DBusActivatable=true

[Desktop Action new-window]
Name=New Window
Exec=/usr/lib/firefox/firefox --new-window %u
";

    #[test]
    fn exec_binary_prefers_tryexec() {
        assert_eq!(exec_binary(FIREFOX).as_deref(), Some("firefox"));
    }

    #[test]
    fn exec_binary_sees_through_an_env_wrapper() {
        let entry = "[Desktop Entry]\nExec=env GDK_BACKEND=x11 /usr/bin/code --unity-launch %F\n";
        assert_eq!(exec_binary(entry).as_deref(), Some("code"));
    }

    #[test]
    fn exec_binary_of_an_entry_without_one_is_none() {
        assert_eq!(exec_binary("[Desktop Entry]\nName=Nothing\n"), None);
    }

    #[test]
    fn rewrite_points_exec_at_the_shortcut_and_keeps_the_arguments() {
        let out = rewrite(FIREFOX, "/usr/bin/firefox", Path::new("/nonexistent"), "firefox");
        assert!(out.contains("Exec=/usr/bin/firefox %u"), "{out}");
        assert!(out.contains("TryExec=/usr/bin/firefox"), "{out}");
        assert!(out.contains("Exec=/usr/bin/firefox --new-window %u"), "{out}");
    }

    #[test]
    fn rewrite_keeps_the_declared_mime_types() {
        let out = rewrite(FIREFOX, "/usr/bin/firefox", Path::new("/nonexistent"), "firefox");
        assert_eq!(
            mime_types(&out),
            ["text/html", "x-scheme-handler/http", "x-scheme-handler/https"]
        );
    }

    #[test]
    fn rewrite_stamps_the_owning_app() {
        let out = rewrite(FIREFOX, "/usr/bin/firefox", Path::new("/nonexistent"), "firefox");
        assert_eq!(owner(&out).as_deref(), Some("firefox"));
        // Directly under the group header, so it cannot land in an action group.
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "[Desktop Entry]");
        assert_eq!(lines[1], "X-Wryayer-App=firefox");
    }

    #[test]
    fn rewrite_disables_dbus_activation() {
        let out = rewrite(FIREFOX, "/usr/bin/firefox", Path::new("/nonexistent"), "firefox");
        assert!(out.contains("DBusActivatable=false"), "{out}");
        assert!(!out.contains("DBusActivatable=true"), "{out}");
    }

    #[test]
    fn rewrite_drops_an_env_wrapper() {
        let entry = "[Desktop Entry]\nExec=env LC_ALL=C /usr/bin/code --unity-launch %F\n";
        let out = rewrite(entry, "/usr/bin/code", Path::new("/nonexistent"), "code");
        assert!(out.contains("Exec=/usr/bin/code --unity-launch %F"), "{out}");
    }

    #[test]
    fn rewrite_adds_a_field_code_when_mime_types_need_one() {
        let entry = "[Desktop Entry]\nExec=/usr/bin/thing\nMimeType=x-scheme-handler/irc;\n";
        let out = rewrite(entry, "/usr/bin/thing", Path::new("/nonexistent"), "thing");
        assert!(out.contains("Exec=/usr/bin/thing %U"), "{out}");
    }

    #[test]
    fn rewrite_leaves_a_plain_entry_without_a_field_code() {
        let entry = "[Desktop Entry]\nExec=/usr/bin/thing --flag\n";
        let out = rewrite(entry, "/usr/bin/thing", Path::new("/nonexistent"), "thing");
        assert!(out.contains("Exec=/usr/bin/thing --flag\n"), "{out}");
        assert!(!out.contains("%U"), "{out}");
    }

    #[test]
    fn icons_resolve_to_an_absolute_path_in_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let icons = tmp.path().join("usr/share/icons/hicolor/256x256/apps");
        std::fs::create_dir_all(&icons).unwrap();
        std::fs::write(icons.join("firefox.png"), b"").unwrap();

        assert_eq!(
            resolve_icon("firefox", tmp.path()),
            icons.join("firefox.png").display().to_string()
        );
    }

    #[test]
    fn unresolvable_icons_stay_as_a_theme_name() {
        assert_eq!(resolve_icon("firefox", Path::new("/nonexistent")), "firefox");
        assert_eq!(resolve_icon("/a/b.png", Path::new("/nonexistent")), "/a/b.png");
    }

    #[test]
    fn entry_reports_whether_it_handles_links() {
        let with = Entry {
            path: "/x/a.desktop".into(),
            mime_types: vec!["x-scheme-handler/https".into()],
        };
        let without =
            Entry { path: "/x/b.desktop".into(), mime_types: vec!["image/png".into()] };
        assert!(with.handles_links());
        assert!(!without.handles_links());
    }

    // ── mimeapps.list editing ────────────────────────────────────────────────

    fn assignments() -> Vec<(String, String)> {
        vec![
            ("x-scheme-handler/https".to_string(), "wryayer-firefox-firefox.desktop".to_string()),
            ("text/html".to_string(), "wryayer-firefox-firefox.desktop".to_string()),
        ]
    }

    #[test]
    fn defaults_are_added_to_an_empty_file() {
        let out = with_defaults("", &assignments());
        assert!(out.contains("[Default Applications]"));
        assert!(out.contains("x-scheme-handler/https=wryayer-firefox-firefox.desktop"));
        assert!(out.contains("text/html=wryayer-firefox-firefox.desktop"));
    }

    #[test]
    fn defaults_replace_an_existing_handler() {
        let current = "[Default Applications]\nx-scheme-handler/https=chromium.desktop\n";
        let out = with_defaults(current, &assignments());
        assert!(!out.contains("chromium.desktop"), "{out}");
        assert_eq!(out.matches("x-scheme-handler/https=").count(), 1, "{out}");
    }

    #[test]
    fn other_groups_and_keys_survive() {
        let current = "\
[Added Associations]
image/png=gimp.desktop

[Default Applications]
image/png=eog.desktop
";
        let out = with_defaults(current, &assignments());
        assert!(out.contains("[Added Associations]"), "{out}");
        assert!(out.contains("image/png=gimp.desktop"), "{out}");
        assert!(out.contains("image/png=eog.desktop"), "{out}");
        assert!(out.contains("x-scheme-handler/https=wryayer-firefox-firefox.desktop"), "{out}");
    }

    #[test]
    fn new_keys_stay_inside_their_group() {
        let current = "\
[Default Applications]
image/png=eog.desktop

[Added Associations]
image/png=gimp.desktop
";
        let out = with_defaults(current, &assignments());
        let lines: Vec<&str> = out.lines().collect();
        let group = lines.iter().position(|l| *l == "[Default Applications]").unwrap();
        let next = lines.iter().position(|l| *l == "[Added Associations]").unwrap();
        let added = lines.iter().position(|l| l.starts_with("text/html=")).unwrap();
        assert!(group < added && added < next, "{out}");
    }

    #[test]
    fn editing_is_idempotent() {
        let once = with_defaults("", &assignments());
        let twice = with_defaults(&once, &assignments());
        assert_eq!(once, twice);
    }

    // ── end to end, against a sandboxed HOME ─────────────────────────────────

    use crate::manifest::{write_manifest, AppMeta, Manifest};

    /// Install a fake app that ships one desktop file, plus its shortcut.
    fn fake_app(name: &str, desktop_file: Option<&str>) {
        let dir = app_dir(name).unwrap();
        std::fs::create_dir_all(dir.join("usr/bin")).unwrap();
        std::fs::write(dir.join("usr/bin").join(name), b"#!/bin/sh\n").unwrap();
        if let Some(content) = desktop_file {
            let apps = dir.join("usr/share/applications");
            std::fs::create_dir_all(&apps).unwrap();
            std::fs::write(apps.join(format!("{name}.desktop")), content).unwrap();
        }
        write_manifest(
            name,
            &Manifest {
                app: AppMeta {
                    name: name.to_string(),
                    main_binary: name.to_string(),
                    installed_at: "2026-08-01T00:00:00Z".to_string(),
                    launchers: vec![name.to_string()],
                    alias_of: None,
                    display_name: None,
                    pkg_name: None,
                    wine_game: None,
                },
                packages: Vec::new(),
            },
        )
        .unwrap();
        crate::launcher::create_launcher(name, name).unwrap();
    }

    #[test]
    fn installing_publishes_an_entry_pointing_at_the_shortcut() {
        let _home = crate::test_support::test_home();
        fake_app("firefox", Some(FIREFOX));

        let entries = install("firefox").unwrap();
        assert_eq!(entries.len(), 1);

        let entry = &entries[0];
        assert_eq!(entry.id(), "wryayer-firefox-firefox.desktop");
        assert!(entry.handles_links());

        let written = std::fs::read_to_string(&entry.path).unwrap();
        let shortcut = crate::launcher::launcher_path("firefox").unwrap();
        assert!(written.contains(&format!("Exec={} %u", shortcut.display())), "{written}");
    }

    #[test]
    fn an_app_without_desktop_files_registers_nothing() {
        let _home = crate::test_support::test_home();
        fake_app("htop", None);
        assert!(install("htop").unwrap().is_empty());
    }

    #[test]
    fn an_entry_for_another_binary_is_not_claimed() {
        let _home = crate::test_support::test_home();
        // A merged tree carries desktop files for packages this app does not
        // own; adopting them would put someone else's launcher in the menu
        // under our name.
        fake_app("firefox", Some("[Desktop Entry]\nExec=/usr/bin/chromium %U\n"));
        assert!(install("firefox").unwrap().is_empty());
    }

    #[test]
    fn reinstalling_replaces_rather_than_accumulates() {
        let _home = crate::test_support::test_home();
        fake_app("firefox", Some(FIREFOX));

        install("firefox").unwrap();
        install("firefox").unwrap();
        assert_eq!(installed("firefox").len(), 1);
    }

    #[test]
    fn removing_takes_only_the_apps_own_entries() {
        let _home = crate::test_support::test_home();
        fake_app("firefox", Some(FIREFOX));
        fake_app("thing", Some("[Desktop Entry]\nExec=thing\nMimeType=text/plain;\n"));
        install("firefox").unwrap();
        install("thing").unwrap();

        // A hand-written entry sharing the directory must survive.
        let foreign = entries_dir().join("chromium.desktop");
        std::fs::write(&foreign, "[Desktop Entry]\nExec=chromium\n").unwrap();

        remove("firefox").unwrap();
        assert!(installed("firefox").is_empty());
        assert_eq!(installed("thing").len(), 1);
        assert!(foreign.exists(), "an entry wryayer did not write must be left alone");
    }

    #[test]
    fn set_default_claims_every_declared_type() {
        let _home = crate::test_support::test_home();
        fake_app("firefox", Some(FIREFOX));
        install("firefox").unwrap();

        let handled = set_default("firefox").unwrap();
        assert_eq!(handled, ["text/html", "x-scheme-handler/http", "x-scheme-handler/https"]);

        let list = std::fs::read_to_string(mimeapps_path().unwrap()).unwrap();
        assert!(
            list.contains("x-scheme-handler/https=wryayer-firefox-firefox.desktop"),
            "{list}"
        );
    }

    #[test]
    fn set_default_without_a_registered_entry_explains_itself() {
        let _home = crate::test_support::test_home();
        fake_app("htop", None);

        let err = set_default("htop").unwrap_err().to_string();
        assert!(err.contains("no registered desktop entries"), "{err}");
    }
}
