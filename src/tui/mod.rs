pub mod konami;
mod ui;

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::ListState;
use ratatui::Terminal;

use crate::commands::dedup::all_du;
use crate::config::{read_config, write_config, AppConfig, LocalDelete, TempMode};
use crate::manifest::{list_all_apps, tree_order, Manifest};

// ── Op messages ───────────────────────────────────────────────────────────────

pub enum Msg {
    Line(String),
    Done(bool),
}

// ── File browser entry ────────────────────────────────────────────────────────

pub struct FbEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_zip: bool,
}

// ── Screen overlays ───────────────────────────────────────────────────────────

pub enum Screen {
    Main,
    Confirm {
        title: String,
        body: Vec<String>,
        action: PendingAction,
        danger: bool,
    },
    Operation {
        title: String,
        log: Vec<String>,
        done: bool,
        success: bool,
        rx: Receiver<Msg>,
        total_bytes: Option<u64>,
        /// (done, total) emitted by the subprocess via `PROGRESS n/total` lines.
        progress: Option<(u64, u64)>,
        started: Instant,
        reload: bool,
        show_log: bool,
        /// Set when the subprocess emits `PROMPT_LAUNCHER_CHOICE:<pkg>:<bins>`.
        /// Triggers the NoLauncherChoice popup once the operation finishes.
        launcher_choice: Option<(String, Vec<String>)>,
        /// The --into target from the original install, if this was a merge install.
        into_target: Option<String>,
        /// Set when the subprocess emits `PROMPT_OUTDATED_PACKAGES:<pkg>`.
        /// Triggers the OutdatedPackages popup once the operation finishes.
        outdated_pkg: Option<String>,
        /// The args originally passed to launch_op — stored so the OutdatedPackages
        /// popup can relaunch the same operation with --sync-db appended.
        original_args: Vec<String>,
    },
    Config {
        app_name: String,
        config: AppConfig,
        selected: usize,
    },
    SharedDirs {
        app_name: String,
        dirs: Vec<String>,
        selected: usize,
    },
    FileBrowser {
        current_dir: PathBuf,
        entries: Vec<FbEntry>,
        fb_state: ListState,
        /// Some(app_name) = dir-pick mode for shared dirs; None = zip import mode
        pick_dir_for: Option<String>,
    },
    /// Choose between a fresh install and merging into an existing app.
    /// Row 0 = fresh install; rows 1..=targets.len() = merge into targets[row-1].
    InstallTarget {
        pkg: String,
        targets: Vec<String>,
        selected: usize,
    },
    /// Popup launched from the Config screen with all valid choices for a
    /// single setting. `setting_idx` is the row index in the Config screen
    /// (0..=5); `selected` is the highlighted option inside the popup.
    OptionPicker {
        app_name: String,
        config: AppConfig,
        setting_idx: usize,
        selected: usize,
    },
    /// Help popup shown when the user presses `?` on a Config row.
    /// Dismisses on any key and returns to Config with `back_selected` active.
    SettingHelp {
        app_name: String,
        config: AppConfig,
        back_selected: usize,
    },
    /// Per-option help popup opened from OptionPicker when the user presses `?`
    /// while hovering over a specific choice.  Dismisses back to OptionPicker.
    OptionHelp {
        app_name: String,
        config: AppConfig,
        setting_idx: usize,
        picker_selected: usize,
    },
    /// Free-text input overlay for spoof string settings.
    TextInput {
        app_name: String,
        config: AppConfig,
        back_selected: usize,
        field_idx: usize,
        value: String,
    },
    /// Full key-bindings reference popup. Dismisses on any key.
    KeyHelp,
    /// Inline text input for renaming an app's display name.
    RenameApp {
        app_name: String,
        value: String,
    },
    /// Inline text input for picking an app name when installing a duplicate package.
    DuplicateInstall {
        pkg: String,
        value: String,
    },
    /// Choice between uninstalling an existing app or installing a second copy.
    AlreadyInstalled {
        pkg: String,
        selected: usize, // 0 = install second copy, 1 = uninstall
    },
    /// Shown after an install fails because no binary was found in the package.
    /// User picks "keep without launcher" or "clean up (already done)".
    NoLauncherChoice {
        pkg: String,
        available_bins: Vec<String>,
        selected: usize, // 0 = keep, 1 = clean (already done)
        /// The --into target if this was a merge install, so the retry can pass it.
        into_target: Option<String>,
    },
    /// Shown when a download returns 404 (package databases are out of date).
    /// User picks "Update & retry" or "Cancel".
    OutdatedPackages {
        pkg: String,
        /// The original install args; retry appends --sync-db to these.
        install_args: Vec<String>,
        selected: usize, // 0 = update & retry, 1 = cancel
    },
}

pub enum PendingAction {
    Remove(String),
    ConfirmedRemove(String),
    /// Remove a target app together with all aliases that point at it.
    RemoveCascade(String, Vec<String>),
    ConfirmedRemoveCascade(String),
    Update(String),
    Install { pkg: String, app_name: Option<String>, into: Option<String> },
    Export(String),
    Snapshot(String),
    Rollback(String, String),
}

// ── Tabs ──────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Installed,
    Install,
    Import,
    Space,
}

// ── App state ─────────────────────────────────────────────────────────────────

pub struct App {
    pub quit: bool,
    pub tab: Tab,
    // Installed tab
    pub installed: Vec<Manifest>,
    pub inst_state: ListState,
    pub update_available: HashMap<String, String>,
    // Install tab — async search
    pub search_input: String,
    /// (package_name, optional_repo) pairs returned by pkg_search.
    pub search_results: Vec<(String, Option<String>)>,
    pub search_searching: bool,
    pub search_gen: u64,
    pub search_tx: Sender<(u64, Vec<(String, Option<String>)>)>,
    pub search_rx: Receiver<(u64, Vec<(String, Option<String>)>)>,
    pub avail_state: ListState,
    pub search_list_focused: bool,
    // Import tab
    pub import_input: String,
    // Disk usage (computed once on load, refreshed on reload)
    pub app_sizes: HashMap<String, u64>,
    pub du_apparent: u64,
    pub du_actual: u64,
    // Overlay
    pub screen: Screen,
    pub status: String,
    pub log_scroll: usize,
    pub needs_clear: bool,
    /// If Some, the event loop will suspend the TUI, exec `wryayer run <app>`
    /// with inherited stdio so the user actually interacts with the app,
    /// then resume. Set by pressing `r`/Enter on an installed app.
    pub run_request: Option<String>,
    /// If Some, the event loop will suspend the TUI, open an editor on the
    /// given path, save config to "custom" after, then resume.
    /// Tuple: (app_name, path_to_edit)
    pub editor_request: Option<(String, PathBuf)>,
    // ── Easter egg ────────────────────────────────────────────────────────────
    pub konami_mode: bool,
    pub konami_state: usize,
}

impl App {
    fn new() -> Result<Self> {
        let installed = tree_order(list_all_apps()?);
        let mut inst_state = ListState::default();
        if !installed.is_empty() {
            inst_state.select(Some(0));
        }
        let (app_sizes, du_apparent, du_actual) = all_du().unwrap_or_default();
        let (search_tx, search_rx) = mpsc::channel();
        Ok(Self {
            quit: false,
            tab: Tab::Installed,
            installed,
            inst_state,
            update_available: HashMap::new(),
            search_input: String::new(),
            search_results: Vec::new(),
            search_searching: false,
            search_gen: 0,
            search_tx,
            search_rx,
            avail_state: ListState::default(),
            search_list_focused: false,
            import_input: String::new(),
            app_sizes,
            du_apparent,
            du_actual,
            screen: Screen::Main,
            status: String::new(),
            log_scroll: 0,
            needs_clear: false,
            run_request: None,
            editor_request: None,
            konami_mode: false,
            konami_state: 0,
        })
    }

    fn reload_installed(&mut self) {
        if let Ok(list) = list_all_apps() {
            self.installed = tree_order(list);
            let sel = self.inst_state.selected().unwrap_or(0);
            if self.installed.is_empty() {
                self.inst_state.select(None);
            } else {
                self.inst_state.select(Some(sel.min(self.installed.len() - 1)));
            }
        }
        if let Ok((sizes, apparent, actual)) = all_du() {
            self.app_sizes = sizes;
            self.du_apparent = apparent;
            self.du_actual = actual;
        }
    }

    pub fn selected_installed(&self) -> Option<&Manifest> {
        self.inst_state.selected().and_then(|i| self.installed.get(i))
    }

