use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

/// Monitors GitHub for assigned issues, mentions, and PR feedback.
#[derive(Parser)]
#[command(version)]
pub(crate) struct Cli {
    /// Path to config file
    #[arg(long, short, env = "PR_BOT_CONFIG")]
    pub(crate) config: Option<String>,
}

// ─── Config ───────────────────────────────────────────

#[derive(Debug, Deserialize, Clone, Default)]
pub(crate) struct Attribution {
    #[serde(default = "default_attribution_enabled")]
    enabled: bool,
    #[serde(default = "default_attribution_text")]
    text: String,
}

const fn default_attribution_enabled() -> bool {
    true
}

fn default_attribution_text() -> String {
    "---\n*Authored by @{author}. Powered by [PR bot](https://github.com/johnsaigle/pr-bot)*".into()
}

impl Attribution {
    fn signature(&self, authorized_user: &str) -> String {
        self.text.replace("{author}", authorized_user)
    }
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub(crate) struct Config {
    pub(crate) bot_username: String,
    pub(crate) authorized_user: String,
    #[serde(default = "default_workflows_dir")]
    pub(crate) workflows_dir: PathBuf,
    #[serde(default = "default_cache_dir")]
    pub(crate) cache_dir: PathBuf,
    #[serde(default = "default_state_file")]
    pub(crate) state_file: PathBuf,
    #[serde(default = "default_max_concurrent")]
    pub(crate) max_concurrent: usize,
    #[serde(default = "default_opencode")]
    pub(crate) opencode_bin: PathBuf,
    #[serde(default = "default_gh")]
    pub(crate) gh_bin: String,
    #[serde(default = "default_poll_interval")]
    pub(crate) poll_interval_secs: u64,
    #[serde(default = "default_health_check_interval")]
    pub(crate) health_check_interval_secs: u64,
    #[serde(default = "default_health_check_grace_period")]
    pub(crate) health_check_grace_period_secs: u64,
    pub(crate) model: Option<String>,
    #[serde(default = "default_task_timeout")]
    pub(crate) task_timeout_secs: u64,
    #[serde(default = "default_true")]
    pub(crate) poll_mentions: bool,
    #[serde(default)]
    pub(crate) attribution: Attribution,
}

fn default_workflows_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("pr-bot/workflows")
}
fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("pr-bot")
}
fn default_state_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("pr-bot/state.json")
}
const fn default_max_concurrent() -> usize {
    3
}
fn default_opencode() -> PathBuf {
    PathBuf::from("opencode")
}
fn default_gh() -> String {
    "gh".into()
}
const fn default_poll_interval() -> u64 {
    300
}
const fn default_health_check_interval() -> u64 {
    60
}
const fn default_health_check_grace_period() -> u64 {
    14 * 24 * 60 * 60
}
const fn default_task_timeout() -> u64 {
    1800
}
const fn default_true() -> bool {
    true
}

impl Config {
    pub(crate) fn attribution_context(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.attribution.enabled,
            "signature": self.attribution.signature(&self.authorized_user),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn health_check_grace_period_defaults_to_fourteen_days() {
        let config: Config = toml::from_str(
            r#"
                bot_username = "bot"
                authorized_user = "human"
            "#,
        )
        .unwrap();

        assert_eq!(config.health_check_grace_period_secs, 14 * 24 * 60 * 60);
    }
}
