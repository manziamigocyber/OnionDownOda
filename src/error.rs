use thiserror::Error;

#[derive(Error, Debug)]
pub enum OnionError {
    #[error("🧅 Tor proxy unavailable at {0} — is the tor service running?")]
    TorUnavailable(String),

    #[allow(dead_code)]
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Download failed: {0}")]
    DownloadFailed(String),

    #[error("Server does not support byte ranges: {0}")]
    RangesUnsupported(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[allow(dead_code)]
    #[error("Configuration error: {0}")]
    Config(String),
}

#[allow(dead_code)]
pub type Result<T> = std::result::Result<T, OnionError>;