    pub fn selected_available(&self) -> Option<&str> {
        self.avail_state
            .selected()
            .and_then(|i| self.search_results.get(i))
            .map(|(name, _)| name.as_str())
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let res = event_loop(&mut terminal);
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new()?;

    loop {
        // Drain op channel
        if let Screen::Operation { rx, log, done, success, progress, launcher_choice, outdated_pkg, .. } = &mut app.screen {
            loop {
                match rx.try_recv() {
                    Ok(Msg::Line(l)) => {
                        if let Some(rest) = l.strip_prefix("PROMPT_LAUNCHER_CHOICE:") {
                            if let Some((pkg, bins_str)) = rest.split_once(':') {
                                let bins: Vec<String> = if bins_str.is_empty() {
                                    vec![]
                                } else {
                                    bins_str.split(',').map(str::to_string).collect()
                                };
                                *launcher_choice = Some((pkg.to_string(), bins));
                            }
                        } else if let Some(rest) = l.strip_prefix("PROMPT_OUTDATED_PACKAGES:") {
                            *outdated_pkg = Some(rest.to_string());
                        } else if let Some(p) = parse_progress(&l) {
                            *progress = Some(p);
                        } else {
                            log.push(l);
                        }
                    }
                    Ok(Msg::Done(ok)) => { *done = true; *success = ok; }
                    Err(_) => break,
                }
            }
        }

        // Auto-transition to NoLauncherChoice popup when op finishes with the marker set.
        if let Screen::Operation { done: true, success: false, launcher_choice: Some(_), .. } = &app.screen {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::Operation { launcher_choice: Some((pkg, bins)), into_target, .. } = screen {
                app.screen = Screen::NoLauncherChoice { pkg, available_bins: bins, selected: 0, into_target };
                app.needs_clear = true;
            }
        }

        // Auto-transition to OutdatedPackages popup when op finishes with the marker set.
        if let Screen::Operation { done: true, success: false, outdated_pkg: Some(_), .. } = &app.screen {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::Operation { outdated_pkg: Some(pkg), original_args, .. } = screen {
                app.screen = Screen::OutdatedPackages { pkg, install_args: original_args, selected: 0 };
                app.needs_clear = true;
            }
        }

        // Drain async search results
        loop {
            match app.search_rx.try_recv() {
                Ok((gen, results)) if gen == app.search_gen => {
                    app.search_results = results;
                    app.search_searching = false;
                    app.status = if app.search_results.is_empty() {
                        "No results.".into()
                    } else {
                        format!("{} results", app.search_results.len())
                    };
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        terminal.draw(|f| ui::draw(f, &mut app))?;

        if app.quit {
            return Ok(());
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return Ok(());
            }
            // Shift+Q force-quits from anywhere, even during running operations
            if key.code == KeyCode::Char('Q') {
                return Ok(());
            }
            handle_key(&mut app, key.code)?;
            if let Some(name) = app.run_request.take() {
                run_app_inline(terminal, &name, &mut app)?;
            }
            if let Some((app_name, cpuinfo_path)) = app.editor_request.take() {
                open_editor_inline(terminal, &app_name, cpuinfo_path, &mut app)?;
            }
            if app.needs_clear {
                app.needs_clear = false;
                terminal.clear()?;
            }
        }
    }
}

/// Suspend the TUI, exec `wryayer run <app>` with inherited stdio so the user
/// sees and interacts with the app on the real terminal, then resume the TUI.
fn run_app_inline(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app_name: &str,
    app: &mut App,
) -> Result<()> {
    // Tear down the TUI's terminal state
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    let status = Command::new(&exe)
        .args(["run", app_name])
        .status();

    // Resume the TUI's terminal state
    enable_raw_mode()?;
    terminal.backend_mut().execute(EnterAlternateScreen)?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    app.needs_clear = false;

    match status {
        Ok(s) if !s.success() => {
            app.status = format!("{app_name} exited with status {}", s.code().unwrap_or(-1));
        }
        Err(e) => {
            app.status = format!("failed to launch {app_name}: {e}");
        }
        _ => app.status.clear(),
    }
    Ok(())
}

/// Suspend the TUI, let the user pick an editor and edit the cpuinfo file,
/// save "custom" into the app config, then resume the TUI.
fn open_editor_inline(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app_name: &str,
    cpuinfo_path: PathBuf,
    app: &mut App,
) -> Result<()> {
    use std::io::Write;

    // Tear down the TUI's terminal state so we get a normal shell.
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let editors = find_editors();
    let chosen = if editors.is_empty() {
        eprintln!("No editor found. Install one of: nvim, vim, vi, nano.");
        std::thread::sleep(std::time::Duration::from_secs(2));
        None
    } else if editors.len() == 1 {
        Some(editors[0])
    } else {
        println!("Select editor:");
        for (i, ed) in editors.iter().enumerate() {
            println!("  {}) {ed}", i + 1);
        }
        print!("Choice [1]: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            Some(editors[0])
        } else {
            trimmed.parse::<usize>().ok()
                .and_then(|n| if n >= 1 && n <= editors.len() { Some(editors[n - 1]) } else { None })
                .or(Some(editors[0]))
        }
    };

    let edited = if let Some(editor) = chosen {
        // Ensure the .spoof/ dir exists.
        if let Some(parent) = cpuinfo_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Pre-populate on first use with the real /proc/cpuinfo so the user
        // has a starting point instead of a blank file.
        if !cpuinfo_path.exists() {
            let seed = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
            let _ = std::fs::write(&cpuinfo_path, seed);
        }
        Command::new(editor)
            .arg(&cpuinfo_path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        false
    };

    // Restore the TUI.
    enable_raw_mode()?;
    terminal.backend_mut().execute(EnterAlternateScreen)?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    app.needs_clear = false;

    // If the editor ran and the file exists, update config and go to Config screen.
    if edited && cpuinfo_path.exists() {
        if let Ok(mut cfg) = crate::config::read_config(app_name) {
            cfg.spoof_cpuinfo = Some("custom".to_string());
            let _ = crate::config::write_config(app_name, &cfg);
            app.screen = Screen::Config {
                app_name: app_name.to_string(),
                config: cfg,
                selected: CFG_SPOOF_CPUINFO,
            };
            app.status = "CPU info saved.".to_string();
        }
    } else {
        if let Ok(cfg) = crate::config::read_config(app_name) {
            app.screen = Screen::Config {
                app_name: app_name.to_string(),
                config: cfg,
                selected: CFG_SPOOF_CPUINFO,
            };
        }
        if chosen.is_none() {
            app.status = "No editor available — install nvim, vim, vi, or nano.".to_string();
        }
    }

    Ok(())
}

/// Return the editors from {nvim, vim, vi, nano} that are on PATH, in that order.
fn find_editors() -> Vec<&'static str> {
    ["nvim", "vim", "vi", "nano"]
        .iter()
        .copied()
        .filter(|ed| {
            std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).any(|dir| dir.join(ed).exists()))
                .unwrap_or(false)
        })
        .collect()
}

// ── Key dispatch ──────────────────────────────────────────────────────────────

fn handle_key(app: &mut App, code: KeyCode) -> Result<()> {
    // Konami code detection — only listens on the main screen so it doesn't
    // interfere with text input or list navigation in modal screens.
    if matches!(app.screen, Screen::Main) {
        konami_step(app, code);
    }

    let tag = match &app.screen {
        Screen::Main => 0u8,
        Screen::Confirm { .. } => 1,
        Screen::Operation { done: false, .. } => 2,
        Screen::Operation { done: true, .. } => 3,
        Screen::Config { .. } => 4,
        Screen::FileBrowser { .. } => 5,
        Screen::SharedDirs { .. } => 6,
        Screen::InstallTarget { .. } => 7,
        Screen::OptionPicker { .. } => 8,
        Screen::SettingHelp { .. } => 9,
        Screen::OptionHelp { .. } => 10,
        Screen::TextInput { .. } => 11,
        Screen::KeyHelp => 12,
        Screen::RenameApp { .. } => 13,
        Screen::DuplicateInstall { .. } => 14,
        Screen::AlreadyInstalled { .. } => 15,
        Screen::NoLauncherChoice { .. } => 16,
        Screen::OutdatedPackages { .. } => 17,
    };

    match tag {
        0 => on_main(app, code)?,
        1 => on_confirm(app, code)?,
        2 => on_op_running(app, code),
        3 => on_op_done(app, code)?,
        4 => on_config(app, code),
        5 => on_file_browser(app, code),
        6 => on_shared_dirs(app, code),
        7 => on_install_target(app, code),
        8 => on_option_picker(app, code),
        9 => on_setting_help(app, code),
        10 => on_option_help(app, code),
        11 => on_text_input(app, code),
        12 => on_key_help(app),
        13 => on_rename_app(app, code),
        14 => on_duplicate_install(app, code),
        15 => on_already_installed(app, code),
        16 => on_no_launcher_choice(app, code),
        17 => on_outdated_packages(app, code),
        _ => {}
    }
    Ok(())
}

// ── Main screen ───────────────────────────────────────────────────────────────

fn on_main(app: &mut App, code: KeyCode) -> Result<()> {
    // Tab cycling — always active
    match code {
        KeyCode::Tab => {
            app.tab = match app.tab {
                Tab::Installed => Tab::Install,
                Tab::Install  => Tab::Import,
                Tab::Import   => Tab::Space,
                Tab::Space    => Tab::Installed,
            };
            app.status.clear();
            return Ok(());
        }
        KeyCode::BackTab => {
            app.tab = match app.tab {
                Tab::Installed => Tab::Space,
                Tab::Install   => Tab::Installed,
                Tab::Import    => Tab::Install,
                Tab::Space     => Tab::Import,
            };
            app.status.clear();
            return Ok(());
        }
        _ => {}
    }

    // 'q' / Esc quit only when NOT in a text-input context.
    // Install tab: input is active while the search list is not focused.
    // Import tab: always a text input.
    let in_text_input = matches!(app.tab, Tab::Import)
        || (matches!(app.tab, Tab::Install) && !app.search_list_focused);
    if !in_text_input && matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
        app.quit = true;
        return Ok(());
    }

