use crate::config::{DefaultMode, StoredSettings};
use crate::downloader::{self, DownloadProgress};
use crate::history::{self, HistoryEntry};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::sync::mpsc;

pub const THREAD_CHOICES: [u32; 6] = [4, 8, 16, 32, 64, 100];
const HISTORY_SAVE_INTERVAL: Duration = Duration::from_millis(1500);

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

impl NetworkMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkMode::Tor => "tor",
            NetworkMode::Normal => "normal",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            NetworkMode::Tor => "TOR",
            NetworkMode::Normal => "NORMAL",
        }
    }
}

/// Smart link handling: `.onion` hosts go over Tor, everything else is normal.
pub fn detect_network_mode(url: &str) -> NetworkMode {
    let lower = url.to_ascii_lowercase();
    let after_scheme = lower.split("://").nth(1).unwrap_or(&lower);
    let host = after_scheme
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("");
    if host.ends_with(".onion") {
        NetworkMode::Tor
    } else {
        NetworkMode::Normal
    }
}

/// Ensure a pasted address has a scheme so reqwest accepts it.
pub fn normalize_url(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.contains("://") {
        t.to_string()
    } else {
        format!("http://{}", t)
    }
}

/// Expand a leading `~` using the supplied home directory.
pub fn expand_tilde(path: &str, home: Option<&Path>) -> PathBuf {
    let t = path.trim();
    if t == "~" {
        return home.map(Path::to_path_buf).unwrap_or_default();
    }
    if let Some(rest) = t.strip_prefix("~/").or_else(|| t.strip_prefix("~\\")) {
        let mut p = home.map(Path::to_path_buf).unwrap_or_default();
        p.push(rest);
        return p;
    }
    PathBuf::from(t)
}

