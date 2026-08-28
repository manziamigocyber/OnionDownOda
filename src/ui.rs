use crate::app::{
    format_bytes, App, AppMode, DialogFocus, DownloadCategory, DownloadStatus, Focus, NetworkMode,
    SettingsField,
};
use crate::banner::BANNER;
use crate::history;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

const LOGO_MAGENTA: Color = Color::Rgb(255, 0, 255);
const LOGO_PINK: Color = Color::Rgb(255, 110, 199);
const MAGENTA: Color = Color::Rgb(191, 64, 255);
const CYAN: Color = Color::Rgb(0, 255, 255);
const GREEN: Color = Color::Rgb(57, 255, 20);
const YELLOW: Color = Color::Rgb(255, 200, 0);
const WHITE: Color = Color::White;
const GRAY: Color = Color::Rgb(180, 180, 180);
const DIM_GRAY: Color = Color::Rgb(80, 80, 80);
const DARK_BG: Color = Color::Rgb(0, 0, 0);
const SURFACE: Color = Color::Rgb(10, 10, 10);

fn category_color(category: DownloadCategory) -> Color {
    match category {
        DownloadCategory::Video => Color::Rgb(255, 105, 180),
        DownloadCategory::Music => Color::Rgb(170, 120, 255),
        DownloadCategory::Documents => CYAN,
        DownloadCategory::Programs => YELLOW,
        DownloadCategory::Archives => GREEN,
        DownloadCategory::Other => GRAY,
    }
}

/// Column offset pieces of the editable path text inside the save dialog.
const DIALOG_PATH_LABEL: &str = "💾 Save to:  ";
/// Column offset pieces of the editable path text inside the settings panel.
const SETTINGS_DIR_LABEL: &str = "Folder:  ";

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub fn draw(frame: &mut Frame, app: &App) {
    let size = frame.area();
    let width = size.width;

    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(0, 0, 0))),
        size,
    );

    let layout_mode = if width >= 120 {
        LayoutMode::Wide
    } else if width >= 80 {
        LayoutMode::Medium
    } else {
        LayoutMode::Narrow
    };

    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(22),
            Constraint::Percentage(6),
            Constraint::Percentage(8),
            Constraint::Percentage(27),
            Constraint::Percentage(23),
            Constraint::Percentage(14),
        ])
        .split(size);

    match layout_mode {
        LayoutMode::Wide => draw_header_wide(frame, main_layout[0]),
        LayoutMode::Medium => draw_header_medium(frame, main_layout[0]),
        LayoutMode::Narrow => draw_header_narrow(frame, main_layout[0]),
    }

    draw_tor_status(frame, app, main_layout[1]);
    draw_input(frame, app, main_layout[2]);
    draw_downloads(frame, app, main_layout[3]);
    draw_history(frame, app, main_layout[4]);
    draw_log(frame, app, main_layout[5]);
    draw_disclaimer_and_help(frame, app, size);

    if app.mode == AppMode::Dialog {
        draw_dialog(frame, app, size);
    } else if app.mode == AppMode::Help {
        draw_help(frame, size);
    } else if app.mode == AppMode::Settings {
        draw_settings(frame, app, size);
    }

    if app.search_mode {
        draw_search_overlay(frame, app, size);
    }
}

#[derive(Clone, Copy)]
enum LayoutMode {
    Wide,
    Medium,
    Narrow,
}

fn draw_header_wide(frame: &mut Frame, area: ratatui::layout::Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(75), Constraint::Percentage(25)])
        .split(area);

    draw_logo_centered(frame, chunks[0]);
    draw_linktree_line(frame, chunks[1]);
}

fn draw_header_medium(frame: &mut Frame, area: ratatui::layout::Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(75), Constraint::Percentage(25)])
        .split(area);

    draw_logo_centered(frame, chunks[0]);
    draw_linktree_line(frame, chunks[1]);
}

