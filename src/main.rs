use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::panic;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

mod app;
mod banner;
mod config;
mod downloader;
mod error;
mod history;
mod tor;
mod ui;

#[cfg(windows)]
mod close_signal {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    static REQUESTED: AtomicBool = AtomicBool::new(false);
    static FINISHED: AtomicBool = AtomicBool::new(false);

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    type Handler = unsafe extern "system" fn(u32) -> i32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<Handler>, add: i32) -> i32;
    }

    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                REQUESTED.store(true, Ordering::SeqCst);

                // Give the async main loop time to flush and persist its work.
                // Windows otherwise terminates a console process shortly after
                // this callback returns for a close/logoff/shutdown event.
                let deadline = Instant::now() + Duration::from_secs(4);
                while !FINISHED.load(Ordering::SeqCst) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(25));
                }
                1
            }
            _ => 0,
        }
    }

    pub fn install() {
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler), 1);
        }
    }

    pub fn requested() -> bool {
        REQUESTED.load(Ordering::SeqCst)
    }

    pub fn finish() {
        FINISHED.store(true, Ordering::SeqCst);
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler), 0);
        }
    }
}

#[cfg(not(windows))]
mod close_signal {
    pub fn install() {}
    pub fn requested() -> bool {
        false
    }
    pub fn finish() {}
}

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
    close_signal::install();

    let result = run_app(&mut terminal, &config).await;

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

    close_signal::finish();

    Ok(())
}

/// Register a download and kick off its tokio task.
fn spawn_transfer(
    app: &mut App,
    transfers: &mut JoinSet<()>,
    shutdown_flags: &mut Vec<Arc<AtomicBool>>,
    url: String,
    network: NetworkMode,
    chunks: usize,
    output_dir: PathBuf,
) {
    let dl_id = app.start_download(&url, network.clone(), chunks, output_dir.clone());
    app.add_log(&format!("🔗 Connecting to {}...", url));

    let client_res = match network {
        NetworkMode::Tor => tor::build_client(&app.proxy_addr),
        NetworkMode::Normal => tor::build_normal_client(),
    };

    let client = match client_res {
        Ok(c) => c,
        Err(e) => {
            app.fail_live(dl_id, &e.to_string());
            return;
        }
    };

    let task_key = history::new_id();
    let tx = app.progress_tx.clone();
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    shutdown_flags.push(shutdown_flag.clone());

    transfers.spawn(async move {
        let _ = downloader::download_file(
            &client,
            &url,
            &output_dir,
            tx,
            dl_id,
            chunks,
            &task_key,
            shutdown_flag,
        )
        .await;
    });
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    // ── Create app state ───────────────────────────
    let mut app = App::new(
        config.proxy.clone(),
        config.output_dir.clone(),
        config.verbose,
    );
    app.apply_config(
        config.output_dir.clone(),
        config.default_mode,
        config.parallel_threads,
        config.ask_directory,
    );

    // ── Load persistent download history ───────────
    let unfinished = app.load_history();
    if unfinished > 0 {
        app.add_log(&format!(
            "📋 {} unfinished download(s) recorded in History",
            unfinished
        ));
    }

    let mut transfers = JoinSet::new();
    let mut shutdown_flags: Vec<Arc<AtomicBool>> = Vec::new();

    // ── Check Tor connectivity ─────────────────────
    app.add_log(&format!("Checking Tor proxy at {}...", config.proxy));
    app.tor_connected = tor::check_tor_connection(&config.proxy).await;
    if app.tor_connected {
        app.add_log("🧅 Connected to Tor SOCKS5 proxy");
    } else {
        app.add_log("⚠ Tor proxy not available — start tor service to download");
    }

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        // Poll for keyboard events (50ms tick for responsive UI updates)
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.handle_key(key) {
                        Action::StartDownload {
                            url,
                            network,
                            chunks,
                            output_dir,
                        } => spawn_transfer(
                            &mut app,
                            &mut transfers,
                            &mut shutdown_flags,
                            url,
                            network,
                            chunks,
                            output_dir,
                        ),
                        Action::Quit => {
                            app.should_quit = true;
                            break;
                        }
                        Action::ShowDialog | Action::None => {}
                    }
                }
            }
        }

        // Process any download progress updates + throttled history autosave.
        app.process_progress();
        app.persist_if_due(false);

        if app.should_quit || close_signal::requested() {
            break;
        }
    }

    // Interrupted transfers become resumable history entries on exit.
    app.shutdown_mark();
    for flag in &shutdown_flags {
        flag.store(true, Ordering::SeqCst);
    }

    // Let every transfer flush its current buffer and finish at a resumable
    // boundary. A deadline prevents a stuck network request from blocking exit.
    let deadline = tokio::time::sleep(Duration::from_secs(4));
    tokio::pin!(deadline);
    while !transfers.is_empty() {
        tokio::select! {
            _ = &mut deadline => {
                transfers.abort_all();
                break;
            }
            _ = transfers.join_next() => {}
        }
    }
    while transfers.join_next().await.is_some() {}

    app.process_progress();
    app.persist_if_due(true);

    Ok(())
}
