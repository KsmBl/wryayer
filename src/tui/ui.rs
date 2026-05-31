use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame,
};

use crate::commands::dedup::format_bytes;
use crate::config::{AppConfig, LocalDelete, TempMode};

use super::{
    App, Screen, Tab, CFG_SAVE, CFG_SHARES, CFG_GAME_EXE, CFG_GAME_PREFIX,
    app_cfg_save_idx, setting_description, setting_options, setting_current, setting_title,
    HOSTNAME_SAMPLE, MACHINE_ID_SAMPLE, USERNAME_SAMPLE,
};

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
        Tab::Install   => draw_install(f, app, chunks[1]),
        Tab::Import    => draw_import(f, app, chunks[1]),
        Tab::Games     => draw_games(f, app, chunks[1]),
        Tab::Space     => draw_space(f, app, chunks[1]),
        Tab::Settings  => draw_settings_tab(f, app, chunks[1]),
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
        Screen::Operation { title, log, done, success, total_bytes, progress, started, show_log, launcher_choice, .. } => {
            let _ = launcher_choice; // handled by the event loop auto-transition
            let title = title.clone();
            let log = log.clone();
            let done = *done;
            let success = *success;
            let total_bytes = *total_bytes;
            let progress = *progress;
            let elapsed = started.elapsed();
            let show_log = *show_log;
            draw_operation(f, area, app, &title, &log, done, success, total_bytes, progress, elapsed, show_log);
        }
        Screen::Config { app_name, config, selected } => {
            let app_name = app_name.clone();
            let config = config.clone();
            let selected = *selected;
            let wine_game = app.editing_wine_game.clone();
            draw_config(f, area, &app_name, &config, selected, wine_game.as_ref());
        }
        Screen::SharedDirs { app_name, dirs, selected } => {
            let app_name = app_name.clone();
            let dirs = dirs.clone();
            let selected = *selected;
            draw_shared_dirs(f, area, &app_name, &dirs, selected);
        }
        Screen::FileBrowser { current_dir, entries, fb_state, mode } => {
            let title = current_dir.to_string_lossy().into_owned();
            let entries: Vec<(String, bool, bool)> = entries
                .iter()
                .map(|e| (e.name.clone(), e.is_dir, e.is_zip))
                .collect();
            let sel = fb_state.selected();
            let pick_dir = !matches!(mode, super::BrowserMode::ImportZip);
            draw_file_browser(f, area, &title, &entries, sel, pick_dir);
        }
        Screen::GameExePick { game_dir, exes, selected } => {
            let game_dir = game_dir.to_string_lossy().into_owned();
            let exes = exes.clone();
            let selected = *selected;
            draw_game_exe_pick(f, area, &game_dir, &exes, selected);
        }
        Screen::GameNameInput { game_dir, exe, value } => {
            let game_dir = game_dir.to_string_lossy().into_owned();
            let exe = exe.clone();
            let value = value.clone();
            draw_game_name_input(f, area, &game_dir, &exe, &value);
        }
        Screen::GameConfirm { game_dir, exe, app_name, delete_source, selected } => {
            let game_dir = game_dir.to_string_lossy().into_owned();
            let exe = exe.clone();
            let app_name = app_name.clone();
            let delete_source = *delete_source;
            let selected = *selected;
            draw_game_confirm(f, area, &game_dir, &exe, &app_name, delete_source, selected);
        }
        Screen::InstallTarget { pkg, targets, selected } => {
            let pkg = pkg.clone();
            let targets = targets.clone();
            let selected = *selected;
            draw_install_target(f, area, &pkg, &targets, selected);
        }
        Screen::OptionPicker { app_name, config, setting_idx, selected } => {
            let app_name = app_name.clone();
            let config = config.clone();
            let setting_idx = *setting_idx;
            let selected = *selected;
            let wine_game = app.editing_wine_game.clone();
            // For app configs draw the Config popup as backing; for the global
            // Settings tab the 2-panel background is already rendered.
            if !app_name.is_empty() {
                draw_config(f, area, &app_name, &config, setting_idx, wine_game.as_ref());
            }
            draw_option_picker(f, area, setting_idx, selected, &config);
        }
        Screen::SettingHelp { app_name, config, back_selected } => {
            let app_name = app_name.clone();
            let config = config.clone();
            let back_selected = *back_selected;
            let wine_game = app.editing_wine_game.clone();
            if !app_name.is_empty() {
                draw_config(f, area, &app_name, &config, back_selected, wine_game.as_ref());
            }
            draw_setting_help(f, area, back_selected);
        }
        Screen::OptionHelp { app_name, config, setting_idx, picker_selected } => {
            let app_name = app_name.clone();
            let config = config.clone();
            let setting_idx = *setting_idx;
            let picker_selected = *picker_selected;
            let wine_game = app.editing_wine_game.clone();
            if !app_name.is_empty() {
                draw_config(f, area, &app_name, &config, setting_idx, wine_game.as_ref());
            }
            draw_option_picker(f, area, setting_idx, picker_selected, &config);
            draw_option_help(f, area, setting_idx, picker_selected);
        }
        Screen::TextInput { app_name, config, back_selected, field_idx, value } => {
            let app_name = app_name.clone();
            let config = config.clone();
            let back_selected = *back_selected;
            let field_idx = *field_idx;
            let value = value.clone();
            let wine_game = app.editing_wine_game.clone();
            if !app_name.is_empty() {
                draw_config(f, area, &app_name, &config, back_selected, wine_game.as_ref());
            }
            let title = match field_idx {
                CFG_GAME_EXE    => "Game Exe path",
                CFG_GAME_PREFIX => "WINEPREFIX path",
                _ => super::setting_title(field_idx),
            };
            draw_text_input(f, area, title, &value);
        }
        Screen::KeyHelp => {
            draw_key_help(f, area);
        }
        Screen::RenameApp { app_name, value } => {
            let app_name = app_name.clone();
            let value = value.clone();
            draw_rename_app(f, area, &app_name, &value);
        }
        Screen::DuplicateInstall { pkg, value, into } => {
            let pkg = pkg.clone();
            let value = value.clone();
            let into = into.clone();
            draw_duplicate_install(f, area, &pkg, &value, into.as_deref());
        }
        Screen::AlreadyInstalled { pkg, selected } => {
            let pkg = pkg.clone();
            let selected = *selected;
            draw_already_installed(f, area, &pkg, selected);
        }
        Screen::NoLauncherChoice { pkg, available_bins, selected, .. } => {
            let pkg = pkg.clone();
            let available_bins = available_bins.clone();
            let selected = *selected;
            draw_no_launcher_choice(f, area, &pkg, &available_bins, selected);
        }
        Screen::OutdatedPackages { pkg, selected, .. } => {
            let pkg = pkg.clone();
            let selected = *selected;
            draw_outdated_packages(f, area, &pkg, selected);
        }
        Screen::AskShortcut { pkg, selected, .. } => {
            let pkg = pkg.clone();
            let selected = *selected;
            draw_ask_shortcut(f, area, &pkg, selected);
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
    let titles = vec![mk("Installed"), mk("Install"), mk("Import"), mk("Games"), mk("Space"), mk("Settings")];
    let sel = match app.tab { Tab::Installed => 0, Tab::Install => 1, Tab::Import => 2, Tab::Games => 3, Tab::Space => 4, Tab::Settings => 5 };
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

    let list_active = !app.detail_focused;
    let list_fg = if list_active { Color::White } else { C_DIM };
    let list_border = if list_active { C_ACCENT } else { C_DIM };

    let items: Vec<ListItem> = app.installed.iter().enumerate().map(|(i, m)| {
        let dot = if app.update_available.contains_key(&m.app.name) {
            Span::styled("●", Style::default().fg(if list_active { C_YELLOW } else { C_DIM }))
        } else {
            Span::raw(" ")
        };
        if let Some(ref target) = m.app.alias_of {
            let is_last = app.installed.get(i + 1)
                .map(|next| next.app.alias_of.as_deref() != Some(target.as_str()))
                .unwrap_or(true);
            let connector = if is_last { "  └── " } else { "  ├── " };
            let mut spans = vec![dot, Span::styled(connector, Style::default().fg(C_DIM))];
            if let Some(ref dn) = m.app.display_name {
                spans.push(Span::styled(dn.clone(), Style::default().fg(list_fg)));
                spans.push(Span::styled(format!(" [{}]", m.app.name), Style::default().fg(C_DIM)));
            } else {
                spans.push(Span::styled(&m.app.name, Style::default().fg(C_DIM)));
            }
            ListItem::new(Line::from(spans))
        } else if let Some(ref dn) = m.app.display_name {
            ListItem::new(Line::from(vec![
                dot,
                Span::styled(format!(" {}", dn), Style::default().fg(list_fg)),
                Span::styled(format!(" [{}]", m.app.name), Style::default().fg(C_DIM)),
            ]))
        } else if let Some(ref pn) = m.app.pkg_name {
            ListItem::new(Line::from(vec![
                dot,
                Span::styled(format!(" {}", m.app.name), Style::default().fg(list_fg)),
                Span::styled(format!(" [{}]", pn), Style::default().fg(C_DIM)),
            ]))
        } else {
            ListItem::new(Line::from(vec![
                dot,
                Span::styled(format!(" {}", m.app.name), Style::default().fg(list_fg)),
            ]))
        }
    }).collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Apps ")
            .title_style(Style::default().fg(list_border))
            .border_style(Style::default().fg(list_border)))
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[0], &mut app.inst_state);
    draw_detail(f, app, chunks[1]);
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.detail_focused;
    let border_color = if focused { C_ACCENT } else { C_DIM };
    let title_style = Style::default().fg(border_color);
    let block = Block::default().borders(Borders::ALL)
        .title(" Details ").title_style(title_style)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(m) = app.selected_installed() else {
        f.render_widget(
            Paragraph::new("No app selected.").style(Style::default().fg(C_DIM)).alignment(Alignment::Center),
            inner,
        );
        return;
    };

    let real_pkg = m.app.pkg_name.as_deref().unwrap_or(&m.app.name);
    let ver = m.packages.iter().find(|p| p.name == real_pkg)
        .map(|p| p.version.as_str()).unwrap_or("?");
    let installed = m.app.installed_at.get(..10).unwrap_or(&m.app.installed_at);
    let has_launcher = !m.app.main_binary.is_empty();
    let dim = Style::default().fg(C_DIM);

    let size_str = app.app_sizes.get(&m.app.name)
        .map(|&b| format_bytes(b))
        .unwrap_or_else(|| "—".to_string());

    let name_line = if let Some(ref dn) = m.app.display_name {
        Line::from(vec![
            Span::styled("  Name:       ", dim),
            Span::styled(dn.as_str(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  [{}]", m.app.name), Style::default().fg(C_DIM)),
        ])
    } else if let Some(ref pn) = m.app.pkg_name {
        Line::from(vec![
            Span::styled("  Name:       ", dim),
            Span::styled(m.app.name.as_str(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  [{}]", pn), Style::default().fg(C_DIM)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  Name:       ", dim),
            Span::styled(m.app.name.as_str(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ])
    };

    let launchers_line = if m.app.launchers.is_empty() {
        Line::from(vec![
            Span::styled("  Launchers:  ", dim),
            Span::styled("none", Style::default().fg(C_DIM)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  Launchers:  ", dim),
            Span::raw(m.app.launchers.join(", ")),
        ])
    };

    let mut lines = vec![
        name_line,
        Line::from(vec![Span::styled("  Version:    ", dim), Span::styled(ver, Style::default().fg(C_GREEN))]),
        Line::from(vec![Span::styled("  Installed:  ", dim), Span::raw(installed)]),
        launchers_line,
        Line::from(vec![Span::styled("  Size:       ", dim), Span::styled(size_str, Style::default().fg(C_ACCENT))]),
    ];

    if let Some(new_ver) = app.update_available.get(&m.app.name) {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  Update:     ", dim),
            Span::styled(format!("{ver} → {new_ver}"), Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD)),
        ]));
    }

    // Snapshot list
    let home = std::env::var("HOME").unwrap_or_default();
    let snap_dir = format!("{home}/.wryayer/{}/.snapshots", m.app.name);
    let mut snap_labels: Vec<String> = std::fs::read_dir(&snap_dir)
        .into_iter().flatten().flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    snap_labels.sort_by(|a, b| b.cmp(a)); // newest first

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            format!("  Snapshots ({}):", snap_labels.len()),
            Style::default().fg(C_DIM),
        ),
    ]));
    if snap_labels.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("    none", dim),
        ]));
    } else {
        for label in &snap_labels {
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled(label.as_str(), Style::default().fg(Color::White)),
            ]));
        }
    }

    // Package list
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            format!("  Packages ({}):", m.packages.len()),
            Style::default().fg(C_DIM),
        ),
    ]));
    let max_name = m.packages.iter().map(|p| p.name.len()).max().unwrap_or(0).min(24);
    for pkg in &m.packages {
        let name: String = pkg.name.chars().take(24).collect();
        lines.push(Line::from(vec![
            Span::styled(format!("    {name:<max_name$}  "), dim),
            Span::styled(&pkg.version, Style::default().fg(Color::White)),
        ]));
    }

    lines.push(Line::raw(""));
    if has_launcher {
        lines.push(Line::from(Span::styled(
            "  [r] Run  [d] Delete  [e] Export  [p] Snapshot  [o] Rollback",
            dim,
        )));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  No launcher — reinstall with ", Style::default().fg(C_YELLOW)),
            Span::styled("--bin-names <name>", Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(Span::styled(
            "  [d] Delete  [e] Export  [p] Snapshot  [o] Rollback",
            dim,
        )));
    }
    lines.push(Line::from(Span::styled(
        "  [c] Check  [u] Update  [s] Config",
        dim,
    )));

    let total = lines.len();
    let visible = inner.height as usize;
    let clamped = app.detail_scroll.min(total.saturating_sub(visible));
    f.render_widget(Paragraph::new(lines).scroll((clamped as u16, 0)), inner);
    app.detail_scroll = clamped; // write back so up-scrolling is immediate
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

    let items: Vec<ListItem> = app.search_results.iter().enumerate().map(|(_i, (pkg, repo))| {
        let is_marked = app.selected_pkgs.contains(pkg.as_str());
        let repo_span = repo.as_deref().map(|r| {
            Span::styled(format!(" [{}]", r), Style::default().fg(C_DIM))
        });
        if installed_names.contains(pkg.as_str()) {
            let mut spans = vec![
                Span::styled("✓ ", Style::default().fg(C_GREEN)),
                Span::styled(pkg.as_str(), Style::default().fg(Color::White)),
            ];
            if let Some(rs) = repo_span { spans.push(rs); }
            spans.push(Span::styled(" [installed]", Style::default().fg(C_GREEN)));
            ListItem::new(Line::from(spans))
        } else if is_marked {
            let mut spans = vec![
                Span::styled("◉ ", Style::default().fg(C_ACCENT)),
                Span::styled(pkg.as_str(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            ];
            if let Some(rs) = repo_span { spans.push(rs); }
            spans.push(Span::styled(" [marked]", Style::default().fg(C_ACCENT)));
            ListItem::new(Line::from(spans))
        } else {
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(pkg.as_str(), Style::default().fg(Color::White)),
            ];
            if let Some(rs) = repo_span { spans.push(rs); }
            ListItem::new(Line::from(spans))
        }
    }).collect();

    let results_title = if app.search_results.is_empty() {
        " Results "
    } else if !app.selected_pkgs.is_empty() {
        " Results — [Space] Mark/Unmark  [Enter] Install all marked "
    } else {
        " Results — [↓] Select  [Space] Mark  [Enter] Install "
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(results_title).title_style(Style::default().fg(C_ACCENT)))
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[1], &mut app.avail_state);

    // Hint line: marked count takes priority
    if !app.selected_pkgs.is_empty() {
        let n = app.selected_pkgs.len();
        let hint = Line::from(vec![
            Span::styled(format!(" {n} marked — press "), Style::default().fg(C_ACCENT)),
            Span::styled("Enter", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(" to install all, ", Style::default().fg(C_ACCENT)),
            Span::styled("Space", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(" to toggle", Style::default().fg(C_ACCENT)),
        ]);
        f.render_widget(Paragraph::new(hint), chunks[2]);
    } else if let Some(i) = app.avail_state.selected() {
        if let Some((pkg, _)) = app.search_results.get(i) {
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
                    Span::styled(" to install, ", Style::default().fg(C_DIM)),
                    Span::styled("Space", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(" to mark", Style::default().fg(C_DIM)),
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
        Tab::Installed if app.detail_focused => "[↑↓] Scroll  [←/Esc] Back  [q] Quit",
        Tab::Installed => "[Tab] Switch  [→] Details  [r] Run  [d] Delete  [e] Export  [p] Snapshot  [o] Rollback  [c] Check  [u] Update  [s] Config  [n] Rename  [?] Help  [q] Quit",
        Tab::Install   => "[Tab] Switch  Type to search  [↓] Select  [Enter] Install/Uninstall  [q] Quit",
        Tab::Import    => "[Tab] Switch  Type zip path  [Enter] Import  [Esc] Clear  [Shift+Q] Quit",
        Tab::Games     => "[Tab] Switch  [↑↓] Navigate  [Enter/r] Run  [s] Settings  [d] Delete  [i/a] Import  [q] Quit",
        Tab::Space     => "[Tab] Switch  [r] Run dedup  [q] Quit",
        Tab::Settings  => "[Tab] Switch  [↑↓] Navigate  [←/→] Cycle  [Enter] Edit  [?] Help  [q] Quit",
    };
    let mut spans: Vec<Span> = vec![];
    if app.konami_mode {
        spans.push(Span::styled(
            " ★ konami mode ",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" │ ", Style::default().fg(C_DIM)));
    }
    if !app.status.is_empty() {
        spans.push(Span::styled(format!(" {} ", app.status), Style::default().fg(Color::White)));
        spans.push(Span::styled(" │ ", Style::default().fg(C_DIM)));
    }
    spans.push(Span::styled(format!(" {hint}"), Style::default().fg(C_DIM)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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

fn log_line_color(l: &str) -> Color {
    if l.starts_with("error") || l.contains("Error") || l.contains("failed") {
        C_RED
    } else if l.starts_with("warning") || l.contains("Warning") || l.starts_with('!') {
        C_YELLOW
    } else if l.contains("Done") || l.contains("complete") || l.contains("Updated") || l.contains("Saved") {
        C_GREEN
    } else {
        Color::White
    }
}

fn draw_operation(
    f: &mut Frame,
    area: Rect,
    app: &App,
    title: &str,
    log: &[String],
    done: bool,
    success: bool,
    total_bytes: Option<u64>,
    progress: Option<(u64, u64)>,
    elapsed: std::time::Duration,
    show_log: bool,
) {
    // ── Konami mode: take over the full operation overlay with animations ─────
    if app.konami_mode && !show_log {
        let kind = crate::tui::konami::Anim::from_title(title);
        draw_konami_overlay(f, area, kind, elapsed, done, success);
        return;
    }

    let border_color = if !done { C_ACCENT } else if success { C_GREEN } else { C_RED };

    let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let frame = (elapsed.as_millis() / 100) as usize % spin.len();
    let spinner = if done { if success { "✓" } else { "✗" } } else { spin[frame] };

    if show_log {
        // ── Log view: large popup with scrollable terminal ────────────────────
        let popup = centered_rect(80, 70, area);
        f.render_widget(Clear, popup);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
            .split(popup);

        let header_block = Block::default()
            .borders(Borders::LEFT | Borders::RIGHT | Borders::TOP)
            .title(format!(" {title} "))
            .title_style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
            .border_style(Style::default().fg(border_color));

        if let Some((d_now, d_total)) = progress.filter(|(_, t)| *t > 0) {
            let ratio = (d_now as f64 / d_total as f64).clamp(0.0, 1.0);
            let label = if done {
                format!(" {d_total}/{d_total} — Done ")
            } else {
                let eta = eta_seconds(d_now, d_total, elapsed);
                format!(" {d_now}/{d_total} — ~{eta:.0}s remaining ")
            };
            f.render_widget(
                Gauge::default().block(header_block)
                    .gauge_style(Style::default().fg(border_color).bg(Color::Black))
                    .ratio(ratio).label(label),
                chunks[0],
            );
        } else if let Some(total) = total_bytes {
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
            let status = if !done {
                format!(" {spinner}  Running… {:.1}s", elapsed.as_secs_f32())
            } else if success {
                format!(" {spinner}  Done")
            } else {
                format!(" {spinner}  Failed")
            };
            f.render_widget(
                Paragraph::new(status).block(header_block).style(Style::default().fg(border_color)),
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
            Line::from(Span::styled(format!(" {l}"), Style::default().fg(log_line_color(l))))
        }).collect();
        f.render_widget(Paragraph::new(lines), inner);

        let footer = if done {
            "  [↑↓] Scroll  [t] Hide log  [Enter/q] Close"
        } else {
            "  [↑↓] Scroll  [t] Hide log"
        };
        f.render_widget(
            Paragraph::new(Span::styled(footer, Style::default().fg(border_color).add_modifier(Modifier::BOLD)))
                .alignment(Alignment::Center)
                .block(Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                    .border_style(Style::default().fg(border_color))),
            chunks[2],
        );
    } else {
        // ── Clean view: small popup with animated bar ─────────────────────────
        let popup = centered_rect(60, 30, area);
        f.render_widget(Clear, popup);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {title} "))
            .title_style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        let bar_w = (inner.width as usize).saturating_sub(4).max(8);

        let real_progress = progress.filter(|(_, t)| *t > 0);
        let bar_str = if done {
            if success { "█".repeat(bar_w) } else { "░".repeat(bar_w) }
        } else if let Some((n, t)) = real_progress {
            let filled = ((n as f64 / t as f64) * bar_w as f64).round() as usize;
            let filled = filled.min(bar_w);
            format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled))
        } else {
            let block_w = (bar_w / 5).max(3);
            let range = bar_w.saturating_sub(block_w);
            let cycle = (range * 2).max(1);
            let raw = (elapsed.as_millis() / 40) as usize % cycle;
            let pos = if raw <= range { raw } else { cycle - raw }.min(range);
            format!(
                "{}{}{}",
                "░".repeat(pos),
                "█".repeat(block_w),
                "░".repeat(bar_w.saturating_sub(pos + block_w)),
            )
        };
        let bar_color = if done && !success { C_DIM } else { border_color };

        let status_str = if !done {
            match real_progress {
                Some((n, t)) => {
                    let pct = (n as f64 / t as f64 * 100.0).round() as u32;
                    let eta = eta_seconds(n, t, elapsed);
                    format!("  {spinner}  {n}/{t}  ({pct}%)  ~{eta:.0}s left")
                }
                None => format!("  {spinner}  Running… {:.1}s", elapsed.as_secs_f32()),
            }
        } else if success {
            "  ✓  Done".to_string()
        } else {
            "  ✗  Failed".to_string()
        };

        let last_log = log.iter().rev().find(|l| !l.trim().is_empty()).map(String::as_str).unwrap_or("");
        let last_log_color = log_line_color(last_log);
        let max_chars = (inner.width as usize).saturating_sub(4);
        let last_log_truncated: String = last_log.chars().take(max_chars).collect();

        let footer_str = if done {
            "  [Enter/q] Close  [t] Debug log"
        } else {
            "  [t] Debug log"
        };

        let h = inner.height;
        if h == 0 { return; }

        f.render_widget(
            Paragraph::new(Span::styled(&status_str, Style::default().fg(border_color))),
            Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 },
        );

        if h >= 2 {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(&bar_str, Style::default().fg(bar_color)),
                ])),
                Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: 1 },
            );
        }

        if h >= 5 && !last_log_truncated.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!("  {last_log_truncated}"),
                    Style::default().fg(last_log_color),
                )),
                Rect { x: inner.x, y: inner.y + 3, width: inner.width, height: 1 },
            );
        }

        f.render_widget(
            Paragraph::new(Span::styled(footer_str, Style::default().fg(C_DIM))),
            Rect { x: inner.x, y: inner.y + h.saturating_sub(1), width: inner.width, height: 1 },
        );
    }
}

