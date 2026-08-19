//! Exporting the shape of a wryayer installation, and rebuilding it elsewhere.
//!
//! `wryayer export` packs an app's *files* — the extracted tree, its sandbox
//! home, its snapshots — which is what moving one app between two machines
//! running the same distribution asks for. It is the wrong thing entirely for
//! "set my new laptop up like the old one": those packages come from a
//! different package manager, in a different format, and have to be fetched
//! fresh on the other side.
//!
//! So this exports the *list* instead: which apps exist, which package each
//! came from, how it was installed — its launchers, whether it was merged into
//! another app's tree — and the settings it runs under. The result is a small
//! TOML file meant to be read and edited by hand, because a package name is the
//! one thing that does not travel: only the user knows what `firefox` is called
//! on the other distribution, or whether it exists there at all.
//!
//! Nothing about a container is exported — not a password, not a size. An
//! encrypted app is recorded as having been encrypted, and the import says so,
//! because re-creating that is a decision with a password attached.

use crate::config::{config_path, parse_ini, write_config};
use crate::manifest::{app_dir, list_all_apps, read_manifest_or_marker, tree_order};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The format version written into every export.
///
/// Bumped when a change would make an older wryayer misread a newer file. The
/// importer refuses anything above what it knows rather than guessing at it.
pub const FORMAT_VERSION: u32 = 1;

/// A whole installation, as a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub version: u32,
    /// When it was written, for the reader's benefit only.
    #[serde(default)]
    pub exported_at: String,
    /// The package manager the names below came from — `arch`, `debian` or
    /// `fedora`. Informational: the import always uses the local one.
    #[serde(default)]
    pub distro: String,
    /// The exporting user's home directory, so paths inside the settings can be
    /// re-pointed at the importing user's.
    #[serde(default)]
    pub home: String,
    #[serde(default, rename = "app")]
    pub apps: Vec<SetupApp>,
}

/// One app in a [`Setup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupApp {
    /// The directory name under `~/.wryayer`, i.e. what `wryayer list` shows
    /// and what the launcher is named after.
    pub name: String,
    /// The package to install. **This is the field to edit** when the other
    /// distribution spells it differently.
    pub package: String,
    /// The command names the app installed. Left out when they are simply the
    /// package's own default, so the importer can let the other distribution's
    /// package decide.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launchers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The app whose tree this one was merged into (`install --into`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub into: Option<String>,
    /// Whether it lived in a VeraCrypt container. Nothing about the container
    /// itself is exported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub encrypted: bool,
    /// Whether it is an imported Windows game. Its files *are* the game, so a
    /// list cannot bring it back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wine_game: bool,
    /// Whether the app installed no command at all — a library, a plugin, a
    /// data package. Recorded because an install that finds no launcher stops
    /// and asks unless it is told this is expected, and here it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_launcher: bool,
    /// The app's `config.ini`, verbatim. Empty when it had none of its own and
    /// simply ran on the global defaults.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub config: String,
}

/// What the importer would do about one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Install it, then apply its settings.
    Install(SetupApp),
    /// Already here — only the settings are applied.
    Configure(SetupApp),
    /// Nothing can be done automatically; the reason says why.
    Manual { app: SetupApp, reason: String },
}

impl Step {
    pub fn app(&self) -> &SetupApp {
        match self {
            Step::Install(app) | Step::Configure(app) => app,
            Step::Manual { app, .. } => app,
        }
    }
}

// ── Export ────────────────────────────────────────────────────────────────────

/// Write the installation to `output`, or to `./wryayer-setup-<date>.toml`.
/// Returns where it went.
pub fn export(output: Option<&Path>) -> Result<PathBuf> {
    let setup = collect()?;
    let path = match output {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(format!(
            "wryayer-setup-{}.toml",
            chrono::Local::now().format("%Y-%m-%d")
        )),
    };

    let body = toml::to_string_pretty(&setup).context("failed to serialize the setup")?;
    std::fs::write(&path, format!("{HEADER}{body}"))
        .with_context(|| format!("failed to write {}", path.display()))?;

    let games = setup.apps.iter().filter(|a| a.wine_game).count();
    eprintln!("Wrote {} app(s) to {}", setup.apps.len(), path.display());
    if games > 0 {
        eprintln!(
            "  {games} of them are Windows games — a list cannot rebuild those, \
             their files are the game."
        );
    }
    eprintln!("Recreate it elsewhere with: wryayer setup import {}", path.display());
    Ok(path)
}