    match app.tab {
        Tab::Installed => on_installed(app, code),
        Tab::Install   => on_install(app, code),
        Tab::Import    => on_import(app, code),
        Tab::Space     => on_space_tab(app, code),
    }
    Ok(())
}

// ── Installed tab ─────────────────────────────────────────────────────────────

fn on_installed(app: &mut App, code: KeyCode) {
    let len = app.installed.len();
    if len == 0 {
        return;
    }

    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            let i = app.inst_state.selected().unwrap_or(0);
            app.inst_state.select(Some(if i == 0 { len - 1 } else { i - 1 }));
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let i = app.inst_state.selected().unwrap_or(0);
            app.inst_state.select(Some((i + 1) % len));
        }
        KeyCode::Char('r') | KeyCode::Enter => {
            if let Some(m) = app.selected_installed() {
                if m.app.main_binary.is_empty() {
                    app.status = format!(
                        "'{}' has no launcher — reinstall with: wryayer install --bin-names <name> {}",
                        m.app.name,
                        m.app.pkg_name.as_deref().unwrap_or(&m.app.name),
                    );
                } else {
                    // Hand off to the event loop, which will suspend the TUI and
                    // run the app attached to the real terminal. Going through the
                    // operation overlay (with piped stdout) would mangle interactive
                    // and ANSI-art output.
                    app.run_request = Some(m.app.name.clone());
                }
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                if m.app.alias_of.is_none() {
                    let dependents: Vec<String> = app.installed
                        .iter()
                        .filter(|other| other.app.alias_of.as_deref() == Some(&name))
                        .map(|other| other.app.name.clone())
                        .collect();
                    if !dependents.is_empty() {
                        let n = dependents.len();
                        app.screen = Screen::Confirm {
                            title: format!("Remove '{name}' and {n} alias(es)?"),
                            body: vec![
                                format!("Also removes: {}", dependents.join(", ")),
                                String::new(),
                                "Press y to delete all, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::RemoveCascade(name, dependents),
                            danger: true,
                        };
                        return;
                    }
                }
                app.screen = Screen::Confirm {
                    title: format!("Remove '{name}'?"),
                    body: vec![
                        format!("Delete ~/.wryayer/{name}/ and all launchers?"),
                        String::new(),
                        "Press y to continue, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Remove(name),
                    danger: true,
                };
            }
        }
        KeyCode::Char('e') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let zip = format!("{}-{}.zip", name, chrono::Local::now().format("%Y-%m-%d"));
                app.screen = Screen::Confirm {
                    title: format!("Export '{name}'?"),
                    body: vec![
                        format!("Output: ~/{zip}"),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Export(name),
                    danger: false,
                };
            }
        }
        KeyCode::Char('p') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                app.screen = Screen::Confirm {
                    title: format!("Snapshot '{name}'?"),
                    body: vec![
                        "Creates a hard-linked snapshot (instant, near-zero disk).".into(),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Snapshot(name),
                    danger: false,
                };
            }
        }
        KeyCode::Char('o') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                match crate::commands::snapshot::latest(&name) {
                    Ok(Some(snap)) => {
                        app.screen = Screen::Confirm {
                            title: format!("Rollback '{name}'?"),
                            body: vec![
                                format!("Restore from snapshot: {snap}"),
                                String::new(),
                                "Press y to roll back, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::Rollback(name, snap),
                            danger: true,
                        };
                    }
                    Ok(None) => app.status = format!("No snapshots for {name}"),
                    Err(e) => app.status = format!("snapshot lookup failed: {e:#}"),
                }
            }
        }
        KeyCode::Char('c') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                launch_op(app, format!("Check updates — {name}"), vec!["update".into(), "--check".into(), name], None, false);
            }
        }
        KeyCode::Char('u') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let new_ver = app.update_available.get(&name).cloned();
                let body = match new_ver {
                    Some(ref v) => {
                        let cur = m.packages.iter().find(|p| p.name == name)
                            .map(|p| p.version.as_str()).unwrap_or("?");
                        vec![format!("  {cur}  →  {v}"), String::new(), "Press y to update, n or Esc to cancel.".into()]
                    }
                    None => vec![
                        "Run [c]heck first to verify an update is available.".into(),
                        String::new(),
                        "Press y to update anyway, n or Esc to cancel.".into(),
                    ],
                };
                app.screen = Screen::Confirm {
                    title: format!("Update '{name}'?"),
                    body,
                    action: PendingAction::Update(name),
                    danger: false,
                };
            }
        }
        KeyCode::Char('s') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let config = read_config(&name).unwrap_or_default();
                app.screen = Screen::Config { app_name: name, config, selected: 0 };
            }
        }
        KeyCode::Char('n') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let value = m.app.display_name.clone().unwrap_or_default();
                app.screen = Screen::RenameApp { app_name: name, value };
                app.needs_clear = true;
            }
        }
        KeyCode::Char('?') => {
            app.screen = Screen::KeyHelp;
            app.needs_clear = true;
        }
        _ => {}
    }
}

fn on_key_help(app: &mut App) {
    app.screen = Screen::Main;
    app.needs_clear = true;
}

// ── App rename overlay ────────────────────────────────────────────────────────

fn on_rename_app(app: &mut App, code: KeyCode) {
    let Screen::RenameApp { app_name, value } = &mut app.screen else { return };
    match code {
        KeyCode::Esc => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let name = app_name.clone();
            let v = value.trim().to_string();
            if let Ok(mut m) = crate::manifest::read_manifest(&name) {
                m.app.display_name = if v.is_empty() { None } else { Some(v.clone()) };
                let _ = crate::manifest::write_manifest(&name, &m);
                app.reload_installed();
                app.status = if v.is_empty() {
                    format!("Display name cleared for '{name}'")
                } else {
                    format!("'{name}' renamed to '{v}'")
                };
            }
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Backspace => { value.pop(); }
        KeyCode::Char(c) => { value.push(c); }
        _ => {}
    }
}

// ── Duplicate install overlay ─────────────────────────────────────────────────

fn on_duplicate_install(app: &mut App, code: KeyCode) {
    let Screen::DuplicateInstall { pkg, value } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let new_name = value.trim().to_string();
            if new_name.is_empty() {
                return;
            }
            if app.installed.iter().any(|m| m.app.name == new_name) {
                app.status = format!("'{new_name}' is already taken — pick a different name");
                return;
            }
            let pkg = pkg.clone();
            app.screen = Screen::Confirm {
                title: format!("Install '{pkg}' as '{new_name}'?"),
                body: vec![
                    format!("Creates ~/.wryayer/{new_name}/ alongside the existing '{pkg}'."),
                    String::new(),
                    "Press y to confirm, n or Esc to cancel.".into(),
                ],
                action: PendingAction::Install { pkg, app_name: Some(new_name), into: None },
                danger: false,
            };
        }
        KeyCode::Backspace => { value.pop(); }
        KeyCode::Char(c) => { value.push(c); }
        _ => {}
    }
}

// ── Already installed choice ──────────────────────────────────────────────────

fn on_already_installed(app: &mut App, code: KeyCode) {
    let Screen::AlreadyInstalled { pkg, selected } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if *selected > 0 { *selected -= 1; }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if *selected < 1 { *selected += 1; }
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let pkg = pkg.clone();
            match *selected {
                0 => {
                    // Install a second copy — ask for a new app name.
                    app.screen = Screen::DuplicateInstall { pkg, value: String::new() };
                    app.needs_clear = true;
                }
                _ => {
                    // Uninstall the existing app.
                    let dependents: Vec<String> = app.installed
                        .iter()
                        .filter(|other| other.app.alias_of.as_deref() == Some(&pkg))
                        .map(|other| other.app.name.clone())
                        .collect();
                    if !dependents.is_empty() {
                        let n = dependents.len();
                        app.screen = Screen::Confirm {
                            title: format!("Remove '{pkg}' and {n} alias(es)?"),
                            body: vec![
                                format!("Also removes: {}", dependents.join(", ")),
                                String::new(),
                                "Press y to delete all, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::RemoveCascade(pkg, dependents),
                            danger: true,
                        };
                    } else {
                        app.screen = Screen::Confirm {
                            title: format!("Remove '{pkg}'?"),
                            body: vec![
                                format!("Delete ~/.wryayer/{pkg}/ and all launchers?"),
                                String::new(),
                                "Press y to continue, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::Remove(pkg),
                            danger: true,
                        };
                    }
                }
            }
        }
        _ => {}
    }
}

// ── No-launcher choice popup ──────────────────────────────────────────────────