fn draw_header_narrow(frame: &mut Frame, area: ratatui::layout::Rect) {
    let text = Paragraph::new(vec![
        Line::from(Span::styled(
            "[ OnionDownOda ]",
            Style::default()
                .fg(LOGO_MAGENTA)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(Span::styled(
            "⚠️  Please resize terminal (min 80 cols)",
            Style::default().fg(YELLOW),
        )),
    ])
    .alignment(Alignment::Center);

    frame.render_widget(text, area);
}

fn draw_logo_centered(frame: &mut Frame, area: ratatui::layout::Rect) {
    let mut lines = Vec::new();
    let max_i = (BANNER.len() as f32 - 1.0).max(1.0);

    for (i, line_str) in BANNER.iter().enumerate() {
        let t = i as f32 / max_i;
        let r = lerp(255.0, 191.0, t) as u8;
        let g = lerp(0.0, 64.0, t) as u8;
        let b = lerp(255.0, 255.0, t) as u8;
        let color = Color::Rgb(r, g, b);

        lines.push(Line::from(Span::styled(
            *line_str,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "made by Amigo.D.Cyber",
        Style::default().fg(GRAY).add_modifier(Modifier::ITALIC),
    )));

    let banner = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .style(Style::default().bg(DARK_BG));

    frame.render_widget(banner, area);
}

fn draw_linktree_line(frame: &mut Frame, area: ratatui::layout::Rect) {
    let line = Paragraph::new(Line::from(vec![
        Span::styled("🌳 All Links: ", Style::default().fg(CYAN)),
        Span::styled("https://linktr.ee/Amigo.D.Cyber", Style::default().fg(GRAY)),
    ]))
    .alignment(Alignment::Center)
    .style(Style::default().bg(DARK_BG));

    frame.render_widget(line, area);
}

fn draw_tor_status(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (icon, text, color) = if app.tor_connected {
        ("●", format!(" Connected ({})", app.proxy_addr), GREEN)
    } else {
        (
            "●",
            " Disconnected — start tor service".to_string(),
            Color::Red,
        )
    };

    let active = app
        .downloads
        .iter()
        .filter(|dl| matches!(dl.status, DownloadStatus::InProgress))
        .count();
    let queued = app
        .downloads
        .iter()
        .filter(|dl| matches!(dl.status, DownloadStatus::Queued))
        .count();
    let completed = app
        .downloads
        .iter()
        .filter(|dl| matches!(dl.status, DownloadStatus::Completed))
        .count();

    let line = Line::from(vec![
        Span::styled(
            "  🧅 Tor Status: ",
            Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        Span::styled(icon, Style::default().fg(color)),
        Span::styled(text, Style::default().fg(color)),
        Span::styled(
            format!(
                "    ● {} active   ◷ {} queued   ✓ {} done",
                active, queued, completed
            ),
            Style::default().fg(GRAY),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MAGENTA))
        .style(Style::default().bg(SURFACE));

    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_search_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(12).min(76).max(32);
    let height = 3;
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(
            " 🔎 Filter downloads and history ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CYAN))
        .style(Style::default().bg(DARK_BG));
    let inner = block.inner(popup);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " / ",
                Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
            ),
            Span::styled(&app.search_query, Style::default().fg(WHITE)),
            Span::styled("  (Enter apply · Esc clear)", Style::default().fg(DIM_GRAY)),
        ]))
        .block(block),
        popup,
    );
    let cursor = app.search_cursor.min(app.search_query.len());
    let text_width = unicode_width::UnicodeWidthStr::width(&app.search_query[..cursor]) as u16;
    frame.set_cursor_position((inner.x + 3 + text_width, inner.y));
}

fn draw_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let focused = app.focus == Focus::Input && app.mode != AppMode::Dialog;
    let border_color = if focused { CYAN } else { DIM_GRAY };
    let title_style = if focused {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MAGENTA)
    };

    let block = Block::default()
        .title(Span::styled(" 📎 Paste Download URL ", title_style))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(SURFACE));

    let display_text = if app.input.is_empty() && !focused {
        Line::from(Span::styled(
            "  Enter a URL — mode is auto-detected, you just confirm the save folder",
            Style::default().fg(DIM_GRAY).add_modifier(Modifier::ITALIC),
        ))
    } else {
        Line::from(Span::styled(
            format!(" {}", app.input),
            Style::default().fg(WHITE),
        ))
    };

    frame.render_widget(
        Paragraph::new(display_text)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );

    if focused {
        let cursor = app.cursor_position.min(app.input.len());
        let prefix_width = unicode_width::UnicodeWidthStr::width(&app.input[..cursor]);
        let x = area.x + prefix_width as u16 + 2;
        let y = area.y + 1;
        frame.set_cursor_position((x, y));
    }
}

