use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Tabs, Wrap,
    },
    Frame,
};

use super::{App, Screen, Tab};

// Colour palette
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
            Constraint::Length(3), // tab bar
            Constraint::Min(0),    // content
            Constraint::Length(1), // status / hint bar
        ])
        .split(area);

    draw_tabs(f, app, chunks[0]);

    match app.tab {
        Tab::Installed => draw_installed(f, app, chunks[1]),
        Tab::Install => draw_install(f, app, chunks[1]),
    }

    draw_statusbar(f, app, chunks[2]);

    // Overlays
    match &app.screen {
        Screen::Main => {}
        Screen::Confirm { title, body, .. } => {
            let title = title.clone();
            let body = body.clone();
            draw_confirm(f, area, &title, &body);
        }
        Screen::Operation {
            title,
            log,
            done,
            success,
            total_bytes,
            started,
            ..
        } => {
            let title = title.clone();
            let log = log.clone();
            let done = *done;
            let success = *success;
            let total_bytes = *total_bytes;
            let elapsed = started.elapsed();
            draw_operation(f, area, app, &title, &log, done, success, total_bytes, elapsed);
        }
        Screen::FileInput { prompt, input } => {
            let prompt = prompt.clone();
            let input = input.clone();
            draw_file_input(f, area, &prompt, &input);
        }
    }
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = vec![
        Line::from(vec![
            Span::raw(" "),
            Span::styled("Installed", Style::default().fg(C_ACCENT)),
            Span::raw(" "),
        ]),
        Line::from(vec![
            Span::raw(" "),
            Span::styled("Install", Style::default().fg(C_ACCENT)),
            Span::raw(" "),
        ]),
    ];
    let sel = match app.tab {
        Tab::Installed => 0,
        Tab::Install => 1,
    };
    let tabs = Tabs::new(titles)
        .select(sel)
        .block(Block::default().borders(Borders::ALL).title(" wryayer ").title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)))
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

    // Left: app list
    let items: Vec<ListItem> = app
        .installed
        .iter()
        .map(|m| {
            let has_update = app.update_available.contains_key(&m.app.name);
            let indicator = if has_update {
                Span::styled(" ●", Style::default().fg(C_YELLOW))
            } else {
                Span::raw("  ")
            };
            let name = Span::styled(&m.app.name, Style::default().fg(Color::White));
            ListItem::new(Line::from(vec![indicator, name]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Apps ")
                .title_style(Style::default().fg(C_ACCENT)),
        )
        .highlight_style(
            Style::default()
                .bg(C_SELECT)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[0], &mut app.inst_state);

    // Right: detail pane
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
        let p = Paragraph::new("No app selected.")
            .style(Style::default().fg(C_DIM))
            .alignment(Alignment::Center);
        f.render_widget(p, inner);
        return;
    };

    let ver = m
        .packages
        .iter()
        .find(|p| p.name == m.app.name)
        .map(|p| p.version.as_str())
        .unwrap_or("?");

    let installed = m.app.installed_at.get(..10).unwrap_or(&m.app.installed_at);
    let launchers = m.app.launchers.join(", ");

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("  Name:       ", Style::default().fg(C_DIM)),
            Span::styled(&m.app.name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("  Version:    ", Style::default().fg(C_DIM)),
            Span::styled(ver, Style::default().fg(C_GREEN)),
        ]),
        Line::from(vec![
            Span::styled("  Installed:  ", Style::default().fg(C_DIM)),
            Span::raw(installed),
        ]),
        Line::from(vec![
            Span::styled("  Launchers:  ", Style::default().fg(C_DIM)),
            Span::raw(launchers),
        ]),
        Line::from(vec![
            Span::styled("  Packages:   ", Style::default().fg(C_DIM)),
            Span::styled(
                m.packages.len().to_string(),
                Style::default().fg(C_ACCENT),
            ),
        ]),
    ];

    if let Some(new_ver) = app.update_available.get(&m.app.name) {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  Update:     ", Style::default().fg(C_DIM)),
            Span::styled(format!("{ver} → {new_ver}"), Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  [r] Run  [d] Delete  [b] Backup  [c] Check  [u] Update",
        Style::default().fg(C_DIM),
    )));

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

// ── Install tab ───────────────────────────────────────────────────────────────

