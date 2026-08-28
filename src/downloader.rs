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
        /// On-disk path being written. Critical for resume: the app records it
        /// so a paused download can continue appending to the same file.
        filepath: PathBuf,
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
    // Persist an absolute scratch path. The app may be relaunched from a
    // different working directory, so a relative output path must not resolve
    // to a different scratch directory on resume.
    let output_dir = if output_dir.is_absolute() {
        output_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(output_dir))
            .unwrap_or_else(|_| output_dir.to_path_buf())
    };
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

/// Probe resumability with a real ranged GET. Some servers (and Tor front
/// ends) omit or lie about `Accept-Ranges` on HEAD while honoring Range on GET.
async fn range_probe_info(client: &Client, url: &str) -> Option<HeadInfo> {
    let response = client
        .get(url)
        .header("Range", "bytes=0-0")
        .send()
        .await
        .ok()?;

    let filename = response
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_disposition);

    if response.status() == StatusCode::PARTIAL_CONTENT {
        let total_length = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range)
            .and_then(|(_, _, total)| total);
        return Some(HeadInfo {
            content_length: total_length,
            accept_ranges: true,
            filename,
        });
    }

    if response.status().is_success() {
        return Some(HeadInfo {
            content_length: response.content_length(),
            accept_ranges: false,
            filename,
        });
    }

    None
}