// ── Space tab ─────────────────────────────────────────────────────────────────

fn draw_space(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Disk Usage ")
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.du_apparent == 0 {
        f.render_widget(
            Paragraph::new("No apps installed.")
                .style(Style::default().fg(C_DIM))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    // Layout constants
    let label_w = app.installed.iter()
        .map(|m| m.app.name.len())
        .max().unwrap_or(0)
        .max(8);           // "Apparent" is 8 chars
    let size_w: usize = 12;
    let pct_w: usize  = 4;
    let prefix: usize = 2; // leading "  "
    let gaps: usize   = 6; // spaces between columns
    let bar_w = (inner.width as usize)
        .saturating_sub(prefix + label_w + size_w + pct_w + gaps)
        .max(8);

    let mut y = inner.y + 1;

    // ── Global bars ───────────────────────────────────────────────────────────

    let savings     = app.du_apparent.saturating_sub(app.du_actual);
    let on_disk_frac = (app.du_actual as f64 / app.du_apparent as f64).clamp(0.0, 1.0);
    let solid       = ((on_disk_frac * bar_w as f64).round() as usize).min(bar_w);
    let dimmed      = bar_w - solid;

    // Row 1 — "Apparent": full solid bar
    let apparent_line = Line::from(vec![
        Span::styled(
            format!("  {:<label_w$}  ", "Apparent"),
            Style::default().fg(C_DIM),
        ),
        Span::styled("█".repeat(bar_w), Style::default().fg(C_ACCENT)),
        Span::styled(
            format!("  {:>size_w$}", format_bytes(app.du_apparent)),
            Style::default().fg(Color::White),
        ),
    ]);
    f.render_widget(Paragraph::new(apparent_line),
        Rect { x: inner.x, y, width: inner.width, height: 1 });
    y += 1;

    // Row 2 — "On disk": solid portion + dimmed savings portion
    let saves_str = if savings > 0 {
        format!("  saves {}", format_bytes(savings))
    } else {
        String::new()
    };
    let on_disk_line = Line::from(vec![
        Span::styled(
            format!("  {:<label_w$}  ", "On disk"),
            Style::default().fg(C_DIM),
        ),
        Span::styled("█".repeat(solid),  Style::default().fg(C_ACCENT)),
        Span::styled("░".repeat(dimmed), Style::default().fg(Color::Rgb(55, 55, 65))),
        Span::styled(
            format!("  {:>size_w$}", format_bytes(app.du_actual)),
            Style::default().fg(Color::White),
        ),
        Span::styled(saves_str, Style::default().fg(C_GREEN)),
    ]);
    f.render_widget(Paragraph::new(on_disk_line),
        Rect { x: inner.x, y, width: inner.width, height: 1 });
    y += 2;

    // Separator
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("  {}", "─".repeat(inner.width as usize - 4)),
            Style::default().fg(Color::Rgb(50, 50, 60)),
        )),
        Rect { x: inner.x, y, width: inner.width, height: 1 },
    );
    y += 2;

    // ── Per-app bars ──────────────────────────────────────────────────────────

    let mut rows: Vec<(&str, u64)> = app.installed.iter()
        .map(|m| (m.app.name.as_str(), *app.app_sizes.get(&m.app.name).unwrap_or(&0)))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));

    for (name, size) in &rows {
        if y + 1 >= inner.y + inner.height { break; }

        let frac = (*size as f64 / app.du_apparent as f64).clamp(0.0, 1.0);
        let pct  = (frac * 100.0).round() as u32;
        let bar  = fractional_bar(bar_w, frac);

        let row = Line::from(vec![
            Span::styled(
                format!("  {:<label_w$}  ", name),
                Style::default().fg(C_DIM),
            ),
            Span::styled(bar, Style::default().fg(C_ACCENT)),
            Span::styled(
                format!("  {:>size_w$}  {:>2}%", format_bytes(*size), pct),
                Style::default().fg(Color::White),
            ),
        ]);
        f.render_widget(Paragraph::new(row),
            Rect { x: inner.x, y, width: inner.width, height: 1 });
        y += 1;
    }

    // Footer
    let footer_y = inner.y + inner.height.saturating_sub(1);
    f.render_widget(
        Paragraph::new(Span::styled(
            "  [r] Run dedup",
            Style::default().fg(C_DIM),
        )),
        Rect { x: inner.x, y: footer_y, width: inner.width, height: 1 },
    );
}