/// The comment block every export opens with. It is the first thing the user
/// reads on the other machine, so it says what to edit before importing.
const HEADER: &str = "\
# wryayer setup — the apps installed on this machine, and how each is set up.
#
# Recreate them on another machine with:
#
#     wryayer setup import <this file>
#
# `package` is the one field that does not travel: package names differ between
# distributions, so edit any that the other side spells differently — or delete
# the app entirely if it has no equivalent there. Everything under `config` is
# distro-independent and is applied as it stands.
#
# Nothing here is a backup. It describes what to install, not what was
# installed: no files, no sandbox homes, no container passwords. Use
# `wryayer export <app>` for a copy of an app itself.

";

/// Read the installation into a [`Setup`].
pub fn collect() -> Result<Setup> {
    let manifests = tree_order(list_all_apps().context("failed to list installed apps")?);
    let home = std::env::var("HOME").unwrap_or_default();

    let mut apps = Vec::new();
    for manifest in &manifests {
        let name = manifest.app.name.clone();
        // A locked app's manifest lives inside its container; the marker left
        // outside it still carries the identity, which is all a list needs.
        let manifest = read_manifest_or_marker(&name).unwrap_or_else(|_| manifest.clone());

        apps.push(SetupApp {
            package: manifest.app.pkg_name.clone().unwrap_or_else(|| name.clone()),
            launchers: recorded_launchers(&manifest),
            display_name: manifest.app.display_name.clone(),
            into: manifest.app.alias_of.clone(),
            encrypted: crate::veracrypt::is_encrypted(&name),
            wine_game: manifest.app.wine_game.is_some(),
            no_launcher: manifest.app.launchers.is_empty(),
            config: app_config_text(&name),
            name,
        });
    }

    Ok(Setup {
        version: FORMAT_VERSION,
        exported_at: chrono::Utc::now().to_rfc3339(),
        distro: distro_name(),
        home,
        apps,
    })
}

fn distro_name() -> String {
    match crate::distro::current() {
        crate::distro::Distro::Arch => "arch",
        crate::distro::Distro::Debian => "debian",
        crate::distro::Distro::Fedora => "fedora",
    }
    .to_string()
}

/// An app's settings, or empty when it has none of its own — such an app runs
/// on the global defaults, and recording those as if they were its own would
/// freeze this machine's defaults into the export.
fn app_config_text(app_name: &str) -> String {
    let Ok(path) = config_path(app_name) else { return String::new() };
    condense_ini(&std::fs::read_to_string(path).unwrap_or_default())
}

/// Keep the settings and drop the manual.
///
/// A written `config.ini` is mostly comments — every option explained where it
/// is set — which is right for a file you edit by hand and wrong for one app
/// among thirty in an export. The section headers stay: they are what makes the
/// remaining thirty lines readable.
pub fn condense_ini(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The launchers worth recording: the ones the user must have chosen.
///
/// A single launcher named after the package is what any install produces by
/// itself, and the other distribution's package may well install a differently
/// named binary — so recording that would turn a working install into a
/// "binary not found". Anything else was a decision (`--bin-name`,
/// `--bin-names`, a merge alias) and has to be carried over.
fn recorded_launchers(manifest: &crate::manifest::Manifest) -> Vec<String> {
    let package = manifest.app.pkg_name.clone().unwrap_or_else(|| manifest.app.name.clone());
    let launchers = &manifest.app.launchers;
    if launchers.len() == 1 && launchers[0] == package {
        return Vec::new();
    }
    launchers.clone()
}

// ── Import ────────────────────────────────────────────────────────────────────

/// Install what `path` lists and apply the settings it records.
pub fn import(path: &Path, dry_run: bool) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let setup = parse(&text)?;

    let installed: Vec<String> = list_all_apps()
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.app.name)
        .collect();
    let steps = plan(&setup, &installed);
    if steps.is_empty() {
        eprintln!("{} lists no apps.", path.display());
        return Ok(());
    }

    let home = std::env::var("HOME").unwrap_or_default();
    if dry_run {
        eprintln!("Would apply {} (from {}):\n", path.display(), setup.distro);
        for step in &steps {
            eprintln!("  {}", describe(step));
        }
        eprintln!("\nRun without --dry-run to do it.");
        return Ok(());
    }

    let mut failed: Vec<(String, String)> = Vec::new();
    let mut manual: Vec<(String, String)> = Vec::new();
    let mut installed_count = 0usize;
    let mut configured = 0usize;

    for step in &steps {
        eprintln!("\n── {}", describe(step));
        let app = step.app();
        match step {
            Step::Manual { reason, .. } => {
                manual.push((app.name.clone(), reason.clone()));
                continue;
            }
            Step::Install(_) => {
                if let Err(e) = install_one(app) {
                    eprintln!("  failed: {e:#}");
                    failed.push((app.name.clone(), format!("{e:#}")));
                    continue;
                }
                installed_count += 1;
            }
            Step::Configure(_) => {}
        }

        match apply_config(app, &setup.home, &home) {
            Ok(true) => configured += 1,
            Ok(false) => {}
            Err(e) => eprintln!("  installed, but its settings could not be applied: {e:#}"),
        }
        if app.encrypted {
            eprintln!(
                "  it was encrypted on the other machine — put it back in a container with: \
                 wryayer encrypt {}",
                app.name
            );
        }
    }

    eprintln!(
        "\nInstalled {installed_count}, configured {configured}, \
         skipped {} that need doing by hand, {} failed.",
        manual.len(),
        failed.len()
    );
    for (name, reason) in &manual {
        eprintln!("  {name}: {reason}");
    }
    for (name, reason) in &failed {
        eprintln!("  {name}: {reason}");
    }
    if !failed.is_empty() {
        bail!(
            "{} of {} apps could not be installed — a package name that differs on this \
             distribution is the usual reason; edit it in {} and run this again",
            failed.len(),
            steps.len(),
            path.display()
        );
    }
    Ok(())
}