/// Parse `Content-Range: bytes START-END/TOTAL`.
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let (unit, value) = value.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let total = if total.trim() == "*" {
        None
    } else {
        Some(total.trim().parse().ok()?)
    };
    Some((start.parse().ok()?, end.parse().ok()?, total))
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
        if attempt > 1 {
            // A failed attempt may have written bytes before the transport
            // error. The retry truncates that file, so remove those bytes from
            // the shared progress count before starting over.
            if let Ok(meta) = fs::metadata(&temp_path).await {
                let old_len = meta.len();
                if old_len > 0 {
                    downloaded_total.fetch_sub(old_len, Ordering::Relaxed);
                }
            }
        }
        // On the first attempt, continue from whatever the scratch file already
        // holds (parallel resume). Later attempts restart the chunk from its
        // beginning after a connection drop.
        let resume_from = if attempt == 1 {
            fs::metadata(&temp_path).await.map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
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
            resume_from,
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
    resume_from: u64,
) -> Result<(), OnionError> {
    // Compute the exact byte range still needed: [span_start + resume_from,
    // span_end]. An already-complete chunk produces an empty range; that task
    // just returns immediately.
    let need_from = span_start.saturating_add(resume_from);
    if need_from > span_end {
        return Ok(());
    }

    let mut request = client.get(url);
    request = request.header("Range", format!("bytes={}-{}", need_from, span_end));
    let response = request
        .send()
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("Chunk {}: {}", chunk_idx, e)))?;

    // A plain 200 body starts at byte 0 of the FILE — useless here. Every
    // parallel request must be confirmed as a real partial response.
    let status = response.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
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
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(OnionError::RangesUnsupported(format!(
            "chunk {} got HTTP {} without honoring Range",
            chunk_idx, status
        )));
    }

    let Some(content_range) = response
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range)
    else {
        return Err(OnionError::RangesUnsupported(format!(
            "chunk {} returned 206 without Content-Range",
            chunk_idx
        )));
    };
    if content_range.0 != need_from || content_range.1 < need_from {
        return Err(OnionError::RangesUnsupported(format!(
            "chunk {} returned mismatched Content-Range start {}",
            chunk_idx, content_range.0
        )));
    }
    if let Some(remote_total) = content_range.2 {
        if remote_total != total_size {
            return Err(OnionError::RangesUnsupported(format!(
                "chunk {} reports remote size {}, expected {}",
                chunk_idx, remote_total, total_size
            )));
        }
    }

    // Append to the scratch file when resuming, create fresh otherwise.
    let file = if resume_from > 0 {
        fs::OpenOptions::new()
            .append(true)
            .open(temp_path)
            .await
            .map_err(|e| {
                OnionError::DownloadFailed(format!("Chunk {} temp file: {}", chunk_idx, e))
            })?
    } else {
        fs::File::create(temp_path).await.map_err(|e| {
            OnionError::DownloadFailed(format!("Chunk {} temp file: {}", chunk_idx, e))
        })?
    };

    let mut writer = BufWriter::new(file);
    let mut stream = response.bytes_stream();
    // The atomic counter was seeded with pre-existing scratch bytes upstream.
    let mut last_sent: u64 = downloaded_total.load(Ordering::Relaxed);
    let expected = span_end - need_from + 1;
    let mut written = 0u64;
    let mut interrupted = false;

    while let Some(chunk) = stream.next().await {
        if shutdown.load(Ordering::Relaxed) {
            interrupted = true;
            break;
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                let _ = writer.flush().await;
                let _ = writer.get_mut().sync_all().await;
                return Err(OnionError::DownloadFailed(format!(
                    "Chunk {} stream error: {}",
                    chunk_idx, e
                )));
            }
        };
        if written.saturating_add(chunk.len() as u64) > expected {
            return Err(OnionError::DownloadFailed(format!(
                "Chunk {} returned more data than requested",
                chunk_idx
            )));
        }
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| OnionError::DownloadFailed(format!("Chunk {} write: {}", chunk_idx, e)))?;
        written += chunk.len() as u64;

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
    writer
        .get_mut()
        .sync_all()
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("Chunk {} sync: {}", chunk_idx, e)))?;

    if interrupted {
        let now_total = downloaded_total.load(Ordering::Relaxed);
        let _ = progress_tx.send(DownloadProgress::Progress {
            id: download_id,
            downloaded: now_total,
            total: Some(total_size),
        });
        return Err(OnionError::DownloadFailed("interrupted by exit".into()));
    }

    if written != expected {
        return Err(OnionError::DownloadFailed(format!(
            "Chunk {} ended early: got {} of {} bytes",
            chunk_idx, written, expected
        )));
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
    resume: bool,
) -> Result<(), OnionError> {
    let base_name = filename_hint.unwrap_or_else(|| extract_filename(url));

    if !resume {
        // Fresh scratch dir for this attempt.
        let _ = fs::remove_dir_all(&temp_dir).await;
        fs::create_dir_all(&temp_dir).await?;
    } else {
        fs::create_dir_all(&temp_dir).await?;
    }

    // Seed the counter with bytes already sitting in the scratch files.
    let mut seeded: u64 = 0;
    if resume {
        for (i, span) in spans.iter().enumerate() {
            let chunk_path = temp_dir.join(format!("chunk_{}", i));
            if let Ok(meta) = fs::metadata(&chunk_path).await {
                // A chunk file larger than its span means corrupt scratch —
                // drop it so the chunk restarts cleanly.
                if meta.len() > span.len() {
                    let _ = fs::remove_file(&chunk_path).await;
                } else {
                    seeded += meta.len();
                }
            }
        }
    }
    let downloaded_total = Arc::new(AtomicU64::new(seeded));

    let _ = progress_tx.send(DownloadProgress::Started {
        id: download_id,
        filename: base_name.clone(),
        total_bytes: Some(total_size),
        // Parallel writes live in per-chunk scratch files; resume continues
        // from them, so the target path is only known once assembled.
        filepath: PathBuf::new(),
    });
    let _ = progress_tx.send(DownloadProgress::Verbose {
        message: if resume {
            format!(
                "Resuming parallel download: {} chunks x ~{}, {} already done",
                spans.len(),
                format_chunk_size(spans[0].len()),
                format_chunk_size(seeded)
            )
        } else {
            format!(
                "Parallel download: {} chunks x ~{}",
                spans.len(),
                format_chunk_size(spans[0].len())
            )
        },
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
            if !resume {
                // These scratch files cannot be interpreted as a single-stream
                // partial, so a fresh download may safely fall back to one
                // stream.
                let _ = fs::remove_dir_all(&temp_dir).await;
                let _ = progress_tx.send(DownloadProgress::Verbose {
                    message: "Server lacks range support — switching to single stream".into(),
                });
            } else {
                // Never destroy a paused download just because the server is
                // temporarily refusing ranges. The next resume attempt may
                // get a different route/response through Tor.
                let _ = progress_tx.send(DownloadProgress::Verbose {
                    message: "Server refused a resume range — keeping partial chunks".into(),
                });
            }
            return Err(e);
        }

        // Keep all other partial chunks. A connection failure must not erase
        // bytes that completed successfully on the other connections.
        if shutdown.load(Ordering::Relaxed) {
            let mut persisted = 0u64;
            for (i, span) in spans.iter().enumerate() {
                if let Ok(meta) = fs::metadata(temp_dir.join(format!("chunk_{}", i))).await {
                    persisted += meta.len().min(span.len());
                }
            }
            let _ = progress_tx.send(DownloadProgress::Progress {
                id: download_id,
                downloaded: persisted,
                total: Some(total_size),
            });
        }
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
        if remaining != 0 {
            assemble_err = Some(OnionError::DownloadFailed(format!(
                "Chunk {} is incomplete: {} bytes missing",
                i, remaining
            )));
            break 'outer;
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
    if let Err(e) = writer.get_mut().sync_all().await {
        drop(writer);
        let _ = fs::remove_file(&filepath).await;
        return Err(OnionError::DownloadFailed(format!("Sync error: {}", e)));
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
        if shutdown.load(Ordering::Relaxed) {
            return Err(OnionError::DownloadFailed("interrupted by exit".into()));
        }
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
            Err(e) => {
                // Do not enter another attempt after an intentional pause:
                // each fresh attempt truncates the single-stream file.
                if shutdown.load(Ordering::Relaxed) {
                    return Err(e);
                }
                last_err = Some(e); // transport-level: worth retrying
            }
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
    if shutdown.load(Ordering::Relaxed) {
        return Err(OnionError::DownloadFailed("interrupted by exit".into()));
    }

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
        filepath: filepath.to_path_buf(),
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
            let _ = progress_tx.send(DownloadProgress::Progress {
                id: download_id,
                downloaded,
                total: total_bytes,
            });
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
    let probe = if head
        .as_ref()
        .is_some_and(|h| h.accept_ranges && h.content_length.is_some())
    {
        None
    } else {
        range_probe_info(client, url).await
    };
    // Prefer a real range probe when it proves support; otherwise retain the
    // HEAD metadata for ordinary single-stream downloads.
    let metadata = probe
        .as_ref()
        .filter(|h| h.accept_ranges && h.content_length.is_some())
        .or(head.as_ref())
        .or(probe.as_ref());

    let hint = metadata.and_then(|h| h.filename.clone());
    if let Some(h) = metadata {
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

    if let Some(info) = metadata {
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
                        false,
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

/// Entry point for resuming a paused download.
#[allow(clippy::too_many_arguments)]
pub async fn download_file_resume(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    max_chunks: usize,
    task_key: &str,
    shutdown: Arc<AtomicBool>,
    partial_path: Option<&Path>,
    temp_dir_override: Option<&Path>,
    saved_total_size: Option<u64>,
) -> Result<(), OnionError> {
    let head = head_info(client, url).await;
    let probe =
        if saved_total_size.is_none() && head.as_ref().and_then(|h| h.content_length).is_none() {
            range_probe_info(client, url).await
        } else {
            None
        };
    let metadata = head.as_ref().or(probe.as_ref());
    // Persisted size is preferred because it defines the original chunk
    // layout. If it is unavailable, use HEAD and then a real range probe.
    let total_size = saved_total_size.or_else(|| metadata.and_then(|h| h.content_length));

    // Parallel resume: if per-chunk scratch files exist for this task, continue
    // the chunked download from them.
    let temp_dir = temp_dir_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| tmp_dir_for(output_dir, task_key));
    if fs::try_exists(&temp_dir).await.unwrap_or(false) {
        if let Some(total) = total_size {
            let spans = chunk_spans(total, max_chunks);
            let mut has_scratch = false;
            for i in 0..spans.len() {
                if fs::try_exists(temp_dir.join(format!("chunk_{}", i)))
                    .await
                    .unwrap_or(false)
                {
                    has_scratch = true;
                    break;
                }
            }
            if has_scratch {
                let hint = metadata.and_then(|h| h.filename.clone());
                return download_parallel(
                    client,
                    url,
                    temp_dir,
                    output_dir,
                    total,
                    hint,
                    spans,
                    shutdown,
                    progress_tx,
                    download_id,
                    true,
                )
                .await;
            }
        }
    }

    // Single-stream resume path.
    let mut resume_offset = if let Some(path) = partial_path {
        match tokio::fs::metadata(path).await {
            Ok(meta) => meta.len(),
            // A remembered progress count is not a trustworthy file offset.
            // Without a stat-able file, restart safely instead of creating a
            // sparse file with an unverified hole.
            Err(_) => 0,
        }
    } else {
        0
    };

    if let Some(total) = total_size {
        if resume_offset == total {
            let _ = progress_tx.send(DownloadProgress::Completed {
                id: download_id,
                filepath: partial_path.unwrap_or(output_dir).to_path_buf(),
                total_bytes: total,
            });
            return Ok(());
        }
        if resume_offset > total {
            // The local partial is from a different/larger remote object. Do
            // not report it as complete and do not append to corrupted data.
            if let Some(path) = partial_path {
                if let Ok(file) = OpenOptions::new().write(true).open(path).await {
                    let _ = file.set_len(0).await;
                }
            }
            resume_offset = 0;
        }
    }

    // Do not trust the HEAD range advertisement to decide whether to resume.
    // The GET below will either return 206 and continue, or return 200 and
    // safely restart into the same path.
    download_single_resume(
        client,
        url,
        output_dir,
        progress_tx,
        download_id,
        shutdown,
        resume_offset,
        total_size,
        partial_path.map(Path::to_path_buf),
    )
    .await
}

/// Resume a single-stream download from a given byte offset, appending to the
/// exact partial file that was captured when the download was paused.
#[allow(clippy::too_many_arguments)]
async fn download_single_resume(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    shutdown: Arc<AtomicBool>,
    resume_offset: u64,
    total_size: Option<u64>,
    partial_path: Option<PathBuf>,
) -> Result<(), OnionError> {
    // Reuse the captured partial if we have one; otherwise fall back to a fresh
    // uniquely-named file in the output dir.
    let filepath = match partial_path {
        Some(p) => p,
        None => {
            let base_name = extract_filename(url);
            create_unique_file(output_dir, &base_name).await?.0
        }
    };
    if let Some(parent) = filepath.parent() {
        fs::create_dir_all(parent).await?;
    }
    let filename = filepath
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());

    let _ = progress_tx.send(DownloadProgress::Started {
        id: download_id,
        filename: filename.clone(),
        total_bytes: total_size,
        filepath: filepath.clone(),
    });

    let request = client
        .get(url)
        .header("Range", format!("bytes={}-", resume_offset));

    let response = request
        .send()
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("connect: {}", e)))?;

    let status = response.status();

    let file = if status == reqwest::StatusCode::OK {
        // Server ignored Range header; restart from byte 0 into the same file.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&filepath)
            .await
            .map_err(OnionError::Io)?
    } else if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let Some(content_range) = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range)
        else {
            return Err(OnionError::DownloadFailed(
                "Server returned 206 without Content-Range".into(),
            ));
        };
        if content_range.0 != resume_offset {
            return Err(OnionError::DownloadFailed(format!(
                "Server resumed at byte {}, requested byte {}",
                content_range.0, resume_offset
            )));
        }
        if let (Some(expected), Some(actual)) = (total_size, content_range.2) {
            if expected != actual {
                return Err(OnionError::DownloadFailed(format!(
                    "Remote size changed from {} to {} bytes",
                    expected, actual
                )));
            }
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&filepath)
            .await
            .map_err(OnionError::Io)?;
        use tokio::io::AsyncSeekExt;
        f.seek(std::io::SeekFrom::Start(resume_offset))
            .await
            .map_err(OnionError::Io)?;
        f
    } else {
        return Err(OnionError::DownloadFailed(format!(
            "Unexpected HTTP {} when resuming",
            status
        )));
    };

    let mut writer = BufWriter::new(file);
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = if status == reqwest::StatusCode::OK {
        0
    } else {
        resume_offset
    };
    let mut last_sent: u64 = downloaded;

    while let Some(chunk) = stream.next().await {
        if shutdown.load(Ordering::Relaxed) {
            let _ = writer.flush().await;
            let _ = progress_tx.send(DownloadProgress::Progress {
                id: download_id,
                downloaded,
                total: total_size,
            });
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
                total: total_size,
            });
        }
    }

    writer.flush().await?;
    writer.get_mut().sync_all().await?;
    if let Some(expected) = total_size {
        let actual = fs::metadata(&filepath).await?.len();
        if actual != expected {
            return Err(OnionError::DownloadFailed(format!(
                "Download ended at {} of {} bytes",
                actual, expected
            )));
        }
    }
    let _ = progress_tx.send(DownloadProgress::Completed {
        id: download_id,
        filepath: filepath.to_path_buf(),
        total_bytes: downloaded,
    });
    Ok(())
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
