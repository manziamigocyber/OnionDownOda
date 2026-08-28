use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::collections::HashMap;
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

/// Number of complete downloads allowed to use network connections at once.
/// Each active download may still use its configured parallel chunk count.
const MAX_ACTIVE_DOWNLOADS: usize = 3;
const MAX_AUTOMATIC_RETRIES: u8 = 3;

fn setup_panic_hook() {
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(info);
    }));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_panic_hook();

    let config = Config::load();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    close_signal::install();

    let result = run_app(&mut terminal, &config).await;

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

/// Register a download and either start it or put it in the queue.
fn spawn_transfer(
    app: &mut App,
    transfers: &mut JoinSet<usize>,
    shutdown_flags: &mut HashMap<usize, Arc<AtomicBool>>,
    draining: &mut HashMap<usize, Arc<AtomicBool>>,
    queued: &mut Vec<usize>,
    url: String,
    network: NetworkMode,
    chunks: usize,
    output_dir: PathBuf,
) {
    let dl_id = app.start_download(&url, network.clone(), chunks, output_dir.clone());
    if shutdown_flags.len() + draining.len() >= MAX_ACTIVE_DOWNLOADS {
        app.queue_download(dl_id);
        queued.push(dl_id);
        return;
    }

    launch_new_transfer(
        app,
        transfers,
        shutdown_flags,
        draining,
        dl_id,
        url,
        network,
        chunks,
        output_dir,
    );
}

/// Launch a newly-created transfer that has not been started before.
fn launch_new_transfer(
    app: &mut App,
    transfers: &mut JoinSet<usize>,
    shutdown_flags: &mut HashMap<usize, Arc<AtomicBool>>,
    draining: &mut HashMap<usize, Arc<AtomicBool>>,
    dl_id: usize,
    url: String,
    network: NetworkMode,
    chunks: usize,
    output_dir: PathBuf,
) {
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

    let task_key = app
        .downloads
        .get(dl_id)
        .and_then(|d| d.history_id.clone())
        .unwrap_or_else(history::new_id);
    let tx = app.progress_tx.clone();
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    shutdown_flags.insert(dl_id, shutdown_flag.clone());
    draining.remove(&dl_id);

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
        dl_id
    });
}

/// Spawn a new transfer for a resumed download, reusing the saved resume info.
fn spawn_resume(
    app: &mut App,
    transfers: &mut JoinSet<usize>,
    shutdown_flags: &mut HashMap<usize, Arc<AtomicBool>>,
    draining: &mut HashMap<usize, Arc<AtomicBool>>,
    id: usize,
) {
    let Some(dl) = app.downloads.get(id) else {
        return;
    };
    let Some(info) = &dl.resume_info else {
        return;
    };

    let url = info.url.clone();
    let network = info.network.clone();
    let chunks = info.chunks;
    let total_size = info.total_size;
    let output_dir = info.output_dir.clone();
    let temp_dir = info.temp_dir.clone();
    let partial_path = info.partial_path.clone();
    let task_key = dl.history_id.clone().unwrap_or_else(history::new_id);
    app.add_log(&format!("🔄 Resuming #{} via {}", id, url));

    let client_res = match network {
        NetworkMode::Tor => tor::build_client(&app.proxy_addr),
        NetworkMode::Normal => tor::build_normal_client(),
    };

    let client = match client_res {
        Ok(c) => c,
        Err(e) => {
            app.fail_live(id, &e.to_string());
            return;
        }
    };
    let tx = app.progress_tx.clone();
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    shutdown_flags.insert(id, shutdown_flag.clone());
    draining.remove(&id);

    transfers.spawn(async move {
        let partial_path_ref = partial_path.as_deref();
        let _ = downloader::download_file_resume(
            &client,
            &url,
            &output_dir,
            tx,
            id,
            chunks,
            &task_key,
            shutdown_flag,
            partial_path_ref,
            temp_dir.as_deref(),
            total_size,
        )
        .await;
        id
    });
}

fn output_dir_for(app: &App, id: usize) -> PathBuf {
    app.downloads
        .get(id)
        .and_then(|dl| dl.history_id.as_ref())
        .and_then(|hid| app.history.iter().find(|e| &e.id == hid))
        .and_then(|e| e.dir.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| app.settings.output_dir.clone())
}

