use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame,
};

use crate::config::{AppConfig, LocalDelete, TempMode};

use super::{App, Screen, Tab, CFG_SAVE};

const C_ACCENT: Color = Color::Cyan;
const C_GREEN: Color = Color::Green;
const C_RED: Color = Color::Red;
const C_YELLOW: Color = Color::Yellow;
const C_DIM: Color = Color::DarkGray;
const C_SELECT: Color = Color::Rgb(40, 60, 80);

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    draw_tabs(f, app, chunks[0]);

    match app.tab {
        Tab::Installed => draw_installed(f, app, chunks[1]),
        Tab::Install => draw_install(f, app, chunks[1]),
        Tab::Import => draw_import(f, app, chunks[1]),
    }

    draw_statusbar(f, app, chunks[2]);

    // Overlays
    match &app.screen {
        Screen::Main => {}
        Screen::Confirm { title, body, danger, .. } => {
            let title = title.clone();
            let body = body.clone();
            let danger = *danger;
            draw_confirm(f, area, &title, &body, danger);
        }
        Screen::Operation { title, log, done, success, total_bytes, started, .. } => {
            let title = title.clone();
            let log = log.clone();
            let done = *done;
            let success = *success;
            let total_bytes = *total_bytes;
            let elapsed = started.elapsed();
            draw_operation(f, area, app, &title, &log, done, success, total_bytes, elapsed);
        }
        Screen::Config { app_name, config, selected } => {
            let app_name = app_name.clone();
            let config = config.clone();
            let selected = *selected;
            draw_config(f, area, &app_name, &config, selected);
        }
        Screen::SharedDirs { app_name, dirs, selected } => {
            let app_name = app_name.clone();
            let dirs = dirs.clone();
            let selected = *selected;
            draw_shared_dirs(f, area, &app_name, &dirs, selected);
        }
        Screen::FileBrowser { current_dir, entries, fb_state, pick_dir_for } => {
            let title = current_dir.to_string_lossy().into_owned();
            let entries: Vec<(String, bool, bool)> = entries
                .iter()
                .map(|e| (e.name.clone(), e.is_dir, e.is_zip))
                .collect();
            let sel = fb_state.selected();
            let pick_dir = pick_dir_for.is_some();
            draw_file_browser(f, area, &title, &entries, sel, pick_dir);
        }
    }
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mk = |label: &str| Line::from(vec![
        Span::raw(" "),
        Span::styled(label.to_string(), Style::default().fg(C_ACCENT)),
        Span::raw(" "),
    ]);
    let titles = vec![mk("Installed"), mk("Install"), mk("Import")];
    let sel = match app.tab { Tab::Installed => 0, Tab::Install => 1, Tab::Import => 2 };
    let tabs = Tabs::new(titles)
        .select(sel)
        .block(Block::default().borders(Borders::ALL)
            .title(" wryayer ").title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)))
        .highlight_style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD).bg(C_SELECT))
        .divider(Span::styled("|", Style::default().fg(C_DIM)));
    f.render_widget(tabs, area);
}

// ── Installed tab ─────────────────────────────────────────────────────────────