/// Step through the fixed thread-count presets.
fn thread_step(current: u32, right: bool) -> u32 {
    let idx = THREAD_CHOICES
        .iter()
        .position(|&x| x == current)
        .unwrap_or_else(|| {
            THREAD_CHOICES
                .iter()
                .position(|&x| x >= current)
                .unwrap_or(THREAD_CHOICES.len() - 1)
        });
    let next = if right {
        (idx + 1).min(THREAD_CHOICES.len() - 1)
    } else {
        idx.saturating_sub(1)
    };
    THREAD_CHOICES[next]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogFocus {
    Path,
    Start,
    Always,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    Directory,
    Mode,
    Threads,
    AskEveryTime,
    Done,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Idle,
    Downloading,
    Dialog,
    Help,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    Downloads,
    History,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadStatus {
    InProgress,
    Completed,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Download {
    pub id: usize,
    pub filename: String,
    pub network: NetworkMode,
    pub chunks: usize,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub status: DownloadStatus,
    pub started_at: Instant,
    pub last_update: Instant,
    pub speed_bps: f64,
    /// Link to the persistent history entry (its `id`), if any.
    pub history_id: Option<String>,
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
    ShowDialog,
    StartDownload {
        url: String,
        network: NetworkMode,
        chunks: usize,
        output_dir: PathBuf,
    },
    Quit,
}

/// User-editable mirror of the persisted settings file.
pub struct Settings {
    pub output_dir: PathBuf,
    pub default_mode: DefaultMode,
    pub parallel_threads: u32,
    pub ask_directory: bool,
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
    pub verbose: bool,
    pub progress_rx: Option<mpsc::UnboundedReceiver<DownloadProgress>>,
    pub progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    pub download_scroll: u16,
    pub log_scroll: u16,

    // ── Save-location dialog ──
    pub dialog_url: String,
    pub dialog_network: NetworkMode,
    /// True when the mode was auto-detected from the URL (shown in the UI).
    pub dialog_mode_auto: bool,
    pub dialog_path: String,
    pub dialog_cursor: usize,
    pub dialog_focus: DialogFocus,

    // ── Settings panel ──
    pub settings: Settings,
    pub settings_field: SettingsField,
    pub settings_dir_buf: String,
    pub settings_cursor: usize,

    // ── Persistent history log ──
    pub history: Vec<HistoryEntry>,
    pub history_selected: usize,
    pub history_scroll: u16,
    history_dirty: bool,
    last_history_save: Option<Instant>,
    /// Overrides where history.json lives (tests inject a temp path).
    pub history_file: Option<PathBuf>,
    /// Skip disk writes for settings (used by tests to stay hermetic).
    pub dry_run_io: bool,
    pub unfinished_on_boot: usize,
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
            verbose,
            progress_rx: Some(rx),
            progress_tx: tx,
            download_scroll: 0,
            log_scroll: 0,
            dialog_url: String::new(),
            dialog_network: NetworkMode::Normal,
            dialog_mode_auto: true,
            dialog_path: output_dir.to_string_lossy().to_string(),
            dialog_cursor: 0,
            dialog_focus: DialogFocus::Path,
            settings: Settings {
                output_dir,
                default_mode: DefaultMode::Auto,
                parallel_threads: 16,
                ask_directory: true,
            },
            settings_field: SettingsField::Directory,
            settings_dir_buf: String::new(),
            settings_cursor: 0,
            history: Vec::new(),
            history_selected: 0,
            history_scroll: 0,
            history_dirty: false,
            last_history_save: None,
            history_file: None,
            dry_run_io: false,
            unfinished_on_boot: 0,
        }
    }

    /// Apply effective CLI/file configuration on top of the defaults.
    pub fn apply_config(
        &mut self,
        output_dir: PathBuf,
        default_mode: DefaultMode,
        parallel_threads: u32,
        ask_directory: bool,
    ) {
        self.settings = Settings {
            output_dir,
            default_mode,
            parallel_threads,
            ask_directory,
        };
    }

    // ── Logging ──────────────────────────────────────────────────

    pub fn add_log(&mut self, msg: &str) {
        self.log_messages.push(format!("{} {}", timestamp(), msg));

        if self.log_messages.len() > 200 {
            self.log_messages.remove(0);
        }
    }

    // ── Key routing ──────────────────────────────────────────────

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }

        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.open_settings();
            return Action::None;
        }

        if key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.mode = AppMode::Help;
            return Action::None;
        }

        if self.mode == AppMode::Help {
            if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
                self.restore_mode();
            }
            return Action::None;
        }

        if self.mode == AppMode::Settings {
            return self.handle_settings_key(key);
        }

        if self.mode == AppMode::Dialog {
            return self.handle_dialog_key(key);
        }

        // Global shortcuts (never fire while typing in the URL box).
        if self.focus != Focus::Input {
            match key.code {
                KeyCode::Char('h') => {
                    self.mode = AppMode::Help;
                    return Action::None;
                }
                KeyCode::Char('s') => {
                    self.open_settings();
                    return Action::None;
                }
                _ => {}
            }
        }

        match self.focus {
            Focus::Input => self.handle_input_key(key),
            Focus::Downloads => self.handle_downloads_key(key),
            Focus::History => self.handle_history_key(key),
        }
    }

    fn active_downloads(&self) -> bool {
        self.downloads
            .iter()
            .any(|d| d.status == DownloadStatus::InProgress)
    }

    fn restore_mode(&mut self) {
        self.mode = if self.active_downloads() {
            AppMode::Downloading
        } else {
            AppMode::Idle
        };
    }

    // ── Save-location dialog ─────────────────────────────────────

    fn open_dir_dialog(&mut self, url: String, network: NetworkMode, auto_detected: bool) {
        self.mode = AppMode::Dialog;
        self.dialog_url = url;
        self.dialog_network = network;
        self.dialog_mode_auto = auto_detected;
        self.dialog_path = self.settings.output_dir.to_string_lossy().to_string();
        self.dialog_cursor = self.dialog_path.len();
        self.dialog_focus = DialogFocus::Path;
    }

    fn close_dialog(&mut self) {
        self.restore_mode();
    }

    fn dialog_insert_char(&mut self, c: char) {
        let pos = self.dialog_cursor.min(self.dialog_path.len());
        let pos = if self.dialog_path.is_char_boundary(pos) {
            pos
        } else {
            move_left(&self.dialog_path, pos)
        };
        self.dialog_path.insert(pos, c);
        self.dialog_cursor = pos + c.len_utf8();
    }

    fn resolved_output_dir(&self, raw: &str) -> PathBuf {
        expand_tilde(raw, dirs::home_dir().as_deref())
    }

    fn build_start_action(&mut self) -> Action {
        let raw = self.dialog_path.trim().to_string();
        if raw.is_empty() {
            self.add_log("⚠ Save directory cannot be empty");
            return Action::None;
        }
        let output_dir = self.resolved_output_dir(&raw);
        self.add_log(&format!(
            "💾 Saving to {} [{}]",
            output_dir.display(),
            self.dialog_network.label()
        ));
        Action::StartDownload {
            url: self.dialog_url.clone(),
            network: self.dialog_network.clone(),
            chunks: self.settings.parallel_threads as usize,
            output_dir,
        }
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.close_dialog();
                Action::None
            }
            KeyCode::Tab | KeyCode::Down => {
                self.dialog_focus = match self.dialog_focus {
                    DialogFocus::Path => DialogFocus::Start,
                    DialogFocus::Start => DialogFocus::Always,
                    DialogFocus::Always => DialogFocus::Cancel,
                    DialogFocus::Cancel => DialogFocus::Path,
                };
                Action::None
            }
            KeyCode::Up => {
                self.dialog_focus = match self.dialog_focus {
                    DialogFocus::Path => DialogFocus::Cancel,
                    DialogFocus::Start => DialogFocus::Path,
                    DialogFocus::Always => DialogFocus::Start,
                    DialogFocus::Cancel => DialogFocus::Always,
                };
                Action::None
            }
            KeyCode::Left => {
                if self.dialog_focus == DialogFocus::Path {
                    self.dialog_cursor = move_left(&self.dialog_path, self.dialog_cursor);
                }
                Action::None
            }
            KeyCode::Right => {
                if self.dialog_focus == DialogFocus::Path {
                    self.dialog_cursor = move_right(&self.dialog_path, self.dialog_cursor);
                }
                Action::None
            }
            KeyCode::Home => {
                if self.dialog_focus == DialogFocus::Path {
                    self.dialog_cursor = 0;
                }
                Action::None
            }
            KeyCode::End => {
                if self.dialog_focus == DialogFocus::Path {
                    self.dialog_cursor = self.dialog_path.len();
                }
                Action::None
            }
            KeyCode::Backspace => {
                if self.dialog_focus == DialogFocus::Path && self.dialog_cursor > 0 {
                    let new_pos = move_left(&self.dialog_path, self.dialog_cursor);
                    self.dialog_path
                        .replace_range(new_pos..self.dialog_cursor.min(self.dialog_path.len()), "");
                    self.dialog_cursor = new_pos;
                }
                Action::None
            }
            KeyCode::Char(c) => {
                if self.dialog_focus == DialogFocus::Path {
                    self.dialog_insert_char(c);
                }
                Action::None
            }
            KeyCode::Enter => match self.dialog_focus {
                DialogFocus::Path | DialogFocus::Start => {
                    self.close_dialog();
                    self.build_start_action()
                }
                DialogFocus::Always => {
                    let raw = self.dialog_path.trim().to_string();
                    if raw.is_empty() {
                        self.add_log("⚠ Save directory cannot be empty");
                        return Action::None;
                    }
                    let output_dir = self.resolved_output_dir(&raw);
                    self.settings.output_dir = output_dir.clone();
                    self.settings.ask_directory = false;
                    self.save_settings();
                    self.add_log(&format!(
                        "📌 Default folder saved: {} — future downloads skip this prompt",
                        output_dir.display()
                    ));
                    self.close_dialog();
                    Action::StartDownload {
                        url: self.dialog_url.clone(),
                        network: self.dialog_network.clone(),
                        chunks: self.settings.parallel_threads as usize,
                        output_dir,
                    }
                }
                DialogFocus::Cancel => {
                    self.close_dialog();
                    Action::None
                }
            },
            _ => Action::None,
        }
    }

    // ── URL input ────────────────────────────────────────────────

    fn handle_input_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter => {
                let raw = self.input.trim().to_string();
                if raw.is_empty() {
                    return Action::None;
                }
                let url = normalize_url(&raw);
                if url.is_empty() {
                    return Action::None;
                }
                self.input.clear();
                self.cursor_position = 0;

                let (network, auto_detected) = match self.settings.default_mode {
                    DefaultMode::Auto => (detect_network_mode(&url), true),
                    DefaultMode::Tor => (NetworkMode::Tor, false),
                    DefaultMode::Normal => (NetworkMode::Normal, false),
                };

                if self.settings.ask_directory {
                    self.open_dir_dialog(url.clone(), network, auto_detected);
                    self.add_log("📁 Choose where to save, then press START");
                    return Action::ShowDialog;
                }

                // Zero-friction path: defaults applied silently.
                let output_dir = self.settings.output_dir.clone();
                self.add_log(&format!(
                    "⚡ Auto: {} · {} threads · saving to {}",
                    network.label(),
                    self.settings.parallel_threads,
                    output_dir.display()
                ));
                Action::StartDownload {
                    url,
                    network,
                    chunks: self.settings.parallel_threads as usize,
                    output_dir,
                }
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
            KeyCode::Home => {
                self.cursor_position = 0;
                Action::None
            }
            KeyCode::End => {
                self.cursor_position = self.input.len();
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

    // ── Live downloads pane ──────────────────────────────────────

    fn handle_downloads_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Tab => {
                self.focus = Focus::History;
                Action::None
            }
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
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

    // ── History pane (plain log) ─────────────────────────────────

    fn handle_history_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Tab => {
                self.focus = Focus::Input;
                Action::None
            }
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Up => {
                if self.history_selected > 0 {
                    self.history_selected -= 1;
                }
                Action::None
            }
            KeyCode::Down => {
                if !self.history.is_empty() && self.history_selected < self.history.len() - 1 {
                    self.history_selected += 1;
                }
                Action::None
            }
            KeyCode::PageUp => {
                self.history_scroll = self.history_scroll.saturating_sub(4);
                Action::None
            }
            KeyCode::PageDown => {
                self.history_scroll = self.history_scroll.saturating_add(4);
                Action::None
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                self.delete_history(self.history_selected);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn delete_history(&mut self, idx: usize) {
        if idx >= self.history.len() {
            return;
        }
        let entry = self.history.remove(idx);

        // Best-effort cleanup of any leftover scratch dir tied to this task.
        let mut dirs_to_clean: Vec<PathBuf> = vec![self.settings.output_dir.clone()];
        if let Some(d) = &entry.dir {
            dirs_to_clean.push(PathBuf::from(d));
        }
        for d in &dirs_to_clean {
            let _ = std::fs::remove_dir_all(downloader::tmp_dir_for(d, &entry.id));
        }

        if self.history_selected >= self.history.len() && self.history_selected > 0 {
            self.history_selected -= 1;
        }
        self.history_dirty = true;
        self.add_log(&format!("🗑 Removed from history: {}", entry.filename));
    }

    // ── Starting transfers ───────────────────────────────────────

    /// Register a live download and record it in the persistent history log.
    pub fn start_download(
        &mut self,
        url: &str,
        network: NetworkMode,
        chunks: usize,
        output_dir: PathBuf,
    ) -> usize {
        let id = self.downloads.len();
        let filename = downloader::extract_filename(url);

        let nid = history::new_id();
        let stamp = history::now_stamp();
        self.history.push(HistoryEntry {
            id: nid.clone(),
            url: url.to_string(),
            filename: filename.clone(),
            filepath: None,
            dir: Some(output_dir.to_string_lossy().to_string()),
            network: network.as_str().to_string(),
            total_bytes: None,
            downloaded_bytes: 0,
            status: history::ST_IN_PROGRESS.to_string(),
            error: None,
            added_at: stamp.clone(),
            updated_at: stamp,
        });
        self.history_dirty = true;
        // Flush promptly so even a very quick exit leaves the record behind.
        self.last_history_save = None;

        self.downloads.push(Download {
            id,
            filename,
            network,
            chunks,
            total_bytes: None,
            downloaded_bytes: 0,
            status: DownloadStatus::InProgress,
            started_at: Instant::now(),
            last_update: Instant::now(),
            speed_bps: 0.0,
            history_id: Some(nid),
        });
        self.selected_download = id;
        self.focus = Focus::Downloads;
        self.mode = AppMode::Downloading;
        id
    }

    /// Explicit failure for client-build errors that never reach the downloader.
    pub fn fail_live(&mut self, id: usize, err: &str) {
        if let Some(dl) = self.downloads.get_mut(id) {
            debug_assert_eq!(dl.id, id);
            dl.status = DownloadStatus::Failed(err.to_string());
            dl.speed_bps = 0.0;
        }
        self.touch_history(id, Some(history::ST_FAILED), Some(err.to_string()), None);
        self.check_mode_idle();
    }

    // ── Progress pump ────────────────────────────────────────────

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
                    self.touch_history(id, None, None, None);
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
                            dl.speed_bps = diff as f64 / elapsed;
                            dl.last_update = now;
                        }
                    }
                    self.touch_history(id, None, None, None);
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
                    self.touch_history(
                        id,
                        Some(history::ST_COMPLETED),
                        None,
                        Some(filepath.clone()),
                    );
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
                    self.touch_history(id, Some(history::ST_FAILED), Some(error.clone()), None);
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
        if !self.active_downloads()
            && self.mode != AppMode::Dialog
            && self.mode != AppMode::Help
            && self.mode != AppMode::Settings
        {
            self.mode = AppMode::Idle;
        }
    }

    /// Copy live state into the linked persistent entry (no I/O here).
    fn touch_history(
        &mut self,
        dl_idx: usize,
        status: Option<&str>,
        error: Option<String>,
        filepath: Option<PathBuf>,
    ) {
        let Some(dl) = self.downloads.get(dl_idx) else {
            return;
        };
        let hid = match &dl.history_id {
            Some(h) => h.clone(),
            None => return,
        };
        let filename = dl.filename.clone();
        let downloaded = dl.downloaded_bytes;
        let total = dl.total_bytes;

        if let Some(e) = self.history.iter_mut().find(|e| e.id == hid) {
            e.filename = filename;
            e.downloaded_bytes = downloaded;
            if total.is_some() {
                e.total_bytes = total;
            }
            if let Some(s) = status {
                e.status = s.to_string();
            }
            if error.is_some() {
                e.error = error;
            }
            if let Some(fp) = filepath {
                e.filepath = Some(fp.to_string_lossy().to_string());
            }
            e.updated_at = history::now_stamp();
            self.history_dirty = true;
        }
    }

    // ── Persistence ──────────────────────────────────────────────

    /// Load the history log and normalise stale states: anything marked
    /// running or paused when the app starts was interrupted by an exit.
    pub fn load_history(&mut self) -> usize {
        let entries = match &self.history_file {
            Some(p) => history::load_from(p),
            None => history::load(),
        };
        let mut unfinished = 0usize;
        let normalized: Vec<HistoryEntry> = entries
            .into_iter()
            .map(|mut e| {
                if e.status != history::ST_COMPLETED {
                    if e.status == history::ST_IN_PROGRESS || e.status == "paused" {
                        e.status = history::ST_FAILED.to_string();
                        if e.error.is_none() {
                            e.error = Some("interrupted by exit".to_string());
                        }
                    }
                    unfinished += 1;
                }
                e
            })
            .collect();
        self.unfinished_on_boot = unfinished;
        self.history = normalized;
        unfinished
    }

    pub fn persist_if_due(&mut self, force: bool) {
        if !self.history_dirty {
            return;
        }
        let due = force
            || match self.last_history_save {
                None => true,
                Some(t) => t.elapsed() >= HISTORY_SAVE_INTERVAL,
            };
        if !due {
            return;
        }
        let result = match &self.history_file {
            Some(p) => history::save_to(p, &self.history),
            None => history::save(&self.history),
        };
        if let Err(e) = result {
            self.add_log(&format!("⚠ Could not save history: {}", e));
        }
        self.history_dirty = false;
        self.last_history_save = Some(Instant::now());
    }

    /// Called once before exit: running transfers are marked interrupted so
    /// the log tells the truth after relaunch.
    pub fn shutdown_mark(&mut self) {
        for i in 0..self.downloads.len() {
            if self.downloads[i].status == DownloadStatus::InProgress {
                self.downloads[i].status = DownloadStatus::Failed("interrupted by exit".into());
                self.touch_history(
                    i,
                    Some(history::ST_FAILED),
                    Some("interrupted by exit".to_string()),
                    None,
                );
            }
        }
        self.persist_if_due(true);
    }

    // ── Settings panel ───────────────────────────────────────────

    pub fn open_settings(&mut self) {
        self.mode = AppMode::Settings;
        self.settings_field = SettingsField::Directory;
        self.settings_dir_buf = self.settings.output_dir.to_string_lossy().to_string();
        self.settings_cursor = self.settings_dir_buf.len();
    }

    fn close_settings(&mut self) {
        let trimmed = self.settings_dir_buf.trim().to_string();
        if !trimmed.is_empty() {
            self.settings.output_dir = self.resolved_output_dir(&trimmed);
        }
        self.save_settings();
        self.restore_mode();
    }

    pub fn save_settings(&mut self) {
        let stored = StoredSettings {
            proxy: Some(self.proxy_addr.clone()),
            output_dir: Some(self.settings.output_dir.clone()),
            verbose: Some(self.verbose),
            default_mode: Some(self.settings.default_mode.as_str().to_string()),
            parallel_threads: Some(self.settings.parallel_threads),
            ask_directory: Some(self.settings.ask_directory),
        };
        if self.dry_run_io {
            return;
        }
        match stored.save() {
            Ok(_) => self.add_log("⚙ Settings saved"),
            Err(e) => self.add_log(&format!("⚠ Could not save settings: {}", e)),
        }
    }

    fn settings_insert_char(&mut self, c: char) {
        let pos = self.settings_cursor.min(self.settings_dir_buf.len());
        let pos = if self.settings_dir_buf.is_char_boundary(pos) {
            pos
        } else {
            move_left(&self.settings_dir_buf, pos)
        };
        self.settings_dir_buf.insert(pos, c);
        self.settings_cursor = pos + c.len_utf8();
    }

    fn handle_settings_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.close_settings();
                Action::None
            }
            KeyCode::Tab | KeyCode::Down => {
                self.settings_field = match self.settings_field {
                    SettingsField::Directory => SettingsField::Mode,
                    SettingsField::Mode => SettingsField::Threads,
                    SettingsField::Threads => SettingsField::AskEveryTime,
                    SettingsField::AskEveryTime => SettingsField::Done,
                    SettingsField::Done => SettingsField::Directory,
                };
                Action::None
            }
            KeyCode::Up => {
                self.settings_field = match self.settings_field {
                    SettingsField::Directory => SettingsField::Done,
                    SettingsField::Mode => SettingsField::Directory,
                    SettingsField::Threads => SettingsField::Mode,
                    SettingsField::AskEveryTime => SettingsField::Threads,
                    SettingsField::Done => SettingsField::AskEveryTime,
                };
                Action::None
            }
            KeyCode::Left => {
                match self.settings_field {
                    SettingsField::Directory => {
                        self.settings_cursor =
                            move_left(&self.settings_dir_buf, self.settings_cursor);
                    }
                    SettingsField::Mode => {
                        self.settings.default_mode = self.settings.default_mode.prev();
                    }
                    SettingsField::Threads => {
                        self.settings.parallel_threads =
                            thread_step(self.settings.parallel_threads, false);
                    }
                    SettingsField::AskEveryTime => {
                        self.settings.ask_directory = !self.settings.ask_directory;
                    }
                    SettingsField::Done => {}
                }
                Action::None
            }
            KeyCode::Right => {
                match self.settings_field {
                    SettingsField::Directory => {
                        self.settings_cursor =
                            move_right(&self.settings_dir_buf, self.settings_cursor);
                    }
                    SettingsField::Mode => {
                        self.settings.default_mode = self.settings.default_mode.next();
                    }
                    SettingsField::Threads => {
                        self.settings.parallel_threads =
                            thread_step(self.settings.parallel_threads, true);
                    }
                    SettingsField::AskEveryTime => {
                        self.settings.ask_directory = !self.settings.ask_directory;
                    }
                    SettingsField::Done => {}
                }
                Action::None
            }
            KeyCode::Home => {
                if self.settings_field == SettingsField::Directory {
                    self.settings_cursor = 0;
                }
                Action::None
            }
            KeyCode::End => {
                if self.settings_field == SettingsField::Directory {
                    self.settings_cursor = self.settings_dir_buf.len();
                }
                Action::None
            }
            KeyCode::Backspace => {
                if self.settings_field == SettingsField::Directory && self.settings_cursor > 0 {
                    let new_pos = move_left(&self.settings_dir_buf, self.settings_cursor);
                    self.settings_dir_buf.replace_range(
                        new_pos..self.settings_cursor.min(self.settings_dir_buf.len()),
                        "",
                    );
                    self.settings_cursor = new_pos;
                }
                Action::None
            }
            KeyCode::Enter => {
                if self.settings_field == SettingsField::Done {
                    self.close_settings();
                }
                Action::None
            }
            KeyCode::Char(c) => {
                if self.settings_field == SettingsField::Directory {
                    self.settings_insert_char(c);
                }
                Action::None
            }
            _ => Action::None,
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

    fn test_app() -> App {
        let mut app = App::new(
            "socks5h://127.0.0.1:9050".into(),
            PathBuf::from("/tmp"),
            false,
        );
        app.dry_run_io = true;
        app
    }

    #[test]
    fn multibyte_backspace_does_not_panic_and_edits_correctly() {
        let mut app = test_app();
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
        let mut app = test_app();
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
        let mut app = test_app();
        type_string(&mut app, "héy");
        app.handle_key(key(KeyCode::Left)); // between é and y
        type_string(&mut app, "Z");
        assert_eq!(app.input, "héZy");
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut app = test_app();
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

    // ── Smart-link handling ──

    #[test]
    fn detects_onion_urls_as_tor() {
        assert_eq!(
            detect_network_mode("http://abcdefgh.onion/file.zip"),
            NetworkMode::Tor
        );
        assert_eq!(
            detect_network_mode("HTTPS://Sub.Host.ONION/x"),
            NetworkMode::Tor
        );
        assert_eq!(
            detect_network_mode("abcdefghij7ugm6t.onion:8080/f"),
            NetworkMode::Tor
        );
    }

    #[test]
    fn detects_regular_urls_as_normal() {
        assert_eq!(
            detect_network_mode("https://example.com/big.iso"),
            NetworkMode::Normal
        );
        assert_eq!(
            detect_network_mode("http://notonion.com/file"),
            NetworkMode::Normal
        );
        assert_eq!(
            detect_network_mode("ftp://files.example.org/x"),
            NetworkMode::Normal
        );
    }

    #[test]
    fn normalize_url_adds_scheme() {
        assert_eq!(normalize_url("  x.onion/f "), "http://x.onion/f");
        assert_eq!(normalize_url("https://a.com"), "https://a.com");
        assert_eq!(normalize_url("   "), "");
    }

    #[test]
    fn expand_tilde_handles_home() {
        let home = Path::new("/home/user");
        assert_eq!(expand_tilde("~", Some(home)), PathBuf::from("/home/user"));
        assert_eq!(
            expand_tilde("~/Downloads", Some(home)),
            PathBuf::from("/home/user/Downloads")
        );
        assert_eq!(
            expand_tilde("/abs/path", Some(home)),
            PathBuf::from("/abs/path")
        );
        assert_eq!(expand_tilde("~", None), PathBuf::from(""));
    }

    #[test]
    fn thread_steps_through_presets() {
        assert_eq!(thread_step(16, true), 32);
        assert_eq!(thread_step(16, false), 8);
        assert_eq!(thread_step(100, true), 100); // clamped at max
        assert_eq!(thread_step(4, false), 4); // clamped at min
        assert_eq!(thread_step(24, true), 64); // unknown value snaps upward
    }

    // ── Zero-friction paste flow ──

    #[test]
    fn paste_opens_dir_dialog_prefilled_and_autodetects() {
        let mut app = test_app();
        type_string(&mut app, "http://abc.onion/file.zip");
        let action = app.handle_key(key(KeyCode::Enter));

        assert!(matches!(action, Action::ShowDialog));
        assert_eq!(app.mode, AppMode::Dialog);
        assert_eq!(app.dialog_network, NetworkMode::Tor);
        assert!(app.dialog_mode_auto);
        assert_eq!(app.dialog_path, "/tmp"); // prefilled from settings

        // Confirming the path field with Enter starts the download.
        app.dialog_focus = DialogFocus::Start;
        let action = app.handle_key(key(KeyCode::Enter));
        match action {
            Action::StartDownload {
                url,
                network,
                chunks,
                output_dir,
            } => {
                assert_eq!(url, "http://abc.onion/file.zip");
                assert_eq!(network, NetworkMode::Tor);
                assert_eq!(chunks, 16);
                assert_eq!(output_dir, PathBuf::from("/tmp"));
            }
            _ => panic!("expected StartDownload"),
        }
    }

    #[test]
    fn paste_skips_prompt_when_ask_directory_disabled() {
        let mut app = test_app();
        app.settings.ask_directory = false;
        app.settings.parallel_threads = 32;
        type_string(&mut app, "https://example.com/movie.mkv");
        let action = app.handle_key(key(KeyCode::Enter));

        match action {
            Action::StartDownload {
                url,
                network,
                chunks,
                output_dir,
            } => {
                assert_eq!(url, "https://example.com/movie.mkv");
                assert_eq!(network, NetworkMode::Normal);
                assert_eq!(chunks, 32);
                assert_eq!(output_dir, PathBuf::from("/tmp"));
            }
            _ => panic!("expected immediate StartDownload"),
        }
        // Mode flips to Downloading when main spawns the transfer.
        assert!(!app.log_messages.is_empty());
    }

    // ── History integration ──

    #[test]
    fn starting_download_creates_linked_history_entry() {
        let mut app = test_app();
        let id = app.start_download(
            "http://x.onion/a.bin",
            NetworkMode::Tor,
            16,
            PathBuf::from("/tmp"),
        );

        assert_eq!(app.history.len(), 1);
        assert_eq!(
            app.downloads[id].history_id,
            Some(app.history[0].id.clone())
        );
        assert_eq!(app.history[0].status, history::ST_IN_PROGRESS);
    }

    #[test]
    fn completion_updates_history_filepath_and_status() {
        let mut app = test_app();
        let id = app.start_download(
            "http://x.onion/a.bin",
            NetworkMode::Tor,
            16,
            PathBuf::from("/tmp"),
        );

        let _ = app.progress_tx.send(DownloadProgress::Started {
            id,
            filename: "a.bin".into(),
            total_bytes: Some(100),
        });
        let _ = app.progress_tx.send(DownloadProgress::Completed {
            id,
            filepath: PathBuf::from("/tmp/a.bin"),
            total_bytes: 100,
        });
        app.process_progress();

        assert_eq!(app.history[0].status, history::ST_COMPLETED);
        assert_eq!(app.history[0].filepath.as_deref(), Some("/tmp/a.bin"));
        assert_eq!(app.history[0].downloaded_bytes, 100);
    }

    #[test]
    fn failure_records_error_in_history() {
        let mut app = test_app();
        let id = app.start_download(
            "http://x.onion/a.bin",
            NetworkMode::Tor,
            16,
            PathBuf::from("/tmp"),
        );

        let _ = app.progress_tx.send(DownloadProgress::Failed {
            id,
            error: "HTTP 403".into(),
        });
        app.process_progress();

        assert_eq!(
            app.downloads[id].status,
            DownloadStatus::Failed("HTTP 403".into())
        );
        assert_eq!(app.history[0].status, history::ST_FAILED);
        assert_eq!(app.history[0].error.as_deref(), Some("HTTP 403"));
    }

    fn make_entry(id: &str, status: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            url: format!("http://x.onion/{}.bin", id),
            filename: format!("{}.bin", id),
            filepath: None,
            dir: None,
            network: "tor".into(),
            total_bytes: Some(10),
            downloaded_bytes: 5,
            status: status.into(),
            error: None,
            added_at: "2026-08-25 00:00".into(),
            updated_at: "2026-08-25 00:00".into(),
        }
    }

    #[test]
    fn load_history_normalizes_stale_states_to_interrupted() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("odo_app_{}", history::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        let entries = vec![
            make_entry("1", history::ST_COMPLETED),
            make_entry("2", history::ST_IN_PROGRESS),
            make_entry("3", "paused"),
            make_entry("4", history::ST_FAILED),
        ];
        history::save_to(&path, &entries).unwrap();

        app.history_file = Some(path.clone());
        let unfinished = app.load_history();

        assert_eq!(unfinished, 3);
        assert_eq!(app.history[1].error.as_deref(), Some("interrupted by exit"));
        assert_eq!(app.history[2].status, history::ST_FAILED);
        assert_eq!(app.history[3].status, history::ST_FAILED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_writes_dirty_state_to_injected_file() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("odo_persist_{}", history::new_id()));
        let path = dir.join("history.json");
        app.history_file = Some(path.clone());

        let _ = app.start_download(
            "http://x.onion/a.bin",
            NetworkMode::Tor,
            16,
            PathBuf::from("/tmp"),
        );
        app.persist_if_due(true); // main loop calls this every tick
        assert!(path.exists(), "autosave should have flushed the file");
        let loaded = history::load_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].url, "http://x.onion/a.bin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_marks_running_transfers_interrupted() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("odo_shutdown_{}", history::new_id()));
        let path = dir.join("history.json");
        app.history_file = Some(path.clone());

        let id = app.start_download(
            "http://x.onion/a.bin",
            NetworkMode::Tor,
            16,
            PathBuf::from("/tmp"),
        );
        app.shutdown_mark();

        assert!(matches!(
            app.downloads[id].status,
            DownloadStatus::Failed(_)
        ));
        assert_eq!(app.history[0].status, history::ST_FAILED);
        let loaded = history::load_from(&path);
        assert_eq!(loaded[0].status, history::ST_FAILED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_modal_edits_apply_on_close() {
        let mut app = test_app();
        app.open_settings();
        assert_eq!(app.mode, AppMode::Settings);

        // Move to Mode and cycle to TOR.
        app.handle_settings_key(key(KeyCode::Down));
        app.handle_settings_key(key(KeyCode::Right));
        assert_eq!(app.settings.default_mode, DefaultMode::Tor);

        // Move to Threads and bump.
        app.handle_settings_key(key(KeyCode::Down));
        app.handle_settings_key(key(KeyCode::Right));
        assert_eq!(app.settings.parallel_threads, 32);

        // Close: restores mode.
        app.handle_settings_key(key(KeyCode::Esc));
        assert_eq!(app.mode, AppMode::Idle);
    }

    #[test]
    fn global_shortcuts_open_help_and_settings() {
        let mut app = test_app();
        app.focus = Focus::Downloads;
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.mode, AppMode::Help);
        app.handle_key(key(KeyCode::Esc));

        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.mode, AppMode::Settings);
    }

    #[test]
    fn ctrl_s_opens_settings_even_from_url_input_focus() {
        let mut app = test_app();
        assert_eq!(app.focus, Focus::Input);

        // Ctrl+S must work while the user is typing in the URL box.
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.handle_key(key);
        assert_eq!(app.mode, AppMode::Settings);
        // The 's' must NOT have been typed into the input.
        assert!(app.input.is_empty());

        // Same for Ctrl+H.
        let key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        app.handle_key(key);
        assert_eq!(app.mode, AppMode::Help);
    }

    #[test]
    fn delete_history_removes_entry_and_adjusts_selection() {
        let mut app = test_app();
        app.history = vec![
            make_entry("1", history::ST_COMPLETED),
            make_entry("2", history::ST_COMPLETED),
        ];
        app.history_selected = 1;

        app.delete_history(1);
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history_selected, 0);
    }
}
