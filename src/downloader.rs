use crate::error::OnionError;
use futures_util::StreamExt;
use percent_encoding::percent_decode_str;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

const MIN_CHUNK_SIZE: u64 = 1024 * 1024;
const PROGRESS_SEND_INTERVAL: u64 = 512 * 1024;

#[derive(Debug, Clone)]
pub enum DownloadProgress {
    Started {
        id: usize,
        filename: String,
        total_bytes: Option<u64>,
    },
    Progress {
        id: usize,
        downloaded: u64,
        total: Option<u64>,
    },
    Completed {
        id: usize,
        filepath: PathBuf,
        total_bytes: u64,
    },
    Failed {
        id: usize,
        error: String,
    },
    Verbose {
        message: String,
    },
}

// ── Byte-range planning (parallel connections) ───────────────────

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ChunkSpan {
    pub start: u64,
    pub end: u64,
}

impl ChunkSpan {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// Scratch directory holding chunk parts until they are assembled.
pub fn tmp_dir_for(output_dir: &Path, task_key: &str) -> PathBuf {
    output_dir.join(format!(".odown_{}", task_key))
}

/// Split `[0, total)` into at most `max_chunks` spans of >= MIN_CHUNK_SIZE.
pub fn chunk_spans(total: u64, max_chunks: usize) -> Vec<ChunkSpan> {
    if total == 0 {
        return vec![ChunkSpan { start: 0, end: 0 }];
    }
    let n = ((total / MIN_CHUNK_SIZE).min(max_chunks as u64).max(1)) as usize;
    let chunk_size = total / n as u64;
    (0..n)
        .map(|i| {
            let start = i as u64 * chunk_size;
            let end = if i == n - 1 {
                total - 1
            } else {
                (i as u64 + 1) * chunk_size - 1
            };
            ChunkSpan { start, end }
        })
        .collect()
}

// ── Filename helpers ─────────────────────────────────────────────

pub fn extract_filename(url: &str) -> String {
    let raw = url
        .split('/')
        .next_back()
        .and_then(|s| s.split('?').next())
        .unwrap_or("");

    if raw.is_empty() {
        return "download".to_string();
    }

    let decoded = percent_decode_str(raw).decode_utf8_lossy();
    let sanitized: String = decoded
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();

    if sanitized.is_empty() {
        "download".to_string()
    } else {
        sanitized
    }
}

fn split_stem_ext(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(idx) if idx > 0 => (name[..idx].to_string(), name[idx..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// Parse a `Content-Disposition` header and extract the filename, handling both
/// plain `filename=` and RFC 5987 `filename*=` forms.
pub fn parse_content_disposition(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();

    if let Some(idx) = lower.find("filename*=") {
        let raw = header[idx + "filename*=".len()..]
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        let decoded = decode_rfc5987(raw);
        if !decoded.is_empty() {
            return Some(decoded);
        }
    }

    let idx = lower.find("filename=")?;
    let raw = header[idx + "filename=".len()..]
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    let decoded = percent_decode_str(raw).decode_utf8_lossy().to_string();
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn decode_rfc5987(raw: &str) -> String {
    // Format: charset'lang'value — we only care about the percent-encoded value.
    match raw
        .split_once('\'')
        .and_then(|(_, rest)| rest.split_once('\''))
    {
        Some((_, value)) => percent_decode_str(value).decode_utf8_lossy().to_string(),
        None => percent_decode_str(raw).decode_utf8_lossy().to_string(),
    }
}

/// Atomically create a uniquely-named file in `dir`, appending " (n)" before the
/// extension on collisions. Creating the file exclusively closes the race where
/// two simultaneous downloads pick the same name.
async fn create_unique_file(dir: &Path, base: &str) -> Result<(PathBuf, fs::File), OnionError> {
    fs::create_dir_all(dir).await?;

    let (stem, ext) = split_stem_ext(base);
    for counter in 0u32.. {
        let name = if counter == 0 {
            base.to_string()
        } else {
            format!("{} ({}){}", stem, counter, ext)
        };
        let path = dir.join(&name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(OnionError::Io(e)),
        }
    }
    unreachable!()
}

/// Remove a reserved-but-unused file so failed downloads don't leave 0-byte litter.
async fn remove_if_empty(path: &Path) {
    if let Ok(meta) = fs::metadata(path).await {
        if meta.is_file() && meta.len() == 0 {
            let _ = fs::remove_file(path).await;
        }
    }
}

struct HeadInfo {
    content_length: Option<u64>,
    accept_ranges: bool,
    filename: Option<String>,
}

async fn head_info(client: &Client, url: &str) -> Option<HeadInfo> {
    let response = client.head(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let accept_ranges = response
        .headers()
        .get("accept-ranges")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase().contains("bytes"))
        .unwrap_or(false);

    let filename = response
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_disposition);

    Some(HeadInfo {
        content_length: response.content_length(),
        accept_ranges,
        filename,
    })
}

// ── Parallel (multi-connection) download ─────────────────────────

const CHUNK_ATTEMPTS: u32 = 3;

/// Download one byte-range with automatic retry. Transient drops (very common
/// over Tor) are absorbed: each attempt restarts that chunk from its beginning.
#[allow(clippy::too_many_arguments)]
async fn download_chunk(
    client: Client,
    url: String,
    span_start: u64,
    span_end: u64,
    chunk_idx: usize,
    temp_dir: PathBuf,
    downloaded_total: Arc<AtomicU64>,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    total_size: u64,
    shutdown: Arc<AtomicBool>,
) -> Result<PathBuf, OnionError> {
    let temp_path = temp_dir.join(format!("chunk_{}", chunk_idx));

    for attempt in 1..=CHUNK_ATTEMPTS {
        if shutdown.load(Ordering::Relaxed) {
            return Err(OnionError::DownloadFailed("interrupted by exit".into()));
        }
        match try_chunk_once(
            &client,
            &url,
            span_start,
            span_end,
            chunk_idx,
            &temp_path,
            &downloaded_total,
            &progress_tx,
            download_id,
            total_size,
            &shutdown,
        )
        .await
        {
            Ok(()) => return Ok(temp_path),
            // Pointless to retry: the server can't serve partial content at all.
            Err(e @ OnionError::RangesUnsupported(_)) => return Err(e),
            Err(e) => {
                if attempt == CHUNK_ATTEMPTS || shutdown.load(Ordering::Relaxed) {
                    return Err(e);
                }
                let _ = progress_tx.send(DownloadProgress::Verbose {
                    message: format!(
                        "Chunk {} attempt {}/{} failed ({}) — retrying",
                        chunk_idx, attempt, CHUNK_ATTEMPTS, e
                    ),
                });
                sleep(Duration::from_millis(1000 * u64::from(attempt))).await;
            }
        }
    }
    unreachable!()
}

/// One attempt at downloading a full chunk span.
#[allow(clippy::too_many_arguments)]
async fn try_chunk_once(
    client: &Client,
    url: &str,
    span_start: u64,
    span_end: u64,
    chunk_idx: usize,
    temp_path: &Path,
    downloaded_total: &Arc<AtomicU64>,
    progress_tx: &mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    total_size: u64,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), OnionError> {
    let mut request = client.get(url);
    request = request.header("Range", format!("bytes={}-{}", span_start, span_end));
    let response = request
        .send()
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("Chunk {}: {}", chunk_idx, e)))?;

    // Chunks after the first one can only be filled by real partial-content
    // answers. A plain 200 body starts at byte 0 of the FILE — useless here;
    // the whole task must switch to single-stream mode instead.
    let status = response.status();
    if span_start > 0 && status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(OnionError::RangesUnsupported(format!(
            "chunk {} got HTTP 416",
            chunk_idx
        )));
    }
    if !status.is_success() {
        return Err(OnionError::DownloadFailed(format!(
            "Chunk {}: HTTP {}",
            chunk_idx, status
        )));
    }
    if span_start > 0 && status.as_u16() != 206 {
        return Err(OnionError::RangesUnsupported(format!(
            "chunk {} got HTTP {} without honoring Range",
            chunk_idx, status
        )));
    }

    let file = fs::File::create(temp_path)
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("Chunk {} temp file: {}", chunk_idx, e)))?;

    let mut writer = BufWriter::new(file);
    let mut stream = response.bytes_stream();
    let mut last_sent: u64 = downloaded_total.load(Ordering::Relaxed);
    let mut interrupted = false;

    while let Some(chunk) = stream.next().await {
        if shutdown.load(Ordering::Relaxed) {
            interrupted = true;
            break;
        }
        let chunk = chunk.map_err(|e| {
            OnionError::DownloadFailed(format!("Chunk {} stream error: {}", chunk_idx, e))
        })?;
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| OnionError::DownloadFailed(format!("Chunk {} write: {}", chunk_idx, e)))?;

        let now_total =
            downloaded_total.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64;
        if now_total.saturating_sub(last_sent) >= PROGRESS_SEND_INTERVAL {
            last_sent = now_total;
            let _ = writer.flush().await;
            let _ = progress_tx.send(DownloadProgress::Progress {
                id: download_id,
                downloaded: now_total,
                total: Some(total_size),
            });
        }
    }

    writer
        .flush()
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("Chunk {} flush: {}", chunk_idx, e)))?;

    if interrupted {
        return Err(OnionError::DownloadFailed("interrupted by exit".into()));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn download_parallel(
    client: &Client,
    url: &str,
    temp_dir: PathBuf,
    output_dir: &Path,
    total_size: u64,
    filename_hint: Option<String>,
    spans: Vec<ChunkSpan>,
    shutdown: Arc<AtomicBool>,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
) -> Result<(), OnionError> {
    let base_name = filename_hint.unwrap_or_else(|| extract_filename(url));

    // Fresh scratch dir for this attempt.
    let _ = fs::remove_dir_all(&temp_dir).await;
    fs::create_dir_all(&temp_dir).await?;

    let downloaded_total = Arc::new(AtomicU64::new(0));

    let _ = progress_tx.send(DownloadProgress::Started {
        id: download_id,
        filename: base_name.clone(),
        total_bytes: Some(total_size),
    });
    let _ = progress_tx.send(DownloadProgress::Verbose {
        message: format!(
            "Parallel download: {} chunks x ~{}",
            spans.len(),
            format_chunk_size(spans[0].len())
        ),
    });

    let mut handles = Vec::new();
    for (i, span) in spans.iter().enumerate() {
        // Stagger connection opens slightly — a 16+ connection burst through a
        // fresh Tor port is a classic way to get everything disconnected.
        if i % 8 != 0 {
            sleep(Duration::from_millis(80 * u64::from(i as u16 % 8))).await;
        }

        let client = client.clone();
        let url = url.to_string();
        let chunk_temp_dir = temp_dir.clone();
        let chunk_total = downloaded_total.clone();
        let tx = progress_tx.clone();
        let chunk_shutdown = shutdown.clone();

        handles.push(tokio::spawn(download_chunk(
            client,
            url,
            span.start,
            span.end,
            i,
            chunk_temp_dir,
            chunk_total,
            tx,
            download_id,
            total_size,
            chunk_shutdown,
        )));
    }

    let mut first_err: Option<OnionError> = None;
    let mut ranges_unsupported = false;
    for handle in handles {
        match handle.await {
            Ok(Ok(_)) => {}
            Ok(Err(OnionError::RangesUnsupported(m))) => {
                ranges_unsupported = true;
                first_err.get_or_insert(OnionError::RangesUnsupported(m));
            }
            Ok(Err(e)) => {
                first_err.get_or_insert(e);
            }
            Err(e) => {
                first_err.get_or_insert(OnionError::DownloadFailed(format!("Join error: {}", e)));
            }
        }
    }

    if let Some(e) = first_err {
        if ranges_unsupported {
            // Chunked mode can never work against this server.
            let _ = fs::remove_dir_all(&temp_dir).await;
            let _ = progress_tx.send(DownloadProgress::Verbose {
                message: "Server lacks range support — switching to single stream".into(),
            });
            return Err(e);
        }
        let _ = fs::remove_dir_all(&temp_dir).await;
        return Err(e);
    }

    // All chunks complete → assemble into a uniquely-named final file.
    let (filepath, file) = create_unique_file(output_dir, &base_name).await?;
    let mut writer = BufWriter::new(file);
    let mut buffer = vec![0u8; 64 * 1024];

    let mut assemble_err: Option<OnionError> = None;
    'outer: for (i, span) in spans.iter().enumerate() {
        let mut remaining = span.len();
        let mut chunk_file = match fs::File::open(temp_dir.join(format!("chunk_{}", i))).await {
            Ok(f) => f,
            Err(e) => {
                assemble_err = Some(OnionError::DownloadFailed(format!("Read error: {}", e)));
                break;
            }
        };

        while remaining > 0 {
            if shutdown.load(Ordering::Relaxed) {
                assemble_err = Some(OnionError::DownloadFailed("interrupted by exit".into()));
                break 'outer;
            }

            let want = buffer.len().min(remaining as usize);
            match chunk_file.read(&mut buffer[..want]).await {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = writer.write_all(&buffer[..n]).await {
                        assemble_err =
                            Some(OnionError::DownloadFailed(format!("Write error: {}", e)));
                        break 'outer;
                    }
                    remaining -= n as u64;
                }
                Err(e) => {
                    assemble_err = Some(OnionError::DownloadFailed(format!("Read error: {}", e)));
                    break 'outer;
                }
            }
        }
    }

    if let Some(e) = assemble_err {
        drop(writer);
        let _ = fs::remove_file(&filepath).await;
        return Err(e);
    }

    if let Err(e) = writer.flush().await {
        drop(writer);
        let _ = fs::remove_file(&filepath).await;
        return Err(OnionError::DownloadFailed(format!("Flush error: {}", e)));
    }
    drop(writer);

    let _ = fs::remove_dir_all(&temp_dir).await;

    let _ = progress_tx.send(DownloadProgress::Completed {
        id: download_id,
        filepath,
        total_bytes: total_downloaded_counted(&downloaded_total, total_size),
    });

    Ok(())
}