const BLOCK_EIGHTHS: &[char] = &[' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

fn fractional_bar(width: usize, fraction: f64) -> String {
    let eighths = (fraction.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let full    = (eighths / 8).min(width);
    let rem     = eighths % 8;
    let mut s   = "█".repeat(full);
    if rem > 0 && full < width {
        s.push(BLOCK_EIGHTHS[rem]);
    }
    while s.chars().count() < width {
        s.push(' ');
    }
    s
}

// ── Settings tab (global defaults) ───────────────────────────────────────────

fn draw_settings_tab(f: &mut Frame, app: &mut App, area: Rect) {
    let config = &app.global_config;
    let selected = app.global_selected;

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);

    // ── Left: settings list ───────────────────────────────────────────────────
    let list_block = Block::default()
        .borders(Borders::ALL)
        .title(" Default Settings ")
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let list_inner = list_block.inner(cols[0]);
    f.render_widget(list_block, cols[0]);

    let b = |v: bool| if v { " on  " } else { " off " };
    let share_label = if config.shared_dirs.is_empty() {
        "none →".to_string()
    } else {
        format!("{}  →", config.shared_dirs.len())
    };
    let spoof_label = |v: &Option<String>, sample: &str| -> String {
        match v.as_deref() {
            None | Some("") => "system".to_string(),
            Some(s) if s == sample => "sample".to_string(),
            Some(s) => s.chars().take(10).collect(),
        }
    };
    let rows: &[(&str, String)] = &[
        ("Network",     b(config.network).to_string()),
        ("Camera",      b(config.camera).to_string()),
        ("Microphone",  b(config.microphone).to_string()),
        ("Audio",       b(config.audio).to_string()),
        ("Temp mode",   match config.temp_mode {
            TempMode::System  => "system".into(),
            TempMode::Ramdisk => "ramdisk".into(),
            TempMode::Local   => "local".into(),
            TempMode::Uuid    => "uuid".into(),
        }),
        ("Temp delete", match config.temp_delete {
            LocalDelete::Never   => "never".into(),
            LocalDelete::OnStart => "on_start".into(),
            LocalDelete::OnClose => "on_close".into(),
        }),
        ("Shared dirs", share_label),
        ("Hostname",    spoof_label(&config.spoof_hostname,    HOSTNAME_SAMPLE)),
        ("Username",    spoof_label(&config.spoof_username,    USERNAME_SAMPLE)),
        ("Machine ID",  match config.spoof_machine_id.as_deref() {
            None            => "system".into(),
            Some("random")  => "random".into(),
            Some(v) if v == MACHINE_ID_SAMPLE => "sample".into(),
            Some(s)         => s.chars().take(10).collect(),
        }),
        ("CPU info",    match config.spoof_cpuinfo.as_deref() {
            None           => "system".into(),
            Some("sample") => "sample".into(),
            Some(s)        => s.chars().take(10).collect(),
        }),
        ("OS release",  match config.spoof_os.as_deref() {
            None               => "system".into(),
            Some("ubuntu")     => "Ubuntu".into(),
            Some("arch")       => "Arch".into(),
            Some("windows")    => "Windows 11".into(),
            Some("arduinoide") => "ArduinoIDE".into(),
            Some(s)            => s.chars().take(10).collect(),
        }),
        ("Spoof term.", if config.spoof_terminal { "detect".into() } else { "off".into() }),
        ("RAM limit",   match config.ram_limit {
            None      => "none".into(),
            Some(mib) if mib % 1024 == 0 => format!("{} GiB", mib / 1024),
            Some(mib) => format!("{} MiB", mib),
        }),
        ("Resolution",  match config.spoof_resolution.as_deref() {
            None             => "system".into(),
            Some("1280x720") => "1280×720".into(),
            Some("1920x1080")=> "1920×1080".into(),
            Some("2560x1440")=> "2560×1440".into(),
            Some("3840x2160")=> "3840×2160".into(),
            Some(s)          => s.chars().take(10).collect(),
        }),
        ("Shortcut",    if config.create_shortcut { "yes".into() } else { "no".into() }),
    ];

    // Reserve last 2 rows for separator + save
    let max_rows = (list_inner.height as usize).saturating_sub(2);
    for (idx, (label, value)) in rows.iter().enumerate().take(max_rows) {
        let is_sel = idx == selected;
        let y = list_inner.y + idx as u16;
        let bg = if is_sel { C_SELECT } else { Color::Reset };
        let val_color = match value.trim() {
            "on" => C_GREEN,
            "off" => C_RED,
            _ => C_YELLOW,
        };
        let label_w = 12usize;
        let padded_label: String = format!("{:width$}", label, width = label_w);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if is_sel { "▶ " } else { "  " }, Style::default().fg(C_ACCENT).bg(bg)),
                Span::styled(padded_label, Style::default().fg(if is_sel { Color::White } else { C_DIM }).bg(bg)),
                Span::styled(" ", Style::default().bg(bg)),
                Span::styled(format!("[{}]", value),
                    Style::default().fg(val_color).bg(bg)
                        .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() })),
            ])),
            Rect { x: list_inner.x, y, width: list_inner.width, height: 1 },
        );
    }

    // Separator + Save button
    let sep_y = list_inner.y + list_inner.height.saturating_sub(2);
    let save_y = list_inner.y + list_inner.height.saturating_sub(1);
    f.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(list_inner.width as usize),
            Style::default().fg(Color::Rgb(50, 50, 60)),
        )),
        Rect { x: list_inner.x, y: sep_y, width: list_inner.width, height: 1 },
    );
    let is_save = selected == CFG_SAVE;
    let save_style = if is_save {
        Style::default().fg(Color::Black).bg(C_GREEN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(C_GREEN)
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(if is_save { "▶ " } else { "  " }, Style::default().fg(C_ACCENT)),
            Span::styled("[ Save defaults ]", save_style),
        ])),
        Rect { x: list_inner.x, y: save_y, width: list_inner.width, height: 1 },
    );

    // ── Right: description + options ──────────────────────────────────────────
    let desc_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", setting_title(selected)))
        .title_style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_DIM));
    let desc_inner = desc_block.inner(cols[1]);
    f.render_widget(desc_block, cols[1]);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(desc_inner);

    // Description paragraph (wrapped)
    let desc_text = if selected == CFG_SAVE {
        "Save the current values as global defaults.\n\nThese settings apply to every new app install that does not have its own config file."
    } else if selected == CFG_SHARES {
        "Shared directories are per-app only.\n\nTo add shared dirs, open an installed app's config with [s] on the Installed tab."
    } else {
        setting_description(selected)
    };
    f.render_widget(
        Paragraph::new(desc_text)
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    // Divider
    f.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(desc_inner.width as usize),
            Style::default().fg(Color::Rgb(50, 50, 60)),
        )),
        chunks[1],
    );

    // Options list
    let opts = setting_options(selected);
    let cur = if selected < CFG_SAVE { setting_current(config, selected) } else { usize::MAX };
    let opt_lines: Vec<Line> = opts.iter().enumerate().map(|(i, opt)| {
        let active = i == cur;
        let bullet = if active { "●" } else { "○" };
        let bullet_color = if active { C_ACCENT } else { C_DIM };
        let text_color = if active { Color::White } else { C_DIM };
        Line::from(vec![
            Span::styled(format!(" {} ", bullet), Style::default().fg(bullet_color)),
            Span::styled(*opt, Style::default().fg(text_color)
                .add_modifier(if active { Modifier::BOLD } else { Modifier::empty() })),
        ])
    }).collect();
    f.render_widget(Paragraph::new(opt_lines), chunks[2]);

    // Hint
    let hint = if selected == CFG_SAVE {
        "  Enter or Space to save"
    } else if opts.is_empty() {
        "  Enter to open"
    } else {
        "  ← / → to cycle   Enter to pick from list   ? for help"
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(C_DIM))),
        chunks[3],
    );

    // Microphone warning
    if !config.microphone && config.audio && desc_inner.height > 6 {
        let warn_y = desc_inner.y + desc_inner.height.saturating_sub(2);
        f.render_widget(
            Paragraph::new(Span::styled(
                "  ⚠  PipeWire/PA mic not fully blocked — set Audio off too",
                Style::default().fg(C_YELLOW),
            )),
            Rect { x: desc_inner.x, y: warn_y, width: desc_inner.width, height: 1 },
        );
    }
}