fn draw_installed(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    let items: Vec<ListItem> = app.installed.iter().map(|m| {
        let dot = if app.update_available.contains_key(&m.app.name) {
            Span::styled(" ●", Style::default().fg(C_YELLOW))
        } else {
            Span::raw("  ")
        };
        ListItem::new(Line::from(vec![dot, Span::styled(&m.app.name, Style::default().fg(Color::White))]))
    }).collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Apps ").title_style(Style::default().fg(C_ACCENT)))
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[0], &mut app.inst_state);
    draw_detail(f, app, chunks[1]);
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL)
        .title(" Details ").title_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(m) = app.selected_installed() else {
        f.render_widget(
            Paragraph::new("No app selected.").style(Style::default().fg(C_DIM)).alignment(Alignment::Center),
            inner,
        );
        return;
    };

    let ver = m.packages.iter().find(|p| p.name == m.app.name)
        .map(|p| p.version.as_str()).unwrap_or("?");
    let installed = m.app.installed_at.get(..10).unwrap_or(&m.app.installed_at);
    let launchers = m.app.launchers.join(", ");
    let dim = Style::default().fg(C_DIM);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("  Name:       ", dim),
            Span::styled(&m.app.name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![Span::styled("  Version:    ", dim), Span::styled(ver, Style::default().fg(C_GREEN))]),
        Line::from(vec![Span::styled("  Installed:  ", dim), Span::raw(installed)]),
        Line::from(vec![Span::styled("  Launchers:  ", dim), Span::raw(launchers)]),
        Line::from(vec![
            Span::styled("  Packages:   ", dim),
            Span::styled(m.packages.len().to_string(), Style::default().fg(C_ACCENT)),
        ]),
    ];

    if let Some(new_ver) = app.update_available.get(&m.app.name) {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  Update:     ", dim),
            Span::styled(format!("{ver} → {new_ver}"), Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  [r] Run  [d] Delete  [b] Backup  [c] Check  [u] Update  [s] Config",
        dim,
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ── Install tab ───────────────────────────────────────────────────────────────

fn draw_install(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let bar_active = !app.search_list_focused;
    let cursor = if bar_active { "█" } else { "" };
    let search_title = if app.search_searching { " Search — searching… " } else { " Search " };

    f.render_widget(
        Paragraph::new(format!("{}{}", app.search_input, cursor))
            .block(Block::default().borders(Borders::ALL).title(search_title)
                .title_style(Style::default().fg(if bar_active { Color::White } else { C_DIM }))
                .border_style(Style::default().fg(if bar_active { C_ACCENT } else { C_DIM })))
            .style(Style::default().fg(Color::White)),
        chunks[0],
    );

    let installed_names: std::collections::HashSet<&str> =
        app.installed.iter().map(|m| m.app.name.as_str()).collect();

    let items: Vec<ListItem> = app.search_results.iter().map(|pkg| {
        if installed_names.contains(pkg.as_str()) {
            ListItem::new(Line::from(vec![
                Span::styled("✓ ", Style::default().fg(C_GREEN)),
                Span::styled(pkg.as_str(), Style::default().fg(Color::White)),
                Span::styled(" [installed]", Style::default().fg(C_GREEN)),
            ]))
        } else {
            ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(pkg.as_str(), Style::default().fg(Color::White)),
            ]))
        }
    }).collect();

    let results_title = if app.search_results.is_empty() {
        " Results "
    } else {
        " Results — [↓] Select  [Enter] Install / Uninstall "
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(results_title).title_style(Style::default().fg(C_ACCENT)))
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[1], &mut app.avail_state);

    // Hint for selected item
    if let Some(i) = app.avail_state.selected() {
        if let Some(pkg) = app.search_results.get(i) {
            let hint = if installed_names.contains(pkg.as_str()) {
                Line::from(vec![
                    Span::styled(" Already installed — ", Style::default().fg(C_GREEN)),
                    Span::styled("Enter", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(" to uninstall", Style::default().fg(C_GREEN)),
                ])
            } else {
                Line::from(vec![
                    Span::styled(" Press ", Style::default().fg(C_DIM)),
                    Span::styled("Enter", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(" to install", Style::default().fg(C_DIM)),
                ])
            };
            f.render_widget(Paragraph::new(hint), chunks[2]);
        }
    }
}

// ── Import tab ────────────────────────────────────────────────────────────────

fn draw_import(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL)
        .title(" Import Backup ").title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new("  Type or paste the path to a .zip backup file, then press Enter.")
            .style(Style::default().fg(C_DIM)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new("  ~ is expanded automatically.  Press Esc to clear.")
            .style(Style::default().fg(C_DIM)),
        chunks[1],
    );
    f.render_widget(Paragraph::new(""), chunks[2]);
    f.render_widget(
        Paragraph::new(format!("  {}{}", app.import_input, "█"))
            .block(Block::default().borders(Borders::ALL)
                .title(" Path ").title_style(Style::default().fg(Color::White))
                .border_style(Style::default().fg(C_ACCENT)))
            .style(Style::default().fg(Color::White)),
        chunks[3],
    );
    f.render_widget(
        Paragraph::new("  [Enter] Start import   [Tab] Switch tabs   [Shift+Q] Quit")
            .style(Style::default().fg(C_DIM)),
        chunks[4],
    );
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let hint = match app.tab {
        Tab::Installed => "[Tab/Shift+Tab] Switch  [r] Run  [d] Delete  [b] Backup  [c] Check  [u] Update  [s] Config  [q] Quit",
        Tab::Install => "[Tab/Shift+Tab] Switch  Type to search  [↓] Select  [Enter] Install/Uninstall  [q] Quit",
        Tab::Import => "[Tab/Shift+Tab] Switch  Type zip path  [Enter] Import  [Esc] Clear  [Shift+Q] Quit",
    };
    let msg = if app.status.is_empty() {
        format!(" {hint}")
    } else {
        format!(" {}  │  {hint}", app.status)
    };
    f.render_widget(Paragraph::new(msg).style(Style::default().fg(C_DIM)), area);
}

