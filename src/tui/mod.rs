pub mod konami;
mod ui;

use std::collections::{HashMap, HashSet, VecDeque};
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
use crate::config::{read_config, read_global_config, write_config, write_global_config, AppConfig, LocalDelete, TempMode};
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
        mode: BrowserMode,
    },
    /// Wine-game wizard: pick the main .exe inside the selected folder.
    GameExePick {
        game_dir: PathBuf,
        exes: Vec<(String, u64)>,
        selected: usize,
    },
    /// Wine-game wizard: type an app name (defaults to sanitized folder name).
    /// The name doubles as the container directory under ~/.wryayer/.
    GameNameInput {
        game_dir: PathBuf,
        exe: String,
        value: String,
    },
    /// Wine-game wizard: confirm install + ask whether to delete the source folder.
    GameConfirm {
        game_dir: PathBuf,
        exe: String,
        app_name: String,
        delete_source: bool,
        selected: usize, // 0 = install, 1 = toggle delete, 2 = cancel
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
    /// Free-text input overlay for spoof string settings and wine_game fields.
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
        /// None = fresh container; Some(target) = merge into target app.
        into: Option<String>,
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
    /// Ask whether to create a ~/bin/<pkg> shortcut before starting the install.
    AskShortcut {
        pkg: String,
        title: String,
        /// Install args ready to pass to spawn_wryayer (without --keep-without-launcher).
        args: Vec<String>,
        selected: usize, // 0 = yes (create shortcut), 1 = no (skip)
    },
}

pub enum PendingAction {
    Remove(String),
    ConfirmedRemove(String),
    /// Remove a target app together with all aliases that point at it.
    RemoveCascade(String, Vec<String>),
    ConfirmedRemoveCascade(String),
    Update(String),
    UpdateAll,
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
    Games,
    Space,
    Settings,
}

/// Where the file browser hands its picked path. Lets one browser handle
/// .zip imports, shared-dir picking, and wine-game folder picking.
#[derive(Clone)]
pub enum BrowserMode {
    /// Pick a .zip file for `wryayer import`.
    ImportZip,
    /// Pick a directory and add it to `app_name`'s shared_dirs.
    PickShareDir(String),
    /// Pick a directory to import as a wine game.
    PickGameDir,
}

// ── App state ─────────────────────────────────────────────────────────────────

pub struct App {
    pub quit: bool,
    pub tab: Tab,
    // Installed tab
    pub installed: Vec<Manifest>,
    pub inst_state: ListState,
    pub update_available: HashMap<String, String>,
    /// Async update check, streamed in from a background thread on start and
    /// after reloads so the list dots appear without blocking the UI.
    pub update_tx: Sender<HashMap<String, String>>,
    pub update_rx: Receiver<HashMap<String, String>>,
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
    // Settings tab (global defaults)
    pub global_config: AppConfig,
    pub global_selected: usize,
    // Disk usage (computed once on load, refreshed on reload)
    pub app_sizes: HashMap<String, u64>,
    pub du_apparent: u64,
    pub du_actual: u64,
    // Running-instance counts per app, keyed by filesystem-root name.
    // Refreshed on a throttle from the event loop (scanning /proc is not free).
    pub running_instances: HashMap<String, usize>,
    pub last_instance_scan: Instant,
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
    // ── Detail panel ─────────────────────────────────────────────────────────
    pub detail_focused: bool,
    pub detail_scroll: usize,
    // ── Install multi-select ──────────────────────────────────────────────────
    pub selected_pkgs: HashSet<String>,
    pub install_queue: VecDeque<String>,
    // Games tab selection
    pub games_state: ListState,
    /// Stores in-progress wine-game (exe, prefix) edits while the Config screen
    /// is open for a wine game. Set when [s] opens Config; cleared when Config
    /// is exited (Save commits it to the manifest; Esc discards).
    /// Lives on App so it survives transitions to OptionPicker / SharedDirs /
    /// SettingHelp / TextInput without having to be threaded through every
    /// Screen variant.
    pub editing_wine_game: Option<(String, String)>,
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
        let (update_tx, update_rx) = mpsc::channel();
        spawn_update_check(update_tx.clone());
        Ok(Self {
            quit: false,
            tab: Tab::Installed,
            installed,
            inst_state,
            update_available: HashMap::new(),
            update_tx,
            update_rx,
            search_input: String::new(),
            search_results: Vec::new(),
            search_searching: false,
            search_gen: 0,
            search_tx,
            search_rx,
            avail_state: ListState::default(),
            search_list_focused: false,
            import_input: String::new(),
            global_config: read_global_config(),
            global_selected: 0,
            app_sizes,
            du_apparent,
            du_actual,
            running_instances: scan_running_instances(),
            last_instance_scan: Instant::now(),
            screen: Screen::Main,
            status: String::new(),
            log_scroll: 0,
            needs_clear: false,
            run_request: None,
            editor_request: None,
            konami_mode: false,
            konami_state: 0,
            detail_focused: false,
            detail_scroll: 0,
            selected_pkgs: HashSet::new(),
            install_queue: VecDeque::new(),
            games_state: ListState::default(),
            editing_wine_game: None,
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
            // Clamp games_state to the current games list.
            let games_count = self.installed.iter().filter(|m| m.app.wine_game.is_some()).count();
            if games_count == 0 {
                self.games_state.select(None);
            } else {
                let gs = self.games_state.selected().unwrap_or(0);
                self.games_state.select(Some(gs.min(games_count - 1)));
            }
        }
        if let Ok((sizes, apparent, actual)) = all_du() {
            self.app_sizes = sizes;
            self.du_apparent = apparent;
            self.du_actual = actual;
        }
        self.running_instances = scan_running_instances();
        self.last_instance_scan = Instant::now();
        // Package versions may have changed (install/update/remove); re-check so
        // stale update dots clear and new ones appear.
        self.update_available.clear();
        spawn_update_check(self.update_tx.clone());
    }

    /// Re-scan /proc for running sandboxes at most once per second so the
    /// instance counts in the list stay live without hammering /proc on every
    /// 50 ms redraw.
    fn refresh_running_instances(&mut self) {
        if self.last_instance_scan.elapsed() >= Duration::from_secs(1) {
            self.running_instances = scan_running_instances();
            self.last_instance_scan = Instant::now();
        }
    }

    pub fn selected_installed(&self) -> Option<&Manifest> {
        self.inst_state.selected().and_then(|i| self.installed.get(i))
    }

    /// Filtered list of wine-game manifests, in `installed` order.
    pub fn games(&self) -> Vec<&Manifest> {
        self.installed.iter().filter(|m| m.app.wine_game.is_some()).collect()
    }

    pub fn selected_game(&self) -> Option<&Manifest> {
        let games = self.games();
        self.games_state.selected().and_then(|i| games.get(i).copied())
    }