// ── Config overlay ────────────────────────────────────────────────────────────

fn draw_config(
    f: &mut Frame,
    area: Rect,
    app_name: &str,
    config: &AppConfig,
    selected: usize,
    wine_game: Option<&(String, String)>,
) {
    let popup = centered_rect(54, 92, area);
    f.render_widget(Clear, popup);

    let title = if app_name.is_empty() {
        " Default Settings ".to_string()
    } else {
        format!(" Config — {app_name} ")
    };
    let block = Block::default().borders(Borders::ALL)
        .title(title)
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
    let spoof_label = |v: &Option<String>, sample: &str| -> String {
        match v.as_deref() {
            None | Some("") => " system ".to_string(),
            Some(s) if s == sample => " sample ".to_string(),
            Some(s) => { let t: String = s.chars().take(12).collect(); format!(" {t} ") }
        }
    };
    let trim_path = |s: &str| -> String {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= 24 {
            format!(" {s} →")
        } else {
            // Keep the tail so the filename stays visible.
            let tail: String = chars[chars.len() - 22..].iter().collect();
            format!(" …{tail} →")
        }
    };
    let mut rows: Vec<(&str, String)> = vec![
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
        ("Hostname   ", spoof_label(&config.spoof_hostname, HOSTNAME_SAMPLE)),
        ("Username   ", spoof_label(&config.spoof_username, USERNAME_SAMPLE)),
        ("Machine ID ", match config.spoof_machine_id.as_deref() {
            None            => " system ".to_string(),
            Some("random")  => " random ".to_string(),
            Some(v) if v == MACHINE_ID_SAMPLE => " sample ".to_string(),
            Some(s)         => { let t: String = s.chars().take(12).collect(); format!(" {t} ") }
        }),
        ("CPU info   ", match config.spoof_cpuinfo.as_deref() {
            None           => " system ".to_string(),
            Some("sample") => " sample ".to_string(),
            Some(s)        => { let t: String = s.chars().take(12).collect(); format!(" {t} ") }
        }),
        ("OS release ", match config.spoof_os.as_deref() {
            None               => " system    ".to_string(),
            Some("ubuntu")     => " Ubuntu    ".to_string(),
            Some("arch")       => " Arch      ".to_string(),
            Some("windows")    => " Windows 11".to_string(),
            Some("arduinoide") => " ArduinoIDE".to_string(),
            Some(s)            => { let t: String = s.chars().take(12).collect(); format!(" {t} ") }
        }),
        ("Spoof term.", if config.spoof_terminal { " detect".to_string() } else { "  off  ".to_string() }),
        ("RAM limit  ", match config.ram_limit {
            None      => " none    ".to_string(),
            Some(mib) if mib % 1024 == 0 => format!(" {} GiB  ", mib / 1024),
            Some(mib) => format!(" {} MiB  ", mib),
        }),
        ("Resolution ", match config.spoof_resolution.as_deref() {
            None             => " system  ".to_string(),
            Some("1280x720") => " 1280×720".to_string(),
            Some("1920x1080")=> " 1920×1080".to_string(),
            Some("2560x1440")=> " 2560×1440".to_string(),
            Some("3840x2160")=> " 3840×2160".to_string(),
            Some(s)          => { let t: String = s.chars().take(10).collect(); format!(" {t}") }
        }),
    ];

    // Wine-game rows are only shown when the Config was opened for a wine game.
    if let Some((exe, prefix)) = wine_game {
        rows.push(("Game Exe   ", trim_path(exe)));
        rows.push(("Game Prefix", trim_path(prefix)));
    }

    let has_wg = wine_game.is_some();
    let save_idx = app_cfg_save_idx(has_wg);

    // Save is pinned to the bottom so it's always reachable on small terminals.
    let save_y = inner.y + inner.height.saturating_sub(2);
    // Stop rendering rows before they collide with the separator + save button.
    let clip_y = save_y.saturating_sub(2);

    let mut y = inner.y;
    for (idx, (label, value)) in rows.iter().enumerate() {
        if y >= clip_y { break; }
        let is_sel = idx == selected;

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
        y += 1;

        // Game rows render compactly (no separator) so the extra two rows fit
        // on the same popup height as the non-game Config screen.
        let is_game_row = has_wg && (idx == CFG_GAME_EXE || idx == CFG_GAME_PREFIX);
        if !is_game_row && y < clip_y {
            f.render_widget(
                Paragraph::new(Span::styled("─".repeat(inner.width as usize), Style::default().fg(Color::Rgb(50, 50, 60)))),
                Rect { x: inner.x, y, width: inner.width, height: 1 },
            );
            y += 1;
        }
    }

    // Save button — always at the bottom
    let is_sel_save = selected == save_idx;
    let btn_style = if is_sel_save {
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
    if save_y < inner.y + inner.height {
        let save_label = if app_name.is_empty() { "[ Save ]" } else { "[ Save & Close ]" };
        let prefix = if is_sel_save { " ▶ " } else { "   " };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(C_ACCENT)),
                Span::styled(save_label, btn_style),
            ])),
            Rect { x: inner.x, y: save_y, width: inner.width, height: 1 },
        );
    }

    // Microphone warning
    if !config.microphone && config.audio {
        let warn_y = inner.y + inner.height.saturating_sub(3);
        if warn_y > inner.y {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "  ⚠  PipeWire/PA mic not blocked — set audio off",
                    Style::default().fg(C_YELLOW),
                )),
                Rect { x: inner.x, y: warn_y, width: inner.width, height: 1 },
            );
        }
    }

    // Footer
    let footer_y = inner.y + inner.height.saturating_sub(1);
    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓] Navigate  [←/→] Cycle  [Enter] Edit  [?] Help  [Esc/q] Discard",
            Style::default().fg(C_DIM),
        )),
        Rect { x: inner.x, y: footer_y, width: inner.width, height: 1 },
    );
}