fn draw_downloads(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let focused = app.focus == Focus::Downloads && app.mode != AppMode::Dialog;
    let title_style = if focused {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD)
    };

    let visible = app
        .downloads
        .iter()
        .enumerate()
        .filter(|(id, _)| app.download_matches_search(*id))
        .count();
    let title = if app.search_query.is_empty() {
        format!(" 📥 Downloads ({}) ", app.downloads.len())
    } else {
        format!(
            " 📥 Downloads {}/{} · /{} ",
            visible,
            app.downloads.len(),
            app.search_query
        )
    };
    let block = Block::default()
        .title(Span::styled(title, title_style))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { CYAN } else { MAGENTA }))
        .style(Style::default().bg(SURFACE));

    if app.downloads.is_empty() || visible == 0 {
        let empty = Paragraph::new(Line::from(Span::styled(
            if app.downloads.is_empty() {
                "  No active downloads — paste a URL above and hit Enter"
            } else {
                "  No downloads match the current search"
            },
            Style::default().fg(DIM_GRAY).add_modifier(Modifier::ITALIC),
        )))
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();

    for (i, dl) in app.downloads.iter().enumerate() {
        if !app.download_matches_search(i) {
            continue;
        }
        let is_selected = app.selected_download == i;
        let prefix = if is_selected { "> " } else { "  " };
        let base_style = if is_selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let icon = match &dl.status {
            DownloadStatus::InProgress => "⏳",
            DownloadStatus::Paused => "⏸",
            DownloadStatus::Queued => "⏱",
            DownloadStatus::Completed => "✅",
            DownloadStatus::Failed(_) => "❌",
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{}{} ", prefix, icon), Style::default().fg(WHITE)),
            Span::styled(&dl.filename, base_style.fg(WHITE)),
            Span::styled(
                format!("  [{}]", dl.category.label()),
                Style::default()
                    .fg(category_color(dl.category))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" [{} | {} conn]", dl.network.label(), dl.chunks),
                Style::default().fg(DIM_GRAY).add_modifier(Modifier::ITALIC),
            ),
        ]));

        match &dl.status {
            DownloadStatus::InProgress => {
                let speed_str = format!("{}/s", format_bytes(dl.speed_bps as u64));

                if let Some(total) = dl.total_bytes {
                    let ratio = dl.downloaded_bytes as f64 / total as f64;
                    let bar_width = (inner.width as usize).saturating_sub(30).max(10);
                    let filled = (ratio * bar_width as f64) as usize;
                    let empty = bar_width.saturating_sub(filled);
                    let eta_str = dl
                        .eta_seconds()
                        .map(|s| format!("ETA {}s", s))
                        .unwrap_or_default();
                    let pct = (ratio * 100.0) as u32;

                    lines.push(Line::from(vec![
                        Span::styled("   ", Style::default()),
                        Span::styled("█".repeat(filled), Style::default().fg(GREEN)),
                        Span::styled("░".repeat(empty), Style::default().fg(DIM_GRAY)),
                        Span::styled(
                            format!(" {:>3}%  {}  {}", pct, speed_str, eta_str),
                            Style::default().fg(CYAN),
                        ),
                    ]));
                } else {
                    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                    let spin_idx =
                        (dl.started_at.elapsed().as_millis() / 100) as usize % spinner.len();
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("   {} Downloading ", spinner[spin_idx]),
                            Style::default().fg(GREEN),
                        ),
                        Span::styled(
                            format!("{}  {}", format_bytes(dl.downloaded_bytes), speed_str),
                            Style::default().fg(CYAN),
                        ),
                    ]));
                }
            }
            DownloadStatus::Paused => {
                let ratio = if let Some(total) = dl.total_bytes {
                    if total > 0 {
                        dl.downloaded_bytes as f64 / total as f64
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                let bar_width = (inner.width as usize).saturating_sub(30).max(10);
                let filled = (ratio * bar_width as f64) as usize;
                let empty = bar_width.saturating_sub(filled);
                let pct = (ratio * 100.0) as u32;
                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled("█".repeat(filled), Style::default().fg(YELLOW)),
                    Span::styled("░".repeat(empty), Style::default().fg(DIM_GRAY)),
                    Span::styled(format!(" {:>3}%  PAUSED", pct), Style::default().fg(YELLOW)),
                ]));
            }
            DownloadStatus::Queued => {
                lines.push(Line::from(Span::styled(
                    "   ⏳ Waiting in queue",
                    Style::default().fg(YELLOW),
                )));
            }
            DownloadStatus::Completed => {
                lines.push(Line::from(Span::styled(
                    format!("   ✓ Done ({})", format_bytes(dl.downloaded_bytes)),
                    Style::default().fg(GREEN),
                )));
            }
            DownloadStatus::Failed(err) => {
                lines.push(Line::from(Span::styled(
                    format!("   ✗ {}  [R/Space retry]", err),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        lines.push(Line::from(""));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.download_scroll, 0)),
        inner,
    );
}