/// Start queued items until all active slots are occupied.
fn fill_download_queue(
    app: &mut App,
    transfers: &mut JoinSet<usize>,
    shutdown_flags: &mut HashMap<usize, Arc<AtomicBool>>,
    draining: &mut HashMap<usize, Arc<AtomicBool>>,
    queued: &mut Vec<usize>,
) {
    while shutdown_flags.len() + draining.len() < MAX_ACTIVE_DOWNLOADS {
        let Some(pos) = queued.iter().position(|id| {
            app.downloads
                .get(*id)
                .is_some_and(|dl| dl.status == app::DownloadStatus::Queued)
        }) else {
            break;
        };
        let id = queued.remove(pos);
        if !app.activate_queued(id) {
            continue;
        }

        let has_resume = app
            .downloads
            .get(id)
            .and_then(|dl| dl.resume_info.as_ref())
            .is_some();
        if has_resume {
            spawn_resume(app, transfers, shutdown_flags, draining, id);
        } else {
            let Some((url, network, chunks)) = app
                .downloads
                .get(id)
                .map(|dl| (dl.url.clone(), dl.network.clone(), dl.chunks))
            else {
                continue;
            };
            let output_dir = output_dir_for(app, id);
            launch_new_transfer(
                app,
                transfers,
                shutdown_flags,
                draining,
                id,
                url,
                network,
                chunks,
                output_dir,
            );
        }
    }
}

