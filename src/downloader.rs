use crate::error::OnionError;
use futures_util::StreamExt;
use percent_encoding::percent_decode_str;
use reqwest::Client;
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

#[allow(clippy::too_many_arguments)]
async fn download_chunk(
    client: Client,
    url: String,
    start: u64,
    end: u64,
    chunk_idx: usize,
    temp_dir: PathBuf,
    paused: Arc<AtomicBool>,
    downloaded_total: Arc<AtomicU64>,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    download_id: usize,
    total_size: u64,
) -> Result<PathBuf, OnionError> {
    let range_header = format!("bytes={}-{}", start, end);

    let response = client
        .get(&url)
        .header("Range", range_header)
        .send()
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("Chunk {}: {}", chunk_idx, e)))?;

    if !response.status().is_success() && response.status().as_u16() != 206 {
        return Err(OnionError::DownloadFailed(format!(
            "Chunk {}: HTTP {}",
            chunk_idx,
            response.status()
        )));
    }

    let temp_path = temp_dir.join(format!("chunk_{}", chunk_idx));
    let mut temp_file = fs::File::create(&temp_path)
        .await
        .map_err(|e| OnionError::DownloadFailed(format!("Chunk {} temp file: {}", chunk_idx, e)))?;

    let mut stream = response.bytes_stream();
    let mut writer = BufWriter::new(&mut temp_file);
    let mut last_sent: u64 = downloaded_total.load(Ordering::Relaxed);

    while let Some(chunk) = stream.next().await {
        while paused.load(Ordering::Relaxed) {
            sleep(Duration::from_millis(100)).await;
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

    Ok(temp_path)
}

#[allow(clippy::too_many_arguments)]
async fn download_parallel(
    client: &Client,
    url: &str,
    output_dir: &Path,
    total_size: u64,
    filename_hint: Option<String>,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    paused: Arc<AtomicBool>,
    download_id: usize,
    max_chunks: usize,
) -> Result<(), OnionError> {
    let base_name = filename_hint.unwrap_or_else(|| extract_filename(url));

    // Reserve the final path up-front so concurrent downloads can't collide.
    let (filepath, reserved) = create_unique_file(output_dir, &base_name).await?;
    drop(reserved);

    let filename = filepath
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| base_name.clone());

    let temp_dir = output_dir.join(format!(
        ".tmp_{}_{}",
        download_id,
        filename.replace('.', "_")
    ));
    fs::create_dir_all(&temp_dir).await?;

    let optimal_chunks = ((total_size / MIN_CHUNK_SIZE).min(max_chunks as u64).max(1)) as usize;
    let chunk_size = total_size / optimal_chunks as u64;

    let _ = progress_tx.send(DownloadProgress::Started {
        id: download_id,
        filename: filename.clone(),
        total_bytes: Some(total_size),
    });
    let _ = progress_tx.send(DownloadProgress::Verbose {
        message: format!(
            "Parallel download: {} chunks x ~{} bytes",
            optimal_chunks,
            format_chunk_size(chunk_size)
        ),
    });

    let downloaded_total = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    for i in 0..optimal_chunks {
        let start = i as u64 * chunk_size;
        let end = if i == optimal_chunks - 1 {
            total_size - 1
        } else {
            (i as u64 + 1) * chunk_size - 1
        };

        let client = client.clone();
        let url = url.to_string();
        let chunk_temp_dir = temp_dir.clone();
        let chunk_paused = paused.clone();
        let chunk_total = downloaded_total.clone();
        let progress_tx = progress_tx.clone();

        handles.push(tokio::spawn(download_chunk(
            client,
            url,
            start,
            end,
            i,
            chunk_temp_dir,
            chunk_paused,
            chunk_total,
            progress_tx,
            download_id,
            total_size,
        )));
    }

    let mut temp_paths: Vec<PathBuf> = Vec::with_capacity(optimal_chunks);
    for handle in handles {
        let res = handle
            .await
            .map_err(|e| OnionError::DownloadFailed(format!("Join error: {}", e)))
            .and_then(|inner| inner);

        match res {
            Ok(temp_path) => temp_paths.push(temp_path),
            Err(e) => {
                let _ = fs::remove_dir_all(&temp_dir).await;
                remove_if_empty(&filepath).await;
                return Err(e);
            }
        }
    }

    // Assemble chunks into the final file. Bytes were already counted during
    // the network phase, so no additional Progress events are emitted here.
    let output_file_res = OpenOptions::new().write(true).open(&filepath).await;
    if let Err(e) = output_file_res {
        let _ = fs::remove_dir_all(&temp_dir).await;
        remove_if_empty(&filepath).await;
        return Err(OnionError::DownloadFailed(format!(
            "Failed to open final file: {}",
            e
        )));
    }
    let mut output_file = BufWriter::new(output_file_res.unwrap());
    let mut buffer = vec![0u8; 64 * 1024];

    for chunk_path in &temp_paths {
        let mut chunk_file = match fs::File::open(chunk_path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = fs::remove_dir_all(&temp_dir).await;
                remove_if_empty(&filepath).await;
                return Err(OnionError::DownloadFailed(format!("Read error: {}", e)));
            }
        };

        loop {
            while paused.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(100)).await;
            }

            match chunk_file.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = output_file.write_all(&buffer[..n]).await {
                        let _ = fs::remove_dir_all(&temp_dir).await;
                        remove_if_empty(&filepath).await;
                        return Err(OnionError::DownloadFailed(format!("Write error: {}", e)));
                    }
                }
                Err(e) => {
                    let _ = fs::remove_dir_all(&temp_dir).await;
                    remove_if_empty(&filepath).await;
                    return Err(OnionError::DownloadFailed(format!("Read error: {}", e)));
                }
            }
        }
    }

    if let Err(e) = output_file.flush().await {
        let _ = fs::remove_dir_all(&temp_dir).await;
        remove_if_empty(&filepath).await;
        return Err(OnionError::DownloadFailed(format!("Flush error: {}", e)));
    }

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

