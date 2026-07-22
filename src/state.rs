use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct State {
    pub(crate) processed_issues: HashMap<String, String>,
    pub(crate) pr_cursors: HashMap<String, PrCursor>,
    pub(crate) processed_mentions: HashMap<String, String>,
    pub(crate) issue_cursors: HashMap<String, IssueCursor>,
    pub(crate) processed_health: HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[allow(clippy::struct_field_names)]
pub(crate) struct PrCursor {
    pub(crate) last_head_sha: String,
    pub(crate) last_comment_id: u64,
    pub(crate) last_review_id: u64,
    pub(crate) last_updated: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct IssueCursor {
    pub(crate) last_comment_id: u64,
    pub(crate) last_updated: String,
}

pub(crate) fn load_state(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

pub(crate) fn save_state(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}
