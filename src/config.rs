use clap::Parser;
use serde::Deserialize;
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

#[derive(Deserialize, Debug, Default)]
pub struct FileConfig {
    pub proxy: Option<String>,
    pub output_dir: Option<PathBuf>,
    pub verbose: Option<bool>,
}

pub struct Config {
    pub proxy: String,
    pub output_dir: PathBuf,
    pub verbose: bool,
}

impl Config {
    pub fn load() -> Self {
        let cli = CliArgs::parse();
        let file_config = dirs::config_dir()
            .map(|d| d.join("oniondownoda").join("config.toml"))
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|s| toml::from_str::<FileConfig>(&s).ok())
            .unwrap_or_default();

        Config {
            proxy: cli
                .proxy
                .or(file_config.proxy)
                .unwrap_or_else(|| "socks5h://127.0.0.1:9050".to_string()),
            output_dir: cli
                .output_dir
                .or(file_config.output_dir)
                .or_else(dirs::download_dir)
                .unwrap_or_else(|| PathBuf::from("~/Downloads")),
            verbose: cli.verbose || file_config.verbose.unwrap_or(false),
        }
    }
}