/// Parse an export, refusing one written by a newer wryayer.
pub fn parse(text: &str) -> Result<Setup> {
    let setup: Setup = toml::from_str(text).context("failed to parse the setup file")?;
    if setup.version > FORMAT_VERSION {
        bail!(
            "this file is version {} and this wryayer understands up to {FORMAT_VERSION} — \
             update wryayer, or edit the version down if you know the difference does not \
             matter",
            setup.version
        );
    }
    Ok(setup)
}

/// What the importer would do, app by app.
///
/// `installed` is what is already here. The order the file lists apps in is
/// kept, except that an app merging into another is moved after its target —
/// the export writes them that way, but the file is meant to be edited.
pub fn plan(setup: &Setup, installed: &[String]) -> Vec<Step> {
    let ordered = targets_first(&setup.apps);
    let mut done: Vec<&str> = installed.iter().map(String::as_str).collect();

    let mut steps = Vec::new();
    for app in ordered {
        let step = if app.wine_game {
            Step::Manual {
                reason: "a Windows game — its files are the game itself, so re-import the \
                         folder with `wryayer install-game`"
                    .to_string(),
                app: app.clone(),
            }
        } else if app.package.trim().is_empty() {
            Step::Manual {
                reason: "no package name — fill it in, or delete the entry".to_string(),
                app: app.clone(),
            }
        } else if installed.iter().any(|n| n == &app.name) {
            // Already here: where its files came from no longer matters, only
            // the settings do.
            Step::Configure(app.clone())
        } else if let Some(target) = app.into.as_deref().filter(|t| !done.contains(t)) {
            Step::Manual {
                reason: format!(
                    "merges into '{target}', which is neither installed nor listed here"
                ),
                app: app.clone(),
            }
        } else {
            Step::Install(app.clone())
        };

        if !matches!(step, Step::Manual { .. }) {
            done.push(&app.name);
        }
        steps.push(step);
    }
    steps
}

/// Order the apps so a merge target comes before anything merging into it,
/// leaving the given order alone otherwise.
fn targets_first(apps: &[SetupApp]) -> Vec<&SetupApp> {
    let mut roots: Vec<&SetupApp> = Vec::new();
    let mut merges: Vec<&SetupApp> = Vec::new();
    for app in apps {
        match app.into {
            Some(_) => merges.push(app),
            None => roots.push(app),
        }
    }
    // Each merge follows its target when that target is in this file; the rest
    // keep their order at the end, where `plan` will call them out.
    let mut out: Vec<&SetupApp> = Vec::new();
    for root in roots {
        out.push(root);
        for merge in merges.iter().filter(|m| m.into.as_deref() == Some(root.name.as_str())) {
            out.push(merge);
        }
    }
    for merge in merges {
        if !out.iter().any(|a| a.name == merge.name) {
            out.push(merge);
        }
    }
    out
}