// ── Text input overlay ────────────────────────────────────────────────────────

fn draw_text_input(f: &mut Frame, area: Rect, title: &str, value: &str) {
    let popup = centered_rect(54, 30, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" {title} "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(3), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled(
            "  Leave blank to disable. Press Enter to confirm.",
            Style::default().fg(C_DIM),
        )),
        chunks[0],
    );

    f.render_widget(
        Paragraph::new(format!(" {}█", value))
            .block(Block::default().borders(Borders::ALL)
                .border_style(Style::default().fg(C_ACCENT)))
            .style(Style::default().fg(Color::White)),
        chunks[1],
    );

    f.render_widget(
        Paragraph::new(Span::styled(
            " [Enter] Confirm  [Esc] Cancel  [Backspace] Delete char",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
    );
}

// ── Option picker overlay ─────────────────────────────────────────────────────

fn draw_option_picker(
    f: &mut Frame,
    area: Rect,
    setting_idx: usize,
    selected: usize,
    config: &AppConfig,
) {
    let title = super::setting_title(setting_idx);
    let options = super::setting_options(setting_idx);
    let current = super::setting_current(config, setting_idx);

    // Size the popup just large enough for header + options + footer.
    let needed_h = (options.len() as u16) + 4; // borders (2) + footer (1) + breathing room
    let h_pct = ((needed_h as f32 / area.height.max(1) as f32) * 100.0)
        .clamp(20.0, 60.0) as u16;
    let popup = centered_rect(36, h_pct, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" {title} "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = options.iter().enumerate().map(|(i, opt)| {
        let is_current = i == current;
        let marker = if is_current { "● " } else { "  " };
        let marker_color = if is_current { C_GREEN } else { C_DIM };
        let opt_color = if is_current { C_GREEN } else { Color::White };
        ListItem::new(Line::from(vec![
            Span::styled(marker, Style::default().fg(marker_color)),
            Span::styled(opt.to_string(), Style::default().fg(opt_color)),
        ]))
    }).collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    let list = List::new(items)
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[0], &mut list_state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓/jk] Navigate  [Enter] Select  [?] Help  [Esc] Cancel",
            Style::default().fg(C_DIM),
        )),
        chunks[1],
    );
}

