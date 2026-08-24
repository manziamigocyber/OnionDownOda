use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::panic;
use std::time::Duration;

mod app;
mod banner;
mod config;
mod downloader;
mod error;
mod tor;
mod ui;

use app::{Action, App, NetworkMode};
use config::Config;

fn setup_panic_hook() {
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Restore terminal before printing panic message
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(info);
    }));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_panic_hook();

    let config = Config::load();

    // ── Setup terminal ─────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // ── Create app state ───────────────────────────
    let mut app = App::new(
        config.proxy.clone(),
        config.output_dir.clone(),
        config.verbose,
    );

    // ── Check Tor connectivity ─────────────────────
    app.add_log(&format!("Checking Tor proxy at {}...", config.proxy));
    app.tor_connected = tor::check_tor_connection(&config.proxy).await;
    if app.tor_connected {
        app.add_log("🧅 Connected to Tor SOCKS5 proxy");
    } else {
        app.add_log("⚠ Tor proxy not available — start tor service to download");
    }

    // ── Main event loop ────────────────────────────
    let result = run_app(&mut terminal, &mut app).await;

    // ── Restore terminal ───────────────────────────
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {}", e);
    }

    Ok(())
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Poll for keyboard events (50ms tick for responsive UI updates)
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let action = app.handle_key(key);
                    match action {
                        Action::ShowDialog(_url) => {
                            app.add_log("Configure download settings and press Enter to start");
                        }
                        Action::StartDownload {
                            url,
                            network,
                            chunks,
                        } => {
                            let tx = app.progress_tx.clone();
                            let dl_id = app.start_download(&url, network.clone(), chunks);
                            app.add_log(&format!("🔗 Connecting to {}...", url));

                            let client_res = match network {
                                NetworkMode::Tor => tor::build_client(&app.proxy_addr),
                                NetworkMode::Normal => tor::build_normal_client(),
                            };

                            let client = match client_res {
                                Ok(c) => c,
                                Err(e) => {
                                    app.add_log(&format!("❌ Client error: {}", e));
                                    app.downloads[dl_id].status =
                                        app::DownloadStatus::Failed(e.to_string());
                                    continue;
                                }
                            };

                            let output_dir = app.output_dir.clone();
                            let url_clone = url.clone();
                            let paused_flag = app.downloads[dl_id].paused.clone();

                            tokio::spawn(async move {
                                let _ = downloader::download_file(
                                    &client,
                                    &url_clone,
                                    &output_dir,
                                    tx,
                                    paused_flag,
                                    dl_id,
                                    chunks,
                                )
                                .await;
                            });
                        }
                        Action::Quit => {
                            app.should_quit = true;
                            break;
                        }
                        Action::Pause(id) => {
                            app.pause_download(id);
                        }
                        Action::Resume(id) => {
                            app.resume_download(id);
                        }
                        Action::None => {}
                    }
                }
            }
        }

        // Process any download progress updates
        app.process_progress();

        if app.should_quit {
            break;
        }
    }

    Ok(())
}