/// A one-line description of a step, for the log and for `--dry-run`.
pub fn describe(step: &Step) -> String {
    let app = step.app();
    match step {
        Step::Install(_) => {
            let into = app.into.as_deref().map(|t| format!(" into {t}")).unwrap_or_default();
            let named = if app.name == app.package {
                String::new()
            } else {
                format!(" as {}", app.name)
            };
            format!("install {}{named}{into}", app.package)
        }
        Step::Configure(_) => format!("{}: already installed — applying its settings", app.name),
        Step::Manual { reason, .. } => format!("{}: skipped — {reason}", app.name),
    }
}

fn install_one(app: &SetupApp) -> Result<()> {
    let app_name = (app.name != app.package).then_some(app.name.as_str());
    crate::commands::install::run(
        &app.package,
        app_name,
        &app.launchers,
        app.into.as_deref(),
        // Only for an app that had no command of its own: the same flag also
        // suppresses launcher creation, and a restored setup is meant to come
        // back with its shortcuts.
        app.no_launcher,
        false,
        crate::commands::install::EncryptOpts::default(),
    )
}

/// Write the app's recorded settings, with paths re-pointed at this user's
/// home. Returns whether there was anything to write.
fn apply_config(app: &SetupApp, old_home: &str, new_home: &str) -> Result<bool> {
    if app.config.trim().is_empty() {
        return Ok(false);
    }
    // Only touch an app that is actually here: a failed install must not leave
    // a config.ini in a directory nothing owns.
    if !app_dir(&app.name).map(|d| d.exists()).unwrap_or(false) {
        return Ok(false);
    }
    let text = rewrite_home(&app.config, old_home, new_home);
    let config = parse_ini(&text).context("failed to parse the recorded settings")?;
    write_config(&app.name, &config)?;

    let display = app.display_name.clone();
    if let Some(display) = display {
        if let Ok(mut manifest) = crate::manifest::read_manifest(&app.name) {
            manifest.app.display_name = Some(display);
            let _ = crate::manifest::write_manifest(&app.name, &manifest);
        }
    }
    Ok(true)
}