// The atomic counter is authoritative when every chunk completed successfully;
// fall back to the HEAD-reported size on any discrepancy.
fn total_downloaded_counted(downloaded_total: &AtomicU64, total_size: u64) -> u64 {
    let counted = downloaded_total.load(Ordering::Relaxed);
    if counted > 0 {
        counted
    } else {
        total_size
    }
}

fn format_chunk_size(bytes: u64) -> String {
    if bytes >= MIN_CHUNK_SIZE {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

// ── Single-stream download ───────────────────────────────────────

const SINGLE_ATTEMPTS: u32 = 3;

#[allow(clippy::too_many_arguments)]
async fn download_single(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    shutdown: Arc<AtomicBool>,
) -> Result<(), OnionError> {
    let base_name = extract_filename(url);
    let (filepath, _) = create_unique_file(output_dir, &base_name).await?;
    let filename = filepath
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| base_name.clone());

    let mut last_err: Option<OnionError> = None;
    for attempt in 1..=SINGLE_ATTEMPTS {
        match try_single_once(
            client,
            url,
            &filepath,
            &filename,
            &progress_tx,
            download_id,
            &shutdown,
        )
        .await
        {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => return Err(e), // definitive failure (bad status)
            Err(e) => last_err = Some(e), // transport-level: worth retrying
        }

        if attempt < SINGLE_ATTEMPTS && !shutdown.load(Ordering::Relaxed) {
            let _ = progress_tx.send(DownloadProgress::Verbose {
                message: format!(
                    "Connection dropped (attempt {}/{}) — restarting",
                    attempt, SINGLE_ATTEMPTS
                ),
            });
            sleep(Duration::from_millis(1000 * u64::from(attempt))).await;
        }
    }

    remove_if_empty(&filepath).await;
    Err(last_err.unwrap_or_else(|| OnionError::DownloadFailed("unknown".into())))
}

/// One connection attempt over the whole file.
#[allow(clippy::too_many_arguments)]
async fn try_single_once(
    client: &Client,
    url: &str,
    filepath: &Path,
    filename: &str,
    progress_tx: &mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    shutdown: &Arc<AtomicBool>,
) -> Result<Result<(), OnionError>, OnionError> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(filepath)
        .await
        .map_err(OnionError::Io)?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("connect: {}", e)))?;

    let status = response.status();
    if !status.is_success() {
        // Definitive: retrying won't change an HTTP error code.
        return Ok(Err(OnionError::DownloadFailed(format!("HTTP {}", status))));
    }
    let total_bytes = response.content_length();

    let _ = progress_tx.send(DownloadProgress::Started {
        id: download_id,
        filename: filename.to_string(),
        total_bytes,
    });
    let _ = progress_tx.send(DownloadProgress::Verbose {
        message: format!("Single-stream download to {}", filepath.display()),
    });

    let mut writer = BufWriter::new(file);
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_sent: u64 = 0;

    while let Some(chunk) = stream.next().await {
        if shutdown.load(Ordering::Relaxed) {
            let _ = writer.flush().await;
            return Err(OnionError::DownloadFailed("interrupted by exit".into()));
        }
        let chunk =
            chunk.map_err(|e| OnionError::DownloadFailed(format!("Stream error: {}", e)))?;
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| OnionError::DownloadFailed(format!("Write error: {}", e)))?;
        downloaded += chunk.len() as u64;

        if downloaded.saturating_sub(last_sent) >= PROGRESS_SEND_INTERVAL {
            last_sent = downloaded;
            let _ = writer.flush().await;
            let _ = progress_tx.send(DownloadProgress::Progress {
                id: download_id,
                downloaded,
                total: total_bytes,
            });
        }
    }

    writer.flush().await?;
    let _ = progress_tx.send(DownloadProgress::Progress {
        id: download_id,
        downloaded,
        total: total_bytes,
    });

    let _ = progress_tx.send(DownloadProgress::Completed {
        id: download_id,
        filepath: filepath.to_path_buf(),
        total_bytes: downloaded,
    });
    Ok(Ok(()))
}