// ── Setting help popup ────────────────────────────────────────────────────────

fn draw_setting_help(f: &mut Frame, area: Rect, setting_idx: usize) {
    let title = super::setting_title(setting_idx);
    let desc  = super::setting_description(setting_idx);

    let popup = centered_rect(54, 40, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" ? {title} "))
        .title_style(Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_YELLOW));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(format!("  {desc}"))
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    f.render_widget(
        Paragraph::new(Span::styled(
            " Press any key to close",
            Style::default().fg(C_DIM),
        )),
        chunks[1],
    );
}

// ── Key bindings help popup ───────────────────────────────────────────────────

fn draw_key_help(f: &mut Frame, area: Rect) {
    const KEYS: &[(&str, &str)] = &[
        ("r / Enter",  "Run the selected app"),
        ("d / Del",    "Delete the selected app"),
        ("e",          "Export app to a zip file"),
        ("p",          "Create a snapshot"),
        ("o",          "Roll back to a snapshot"),
        ("c",          "Check for updates (no install)"),
        ("u",          "Update the selected app"),
        ("s",          "Open per-app settings (incl. game exe/prefix for wine games)"),
        ("n",          "Rename app (set display name)"),
        ("Tab",        "Switch between tabs"),
        ("↑ / k",      "Move selection up"),
        ("↓ / j",      "Move selection down"),
        ("→ / l",      "Enter detail panel"),
        ("← / h",      "Exit detail panel"),
        ("?",          "Show this help"),
        ("q / Esc",    "Quit"),
    ];

    let max_key = KEYS.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(
                    format!("  {:>width$}  ", k, width = max_key),
                    Style::default().fg(C_ACCENT),
                ),
                Span::styled(*v, Style::default().fg(Color::White)),
            ])
        })
        .collect();

    let needed_h = (lines.len() as u16) + 4;
    let popup = {
        let w = 52u16.min(area.width);
        let h = needed_h.min(area.height);
        Rect {
            x: area.x + area.width.saturating_sub(w) / 2,
            y: area.y + area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        }
    };
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(" ? Key bindings ")
        .title_style(Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_YELLOW));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(Paragraph::new(lines), chunks[0]);
    f.render_widget(
        Paragraph::new(Span::styled(" Press any key to close", Style::default().fg(C_DIM))),
        chunks[1],
    );
}

// ── Option help popup ─────────────────────────────────────────────────────────

fn draw_option_help(f: &mut Frame, area: Rect, setting_idx: usize, choice_idx: usize) {
    let options = super::setting_options(setting_idx);
    let opt_name = options.get(choice_idx).copied().unwrap_or("?");
    let desc = super::option_description(setting_idx, choice_idx);

    let popup = centered_rect(54, 35, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" ? {opt_name} "))
        .title_style(Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_YELLOW));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(format!("  {desc}"))
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    f.render_widget(
        Paragraph::new(Span::styled(
            " Press any key to close",
            Style::default().fg(C_DIM),
        )),
        chunks[1],
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

// ── Install target picker overlay ─────────────────────────────────────────────

fn draw_install_target(f: &mut Frame, area: Rect, pkg: &str, targets: &[String], selected: usize) {
    let popup = centered_rect(60, 70, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" Install '{pkg}' "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled(
            "  Where should it go?",
            Style::default().fg(C_DIM),
        )),
        chunks[0],
    );

    // Row 0 — fresh install
    let mut items: Vec<ListItem> = vec![
        ListItem::new(Line::from(vec![
            Span::styled("✚ ", Style::default().fg(C_GREEN)),
            Span::styled("New app", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("  ~/.wryayer/{pkg}/"),
                Style::default().fg(C_DIM),
            ),
        ])),
    ];
    // Rows 1..n — merge targets
    for t in targets {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("⇆ ", Style::default().fg(C_YELLOW)),
            Span::styled("Merge into ", Style::default().fg(C_DIM)),
            Span::styled(t.as_str(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("  ~/.wryayer/{t}/"),
                Style::default().fg(C_DIM),
            ),
        ])));
    }

    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    let list = List::new(items)
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut list_state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓/jk] Navigate  [Enter] Select  [Esc/q] Cancel",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
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

