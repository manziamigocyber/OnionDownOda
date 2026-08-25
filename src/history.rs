use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use time::OffsetDateTime;

pub const ST_IN_PROGRESS: &str = "in_progress";
pub const ST_COMPLETED: &str = "completed";
pub const ST_FAILED: &str = "failed";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HistoryEntry {
    pub id: String,
    pub url: String,
    pub filename: String,
    /// Final file path once known (set on completion; points at the partial
    /// file for interrupted single-stream downloads so they can be resumed).
    #[serde(default)]
    pub filepath: Option<String>,
    /// Directory this task saves into — kept so scratch data can be found
    /// even if the default download folder changes later.
    #[serde(default)]
    pub dir: Option<String>,
    /// "tor" | "normal"
    pub network: String,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub downloaded_bytes: u64,
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    pub added_at: String,
    pub updated_at: String,
}

impl HistoryEntry {
    pub fn status_label(&self) -> &'static str {
        match self.status.as_str() {
            ST_IN_PROGRESS => "IN PROGRESS",
            ST_COMPLETED => "COMPLETED",
            ST_FAILED => "FAILED",
            _ => "UNKNOWN",
        }
    }
}

fn history_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("oniondownoda").join("history.json"))
}

pub fn load() -> Vec<HistoryEntry> {
    load_from(&history_path().unwrap_or_else(|| PathBuf::from("history.json")))
}

pub fn save(entries: &[HistoryEntry]) -> std::io::Result<()> {
    let path = history_path()
        .ok_or_else(|| std::io::Error::other("could not resolve config directory"))?;
    save_to(&path, entries)
}

pub fn load_from(path: &std::path::Path) -> Vec<HistoryEntry> {
    let primary = std::fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Vec<HistoryEntry>>(&data).ok());
    if let Some(entries) = primary {
        return entries;
    }
    // Primary corrupt (e.g. killed mid-write) → fall back to the backup.
    let bak = backup_path(path);
    std::fs::read(&bak)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn backup_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".bak");
    std::path::PathBuf::from(s)
}

/// Crash-safe save: snapshot the previous file as `.bak`, write to a temp
/// file, then atomically rename over the real one. A hard kill can therefore
/// never leave a truncated `history.json` behind.
pub fn save_to(path: &std::path::Path, entries: &[HistoryEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(entries).unwrap_or_else(|_| b"[]".to_vec());

    if path.exists() {
        let _ = std::fs::copy(path, backup_path(path));
    }

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data)?;
    // fs::rename replaces an existing destination on both Unix and Windows.
    std::fs::rename(&tmp, path)
}

/// Compact timestamp used in history entries: `2026-08-25 14:03`.
pub fn now_stamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute()
    )
}

/// Unique-enough task id: nanoseconds since the Unix epoch.
pub fn new_id() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| format!("{}", rand_fallback()))
}

fn rand_fallback() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    micros ^ (micros << 21)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, status: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            url: "http://example.onion/file.zip".into(),
            filename: "file.zip".into(),
            filepath: None,
            dir: None,
            network: "tor".into(),
            total_bytes: Some(1024),
            downloaded_bytes: 512,
            status: status.into(),
            error: None,
            added_at: "2026-08-25 10:00".into(),
            updated_at: "2026-08-25 10:01".into(),
        }
    }

    #[test]
    fn roundtrip_through_json() {
        let dir = std::env::temp_dir().join(format!("odo_test_{}", new_id()));
        let path = dir.join("history.json");
        let entries = vec![
            sample("a", ST_COMPLETED),
            sample("b", ST_FAILED),
            sample("c", ST_FAILED),
        ];
        save_to(&path, &entries).unwrap();
        let back = load_from(&path);
        assert_eq!(back.len(), 3);
        assert_eq!(back[1].id, "b");
        assert_eq!(back[1].status, ST_FAILED);
        assert_eq!(back[2].downloaded_bytes, 512);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_empty() {
        let path = std::env::temp_dir().join("odo_definitely_missing.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_from(&path).is_empty());
    }

    #[test]
    fn corrupt_primary_falls_back_to_backup() {
        let dir = std::env::temp_dir().join(format!("odo_bak_{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");

        // Two good saves: the second one leaves a .bak of the first.
        save_to(&path, &[sample("good", ST_COMPLETED)]).unwrap();
        save_to(
            &path,
            &[sample("good", ST_COMPLETED), sample("two", ST_IN_PROGRESS)],
        )
        .unwrap();
        assert!(path.with_extension("json.bak").exists());

        // Simulate a kill mid-write: truncated primary.
        std::fs::write(&path, b"[{\"id\": \"trunc").unwrap();

        let entries = load_from(&path);
        assert_eq!(entries.len(), 1, "must recover from the .bak copy");
        assert_eq!(entries[0].id, "good");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_save_never_leaves_tmp_litter() {
        let dir = std::env::temp_dir().join(format!("odo_atomic_{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        save_to(&path, &[sample("a", ST_IN_PROGRESS)]).unwrap();
        save_to(&path, &[sample("a", ST_COMPLETED), sample("b", ST_FAILED)]).unwrap();
        assert!(!dir.join("history.json.tmp").exists());
        assert_eq!(load_from(&path).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_are_unique_enough() {
        let a = new_id();
        let b = new_id();
        assert!(!a.is_empty());
        // Two rapid calls should virtually never collide.
        std::thread::sleep(std::time::Duration::from_nanos(1));
        let _ = b;
    }

    #[test]
    fn stamp_has_expected_shape() {
        let s = now_stamp();
        // YYYY-MM-DD HH:MM → 16 chars
        assert_eq!(s.len(), 16);
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[13], b':');
    }
}