fn on_no_launcher_choice(app: &mut App, code: KeyCode) {
    let Screen::NoLauncherChoice { pkg, selected, into_target, .. } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if *selected > 0 { *selected -= 1; }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if *selected < 1 { *selected += 1; }
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let pkg = pkg.clone();
            let into_target = into_target.clone();
            match *selected {
                0 => {
                    // Re-run install with --keep-without-launcher, preserving --into if set.
                    let mut args = vec!["install".into(), pkg.clone(), "--keep-without-launcher".into()];
                    if let Some(t) = &into_target {
                        args.extend(["--into".into(), t.clone()]);
                    }
                    launch_op(
                        app,
                        format!("Install — {pkg} (no launcher)"),
                        args,
                        None,
                        true,
                    );
                }
                _ => {
                    // Clean up — install already cleaned up on error; just go back.
                    app.screen = Screen::Main;
                    app.needs_clear = true;
                }
            }
        }
        _ => {}
    }
}

fn on_outdated_packages(app: &mut App, code: KeyCode) {
    let Screen::OutdatedPackages { selected, install_args, pkg, .. } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if *selected > 0 { *selected -= 1; }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if *selected < 1 { *selected += 1; }
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let s = *selected;
            let pkg = pkg.clone();
            let mut args = install_args.clone();
            match s {
                0 => {
                    // Update sources and retry: append --sync-db so the install
                    // command runs 'sudo pacman -Sy' before downloading.
                    args.push("--sync-db".into());
                    launch_op(app, format!("Install — {pkg} (update sources)"), args, None, true);
                }
                _ => {
                    app.screen = Screen::Main;
                    app.needs_clear = true;
                }
            }
        }
        _ => {}
    }
}

// ── Install tab ───────────────────────────────────────────────────────────────

fn on_install(app: &mut App, code: KeyCode) {
    if !app.search_list_focused {
        match code {
            KeyCode::Char(c) => {
                app.search_input.push(c);
                trigger_search(app);
            }
            KeyCode::Backspace => {
                app.search_input.pop();
                trigger_search(app);
            }
            KeyCode::Down => {
                if !app.search_results.is_empty() {
                    app.search_list_focused = true;
                    app.avail_state.select(Some(0));
                }
            }
            _ => {}
        }
    } else {
        let len = app.search_results.len();
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                let i = app.avail_state.selected().unwrap_or(0);
                if i == 0 {
                    app.search_list_focused = false;
                    app.avail_state.select(None);
                } else {
                    app.avail_state.select(Some(i - 1));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = app.avail_state.selected().unwrap_or(0);
                if len > 0 {
                    app.avail_state.select(Some((i + 1).min(len - 1)));
                }
            }
            KeyCode::Enter => {
                if let Some(pkg) = app.selected_available() {
                    let pkg = pkg.to_string();
                    if app.installed.iter().any(|m| m.app.name == pkg) {
                        // Already installed — let the user choose: install again or uninstall.
                        app.screen = Screen::AlreadyInstalled { pkg, selected: 0 };
                        app.needs_clear = true;
                    } else {
                        // If there's at least one installed app, offer the
                        // user a choice between a fresh install and merging
                        // into an existing app (-> `wryayer install --into`).
                        let targets: Vec<String> = app.installed
                            .iter()
                            .filter(|m| m.app.alias_of.is_none())
                            .map(|m| m.app.name.clone())
                            .collect();
                        if targets.is_empty() {
                            app.screen = Screen::Confirm {
                                title: format!("Install '{pkg}'?"),
                                body: vec![
                                    format!("Installs {pkg} into ~/.wryayer/{pkg}/"),
                                    String::new(),
                                    "Press y to confirm, n or Esc to cancel.".into(),
                                ],
                                action: PendingAction::Install { pkg, app_name: None, into: None },
                                danger: false,
                            };
                        } else {
                            app.screen = Screen::InstallTarget { pkg, targets, selected: 0 };
                        }
                    }
                }
            }
            KeyCode::Esc => {
                app.search_list_focused = false;
                app.avail_state.select(None);
            }
            _ => {}
        }
    }
}

fn trigger_search(app: &mut App) {
    let query = app.search_input.trim().to_string();
    if query.is_empty() {
        app.search_gen += 1;
        app.search_results.clear();
        app.search_searching = false;
        app.status.clear();
        return;
    }
    app.search_gen += 1;
    app.search_searching = true;
    app.status = format!("Searching '{query}'…");
    let gen = app.search_gen;
    let tx = app.search_tx.clone();
    thread::spawn(move || {
        let results = crate::distro::pkg_search(&query);
        let _ = tx.send((gen, results));
    });
}

// ── Install target picker ─────────────────────────────────────────────────────

fn on_install_target(app: &mut App, code: KeyCode) {
    let Screen::InstallTarget { pkg, targets, selected } = &mut app.screen else { return };
    let rows = targets.len() + 1; // row 0 = fresh; rest = merge targets

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = if *selected == 0 { rows - 1 } else { *selected - 1 };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1) % rows;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let pkg = pkg.clone();
            let into = if *selected == 0 {
                None
            } else {
                Some(targets[*selected - 1].clone())
            };
            let (title, body) = match &into {
                None => (
                    format!("Install '{pkg}'?"),
                    vec![
                        format!("Installs {pkg} into ~/.wryayer/{pkg}/"),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                ),
                Some(target) => (
                    format!("Merge '{pkg}' into '{target}'?"),
                    vec![
                        format!("Adds {pkg} (and missing deps) to ~/.wryayer/{target}/"),
                        format!("A launcher ~/bin/{pkg} will be created."),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                ),
            };
            app.screen = Screen::Confirm {
                title,
                body,
                action: PendingAction::Install { pkg, app_name: None, into },
                danger: false,
            };
        }
        _ => {}
    }
}

// ── Import tab ────────────────────────────────────────────────────────────────

fn on_import(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            // Clear input or switch away
            if app.import_input.is_empty() {
                app.tab = Tab::Installed;
            } else {
                app.import_input.clear();
            }
        }
        KeyCode::Char('q') => {
            // In import tab 'q' is intercepted here — typing 'q' into the path
            // To quit from import tab, use Shift+Q
            app.import_input.push('q');
        }
        KeyCode::Char(c) => app.import_input.push(c),
        KeyCode::Backspace => { app.import_input.pop(); }
        KeyCode::Tab | KeyCode::BackTab => {} // handled by on_main already
        KeyCode::F(1) | KeyCode::F(2) => {
            open_file_browser(app, None);
        }
        KeyCode::Enter => {
            let raw = app.import_input.trim().to_string();
            if raw.is_empty() {
                return;
            }
            let path = shellexpand::tilde(&raw).into_owned();
            let zip_bytes = std::fs::metadata(&path).ok().map(|m| m.len());
            launch_op(app, format!("Import — {path}"), vec!["import".into(), path], zip_bytes, true);
            app.import_input.clear();
        }
        _ => {}
    }
}

// ── Confirm dialog ────────────────────────────────────────────────────────────

fn on_confirm(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::Confirm { action, .. } = screen {
                execute_action(app, action);
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.screen = Screen::Main;
        }
        _ => {}
    }
    Ok(())
}

fn execute_action(app: &mut App, action: PendingAction) {
    match action {
        PendingAction::Remove(name) => {
            // Double-confirm: first press shows this second dialog
            app.screen = Screen::Confirm {
                title: format!("PERMANENTLY delete '{name}'?"),
                body: vec![
                    "This cannot be undone.".into(),
                    String::new(),
                    "Press y again to permanently delete, n or Esc to cancel.".into(),
                ],
                action: PendingAction::ConfirmedRemove(name),
                danger: true,
            };
        }
        PendingAction::ConfirmedRemove(name) =>
            launch_op(app, format!("Remove — {name}"), vec!["remove".into(), name], None, true),
        PendingAction::RemoveCascade(name, aliases) => {
            let alias_list = aliases.join(", ");
            app.screen = Screen::Confirm {
                title: format!("PERMANENTLY delete '{name}' and all aliases?"),
                body: vec![
                    format!("Will delete: {name}, {alias_list}"),
                    "This cannot be undone.".into(),
                    String::new(),
                    "Press y again to confirm, n or Esc to cancel.".into(),
                ],
                action: PendingAction::ConfirmedRemoveCascade(name),
                danger: true,
            };
        }
        PendingAction::ConfirmedRemoveCascade(name) =>
            launch_op(app, format!("Remove — {name}"), vec!["remove".into(), "--cascade".into(), name], None, true),
        PendingAction::Update(name) =>
            launch_op(app, format!("Update — {name}"), vec!["update".into(), name], None, true),
        PendingAction::Install { pkg, app_name: None, into: None } =>
            launch_op(app, format!("Install — {pkg}"), vec!["install".into(), pkg], None, true),
        PendingAction::Install { pkg, app_name: Some(an), into: None } =>
            launch_op(app, format!("Install — {pkg} as {an}"),
                vec!["install".into(), pkg, "--app-name".into(), an], None, true),
        PendingAction::Install { pkg, app_name, into: Some(target) } => {
            let mut args = vec!["install".into(), pkg.clone(), "--into".into(), target.clone()];
            if let Some(an) = app_name { args.extend(["--app-name".into(), an]); }
            launch_op(app, format!("Install — {pkg} → {target}"), args, None, true);
        }
        PendingAction::Export(name) => {
            let total = dir_bytes(&format!(
                "{}/.wryayer/{name}",
                std::env::var("HOME").unwrap_or_default()
            ));
            launch_op(app, format!("Export — {name}"), vec!["export".into(), name], total, false);
        }
        PendingAction::Snapshot(name) =>
            launch_op(app, format!("Snapshot — {name}"), vec!["snapshot".into(), name], None, true),
        PendingAction::Rollback(name, snap) =>
            launch_op(app, format!("Rollback — {name}"), vec!["rollback".into(), name, snap], None, true),
    }
}