async fn download_single(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    paused: Arc<AtomicBool>,
    download_id: usize,
) -> Result<(), OnionError> {
    let base_name = extract_filename(url);
    let (filepath, file) = create_unique_file(output_dir, &base_name).await?;
    let filename = filepath
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| base_name);

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| OnionError::DownloadFailed(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        drop(response);
        remove_if_empty(&filepath).await;
        return Err(OnionError::DownloadFailed(format!("HTTP {}", status)));
    }

    let total_bytes = response.content_length();
    let mut writer = BufWriter::new(file);

    let _ = progress_tx.send(DownloadProgress::Started {
        id: download_id,
        filename: filename.clone(),
        total_bytes,
    });
    let _ = progress_tx.send(DownloadProgress::Verbose {
        message: format!("Single-stream download to {}", filepath.display()),
    });

    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_sent: u64 = 0;

    while let Some(chunk) = stream.next().await {
        while paused.load(Ordering::Relaxed) {
            sleep(Duration::from_millis(100)).await;
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
        filepath,
        total_bytes: downloaded,
    });

    Ok(())
}

/// Entry point: guarantees a `Failed` event is emitted for every error path so
/// the UI never leaves a download stuck in the InProgress state.
pub async fn download_file(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: mpsc::UnboundedSender<DownloadProgress>,
    paused: Arc<AtomicBool>,
    download_id: usize,
    max_chunks: usize,
) -> Result<(), OnionError> {
    let result = run_download(
        client,
        url,
        output_dir,
        &progress_tx,
        paused,
        download_id,
        max_chunks,
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

async fn run_download(
    client: &Client,
    url: &str,
    output_dir: &Path,
    progress_tx: &mpsc::UnboundedSender<DownloadProgress>,
    paused: Arc<AtomicBool>,
    download_id: usize,
    max_chunks: usize,
) -> Result<(), OnionError> {
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
                    return download_parallel(
                        client,
                        url,
                        output_dir,
                        total_size,
                        hint,
                        progress_tx.clone(),
                        paused,
                        download_id,
                        max_chunks,
                    )
                    .await;
                }
            }
        }
    }

    download_single(
        client,
        url,
        output_dir,
        progress_tx.clone(),
        paused,
        download_id,
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
}
