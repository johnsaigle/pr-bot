use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

// ─── Config ───────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct Config {
    bot_username: String,
    authorized_user: String,
    #[serde(default = "default_workflows_dir")]
    workflows_dir: PathBuf,
    #[serde(default = "default_cache_dir")]
    cache_dir: PathBuf,
    #[serde(default = "default_state_file")]
    state_file: PathBuf,
    #[serde(default = "default_max_concurrent")]
    max_concurrent: usize,
    #[serde(default = "default_opencode")]
    opencode_bin: PathBuf,
    #[serde(default = "default_gh")]
    gh_bin: String,
    #[serde(default = "default_poll_interval")]
    poll_interval_secs: u64,
    model: Option<String>,
    #[serde(default = "default_task_timeout")]
    task_timeout_secs: u64,
    #[serde(default = "default_true")]
    poll_mentions: bool,
}

fn default_workflows_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("pr-bot/workflows")
}
fn default_cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("pr-bot")
}
fn default_state_file() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("pr-bot/state.json")
}
const fn default_max_concurrent() -> usize { 3 }
fn default_opencode() -> PathBuf { PathBuf::from("opencode") }
fn default_gh() -> String { "gh".into() }
const fn default_poll_interval() -> u64 { 300 }
const fn default_task_timeout() -> u64 { 1800 }
fn default_true() -> bool { true }

