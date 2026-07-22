use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::agent::{ensure_task_dir, launch_opencode};
use crate::config::Config;
use crate::github::{
    fetch_assigned_issues, fetch_open_prs, fetch_pr_issue_comments, fetch_pr_reviews,
    is_authorized, issue_url, pr_url, run_cmd,
};
use crate::state::{State, save_state};

#[derive(Debug, Deserialize)]
struct GhPrMergeable {
    mergeable: Option<String>,
    #[serde(rename = "mergeStateStatus")]
    merge_state_status: Option<String>,
}

fn grace_period_elapsed(timestamp: &str, grace_period_secs: u64, now: DateTime<Utc>) -> bool {
    let Ok(timestamp) = DateTime::parse_from_rfc3339(timestamp) else {
        return true;
    };
    let Ok(grace_period_secs) = i64::try_from(grace_period_secs) else {
        return false;
    };
    now.signed_duration_since(timestamp).num_seconds() >= grace_period_secs
}

async fn fetch_pr_mergeable(config: &Config, repo: &str, pr_number: u64) -> Result<GhPrMergeable> {
    let args: Vec<String> = vec![
        "pr".into(),
        "view".into(),
        pr_number.to_string(),
        "--repo".into(),
        repo.to_string(),
        "--json".into(),
        "mergeable,mergeStateStatus".into(),
    ];
    let stdout = run_cmd(&config.gh_bin, &args).await?;
    serde_json::from_str(&stdout).context("Failed to parse pr mergeable")
}

pub(crate) async fn run_health_checks(
    config: &Config,
    state: &mut State,
    semaphore: &Arc<Semaphore>,
) {
    let _ = check_prs_health(config, state, semaphore).await;
    let _ = check_stale_assigned_issues(config, state, semaphore).await;
}

async fn check_prs_health(
    config: &Config,
    state: &mut State,
    semaphore: &Arc<Semaphore>,
) -> Result<()> {
    let prs = match fetch_open_prs(config).await {
        Ok(p) => p,
        Err(e) => {
            warn!("health: pr fetch failed: {e:#}");
            return Ok(());
        }
    };

    for pr in &prs {
        let repo = &pr.repo;
        let num = pr.number;
        let pr_key = format!("{repo}/prs#{num}");

        if state.pr_cursors.get(&pr_key).is_some_and(|cursor| {
            !grace_period_elapsed(
                &cursor.last_updated,
                config.health_check_grace_period_secs,
                Utc::now(),
            )
        }) {
            info!(
                "  health: skipping {}; normal PR activity is within grace period",
                pr_url(repo, num)
            );
            continue;
        }

        let reviews = fetch_pr_reviews(config, repo, num)
            .await
            .unwrap_or_default();
        let comments = fetch_pr_issue_comments(config, repo, num, &pr.url)
            .await
            .unwrap_or_default();

        // ── changes-requested ──
        let has_changes_requested = reviews
            .iter()
            .any(|r| r.state == "CHANGES_REQUESTED" && is_authorized(r.user.as_ref(), config));
        if has_changes_requested {
            let key = format!("{repo}/prs#{num}-changes-requested");
            if !state.processed_health.contains_key(&key) {
                state
                    .processed_health
                    .insert(key.clone(), Utc::now().to_rfc3339());
                info!("  health: {} CHANGES_REQUESTED review", pr_url(repo, num));

                let ctx = json!({
                    "repo": repo,
                    "pr_number": num,
                    "title": pr.title,
                    "type": "changes-requested",
                    "details": "The authorized user left a CHANGES_REQUESTED review on this PR. The PR needs updates before it can be merged.",
                    "bot_username": config.bot_username,
                });

                let config = config.clone();
                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let label = format!("{}-health-pr-{}-cr", repo.replace('/', "-"), num);
                tokio::spawn(async move {
                    let _permit = permit;
                    let dir = match ensure_task_dir(&config, &label).await {
                        Ok(d) => d,
                        Err(e) => {
                            error!("[{label}] task dir failed: {e:#}");
                            return;
                        }
                    };
                    let _ =
                        launch_opencode(&config, &dir, &ctx.to_string(), "health-check", &label)
                            .await;
                });
            }
        }

        // ── unresolved-comment ──
        let auth_comments: Vec<_> = comments
            .iter()
            .filter(|c| is_authorized(c.author.as_ref(), config))
            .collect();
        if let Some(last_auth_comment) = auth_comments.last() {
            if !grace_period_elapsed(
                &last_auth_comment.created_at,
                config.health_check_grace_period_secs,
                Utc::now(),
            ) {
                continue;
            }
            let bot_has_replied = comments.iter().any(|c| {
                c.id > last_auth_comment.id
                    && c.author
                        .as_ref()
                        .is_some_and(|a| a.login == config.bot_username)
            });
            if !bot_has_replied {
                let key = format!("{repo}/prs#{num}-unresolved-{}", last_auth_comment.id);
                if !state.processed_health.contains_key(&key) {
                    state
                        .processed_health
                        .insert(key.clone(), Utc::now().to_rfc3339());
                    info!(
                        "  health: {} unresolved comment {}",
                        pr_url(repo, num),
                        last_auth_comment.id
                    );

                    let ctx = json!({
                        "repo": repo,
                        "pr_number": num,
                        "title": pr.title,
                        "type": "unresolved-comment",
                        "details": format!(
                            "The authorized user left a comment (id {}) on this PR that has not been answered:\n\n{}",
                            last_auth_comment.id,
                            last_auth_comment.body,
                        ),
                        "bot_username": config.bot_username,
                    });

                    let config = config.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    let label = format!("{}-health-pr-{}-uc", repo.replace('/', "-"), num);
                    tokio::spawn(async move {
                        let _permit = permit;
                        let dir = match ensure_task_dir(&config, &label).await {
                            Ok(d) => d,
                            Err(e) => {
                                error!("[{label}] task dir failed: {e:#}");
                                return;
                            }
                        };
                        let _ = launch_opencode(
                            &config,
                            &dir,
                            &ctx.to_string(),
                            "health-check",
                            &label,
                        )
                        .await;
                    });
                }
            }
        }

        // ── merge-conflict ──
        match fetch_pr_mergeable(config, repo, num).await {
            Ok(m) => {
                let conflicted = m.merge_state_status.as_deref() == Some("DIRTY")
                    || m.merge_state_status.as_deref() == Some("BLOCKED")
                    || m.mergeable.as_deref() == Some("CONFLICTING");
                if conflicted {
                    let key = format!("{repo}/prs#{num}-merge-conflict");
                    if !state.processed_health.contains_key(&key) {
                        state
                            .processed_health
                            .insert(key.clone(), Utc::now().to_rfc3339());
                        info!(
                            "  health: {} merge conflict (status={:?})",
                            pr_url(repo, num),
                            m.merge_state_status
                        );

                        let ctx = json!({
                            "repo": repo,
                            "pr_number": num,
                            "title": pr.title,
                            "type": "merge-conflict",
                            "details": format!(
                                "This PR has merge conflicts against the base branch (mergeStateStatus: {:?}, mergeable: {:?}). It needs a rebase.",
                                m.merge_state_status, m.mergeable,
                            ),
                            "bot_username": config.bot_username,
                        });

                        let config = config.clone();
                        let permit = semaphore.clone().acquire_owned().await.unwrap();
                        let label = format!("{}-health-pr-{}-mc", repo.replace('/', "-"), num);
                        tokio::spawn(async move {
                            let _permit = permit;
                            let dir = match ensure_task_dir(&config, &label).await {
                                Ok(d) => d,
                                Err(e) => {
                                    error!("[{label}] task dir failed: {e:#}");
                                    return;
                                }
                            };
                            let _ = launch_opencode(
                                &config,
                                &dir,
                                &ctx.to_string(),
                                "health-check",
                                &label,
                            )
                            .await;
                        });
                    }
                }
            }
            Err(e) => warn!(
                "health: mergeable check failed for {}: {e:#}",
                pr_url(repo, num)
            ),
        }
    }

    save_state(&config.state_file, state).ok();
    Ok(())
}