// ── Confirm overlay ───────────────────────────────────────────────────────────

fn draw_confirm(f: &mut Frame, area: Rect, title: &str, body: &[String], danger: bool) {
    let popup = centered_rect(52, 40, area);
    f.render_widget(Clear, popup);

    let (border_color, title_color) = if danger {
        (C_RED, C_RED)
    } else {
        (C_YELLOW, C_YELLOW)
    };

    let lines: Vec<Line> = body.iter().map(|l| Line::from(format!("  {l}"))).collect();
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL)
                .title(format!(" {title} "))
                .title_style(Style::default().fg(title_color).add_modifier(Modifier::BOLD))
                .border_style(Style::default().fg(border_color)))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

// ── Operation overlay ─────────────────────────────────────────────────────────

fn draw_operation(
    f: &mut Frame,
    area: Rect,
    app: &App,
    title: &str,
    log: &[String],
    done: bool,
    success: bool,
    total_bytes: Option<u64>,
    elapsed: std::time::Duration,
) {
    let popup = centered_rect(80, 70, area);
    f.render_widget(Clear, popup);

    let (border_color, status_line) = if !done {
        (C_ACCENT, format!(" Running… {:.1}s ", elapsed.as_secs_f32()))
    } else if success {
        (C_GREEN, " Done ✓  [Enter/q] Close ".to_string())
    } else {
        (C_RED, " Failed ✗  [Enter/q] Close ".to_string())
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(popup);

    let header_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
        .title(format!(" {title} "))
        .title_style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(border_color));

    if let Some(total) = total_bytes {
        let mb = total as f64 / 1_048_576.0;
        let ratio = if done { 1.0f64 } else { 0.0 };
        let label = if done {
            format!(" {mb:.1} MB — Done ")
        } else {
            let est = (mb / 20.0).max(1.0);
            let remaining = (est - elapsed.as_secs_f64()).max(0.0);
            format!(" {mb:.1} MB — ~{remaining:.0}s remaining ")
        };
        f.render_widget(
            Gauge::default().block(header_block)
                .gauge_style(Style::default().fg(border_color).bg(Color::Black))
                .ratio(ratio).label(label),
            chunks[0],
        );
    } else {
        let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = (elapsed.as_millis() / 100) as usize % spin.len();
        let spinner = if done { if success { "✓" } else { "✗" } } else { spin[frame] };
        f.render_widget(
            Paragraph::new(format!(" {spinner} {}", status_line.trim()))
                .block(header_block).style(Style::default().fg(border_color)),
            chunks[0],
        );
    }

    let log_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(border_color));
    let inner = log_block.inner(chunks[1]);
    f.render_widget(log_block, chunks[1]);

    let visible = inner.height as usize;
    let scroll = app.log_scroll.min(log.len().saturating_sub(visible));
    let lines: Vec<Line> = log.iter().skip(scroll).take(visible).map(|l| {
        let color = if l.starts_with("error") || l.contains("Error") || l.contains("failed") {
            C_RED
        } else if l.starts_with("warning") || l.contains("Warning") || l.starts_with('!') {
            C_YELLOW
        } else if l.contains("Done") || l.contains("complete") || l.contains("Updated") || l.contains("Saved") {
            C_GREEN
        } else {
            Color::White
        };
        Line::from(Span::styled(format!(" {l}"), Style::default().fg(color)))
    }).collect();
    f.render_widget(Paragraph::new(lines), inner);

    f.render_widget(
        Paragraph::new(status_line.as_str())
            .style(Style::default().fg(border_color).add_modifier(Modifier::BOLD))
            .alignment(Alignment::Center)
            .block(Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                .border_style(Style::default().fg(border_color))),
        chunks[2],
    );
}