// ── Operation screens ─────────────────────────────────────────────────────────

fn on_op_running(app: &mut App, code: KeyCode) {
    if let Screen::Operation { show_log, log, .. } = &mut app.screen {
        match code {
            KeyCode::Char('t') => {
                *show_log = !*show_log;
                // When opening the log, jump to the bottom.
                if *show_log {
                    app.log_scroll = log.len().saturating_sub(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if *show_log => {
                if app.log_scroll > 0 { app.log_scroll -= 1; }
            }
            KeyCode::Down | KeyCode::Char('j') if *show_log => {
                app.log_scroll += 1;
            }
            _ => {}
        }
    }
}

fn on_op_done(app: &mut App, code: KeyCode) -> Result<()> {
    if let Screen::Operation { show_log, log, .. } = &mut app.screen {
        if code == KeyCode::Char('t') {
            *show_log = !*show_log;
            if *show_log {
                app.log_scroll = log.len().saturating_sub(1);
            }
            return Ok(());
        }
        if *show_log {
            match code {
                KeyCode::Up | KeyCode::Char('k') => { if app.log_scroll > 0 { app.log_scroll -= 1; } }
                KeyCode::Down | KeyCode::Char('j') => { app.log_scroll += 1; }
                _ => {}
            }
        }
    }
    match code {
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::Operation { reload, success, .. } = screen {
                if reload && success {
                    app.reload_installed();
                }
            }
            app.needs_clear = true;
        }
        _ => {}
    }
    Ok(())
}

// ── Config screen ─────────────────────────────────────────────────────────────

// Rows: 0=network 1=camera 2=microphone 3=audio 4=temp_mode 5=temp_delete 6=shared_dirs
//       7=spoof_hostname 8=spoof_username 9=spoof_machine_id 10=spoof_cpuinfo 11=spoof_os
//       12=spoof_terminal 13=ram_limit 14=Save
pub const CFG_LEN: usize = 15;
pub const CFG_SHARES: usize = 6;
pub const CFG_SPOOF_HOSTNAME: usize = 7;
pub const CFG_SPOOF_USERNAME: usize = 8;
pub const CFG_SPOOF_MACHINE_ID: usize = 9;
pub const CFG_SPOOF_CPUINFO: usize = 10;
pub const CFG_SPOOF_OS: usize = 11;
pub const CFG_SPOOF_TERMINAL: usize = 12;
pub const CFG_RAM_LIMIT: usize = 13;
pub const CFG_SAVE: usize = 14;

/// A fixed 32-char hex machine-id that apps can use as a plausible-looking ID.
pub const MACHINE_ID_SAMPLE: &str = "cafebabe0011223344556677deadbeef";
/// Generic hostname used by the "sample" option.
pub const HOSTNAME_SAMPLE: &str = "workstation";
/// Generic username used by the "sample" option.
pub const USERNAME_SAMPLE: &str = "user";

fn on_config(app: &mut App, code: KeyCode) {
    let Screen::Config { app_name, config, selected } = &mut app.screen else { return };

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Discard changes
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1).min(CFG_LEN - 1);
        }
        KeyCode::Right | KeyCode::Char(' ') => {
            if *selected == CFG_SAVE {
                let name = app_name.clone();
                let cfg = config.clone();
                app.screen = Screen::Main;
                let _ = write_config(&name, &cfg);
                app.needs_clear = true;
                return;
            }
            if *selected == CFG_SHARES {
                let name = app_name.clone();
                let dirs = config.shared_dirs.clone();
                let sel = if dirs.is_empty() { 0 } else { dirs.len() - 1 };
                app.screen = Screen::SharedDirs { app_name: name, dirs, selected: sel };
                app.needs_clear = true;
                return;
            }
            cycle_setting(config, *selected, 1);
        }
        KeyCode::Left => {
            // Inverse of Right — cycle backward. Special rows are no-ops.
            if *selected != CFG_SAVE && *selected != CFG_SHARES {
                cycle_setting(config, *selected, -1);
            }
        }
        KeyCode::Enter => {
            if *selected == CFG_SAVE {
                let name = app_name.clone();
                let cfg = config.clone();
                app.screen = Screen::Main;
                let _ = write_config(&name, &cfg);
                app.needs_clear = true;
                return;
            }
            if *selected == CFG_SHARES {
                let name = app_name.clone();
                let dirs = config.shared_dirs.clone();
                let sel = if dirs.is_empty() { 0 } else { dirs.len() - 1 };
                app.screen = Screen::SharedDirs { app_name: name, dirs, selected: sel };
                app.needs_clear = true;
                return;
            }
            // Open the option picker for this row.
            let name = app_name.clone();
            let cfg = config.clone();
            let idx = *selected;
            let cur = setting_current(&cfg, idx);
            app.screen = Screen::OptionPicker {
                app_name: name,
                config: cfg,
                setting_idx: idx,
                selected: cur,
            };
            app.needs_clear = true;
        }
        KeyCode::Char('?') => {
            let name = app_name.clone();
            let cfg = config.clone();
            let sel = *selected;
            app.screen = Screen::SettingHelp { app_name: name, config: cfg, back_selected: sel };
            app.needs_clear = true;
        }
        _ => {}
    }
}

// ── Setting helpers (shared by Config screen + OptionPicker) ─────────────────

/// The list of valid choices for the config row at `idx`.
/// Rows 0..=3 are booleans; 4 = temp_mode; 5 = temp_delete.
/// Rows 7..=10 use pickers (system / sample / input, or system / random / sample / input for machine_id).
pub fn setting_options(idx: usize) -> Vec<&'static str> {
    match idx {
        0..=3 => vec!["on", "off"],
        4 => vec!["system", "ramdisk", "local", "uuid"],
        5 => vec!["never", "on_start", "on_close"],
        CFG_SPOOF_HOSTNAME | CFG_SPOOF_USERNAME => vec!["system", "sample", "input"],
        CFG_SPOOF_OS => vec!["system", "Ubuntu", "Arch", "Windows 11", "ArduinoIDE", "input"],
        CFG_SPOOF_CPUINFO => vec!["system", "sample", "edit"],
        CFG_SPOOF_MACHINE_ID => vec!["system", "random", "sample", "input"],
        CFG_SPOOF_TERMINAL => vec!["off", "detect"],
        CFG_RAM_LIMIT => vec!["none", "512 MiB", "1 GiB", "2 GiB", "4 GiB", "8 GiB"],
        _ => vec![],
    }
}

/// Human-readable title for the popup that edits the row at `idx`.
pub fn setting_title(idx: usize) -> &'static str {
    match idx {
        0 => "Network",
        1 => "Camera",
        2 => "Microphone",
        3 => "Audio",
        4 => "Temp mode",
        5 => "Temp delete",
        6 => "Shared dirs",
        7 => "Spoof hostname",
        8 => "Spoof username",
        9 => "Spoof machine ID",
        10 => "Spoof CPU info",
        11 => "Spoof OS release",
        12 => "Spoof terminal name",
        13 => "RAM limit",
        _ => "Option",
    }
}

/// One-paragraph description of what each config row controls.
pub fn setting_description(idx: usize) -> &'static str {
    match idx {
        0 => "Allow outgoing internet access from the sandbox. Disable to run the app fully offline and prevent all network calls.",
        1 => "Allow the app to access webcam devices (/dev/video*). Disable to block camera access entirely.",
        2 => "Allow microphone access. Note: PipeWire/PulseAudio mic is only fully blocked when Audio is also disabled.",
        3 => "Allow audio playback and capture via PipeWire/PulseAudio. Disabling this also helps block microphone access through the sound server.",
        4 => "Where the app's /tmp lives: 'system' shares the host /tmp; 'ramdisk' uses an in-memory tmpfs (fast, private); 'local' uses ~/.wryayer/<app>/.tmp/; 'uuid' creates a fresh private dir on each launch.",
        5 => "When to clean up the local temp dir (local/uuid modes): 'never' keeps it between launches; 'on_start' deletes it before each launch; 'on_close' deletes it after the app exits.",
        6 => "Host directories bind-mounted read-write into the sandbox. Useful for sharing downloads, projects, or config files between the app and your system.",
        7 => "Override /etc/hostname and $HOSTNAME inside the sandbox. 'system' uses the real hostname; 'sample' sets it to 'workstation'; 'input' lets you type any custom name.",
        8 => "Override $USER and $LOGNAME inside the sandbox. 'system' uses your real username; 'sample' sets it to 'user'; 'input' lets you type any custom name.",
        9 => "Override /etc/machine-id inside the sandbox. 'system' uses the real ID; 'random' generates a fresh UUID each launch; 'sample' uses a fixed placeholder; 'input' lets you type a 32-char hex value.",
        10 => "Override /proc/cpuinfo inside the sandbox. 'system' exposes the real CPU; 'sample' shows a generic Intel i7; 'edit' opens a text editor so you can write a fully custom cpuinfo — pre-filled with your real CPU data.",
        11 => "Override /etc/os-release inside the sandbox. Choose a preset (Ubuntu, Arch, Windows 11, ArduinoIDE) or 'input' to type any custom OS name. 'system' exposes the real OS release.",
        12 => "'detect' walks the process tree to find your real terminal emulator (kitty, foot, alacritty, WezTerm, …) and sets the env var that identifies it inside the sandbox (KITTY_WINDOW_ID, $TERM, WEZTERM_PANE, …). Fixes fastfetch/neofetch showing 'bwrap' instead of your real terminal.",
        13 => "Maximum RAM the app may use (RAM + swap both capped). Enforced via systemd-run MemoryMax + MemorySwapMax=0. 'none' disables the limit. Requires systemd.",
        _ => "No description available.",
    }
}