fn draw_konami_overlay(
    f: &mut Frame,
    area: Rect,
    kind: crate::tui::konami::Anim,
    elapsed: std::time::Duration,
    done: bool,
    success: bool,
) {
    use crate::tui::konami;
    let kf = konami::render(
        kind,
        area.width,
        area.height,
        elapsed.as_millis() as u64,
        done,
        success,
    );

    for y in 0..kf.height {
        let mut line = Vec::with_capacity(kf.width as usize);
        for x in 0..kf.width {
            let (ch, col) = kf.get(x, y);
            line.push(Span::styled(
                ch.to_string(),
                Style::default().fg(col).add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(
            Paragraph::new(Line::from(line)),
            Rect { x: area.x, y: area.y + y, width: area.width, height: 1 },
        );
    }

    // Footer hint
    let footer = if done {
        " [Enter/q] Close  [t] Debug log "
    } else {
        " [t] Debug log "
    };
    let fy = area.y + area.height.saturating_sub(1);
    f.render_widget(
        Paragraph::new(Span::styled(footer, Style::default().fg(Color::White).bg(Color::DarkGray))),
        Rect { x: area.x, y: fy, width: area.width, height: 1 },
    );
}

// ── Rename app overlay ────────────────────────────────────────────────────────

fn draw_rename_app(f: &mut Frame, area: Rect, app_name: &str, value: &str) {
    let popup = centered_rect(54, 30, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" Rename '{app_name}' "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(3), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled(
            "  Display name shown in the list. Leave blank to clear.",
            Style::default().fg(C_DIM),
        )),
        chunks[0],
    );

    f.render_widget(
        Paragraph::new(format!(" {}█", value))
            .block(Block::default().borders(Borders::ALL)
                .border_style(Style::default().fg(C_ACCENT)))
            .style(Style::default().fg(Color::White)),
        chunks[1],
    );

    f.render_widget(
        Paragraph::new(Span::styled(
            " [Enter] Confirm  [Esc] Cancel  [Backspace] Delete char",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
    );
}

// ── Already installed choice overlay ─────────────────────────────────────────

fn draw_already_installed(f: &mut Frame, area: Rect, pkg: &str, selected: usize) {
    let popup = centered_rect(56, 40, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" '{pkg}' is already installed "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled("  What would you like to do?", Style::default().fg(C_DIM))),
        chunks[0],
    );

    let choices: &[(&str, &str, Color)] = &[
        ("✚", "Install a second copy   →  pick container, then name", C_GREEN),
        ("✕", "Uninstall               →  delete the existing install", C_RED),
    ];

    let items: Vec<ListItem> = choices.iter().enumerate().map(|(i, (icon, label, color))| {
        let is_sel = i == selected;
        let style = if is_sel {
            Style::default().fg(*color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_DIM)
        };
        ListItem::new(Line::from(vec![
            Span::styled(if is_sel { " ▶ " } else { "   " }, Style::default().fg(C_ACCENT)),
            Span::styled(*icon, Style::default().fg(*color)),
            Span::raw(" "),
            Span::styled(*label, style),
        ]))
    }).collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    let list = List::new(items)
        .highlight_style(Style::default().bg(C_SELECT));
    f.render_stateful_widget(list, chunks[1], &mut list_state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓/jk] Navigate  [Enter] Select  [Esc/q] Cancel",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
    );
}

// ── No-launcher choice overlay ────────────────────────────────────────────────

fn draw_outdated_packages(f: &mut Frame, area: Rect, pkg: &str, selected: usize) {
    let popup = centered_rect(62, 40, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(" Package databases may be out of date ")
        .title_style(Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_YELLOW));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let mut items: Vec<ListItem> = vec![
        ListItem::new(Line::from(vec![
            Span::styled("  Got 404 downloading ", Style::default().fg(C_DIM)),
            Span::styled(pkg, Style::default().fg(Color::White)),
            Span::styled(" — the mirror no longer", Style::default().fg(C_DIM)),
        ])),
        ListItem::new(Line::from(vec![
            Span::styled("  hosts the version in your local database.", Style::default().fg(C_DIM)),
        ])),
        ListItem::new(Line::raw("")),
    ];

    let choices: &[(&str, &str, &str, Color)] = &[
        ("↻", "Update & retry", "run 'sudo pacman -Sy', then retry install", C_GREEN),
        ("✕", "Cancel",         "return to main screen",                     C_RED),
    ];

    for (i, (icon, label, desc, color)) in choices.iter().enumerate() {
        let is_sel = i == selected;
        let label_style = if is_sel {
            Style::default().fg(*color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_DIM)
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(if is_sel { " ▶ " } else { "   " }, Style::default().fg(C_YELLOW)),
            Span::styled(*icon, Style::default().fg(*color)),
            Span::raw(" "),
            Span::styled(*label, label_style),
            Span::styled(format!("  — {desc}"), Style::default().fg(C_DIM)),
        ])));
    }

    let mut list_state = ListState::default();
    list_state.select(Some(3 + selected)); // 3 info rows before the choices

    let list = List::new(items)
        .highlight_style(Style::default().bg(C_SELECT));
    f.render_stateful_widget(list, chunks[0], &mut list_state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓/jk] Navigate  [Enter] Select  [Esc/q] Cancel",
            Style::default().fg(C_DIM),
        )),
        chunks[1],
    );
}

fn draw_ask_shortcut(f: &mut Frame, area: Rect, pkg: &str, selected: usize) {
    let popup = centered_rect(52, 40, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Create shortcut? ")
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ~/bin/", Style::default().fg(C_DIM)),
            Span::styled(pkg, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ])),
        chunks[0],
    );

    let choices: &[(&str, &str, Color)] = &[
        ("Yes", "add shortcut to ~/bin/", C_GREEN),
        ("No",  "install without shortcut", C_DIM),
    ];
    let items: Vec<ListItem> = choices.iter().enumerate().map(|(i, (label, desc, color))| {
        let is_sel = i == selected;
        ListItem::new(Line::from(vec![
            Span::styled(if is_sel { " ▶ " } else { "   " }, Style::default().fg(C_ACCENT)),
            Span::styled(*label, Style::default().fg(if is_sel { *color } else { C_DIM })
                .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() })),
            Span::styled(format!("  — {desc}"), Style::default().fg(C_DIM)),
        ]))
    }).collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    let list = List::new(items).highlight_style(Style::default().bg(C_SELECT));
    f.render_stateful_widget(list, chunks[1], &mut list_state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓] Navigate  [Enter] Confirm  [Esc] Cancel",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
    );
}

fn draw_no_launcher_choice(f: &mut Frame, area: Rect, pkg: &str, available_bins: &[String], selected: usize) {
    let popup = centered_rect(60, 50, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" '{pkg}' — no launcher binary found "))
        .title_style(Style::default().fg(C_YELLOW).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_YELLOW));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let mut items: Vec<ListItem> = vec![];

    if !available_bins.is_empty() {
        let bins_str = available_bins.join(", ");
        let truncated: String = bins_str.chars().take(inner.width as usize - 4).collect();
        items.push(ListItem::new(Line::from(vec![
            Span::styled("  Available: ", Style::default().fg(C_DIM)),
            Span::styled(truncated, Style::default().fg(Color::White)),
        ])));
        items.push(ListItem::new(Line::from(vec![
            Span::styled("  Reinstall with ", Style::default().fg(C_DIM)),
            Span::styled("--bin-names <name>", Style::default().fg(Color::White)),
            Span::styled(" to add a launcher.", Style::default().fg(C_DIM)),
        ])));
        items.push(ListItem::new(Line::raw("")));
    }

    let choices: &[(&str, &str, &str, Color)] = &[
        ("✚", "Keep without launcher", "files installed, no ~/bin/ shortcut", C_GREEN),
        ("✕", "Clean up",              "remove all installed files",           C_RED),
    ];

    for (i, (icon, label, desc, color)) in choices.iter().enumerate() {
        let is_sel = i == selected;
        let label_style = if is_sel {
            Style::default().fg(*color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_DIM)
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(if is_sel { " ▶ " } else { "   " }, Style::default().fg(C_YELLOW)),
            Span::styled(*icon, Style::default().fg(*color)),
            Span::raw(" "),
            Span::styled(*label, label_style),
            Span::styled(format!("  — {desc}"), Style::default().fg(C_DIM)),
        ])));
    }

    let mut list_state = ListState::default();
    // The selectable rows start after the info rows.
    let info_rows = if available_bins.is_empty() { 0 } else { 3 };
    list_state.select(Some(info_rows + selected));

    let list = List::new(items)
        .highlight_style(Style::default().bg(C_SELECT));
    f.render_stateful_widget(list, chunks[0], &mut list_state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓/jk] Navigate  [Enter] Select  [Esc/q] Cancel",
            Style::default().fg(C_DIM),
        )),
        chunks[1],
    );
}