/// Re-point absolute paths at the importing user's home.
///
/// Shared directories are recorded as they were — `/home/alice/Downloads` — and
/// on another machine that path belongs to somebody else, or to nobody. Only
/// the home prefix is rewritten, so a shared directory elsewhere on the disk is
/// left exactly as it was.
pub fn rewrite_home(config: &str, old_home: &str, new_home: &str) -> String {
    if old_home.is_empty() || new_home.is_empty() || old_home == new_home {
        return config.to_string();
    }
    config.replace(old_home, new_home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> SetupApp {
        SetupApp {
            name: name.to_string(),
            package: name.to_string(),
            launchers: Vec::new(),
            display_name: None,
            into: None,
            encrypted: false,
            wine_game: false,
            no_launcher: false,
            config: String::new(),
        }
    }

    fn setup(apps: Vec<SetupApp>) -> Setup {
        Setup {
            version: FORMAT_VERSION,
            exported_at: "2026-08-19T00:00:00Z".to_string(),
            distro: "arch".to_string(),
            home: "/home/alice".to_string(),
            apps,
        }
    }

    // ── the file itself ──────────────────────────────────────────────────────

    #[test]
    fn an_export_round_trips_through_the_file() {
        let mut firefox = app("firefox");
        firefox.config = "[sandbox]\nnetwork = off\n".to_string();
        firefox.encrypted = true;
        let original = setup(vec![firefox]);

        let text = toml::to_string_pretty(&original).unwrap();
        assert_eq!(parse(&text).unwrap(), original);
    }

    #[test]
    fn a_file_from_a_newer_wryayer_is_refused_rather_than_guessed_at() {
        let mut newer = setup(vec![]);
        newer.version = FORMAT_VERSION + 1;
        let err = parse(&toml::to_string_pretty(&newer).unwrap()).unwrap_err().to_string();
        assert!(err.contains("understands up to"), "{err}");
    }

    #[test]
    fn a_hand_written_file_needs_only_a_version_and_a_package() {
        // What someone would write by hand, or cut down to.
        let text = "version = 1\n[[app]]\nname = \"vlc\"\npackage = \"vlc\"\n";
        let parsed = parse(text).unwrap();
        assert_eq!(parsed.apps.len(), 1);
        assert_eq!(parsed.apps[0].name, "vlc");
        assert!(parsed.apps[0].config.is_empty());
    }

    // ── planning ─────────────────────────────────────────────────────────────

    #[test]
    fn an_app_that_is_not_here_yet_is_installed() {
        let steps = plan(&setup(vec![app("firefox")]), &[]);
        assert!(matches!(steps[0], Step::Install(_)));
        assert_eq!(describe(&steps[0]), "install firefox");
    }

    #[test]
    fn an_app_that_is_already_here_only_gets_its_settings() {
        let steps = plan(&setup(vec![app("firefox")]), &["firefox".to_string()]);
        assert!(matches!(steps[0], Step::Configure(_)));
    }

    #[test]
    fn a_renamed_app_says_what_it_will_be_called() {
        let mut python = app("py312");
        python.package = "python".to_string();
        let steps = plan(&setup(vec![python]), &[]);
        assert_eq!(describe(&steps[0]), "install python as py312");
    }

    #[test]
    fn a_merge_is_planned_after_the_app_it_merges_into() {
        let mut rg = app("ripgrep");
        rg.into = Some("neovim".to_string());
        // Listed the wrong way round on purpose: the file is meant to be edited.
        let steps = plan(&setup(vec![rg, app("neovim")]), &[]);
        assert_eq!(steps[0].app().name, "neovim");
        assert_eq!(steps[1].app().name, "ripgrep");
        assert_eq!(describe(&steps[1]), "install ripgrep into neovim");
    }

    #[test]
    fn a_merge_into_an_app_nobody_has_is_left_to_the_user() {
        let mut rg = app("ripgrep");
        rg.into = Some("neovim".to_string());
        let steps = plan(&setup(vec![rg]), &[]);
        assert!(matches!(steps[0], Step::Manual { .. }));
        assert!(describe(&steps[0]).contains("neither installed nor listed"));
    }

    #[test]
    fn an_installed_merge_is_configured_rather_than_questioned() {
        // The target is nowhere in this file, but the app is already here — so
        // where its files came from is settled and only its settings are left.
        let mut rg = app("ripgrep");
        rg.into = Some("neovim".to_string());
        let steps = plan(&setup(vec![rg]), &["ripgrep".to_string()]);
        assert!(matches!(steps[0], Step::Configure(_)));
    }

    #[test]
    fn a_merge_into_something_already_installed_is_fine() {
        let mut rg = app("ripgrep");
        rg.into = Some("neovim".to_string());
        let steps = plan(&setup(vec![rg]), &["neovim".to_string()]);
        assert!(matches!(steps[0], Step::Install(_)));
    }

    #[test]
    fn an_app_with_no_command_of_its_own_is_recorded_as_such() {
        // A library or plugin package installs nothing to run, and the install
        // has to be told that is expected rather than a failure.
        let plain = manifest("libsndfile", None, &[]);
        assert!(plain.app.launchers.is_empty());
        let mut recorded = app("libsndfile");
        recorded.no_launcher = true;
        // The flag survives the file, which is what the importer reads.
        let text = toml::to_string_pretty(&setup(vec![recorded.clone()])).unwrap();
        assert!(parse(&text).unwrap().apps[0].no_launcher);
        // An ordinary app does not carry it, so its shortcuts come back.
        assert!(!toml::to_string_pretty(&setup(vec![app("firefox")])).unwrap().contains("no_launcher"));
    }

    #[test]
    fn a_wine_game_says_why_a_list_cannot_bring_it_back() {
        let mut game = app("nfsu2");
        game.wine_game = true;
        let steps = plan(&setup(vec![game]), &[]);
        assert!(matches!(steps[0], Step::Manual { .. }));
        assert!(describe(&steps[0]).contains("install-game"));
    }

    #[test]
    fn an_entry_with_no_package_asks_to_be_filled_in() {
        let mut blank = app("mystery");
        blank.package = "  ".to_string();
        let steps = plan(&setup(vec![blank]), &[]);
        assert!(describe(&steps[0]).contains("no package name"));
    }

    #[test]
    fn recorded_settings_keep_the_settings_and_drop_the_manual() {
        let written = "\
[temp]
; ramdisk = private in-memory tmpfs, discarded on close
mode = ramdisk

[network]
# another comment
network = off
";
        assert_eq!(condense_ini(written), "[temp]\nmode = ramdisk\n[network]\nnetwork = off\n");
        // And what is kept still parses back to the same settings.
        let config = parse_ini(&condense_ini(written)).unwrap();
        assert!(!config.network);
    }

    // ── carrying settings across ─────────────────────────────────────────────

    #[test]
    fn shared_directories_follow_the_user_to_the_new_home() {
        let config = "[sandbox]\nshare_dir = /home/alice/Downloads\nshare_dir = /mnt/media\n";
        let out = rewrite_home(config, "/home/alice", "/home/bob");
        assert!(out.contains("/home/bob/Downloads"), "{out}");
        // A path outside the home is not the importer's to move.
        assert!(out.contains("/mnt/media"), "{out}");
    }

    #[test]
    fn nothing_is_rewritten_when_the_home_is_the_same_or_unknown() {
        let config = "share_dir = /home/alice/Downloads\n";
        assert_eq!(rewrite_home(config, "/home/alice", "/home/alice"), config);
        assert_eq!(rewrite_home(config, "", "/home/bob"), config);
        assert_eq!(rewrite_home(config, "/home/alice", ""), config);
    }

    // ── applying a file ──────────────────────────────────────────────────────

    /// Put an app on disk without installing anything: the manifest and the
    /// directory are all the import path looks at for an app already present.
    fn pretend_installed(name: &str) {
        let dir = app_dir(name).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        crate::manifest::write_manifest(name, &manifest(name, None, &[name])).unwrap();
    }

    fn write_setup(dir: &Path, setup: &Setup) -> PathBuf {
        let path = dir.join("setup.toml");
        std::fs::write(&path, toml::to_string_pretty(setup).unwrap()).unwrap();
        path
    }

    #[test]
    fn importing_applies_the_settings_of_an_app_that_is_already_here() {
        let home = crate::test_support::test_home();
        pretend_installed("firefox");

        let mut firefox = app("firefox");
        firefox.config =
            "[network]\nnetwork = off\n[sandbox]\nshare_dir = /home/alice/Downloads\n".to_string();
        let path = write_setup(&home.root(), &setup(vec![firefox]));

        import(&path, false).unwrap();

        let config = crate::config::read_config("firefox").unwrap();
        assert!(!config.network);
        // The shared directory came from another user's home and now points at
        // this one's.
        let mine = std::env::var("HOME").unwrap();
        assert_eq!(config.shared_dirs, [format!("{mine}/Downloads")]);
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let home = crate::test_support::test_home();
        pretend_installed("firefox");

        let mut firefox = app("firefox");
        firefox.config = "[network]\nnetwork = off\n".to_string();
        let path = write_setup(&home.root(), &setup(vec![firefox]));

        import(&path, true).unwrap();
        assert!(!config_path("firefox").unwrap().exists());
    }

    #[test]
    fn an_app_the_file_could_not_install_gets_no_config_written() {
        let home = crate::test_support::test_home();
        // Nothing is installed, and a wine game is never installed from a list,
        // so its settings must not be written into a directory nothing owns.
        let mut game = app("nfsu2");
        game.wine_game = true;
        game.config = "[network]\nnetwork = off\n".to_string();
        let path = write_setup(&home.root(), &setup(vec![game]));

        import(&path, false).unwrap();
        assert!(!app_dir("nfsu2").unwrap().exists());
    }

    // ── what is worth recording ──────────────────────────────────────────────

    fn manifest(name: &str, pkg: Option<&str>, launchers: &[&str]) -> crate::manifest::Manifest {
        crate::manifest::Manifest {
            app: crate::manifest::AppMeta {
                name: name.to_string(),
                main_binary: launchers.first().copied().unwrap_or("").to_string(),
                installed_at: "2026-01-01".to_string(),
                launchers: launchers.iter().map(|s| s.to_string()).collect(),
                alias_of: None,
                display_name: None,
                pkg_name: pkg.map(str::to_string),
                wine_game: None,
            },
            packages: vec![],
        }
    }

    #[test]
    fn a_default_launcher_is_left_for_the_other_distribution_to_decide() {
        // `install firefox` would produce exactly this, and the package there
        // may install a differently named binary.
        assert!(recorded_launchers(&manifest("firefox", None, &["firefox"])).is_empty());
    }

    #[test]
    fn launchers_the_user_chose_are_recorded() {
        assert_eq!(
            recorded_launchers(&manifest("imagemagick", None, &["convert", "identify"])),
            ["convert", "identify"]
        );
        // `install python --app-name py312 --bin-name python3.12`
        assert_eq!(
            recorded_launchers(&manifest("py312", Some("python"), &["python3.12"])),
            ["python3.12"]
        );
    }
}