// ── History pane ─────────────────────────────────────────────────

fn history_status_visual(status: &str) -> (&'static str, Color) {
    match status {
        history::ST_IN_PROGRESS => ("⏳", CYAN),
        history::ST_COMPLETED => ("✅", GREEN),
        history::ST_QUEUED => ("⏱", YELLOW),
        history::ST_FAILED => ("❌", Color::Red),
        _ => ("•", GRAY),
    }
}

fn draw_history(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let focused = app.focus == Focus::History && app.mode != AppMode::Dialog;
    let title_style = if focused {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD)
    };

    let visible = app
        .history
        .iter()
        .enumerate()
        .filter(|(id, _)| app.history_matches_search(*id))
        .count();
    let unfinished = app
        .history
        .iter()
        .enumerate()
        .filter(|(id, e)| app.history_matches_search(*id) && e.status != history::ST_COMPLETED)
        .count();
    let history_title = if app.search_query.is_empty() {
        format!(" 📜 History ({}) ", app.history.len())
    } else {
        format!(
            " 📜 History {}/{} · /{} ",
            visible,
            app.history.len(),
            app.search_query
        )
    };

    let block = Block::default()
        .title(Span::styled(history_title, title_style))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { CYAN } else { MAGENTA }))
        .style(Style::default().bg(SURFACE));

    // Reserve one bottom row for key hints.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(block.inner(area));

    frame.render_widget(block, area);

    let hint = Paragraph::new(Line::from(Span::styled(
        " ↑↓ select · R retry failed · D delete · Tab back",
        Style::default().fg(DIM_GRAY),
    )));
    frame.render_widget(hint, rows[1]);

    if app.history.is_empty() || visible == 0 {
        let empty = Paragraph::new(Line::from(Span::styled(
            if app.history.is_empty() {
                "  Nothing yet — finished and interrupted downloads appear here"
            } else {
                "  No history entries match the current search"
            },
            Style::default().fg(DIM_GRAY).add_modifier(Modifier::ITALIC),
        )));
        frame.render_widget(empty, rows[0]);
        return;
    }

    let mut lines = Vec::new();

    if unfinished > 0 {
        lines.push(Line::from(Span::styled(
            format!(" ⚠ {} unfinished download(s) on record", unfinished),
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    for (i, entry) in app.history.iter().enumerate() {
        if !app.history_matches_search(i) {
            continue;
        }
        let is_selected = app.history_selected == i;
        let prefix = if is_selected { "> " } else { "  " };
        let (icon, color) = history_status_visual(&entry.status);

        let size_part = match entry.total_bytes {
            Some(total) if total > 0 => {
                let pct = ((entry.downloaded_bytes as f64 / total as f64) * 100.0) as u32;
                format!(
                    "{}/{} ({}%)",
                    format_bytes(entry.downloaded_bytes),
                    format_bytes(total),
                    pct.min(100)
                )
            }
            _ => format_bytes(entry.downloaded_bytes),
        };

        let err_part = match (&entry.error, entry.status.as_str()) {
            (Some(e), history::ST_FAILED) => format!(" · {}", truncate(e, 42)),
            _ => String::new(),
        };

        let base_style = if is_selected {
            Style::default().add_modifier(Modifier::BOLD).fg(WHITE)
        } else {
            Style::default().fg(WHITE)
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{}{} ", prefix, icon), Style::default().fg(color)),
            Span::styled(entry.filename.clone(), base_style),
            Span::styled(
                format!(
                    "  [{}]",
                    if entry.category.is_empty() {
                        "Other"
                    } else {
                        &entry.category
                    }
                ),
                Style::default().fg(category_color(DownloadCategory::parse(&entry.category))),
            ),
            Span::styled(format!("  {}", size_part), Style::default().fg(CYAN)),
            Span::styled(
                format!("  [{}]", entry.network.to_uppercase()),
                Style::default().fg(DIM_GRAY),
            ),
            Span::styled(
                format!(" {} · {}", entry.status_label(), entry.updated_at),
                Style::default().fg(color),
            ),
            Span::styled(err_part, Style::default().fg(Color::Red)),
        ]));

        lines.push(Line::from(Span::styled(
            format!("     {}", truncate(&entry.url, 96)),
            Style::default().fg(DIM_GRAY).add_modifier(Modifier::ITALIC),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.history_scroll, 0)),
        rows[0],
    );
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", cut)
    }
}

// ── Log ──────────────────────────────────────────────────────────

fn draw_log(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .title(Span::styled(
            " 📋 Log (scroll ↑↓ PgUp/PgDn) ",
            Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(40, 40, 40)))
        .style(Style::default().bg(Color::Rgb(10, 10, 10)));

    let max_visible = 5;
    let total_logs = app.log_messages.len();
    let scroll = app.log_scroll as usize;

    let start = scroll.min(total_logs.saturating_sub(max_visible));
    let end = (start + max_visible).min(total_logs);

    let log_lines: Vec<Line> = app
        .log_messages
        .iter()
        .skip(start)
        .take(end - start)
        .map(|msg| {
            let color = if msg.contains('✅') || msg.contains("Connected") || msg.contains("⚙") {
                GREEN
            } else if msg.contains('❌') || msg.contains('⚠') || msg.contains('🗑') {
                Color::Red
            } else if msg.contains("📥")
                || msg.contains("🔗")
                || msg.contains('🔁')
                || msg.contains("💾")
                || msg.contains("⚡")
                || msg.contains("📌")
            {
                CYAN
            } else {
                GRAY
            };
            Line::from(Span::styled(
                format!("  {}", msg),
                Style::default().fg(color),
            ))
        })
        .collect();

    frame.render_widget(Paragraph::new(log_lines).block(block), area);
}

// ── Disclaimer + footer ──────────────────────────────────────────

fn draw_disclaimer_and_help(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let bottom = Layout::default()
        .direction(Direction::Vertical)
        .margin(0)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(area);

    let footer_area = bottom[1];

    let disclaimer = Paragraph::new(Line::from(vec![
        Span::styled(" ⚠️  ", Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
        Span::styled(
            "The developer is NOT responsible for any files downloaded without proper authorization.",
            Style::default().fg(YELLOW),
        ),
    ]));

    frame.render_widget(disclaimer, footer_area);

    if footer_area.height >= 2 {
        let help_area = ratatui::layout::Rect {
            x: footer_area.x,
            y: footer_area.y + 1,
            width: footer_area.width,
            height: 1,
        };

        let focus_label = match app.focus {
            Focus::Input => "INPUT",
            Focus::Downloads => "DOWNLOADS",
            Focus::History => "HISTORY",
        };

        let k = |s: &'static str| {
            Span::styled(s, Style::default().fg(GREEN).add_modifier(Modifier::BOLD))
        };

        let help = Line::from(vec![
            k("[Enter]"),
            Span::styled(" Download  ", Style::default().fg(WHITE)),
            k("[Tab]"),
            Span::styled(" Panes  ", Style::default().fg(WHITE)),
            k("[S]"),
            Span::styled(" Settings  ", Style::default().fg(WHITE)),
            k("[H]"),
            Span::styled(" Help  ", Style::default().fg(WHITE)),
            k("[Space]"),
            Span::styled(" Pause/Resume  ", Style::default().fg(WHITE)),
            k("[/]"),
            Span::styled(" Search  ", Style::default().fg(WHITE)),
            k("[P/U]"),
            Span::styled(" Pause/Resume all  ", Style::default().fg(WHITE)),
            k("[Y]"),
            Span::styled(" Retry all  ", Style::default().fg(WHITE)),
            Span::styled(
                format!("▸ {}", focus_label),
                Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  [Esc]",
                Style::default().fg(LOGO_PINK).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Quit", Style::default().fg(WHITE)),
        ]);

        frame.render_widget(
            Paragraph::new(help).style(Style::default().bg(DARK_BG)),
            help_area,
        );
    }
}

// ── Save-location dialog ─────────────────────────────────────────

fn popup_area(area: Rect, pct_h: u16, height: u16, pct_w: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(pct_h),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(pct_w),
            Constraint::Percentage(100 - pct_w * 2),
            Constraint::Percentage(pct_w),
        ])
        .split(vertical[1])[1]
}

