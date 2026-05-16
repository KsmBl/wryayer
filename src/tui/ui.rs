use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Tabs, Wrap},
    Frame,
};

use super::{App, Screen, Tab};

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
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    draw_tabs(f, app, chunks[0]);

    match app.tab {
        Tab::Installed => draw_installed(f, app, chunks[1]),
        Tab::Install => draw_install(f, app, chunks[1]),
        Tab::Import => draw_import(f, app, chunks[1]),
    }

    draw_statusbar(f, app, chunks[2]);

    // Overlays (drawn last so they appear on top)
    match &app.screen {
        Screen::Main => {}
        Screen::Confirm { title, body, .. } => {
            let title = title.clone();
            let body = body.clone();
            draw_confirm(f, area, &title, &body);
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
    }
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mk = |label: &str| {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(label.to_string(), Style::default().fg(C_ACCENT)),
            Span::raw(" "),
        ])
    };
    let titles = vec![mk("Installed"), mk("Install"), mk("Import")];
    let sel = match app.tab {
        Tab::Installed => 0,
        Tab::Install => 1,
        Tab::Import => 2,
    };
    let tabs = Tabs::new(titles)
        .select(sel)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" wryayer ")
                .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
                .bg(C_SELECT),
        )
        .divider(Span::styled("|", Style::default().fg(C_DIM)));
    f.render_widget(tabs, area);
}

// ── Installed tab ─────────────────────────────────────────────────────────────

fn draw_installed(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    let items: Vec<ListItem> = app
        .installed
        .iter()
        .map(|m| {
            let dot = if app.update_available.contains_key(&m.app.name) {
                Span::styled(" ●", Style::default().fg(C_YELLOW))
            } else {
                Span::raw("  ")
            };
            ListItem::new(Line::from(vec![dot, Span::styled(&m.app.name, Style::default().fg(Color::White))]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Apps ").title_style(Style::default().fg(C_ACCENT)))
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[0], &mut app.inst_state);
    draw_detail(f, app, chunks[1]);
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Details ")
        .title_style(Style::default().fg(C_ACCENT));
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
        Line::from(vec![
            Span::styled("  Version:    ", dim),
            Span::styled(ver, Style::default().fg(C_GREEN)),
        ]),
        Line::from(vec![
            Span::styled("  Installed:  ", dim),
            Span::raw(installed),
        ]),
        Line::from(vec![
            Span::styled("  Launchers:  ", dim),
            Span::raw(launchers),
        ]),
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
        "  [r] Run  [d] Delete  [b] Backup  [c] Check  [u] Update",
        dim,
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ── Install tab ───────────────────────────────────────────────────────────────

fn draw_install(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    // Search bar — active border when typing, dim when list is focused
    let bar_active = !app.search_list_focused;
    let cursor = if bar_active { "█" } else { "" };
    let search_title = if app.search_searching {
        " Search — searching… "
    } else {
        " Search "
    };
    let search_widget = Paragraph::new(format!("{}{}", app.search_input, cursor))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(search_title)
                .title_style(Style::default().fg(if bar_active { Color::White } else { C_DIM }))
                .border_style(Style::default().fg(if bar_active { C_ACCENT } else { C_DIM })),
        )
        .style(Style::default().fg(Color::White));
    f.render_widget(search_widget, chunks[0]);

    // Results
    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .map(|pkg| ListItem::new(Span::styled(pkg.as_str(), Style::default().fg(Color::White))))
        .collect();

    let results_title = if app.search_results.is_empty() {
        " Results "
    } else {
        " Results — [↓] Select  [Enter] Install "
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(results_title)
                .title_style(Style::default().fg(C_ACCENT)),
        )
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[1], &mut app.avail_state);
}

// ── Import tab ────────────────────────────────────────────────────────────────

fn draw_import(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Import Backup ")
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new("  Paste or type the path to a .zip backup file, then press Enter.")
            .style(Style::default().fg(C_DIM)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new("  Supports ~ expansion.").style(Style::default().fg(C_DIM)),
        chunks[1],
    );
    f.render_widget(Paragraph::new(""), chunks[2]);
    f.render_widget(
        Paragraph::new(format!("  {}{}", app.import_input, "█"))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Path ")
                    .title_style(Style::default().fg(Color::White))
                    .border_style(Style::default().fg(C_ACCENT)),
            )
            .style(Style::default().fg(Color::White)),
        chunks[3],
    );
    f.render_widget(
        Paragraph::new("  [Enter] Start import   [Tab] Switch tabs   [q] Quit")
            .style(Style::default().fg(C_DIM)),
        chunks[4],
    );
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let hint = match app.tab {
        Tab::Installed => "[Tab] Next tab  [r] Run  [d] Delete  [b] Backup  [c] Check  [u] Update  [q] Quit",
        Tab::Install => "[Tab] Next tab  Type to search  [↓] Select result  [Enter] Install  [q] Quit",
        Tab::Import => "[Tab] Next tab  Type zip path  [Enter] Import  [q] Quit",
    };

    let msg = if app.status.is_empty() {
        format!(" {hint}")
    } else {
        format!(" {}  │  {hint}", app.status)
    };

    f.render_widget(Paragraph::new(msg).style(Style::default().fg(C_DIM)), area);
}

// ── Confirm overlay ───────────────────────────────────────────────────────────

fn draw_confirm(f: &mut Frame, area: Rect, title: &str, body: &[String]) {
    let popup = centered_rect(50, 40, area);
    f.render_widget(Clear, popup);

    let lines: Vec<Line> = body.iter().map(|l| Line::from(format!("  {l}"))).collect();
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} "))
                    .title_style(Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD))
                    .border_style(Style::default().fg(C_YELLOW)),
            )
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
            Gauge::default()
                .block(header_block)
                .gauge_style(Style::default().fg(border_color).bg(Color::Black))
                .ratio(ratio)
                .label(label),
            chunks[0],
        );
    } else {
        let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = (elapsed.as_millis() / 100) as usize % spin.len();
        let spinner = if done { if success { "✓" } else { "✗" } } else { spin[frame] };
        f.render_widget(
            Paragraph::new(format!(" {spinner} {}", status_line.trim()))
                .block(header_block)
                .style(Style::default().fg(border_color)),
            chunks[0],
        );
    }

    // Log area
    let log_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(border_color));
    let inner = log_block.inner(chunks[1]);
    f.render_widget(log_block, chunks[1]);

    let visible = inner.height as usize;
    let scroll = app.log_scroll.min(log.len().saturating_sub(visible));
    let lines: Vec<Line> = log
        .iter()
        .skip(scroll)
        .take(visible)
        .map(|l| {
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
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);

    // Bottom status bar of the overlay
    f.render_widget(
        Paragraph::new(status_line.as_str())
            .style(Style::default().fg(border_color).add_modifier(Modifier::BOLD))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                    .border_style(Style::default().fg(border_color)),
            ),
        chunks[2],
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
