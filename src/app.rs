use crate::downloader::{self, DownloadProgress};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use time::OffsetDateTime;
use tokio::sync::mpsc;

fn timestamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "[{:02}:{:02}:{:02}]",
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Byte index of the char boundary strictly before `pos` (or 0).
fn move_left(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Byte index of the char boundary strictly after `pos` (or s.len()).
fn move_right(s: &str, pos: usize) -> usize {
    let mut p = (pos + 1).min(s.len());
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

#[derive(Debug, Clone, PartialEq)]
pub enum NetworkMode {
    Tor,
    Normal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DialogFocus {
    Network,
    Chunks,
    Start,
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Idle,
    Downloading,
    Dialog,
    Help,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Focus {
    Input,
    Downloads,
    Help,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadStatus {
    InProgress,
    Paused,
    Completed,
    Failed(String),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Download {
    pub id: usize,
    pub filename: String,
    pub url: String,
    pub network: NetworkMode,
    pub chunks: usize,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub status: DownloadStatus,
    pub started_at: Instant,
    pub last_update: Instant,
    pub speed_bps: f64,
    pub paused: Arc<AtomicBool>,
}

impl Download {
    pub fn eta_seconds(&self) -> Option<u64> {
        if self.speed_bps > 0.0 {
            if let Some(total) = self.total_bytes {
                let remaining = total.saturating_sub(self.downloaded_bytes);
                return Some((remaining as f64 / self.speed_bps) as u64);
            }
        }
        None
    }
}

pub enum Action {
    None,
    ShowDialog(String),
    StartDownload {
        url: String,
        network: NetworkMode,
        chunks: usize,
    },
    Pause(usize),
    Resume(usize),
    Quit,
}

pub struct App {
    pub mode: AppMode,
    pub input: String,
    pub cursor_position: usize,
    pub downloads: Vec<Download>,
    pub selected_download: usize,
    pub log_messages: Vec<String>,
    pub tor_connected: bool,
    pub focus: Focus,
    pub should_quit: bool,
    pub proxy_addr: String,
    pub output_dir: PathBuf,
    pub verbose: bool,
    pub progress_rx: Option<mpsc::UnboundedReceiver<DownloadProgress>>,
    pub progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    pub download_scroll: u16,
    pub log_scroll: u16,
    pub dialog_url: String,
    pub dialog_network: NetworkMode,
    pub dialog_chunks: usize,
    pub dialog_focus: DialogFocus,
}

impl App {
    pub fn new(proxy_addr: String, output_dir: PathBuf, verbose: bool) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            mode: AppMode::Idle,
            input: String::new(),
            cursor_position: 0,
            downloads: Vec::new(),
            selected_download: 0,
            log_messages: vec!["🧅 Welcome to OnionDownOda".to_string()],
            tor_connected: false,
            focus: Focus::Input,
            should_quit: false,
            proxy_addr,
            output_dir,
            verbose,
            progress_rx: Some(rx),
            progress_tx: tx,
            download_scroll: 0,
            log_scroll: 0,
            dialog_url: String::new(),
            dialog_network: NetworkMode::Tor,
            dialog_chunks: 100,
            dialog_focus: DialogFocus::Network,
        }
    }

    pub fn add_log(&mut self, msg: &str) {
        self.log_messages.push(format!("{} {}", timestamp(), msg));

        if self.log_messages.len() > 200 {
            self.log_messages.remove(0);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }

        if self.mode == AppMode::Help {
            if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
                self.mode = if self.downloads.is_empty() {
                    AppMode::Idle
                } else {
                    AppMode::Downloading
                };
            }
            return Action::None;
        }

        if self.mode == AppMode::Dialog {
            return self.handle_dialog_key(key);
        }

        match self.focus {
            Focus::Input => self.handle_input_key(key),
            Focus::Downloads => self.handle_downloads_key(key),
            Focus::Help => self.handle_help_key(key),
        }
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.mode = if self.downloads.is_empty() {
                    AppMode::Idle
                } else {
                    AppMode::Downloading
                };
                Action::None
            }
            KeyCode::Tab | KeyCode::Down => {
                self.dialog_focus = match self.dialog_focus {
                    DialogFocus::Network => DialogFocus::Chunks,
                    DialogFocus::Chunks => DialogFocus::Start,
                    DialogFocus::Start => DialogFocus::Cancel,
                    DialogFocus::Cancel => DialogFocus::Network,
                };
                Action::None
            }
            KeyCode::Up => {
                self.dialog_focus = match self.dialog_focus {
                    DialogFocus::Network => DialogFocus::Cancel,
                    DialogFocus::Chunks => DialogFocus::Network,
                    DialogFocus::Start => DialogFocus::Chunks,
                    DialogFocus::Cancel => DialogFocus::Start,
                };
                Action::None
            }
            KeyCode::Left | KeyCode::Right => {
                match self.dialog_focus {
                    DialogFocus::Network => {
                        self.dialog_network = match self.dialog_network {
                            NetworkMode::Tor => NetworkMode::Normal,
                            NetworkMode::Normal => NetworkMode::Tor,
                        };
                    }
                    DialogFocus::Chunks => {
                        let is_right = key.code == KeyCode::Right;
                        let choices = [16, 32, 64, 100];
                        let current_idx = choices
                            .iter()
                            .position(|&x| x == self.dialog_chunks)
                            .unwrap_or(0);
                        let next_idx = if is_right {
                            (current_idx + 1) % choices.len()
                        } else {
                            if current_idx == 0 {
                                choices.len() - 1
                            } else {
                                current_idx - 1
                            }
                        };
                        self.dialog_chunks = choices[next_idx];
                    }
                    DialogFocus::Start => {
                        self.dialog_focus = DialogFocus::Cancel;
                    }
                    DialogFocus::Cancel => {
                        self.dialog_focus = DialogFocus::Start;
                    }
                }
                Action::None
            }
            KeyCode::Enter => match self.dialog_focus {
                DialogFocus::Start => {
                    self.mode = AppMode::Downloading;
                    Action::StartDownload {
                        url: self.dialog_url.clone(),
                        network: self.dialog_network.clone(),
                        chunks: self.dialog_chunks,
                    }
                }
                DialogFocus::Cancel => {
                    self.mode = if self.downloads.is_empty() {
                        AppMode::Idle
                    } else {
                        AppMode::Downloading
                    };
                    Action::None
                }
                _ => Action::None,
            },
            _ => Action::None,
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter => {
                let url = self.input.trim().to_string();
                if !url.is_empty() {
                    self.input.clear();
                    self.cursor_position = 0;
                    self.mode = AppMode::Dialog;
                    self.dialog_url = url;
                    self.dialog_network = if self.tor_connected {
                        NetworkMode::Tor
                    } else {
                        NetworkMode::Normal
                    };
                    self.dialog_chunks = 100;
                    self.dialog_focus = DialogFocus::Network;
                    return Action::ShowDialog(self.dialog_url.clone());
                }
                Action::None
            }
            KeyCode::Tab => {
                self.focus = Focus::Downloads;
                Action::None
            }
            KeyCode::Esc => Action::Quit,
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    let new_pos = move_left(&self.input, self.cursor_position);
                    self.input
                        .replace_range(new_pos..self.cursor_position.min(self.input.len()), "");
                    self.cursor_position = new_pos;
                }
                Action::None
            }
            KeyCode::Left => {
                self.cursor_position = move_left(&self.input, self.cursor_position);
                Action::None
            }
            KeyCode::Right => {
                self.cursor_position = move_right(&self.input, self.cursor_position);
                Action::None
            }
            KeyCode::Char(c) => {
                let pos = self.cursor_position.min(self.input.len());
                let pos = if self.input.is_char_boundary(pos) {
                    pos
                } else {
                    move_left(&self.input, pos)
                };
                self.input.insert(pos, c);
                self.cursor_position = pos + c.len_utf8();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_downloads_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Tab => {
                self.focus = Focus::Help;
                Action::None
            }
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char(' ') => {
                if !self.downloads.is_empty() && self.selected_download < self.downloads.len() {
                    let dl = &self.downloads[self.selected_download];
                    if dl.status == DownloadStatus::InProgress {
                        return Action::Pause(self.selected_download);
                    } else if dl.status == DownloadStatus::Paused {
                        return Action::Resume(self.selected_download);
                    }
                }
                Action::None
            }
            KeyCode::Up => {
                if self.selected_download > 0 {
                    self.selected_download -= 1;
                }
                Action::None
            }
            KeyCode::Down => {
                if !self.downloads.is_empty() && self.selected_download < self.downloads.len() - 1 {
                    self.selected_download += 1;
                }
                Action::None
            }
            KeyCode::PageUp => {
                self.log_scroll = self.log_scroll.saturating_sub(3);
                Action::None
            }
            KeyCode::PageDown => {
                self.log_scroll = self.log_scroll.saturating_add(3);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Tab => {
                self.focus = Focus::Input;
                Action::None
            }
            KeyCode::Enter => {
                self.mode = AppMode::Help;
                Action::None
            }
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            _ => Action::None,
        }
    }

    pub fn start_download(&mut self, url: &str, network: NetworkMode, chunks: usize) -> usize {
        let id = self.downloads.len();
        let filename = downloader::extract_filename(url);
        self.downloads.push(Download {
            id,
            filename,
            url: url.to_string(),
            network,
            chunks,
            total_bytes: None,
            downloaded_bytes: 0,
            status: DownloadStatus::InProgress,
            started_at: Instant::now(),
            last_update: Instant::now(),
            speed_bps: 0.0,
            paused: Arc::new(AtomicBool::new(false)),
        });
        self.selected_download = id;
        self.mode = AppMode::Downloading;
        id
    }

    pub fn process_progress(&mut self) {
        let messages: Vec<DownloadProgress> = if let Some(rx) = &mut self.progress_rx {
            let mut msgs = Vec::new();
            while let Ok(progress) = rx.try_recv() {
                msgs.push(progress);
            }
            msgs
        } else {
            return;
        };

        let now = Instant::now();

        for progress in messages {
            match progress {
                DownloadProgress::Started {
                    id,
                    filename,
                    total_bytes,
                } => {
                    let size_str = total_bytes
                        .map(format_bytes)
                        .unwrap_or_else(|| "unknown size".to_string());

                    if id < self.downloads.len() {
                        let dl = &mut self.downloads[id];
                        dl.total_bytes = total_bytes;
                        dl.filename = filename.clone();
                    }
                    self.add_log(&format!("📥 Starting: {} ({})", filename, size_str));
                }
                DownloadProgress::Progress {
                    id,
                    downloaded,
                    total,
                } => {
                    if id < self.downloads.len() {
                        let dl = &mut self.downloads[id];
                        let diff = downloaded.saturating_sub(dl.downloaded_bytes);
                        dl.downloaded_bytes = downloaded;
                        if dl.total_bytes.is_none() {
                            dl.total_bytes = total;
                        }

                        let elapsed = now.duration_since(dl.last_update).as_secs_f64();
                        if elapsed >= 0.5 {
                            // Convert speed to bps explicitly matching over interval
                            dl.speed_bps = diff as f64 / elapsed;
                            dl.last_update = now;
                        }
                    }
                }
                DownloadProgress::Completed {
                    id,
                    filepath,
                    total_bytes,
                } => {
                    if id < self.downloads.len() {
                        let dl = &mut self.downloads[id];
                        dl.downloaded_bytes = total_bytes;
                        dl.status = DownloadStatus::Completed;
                        dl.speed_bps = 0.0;
                    }
                    self.add_log(&format!(
                        "✅ Done: {} ({})",
                        filepath.display(),
                        format_bytes(total_bytes)
                    ));
                    self.check_mode_idle();
                }
                DownloadProgress::Failed { id, error } => {
                    if id < self.downloads.len() {
                        let dl = &mut self.downloads[id];
                        dl.status = DownloadStatus::Failed(error.clone());
                        dl.speed_bps = 0.0;
                    }
                    self.add_log(&format!("❌ Failed: {}", error));
                    self.check_mode_idle();
                }
                DownloadProgress::Verbose { message } => {
                    if self.verbose {
                        self.add_log(&message);
                    }
                }
            }
        }
    }

    fn check_mode_idle(&mut self) {
        let active = self
            .downloads
            .iter()
            .any(|d| d.status == DownloadStatus::InProgress || d.status == DownloadStatus::Paused);
        if !active && self.mode != AppMode::Dialog && self.mode != AppMode::Help {
            self.mode = AppMode::Idle;
        }
    }

    pub fn pause_download(&mut self, id: usize) {
        if let Some(dl) = self.downloads.get_mut(id) {
            if dl.status == DownloadStatus::InProgress {
                dl.paused.store(true, Ordering::Relaxed);
                dl.status = DownloadStatus::Paused;
                dl.speed_bps = 0.0;
                let filename = dl.filename.clone();
                self.add_log(&format!("⏸️ Paused: {}", filename));
            }
        }
    }

    pub fn resume_download(&mut self, id: usize) {
        if let Some(dl) = self.downloads.get_mut(id) {
            if dl.status == DownloadStatus::Paused {
                dl.paused.store(false, Ordering::Relaxed);
                dl.status = DownloadStatus::InProgress;
                dl.last_update = Instant::now();
                let filename = dl.filename.clone();
                self.add_log(&format!("▶️ Resumed: {}", filename));
            }
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_string(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn multibyte_backspace_does_not_panic_and_edits_correctly() {
        let mut app = App::new(
            "socks5h://127.0.0.1:9050".into(),
            PathBuf::from("/tmp"),
            false,
        );
        type_string(&mut app, "aé🧅b");
        assert_eq!(app.input, "aé🧅b");
        assert_eq!(app.cursor_position, "aé🧅b".len());

        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "aé🧅");
        assert_eq!(app.cursor_position, "aé🧅".len());

        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "aé");
        assert_eq!(app.cursor_position, "aé".len());
    }

    #[test]
    fn arrow_keys_move_by_character_not_byte() {
        let mut app = App::new(String::new(), PathBuf::from("/tmp"), false);
        type_string(&mut app, "éx");

        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.cursor_position, "é".len()); // before 'x'
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.cursor_position, 0);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.cursor_position, "é".len());
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.cursor_position, "éx".len());
    }

    #[test]
    fn insert_in_middle_of_multibyte_text() {
        let mut app = App::new(String::new(), PathBuf::from("/tmp"), false);
        type_string(&mut app, "héy");
        app.handle_key(key(KeyCode::Left)); // between é and y
        type_string(&mut app, "Z");
        assert_eq!(app.input, "héZy");
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut app = App::new(String::new(), PathBuf::from("/tmp"), false);
        type_string(&mut app, "abc");
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Left));
        }
        assert_eq!(app.cursor_position, 0);
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "abc");
    }

    #[test]
    fn verbose_messages_logged_only_when_verbose() {
        let mut quiet = App::new(String::new(), PathBuf::from("/tmp"), false);
        let mut loud = App::new(String::new(), PathBuf::from("/tmp"), true);

        let _ = quiet.progress_tx.send(DownloadProgress::Verbose {
            message: "detail".into(),
        });
        let _ = loud.progress_tx.send(DownloadProgress::Verbose {
            message: "detail".into(),
        });

        let before_quiet = quiet.log_messages.len();
        let before_loud = loud.log_messages.len();

        quiet.process_progress();
        loud.process_progress();

        assert_eq!(quiet.log_messages.len(), before_quiet);
        assert_eq!(loud.log_messages.len(), before_loud + 1);
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn move_left_right_respect_boundaries() {
        let s = "aé🧅";
        assert_eq!(move_left(s, s.len()), "aé".len());
        assert_eq!(move_left(s, 0), 0);
        assert_eq!(move_right(s, 0), 1);
        assert_eq!(move_right(s, s.len()), s.len());
    }

    #[test]
    fn timestamp_is_formatted_with_brackets() {
        let ts = timestamp();
        assert!(ts.starts_with('[') && ts.ends_with(']'));
        assert_eq!(ts.len(), 10);
    }

    #[test]
    fn pause_resume_updates_status_and_logs_once() {
        let mut app = App::new(String::new(), PathBuf::from("/tmp"), false);
        let id = app.start_download("http://x.onion/file.zip", NetworkMode::Tor, 16);

        let logs_before = app.log_messages.len();
        app.pause_download(id);
        assert_eq!(app.downloads[id].status, DownloadStatus::Paused);
        assert_eq!(app.log_messages.len(), logs_before + 1);

        let logs_before = app.log_messages.len();
        // Pausing an already-paused download must not log again.
        app.pause_download(id);
        assert_eq!(app.log_messages.len(), logs_before);

        app.resume_download(id);
        assert_eq!(app.downloads[id].status, DownloadStatus::InProgress);
    }
}
