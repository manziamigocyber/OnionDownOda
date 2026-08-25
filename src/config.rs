use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "oniondownoda",
    version,
    about = "🧅 OnionDownOda — Download files from .onion URLs via Tor"
)]
pub struct CliArgs {
    /// SOCKS5 proxy address for Tor
    #[arg(short, long)]
    pub proxy: Option<String>,

    /// Output directory for downloaded files
    #[arg(short, long)]
    pub output_dir: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,
}

/// Everything persisted in `config.toml`. Options round-trip so the in-app
/// Settings panel never destroys values it does not own.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StoredSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbose: Option<bool>,

    /// "auto" | "tor" | "normal"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_threads: Option<u32>,

    /// Prompt for a save directory on every download
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_directory: Option<bool>,
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("oniondownoda").join("config.toml"))
}

impl StoredSettings {
    pub fn load() -> Self {
        settings_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = settings_path()
            .ok_or_else(|| std::io::Error::other("could not resolve config directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self).unwrap_or_default())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultMode {
    Auto,
    Tor,
    Normal,
}

impl DefaultMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DefaultMode::Auto => "auto",
            DefaultMode::Tor => "tor",
            DefaultMode::Normal => "normal",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "tor" => DefaultMode::Tor,
            "normal" => DefaultMode::Normal,
            _ => DefaultMode::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DefaultMode::Auto => "AUTOMATIC",
            DefaultMode::Tor => "TOR ONLY",
            DefaultMode::Normal => "NORMAL ONLY",
        }
    }

    pub fn next(self) -> Self {
        match self {
            DefaultMode::Auto => DefaultMode::Tor,
            DefaultMode::Tor => DefaultMode::Normal,
            DefaultMode::Normal => DefaultMode::Auto,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            DefaultMode::Auto => DefaultMode::Normal,
            DefaultMode::Tor => DefaultMode::Auto,
            DefaultMode::Normal => DefaultMode::Tor,
        }
    }
}

pub struct Config {
    pub proxy: String,
    pub output_dir: PathBuf,
    pub verbose: bool,
    pub default_mode: DefaultMode,
    pub parallel_threads: u32,
    pub ask_directory: bool,
}

impl Config {
    pub fn load() -> Self {
        let cli = CliArgs::parse();
        let file_config = StoredSettings::load();

        Config {
            proxy: cli
                .proxy
                .or(file_config.proxy)
                .unwrap_or_else(|| "socks5h://127.0.0.1:9050".to_string()),
            output_dir: cli
                .output_dir
                .or(file_config.output_dir)
                .or_else(dirs::download_dir)
                .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
                .unwrap_or_else(|| PathBuf::from("Downloads")),
            verbose: cli.verbose || file_config.verbose.unwrap_or(false),
            default_mode: file_config
                .default_mode
                .as_deref()
                .map(DefaultMode::parse)
                .unwrap_or(DefaultMode::Auto),
            parallel_threads: file_config.parallel_threads.unwrap_or(16).clamp(1, 128),
            ask_directory: file_config.ask_directory.unwrap_or(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_roundtrip() {
        for m in [DefaultMode::Auto, DefaultMode::Tor, DefaultMode::Normal] {
            assert_eq!(DefaultMode::parse(m.as_str()), m);
        }
        assert_eq!(DefaultMode::parse("TOR"), DefaultMode::Tor);
        assert_eq!(DefaultMode::parse("garbage"), DefaultMode::Auto);
    }

    #[test]
    fn default_mode_cycles() {
        assert_eq!(DefaultMode::Auto.next(), DefaultMode::Tor);
        assert_eq!(DefaultMode::Tor.next(), DefaultMode::Normal);
        assert_eq!(DefaultMode::Normal.next(), DefaultMode::Auto);
        assert_eq!(DefaultMode::Auto.prev(), DefaultMode::Normal);
    }

    #[test]
    fn stored_settings_roundtrip_through_toml() {
        let st = StoredSettings {
            proxy: Some("socks5h://127.0.0.1:9050".into()),
            output_dir: Some(PathBuf::from("C:/tmp/dl")),
            verbose: Some(true),
            default_mode: Some("tor".into()),
            parallel_threads: Some(16),
            ask_directory: Some(false),
        };
        let s = toml::to_string_pretty(&st).unwrap();
        let back: StoredSettings = toml::from_str(&s).unwrap();
        assert_eq!(back.proxy, st.proxy);
        assert_eq!(back.output_dir, st.output_dir);
        assert_eq!(back.verbose, st.verbose);
        assert_eq!(back.default_mode, st.default_mode);
        assert_eq!(back.parallel_threads, st.parallel_threads);
        assert_eq!(back.ask_directory, st.ask_directory);
    }
}