fn draw_install(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    // Search bar
    let cursor_char = if app.search_focused { "█" } else { "" };
    let search_block = Block::default()
        .borders(Borders::ALL)
        .title(" Search (Enter to run) ")
        .title_style(Style::default().fg(if app.search_focused { Color::White } else { C_DIM }))
        .border_style(Style::default().fg(if app.search_focused { C_ACCENT } else { C_DIM }));
    let search_text = Paragraph::new(format!("{}{}", app.search_input, cursor_char))
        .block(search_block)
        .style(Style::default().fg(Color::White));
    f.render_widget(search_text, chunks[0]);

    // Results list
    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .map(|pkg| {
            ListItem::new(Span::styled(pkg.as_str(), Style::default().fg(Color::White)))
        })
        .collect();

    let hint = if app.search_results.is_empty() {
        " Results "
    } else {
        " Results — Enter to install, [i] Import zip "
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(hint)
                .title_style(Style::default().fg(C_ACCENT)),
        )
        .highlight_style(
            Style::default()
                .bg(C_SELECT)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[1], &mut app.avail_state);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let hint = match app.tab {
        Tab::Installed => " [Tab] Switch  [r] Run  [d] Delete  [b] Backup  [c] Check  [u] Update  [q] Quit",
        Tab::Install => " [Tab] Switch  [/] Search  [Enter] Install  [i] Import zip  [q] Quit",
    };

    let msg = if app.status.is_empty() {
        hint.to_string()
    } else {
        format!(" {}  |{hint}", app.status)
    };

    let p = Paragraph::new(msg).style(Style::default().fg(C_DIM));
    f.render_widget(p, area);
}

// ── Confirm overlay ───────────────────────────────────────────────────────────

fn draw_confirm(f: &mut Frame, area: Rect, title: &str, body: &[String]) {
    let popup = centered_rect(50, 40, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .title_style(Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_YELLOW));

    let lines: Vec<Line> = body
        .iter()
        .map(|l| Line::from(format!("  {l}")))
        .collect();

    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(p, popup);
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
        .constraints([
            Constraint::Length(3), // header / progress
            Constraint::Min(0),    // log
            Constraint::Length(1), // status line
        ])
        .split(popup);

    // Header with optional progress bar
    let header_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
        .title(format!(" {title} "))
        .title_style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(border_color));

    if let Some(total) = total_bytes {
        // Show estimate from elapsed + size
        let mb = total as f64 / 1_048_576.0;
        let ratio = if total > 0 && done { 1.0 } else { 0.0 };
        let label = if done {
            format!(" {:.1} MB — Done ", mb)
        } else {
            let secs = elapsed.as_secs_f64();
            // rough: assume ~20 MB/s compression
            let est = (mb / 20.0).max(1.0);
            let remaining = (est - secs).max(0.0);
            format!(" {mb:.1} MB — ~{remaining:.0}s remaining ")
        };
        let gauge = Gauge::default()
            .block(header_block)
            .gauge_style(Style::default().fg(border_color).bg(Color::Black))
            .ratio(ratio)
            .label(label);
        f.render_widget(gauge, chunks[0]);
    } else {
        // Spinner
        let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = (elapsed.as_millis() / 100) as usize % spin.len();
        let spinner = if done { "✓" } else { spin[frame] };
        let header_text = Paragraph::new(format!(" {spinner} {}", status_line.trim()))
            .block(header_block)
            .style(Style::default().fg(border_color));
        f.render_widget(header_text, chunks[0]);
    }

    // Log
    let log_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_style(Style::default().fg(border_color));

    let inner = log_block.inner(chunks[1]);
    f.render_widget(log_block, chunks[1]);

    let visible = inner.height as usize;
    let total_lines = log.len();
    let scroll = app
        .log_scroll
        .min(total_lines.saturating_sub(visible));
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

    let log_p = Paragraph::new(lines);
    f.render_widget(log_p, inner);

    // Bottom status
    let status = Paragraph::new(status_line.as_str())
        .style(Style::default().fg(border_color).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM).border_style(Style::default().fg(border_color)));
    f.render_widget(status, chunks[2]);
}

// ── File input overlay ────────────────────────────────────────────────────────

fn draw_file_input(f: &mut Frame, area: Rect, prompt: &str, input: &str) {
    let popup = centered_rect(60, 20, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Import Backup ")
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));

    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(format!("  {prompt}")).style(Style::default().fg(C_DIM)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(format!("  {input}█")).style(Style::default().fg(Color::White)),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new("  [Enter] Confirm  [Esc] Cancel").style(Style::default().fg(C_DIM)),
        chunks[2],
    );
}

// ── Layout helper ─────────────────────────────────────────────────────────────

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