async fn check_stale_assigned_issues(
    config: &Config,
    state: &mut State,
    semaphore: &Arc<Semaphore>,
) -> Result<()> {
    let issues = match fetch_assigned_issues(config).await {
        Ok(i) => i,
        Err(e) => {
            warn!("health: issue fetch failed: {e:#}");
            return Ok(());
        }
    };

    for issue in &issues {
        let issue_key = format!("{}/issues#{}", issue.repo, issue.number);
        let health_key = format!("{}/issues#{}", issue.repo, issue.number);

        if !is_authorized(issue.author.as_ref(), config) {
            continue;
        }
        // Only check issues that were previously processed (launched) but still open
        if !state.processed_issues.contains_key(&issue_key) {
            continue;
        }
        if state
            .processed_issues
            .get(&issue_key)
            .is_some_and(|processed_at| {
                !grace_period_elapsed(
                    processed_at,
                    config.health_check_grace_period_secs,
                    Utc::now(),
                )
            })
        {
            continue;
        }
        if state.processed_health.contains_key(&health_key) {
            continue;
        }

        state
            .processed_health
            .insert(health_key.clone(), Utc::now().to_rfc3339());
        info!(
            "  health: {} stale assigned issue",
            issue_url(&issue.repo, issue.number)
        );

        let ctx = json!({
            "repo": issue.repo,
            "issue_number": issue.number,
            "title": issue.title,
            "type": "stale-issue",
            "details": format!(
                "This issue was assigned to you and a task was launched, but the issue is still open.\nIssue body:\n\n{}",
                issue.body,
            ),
            "bot_username": config.bot_username,
        });

        let config = config.clone();
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let label = format!(
            "{}-health-issue-{}",
            issue.repo.replace('/', "-"),
            issue.number
        );
        tokio::spawn(async move {
            let _permit = permit;
            let dir = match ensure_task_dir(&config, &label).await {
                Ok(d) => d,
                Err(e) => {
                    error!("[{label}] task dir failed: {e:#}");
                    return;
                }
            };
            let _ = launch_opencode(&config, &dir, &ctx.to_string(), "health-check", &label).await;
        });
    }

    save_state(&config.state_file, state).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::grace_period_elapsed;
    use chrono::{DateTime, Utc};

    fn now() -> DateTime<Utc> {
        "2026-07-22T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn grace_period_is_inclusive() {
        assert!(grace_period_elapsed(
            "2026-07-08T12:00:00Z",
            14 * 24 * 60 * 60,
            now()
        ));
    }

    #[test]
    fn recent_and_future_timestamps_remain_in_grace_period() {
        assert!(!grace_period_elapsed(
            "2026-07-22T11:59:59Z",
            14 * 24 * 60 * 60,
            now()
        ));
        assert!(!grace_period_elapsed(
            "2026-07-23T12:00:00Z",
            14 * 24 * 60 * 60,
            now()
        ));
    }

    #[test]
    fn malformed_timestamps_do_not_block_health_checks() {
        assert!(grace_period_elapsed("not-a-timestamp", 60, now()));
    }
}
