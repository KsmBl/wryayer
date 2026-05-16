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

use crate::config::{read_config, write_config, AppConfig, LocalDelete, TempMode};
use crate::manifest::{list_all_apps, Manifest};

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
        started: Instant,
        reload: bool,
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
}

pub enum PendingAction {
    Remove(String),
    ConfirmedRemove(String),
    Update(String),
    Install(String),
    Backup(String),
}

// ── Tabs ──────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Installed,
    Install,
    Import,
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
    pub search_results: Vec<String>,
    pub search_searching: bool,
    pub search_gen: u64,
    pub search_tx: Sender<(u64, Vec<String>)>,
    pub search_rx: Receiver<(u64, Vec<String>)>,
    pub avail_state: ListState,
    pub search_list_focused: bool,
    // Import tab
    pub import_input: String,
    // Overlay
    pub screen: Screen,
    pub status: String,
    pub log_scroll: usize,
    pub needs_clear: bool,
}

impl App {
    fn new() -> Result<Self> {
        let installed = list_all_apps()?;
        let mut inst_state = ListState::default();
        if !installed.is_empty() {
            inst_state.select(Some(0));
        }
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
            screen: Screen::Main,
            status: String::new(),
            log_scroll: 0,
            needs_clear: false,
        })
    }

    fn reload_installed(&mut self) {
        if let Ok(list) = list_all_apps() {
            self.installed = list;
            let sel = self.inst_state.selected().unwrap_or(0);
            if self.installed.is_empty() {
                self.inst_state.select(None);
            } else {
                self.inst_state.select(Some(sel.min(self.installed.len() - 1)));
            }
        }
    }

    pub fn selected_installed(&self) -> Option<&Manifest> {
        self.inst_state.selected().and_then(|i| self.installed.get(i))
    }

    pub fn selected_available(&self) -> Option<&str> {
        self.avail_state
            .selected()
            .and_then(|i| self.search_results.get(i))
            .map(String::as_str)
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
        if let Screen::Operation { rx, log, done, success, .. } = &mut app.screen {
            loop {
                match rx.try_recv() {
                    Ok(Msg::Line(l)) => log.push(l),
                    Ok(Msg::Done(ok)) => { *done = true; *success = ok; }
                    Err(_) => break,
                }
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
            if app.needs_clear {
                app.needs_clear = false;
                terminal.clear()?;
            }
        }
    }
}

// ── Key dispatch ──────────────────────────────────────────────────────────────

fn handle_key(app: &mut App, code: KeyCode) -> Result<()> {
    let tag = match &app.screen {
        Screen::Main => 0u8,
        Screen::Confirm { .. } => 1,
        Screen::Operation { done: false, .. } => 2,
        Screen::Operation { done: true, .. } => 3,
        Screen::Config { .. } => 4,
        Screen::FileBrowser { .. } => 5,
        Screen::SharedDirs { .. } => 6,
    };

    match tag {
        0 => on_main(app, code)?,
        1 => on_confirm(app, code)?,
        2 => on_op_running(app, code),
        3 => on_op_done(app, code)?,
        4 => on_config(app, code),
        5 => on_file_browser(app, code),
        6 => on_shared_dirs(app, code),
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
                Tab::Install => Tab::Import,
                Tab::Import => Tab::Installed,
            };
            app.status.clear();
            return Ok(());
        }
        KeyCode::BackTab => {
            app.tab = match app.tab {
                Tab::Installed => Tab::Import,
                Tab::Install => Tab::Installed,
                Tab::Import => Tab::Install,
            };
            app.status.clear();
            return Ok(());
        }
        _ => {}
    }

    // 'q' / Esc quit only when NOT in the Import text-input tab
    if !matches!(app.tab, Tab::Import) {
        if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
            app.quit = true;
            return Ok(());
        }
    }

    match app.tab {
        Tab::Installed => on_installed(app, code),
        Tab::Install => on_install(app, code),
        Tab::Import => on_import(app, code),
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
                let name = m.app.name.clone();
                launch_op(app, format!("Run — {name}"), vec!["run".into(), name], None, false);
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
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
        KeyCode::Char('b') => {
            if let Some(m) = app.selected_installed() {
                let name = m.app.name.clone();
                let zip = format!("{}-{}.zip", name, chrono::Local::now().format("%Y-%m-%d"));
                app.screen = Screen::Confirm {
                    title: format!("Backup '{name}'?"),
                    body: vec![
                        format!("Output: ~/{zip}"),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Backup(name),
                    danger: false,
                };
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
                        app.screen = Screen::Confirm {
                            title: format!("'{pkg}' is already installed"),
                            body: vec![
                                format!("Remove '{pkg}' and its isolated directory?"),
                                String::new(),
                                "Press y to uninstall, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::Remove(pkg),
                            danger: true,
                        };
                    } else {
                        app.screen = Screen::Confirm {
                            title: format!("Install '{pkg}'?"),
                            body: vec![
                                format!("Installs {pkg} into ~/.wryayer/{pkg}/"),
                                String::new(),
                                "Press y to confirm, n or Esc to cancel.".into(),
                            ],
                            action: PendingAction::Install(pkg),
                            danger: false,
                        };
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
        let out = Command::new("pacman").args(["-Ssq", &query]).output();
        let results = match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect(),
            Err(_) => vec![],
        };
        let _ = tx.send((gen, results));
    });
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
        PendingAction::Update(name) =>
            launch_op(app, format!("Update — {name}"), vec!["update".into(), name], None, true),
        PendingAction::Install(pkg) =>
            launch_op(app, format!("Install — {pkg}"), vec!["install".into(), pkg], None, true),
        PendingAction::Backup(name) => {
            let total = dir_bytes(&format!(
                "{}/.wryayer/{name}",
                std::env::var("HOME").unwrap_or_default()
            ));
            launch_op(app, format!("Backup — {name}"), vec!["backup".into(), name], total, false);
        }
    }
}