/// Request a resume while respecting the old task's drain period and the
/// global active-download limit.
fn request_resume(
    app: &mut App,
    transfers: &mut JoinSet<usize>,
    shutdown_flags: &mut HashMap<usize, Arc<AtomicBool>>,
    draining: &mut HashMap<usize, Arc<AtomicBool>>,
    queued: &mut Vec<usize>,
    pending_resumes: &mut Vec<usize>,
    id: usize,
) {
    if draining.contains_key(&id) {
        if !pending_resumes.contains(&id) {
            pending_resumes.push(id);
        }
        app.add_log(&format!(
            "⏳ Waiting for task #{} to stop before resuming...",
            id
        ));
        return;
    }

    if !matches!(app.resume_download(id), Action::ResumeDownload { .. }) {
        return;
    }
    if shutdown_flags.len() + draining.len() >= MAX_ACTIVE_DOWNLOADS {
        app.queue_download(id);
        if !queued.contains(&id) {
            queued.push(id);
        }
    } else {
        spawn_resume(app, transfers, shutdown_flags, draining, id);
    }
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
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

    let unfinished = app.load_history();
    if unfinished > 0 {
        app.add_log(&format!(
            "📋 {} unfinished download(s) recorded in History",
            unfinished
        ));
    }

    let mut transfers: JoinSet<usize> = JoinSet::new();
    let mut shutdown_flags: HashMap<usize, Arc<AtomicBool>> = HashMap::new();

    // Tracks download IDs whose old task was flagged to stop but hasn't
    // exited yet. A pending resume for that ID is held here until the
    // draining flag is confirmed gone (task finished).
    let mut draining: HashMap<usize, Arc<AtomicBool>> = HashMap::new();
    // Queue of download IDs waiting to be resumed once their old task exits.
    let mut pending_resumes: Vec<usize> = Vec::new();
    // Queue of new or failed downloads waiting for an active slot.
    let mut queued: Vec<usize> = app
        .downloads
        .iter()
        .filter(|dl| dl.status == app::DownloadStatus::Queued)
        .map(|dl| dl.id)
        .collect();
    let mut automatic_retries: HashMap<usize, u8> = HashMap::new();

    app.add_log(&format!("Checking Tor proxy at {}...", config.proxy));
    app.tor_connected = tor::check_tor_connection(&config.proxy).await;
    if app.tor_connected {
        app.add_log("🧅 Connected to Tor SOCKS5 proxy");
    } else {
        app.add_log("⚠ Tor proxy not available — start tor service to download");
    }

    fill_download_queue(
        &mut app,
        &mut transfers,
        &mut shutdown_flags,
        &mut draining,
        &mut queued,
    );

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let action = app.handle_key(key);
                    match action {
                        Action::StartDownload {
                            url,
                            network,
                            chunks,
                            output_dir,
                        } => spawn_transfer(
                            &mut app,
                            &mut transfers,
                            &mut shutdown_flags,
                            &mut draining,
                            &mut queued,
                            url,
                            network,
                            chunks,
                            output_dir,
                        ),
                        Action::PauseDownload { id } => {
                            app.pause_download(id);
                            if let Some(flag) = shutdown_flags.remove(&id) {
                                flag.store(true, Ordering::SeqCst);
                                // Remember this task is still draining — don't
                                // resume until it's gone.
                                draining.insert(id, flag);
                            }
                        }
                        Action::PauseAll => {
                            let ids: Vec<usize> = app
                                .downloads
                                .iter()
                                .filter(|dl| dl.status == app::DownloadStatus::InProgress)
                                .map(|dl| dl.id)
                                .collect();
                            for id in ids.iter().copied() {
                                app.pause_download(id);
                                if let Some(flag) = shutdown_flags.remove(&id) {
                                    flag.store(true, Ordering::SeqCst);
                                    draining.insert(id, flag);
                                }
                            }
                            if !ids.is_empty() {
                                app.add_log(&format!("⏸ Paused {} active download(s)", ids.len()));
                            }
                        }
                        Action::ResumeDownload { id } => {
                            request_resume(
                                &mut app,
                                &mut transfers,
                                &mut shutdown_flags,
                                &mut draining,
                                &mut queued,
                                &mut pending_resumes,
                                id,
                            );
                        }
                        Action::ResumeAll => {
                            let ids: Vec<usize> = app
                                .downloads
                                .iter()
                                .filter(|dl| dl.status == app::DownloadStatus::Paused)
                                .map(|dl| dl.id)
                                .collect();
                            for id in ids.iter().copied() {
                                request_resume(
                                    &mut app,
                                    &mut transfers,
                                    &mut shutdown_flags,
                                    &mut draining,
                                    &mut queued,
                                    &mut pending_resumes,
                                    id,
                                );
                            }
                            if !ids.is_empty() {
                                app.add_log(&format!(
                                    "▶ Resuming {} paused download(s)",
                                    ids.len()
                                ));
                            }
                        }
                        Action::RetryDownload { id } => {
                            if app.retry_download(id) {
                                automatic_retries.remove(&id);
                                queued.push(id);
                            }
                        }
                        Action::RetryAll => {
                            let ids: Vec<usize> = app
                                .downloads
                                .iter()
                                .filter(|dl| matches!(dl.status, app::DownloadStatus::Failed(_)))
                                .map(|dl| dl.id)
                                .collect();
                            for id in ids.iter().copied() {
                                if app.retry_download(id) && !queued.contains(&id) {
                                    automatic_retries.remove(&id);
                                    queued.push(id);
                                }
                            }
                            if !ids.is_empty() {
                                app.add_log(&format!(
                                    "🔁 Retrying {} failed download(s)",
                                    ids.len()
                                ));
                            }
                        }
                        Action::Quit => {
                            app.should_quit = true;
                            break;
                        }
                        Action::ShowDialog | Action::None => {}
                    }
                }
            }
        }

        app.process_progress();
        app.persist_if_due(false);

        // Drain finished tasks and free their active slots.
        loop {
            match transfers.try_join_next() {
                Some(Ok(id)) => {
                    shutdown_flags.remove(&id);
                    let was_draining = draining.remove(&id).is_some();
                    // The task may have emitted its final Failed event just
                    // before joining; process it before deciding on an auto
                    // retry. Intentional pauses are excluded by was_draining.
                    app.process_progress();
                    if !was_draining
                        && app
                            .downloads
                            .get(id)
                            .is_some_and(|dl| matches!(dl.status, app::DownloadStatus::Failed(_)))
                    {
                        let attempts = automatic_retries.entry(id).or_default();
                        if *attempts < MAX_AUTOMATIC_RETRIES && app.retry_download(id) {
                            *attempts += 1;
                            queued.push(id);
                        } else if *attempts >= MAX_AUTOMATIC_RETRIES {
                            app.add_log(&format!(
                                "❌ Download #{} failed after {} automatic retries — press R to retry",
                                id, MAX_AUTOMATIC_RETRIES
                            ));
                        }
                    }
                }
                Some(Err(e)) => {
                    app.add_log(&format!("⚠ Download task stopped unexpectedly: {}", e));
                }
                None => break,
            }
        }

        // Spawn any pending resumes whose draining task has now exited.
        pending_resumes.retain(|&id| {
            if draining.contains_key(&id) {
                true // still waiting
            } else {
                // Old task is confirmed gone — now safe to flip status and spawn.
                if matches!(app.resume_download(id), Action::ResumeDownload { .. }) {
                    if shutdown_flags.len() + draining.len() >= MAX_ACTIVE_DOWNLOADS {
                        app.queue_download(id);
                        queued.push(id);
                    } else {
                        spawn_resume(
                            &mut app,
                            &mut transfers,
                            &mut shutdown_flags,
                            &mut draining,
                            id,
                        );
                    }
                }
                false // remove from pending
            }
        });

        fill_download_queue(
            &mut app,
            &mut transfers,
            &mut shutdown_flags,
            &mut draining,
            &mut queued,
        );

        if app.should_quit || close_signal::requested() {
            break;
        }
    }

    for flag in shutdown_flags.values() {
        flag.store(true, Ordering::SeqCst);
    }
    app.shutdown_mark();

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
