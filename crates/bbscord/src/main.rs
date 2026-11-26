//! `bbscord`: production-ready retro BBS-styled Discord terminal client.

#![forbid(unsafe_code)]

mod bridge;
mod cli;
mod config;
mod login;
mod platform;
mod runner;

use anyhow::Result;
use clap::Parser as _;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::config::{
    default_config_path, default_secrets_path, load_config, load_optional_secrets,
};
use crate::runner::{run_headless_smoke, run_interactive};

#[tokio::main]
async fn main() -> Result<()> {
    // Install the `ring` rustls CryptoProvider before any `wss://` Voice/Video Gateway TLS dial.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    // Only emit tracing logs when explicitly opted-in via `RUST_LOG` or in headless mode,
    // writing to stderr so interactive TUI PTY frames are never corrupted.
    if cli.headless_smoke || std::env::var_os("RUST_LOG").is_some() {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    }

    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let app_cfg = load_config(&config_path)?;

    if cli.headless_smoke {
        let summary = run_headless_smoke(&cli, &app_cfg).await?;
        println!(
            "bbscord {} headless-smoke PASS: frames={} viewports=16x2,80x24,512x144 themes={} active_theme=\"{}\" image_slots={}",
            bbs_core::CRATE_VERSION,
            summary.frames_rendered,
            summary.themes_discovered,
            summary.active_theme,
            summary.total_image_slots
        );
        return Ok(());
    }

    let api_secrets = if cli.mock_demo {
        config::OptionalApiSecrets::default()
    } else {
        let secrets_path = default_secrets_path();
        match load_optional_secrets(&secrets_path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(error = %err, "ignoring optional API secrets file");
                config::OptionalApiSecrets::default()
            }
        }
    };

    run_interactive(&cli, &config_path, app_cfg, api_secrets).await
}
