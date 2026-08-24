use crate::app::{format_bytes, App, AppMode, DialogFocus, DownloadStatus, Focus, NetworkMode};
use crate::banner::BANNER;
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
            Constraint::Percentage(28),
            Constraint::Percentage(7),
            Constraint::Percentage(10),
            Constraint::Percentage(32),
            Constraint::Percentage(12),
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
    draw_log(frame, app, main_layout[4]);
    draw_disclaimer_and_help(frame, app, size);

    if app.mode == AppMode::Dialog {
        draw_dialog(frame, app, size);
    } else if app.mode == AppMode::Help {
        draw_help(frame, size);
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

    let line = Line::from(vec![
        Span::styled(
            "  🧅 Tor Status: ",
            Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        Span::styled(icon, Style::default().fg(color)),
        Span::styled(text, Style::default().fg(color)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MAGENTA))
        .style(Style::default().bg(SURFACE));

    frame.render_widget(Paragraph::new(line).block(block), area);
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
            "  Enter a URL and press Enter to configure download...",
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

    let block = Block::default()
        .title(Span::styled(" 📥 Downloads ", title_style))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { CYAN } else { MAGENTA }))
        .style(Style::default().bg(SURFACE));

    if app.downloads.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "  No downloads yet — paste a URL above and hit Enter",
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
        let is_selected = app.selected_download == i;
        let prefix = if is_selected { "> " } else { "  " };
        let base_style = if is_selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let icon = match &dl.status {
            DownloadStatus::InProgress => "⏳",
            DownloadStatus::Paused => "⏸️",
            DownloadStatus::Completed => "✅",
            DownloadStatus::Failed(_) => "❌",
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{}{} ", prefix, icon), Style::default().fg(WHITE)),
            Span::styled(&dl.filename, base_style.fg(WHITE)),
            Span::styled(
                format!(
                    "  [{} | {} chunks]",
                    if dl.network == NetworkMode::Tor {
                        "TOR"
                    } else {
                        "NORMAL"
                    },
                    dl.chunks
                ),
                Style::default().fg(DIM_GRAY).add_modifier(Modifier::ITALIC),
            ),
        ]));

        match &dl.status {
            DownloadStatus::InProgress | DownloadStatus::Paused => {
                let is_paused = dl.status == DownloadStatus::Paused;
                let speed = if is_paused { 0.0 } else { dl.speed_bps };
                let speed_str = format!("{}/s", format_bytes(speed as u64));

                if let Some(total) = dl.total_bytes {
                    let ratio = dl.downloaded_bytes as f64 / total as f64;
                    let bar_width = (inner.width as usize).saturating_sub(30).max(10);
                    let filled = (ratio * bar_width as f64) as usize;
                    let empty = bar_width.saturating_sub(filled);
                    let eta_str = if is_paused {
                        "PAUSED".to_string()
                    } else {
                        dl.eta_seconds()
                            .map(|s| format!("ETA {}s", s))
                            .unwrap_or_default()
                    };
                    let pct = (ratio * 100.0) as u32;
                    let bar_color = if is_paused { Color::Red } else { GREEN };

                    lines.push(Line::from(vec![
                        Span::styled("   ", Style::default()),
                        Span::styled("█".repeat(filled), Style::default().fg(bar_color)),
                        Span::styled("░".repeat(empty), Style::default().fg(DIM_GRAY)),
                        Span::styled(
                            format!(" {:>3}%  {}  {}", pct, speed_str, eta_str),
                            Style::default().fg(if is_paused { Color::Red } else { CYAN }),
                        ),
                    ]));
                } else {
                    if is_paused {
                        lines.push(Line::from(vec![
                            Span::styled(
                                "   ⏸️ Paused ".to_string(),
                                Style::default().fg(Color::Red),
                            ),
                            Span::styled(
                                format!("{}  {}", format_bytes(dl.downloaded_bytes), speed_str),
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
            }
            DownloadStatus::Completed => {
                lines.push(Line::from(Span::styled(
                    format!("   ✓ Done ({})", format_bytes(dl.downloaded_bytes)),
                    Style::default().fg(GREEN),
                )));
            }
            DownloadStatus::Failed(err) => {
                lines.push(Line::from(Span::styled(
                    format!("   ✗ {}", err),
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

fn draw_log(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .title(Span::styled(
            " 📋 Log (scroll ↑↓ PgUp/PgDn) ",
            Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(40, 40, 40)))
        .style(Style::default().bg(Color::Rgb(10, 10, 10)));

    let max_visible = 4;
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
            let color = if msg.contains('✅') || msg.contains("Connected") {
                GREEN
            } else if msg.contains('❌') || msg.contains('⚠') {
                Color::Red
            } else if msg.contains("📥") || msg.contains("🔗") {
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
            Focus::Help => "HELP",
        };

        let help_badge_style = if app.focus == Focus::Help {
            Style::default()
                .fg(DARK_BG)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        };

        let help = Line::from(vec![
            Span::styled(
                "[Enter]",
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Download  ", Style::default().fg(WHITE)),
            Span::styled(
                "[Space]",
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Pause  ", Style::default().fg(WHITE)),
            Span::styled(
                "[Tab]",
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Focus  ", Style::default().fg(WHITE)),
            Span::styled(
                "[↑↓]",
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Scroll/Select  ", Style::default().fg(WHITE)),
            Span::styled(" [HELP] ", help_badge_style),
            Span::styled(
                "  [Esc]",
                Style::default().fg(LOGO_PINK).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Quit  ", Style::default().fg(WHITE)),
            Span::styled(
                format!("▸ {}", focus_label),
                Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
            ),
        ]);

        frame.render_widget(
            Paragraph::new(help).style(Style::default().bg(DARK_BG)),
            help_area,
        );
    }
}

fn draw_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Length(12),
            Constraint::Percentage(30),
        ])
        .split(area);

    let popup_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(60),
            Constraint::Percentage(20),
        ])
        .split(popup_layout[1])[1];

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(Span::styled(
            " Configure Download ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CYAN))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let mut lines = Vec::new();
    lines.push(Line::from(""));

    let net_style = if app.dialog_focus == DialogFocus::Network {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(GRAY)
    };
    let net_sel = match app.dialog_network {
        NetworkMode::Tor => "[ TOR ]  Normal ",
        NetworkMode::Normal => "  Tor   [ NORMAL ]",
    };
    lines.push(Line::from(vec![
        Span::styled(
            if app.dialog_focus == DialogFocus::Network {
                "  > Network:  "
            } else {
                "    Network:  "
            },
            net_style,
        ),
        Span::styled(net_sel, net_style),
    ]));
    lines.push(Line::from(""));

    let chk_style = if app.dialog_focus == DialogFocus::Chunks {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(GRAY)
    };
    let c16 = if app.dialog_chunks == 16 {
        "[16]"
    } else {
        " 16 "
    };
    let c32 = if app.dialog_chunks == 32 {
        "[32]"
    } else {
        " 32 "
    };
    let c64 = if app.dialog_chunks == 64 {
        "[64]"
    } else {
        " 64 "
    };
    let c100 = if app.dialog_chunks == 100 {
        "[100]"
    } else {
        " 100 "
    };

    lines.push(Line::from(vec![
        Span::styled(
            if app.dialog_focus == DialogFocus::Chunks {
                "  > Chunks:   "
            } else {
                "    Chunks:   "
            },
            chk_style,
        ),
        Span::styled(format!("{} {} {} {}", c16, c32, c64, c100), chk_style),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    let start_style = if app.dialog_focus == DialogFocus::Start {
        Style::default()
            .fg(DARK_BG)
            .bg(GREEN)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(GREEN)
    };
    let cancel_style = if app.dialog_focus == DialogFocus::Cancel {
        Style::default()
            .fg(DARK_BG)
            .bg(LOGO_PINK)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(LOGO_PINK)
    };

    let buttons = Line::from(vec![
        Span::styled("   [ START ]  ", start_style),
        Span::from("      "),
        Span::styled("   [ CANCEL ]   ", cancel_style),
    ]);

    lines.push(buttons);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "   (Use Arrows/Tab to navigate, Enter to select)",
        Style::default().fg(DIM_GRAY),
    )));

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Length(20),
            Constraint::Percentage(15),
        ])
        .split(area);

    let popup_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Percentage(70),
            Constraint::Percentage(15),
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

    let mut lines = Vec::new();
    lines.push(Line::from(""));

    let instructions = vec![
        (
            "📎 Start Download:",
            "Paste standard or .onion URL inside the top Input box and hit Enter.",
        ),
        (
            "⚙️ Configurations:",
            "In the pop-up, you can select whether to force Tor routing or normal.",
        ),
        (
            "🚀 Chunks:",
            "Select the concurrent thread limits (Parallel downloads). Use Left/Right keys.",
        ),
        (
            "⏸️ Pause/Resume:",
            "Press Tab to switch to the Downloads pane, use Arrows to select, hit Space.",
        ),
        (
            "❌ Failures/403:",
            "If a file fails immediately, the host blocked us. Verify the exact direct link.",
        ),
        (
            "📋 Log Output:",
            "Errors and start states log in the bottom console matrix.",
        ),
    ];

    for (cmd, desc) in instructions {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                cmd.to_string(),
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" - {}", desc), Style::default().fg(WHITE)),
        ]));
        lines.push(Line::from(""));
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