fn draw_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let popup_area = popup_area(area, 28, 13, 20);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(Span::styled(
            " 📁 Choose Save Location ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CYAN))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let mode_sel = match app.dialog_network {
        NetworkMode::Tor => "[ TOR ]",
        NetworkMode::Normal => "[ NORMAL ]",
    };
    let mode_note = if app.dialog_mode_auto {
        "auto-detected"
    } else {
        "forced in Settings"
    };

    let url_display = truncate(&app.dialog_url, (inner.width as usize).saturating_sub(6));

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  🔗 ", Style::default().fg(MAGENTA)),
        Span::styled(url_display, Style::default().fg(GRAY)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  🌐 Mode:  ", Style::default().fg(MAGENTA)),
        Span::styled(
            format!("{} ", mode_sel),
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("({})", mode_note),
            Style::default().fg(DIM_GRAY).add_modifier(Modifier::ITALIC),
        ),
    ]));
    lines.push(Line::from(""));

    let path_style = if app.dialog_focus == DialogFocus::Path {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(GRAY)
    };
    let path_marker = if app.dialog_focus == DialogFocus::Path {
        "> "
    } else {
        "  "
    };
    let path_prefix = format!("{}{}", path_marker, DIALOG_PATH_LABEL);
    lines.push(Line::from(vec![
        Span::styled(path_prefix.clone(), path_style),
        Span::styled(app.dialog_path.clone(), path_style),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    let btn = |focused: bool, label: &str, color: Color| {
        if focused {
            Span::styled(
                label.to_string(),
                Style::default()
                    .fg(DARK_BG)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(label.to_string(), Style::default().fg(color))
        }
    };

    lines.push(Line::from(vec![
        Span::from("   "),
        btn(app.dialog_focus == DialogFocus::Start, "[ START ]", GREEN),
        Span::from("   "),
        btn(
            app.dialog_focus == DialogFocus::Always,
            "[ ALWAYS HERE ]",
            YELLOW,
        ),
        Span::from("   "),
        btn(
            app.dialog_focus == DialogFocus::Cancel,
            "[ CANCEL ]",
            LOGO_PINK,
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "   Type to edit path · Arrows navigate · Enter selects",
        Style::default().fg(DIM_GRAY),
    )));

    frame.render_widget(Paragraph::new(lines), inner);

    if app.dialog_focus == DialogFocus::Path {
        let cursor = app.dialog_cursor.min(app.dialog_path.len());
        let prefix_width = unicode_width::UnicodeWidthStr::width(path_prefix.as_str()) as u16;
        let typed_width = unicode_width::UnicodeWidthStr::width(&app.dialog_path[..cursor]) as u16;
        frame.set_cursor_position((inner.x + prefix_width + typed_width, inner.y + 4));
    }
}

// ── Help ─────────────────────────────────────────────────────────

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(8),
            Constraint::Percentage(84),
            Constraint::Percentage(8),
        ])
        .split(area);

    let popup_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(14),
            Constraint::Percentage(72),
            Constraint::Percentage(14),
        ])
        .split(popup_layout[1])[1];

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(Span::styled(
            " 📖 How to Use OnionDownOda ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MAGENTA))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let mut lines = vec![Line::from("")];

    // ── Connection modes explainer ──
    lines.push(Line::from(Span::styled(
        "  🌐 Connection Modes",
        Style::default()
            .fg(LOGO_MAGENTA)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "  NORMAL MODE — Downloads directly over the regular internet using standard",
        Style::default().fg(WHITE),
    )));
    lines.push(Line::from(Span::styled(
        "  HTTP/HTTPS protocols. Fast and suitable for most files.",
        Style::default().fg(WHITE),
    )));
    lines.push(Line::from(Span::styled(
        "  TOR MODE — Routes the download through the Tor anonymity network via",
        Style::default().fg(WHITE),
    )));
    lines.push(Line::from(Span::styled(
        "  .onion URLs. Slower, but private and reaches hidden services. Requires",
        Style::default().fg(WHITE),
    )));
    lines.push(Line::from(Span::styled(
        "  Tor running locally (socks5://127.0.0.1:9050 by default).",
        Style::default().fg(WHITE),
    )));
    lines.push(Line::from(Span::styled(
        "  🔗 SMART LINKS — Paste any link: .onion → Tor automatically, regular",
        Style::default().fg(GREEN),
    )));
    lines.push(Line::from(Span::styled(
        "  http(s) links → Normal automatically. No questions asked.",
        Style::default().fg(GREEN),
    )));
    lines.push(Line::from(""));

    let instructions = vec![
        (
            "📎 Start Download:",
            "Paste a URL, hit Enter, confirm the save folder. That's it — defaults handle the rest.",
        ),
        (
            "📁 Always Here:",
            "In the save dialog, pick ALWAYS HERE once to skip the folder prompt forever.",
        ),
        (
            "📜 History:",
            "Every download is logged with its size, status and date. Press D to remove an entry.",
        ),
        (
            "⚙️ Settings:",
            "Press S or Ctrl+S from anywhere — default folder, connection mode, threads.",
        ),
        (
            "❌ Failures/403:",
            "If a file fails immediately, the host blocked us. Verify the exact direct link.",
        ),
        (
            "📋 Log Output:",
            "Errors and start states log in the bottom console matrix.",
        ),
        (
            "🔎 Search:",
            "From Downloads or History, press / and type a filename, URL, or category.",
        ),
        (
            "🎛 Batch Actions:",
            "P pauses active items, U resumes paused items, and Y retries failed items.",
        ),
    ];

    for (cmd, desc) in instructions {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                cmd.to_string(),
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}", desc), Style::default().fg(WHITE)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "   [ Press ESC or Enter to close Help ]",
        Style::default().fg(LOGO_PINK).add_modifier(Modifier::BOLD),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false }),
        inner,
    );
}