// ─── State ────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Default)]
struct State {
    processed_issues: HashMap<String, String>,
    pr_cursors: HashMap<String, PrCursor>,
    processed_mentions: HashMap<String, String>,
    issue_cursors: HashMap<String, IssueCursor>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct PrCursor {
    last_head_sha: String,
    last_comment_id: u64,
    last_review_id: u64,
    last_updated: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct IssueCursor {
    last_comment_id: u64,
    last_updated: String,
}

// ─── GitHub JSON types ────────────────────────────────

fn repo_from_url(url: &str) -> String {
    let after = url.strip_prefix("https://github.com/").unwrap_or(url);
    let parts: Vec<&str> = after.splitn(3, '/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[0], parts[1])
    } else {
        after.to_string()
    }
}

fn deserialize_repo_from_head<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct RepoObj {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }
    Ok(RepoObj::deserialize(deserializer)?.name_with_owner)
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GhIssue {
    number: u64,
    title: String,
    body: String,
    url: String,
    #[serde(skip)]
    repo: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    author: Option<GhAuthor>,
}

#[derive(Debug, Deserialize)]
struct GhPr {
    number: u64,
    title: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "headRepository", deserialize_with = "deserialize_repo_from_head")]
    repo: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct GhComment {
    id: u64,
    body: String,
    #[serde(rename = "user")]
    author: Option<GhAuthor>,
    created_at: String,
}

#[derive(Debug, Deserialize, Clone)]
struct GhReview {
    id: u64,
    body: String,
    state: String,
    user: Option<GhAuthor>,
}

#[derive(Debug, Deserialize, Clone)]
struct GhAuthor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhSearchItem {
    number: u64,
    title: String,
    body: Option<String>,
    #[serde(rename = "html_url")]
    html_url: String,
    user: Option<GhAuthor>,
    #[serde(rename = "pull_request")]
    pull_request: Option<serde_json::Value>,
}

// ─── Helpers ──────────────────────────────────────────

async fn run_cmd<S: AsRef<std::ffi::OsStr>>(cmd: &str, args: &[S]) -> Result<String> {
    let args_display: Vec<&str> = args.iter().filter_map(|a| a.as_ref().to_str()).collect();
    let output = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context(format!("Failed to spawn '{cmd} {}'", args_display.join(" ")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        if stderr.contains("rate limit") || stderr.contains("429") {
            bail!("GitHub rate limited: {stderr}");
        }
        bail!("'{cmd} {}' failed (exit {:?}): {stderr}", args_display.join(" "), output.status.code());
    }
    if !stderr.is_empty() {
        debug!("stderr '{cmd} {}': {stderr}", args_display.join(" "));
    }
    Ok(stdout)
}

// ─── Author gate ──────────────────────────────────────

fn is_authorized(author: &Option<GhAuthor>, config: &Config) -> bool {
    author
        .as_ref()
        .map(|a| a.login == config.authorized_user)
        .unwrap_or(false)
}

// ─── GitHub polling ───────────────────────────────────

async fn fetch_assigned_issues(config: &Config) -> Result<Vec<GhIssue>> {
    let assignee = format!("@{}", config.bot_username);
    let args: Vec<String> = vec![
        "issue".into(), "list".into(),
        "--assignee".into(), assignee,
        "--state".into(), "open".into(),
        "--search".into(), "is:issue".into(),
        "--json".into(), "number,title,body,url,createdAt,author".into(),
        "--limit".into(), "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() { return Ok(vec![]); }
    let mut issues: Vec<GhIssue> = serde_json::from_str(&stdout).context("Failed to parse gh issue list")?;
    for issue in &mut issues {
        issue.repo = repo_from_url(&issue.url);
    }
    Ok(issues)
}

async fn fetch_open_prs(config: &Config) -> Result<Vec<GhPr>> {
    let author = format!("@{}", config.bot_username);
    let args: Vec<String> = vec![
        "pr".into(), "list".into(),
        "--author".into(), author,
        "--state".into(), "open".into(),
        "--json".into(), "number,title,headRefOid,headRepository".into(),
        "--limit".into(), "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() { return Ok(vec![]); }
    serde_json::from_str(&stdout).context("Failed to parse gh pr list")
}

async fn fetch_bot_issues(config: &Config) -> Result<Vec<GhIssue>> {
    let author = format!("@{}", config.bot_username);
    let args: Vec<String> = vec![
        "issue".into(), "list".into(),
        "--author".into(), author,
        "--state".into(), "open".into(),
        "--search".into(), "is:issue".into(),
        "--json".into(), "number,title,body,url,createdAt,author".into(),
        "--limit".into(), "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() { return Ok(vec![]); }
    let mut issues: Vec<GhIssue> = serde_json::from_str(&stdout).context("Failed to parse gh issue list")?;
    for issue in &mut issues {
        issue.repo = repo_from_url(&issue.url);
    }
    Ok(issues)
}

async fn fetch_authorized_issues(config: &Config) -> Result<Vec<GhIssue>> {
    let author = format!("@{}", config.authorized_user);
    let args: Vec<String> = vec![
        "issue".into(), "list".into(),
        "--author".into(), author,
        "--state".into(), "open".into(),
        "--search".into(), "is:issue".into(),
        "--json".into(), "number,title,body,url,createdAt,author".into(),
        "--limit".into(), "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() { return Ok(vec![]); }
    let mut issues: Vec<GhIssue> = serde_json::from_str(&stdout).context("Failed to parse gh issue list")?;
    for issue in &mut issues {
        issue.repo = repo_from_url(&issue.url);
    }
    Ok(issues)
}

async fn fetch_authorized_prs(config: &Config) -> Result<Vec<GhPr>> {
    let author = format!("@{}", config.authorized_user);
    let args: Vec<String> = vec![
        "pr".into(), "list".into(),
        "--author".into(), author,
        "--state".into(), "open".into(),
        "--json".into(), "number,title,headRefOid,headRepository".into(),
        "--limit".into(), "30".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout.is_empty() { return Ok(vec![]); }
    serde_json::from_str(&stdout).context("Failed to parse gh pr list")
}

async fn fetch_pr_issue_comments(config: &Config, repo: &str, pr_number: u64) -> Result<Vec<GhComment>> {
    let endpoint = format!("/repos/{repo}/issues/{pr_number}/comments");
    let args: Vec<String> = vec!["api".into(), endpoint, "--jq".into(), ".".into()];
    match run_cmd(&config.gh_bin, &args).await {
        Ok(stdout) => {
            if stdout == "[]" || stdout.is_empty() { return Ok(vec![]); }
            match serde_json::from_str::<Vec<GhComment>>(&stdout) {
                Ok(comments) => Ok(comments),
                Err(e) => {
                    warn!("[{repo}#{pr_number}] deserialize comments failed: {e:#}; raw={:.200}", stdout);
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

async fn fetch_pr_review_comments(config: &Config, repo: &str, pr_number: u64) -> Result<Vec<GhComment>> {
    let endpoint = format!("/repos/{repo}/pulls/{pr_number}/comments");
    let args: Vec<String> = vec!["api".into(), endpoint, "--jq".into(), ".".into()];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout == "[]" || stdout.is_empty() { return Ok(vec![]); }
    serde_json::from_str(&stdout).context("Failed to parse review comments")
}

async fn fetch_pr_reviews(config: &Config, repo: &str, pr_number: u64) -> Result<Vec<GhReview>> {
    let endpoint = format!("/repos/{repo}/pulls/{pr_number}/reviews");
    let args: Vec<String> = vec!["api".into(), endpoint, "--jq".into(), ".".into()];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout == "[]" || stdout.is_empty() { return Ok(vec![]); }
    serde_json::from_str(&stdout).context("Failed to parse reviews")
}

async fn fetch_mentions(config: &Config) -> Result<Vec<GhSearchItem>> {
    let query = format!("mentions:@{} is:open", config.bot_username);
    let args: Vec<String> = vec![
        "api".into(), "/search/issues".into(),
        "--method".into(), "GET".into(),
        "-f".into(), format!("q={query}").into(),
        "-f".into(), "sort=updated".into(),
        "-f".into(), "order=desc".into(),
        "-f".into(), "per_page=30".into(),
        "--jq".into(), ".items".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    if stdout == "[]" || stdout.is_empty() { return Ok(vec![]); }
    serde_json::from_str(&stdout).context("Failed to parse search results")
}

fn contains_mention(body: &str, username: &str) -> bool {
    let needle = format!("@{username}");
    let mut start = 0usize;
    while let Some(pos) = body[start..].find(&needle) {
        let abs = start + pos;
        let after = abs + needle.len();
        let boundary = match body.get(after..).and_then(|s| s.chars().next()) {
            None => true,
            Some(c) => !c.is_alphanumeric() && c != '-' && c != '_',
        };
        if boundary {
            return true;
        }
        start = abs + 1;
    }
    false
}

// ─── Reactions ────────────────────────────────────────

async fn add_eyes_reaction(config: &Config, repo: &str, number: u64) {
    let endpoint = format!("/repos/{repo}/issues/{number}/reactions");
    let args: Vec<String> = vec![
        "api".into(),
        "--method".into(), "POST".into(),
        endpoint,
        "-f".into(), "content=eyes".into(),
        "--silent".into(),
    ];
    match run_cmd(&config.gh_bin, &args).await {
        Ok(_) => info!("  👀 {repo}#{number}"),
        Err(e) => debug!("failed to add 👀 reaction to {repo}#{number}: {e:#}"),
    }
}

// ─── Task directory ───────────────────────────────────

async fn ensure_task_dir(config: &Config, label: &str) -> Result<PathBuf> {
    let dir = config.cache_dir.join("tasks").join(label.replace('/', "-"));
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

// ─── Launch opencode ──────────────────────────────────

async fn launch_opencode(
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
        .arg("--dir").arg(task_dir)
        .arg(&prompt)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(ref model) = config.model {
        cmd.arg("--model").arg(model);
    }

    let child = cmd.spawn().context("Failed to spawn opencode")?;
    let pid = child.id().unwrap_or(0);

    let result = tokio::time::timeout(
        Duration::from_secs(config.task_timeout_secs),
        async { child.wait_with_output().await.map(|o| o.status.success()) },
    ).await;

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

// ─── State helpers ────────────────────────────────────

fn load_state(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

// ─── Main loop ────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pr_bot=info".into()),
        )
        .init();

    let config_path = std::env::var("PR_BOT_CONFIG").unwrap_or_else(|_| {
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

    info!("pr-bot starting. bot=@{} authorized=@{}",
        config.bot_username, config.authorized_user);

    let state = Arc::new(Mutex::new(load_state(&config.state_file)));
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent));

    loop {
        info!("── poll ──");

        // ── 1. Assigned issues (author-gated) ──
        match fetch_assigned_issues(&config).await {
            Ok(issues) => {
                info!("[issues] fetched {} assigned issue(s)", issues.len());
                let new: Vec<_> = {
                    let s = state.lock().unwrap();
                    issues.into_iter().filter(|i| {
                        let key = format!("{}/issues#{}", i.repo, i.number);
                        is_authorized(&i.author, &config)
                            && !s.processed_issues.contains_key(&key)
                    }).collect()
                };

                if !new.is_empty() {
                    info!("[issues] {} new authorized issue(s)", new.len());
                }

                for issue in new {
                    let key = format!("{}/issues#{}", issue.repo, issue.number);
                    {
                        let mut s = state.lock().unwrap();
                        s.processed_issues.insert(key.clone(), Utc::now().to_rfc3339());
                    }
                    info!("  issue {repo}#{num}", repo = issue.repo, num = issue.number);

                    add_eyes_reaction(&config, &issue.repo, issue.number).await;

                    let ctx = serde_json::json!({
                        "repo": issue.repo,
                        "issue_number": issue.number,
                        "title": issue.title,
                        "body": issue.body,
                        "author": issue.author.map(|a| a.login),
                        "bot_username": config.bot_username,
                    });

                    let config = config.clone();
                    let state = Arc::clone(&state);
                    let state_file = config.state_file.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let label = format!("{}-issue-{}", ctx["repo"].as_str().unwrap(), ctx["issue_number"]);
                        let dir = match ensure_task_dir(&config, &label).await {
                            Ok(d) => d,
                            Err(e) => { error!("[{label}] task dir failed: {e:#}"); return; }
                        };
                        if let Ok(true) = launch_opencode(&config, &dir, &ctx.to_string(), "new-issue", &label).await {
                            let s = state.lock().unwrap();
                            save_state(&state_file, &s).ok();
                        } else {
                            let mut s = state.lock().unwrap();
                            s.processed_issues.remove(&key);
                            save_state(&state_file, &s).ok();
                        }
                    });
                }
            }
            Err(e) => warn!("issue fetch failed: {e:#}"),
        }

        // ── 2. Issue comments (author-gated) ──
        match fetch_bot_issues(&config).await {
            Ok(issues) => {

                for issue in issues {
                    let key = format!("{}/issues#{}", issue.repo, issue.number);
                    let max_cid = {
                        let s = state.lock().unwrap();
                        s.issue_cursors.get(&key).map(|c| c.last_comment_id).unwrap_or(0)
                    };

                    let comments = fetch_pr_issue_comments(&config, &issue.repo, issue.number).await.unwrap_or_default();
                    let new_comments: Vec<_> = comments.iter()
                        .filter(|c| c.id > max_cid && is_authorized(&c.author, &config))
                        .cloned().collect();

                    if new_comments.is_empty() {
                        continue;
                    }

                    let new_max = new_comments.iter().map(|c| c.id).max().unwrap_or(max_cid).max(max_cid);
                    {
                        let mut s = state.lock().unwrap();
                        s.issue_cursors.insert(key.clone(), IssueCursor {
                            last_comment_id: new_max,
                            last_updated: Utc::now().to_rfc3339(),
                        });
                        save_state(&config.state_file, &s).ok();
                    }

                    info!("  issue {}/{} — {} new authorized comment(s)", issue.repo, issue.number, new_comments.len());

                    let ctx = serde_json::json!({
                        "repo": issue.repo,
                        "number": issue.number,
                        "title": issue.title,
                        "author": issue.author.map(|a| a.login),
                        "type": "Issue",
                        "url": issue.url,
                        "body": new_comments.last().map(|c| c.body.as_str()).unwrap_or(""),
                        "comments": new_comments.iter().map(|c| json!({
                            "author": c.author.as_ref().map(|a| a.login.as_str()),
                            "body": c.body,
                        })).collect::<Vec<_>>(),
                    });

                    let config = config.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let label = format!("{}-issue-comment-{}", ctx["repo"].as_str().unwrap(), ctx["number"]);
                        let dir = match ensure_task_dir(&config, &label).await {
                            Ok(d) => d,
                            Err(e) => { error!("[{label}] task dir failed: {e:#}"); return; }
                        };
                        let _ = launch_opencode(&config, &dir, &ctx.to_string(), "mention", &label).await;
                    });
                }

            }
            Err(e) => warn!("issue comment fetch failed: {e:#}"),
        }

        // ── 3. PR feedback (author-gated) ──
        match fetch_open_prs(&config).await {
            Ok(prs) => {
                info!("[pr-feedback] fetched {} open PR(s)", prs.len());

                for pr in prs {
                    let pr_key = format!("{}/prs#{}", pr.repo, pr.number);
                    let cursor = {
                        let s = state.lock().unwrap();
                        s.pr_cursors.get(&pr_key).cloned()
                    };
                    if cursor.as_ref().map(|c| c.last_head_sha == pr.head_ref_oid).unwrap_or(false) {
                        continue;
                    }

                    let ic = fetch_pr_issue_comments(&config, &pr.repo, pr.number).await.unwrap_or_default();
                    let rc = fetch_pr_review_comments(&config, &pr.repo, pr.number).await.unwrap_or_default();
                    let rv = fetch_pr_reviews(&config, &pr.repo, pr.number).await.unwrap_or_default();

                    let max_cid = cursor.as_ref().map(|c| c.last_comment_id).unwrap_or(0);
                    let max_rid = cursor.as_ref().map(|c| c.last_review_id).unwrap_or(0);

                    let new_ic: Vec<_> = ic.iter()
                        .filter(|c| c.id > max_cid && is_authorized(&c.author, &config))
                        .cloned().collect();
                    let new_rc: Vec<_> = rc.iter()
                        .filter(|c| c.id > max_cid && is_authorized(&c.author, &config))
                        .cloned().collect();
                    let new_rv: Vec<_> = rv.iter()
                        .filter(|r| r.id > max_rid && is_authorized(&r.user, &config))
                        .cloned().collect();

                    if new_ic.is_empty() && new_rc.is_empty() && new_rv.is_empty() {
                        info!("[pr-feedback] skip {}/{} — {} ic, {} rc, {} rv total; {} ic, {} rc, {} rv after cursor; 0 authorized",
                            pr.repo, pr.number, ic.len(), rc.len(), rv.len(),
                            ic.iter().filter(|c| c.id > max_cid).count(),
                            rc.iter().filter(|c| c.id > max_cid).count(),
                            rv.iter().filter(|r| r.id > max_rid).count());
                        continue;
                    }

                    let new_max_comment = new_ic.iter().chain(new_rc.iter())
                        .map(|c| c.id).max().unwrap_or(max_cid).max(max_cid);
                    let new_max_review = new_rv.iter()
                        .map(|r| r.id).max().unwrap_or(max_rid).max(max_rid);

                    let new_cursor = PrCursor {
                        last_head_sha: pr.head_ref_oid.clone(),
                        last_comment_id: new_max_comment,
                        last_review_id: new_max_review,
                        last_updated: Utc::now().to_rfc3339(),
                    };
                    let old_cursor = cursor;
                    {
                        let mut s = state.lock().unwrap();
                        s.pr_cursors.insert(pr_key.clone(), new_cursor);
                    }

                    let ctx = serde_json::json!({
                        "repo": pr.repo,
                        "pr_number": pr.number,
                        "title": pr.title,
                        "bot_username": config.bot_username,
                        "comments": new_ic.iter().map(|c| json!({
                            "author": c.author.as_ref().map(|a| a.login.as_str()),
                            "body": c.body,
                        })).collect::<Vec<_>>(),
                        "review_comments": new_rc.iter().map(|c| json!({
                            "author": c.author.as_ref().map(|a| a.login.as_str()),
                            "body": c.body,
                        })).collect::<Vec<_>>(),
                        "reviews": new_rv.iter().map(|r| json!({
                            "author": r.user.as_ref().map(|u| u.login.as_str()),
                            "state": r.state,
                            "body": r.body,
                        })).collect::<Vec<_>>(),
                    });

                    info!("[pr-feedback] pr {}/{} — {} new authorized comment(s) ({} ic, {} rc, {} rv)",
                        pr.repo, pr.number, new_ic.len() + new_rc.len() + new_rv.len(),
                        new_ic.len(), new_rc.len(), new_rv.len());

                    add_eyes_reaction(&config, &pr.repo, pr.number).await;

                    let config = config.clone();
                    let state = Arc::clone(&state);
                    let state_file = config.state_file.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let label = format!("{}-pr-{}", ctx["repo"].as_str().unwrap(), ctx["pr_number"]);
                        let dir = match ensure_task_dir(&config, &label).await {
                            Ok(d) => d,
                            Err(e) => { error!("[{label}] task dir failed: {e:#}"); return; }
                        };
                        if let Ok(true) = launch_opencode(&config, &dir, &ctx.to_string(), "pr-feedback", &label).await {
                            let s = state.lock().unwrap();
                            save_state(&state_file, &s).ok();
                        } else {
                            let mut s = state.lock().unwrap();
                            if let Some(prev) = old_cursor {
                                s.pr_cursors.insert(pr_key.clone(), prev);
                            } else {
                                s.pr_cursors.remove(&pr_key);
                            }
                            save_state(&state_file, &s).ok();
                        }
                    });
                }
            }
            Err(e) => warn!("pr fetch failed: {e:#}"),
        }

        // ── 4. Mentions via search API (author-gated) ──
        if config.poll_mentions {
            // ── 4a. Proactive: scan authorized user's issue/PR comment threads ──
            // Checks comments on threads the authorized user created, so the bot
            // picks up mentions that happen later in the lifecycle, even if the
            // search API hasn't indexed them yet.
            {
                match fetch_authorized_issues(&config).await {
                    Ok(issues) => {
                        let bot = &config.bot_username;

                        info!("[mentions] scanning {} authorized user issue(s) for comment mentions",
                            issues.len());

                        for issue in &issues {
                            let comments = fetch_pr_issue_comments(&config, &issue.repo, issue.number)
                                .await.unwrap_or_default();
                            for comment in &comments {
                                let ckey = format!("{}#{}#comment-{}", issue.repo, issue.number, comment.id);
                                {
                                    let s = state.lock().unwrap();
                                    if s.processed_mentions.contains_key(&ckey) {
                                        continue;
                                    }
                                }
                                if !contains_mention(&comment.body, bot) {
                                    continue;
                                }
                                if !is_authorized(&comment.author, &config) {
                                    info!("[mentions] skip {}#{} comment {} — author not authorized",
                                        issue.repo, issue.number, comment.id);
                                    continue;
                                }
                                {
                                    let mut s = state.lock().unwrap();
                                    s.processed_mentions.insert(ckey.clone(), Utc::now().to_rfc3339());
                                    save_state(&config.state_file, &s).ok();
                                }
                                info!("[mentions] {}#{} issue comment {} — mention found",
                                    issue.repo, issue.number, comment.id);

                                add_eyes_reaction(&config, &issue.repo, issue.number).await;

                                let ctx = serde_json::json!({
                                    "repo": issue.repo,
                                    "number": issue.number,
                                    "title": issue.title,
                                    "body": comment.body,
                                    "author": comment.author.as_ref().map(|a| a.login.as_str()),
                                    "type": "Issue",
                                    "source": "comment",
                                    "url": issue.url,
                                    "bot_username": config.bot_username,
                                });

                                let label = format!("{}-auth-issue-{}-comment-{}",
                                    issue.repo, issue.number, comment.id);
                                let config = config.clone();
                                let permit = semaphore.clone().acquire_owned().await.unwrap();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let dir = match ensure_task_dir(&config, &label).await {
                                        Ok(d) => d,
                                        Err(e) => { error!("[{label}] task dir failed: {e:#}"); return; }
                                    };
                                    let _ = launch_opencode(&config, &dir, &ctx.to_string(),
                                        "mention", &label).await;
                                });
                            }
                        }
                    }
                    Err(e) => warn!("auth issues fetch failed: {e:#}"),
                }

                match fetch_authorized_prs(&config).await {
                    Ok(prs) => {
                        let bot = &config.bot_username;

                        info!("[mentions] scanning {} authorized user PR(s) for comment mentions",
                            prs.len());

                        for pr in &prs {
                            let comments = fetch_pr_issue_comments(&config, &pr.repo, pr.number)
                                .await.unwrap_or_default();
                            for comment in &comments {
                                let ckey = format!("{}#{}#comment-{}", pr.repo, pr.number, comment.id);
                                {
                                    let s = state.lock().unwrap();
                                    if s.processed_mentions.contains_key(&ckey) {
                                        continue;
                                    }
                                }
                                if !contains_mention(&comment.body, bot) {
                                    continue;
                                }
                                if !is_authorized(&comment.author, &config) {
                                    info!("[mentions] skip {}#{} pr comment {} — author not authorized",
                                        pr.repo, pr.number, comment.id);
                                    continue;
                                }
                                {
                                    let mut s = state.lock().unwrap();
                                    s.processed_mentions.insert(ckey.clone(), Utc::now().to_rfc3339());
                                    save_state(&config.state_file, &s).ok();
                                }
                                info!("[mentions] {}#{} pr comment {} — mention found",
                                    pr.repo, pr.number, comment.id);

                                add_eyes_reaction(&config, &pr.repo, pr.number).await;

                                let ctx = serde_json::json!({
                                    "repo": pr.repo,
                                    "number": pr.number,
                                    "title": pr.title,
                                    "body": comment.body,
                                    "author": comment.author.as_ref().map(|a| a.login.as_str()),
                                    "type": "PullRequest",
                                    "source": "comment",
                                    "url": format!("https://github.com/{}/pull/{}", pr.repo, pr.number),
                                    "bot_username": config.bot_username,
                                });

                                let label = format!("{}-auth-pr-{}-comment-{}",
                                    pr.repo, pr.number, comment.id);
                                let config = config.clone();
                                let permit = semaphore.clone().acquire_owned().await.unwrap();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let dir = match ensure_task_dir(&config, &label).await {
                                        Ok(d) => d,
                                        Err(e) => { error!("[{label}] task dir failed: {e:#}"); return; }
                                    };
                                    let _ = launch_opencode(&config, &dir, &ctx.to_string(),
                                        "mention", &label).await;
                                });
                            }
                        }
                    }
                    Err(e) => warn!("auth prs fetch failed: {e:#}"),
                }
            }
            // ── 4b. Search API (fallback / broad catch-all) ──
            match fetch_mentions(&config).await {
                Ok(items) => {
                    let bot = &config.bot_username;
                    info!("[mentions] fetched {} search result(s)", items.len());

                    for item in &items {
                        let repo = repo_from_url(&item.html_url);
                        let num = item.number;
                        let is_pr = item.pull_request.is_some();
                        let kind = if is_pr { "PullRequest" } else { "Issue" };

                        let body_key = format!("{repo}#{num}#body");
                        let should_dispatch_body = {
                            let s = state.lock().unwrap();
                            let contained = s.processed_mentions.contains_key(&body_key);
                            if contained { false } else {
                                if let Some(ref body) = item.body {
                                    if contains_mention(body, bot) && is_authorized(&item.user, &config) {
                                        drop(s);
                                        {
                                            let mut s = state.lock().unwrap();
                                            s.processed_mentions.insert(body_key.clone(), Utc::now().to_rfc3339());
                                        }
                                        true
                                    } else { false }
                                } else { false }
                            }
                        };

                        if should_dispatch_body {
                            let body = item.body.as_ref().unwrap();
                            info!("  mention {repo}#{num} ({kind} body)");

                            let ctx = serde_json::json!({
                                "repo": repo,
                                "number": num,
                                "title": item.title,
                                "body": body,
                                "author": item.user.as_ref().map(|a| a.login.as_str()),
                                "type": kind,
                                "source": "body",
                                "url": item.html_url,
                                "bot_username": config.bot_username,
                            });

                            let label = format!("{}-{}-{}-body", repo, kind.to_lowercase(), num);
                            let config = config.clone();
                            let state = Arc::clone(&state);
                            let state_file = config.state_file.clone();
                            let key = body_key;
                            let permit = semaphore.clone().acquire_owned().await.unwrap();
                            tokio::spawn(async move {
                                let _permit = permit;
                                let dir = match ensure_task_dir(&config, &label).await {
                                    Ok(d) => d,
                                    Err(e) => { error!("[{label}] task dir failed: {e:#}"); return; }
                                };
                                if let Ok(true) = launch_opencode(&config, &dir, &ctx.to_string(), "mention", &label).await {
                                    let s = state.lock().unwrap();
                                    save_state(&state_file, &s).ok();
                                } else {
                                    let mut s = state.lock().unwrap();
                                    s.processed_mentions.remove(&key);
                                    save_state(&state_file, &s).ok();
                                }
                            });
                        }

                        let comments = fetch_pr_issue_comments(&config, &repo, num).await.unwrap_or_default();
                        info!("[mentions] {repo}#{num} — {} comment(s) to scan", comments.len());
                        for comment in &comments {
                            let ckey = format!("{repo}#{num}#comment-{}", comment.id);
                            let should_dispatch_comment = {
                                let s = state.lock().unwrap();
                                let already_processed = s.processed_mentions.contains_key(&ckey);
                                if already_processed { false } else {
                                    if contains_mention(&comment.body, bot) && is_authorized(&comment.author, &config) {
                                        drop(s);
                                        {
                                            let mut s = state.lock().unwrap();
                                            s.processed_mentions.insert(ckey.clone(), Utc::now().to_rfc3339());
                                        }
                                        true
                                    } else { false }
                                }
                            };

                            if should_dispatch_comment {
                                info!("  mention {repo}#{num} ({kind} comment {})", comment.id);

                                let ctx = serde_json::json!({
                                    "repo": repo,
                                    "number": num,
                                    "title": item.title,
                                    "body": comment.body,
                                    "author": comment.author.as_ref().map(|a| a.login.as_str()),
                                    "type": kind,
                                    "source": "comment",
                                    "url": item.html_url,
                                    "bot_username": config.bot_username,
                                });

                                let label = format!("{}-{}-{}-comment-{}", repo, kind.to_lowercase(), num, comment.id);
                                let config = config.clone();
                                let state = Arc::clone(&state);
                                let state_file = config.state_file.clone();
                                let key = ckey;
                                let permit = semaphore.clone().acquire_owned().await.unwrap();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let dir = match ensure_task_dir(&config, &label).await {
                                        Ok(d) => d,
                                        Err(e) => { error!("[{label}] task dir failed: {e:#}"); return; }
                                    };
                                    if let Ok(true) = launch_opencode(&config, &dir, &ctx.to_string(), "mention", &label).await {
                                        let s = state.lock().unwrap();
                                        save_state(&state_file, &s).ok();
                                    } else {
                                        let mut s = state.lock().unwrap();
                                        s.processed_mentions.remove(&key);
                                        save_state(&state_file, &s).ok();
                                    }
                                });
                            }
                        }
                    }
                }
                Err(e) => warn!("mentions fetch failed: {e:#}"),
            }
        }

        info!("sleeping {}s", config.poll_interval_secs);
        tokio::time::sleep(Duration::from_secs(config.poll_interval_secs)).await;
    }
}