// ── Duplicate install overlay ─────────────────────────────────────────────────

fn draw_duplicate_install(f: &mut Frame, area: Rect, pkg: &str, value: &str, into: Option<&str>) {
    let popup = centered_rect(54, 30, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(format!(" Install '{pkg}' again "))
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Length(3), Constraint::Length(1)])
        .split(inner);

    let desc = match into {
        None => format!("  '{pkg}' already exists. Choose a name for the new container:"),
        Some(target) => format!("  '{pkg}' already exists. Choose an alias name for the merge into '{target}':"),
    };
    f.render_widget(
        Paragraph::new(desc)
            .style(Style::default().fg(C_DIM))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        chunks[0],
    );

    f.render_widget(
        Paragraph::new(format!(" {}█", value))
            .block(Block::default().borders(Borders::ALL)
                .border_style(Style::default().fg(C_ACCENT)))
            .style(Style::default().fg(Color::White)),
        chunks[1],
    );

    f.render_widget(
        Paragraph::new(Span::styled(
            " [Enter] Install  [Esc] Cancel  [Backspace] Delete char",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
    );
}

/// Estimate remaining seconds given linear progress so far.
fn eta_seconds(done: u64, total: u64, elapsed: std::time::Duration) -> f64 {
    if done == 0 { return 0.0; }
    let frac = done as f64 / total as f64;
    if frac <= 0.0 { return 0.0; }
    let total_estimated = elapsed.as_secs_f64() / frac;
    (total_estimated - elapsed.as_secs_f64()).max(0.0)
}

// ── Games tab ─────────────────────────────────────────────────────────────────

fn draw_games(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::ALL)
        .title(" Wine Games ").title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let games: Vec<&crate::manifest::Manifest> = app.installed
        .iter()
        .filter(|m| m.app.wine_game.is_some())
        .collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new("  Each game becomes its own ~/.wryayer/<name>/ container with a fresh wine install")
            .style(Style::default().fg(C_DIM)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new("  and its own WINEPREFIX, so games can't interfere with each other.")
            .style(Style::default().fg(C_DIM)),
        chunks[1],
    );

    if games.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  No games imported yet. Press [i] or [Enter] to import a folder.",
                Style::default().fg(C_DIM),
            )),
            chunks[3],
        );
    } else {
        let items: Vec<ListItem> = games.iter().map(|m| {
            let exe = m.app.wine_game.as_ref().map(|w| w.exe.as_str()).unwrap_or("?");
            let display = m.app.display_name.as_deref().unwrap_or(m.app.name.as_str());
            ListItem::new(Line::from(vec![
                Span::styled("  🎮 ", Style::default().fg(C_YELLOW)),
                Span::styled(display.to_string(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(format!("   {exe}"), Style::default().fg(C_ACCENT)),
            ]))
        }).collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::TOP)
                .title(format!(" Imported games ({}) ", games.len()))
                .title_style(Style::default().fg(C_ACCENT))
                .border_style(Style::default().fg(C_DIM)))
            .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, chunks[3], &mut app.games_state);
    }

    f.render_widget(
        Paragraph::new(Span::styled(
            "  [Enter/r] Run    [s] Settings    [d] Delete    [i/a] Import",
            Style::default().fg(C_ACCENT),
        )),
        chunks[4],
    );
}

// ── Wine game wizard overlays ─────────────────────────────────────────────────

fn draw_game_exe_pick(f: &mut Frame, area: Rect, game_dir: &str, exes: &[(String, u64)], selected: usize) {
    let popup = centered_rect(70, 70, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(" 1/3 — Pick main .exe ")
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  Folder: ", Style::default().fg(C_DIM)),
            Span::styled(game_dir, Style::default().fg(Color::White)),
        ])),
        chunks[0],
    );

    let items: Vec<ListItem> = exes.iter().map(|(rel, sz)| {
        let mib = sz / 1_048_576;
        ListItem::new(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(rel.as_str(), Style::default().fg(Color::White)),
            Span::styled(format!("   {mib} MiB"), Style::default().fg(C_DIM)),
        ]))
    }).collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    let list = List::new(items)
        .highlight_style(Style::default().bg(C_SELECT).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut list_state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓/jk] Navigate  [Enter] Select  [Esc/q] Cancel",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
    );
}

fn draw_game_name_input(f: &mut Frame, area: Rect, game_dir: &str, exe: &str, value: &str) {
    let popup = centered_rect(60, 40, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(" 2/3 — Container name ")
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  Folder:  ", Style::default().fg(C_DIM)),
                Span::styled(game_dir, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  Exe:     ", Style::default().fg(C_DIM)),
                Span::styled(exe, Style::default().fg(C_ACCENT)),
            ]),
        ]),
        chunks[0],
    );

    f.render_widget(
        Paragraph::new(format!(" {value}█"))
            .block(Block::default().borders(Borders::ALL)
                .title(" Name (~/.wryayer/<name>/ and ~/bin/<name>) ")
                .title_style(Style::default().fg(Color::White))
                .border_style(Style::default().fg(C_ACCENT)))
            .style(Style::default().fg(Color::White)),
        chunks[1],
    );

    f.render_widget(
        Paragraph::new(Span::styled(
            "  Lowercase letters, digits, dash/underscore/dot. Other chars become '-'.",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
    );

    f.render_widget(
        Paragraph::new(Span::styled(
            " [Enter] Continue  [Esc] Back  [Backspace] Delete char",
            Style::default().fg(C_DIM),
        )),
        chunks[3],
    );
}

fn draw_game_confirm(
    f: &mut Frame,
    area: Rect,
    game_dir: &str,
    exe: &str,
    app_name: &str,
    delete_source: bool,
    selected: usize,
) {
    let popup = centered_rect(64, 60, area);
    f.render_widget(Clear, popup);

    let block = Block::default().borders(Borders::ALL)
        .title(" 3/3 — Confirm import ")
        .title_style(Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  Name:    ", Style::default().fg(C_DIM)),
                Span::styled(app_name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("  Exe:     ", Style::default().fg(C_DIM)),
                Span::styled(exe, Style::default().fg(C_ACCENT)),
            ]),
            Line::from(vec![
                Span::styled("  Source:  ", Style::default().fg(C_DIM)),
                Span::styled(game_dir, Style::default().fg(Color::White)),
            ]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  Dest:    ", Style::default().fg(C_DIM)),
                Span::styled(format!("~/.wryayer/{app_name}/games/{app_name}/"), Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  Wine:    ", Style::default().fg(C_DIM)),
                Span::styled("installed fresh into the container", Style::default().fg(C_GREEN)),
            ]),
        ]),
        chunks[0],
    );

    let del_marker = if delete_source { "[x]" } else { "[ ]" };
    let del_label = format!("{del_marker} Delete source folder after copy");
    let choices: &[(String, Color)] = &[
        ("✓ Install".to_string(), C_GREEN),
        (del_label, if delete_source { C_RED } else { C_DIM }),
        ("✕ Cancel".to_string(), C_RED),
    ];
    let items: Vec<ListItem> = choices.iter().enumerate().map(|(i, (label, color))| {
        let is_sel = i == selected;
        let style = if is_sel {
            Style::default().fg(*color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_DIM)
        };
        ListItem::new(Line::from(vec![
            Span::styled(if is_sel { " ▶ " } else { "   " }, Style::default().fg(C_ACCENT)),
            Span::styled(label.clone(), style),
        ]))
    }).collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    let list = List::new(items).highlight_style(Style::default().bg(C_SELECT));
    f.render_stateful_widget(list, chunks[1], &mut list_state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " [↑↓] Navigate  [Space] Toggle delete  [Enter] Confirm  [Esc] Cancel",
            Style::default().fg(C_DIM),
        )),
        chunks[2],
    );
}

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
