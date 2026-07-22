use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::Config;

pub(crate) async fn ensure_task_dir(config: &Config, label: &str) -> Result<PathBuf> {
    let dir = config.cache_dir.join("tasks").join(label.replace('/', "-"));
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

// ─── Launch opencode ──────────────────────────────────

pub(crate) async fn launch_opencode(
    config: &Config,
    task_dir: &Path,
    context_json: &str,
    workflow_name: &str,
    label: &str,
) -> Result<bool> {
    let prompt = format!(
        "WORKFLOW: {workflow_name}\n\
         WORKFLOWS_DIR: {workflows_dir}\n\n\
         Read the workflow file in the workflows directory for instructions.\n\
         Here is the task context as JSON:\n\n{context_json}",
        workflow_name = workflow_name,
        workflows_dir = config.workflows_dir.display(),
        context_json = context_json,
    );

    info!("[{label}] launching opencode in {task_dir:?}");
    let mut cmd = Command::new(&config.opencode_bin);
    cmd.args(["run", "--dangerously-skip-permissions"])
        .arg("--dir")
        .arg(task_dir)
        .arg(&prompt)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(ref model) = config.model {
        cmd.arg("--model").arg(model);
    }

    let child = cmd.spawn().context("Failed to spawn opencode")?;
    let pid = child.id().unwrap_or(0);

    let result = tokio::time::timeout(Duration::from_secs(config.task_timeout_secs), async {
        child.wait_with_output().await.map(|o| o.status.success())
    })
    .await;

    let success = match result {
        Ok(Ok(true)) => {
            info!("[{label}] done");
            true
        }
        Ok(Ok(false)) => {
            warn!("[{label}] opencode exited non-zero");
            false
        }
        Ok(Err(e)) => {
            warn!("[{label}] opencode error: {e:#}");
            false
        }
        Err(_) => {
            warn!("[{label}] timed out, killing pid {pid}");
            let _ = Command::new("kill").arg(pid.to_string()).output().await;
            false
        }
    };
    Ok(success)
}
