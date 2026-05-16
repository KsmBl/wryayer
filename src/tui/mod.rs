mod ui;

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader};
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

use crate::manifest::{list_all_apps, Manifest};

// ── Op messages ───────────────────────────────────────────────────────────────

pub enum Msg {
    Line(String),
    Done(bool),
}

// ── Screen overlays ───────────────────────────────────────────────────────────

pub enum Screen {
    Main,
    Confirm {
        title: String,
        body: Vec<String>,
        action: PendingAction,
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
}

pub enum PendingAction {
    Remove(String),
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

        // Drain async search results — only apply if generation matches
        loop {
            match app.search_rx.try_recv() {
                Ok((gen, results)) if gen == app.search_gen => {
                    app.search_results = results;
                    app.search_searching = false;
                    if app.search_results.is_empty() {
                        app.status = "No results.".into();
                    } else {
                        app.status = format!("{} results", app.search_results.len());
                    }
                }
                Ok(_) => {} // stale generation, discard
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
            handle_key(&mut app, key.code)?;
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
    };

    match tag {
        0 => on_main(app, code)?,
        1 => on_confirm(app, code)?,
        2 => on_op_running(app, code),
        3 => on_op_done(app, code)?,
        _ => {}
    }
    Ok(())
}

// ── Main screen ───────────────────────────────────────────────────────────────

fn on_main(app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.quit = true;
            return Ok(());
        }
        KeyCode::Tab => {
            app.tab = match app.tab {
                Tab::Installed => Tab::Install,
                Tab::Install => Tab::Import,
                Tab::Import => Tab::Installed,
            };
            app.status.clear();
            return Ok(());
        }
        _ => {}
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
                        format!("This deletes ~/.wryayer/{name}/ and all launchers."),
                        String::new(),
                        "Press y to confirm, n or Esc to cancel.".into(),
                    ],
                    action: PendingAction::Remove(name),
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
                        vec![
                            format!("  {cur}  →  {v}"),
                            String::new(),
                            "Press y to update, n or Esc to cancel.".into(),
                        ]
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
                };
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
        app.search_gen += 1; // invalidate any in-flight search
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
        KeyCode::Char(c) => app.import_input.push(c),
        KeyCode::Backspace => { app.import_input.pop(); }
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
        PendingAction::Remove(name) =>
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
        }
        _ => {}
    }
    Ok(())
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