// ── Config overlay ────────────────────────────────────────────────────────────

fn draw_config(f: &mut Frame, area: Rect, app_name: &str, config: &AppConfig, selected: usize) {
    let popup = centered_rect(54, 80, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" Config — {app_name} "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let b = |v: bool| if v { "  on " } else { " off " };
    let share_label = if config.shared_dirs.is_empty() {
        " none  →".to_string()
    } else {
        format!(" {}  →", config.shared_dirs.len())
    };
    let rows: Vec<(&str, String)> = vec![
        ("Network    ", b(config.network).to_string()),
        ("Camera     ", b(config.camera).to_string()),
        ("Microphone ", b(config.microphone).to_string()),
        ("Audio      ", b(config.audio).to_string()),
        ("Temp mode  ", match config.temp_mode {
            TempMode::System  => " system  ",
            TempMode::Ramdisk => " ramdisk ",
            TempMode::Local   => " local   ",
            TempMode::Uuid    => " uuid    ",
        }.to_string()),
        ("Temp delete", match config.temp_delete {
            LocalDelete::Never   => " never    ",
            LocalDelete::OnStart => " on_start ",
            LocalDelete::OnClose => " on_close ",
        }.to_string()),
        ("Shared dirs", share_label),
    ];

    let row_h = 2u16;

    for (idx, (label, value)) in rows.iter().enumerate() {
        let is_sel = idx == selected;
        let y = inner.y + idx as u16 * row_h;
        if y >= inner.y + inner.height.saturating_sub(3) { break; }

        let val_color = match value.trim() {
            "on"  => C_GREEN,
            "off" => C_RED,
            _     => C_YELLOW,
        };
        let bg = if is_sel { C_SELECT } else { Color::Reset };
        let row = Rect { x: inner.x, y, width: inner.width, height: 1 };

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if is_sel { " ▶ " } else { "   " }, Style::default().fg(C_ACCENT)),
                Span::styled(format!("{label}  "), Style::default().fg(if is_sel { Color::White } else { C_DIM }).bg(bg)),
                Span::styled(format!("[{value}]"), Style::default().fg(val_color).bg(bg)
                    .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() })),
            ])),
            row,
        );

        if y + 1 < inner.y + inner.height.saturating_sub(3) {
            f.render_widget(
                Paragraph::new(Span::styled("─".repeat(inner.width as usize), Style::default().fg(Color::Rgb(50, 50, 60)))),
                Rect { x: inner.x, y: y + 1, width: inner.width, height: 1 },
            );
        }
    }

    // Save button
    let save_y = inner.y + CFG_SAVE as u16 * row_h;
    if save_y + 1 < inner.y + inner.height {
        let is_sel = selected == CFG_SAVE;
        let btn_style = if is_sel {
            Style::default().fg(Color::Black).bg(C_GREEN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_GREEN)
        };
        let sep_y = save_y.saturating_sub(1);
        if sep_y > inner.y {
            f.render_widget(
                Paragraph::new(Span::styled("─".repeat(inner.width as usize), Style::default().fg(Color::Rgb(50, 50, 60)))),
                Rect { x: inner.x, y: sep_y, width: inner.width, height: 1 },
            );
        }
        let btn_area = Rect { x: inner.x, y: save_y, width: inner.width, height: 1 };
        let prefix = if is_sel { " ▶ " } else { "   " };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(C_ACCENT)),
                Span::styled("[ Save & Close ]", btn_style),
            ])),
            btn_area,
        );
    }

    // Microphone warning
    if !config.microphone && config.audio {
        let warn_y = inner.y + inner.height.saturating_sub(2);
        f.render_widget(
            Paragraph::new(Span::styled(
                "  ⚠  PipeWire/PA mic not blocked — set audio off",
                Style::default().fg(C_YELLOW),
            )),
            Rect { x: inner.x, y: warn_y, width: inner.width, height: 1 },
        );
    }

    // Footer
    let footer_y = inner.y + inner.height.saturating_sub(1);
    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓] Navigate  [Space/Enter] Toggle  [Esc/q] Discard",
            Style::default().fg(C_DIM),
        )),
        Rect { x: inner.x, y: footer_y, width: inner.width, height: 1 },
    );
}