/// Description of the specific choice `choice_idx` within the setting at `idx`.
pub fn option_description(setting_idx: usize, choice_idx: usize) -> &'static str {
    match (setting_idx, choice_idx) {
        // Network
        (0, 0) => "on — Allow outgoing network connections from the sandbox.",
        (0, 1) => "off — Block all network access; the app runs fully offline.",
        // Camera
        (1, 0) => "on — Allow the app to access webcam devices (/dev/video*).",
        (1, 1) => "off — Block access to all camera devices.",
        // Microphone
        (2, 0) => "on — Allow microphone access. For full isolation also turn Audio off.",
        (2, 1) => "off — Block microphone access via device permissions.",
        // Audio
        (3, 0) => "on — Allow audio playback and capture via PipeWire/PulseAudio.",
        (3, 1) => "off — Block PipeWire/PulseAudio sockets; also cuts off the sound-server mic path.",
        // Temp mode
        (4, 0) => "system — Use the host /tmp. Fast, but shared with the rest of the system.",
        (4, 1) => "ramdisk — Mount a private in-memory tmpfs as /tmp. Fast, fully isolated, and wiped when the app exits.",
        (4, 2) => "local — Use ~/.wryayer/<app>/.tmp/ as /tmp. Persists across launches; controlled by Temp delete.",
        (4, 3) => "uuid — Create a fresh private temp dir on each launch. Maximum isolation; nothing from prior runs is reused.",
        // Temp delete
        (5, 0) => "never — Keep the temp dir between launches. Useful for apps that cache heavy data in /tmp.",
        (5, 1) => "on_start — Delete and recreate the temp dir before each launch; always starts fresh.",
        (5, 2) => "on_close — Delete the temp dir after the app exits; cleans up automatically.",
        // Hostname
        (7, 0) => "system — Use the real hostname from the host. No spoofing.",
        (7, 1) => "sample — Set hostname to 'workstation'. Generic name that won't expose your machine.",
        (7, 2) => "input — Type a custom hostname. Saved to /etc/hostname and $HOSTNAME inside the sandbox.",
        // Username
        (8, 0) => "system — Use your real login name. No spoofing.",
        (8, 1) => "sample — Set username to 'user'. Generic name that won't expose your real login.",
        (8, 2) => "input — Type a custom username. Applied to $USER and $LOGNAME inside the sandbox.",
        // Machine ID
        (9, 0) => "system — Use the real /etc/machine-id from the host (no spoofing).",
        (9, 1) => "random — Generate a fresh 32-char hex UUID on every launch. Prevents cross-session fingerprinting.",
        (9, 2) => "sample — Use a fixed placeholder ID: cafebabe0011223344556677deadbeef. Same every run, but not your real ID.",
        (9, 3) => "input — Type your own 32-char hex machine-id. Useful for reproducing a specific identity.",
        // CPU info
        (10, 0) => "system — Expose the real /proc/cpuinfo to the app. No spoofing.",
        (10, 1) => "sample — Bind a built-in generic Intel Core i7-8550U cpuinfo. The app won't see your real CPU model.",
        (10, 2) => "edit — Open a text editor (nvim/vim/vi/nano) to write a custom /proc/cpuinfo. Pre-filled with your real CPU info on first use. Content is saved per-app.",
        // OS release
        (11, 0) => "system — Expose the real /etc/os-release to the app. No spoofing.",
        (11, 1) => "Ubuntu — Presents as Ubuntu 24.04 LTS. Apps see NAME=Ubuntu, ID=ubuntu, VERSION_ID=24.04.",
        (11, 2) => "Arch — Presents as Arch Linux. Apps see NAME=\"Arch Linux\", ID=arch, BUILD_ID=rolling.",
        (11, 3) => "Windows 11 — Presents as Windows 11. Apps see NAME=\"Windows 11\", ID=windows, VERSION_ID=11.",
        (11, 4) => "ArduinoIDE — Presents as ArduinoIDE. Apps see NAME=ArduinoIDE, ID=arduinoide, VERSION_ID=2.3.",
        (11, 5) => "input — Type a custom OS name (e.g. 'fedora'). Used as ID= and NAME= in /etc/os-release inside the sandbox.",
        // Spoof terminal
        (12, 0) => "off — Do not override terminal identity. Tools like fastfetch may show 'bwrap' as the terminal.",
        (12, 1) => "detect — Walk the process tree to find the real terminal (kitty, foot, alacritty, WezTerm, …) and set the correct env var inside the sandbox. Fixes fastfetch showing 'bwrap'.",
        // RAM limit
        (13, 0) => "none — No RAM limit. The app may use as much memory as the system allows.",
        (13, 1) => "512 MiB — Hard cap at 512 MiB (RAM + swap). Processes are OOM-killed if they exceed this.",
        (13, 2) => "1 GiB — Cap the app at 1 GiB (1024 MiB) of RAM.",
        (13, 3) => "2 GiB — Cap the app at 2 GiB (2048 MiB) of RAM. Good default for everyday apps.",
        (13, 4) => "4 GiB — Cap the app at 4 GiB (4096 MiB) of RAM.",
        (13, 5) => "8 GiB — Cap the app at 8 GiB (8192 MiB) of RAM.",
        _ => "No description available.",
    }
}

/// Which option index in `setting_options(idx)` matches the current value
/// stored in `config`.
pub fn setting_current(config: &AppConfig, idx: usize) -> usize {
    match idx {
        0 => if config.network { 0 } else { 1 },
        1 => if config.camera { 0 } else { 1 },
        2 => if config.microphone { 0 } else { 1 },
        3 => if config.audio { 0 } else { 1 },
        4 => match config.temp_mode {
            TempMode::System  => 0,
            TempMode::Ramdisk => 1,
            TempMode::Local   => 2,
            TempMode::Uuid    => 3,
        },
        5 => match config.temp_delete {
            LocalDelete::Never   => 0,
            LocalDelete::OnStart => 1,
            LocalDelete::OnClose => 2,
        },
        CFG_SPOOF_HOSTNAME => match config.spoof_hostname.as_deref() {
            None => 0,
            Some(v) if v == HOSTNAME_SAMPLE => 1,
            _ => 2,
        },
        CFG_SPOOF_USERNAME => match config.spoof_username.as_deref() {
            None => 0,
            Some(v) if v == USERNAME_SAMPLE => 1,
            _ => 2,
        },
        CFG_SPOOF_MACHINE_ID => match config.spoof_machine_id.as_deref() {
            None             => 0,
            Some("random")   => 1,
            Some(v) if v == MACHINE_ID_SAMPLE => 2,
            _                => 3,
        },
        CFG_SPOOF_CPUINFO => match config.spoof_cpuinfo.as_deref() {
            None           => 0,
            Some("sample") => 1,
            _              => 2,  // "custom" or legacy path both show as "edit"
        },
        CFG_SPOOF_OS => match config.spoof_os.as_deref() {
            None               => 0,
            Some("ubuntu")     => 1,
            Some("arch")       => 2,
            Some("windows")    => 3,
            Some("arduinoide") => 4,
            _                  => 5,
        },
        CFG_SPOOF_TERMINAL => if config.spoof_terminal { 1 } else { 0 },
        CFG_RAM_LIMIT => match config.ram_limit {
            None        => 0,
            Some(512)   => 1,
            Some(1024)  => 2,
            Some(2048)  => 3,
            Some(4096)  => 4,
            Some(8192)  => 5,
            Some(n) if n <= 512  => 1,
            Some(n) if n <= 1024 => 2,
            Some(n) if n <= 2048 => 3,
            Some(n) if n <= 4096 => 4,
            _           => 5,
        },
        _ => 0,
    }
}