// ── Entry point ──────────────────────────────────────────────────

/// Guarantees a `Failed` event is emitted for every error path so the UI never
/// leaves a download stuck in the InProgress state.
///
/// `task_key` names this attempt's scratch dir for multi-connection downloads.
#[allow(clippy::too_many_arguments)]
pub async fn download_file(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    max_chunks: usize,
    task_key: &str,
    shutdown: Arc<AtomicBool>,
) -> Result<(), OnionError> {
    let result = run_download(
        client,
        url,
        output_dir,
        &progress_tx,
        download_id,
        max_chunks,
        task_key,
        shutdown,
    )
    .await;

    if let Err(e) = &result {
        let _ = progress_tx.send(DownloadProgress::Failed {
            id: download_id,
            error: e.to_string(),
        });
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn run_download(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: &mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    max_chunks: usize,
    task_key: &str,
    shutdown: Arc<AtomicBool>,
) -> Result<(), OnionError> {
    let temp_dir = tmp_dir_for(output_dir, task_key);

    let head = head_info(client, url).await;

    let hint = head.as_ref().and_then(|h| h.filename.clone());
    if let Some(h) = &head {
        let _ = progress_tx.send(DownloadProgress::Verbose {
            message: format!(
                "HEAD ok: ranges={} size={} cd-name={}",
                h.accept_ranges,
                h.content_length
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
                h.filename.clone().unwrap_or_else(|| "none".into())
            ),
        });
    }

    if let Some(info) = head {
        if info.accept_ranges {
            if let Some(total_size) = info.content_length {
                if total_size >= MIN_CHUNK_SIZE {
                    let spans = chunk_spans(total_size, max_chunks);
                    let res = download_parallel(
                        client,
                        url,
                        temp_dir,
                        output_dir,
                        total_size,
                        hint,
                        spans,
                        shutdown.clone(),
                        progress_tx.clone(),
                        download_id,
                    )
                    .await;
                    return match res {
                        Err(OnionError::RangesUnsupported(m)) => {
                            fallback_single(
                                client,
                                url,
                                output_dir,
                                progress_tx,
                                download_id,
                                shutdown,
                                m,
                            )
                            .await
                        }
                        other => other,
                    };
                }
            }
        }
    }

    download_single(
        client,
        url,
        output_dir,
        progress_tx.clone(),
        download_id,
        shutdown,
    )
    .await
}

/// Server refused ranged requests mid-parallel: finish as one plain stream.
async fn fallback_single(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: &mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    shutdown: Arc<AtomicBool>,
    reason: String,
) -> Result<(), OnionError> {
    let _ = progress_tx.send(DownloadProgress::Verbose {
        message: format!("{} — switching to single-stream download", reason),
    });
    download_single(
        client,
        url,
        output_dir,
        progress_tx.clone(),
        download_id,
        shutdown,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_filename() {
        assert_eq!(extract_filename("http://x.onion/dir/file.zip"), "file.zip");
    }

    #[test]
    fn strips_query_string() {
        assert_eq!(
            extract_filename("http://x.onion/a/b.mp4?token=1&x=2"),
            "b.mp4"
        );
    }

    #[test]
    fn decodes_percent_encoding() {
        assert_eq!(
            extract_filename("http://x.onion/my%20report.pdf"),
            "my report.pdf"
        );
    }

    #[test]
    fn sanitizes_illegal_characters() {
        assert_eq!(extract_filename("http://x.onion/a%2Fb:c*.txt"), "abc.txt");
    }

    #[test]
    fn falls_back_when_no_filename() {
        assert_eq!(extract_filename("http://x.onion/"), "download");
        assert_eq!(extract_filename(""), "download");
    }

    #[test]
    fn parses_plain_content_disposition() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"report.pdf\""),
            Some("report.pdf".to_string())
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=data.bin"),
            Some("data.bin".to_string())
        );
    }

    #[test]
    fn parses_rfc5987_content_disposition() {
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''Na%C3%AFve%20file.txt"),
            Some("Naïve file.txt".to_string())
        );
    }

    #[test]
    fn prefers_filename_star_over_plain() {
        assert_eq!(
            parse_content_disposition(
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8''real.txt"
            ),
            Some("real.txt".to_string())
        );
    }

    #[test]
    fn returns_none_for_missing_filename() {
        assert_eq!(parse_content_disposition("attachment"), None);
        assert_eq!(parse_content_disposition("attachment; filename=\"\""), None);
    }

    #[test]
    fn splits_stem_and_ext() {
        assert_eq!(
            split_stem_ext("a.tar.gz"),
            ("a.tar".to_string(), ".gz".to_string())
        );
        assert_eq!(
            split_stem_ext(".hidden"),
            (".hidden".to_string(), String::new())
        );
        assert_eq!(
            split_stem_ext("noext"),
            ("noext".to_string(), String::new())
        );
    }

    #[test]
    fn chunk_spans_cover_exactly() {
        let total: u64 = 10 * 1024 * 1024;
        let spans = chunk_spans(total, 16);
        assert!(!spans.is_empty());
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans.last().unwrap().end, total - 1);
        // Contiguous coverage
        for w in spans.windows(2) {
            assert_eq!(w[1].start, w[0].end + 1);
        }
    }

    #[test]
    fn chunk_spans_small_file_is_single_chunk() {
        let spans = chunk_spans(512 * 1024, 16);
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0],
            ChunkSpan {
                start: 0,
                end: 512 * 1024 - 1
            }
        );
    }

    #[test]
    fn chunk_spans_zero_is_safe() {
        let spans = chunk_spans(0, 16);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].len(), 1);
    }

    #[test]
    fn chunk_spans_never_exceeds_max() {
        let total: u64 = 500 * 1024 * 1024;
        assert!(chunk_spans(total, 4).len() <= 4);
        assert!(chunk_spans(total, 100).len() <= 100);
    }

    #[test]
    fn tmp_dir_is_hidden_and_keyed() {
        let d = tmp_dir_for(Path::new("/out"), "key123");
        assert!(d.to_string_lossy().contains(".odown_key123"));
    }
}