// ── Shared dirs overlay ───────────────────────────────────────────────────────

fn draw_shared_dirs(f: &mut Frame, area: Rect, app_name: &str, dirs: &[String], selected: usize) {
    let popup = centered_rect(60, 70, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" Shared Folders — {app_name} "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    if dirs.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  No directories shared. Press [a] to add one.",
                Style::default().fg(C_DIM),
            )).wrap(Wrap { trim: false }),
            chunks[0],
        );
    } else {
        let items: Vec<ListItem> = dirs.iter().enumerate().map(|(i, d)| {
            let is_sel = i == selected;
            let style = if is_sel {
                Style::default().fg(Color::White).bg(C_SELECT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(C_ACCENT)
            };
            ListItem::new(Line::from(vec![
                Span::styled(if is_sel { " ▶ " } else { "   " }, Style::default().fg(C_ACCENT)),
                Span::styled(d.as_str(), style),
            ]))
        }).collect();

        let mut list_state = ListState::default();
        list_state.select(if dirs.is_empty() { None } else { Some(selected) });

        let list = List::new(items)
            .block(Block::default())
            .highlight_style(Style::default().bg(C_SELECT));
        f.render_stateful_widget(list, chunks[0], &mut list_state);
    }

    f.render_widget(
        Paragraph::new(Span::styled(
            " [a] Add  [d/Del] Remove  [Esc/q] Back",
            Style::default().fg(C_DIM),
        )),
        chunks[1],
    );
}

// ── File browser overlay ──────────────────────────────────────────────────────

fn draw_file_browser(
    f: &mut Frame,
    area: Rect,
    current_dir: &str,
    entries: &[(String, bool, bool)], // (name, is_dir, is_zip)
    selected: Option<usize>,
    pick_dir: bool,
) {
    let popup = centered_rect(70, 80, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" Browse: {current_dir} "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = entries.iter().map(|(name, is_dir, is_zip)| {
        if *is_dir {
            ListItem::new(Line::from(vec![
                Span::styled("📁 ", Style::default().fg(C_YELLOW)),
                Span::styled(name.as_str(), Style::default().fg(C_YELLOW)),
                Span::styled("/", Style::default().fg(C_DIM)),
            ]))
        } else if *is_zip {
            ListItem::new(Line::from(vec![
                Span::styled("📦 ", Style::default().fg(C_GREEN)),
                Span::styled(name.as_str(), Style::default().fg(C_GREEN)),
            ]))
        } else {
            ListItem::new(Line::from(vec![
                Span::raw("   "),
                Span::styled(name.as_str(), Style::default().fg(C_DIM)),
            ]))
        }
    }).collect();

    let mut list_state = ListState::default();
    list_state.select(selected);

    let list = List::new(items)
        .block(Block::default())
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[0], &mut list_state);

    let footer = if pick_dir {
        " [↑↓/jk] Navigate  [Enter/→] Open dir  [Space/s] Select this dir  [Esc] Cancel"
    } else {
        " [↑↓/jk] Navigate  [Enter/→] Open  [Backspace/←] Up  [q/Esc] Cancel"
    };
    f.render_widget(
        Paragraph::new(Span::styled(footer, Style::default().fg(C_DIM))),
        chunks[1],
    );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area)[1];

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v)[1]
}