/// Write the option `choice` (an index into `setting_options(idx)`) into the
/// matching config field. Out-of-range pairs are silent no-ops.
pub fn apply_setting(config: &mut AppConfig, idx: usize, choice: usize) {
    match (idx, choice) {
        (0, 0) => config.network = true,
        (0, 1) => config.network = false,
        (1, 0) => config.camera = true,
        (1, 1) => config.camera = false,
        (2, 0) => config.microphone = true,
        (2, 1) => config.microphone = false,
        (3, 0) => config.audio = true,
        (3, 1) => config.audio = false,
        (4, 0) => config.temp_mode = TempMode::System,
        (4, 1) => config.temp_mode = TempMode::Ramdisk,
        (4, 2) => config.temp_mode = TempMode::Local,
        (4, 3) => config.temp_mode = TempMode::Uuid,
        (5, 0) => config.temp_delete = LocalDelete::Never,
        (5, 1) => config.temp_delete = LocalDelete::OnStart,
        (5, 2) => config.temp_delete = LocalDelete::OnClose,
        (7, 0) => config.spoof_hostname = None,
        (7, 1) => config.spoof_hostname = Some(HOSTNAME_SAMPLE.to_string()),
        // (7, 2) = "input" — handled by on_option_picker which opens TextInput
        (8, 0) => config.spoof_username = None,
        (8, 1) => config.spoof_username = Some(USERNAME_SAMPLE.to_string()),
        // (8, 2) = "input" — handled by on_option_picker which opens TextInput
        (9, 0) => config.spoof_machine_id = None,
        (9, 1) => config.spoof_machine_id = Some("random".to_string()),
        (9, 2) => config.spoof_machine_id = Some(MACHINE_ID_SAMPLE.to_string()),
        // (9, 3) = "input" — handled by on_option_picker which opens TextInput
        (10, 0) => config.spoof_cpuinfo = None,
        (10, 1) => config.spoof_cpuinfo = Some("sample".to_string()),
        // (10, 2) = "edit" — handled by on_option_picker which opens editor
        (11, 0) => config.spoof_os = None,
        (11, 1) => config.spoof_os = Some("ubuntu".to_string()),
        (11, 2) => config.spoof_os = Some("arch".to_string()),
        (11, 3) => config.spoof_os = Some("windows".to_string()),
        (11, 4) => config.spoof_os = Some("arduinoide".to_string()),
        // (11, 5) = "input" — handled by on_option_picker which opens TextInput
        (12, 0) => config.spoof_terminal = false,
        (12, 1) => config.spoof_terminal = true,
        (13, 0) => config.ram_limit = None,
        (13, 1) => config.ram_limit = Some(512),
        (13, 2) => config.ram_limit = Some(1024),
        (13, 3) => config.ram_limit = Some(2048),
        (13, 4) => config.ram_limit = Some(4096),
        (13, 5) => config.ram_limit = Some(8192),
        _ => {}
    }
}

/// Cycle the setting at `idx` forward (`dir == 1`) or backward (`dir == -1`).
/// Wraps at the ends of the option list.
pub fn cycle_setting(config: &mut AppConfig, idx: usize, dir: i32) {
    let n = setting_options(idx).len();
    if n == 0 { return; }
    let cur = setting_current(config, idx);
    let next = if dir > 0 {
        (cur + 1) % n
    } else {
        (cur + n - 1) % n
    };
    apply_setting(config, idx, next);
}

// ── Option picker overlay ─────────────────────────────────────────────────────

fn on_option_picker(app: &mut App, code: KeyCode) {
    let Screen::OptionPicker { app_name, config, setting_idx, selected } = &mut app.screen else { return };
    let n = setting_options(*setting_idx).len();
    if n == 0 {
        // Pathological: pop back to config
        let name = app_name.clone();
        let cfg = config.clone();
        let idx = *setting_idx;
        app.screen = Screen::Config { app_name: name, config: cfg, selected: idx };
        app.needs_clear = true;
        return;
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Discard the picker choice, return to Config unchanged.
            let name = app_name.clone();
            let cfg = config.clone();
            let idx = *setting_idx;
            app.screen = Screen::Config { app_name: name, config: cfg, selected: idx };
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = if *selected == 0 { n - 1 } else { *selected - 1 };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1) % n;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let name = app_name.clone();
            let mut cfg = config.clone();
            let idx = *setting_idx;
            let choice = *selected;
            // cpuinfo "edit" → tear down TUI, open editor, save content.
            if idx == CFG_SPOOF_CPUINFO && choice == 2 {
                let name2 = name.clone();
                let cpuinfo_file = crate::manifest::app_dir(&name)
                    .map(|d| d.join(".spoof").join("cpuinfo"));
                app.screen = Screen::Main;
                app.needs_clear = true;
                match cpuinfo_file {
                    Ok(path) => { app.editor_request = Some((name2, path)); }
                    Err(_) => { app.status = "error: cannot find app directory".to_string(); }
                }
                return;
            }

            // "input" option opens the free-text overlay for hostname/username/machine-id/os.
            let is_input_choice = match idx {
                CFG_SPOOF_HOSTNAME | CFG_SPOOF_USERNAME => choice == 2,
                CFG_SPOOF_OS => choice == 5,
                CFG_SPOOF_MACHINE_ID => choice == 3,
                _ => false,
            };
            if is_input_choice {
                let current = match idx {
                    CFG_SPOOF_HOSTNAME   => cfg.spoof_hostname.clone().unwrap_or_default(),
                    CFG_SPOOF_USERNAME   => cfg.spoof_username.clone().unwrap_or_default(),
                    CFG_SPOOF_MACHINE_ID => cfg.spoof_machine_id.clone().unwrap_or_default(),
                    CFG_SPOOF_OS         => cfg.spoof_os.clone().unwrap_or_default(),
                    _ => String::new(),
                };
                // Clear pre-fill when current value is one of the fixed presets.
                let is_preset = match idx {
                    CFG_SPOOF_HOSTNAME   => current == HOSTNAME_SAMPLE,
                    CFG_SPOOF_USERNAME   => current == USERNAME_SAMPLE,
                    CFG_SPOOF_MACHINE_ID => current == "random" || current == MACHINE_ID_SAMPLE,
                    CFG_SPOOF_OS         => matches!(current.as_str(), "ubuntu" | "arch" | "windows" | "arduinoide"),
                    _ => false,
                };
                let value = if is_preset || current.is_empty() { String::new() } else { current };
                app.screen = Screen::TextInput {
                    app_name: name,
                    config: cfg,
                    back_selected: idx,
                    field_idx: idx,
                    value,
                };
                app.needs_clear = true;
                return;
            }
            apply_setting(&mut cfg, idx, choice);
            app.screen = Screen::Config { app_name: name, config: cfg, selected: idx };
            app.needs_clear = true;
        }
        KeyCode::Char('?') => {
            let name = app_name.clone();
            let cfg = config.clone();
            let idx = *setting_idx;
            let sel = *selected;
            app.screen = Screen::OptionHelp {
                app_name: name,
                config: cfg,
                setting_idx: idx,
                picker_selected: sel,
            };
            app.needs_clear = true;
        }
        _ => {}
    }
}

// ── Option help popup ─────────────────────────────────────────────────────────

fn on_option_help(app: &mut App, _code: KeyCode) {
    let Screen::OptionHelp { app_name, config, setting_idx, picker_selected } = &mut app.screen else { return };
    let name = app_name.clone();
    let cfg = config.clone();
    let idx = *setting_idx;
    let sel = *picker_selected;
    app.screen = Screen::OptionPicker {
        app_name: name,
        config: cfg,
        setting_idx: idx,
        selected: sel,
    };
    app.needs_clear = true;
}

// ── Setting help popup ────────────────────────────────────────────────────────

fn on_setting_help(app: &mut App, _code: KeyCode) {
    let Screen::SettingHelp { app_name, config, back_selected } = &mut app.screen else { return };
    let name = app_name.clone();
    let cfg = config.clone();
    let sel = *back_selected;
    app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
    app.needs_clear = true;
}

// ── Text input overlay (spoof settings) ──────────────────────────────────────

fn set_spoof_field(config: &mut AppConfig, idx: usize, value: String) {
    let v = if value.is_empty() { None } else { Some(value) };
    match idx {
        CFG_SPOOF_HOSTNAME   => config.spoof_hostname   = v,
        CFG_SPOOF_USERNAME   => config.spoof_username   = v,
        CFG_SPOOF_MACHINE_ID => config.spoof_machine_id = v,
        CFG_SPOOF_CPUINFO    => config.spoof_cpuinfo    = v,
        CFG_SPOOF_OS         => config.spoof_os         = v,
        _ => {}
    }
}

fn on_text_input(app: &mut App, code: KeyCode) {
    let Screen::TextInput { app_name, config, back_selected, field_idx, value } = &mut app.screen
    else {
        return;
    };
    match code {
        KeyCode::Esc => {
            let name = app_name.clone();
            let cfg = config.clone();
            let sel = *back_selected;
            app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let name = app_name.clone();
            let mut cfg = config.clone();
            let sel = *back_selected;
            let idx = *field_idx;
            let v = value.trim().to_string();
            set_spoof_field(&mut cfg, idx, v);
            app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
            app.needs_clear = true;
        }
        KeyCode::Backspace => {
            value.pop();
        }
        KeyCode::Char(c) => {
            value.push(c);
        }
        _ => {}
    }
}

// ── Space tab ─────────────────────────────────────────────────────────────────

