#![allow(dead_code)]
#![allow(clippy::too_many_lines, clippy::similar_names)]
#![allow(clippy::redundant_pub_crate)]

pub(crate) mod agent;
pub(crate) mod app;
pub(crate) mod config;
pub(crate) mod github;
pub(crate) mod health;
pub(crate) mod state;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tracing::info;

use crate::config::{Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pr_bot=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pr-bot/config.toml")
            .to_string_lossy()
            .to_string()
    });

    let config: Config = {
        let data = std::fs::read_to_string(&config_path)
            .context(format!("Config not found at {config_path}"))?;
        toml::from_str(&data).context("Failed to parse config")?
    };

    info!(
        "pr-bot starting. bot=@{} authorized=@{}",
        config.bot_username, config.authorized_user
    );

    app::run(config).await;
    Ok(())
}
