use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::Config;

pub(crate) fn repo_from_url(url: &str) -> String {
    let after = url.strip_prefix("https://github.com/").unwrap_or(url);
    let parts: Vec<&str> = after.splitn(3, '/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[0], parts[1])
    } else {
        after.to_string()
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct GhIssue {
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) url: String,
    #[serde(skip)]
    pub(crate) repo: String,
    #[serde(rename = "createdAt")]
    pub(crate) created_at: String,
    pub(crate) author: Option<GhAuthor>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GhPr {
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) url: String,
    #[serde(rename = "headRefOid")]
    pub(crate) head_ref_oid: String,
    #[serde(skip)]
    pub(crate) repo: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub(crate) struct GhComment {
    pub(crate) id: u64,
    pub(crate) body: String,
    #[serde(rename = "user")]
    pub(crate) author: Option<GhAuthor>,
    pub(crate) created_at: String,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct GhReview {
    pub(crate) id: u64,
    pub(crate) body: String,
    pub(crate) state: String,
    pub(crate) user: Option<GhAuthor>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct GhAuthor {
    pub(crate) login: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GhSearchItem {
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) body: Option<String>,
    #[serde(rename = "html_url")]
    pub(crate) html_url: String,
    pub(crate) user: Option<GhAuthor>,
    #[serde(rename = "pull_request")]
    pub(crate) pull_request: Option<serde_json::Value>,
}

// ─── Helpers ──────────────────────────────────────────

pub(crate) async fn run_cmd<S: AsRef<std::ffi::OsStr> + Sync>(
    cmd: &str,
    args: &[S],
) -> Result<String> {
    let args_display: Vec<&str> = args.iter().filter_map(|a| a.as_ref().to_str()).collect();
    let output = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context(format!(
            "Failed to spawn '{cmd} {}'",
            args_display.join(" ")
        ))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        if stderr.contains("rate limit") || stderr.contains("429") {
            bail!("GitHub rate limited: {stderr}");
        }
        bail!(
            "'{cmd} {}' failed (exit {:?}): {stderr}",
            args_display.join(" "),
            output.status.code()
        );
    }
    if !stderr.is_empty() {
        debug!("stderr '{cmd} {}': {stderr}", args_display.join(" "));
    }
    Ok(stdout)
}

pub(crate) fn is_authorized(author: Option<&GhAuthor>, config: &Config) -> bool {
    author.is_some_and(|a| a.login == config.authorized_user)
}

// ─── GitHub polling ───────────────────────────────────

pub(crate) async fn fetch_assigned_issues(config: &Config) -> Result<Vec<GhIssue>> {
    let assignee = format!("@{}", config.bot_username);
    let args: Vec<String> = vec![
        "issue".into(),
        "list".into(),
        "--assignee".into(),
        assignee,
        "--state".into(),
        "open".into(),
        "--search".into(),
        "is:issue".into(),
        "--json".into(),
        "number,title,body,url,createdAt,author".into(),
        "--limit".into(),
        "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() {
        return Ok(vec![]);
    }
    let mut issues: Vec<GhIssue> =
        serde_json::from_str(&stdout).context("Failed to parse gh issue list")?;
    for issue in &mut issues {
        issue.repo = repo_from_url(&issue.url);
    }
    Ok(issues)
}

pub(crate) async fn fetch_open_prs(config: &Config) -> Result<Vec<GhPr>> {
    let author = format!("@{}", config.bot_username);
    let args: Vec<String> = vec![
        "pr".into(),
        "list".into(),
        "--author".into(),
        author,
        "--state".into(),
        "open".into(),
        "--json".into(),
        "number,title,headRefOid,url".into(),
        "--limit".into(),
        "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() {
        return Ok(vec![]);
    }
    let mut prs: Vec<GhPr> = serde_json::from_str(&stdout).context("Failed to parse gh pr list")?;
    for pr in &mut prs {
        pr.repo = repo_from_url(&pr.url);
    }
    Ok(prs)
}

pub(crate) async fn fetch_bot_issues(config: &Config) -> Result<Vec<GhIssue>> {
    let author = format!("@{}", config.bot_username);
    let args: Vec<String> = vec![
        "issue".into(),
        "list".into(),
        "--author".into(),
        author,
        "--state".into(),
        "open".into(),
        "--search".into(),
        "is:issue".into(),
        "--json".into(),
        "number,title,body,url,createdAt,author".into(),
        "--limit".into(),
        "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() {
        return Ok(vec![]);
    }
    let mut issues: Vec<GhIssue> =
        serde_json::from_str(&stdout).context("Failed to parse gh issue list")?;
    for issue in &mut issues {
        issue.repo = repo_from_url(&issue.url);
    }
    Ok(issues)
}

pub(crate) async fn fetch_authorized_issues(config: &Config) -> Result<Vec<GhIssue>> {
    let author = format!("@{}", config.authorized_user);
    let args: Vec<String> = vec![
        "issue".into(),
        "list".into(),
        "--author".into(),
        author,
        "--state".into(),
        "open".into(),
        "--search".into(),
        "is:issue".into(),
        "--json".into(),
        "number,title,body,url,createdAt,author".into(),
        "--limit".into(),
        "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() {
        return Ok(vec![]);
    }
    let mut issues: Vec<GhIssue> =
        serde_json::from_str(&stdout).context("Failed to parse gh issue list")?;
    for issue in &mut issues {
        issue.repo = repo_from_url(&issue.url);
    }
    Ok(issues)
}

pub(crate) async fn fetch_authorized_prs(config: &Config) -> Result<Vec<GhPr>> {
    let author = format!("@{}", config.authorized_user);
    let args: Vec<String> = vec![
        "pr".into(),
        "list".into(),
        "--author".into(),
        author,
        "--state".into(),
        "open".into(),
        "--json".into(),
        "number,title,headRefOid,url".into(),
        "--limit".into(),
        "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() {
        return Ok(vec![]);
    }
    let mut prs: Vec<GhPr> = serde_json::from_str(&stdout).context("Failed to parse gh pr list")?;
    for pr in &mut prs {
        pr.repo = repo_from_url(&pr.url);
    }
    Ok(prs)
}

pub(crate) async fn fetch_pr_issue_comments(
    config: &Config,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<GhComment>> {
    let endpoint = format!("/repos/{repo}/issues/{pr_number}/comments");
    let args: Vec<String> = vec!["api".into(), endpoint, "--jq".into(), ".".into()];
    match run_cmd(&config.gh_bin, &args).await {
        Ok(stdout) => {
            if stdout == "[]" || stdout.is_empty() {
                return Ok(vec![]);
            }
            match serde_json::from_str::<Vec<GhComment>>(&stdout) {
                Ok(comments) => Ok(comments),
                Err(e) => {
                    warn!(
                        "[{repo}#{pr_number}] deserialize comments failed: {e:#}; raw={:.200}",
                        stdout
                    );
                    Ok(vec![])
                }
            }
        }
        Err(e) => {
            warn!("[{repo}#{pr_number}] fetch comments failed: {e:#}");
            Ok(vec![])
        }
    }
}

pub(crate) async fn fetch_pr_review_comments(
    config: &Config,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<GhComment>> {
    let endpoint = format!("/repos/{repo}/pulls/{pr_number}/comments");
    let args: Vec<String> = vec!["api".into(), endpoint, "--jq".into(), ".".into()];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout == "[]" || stdout.is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_str(&stdout).context("Failed to parse review comments")
}

pub(crate) async fn fetch_pr_reviews(
    config: &Config,
    repo: &str,
    pr_number: u64,
) -> Result<Vec<GhReview>> {
    let endpoint = format!("/repos/{repo}/pulls/{pr_number}/reviews");
    let args: Vec<String> = vec!["api".into(), endpoint, "--jq".into(), ".".into()];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout == "[]" || stdout.is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_str(&stdout).context("Failed to parse reviews")
}

pub(crate) async fn fetch_mentions(config: &Config) -> Result<Vec<GhSearchItem>> {
    let query = format!("mentions:@{} is:open", config.bot_username);
    let args: Vec<String> = vec![
        "api".into(),
        "/search/issues".into(),
        "--method".into(),
        "GET".into(),
        "-f".into(),
        format!("q={query}"),
        "-f".into(),
        "sort=updated".into(),
        "-f".into(),
        "order=desc".into(),
        "-f".into(),
        "per_page=30".into(),
        "--jq".into(),
        ".items".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout == "[]" || stdout.is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_str(&stdout).context("Failed to parse search results")
}

pub(crate) fn contains_mention(body: &str, username: &str) -> bool {
    let needle = format!("@{username}");
    let mut start = 0usize;
    while let Some(pos) = body[start..].find(&needle) {
        let abs = start + pos;
        let after = abs + needle.len();
        let boundary = body
            .get(after..)
            .and_then(|s| s.chars().next())
            .is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_');
        if boundary {
            return true;
        }
        start = abs + 1;
    }
    false
}

// ─── Reactions ────────────────────────────────────────

pub(crate) async fn add_eyes_reaction(config: &Config, repo: &str, number: u64) {
    let endpoint = format!("/repos/{repo}/issues/{number}/reactions");
    let args: Vec<String> = vec![
        "api".into(),
        "--method".into(),
        "POST".into(),
        endpoint,
        "-f".into(),
        "content=eyes".into(),
        "--silent".into(),
    ];
    match run_cmd(&config.gh_bin, &args).await {
        Ok(_) => info!("  👀 {repo}#{number}"),
        Err(e) => debug!("failed to add 👀 reaction to {repo}#{number}: {e:#}"),
    }
}