    pub fn selected_available(&self) -> Option<&str> {
        self.avail_state
            .selected()
            .and_then(|i| self.search_results.get(i))
            .map(|(name, _)| name.as_str())
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Count running sandboxes per app by scanning /proc for the bwrap monitor
/// process of each launch.  Every wryayer sandbox runs `bwrap --bind <app_root>
/// / …`, and only the outer monitor keeps that argv (the inner process is
/// exec'd into the app), so one match == one running instance.
///
/// The result is keyed by `app.name`.  Programs installed with `--into <parent>`
/// share the parent's sandbox root, so the `--bind <root> /` triple only names
/// the parent; counting by root alone would attribute a running child to its
/// parent and show the same total on both rows.  We instead disambiguate by the
/// binary the sandbox is actually running (`bwrap … -- <binary> …`), which is
/// each program's own `main_binary`, and fall back to the root's own app when a
/// launch used a binary that matches no manifest (e.g. a `run --bin` override).
fn scan_running_instances() -> HashMap<String, usize> {
    let mut map: HashMap<String, usize> = HashMap::new();
    let Ok(root) = crate::manifest::wryayer_root() else { return map };
    let root_prefix = format!("{}/", root.to_string_lossy());

    // (fs_root dir name, main-binary basename) -> app.name, plus the non-alias
    // owner of each root as the fallback attribution.
    let manifests = crate::manifest::list_all_apps().unwrap_or_default();
    let mut by_bin: HashMap<(String, String), String> = HashMap::new();
    let mut root_owner: HashMap<String, String> = HashMap::new();
    for m in &manifests {
        let fs_root = m.app.alias_of.clone().unwrap_or_else(|| m.app.name.clone());
        let bin = std::path::Path::new(&m.app.main_binary)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(m.app.main_binary.as_str())
            .to_string();
        by_bin.insert((fs_root.clone(), bin), m.app.name.clone());
        if m.app.alias_of.is_none() {
            root_owner.insert(fs_root, m.app.name.clone());
        }
    }

    let Ok(entries) = std::fs::read_dir("/proc") else { return map };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        // Only numeric PID directories.
        if !fname.to_str().map(|s| s.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false) {
            continue;
        }
        // Count only the actual bwrap monitor.  When ram_limit is set the launch
        // is `systemd-run … bwrap --bind <root> / …`, so systemd-run's cmdline
        // carries the same triple; filtering by the real executable (bwrap is
        // exec'd through a symlink, but /proc/<pid>/exe still resolves to it)
        // avoids double-counting that wrapper.
        let is_bwrap = std::fs::read_link(entry.path().join("exe"))
            .ok()
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|n| n == "bwrap")
            .unwrap_or(false);
        if !is_bwrap {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let args: Vec<&str> = raw
            .split(|&b| b == 0)
            .filter_map(|s| std::str::from_utf8(s).ok())
            .filter(|s| !s.is_empty())
            .collect();

        // The binary bwrap execs sits right after the `--` command separator.
        let run_bin = args
            .iter()
            .position(|&a| a == "--")
            .and_then(|i| args.get(i + 1))
            .and_then(|p| std::path::Path::new(p).file_name())
            .and_then(|n| n.to_str());

        // Look for the `--bind <app_root> /` triple that mounts the sandbox root.
        for w in args.windows(3) {
            if w[0] == "--bind" && w[2] == "/" && w[1].starts_with(&root_prefix) {
                if let Some(rest) = w[1].strip_prefix(&root_prefix) {
                    let fs_root = rest.split('/').next().unwrap_or("");
                    if !fs_root.is_empty() {
                        // Attribute to the program whose main_binary is running in
                        // this shared root; fall back to the root's own app.
                        let key = run_bin
                            .and_then(|b| by_bin.get(&(fs_root.to_string(), b.to_string())).cloned())
                            .or_else(|| root_owner.get(fs_root).cloned())
                            .unwrap_or_else(|| fs_root.to_string());
                        *map.entry(key).or_insert(0) += 1;
                    }
                }
                break;
            }
        }
    }
    map
}

/// Kick off an update check on a background thread; the result map is sent back
/// over `tx` and drained into `App::update_available` by the event loop.
fn spawn_update_check(tx: Sender<HashMap<String, String>>) {
    thread::spawn(move || {
        let _ = tx.send(crate::commands::update::check_all_updates());
    });
}

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

        // Drain async update-check results
        while let Ok(map) = app.update_rx.try_recv() {
            app.update_available = map;
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

        app.refresh_running_instances();
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

    // If the editor ran and the file exists, update config and return to the right screen.
    if app_name.is_empty() {
        // Global settings: update in-memory global config, return to Settings tab.
        if edited && cpuinfo_path.exists() {
            app.global_config.spoof_cpuinfo = Some("custom".to_string());
            let _ = crate::config::write_global_config(&app.global_config);
            app.status = "CPU info saved.".to_string();
        } else if chosen.is_none() {
            app.status = "No editor available — install nvim, vim, vi, or nano.".to_string();
        }
        app.global_selected = CFG_SPOOF_CPUINFO;
        app.tab = Tab::Settings;
        app.screen = Screen::Main;
    } else if edited && cpuinfo_path.exists() {
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
        Screen::AskShortcut { .. } => 18,
        Screen::GameExePick { .. } => 19,
        Screen::GameNameInput { .. } => 20,
        Screen::GameConfirm { .. } => 21,
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
        18 => on_ask_shortcut(app, code),
        19 => on_game_exe_pick(app, code),
        20 => on_game_name_input(app, code),
        21 => on_game_confirm(app, code),
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
                Tab::Install   => Tab::Import,
                Tab::Import    => Tab::Games,
                Tab::Games     => Tab::Space,
                Tab::Space     => Tab::Settings,
                Tab::Settings  => Tab::Installed,
            };
            app.status.clear();
            return Ok(());
        }
        KeyCode::BackTab => {
            app.tab = match app.tab {
                Tab::Installed => Tab::Settings,
                Tab::Install   => Tab::Installed,
                Tab::Import    => Tab::Install,
                Tab::Games     => Tab::Import,
                Tab::Space     => Tab::Games,
                Tab::Settings  => Tab::Space,
            };
            app.status.clear();
            return Ok(());
        }
        _ => {}
    }

    // Esc while the detail panel is focused: exit detail mode, don't quit.
    if code == KeyCode::Esc && matches!(app.tab, Tab::Installed) && app.detail_focused {
        app.detail_focused = false;
        return Ok(());
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
        Tab::Games     => on_games(app, code),
        Tab::Space     => on_space_tab(app, code),
        Tab::Settings  => on_settings_tab(app, code),
    }
    Ok(())
}