fn on_space_tab(app: &mut App, code: KeyCode) {
    if code == KeyCode::Char('r') {
        launch_op(app, "Dedup".to_string(), vec!["dedup".to_string()], None, true);
    }
}

// ── Shared dirs screen ────────────────────────────────────────────────────────

fn on_shared_dirs(app: &mut App, code: KeyCode) {
    let Screen::SharedDirs { app_name, dirs, selected } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            let name = app_name.clone();
            let config = read_config(&name).unwrap_or_default();
            app.screen = Screen::Config { app_name: name, config, selected: CFG_SHARES };
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !dirs.is_empty() {
                *selected = (*selected + 1).min(dirs.len() - 1);
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if !dirs.is_empty() {
                let name = app_name.clone();
                let idx = *selected;
                dirs.remove(idx);
                if !dirs.is_empty() && idx >= dirs.len() {
                    *selected = dirs.len() - 1;
                }
                let mut config = read_config(&name).unwrap_or_default();
                config.shared_dirs = dirs.clone();
                let _ = write_config(&name, &config);
            }
        }
        KeyCode::Char('a') => {
            let name = app_name.clone();
            open_file_browser(app, Some(name));
        }
        _ => {}
    }
}

// ── File browser ──────────────────────────────────────────────────────────────

fn open_file_browser(app: &mut App, pick_dir_for: Option<String>) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let dir = PathBuf::from(home);
    let entries = load_dir_entries(&dir);
    let mut fb_state = ListState::default();
    if !entries.is_empty() {
        fb_state.select(Some(0));
    }
    app.screen = Screen::FileBrowser { current_dir: dir, entries, fb_state, pick_dir_for };
}

fn load_dir_entries(dir: &PathBuf) -> Vec<FbEntry> {
    let mut entries = vec![];
    let Ok(rd) = std::fs::read_dir(dir) else { return entries };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') { continue; }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_zip = name.ends_with(".zip");
        entries.push(FbEntry { name, is_dir, is_zip });
    }
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    entries
}

enum FbAction {
    Nothing,
    EnterDir(PathBuf),
    SelectFile(String),
    /// Pick-dir mode: user selected `path` for `app_name`
    SelectDir { path: PathBuf, app_name: String },
    GoUp,
    /// None = return to Import tab; Some(app_name) = return to SharedDirs
    Close(Option<String>),
}

fn on_file_browser(app: &mut App, code: KeyCode) {
    let action = {
        let Screen::FileBrowser { current_dir, entries, fb_state, pick_dir_for } = &mut app.screen else { return };
        let pick = pick_dir_for.clone();
        match code {
            KeyCode::Esc | KeyCode::Char('q') => FbAction::Close(pick),
            // Space / s selects the current directory in pick-dir mode
            KeyCode::Char(' ') | KeyCode::Char('s') if pick.is_some() => {
                FbAction::SelectDir { path: current_dir.clone(), app_name: pick.unwrap() }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = fb_state.selected().unwrap_or(0);
                fb_state.select(Some(i.saturating_sub(1)));
                FbAction::Nothing
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = fb_state.selected().unwrap_or(0);
                fb_state.select(Some((i + 1).min(entries.len().saturating_sub(1))));
                FbAction::Nothing
            }
            KeyCode::Left | KeyCode::Backspace => {
                if current_dir.parent().is_some() { FbAction::GoUp } else { FbAction::Nothing }
            }
            KeyCode::Enter | KeyCode::Right => {
                let i = fb_state.selected().unwrap_or(0);
                if let Some(entry) = entries.get(i) {
                    if entry.is_dir {
                        FbAction::EnterDir(current_dir.join(&entry.name))
                    } else if entry.is_zip && pick.is_none() {
                        FbAction::SelectFile(
                            current_dir.join(&entry.name).to_string_lossy().into_owned(),
                        )
                    } else {
                        FbAction::Nothing
                    }
                } else {
                    FbAction::Nothing
                }
            }
            _ => FbAction::Nothing,
        }
    };

    match action {
        FbAction::Nothing => {}
        FbAction::Close(None) => {
            app.screen = Screen::Main;
            app.tab = Tab::Import;
            app.needs_clear = true;
        }
        FbAction::Close(Some(app_name)) => {
            let config = read_config(&app_name).unwrap_or_default();
            let dirs = config.shared_dirs;
            let selected = dirs.len().saturating_sub(1);
            app.screen = Screen::SharedDirs { app_name, dirs, selected };
            app.needs_clear = true;
        }
        FbAction::GoUp => {
            if let Screen::FileBrowser { current_dir, entries, fb_state, .. } = &mut app.screen {
                if let Some(parent) = current_dir.parent().map(|p| p.to_path_buf()) {
                    *entries = load_dir_entries(&parent);
                    *current_dir = parent;
                    fb_state.select(if entries.is_empty() { None } else { Some(0) });
                }
            }
        }
        FbAction::EnterDir(new_dir) => {
            if let Screen::FileBrowser { current_dir, entries, fb_state, .. } = &mut app.screen {
                let new_entries = load_dir_entries(&new_dir);
                *current_dir = new_dir;
                *entries = new_entries;
                fb_state.select(if entries.is_empty() { None } else { Some(0) });
            }
        }
        FbAction::SelectFile(path) => {
            app.import_input = path;
            app.screen = Screen::Main;
            app.tab = Tab::Import;
        }
        FbAction::SelectDir { path, app_name } => {
            let path_str = path.to_string_lossy().into_owned();
            let mut config = read_config(&app_name).unwrap_or_default();
            if !config.shared_dirs.contains(&path_str) {
                config.shared_dirs.push(path_str);
                let _ = write_config(&app_name, &config);
            }
            let dirs = config.shared_dirs;
            let selected = dirs.len().saturating_sub(1);
            app.screen = Screen::SharedDirs { app_name, dirs, selected };
            app.needs_clear = true;
        }
    }
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn launch_op(app: &mut App, title: String, args: Vec<String>, total_bytes: Option<u64>, reload: bool) {
    let into_target = args.windows(2)
        .find(|w| w[0] == "--into")
        .map(|w| w[1].clone());
    let (tx, rx) = mpsc::channel();
    let original_args = args.clone();
    spawn_wryayer(args, tx);
    app.log_scroll = 0;
    app.screen = Screen::Operation {
        title,
        log: vec![],
        done: false,
        success: false,
        rx,
        total_bytes,
        progress: None,
        started: Instant::now(),
        reload,
        show_log: false,
        launcher_choice: None,
        into_target,
        outdated_pkg: None,
        original_args,
    };
}

/// Parse `PROGRESS <done>/<total>` lines emitted by subprocess commands.
pub fn parse_progress(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("PROGRESS ")?;
    let (a, b) = rest.split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// The canonical konami code: ↑↑↓↓←→←→BA.
pub const KONAMI: &[KeyCode] = &[
    KeyCode::Up, KeyCode::Up,
    KeyCode::Down, KeyCode::Down,
    KeyCode::Left, KeyCode::Right,
    KeyCode::Left, KeyCode::Right,
    KeyCode::Char('b'), KeyCode::Char('a'),
];

/// Advance the konami detection FSM by one keystroke. Returns true iff the
/// code was just completed on this step (used by tests).
pub fn konami_advance(state: &mut usize, code: KeyCode) -> bool {
    let expected = KONAMI[*state];
    // Match — including case-insensitive char compare so SHIFT doesn't break it.
    let matches = match (expected, code) {
        (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
        (a, b) => a == b,
    };
    if matches {
        *state += 1;
        if *state == KONAMI.len() {
            *state = 0;
            return true;
        }
    } else {
        // Allow the failed key to be the start of a new attempt
        *state = if code == KONAMI[0] { 1 } else { 0 };
    }
    false
}

fn konami_step(app: &mut App, code: KeyCode) {
    if konami_advance(&mut app.konami_state, code) {
        app.konami_mode = !app.konami_mode;
        app.status = konami_status_for_toggle(app.konami_mode);
    }
}

/// What to write into `app.status` after a konami toggle. The statusbar
/// already renders a dedicated `★ konami mode` chip from `app.konami_mode`,
/// so mirroring the same text into `app.status` would double-render. We only
/// emit a status string when toggling OFF, since the chip is gone by then.
pub fn konami_status_for_toggle(now_active: bool) -> String {
    if now_active {
        String::new()
    } else {
        "konami mode off".to_string()
    }
}

fn spawn_wryayer(args: Vec<String>, tx: mpsc::Sender<Msg>) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    thread::spawn(move || {
        let mut child = match Command::new(&exe)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Msg::Line(format!("error: {e}")));
                let _ = tx.send(Msg::Done(false));
                return;
            }
        };

        let stderr = child.stderr.take().unwrap();
        let tx2 = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().flatten() {
                let _ = tx2.send(Msg::Line(line));
            }
        });

        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().flatten() {
                let _ = tx.send(Msg::Line(line));
            }
        }

        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        let _ = tx.send(Msg::Done(ok));
    });
}

fn dir_bytes(path: &str) -> Option<u64> {
    let out = Command::new("du").args(["-sb", path]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.split_whitespace().next()?.parse().ok()
}