// ── Operation screens ─────────────────────────────────────────────────────────

fn on_op_running(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Up | KeyCode::Char('k') => { if app.log_scroll > 0 { app.log_scroll -= 1; } }
        KeyCode::Down | KeyCode::Char('j') => { app.log_scroll += 1; }
        _ => {}
    }
}

fn on_op_done(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Up | KeyCode::Char('k') => { if app.log_scroll > 0 { app.log_scroll -= 1; } }
        KeyCode::Down | KeyCode::Char('j') => { app.log_scroll += 1; }
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

// Rows: 0=network 1=camera 2=microphone 3=audio 4=temp_mode 5=temp_delete 6=shared_dirs 7=Save
pub const CFG_LEN: usize = 8;
pub const CFG_SHARES: usize = 6;
pub const CFG_SAVE: usize = 7;

fn on_config(app: &mut App, code: KeyCode) {
    let Screen::Config { app_name, config, selected } = &mut app.screen else { return };

    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Discard changes
            app.screen = Screen::Main;
            app.needs_clear = true;
            return;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1).min(CFG_LEN - 1);
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter => {
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
            match *selected {
                0 => config.network = !config.network,
                1 => config.camera = !config.camera,
                2 => config.microphone = !config.microphone,
                3 => config.audio = !config.audio,
                4 => {
                    config.temp_mode = match config.temp_mode {
                        TempMode::System  => TempMode::Ramdisk,
                        TempMode::Ramdisk => TempMode::Local,
                        TempMode::Local   => TempMode::Uuid,
                        TempMode::Uuid    => TempMode::System,
                    };
                }
                5 => {
                    config.temp_delete = match config.temp_delete {
                        LocalDelete::Never   => LocalDelete::OnStart,
                        LocalDelete::OnStart => LocalDelete::OnClose,
                        LocalDelete::OnClose => LocalDelete::Never,
                    };
                }
                _ => {}
            }
        }
        _ => {}
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
    let (tx, rx) = mpsc::channel();
    spawn_wryayer(args, tx);
    app.log_scroll = 0;
    app.screen = Screen::Operation {
        title,
        log: vec![],
        done: false,
        success: false,
        rx,
        total_bytes,
        started: Instant::now(),
        reload,
    };
}

fn spawn_wryayer(args: Vec<String>, tx: mpsc::Sender<Msg>) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wryayer".into());
    thread::spawn(move || {
        let mut child = match Command::new(&exe)
            .args(&args)
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