// ── Installed tab ─────────────────────────────────────────────────────────────

fn on_installed(app: &mut App, code: KeyCode) {
    let len = app.installed.len();
    if len == 0 {
        return;
    }

    // Detail panel focus mode: scroll up/down, Left/h exits
    if app.detail_focused {
        match code {
            KeyCode::Left | KeyCode::Char('h') => { app.detail_focused = false; }
            KeyCode::Up | KeyCode::Char('k') => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.detail_scroll += 1; // clamped in draw_detail
            }
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Right | KeyCode::Char('l') => {
            if app.selected_installed().is_some() {
                app.detail_focused = true;
                app.detail_scroll = 0;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = app.inst_state.selected().unwrap_or(0);
            app.inst_state.select(Some(if i == 0 { len - 1 } else { i - 1 }));
            app.detail_scroll = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let i = app.inst_state.selected().unwrap_or(0);
            app.inst_state.select(Some((i + 1) % len));
            app.detail_scroll = 0;
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
        KeyCode::Char('U') => {
            let mut names: Vec<&String> = app.update_available.keys().collect();
            names.sort();
            let body = if names.is_empty() {
                vec![
                    "No updates detected from the last check.".into(),
                    String::new(),
                    "Press y to update all apps anyway, n or Esc to cancel.".into(),
                ]
            } else {
                let mut b = vec![format!("{} app(s) with updates:", names.len())];
                for n in &names {
                    b.push(format!("  • {n}"));
                }
                b.push(String::new());
                b.push("Press y to update all, n or Esc to cancel.".into());
                b
            };
            app.screen = Screen::Confirm {
                title: "Update all apps?".into(),
                body,
                action: PendingAction::UpdateAll,
                danger: false,
            };
            app.needs_clear = true;
        }
        KeyCode::Char('s') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let config = read_config(&name).unwrap_or_default();
                app.editing_wine_game = m.app.wine_game.as_ref()
                    .map(|w| (w.exe.clone(), w.prefix.clone()));
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
    let Screen::DuplicateInstall { pkg, value, into } = &mut app.screen else { return };
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
            let into = into.clone();
            let body = match &into {
                None => vec![
                    format!("Creates ~/.wryayer/{new_name}/ alongside the existing '{pkg}'."),
                    String::new(),
                    "Press y to confirm, n or Esc to cancel.".into(),
                ],
                Some(target) => vec![
                    format!("Merges '{pkg}' into ~/.wryayer/{target}/ as alias '{new_name}'."),
                    String::new(),
                    "Press y to confirm, n or Esc to cancel.".into(),
                ],
            };
            install_confirm(
                app,
                format!("Install '{pkg}' as '{new_name}'?"),
                body,
                PendingAction::Install { pkg, app_name: Some(new_name), into },
            );
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
                    // Install a second copy — show the same target picker as a
                    // first-time install so the user can choose fresh vs merge.
                    let targets: Vec<String> = app.installed
                        .iter()
                        .filter(|m| m.app.alias_of.is_none())
                        .map(|m| m.app.name.clone())
                        .collect();
                    app.screen = Screen::InstallTarget { pkg, targets, selected: 0 };
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
            KeyCode::Enter if !app.selected_pkgs.is_empty() => {
                enqueue_marked(app);
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
            KeyCode::Char(' ') => {
                if let Some(i) = app.avail_state.selected() {
                    if let Some((pkg, _)) = app.search_results.get(i) {
                        let is_installed = app.installed.iter().any(|m| m.app.name == *pkg);
                        if !is_installed {
                            let pkg = pkg.clone();
                            if app.selected_pkgs.contains(&pkg) {
                                app.selected_pkgs.remove(&pkg);
                            } else {
                                app.selected_pkgs.insert(pkg);
                            }
                        }
                    }
                }
            }
            KeyCode::Enter => {
                if !app.selected_pkgs.is_empty() {
                    enqueue_marked(app);
                } else if let Some(pkg) = app.selected_available() {
                    let pkg = pkg.to_string();
                    if app.installed.iter().any(|m| m.app.name == pkg) {
                        // Already installed — let the user choose: install again or uninstall.
                        app.screen = Screen::AlreadyInstalled { pkg, selected: 0 };
                        app.needs_clear = true;
                    } else {
                        let targets: Vec<String> = app.installed
                            .iter()
                            .filter(|m| m.app.alias_of.is_none())
                            .map(|m| m.app.name.clone())
                            .collect();
                        if targets.is_empty() {
                            install_confirm(
                                app,
                                format!("Install '{pkg}'?"),
                                vec![
                                    format!("Installs {pkg} into ~/.wryayer/{pkg}/"),
                                    String::new(),
                                    "Press y to confirm, n or Esc to cancel.".into(),
                                ],
                                PendingAction::Install { pkg, app_name: None, into: None },
                            );
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
            app.install_queue.clear(); // cancel any pending queue
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
            // If the default name (pkg) is already taken as an installed app,
            // the user must pick a unique alias name before we can confirm.
            let name_taken = app.installed.iter().any(|m| m.app.name == pkg);
            if name_taken {
                app.screen = Screen::DuplicateInstall {
                    pkg,
                    value: String::new(),
                    into,
                };
                app.needs_clear = true;
                return;
            }
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
            install_confirm(app, title, body, PendingAction::Install { pkg, app_name: None, into });
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
            open_file_browser(app, BrowserMode::ImportZip);
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
            app.install_queue.clear(); // cancel any pending queue
            app.screen = Screen::Main;
        }
        _ => {}
    }
    Ok(())
}

/// Show the "Install '<pkg>'?" confirmation, or — when the user turned
/// confirm_install off in the global settings — skip it and start the install
/// straight away.
fn install_confirm(app: &mut App, title: String, body: Vec<String>, action: PendingAction) {
    if app.global_config.confirm_install {
        app.screen = Screen::Confirm { title, body, action, danger: false };
    } else {
        execute_action(app, action);
    }
    app.needs_clear = true;
}

/// Ask whether to create a ~/bin shortcut, or — when ask_shortcut is off —
/// launch the install immediately using the create_shortcut default.
fn ask_shortcut_or_launch(app: &mut App, pkg: String, title: String, args: Vec<String>) {
    if app.global_config.ask_shortcut {
        app.screen = Screen::AskShortcut {
            pkg,
            title,
            args,
            selected: if app.global_config.create_shortcut { 0 } else { 1 },
        };
        app.needs_clear = true;
    } else {
        let mut args = args;
        if !app.global_config.create_shortcut {
            args.push("--keep-without-launcher".into());
        }
        launch_op(app, title, args, None, true);
    }
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
        PendingAction::UpdateAll =>
            launch_op(app, "Update all apps".into(), vec!["update".into()], None, true),
        PendingAction::Install { pkg, app_name: None, into: None } => {
            let title = format!("Install — {pkg}");
            let args = vec!["install".into(), pkg.clone()];
            ask_shortcut_or_launch(app, pkg, title, args);
        }
        PendingAction::Install { pkg, app_name: Some(an), into: None } => {
            let title = format!("Install — {pkg} as {an}");
            let args = vec!["install".into(), pkg.clone(), "--app-name".into(), an];
            ask_shortcut_or_launch(app, pkg, title, args);
        }
        PendingAction::Install { pkg, app_name, into: Some(target) } => {
            let mut args = vec!["install".into(), pkg.clone(), "--into".into(), target.clone()];
            if let Some(an) = app_name { args.extend(["--app-name".into(), an]); }
            let title = format!("Install — {pkg} → {target}");
            ask_shortcut_or_launch(app, pkg, title, args);
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
            if !app.install_queue.is_empty() {
                process_install_queue(app);
            }
        }
        _ => {}
    }
    Ok(())
}

// ── Config screen ─────────────────────────────────────────────────────────────

// Rows: 0=network 1=camera 2=microphone 3=audio 4=temp_mode 5=temp_delete 6=shared_dirs
//       7=spoof_hostname 8=spoof_username 9=spoof_machine_id 10=spoof_cpuinfo 11=spoof_os
//       12=spoof_terminal 13=ram_limit 14=spoof_resolution
// Per-app Config (no wine_game):  15=Save
// Per-app Config (wine_game):     15=game_exe 16=game_prefix 17=Save
// Global Settings:                15=create_shortcut 16=confirm_install 17=ask_shortcut 18=Save
pub const CFG_SHARES: usize = 6;
pub const CFG_SPOOF_HOSTNAME: usize = 7;
pub const CFG_SPOOF_USERNAME: usize = 8;
pub const CFG_SPOOF_MACHINE_ID: usize = 9;
pub const CFG_SPOOF_CPUINFO: usize = 10;
pub const CFG_SPOOF_OS: usize = 11;
pub const CFG_SPOOF_TERMINAL: usize = 12;
pub const CFG_RAM_LIMIT: usize = 13;
pub const CFG_SPOOF_RESOLUTION: usize = 14;
/// Wine-game rows (only present when the Config screen carries `wine_game = Some`).
pub const CFG_GAME_EXE: usize = 15;
pub const CFG_GAME_PREFIX: usize = 16;
/// The following three are only shown in the global Settings tab, not per-app
/// Config. Their indices sit past the per-app rows (which top out at 17 = wine
/// save), so the shared setting_* helpers never see them from a per-app screen.
pub const CFG_CREATE_SHORTCUT: usize = 15;
pub const CFG_CONFIRM_INSTALL: usize = 16;
pub const CFG_ASK_SHORTCUT: usize = 17;
pub const CFG_SAVE: usize = 18;
pub const CFG_LEN: usize = 19;

/// Index of the Save button in the per-app Config screen. Shifts down by 2 when
/// the screen carries wine_game rows.
pub fn app_cfg_save_idx(has_wine_game: bool) -> usize {
    if has_wine_game { 17 } else { 15 }
}

/// Total navigable rows in the per-app Config screen.
pub fn app_cfg_total_rows(has_wine_game: bool) -> usize {
    if has_wine_game { 18 } else { 16 }
}

/// A fixed 32-char hex machine-id that apps can use as a plausible-looking ID.
pub const MACHINE_ID_SAMPLE: &str = "cafebabe0011223344556677deadbeef";
/// Generic hostname used by the "sample" option.
pub const HOSTNAME_SAMPLE: &str = "workstation";
/// Generic username used by the "sample" option.
pub const USERNAME_SAMPLE: &str = "user";

fn on_config(app: &mut App, code: KeyCode) {
    // Capture wine_game presence before mut-borrowing app.screen so the row
    // count is known without dropping the borrow.
    let has_wg = app.editing_wine_game.is_some();
    let save_idx = app_cfg_save_idx(has_wg);
    let total = app_cfg_total_rows(has_wg);

    let Screen::Config { app_name, config, selected } = &mut app.screen else { return };

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Discard changes (including any in-progress wine-game edits)
            app.editing_wine_game = None;
            app.screen = Screen::Main;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1).min(total - 1);
        }
        KeyCode::Right | KeyCode::Char(' ') => {
            if *selected == save_idx {
                let name = app_name.clone();
                let cfg = config.clone();
                save_config_and_wine_game(app, name, cfg);
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
            if has_wg && (*selected == CFG_GAME_EXE || *selected == CFG_GAME_PREFIX) {
                open_game_field_input(app);
                return;
            }
            cycle_setting(config, *selected, 1);
        }
        KeyCode::Left => {
            // Inverse of Right — cycle backward. Special rows are no-ops.
            let sel = *selected;
            let is_game_row = has_wg && (sel == CFG_GAME_EXE || sel == CFG_GAME_PREFIX);
            if sel != save_idx && sel != CFG_SHARES && !is_game_row {
                cycle_setting(config, sel, -1);
            }
        }
        KeyCode::Enter => {
            if *selected == save_idx {
                let name = app_name.clone();
                let cfg = config.clone();
                save_config_and_wine_game(app, name, cfg);
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
            if has_wg && (*selected == CFG_GAME_EXE || *selected == CFG_GAME_PREFIX) {
                open_game_field_input(app);
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

/// Persist config.ini and (if `editing_wine_game` is Some) the wine_game block
/// in the manifest, then return to Main. Consumes `editing_wine_game`.
fn save_config_and_wine_game(app: &mut App, name: String, cfg: AppConfig) {
    let _ = write_config(&name, &cfg);
    if let Some((exe, prefix)) = app.editing_wine_game.take() {
        if let Ok(mut m) = crate::manifest::read_manifest(&name) {
            if let Some(g) = m.app.wine_game.as_mut() {
                g.exe = exe;
                g.prefix = prefix;
                let _ = crate::manifest::write_manifest(&name, &m);
            }
        }
    }
    app.reload_installed();
    app.screen = Screen::Main;
    app.needs_clear = true;
}

/// Open a free-text input for the currently-selected game field (Exe or Prefix).
/// Assumes the active screen is Screen::Config and editing_wine_game is Some.
fn open_game_field_input(app: &mut App) {
    let Screen::Config { app_name, config, selected } = &app.screen else { return };
    let name = app_name.clone();
    let cfg = config.clone();
    let sel = *selected;
    let Some((exe, prefix)) = app.editing_wine_game.as_ref() else { return };
    let value = if sel == CFG_GAME_EXE { exe.clone() } else { prefix.clone() };
    app.screen = Screen::TextInput {
        app_name: name,
        config: cfg,
        back_selected: sel,
        field_idx: sel,
        value,
    };
    app.needs_clear = true;
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
        CFG_SPOOF_RESOLUTION => vec!["system", "1280×720", "1920×1080", "2560×1440", "3840×2160", "input"],
        CFG_CREATE_SHORTCUT => vec!["yes", "no"],
        CFG_CONFIRM_INSTALL | CFG_ASK_SHORTCUT => vec!["on", "off"],
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
        14 => "Spoof resolution",
        15 => "Default shortcut",
        16 => "Confirm install",
        17 => "Ask shortcut",
        _ => "Option",
    }
}

/// One-paragraph description of what each config row controls.
pub fn setting_description(idx: usize) -> &'static str {
    match idx {
        0 => "Allow outgoing internet access from the sandbox.\n\nDisable to run the app fully offline and prevent all network calls.",
        1 => "Allow the app to access webcam devices (/dev/video*).\n\nDisable to block camera access entirely.",
        2 => "Allow microphone access.\n\nNote: PipeWire/PulseAudio mic is only fully blocked when Audio is also disabled.",
        3 => "Allow audio playback and capture via PipeWire/PulseAudio.\n\nDisabling this also cuts off the mic path through the sound server.",
        4 => "Where the app's /tmp lives:\n\n• system   — share the host /tmp\n• ramdisk  — private in-memory tmpfs, wiped on close\n• local    — persistent ~/.wryayer/<app>/.tmp/\n• uuid     — fresh private dir on each launch",
        5 => "When to clean up the local temp dir (local/uuid modes):\n\n• never    — keep temp between launches\n• on_start — delete before each launch\n• on_close — delete after the app exits",
        6 => "Host directories bind-mounted read-write into the sandbox.\n\nUseful for sharing downloads, projects, or config files between the app and your system.",
        7 => "Override /etc/hostname and $HOSTNAME inside the sandbox.\n\n• system — use the real hostname\n• sample — sets it to 'workstation'\n• input  — type any custom name",
        8 => "Override $USER and $LOGNAME inside the sandbox.\n\n• system — use your real login name\n• sample — sets it to 'user'\n• input  — type any custom name",
        9 => "Override /etc/machine-id inside the sandbox.\n\n• system — real machine ID\n• random — fresh UUID every launch\n• sample — fixed placeholder\n• input  — type a 32-char hex value",
        10 => "Override /proc/cpuinfo inside the sandbox.\n\n• system — expose the real CPU\n• sample — generic Intel i7 cpuinfo\n• edit   — open a text editor to write a fully custom file (pre-filled with your real CPU data)",
        11 => "Override /etc/os-release inside the sandbox.\n\nChoose a preset (Ubuntu, Arch, Windows 11, ArduinoIDE) or 'input' to type any OS name.\n'system' exposes the real OS release.",
        12 => "Detect your real terminal emulator and pass its identity into the sandbox.\n\nWalks the process tree to find kitty, foot, alacritty, WezTerm, etc., then sets the matching env var (KITTY_WINDOW_ID, WEZTERM_PANE, …).\n\nFixes fastfetch / neofetch showing 'bwrap' instead of your real terminal.",
        13 => "Maximum RAM the app may use (RAM + swap both capped).\n\nEnforced via systemd-run MemoryMax + MemorySwapMax=0.\n'none' disables the limit. Requires systemd.",
        14 => "Spoof the screen resolution reported to the app.\n\nCreates a fake xrandr binary inside the sandbox and sets resolution env vars. Works for apps that call xrandr as a subprocess.\n\nNote: Chromium/Electron apps query the display server directly (X11/Wayland) and are not affected by this setting.",
        15 => "Whether to pre-select 'Yes' or 'No' in the shortcut prompt shown before each install.\n\nThe prompt always appears — this only controls which answer is highlighted by default.",
        16 => "Whether to show the 'Install <pkg>?' confirmation before installing.\n\n• on  — ask for a y/n confirmation first (default)\n• off — start the install immediately, no prompt",
        17 => "Whether to ask about creating a ~/bin shortcut before installing.\n\n• on  — show the shortcut prompt (default)\n• off — skip it and use the 'Default shortcut' setting above without asking",
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
        // Spoof resolution
        (14, 0) => "system — No resolution spoofing. The app sees the real screen dimensions.",
        (14, 1) => "1280×720 — Report HD (1280×720) to xrandr and via env vars.",
        (14, 2) => "1920×1080 — Report FHD (1920×1080) to xrandr and via env vars.",
        (14, 3) => "2560×1440 — Report QHD (2560×1440) to xrandr and via env vars.",
        (14, 4) => "3840×2160 — Report 4K (3840×2160) to xrandr and via env vars.",
        (14, 5) => "input — Type a custom resolution (e.g. 1600x900). Stored as WxH.",
        // Default shortcut
        (15, 0) => "yes — Pre-select 'Yes' in the shortcut prompt. The prompt still appears; press Enter to confirm quickly.",
        (15, 1) => "no — Pre-select 'No' in the shortcut prompt. Useful if you rarely want ~/bin shortcuts.",
        // Confirm install
        (16, 0) => "on — Show the 'Install <pkg>?' confirmation before every install.",
        (16, 1) => "off — Skip the confirmation and start installing right away.",
        // Ask shortcut
        (17, 0) => "on — Ask whether to create a ~/bin shortcut before each install.",
        (17, 1) => "off — Don't ask; silently apply the 'Default shortcut' setting.",
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
        CFG_SPOOF_RESOLUTION => match config.spoof_resolution.as_deref() {
            None             => 0,
            Some("1280x720") => 1,
            Some("1920x1080")=> 2,
            Some("2560x1440")=> 3,
            Some("3840x2160")=> 4,
            _                => 5,
        },
        CFG_CREATE_SHORTCUT => if config.create_shortcut { 0 } else { 1 },
        CFG_CONFIRM_INSTALL => if config.confirm_install { 0 } else { 1 },
        CFG_ASK_SHORTCUT => if config.ask_shortcut { 0 } else { 1 },
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
        (14, 0) => config.spoof_resolution = None,
        (14, 1) => config.spoof_resolution = Some("1280x720".to_string()),
        (14, 2) => config.spoof_resolution = Some("1920x1080".to_string()),
        (14, 3) => config.spoof_resolution = Some("2560x1440".to_string()),
        (14, 4) => config.spoof_resolution = Some("3840x2160".to_string()),
        // (14, 5) = "input" — handled by on_option_picker which opens TextInput
        (15, 0) => config.create_shortcut = true,
        (15, 1) => config.create_shortcut = false,
        (16, 0) => config.confirm_install = true,
        (16, 1) => config.confirm_install = false,
        (17, 0) => config.ask_shortcut = true,
        (17, 1) => config.ask_shortcut = false,
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
            // Discard the picker choice. Per-app pickers return to the
            // per-app Config popup; global-defaults pickers (empty app_name)
            // return to the Settings tab.
            let name = app_name.clone();
            let cfg = config.clone();
            let idx = *setting_idx;
            if name.is_empty() {
                app.global_config = cfg;
                app.global_selected = idx;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: idx };
            }
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

            // "input" option opens the free-text overlay for hostname/username/machine-id/os/resolution.
            let is_input_choice = match idx {
                CFG_SPOOF_HOSTNAME | CFG_SPOOF_USERNAME => choice == 2,
                CFG_SPOOF_OS => choice == 5,
                CFG_SPOOF_MACHINE_ID => choice == 3,
                CFG_SPOOF_RESOLUTION => choice == 5,
                _ => false,
            };
            if is_input_choice {
                let current = match idx {
                    CFG_SPOOF_HOSTNAME    => cfg.spoof_hostname.clone().unwrap_or_default(),
                    CFG_SPOOF_USERNAME    => cfg.spoof_username.clone().unwrap_or_default(),
                    CFG_SPOOF_MACHINE_ID  => cfg.spoof_machine_id.clone().unwrap_or_default(),
                    CFG_SPOOF_OS          => cfg.spoof_os.clone().unwrap_or_default(),
                    CFG_SPOOF_RESOLUTION  => cfg.spoof_resolution.clone().unwrap_or_default(),
                    _ => String::new(),
                };
                // Clear pre-fill when current value is one of the fixed presets.
                let is_preset = match idx {
                    CFG_SPOOF_HOSTNAME    => current == HOSTNAME_SAMPLE,
                    CFG_SPOOF_USERNAME    => current == USERNAME_SAMPLE,
                    CFG_SPOOF_MACHINE_ID  => current == "random" || current == MACHINE_ID_SAMPLE,
                    CFG_SPOOF_OS          => matches!(current.as_str(), "ubuntu" | "arch" | "windows" | "arduinoide"),
                    CFG_SPOOF_RESOLUTION  => matches!(current.as_str(), "1280x720" | "1920x1080" | "2560x1440" | "3840x2160"),
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
            if name.is_empty() {
                // Global settings mode: update in-memory config and return to Settings tab
                app.global_config = cfg;
                app.global_selected = idx;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: idx };
            }
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
    if name.is_empty() {
        // Global settings mode
        app.global_config = cfg;
        app.global_selected = sel;
        app.tab = Tab::Settings;
        app.screen = Screen::Main;
    } else {
        app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
    }
    app.needs_clear = true;
}

// ── Text input overlay (spoof settings) ──────────────────────────────────────

fn set_spoof_field(config: &mut AppConfig, idx: usize, value: String) {
    let v = if value.is_empty() { None } else { Some(value) };
    match idx {
        CFG_SPOOF_HOSTNAME    => config.spoof_hostname    = v,
        CFG_SPOOF_USERNAME    => config.spoof_username    = v,
        CFG_SPOOF_MACHINE_ID  => config.spoof_machine_id  = v,
        CFG_SPOOF_CPUINFO     => config.spoof_cpuinfo     = v,
        CFG_SPOOF_OS          => config.spoof_os          = v,
        CFG_SPOOF_RESOLUTION  => config.spoof_resolution  = v,
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
            if name.is_empty() {
                app.global_config = cfg;
                app.global_selected = sel;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
            }
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let name = app_name.clone();
            let mut cfg = config.clone();
            let sel = *back_selected;
            let idx = *field_idx;
            let v = value.trim().to_string();
            // Game-field inputs mutate the in-memory editing_wine_game tuple;
            // spoof-field inputs mutate AppConfig as before.
            let is_game_field = matches!(idx, CFG_GAME_EXE | CFG_GAME_PREFIX);
            if is_game_field {
                if let Some((exe, prefix)) = app.editing_wine_game.as_mut() {
                    if idx == CFG_GAME_EXE {
                        *exe = v;
                    } else {
                        *prefix = v;
                    }
                }
            } else {
                set_spoof_field(&mut cfg, idx, v);
            }
            if name.is_empty() {
                // Global settings mode
                app.global_config = cfg;
                app.global_selected = sel;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                app.screen = Screen::Config { app_name: name, config: cfg, selected: sel };
            }
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

// ── Settings tab (global defaults) ───────────────────────────────────────────

fn on_settings_tab(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.global_selected = app.global_selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.global_selected = (app.global_selected + 1).min(CFG_LEN - 1);
        }
        KeyCode::Right | KeyCode::Char(' ') => {
            if app.global_selected == CFG_SAVE {
                let cfg = app.global_config.clone();
                if write_global_config(&cfg).is_ok() {
                    app.status = "Global defaults saved.".into();
                } else {
                    app.status = "error: failed to save global defaults".into();
                }
                return;
            }
            if app.global_selected == CFG_SHARES {
                open_shared_dirs_global(app);
                return;
            }
            cycle_setting(&mut app.global_config, app.global_selected, 1);
        }
        KeyCode::Left => {
            if app.global_selected != CFG_SAVE && app.global_selected != CFG_SHARES {
                cycle_setting(&mut app.global_config, app.global_selected, -1);
            }
        }
        KeyCode::Enter => {
            if app.global_selected == CFG_SAVE {
                let cfg = app.global_config.clone();
                if write_global_config(&cfg).is_ok() {
                    app.status = "Global defaults saved.".into();
                } else {
                    app.status = "error: failed to save global defaults".into();
                }
                return;
            }
            if app.global_selected == CFG_SHARES {
                open_shared_dirs_global(app);
                return;
            }
            let idx = app.global_selected;
            let cur = setting_current(&app.global_config, idx);
            // Use empty app_name as sentinel for global settings mode
            app.screen = Screen::OptionPicker {
                app_name: String::new(),
                config: app.global_config.clone(),
                setting_idx: idx,
                selected: cur,
            };
            app.needs_clear = true;
        }
        KeyCode::Char('?') => {
            let idx = app.global_selected;
            app.screen = Screen::SettingHelp {
                app_name: String::new(),
                config: app.global_config.clone(),
                back_selected: idx,
            };
            app.needs_clear = true;
        }
        _ => {}
    }
}

// ── Shared dirs screen ────────────────────────────────────────────────────────

/// Open the SharedDirs editor for the global defaults (empty app_name
/// signals global mode throughout the SharedDirs / file-browser flow).
fn open_shared_dirs_global(app: &mut App) {
    let dirs = app.global_config.shared_dirs.clone();
    let sel = dirs.len().saturating_sub(1);
    app.screen = Screen::SharedDirs { app_name: String::new(), dirs, selected: sel };
    app.needs_clear = true;
}

/// Read shared-dirs config from either the per-app file or the global defaults
/// file, depending on whether app_name is set.
fn read_shared_cfg(app_name: &str) -> AppConfig {
    if app_name.is_empty() {
        read_global_config()
    } else {
        read_config(app_name).unwrap_or_default()
    }
}

/// Persist a config back to either the per-app file or defaults.ini.
fn write_shared_cfg(app_name: &str, cfg: &AppConfig) {
    if app_name.is_empty() {
        let _ = write_global_config(cfg);
    } else {
        let _ = write_config(app_name, cfg);
    }
}

fn on_shared_dirs(app: &mut App, code: KeyCode) {
    let Screen::SharedDirs { app_name, dirs, selected } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            let name = app_name.clone();
            if name.is_empty() {
                // Returning from the global-defaults editor: refresh the
                // in-memory global_config from disk and pop back to the
                // Settings tab instead of the per-app Config popup.
                app.global_config = read_global_config();
                app.global_selected = CFG_SHARES;
                app.tab = Tab::Settings;
                app.screen = Screen::Main;
            } else {
                let config = read_config(&name).unwrap_or_default();
                app.screen = Screen::Config { app_name: name, config, selected: CFG_SHARES };
            }
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
                let mut config = read_shared_cfg(&name);
                config.shared_dirs = dirs.clone();
                write_shared_cfg(&name, &config);
            }
        }
        KeyCode::Char('a') => {
            let name = app_name.clone();
            open_file_browser(app, BrowserMode::PickShareDir(name));
        }
        _ => {}
    }
}

// ── File browser ──────────────────────────────────────────────────────────────

fn open_file_browser(app: &mut App, mode: BrowserMode) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let dir = PathBuf::from(home);
    let entries = load_dir_entries(&dir);
    let mut fb_state = ListState::default();
    if !entries.is_empty() {
        fb_state.select(Some(0));
    }
    app.screen = Screen::FileBrowser { current_dir: dir, entries, fb_state, mode };
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
    SelectShareDir { path: PathBuf, app_name: String },
    SelectGameDir(PathBuf),
    GoUp,
    Close(BrowserMode),
}

fn on_file_browser(app: &mut App, code: KeyCode) {
    let action = {
        let Screen::FileBrowser { current_dir, entries, fb_state, mode } = &mut app.screen else { return };
        let mode_clone = mode.clone();
        let pick_dir = !matches!(mode_clone, BrowserMode::ImportZip);
        match code {
            KeyCode::Esc | KeyCode::Char('q') => FbAction::Close(mode_clone),
            // Space / s selects the current directory in pick-dir modes
            KeyCode::Char(' ') | KeyCode::Char('s') if pick_dir => {
                match mode_clone {
                    BrowserMode::PickShareDir(app_name) => {
                        FbAction::SelectShareDir { path: current_dir.clone(), app_name }
                    }
                    BrowserMode::PickGameDir => {
                        FbAction::SelectGameDir(current_dir.clone())
                    }
                    BrowserMode::ImportZip => FbAction::Nothing,
                }
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
                    } else if entry.is_zip && matches!(mode_clone, BrowserMode::ImportZip) {
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
        FbAction::Close(BrowserMode::ImportZip) => {
            app.screen = Screen::Main;
            app.tab = Tab::Import;
            app.needs_clear = true;
        }
        FbAction::Close(BrowserMode::PickGameDir) => {
            app.screen = Screen::Main;
            app.tab = Tab::Games;
            app.needs_clear = true;
        }
        FbAction::Close(BrowserMode::PickShareDir(app_name)) => {
            let config = read_shared_cfg(&app_name);
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
        FbAction::SelectShareDir { path, app_name } => {
            let path_str = path.to_string_lossy().into_owned();
            let mut config = read_shared_cfg(&app_name);
            if !config.shared_dirs.contains(&path_str) {
                config.shared_dirs.push(path_str);
                write_shared_cfg(&app_name, &config);
            }
            let dirs = config.shared_dirs;
            let selected = dirs.len().saturating_sub(1);
            app.screen = Screen::SharedDirs { app_name, dirs, selected };
            app.needs_clear = true;
        }
        FbAction::SelectGameDir(path) => {
            enter_game_wizard(app, path);
        }
    }
}

// ── Install queue + background ops ────────────────────────────────────────────

/// Pop the next package from the install queue and show InstallTarget or Confirm.
/// Build the install queue from all marked packages and kick it off.
/// Marks in display order come first, then any marked packages not currently
/// in search_results (sorted). Clears selected_pkgs before returning.
fn enqueue_marked(app: &mut App) {
    // Preserve visible display order for marks that appear in current results
    let mut pkgs: Vec<String> = app.search_results.iter()
        .filter(|(name, _)| app.selected_pkgs.contains(name.as_str()))
        .map(|(name, _)| name.clone())
        .collect();
    // Append marks that are outside the current search results (sorted)
    let mut hidden: Vec<String> = app.selected_pkgs.iter()
        .filter(|p| !pkgs.contains(*p))
        .cloned()
        .collect();
    hidden.sort();
    pkgs.extend(hidden);
    app.selected_pkgs.clear();
    app.install_queue = pkgs.into_iter().collect();
    process_install_queue(app);
}

/// Already-installed packages are skipped with a status note.
fn process_install_queue(app: &mut App) {
    loop {
        let Some(pkg) = app.install_queue.pop_front() else { return };
        if app.installed.iter().any(|m| m.app.name == pkg) {
            let note = format!("'{pkg}' already installed — skipped");
            app.status = if app.status.is_empty() { note } else { format!("{}; {}", app.status, note) };
            continue;
        }
        let targets: Vec<String> = app.installed
            .iter()
            .filter(|m| m.app.alias_of.is_none())
            .map(|m| m.app.name.clone())
            .collect();
        if targets.is_empty() {
            install_confirm(
                app,
                format!("Install '{pkg}'?"),
                vec![
                    format!("Installs {pkg} into ~/.wryayer/{pkg}/"),
                    String::new(),
                    "Press y to confirm, n or Esc to cancel.".into(),
                ],
                PendingAction::Install { pkg, app_name: None, into: None },
            );
        } else {
            app.screen = Screen::InstallTarget { pkg, targets, selected: 0 };
        }
        app.needs_clear = true;
        return;
    }
}

// ── Shortcut confirmation ─────────────────────────────────────────────────────

fn on_ask_shortcut(app: &mut App, code: KeyCode) {
    let Screen::AskShortcut { selected, .. } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.install_queue.clear();
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
            let screen = std::mem::replace(&mut app.screen, Screen::Main);
            if let Screen::AskShortcut { title, mut args, selected, .. } = screen {
                if selected == 1 {
                    args.push("--keep-without-launcher".into());
                }
                launch_op(app, title, args, None, true);
            }
        }
        _ => {}
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

// ── Games tab ─────────────────────────────────────────────────────────────────

fn on_games(app: &mut App, code: KeyCode) {
    let games_count = app.games().len();
    // Make sure games_state has a sensible selection when the list is non-empty.
    if games_count > 0 && app.games_state.selected().is_none() {
        app.games_state.select(Some(0));
    } else if games_count == 0 {
        app.games_state.select(None);
    }

    match code {
        KeyCode::Char('i') | KeyCode::Char('a') => {
            open_file_browser(app, BrowserMode::PickGameDir);
        }
        KeyCode::Up | KeyCode::Char('k') if games_count > 0 => {
            let i = app.games_state.selected().unwrap_or(0);
            app.games_state.select(Some(if i == 0 { games_count - 1 } else { i - 1 }));
        }
        KeyCode::Down | KeyCode::Char('j') if games_count > 0 => {
            let i = app.games_state.selected().unwrap_or(0);
            app.games_state.select(Some((i + 1) % games_count));
        }
        KeyCode::Char('r') | KeyCode::Enter if games_count > 0 => {
            if let Some(m) = app.selected_game() {
                app.run_request = Some(m.app.name.clone());
            }
        }
        KeyCode::Enter if games_count == 0 => {
            // No games yet — Enter is a shortcut to start the import wizard.
            open_file_browser(app, BrowserMode::PickGameDir);
        }
        KeyCode::Char('s') if games_count > 0 => {
            if let Some(m) = app.selected_game() {
                let name = m.app.name.clone();
                let config = read_config(&name).unwrap_or_default();
                app.editing_wine_game = m.app.wine_game.as_ref()
                    .map(|w| (w.exe.clone(), w.prefix.clone()));
                app.screen = Screen::Config { app_name: name, config, selected: 0 };
                app.needs_clear = true;
            }
        }
        KeyCode::Char('d') | KeyCode::Delete if games_count > 0 => {
            if let Some(m) = app.selected_game() {
                let name = m.app.name.clone();
                app.screen = Screen::Confirm {
                    title: format!("Remove game '{name}'?"),
                    body: vec![
                        format!("Delete ~/.wryayer/{name}/ and its wine prefix?"),
                        String::new(),
                        "Press y to continue, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Remove(name),
                    danger: true,
                };
                app.needs_clear = true;
            }
        }
        _ => {}
    }
}

/// Walk the picked folder for .exe files, return Vec<(relative_path, size_bytes)>.
fn scan_exes(root: &std::path::Path) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 { continue; }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if ft.is_dir() {
                if name_str.starts_with('.') || name_str.eq_ignore_ascii_case("drive_c") {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else if ft.is_file() && name_str.to_lowercase().ends_with(".exe") {
                let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let rel = entry.path().strip_prefix(root).unwrap_or(&entry.path())
                    .to_string_lossy().into_owned();
                out.push((rel, sz));
            }
        }
    }
    // Sort: prefer top-level, then by size desc
    out.sort_by(|a, b| {
        let da = a.0.matches('/').count();
        let db = b.0.matches('/').count();
        da.cmp(&db).then(b.1.cmp(&a.1))
    });
    out
}

fn sanitize_game_name(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn enter_game_wizard(app: &mut App, game_dir: PathBuf) {
    let exes = scan_exes(&game_dir);
    if exes.is_empty() {
        app.status = format!("No .exe files found under {}", game_dir.display());
        app.screen = Screen::Main;
        app.tab = Tab::Games;
        app.needs_clear = true;
        return;
    }
    app.screen = Screen::GameExePick { game_dir, exes, selected: 0 };
    app.needs_clear = true;
}

fn on_game_exe_pick(app: &mut App, code: KeyCode) {
    let Screen::GameExePick { game_dir, exes, selected } = &mut app.screen else { return };
    let len = exes.len();
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.tab = Tab::Games;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = if *selected == 0 { len - 1 } else { *selected - 1 };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1) % len;
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let gd = game_dir.clone();
            let exe = exes[*selected].0.clone();
            let default_name = sanitize_game_name(
                gd.file_name().and_then(|n| n.to_str()).unwrap_or("game"),
            );
            app.screen = Screen::GameNameInput {
                game_dir: gd,
                exe,
                value: default_name,
            };
            app.needs_clear = true;
        }
        _ => {}
    }
}

fn on_game_name_input(app: &mut App, code: KeyCode) {
    let Screen::GameNameInput { game_dir, exe, value } = &mut app.screen else { return };
    match code {
        KeyCode::Esc => {
            let gd = game_dir.clone();
            let exes = scan_exes(&gd);
            app.screen = Screen::GameExePick { game_dir: gd, exes, selected: 0 };
            app.needs_clear = true;
        }
        KeyCode::Enter => {
            let name = value.trim().to_string();
            if name.is_empty() { return; }
            if app.installed.iter().any(|m| m.app.name == name) {
                app.status = format!("'{name}' is already taken — pick a different name");
                return;
            }
            let gd = game_dir.clone();
            let exe = exe.clone();
            app.screen = Screen::GameConfirm {
                game_dir: gd,
                exe,
                app_name: name,
                delete_source: false,
                selected: 0,
            };
            app.needs_clear = true;
        }
        KeyCode::Backspace => { value.pop(); }
        KeyCode::Char(c) => { value.push(c); }
        _ => {}
    }
}

fn on_game_confirm(app: &mut App, code: KeyCode) {
    let Screen::GameConfirm { game_dir, exe, app_name, delete_source, selected } = &mut app.screen else { return };
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.screen = Screen::Main;
            app.tab = Tab::Games;
            app.needs_clear = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = if *selected == 0 { 2 } else { *selected - 1 };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1) % 3;
        }
        KeyCode::Char(' ') if *selected == 1 => {
            *delete_source = !*delete_source;
        }
        KeyCode::Enter => {
            let s = *selected;
            let gd = game_dir.clone();
            let exe = exe.clone();
            let app_name = app_name.clone();
            let delete = *delete_source;
            match s {
                0 => {
                    let mut args = vec![
                        "install-game".to_string(),
                        gd.to_string_lossy().into_owned(),
                        "--exe".to_string(), exe,
                        "--app-name".to_string(), app_name.clone(),
                    ];
                    if delete {
                        args.push("--delete-source".into());
                    }
                    let total = dir_bytes(&gd.to_string_lossy());
                    launch_op(app, format!("Import game — {app_name}"), args, total, true);
                }
                1 => { *delete_source = !*delete_source; }
                _ => {
                    app.screen = Screen::Main;
                    app.tab = Tab::Games;
                    app.needs_clear = true;
                }
            }
        }
        _ => {}
    }
}