// ── Settings panel ───────────────────────────────────────────────

fn draw_settings(frame: &mut Frame, app: &App, area: Rect) {
    let popup_area = popup_area(area, 25, 12, 18);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(Span::styled(
            " ⚙️ Settings ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CYAN))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let field_style = |f: SettingsField| {
        if app.settings_field == f {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(GRAY)
        }
    };
    let marker = |f: SettingsField| {
        if app.settings_field == f {
            "> "
        } else {
            "  "
        }
    };

    let threads_display = if app.settings_field == SettingsField::Threads {
        format!("‹ {} ›", app.settings.parallel_threads)
    } else {
        format!("  {}  ", app.settings.parallel_threads)
    };
    let ask_display = if app.settings.ask_directory {
        "ON"
    } else {
        "OFF"
    };

    let dir_row_is_focused = app.settings_field == SettingsField::Directory;
    let dir_marker = if dir_row_is_focused { "> " } else { "  " };
    let dir_prefix = format!("{}{}", dir_marker, SETTINGS_DIR_LABEL);

    let done_style = if app.settings_field == SettingsField::Done {
        Style::default()
            .fg(DARK_BG)
            .bg(GREEN)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(GREEN)
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(dir_prefix, field_style(SettingsField::Directory)),
            Span::styled(
                app.settings_dir_buf.clone(),
                field_style(SettingsField::Directory),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}Connection Mode:  ", marker(SettingsField::Mode)),
                field_style(SettingsField::Mode),
            ),
            Span::styled(
                format!("‹ {} ›", app.settings.default_mode.label()),
                field_style(SettingsField::Mode),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}Parallel Threads:  ", marker(SettingsField::Threads)),
                field_style(SettingsField::Threads),
            ),
            Span::styled(threads_display, field_style(SettingsField::Threads)),
        ]),
        Line::from(vec![
            Span::styled(
                format!(
                    "{}Ask Folder Each Time:  ",
                    marker(SettingsField::AskEveryTime)
                ),
                field_style(SettingsField::AskEveryTime),
            ),
            Span::styled(ask_display, field_style(SettingsField::AskEveryTime)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::from("   "),
            Span::styled("  [ DONE ]  ", done_style),
            Span::styled(
                "(Esc saves & closes · changes apply instantly)",
                Style::default().fg(DIM_GRAY),
            ),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);

    if dir_row_is_focused {
        let cursor = app.settings_cursor.min(app.settings_dir_buf.len());
        let prefix_width = unicode_width::UnicodeWidthStr::width(dir_marker) as u16
            + unicode_width::UnicodeWidthStr::width(SETTINGS_DIR_LABEL) as u16;
        let typed_width =
            unicode_width::UnicodeWidthStr::width(&app.settings_dir_buf[..cursor]) as u16;
        frame.set_cursor_position((inner.x + prefix_width + typed_width, inner.y + 1));
    }
}
