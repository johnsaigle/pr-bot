use chrono::Utc;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::agent::{ensure_task_dir, launch_opencode};
use crate::config::Config;
use crate::github::{
    add_eyes_reaction, contains_mention, fetch_assigned_issues, fetch_authorized_issues,
    fetch_authorized_prs, fetch_bot_issues, fetch_mentions, fetch_open_prs,
    fetch_pr_issue_comments, fetch_pr_review_comments, fetch_pr_reviews, is_authorized,
    repo_from_url,
};
use crate::health::run_health_checks;
use crate::state::{IssueCursor, PrCursor, load_state, save_state};

#[allow(
    clippy::significant_drop_tightening,
    clippy::await_holding_lock,
    clippy::future_not_send
)]
pub(crate) async fn run(config: Config) {
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
                    issues
                        .into_iter()
                        .filter(|i| {
                            let key = format!("{}/issues#{}", i.repo, i.number);
                            is_authorized(i.author.as_ref(), &config)
                                && !s.processed_issues.contains_key(&key)
                        })
                        .collect()
                };

                if !new.is_empty() {
                    info!("[issues] {} new authorized issue(s)", new.len());
                }

                for issue in new {
                    let key = format!("{}/issues#{}", issue.repo, issue.number);
                    {
                        let mut s = state.lock().unwrap();
                        s.processed_issues
                            .insert(key.clone(), Utc::now().to_rfc3339());
                    }
                    info!(
                        "  issue {repo}#{num}",
                        repo = issue.repo,
                        num = issue.number
                    );

                    add_eyes_reaction(&config, &issue.repo, issue.number).await;

                    let ctx = serde_json::json!({
                        "repo": issue.repo,
                        "issue_number": issue.number,
                        "title": issue.title,
                        "body": issue.body,
                        "author": issue.author.map(|a| a.login),
                        "bot_username": config.bot_username,
                        "attribution": config.attribution_context(),
                    });

                    let config = config.clone();
                    let state = Arc::clone(&state);
                    let state_file = config.state_file.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let label = format!(
                            "{}-issue-{}",
                            ctx["repo"].as_str().unwrap(),
                            ctx["issue_number"]
                        );
                        let dir = match ensure_task_dir(&config, &label).await {
                            Ok(d) => d,
                            Err(e) => {
                                error!("[{label}] task dir failed: {e:#}");
                                return;
                            }
                        };
                        if matches!(
                            launch_opencode(&config, &dir, &ctx.to_string(), "new-issue", &label)
                                .await,
                            Ok(true)
                        ) {
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
                        s.issue_cursors.get(&key).map_or(0, |c| c.last_comment_id)
                    };

                    let comments = fetch_pr_issue_comments(&config, &issue.repo, issue.number)
                        .await
                        .unwrap_or_default();
                    let new_comments: Vec<_> = comments
                        .iter()
                        .filter(|c| c.id > max_cid && is_authorized(c.author.as_ref(), &config))
                        .cloned()
                        .collect();

                    if new_comments.is_empty() {
                        continue;
                    }

                    let new_max = new_comments
                        .iter()
                        .map(|c| c.id)
                        .max()
                        .unwrap_or(max_cid)
                        .max(max_cid);
                    {
                        let mut s = state.lock().unwrap();
                        s.issue_cursors.insert(
                            key.clone(),
                            IssueCursor {
                                last_comment_id: new_max,
                                last_updated: Utc::now().to_rfc3339(),
                            },
                        );
                        save_state(&config.state_file, &s).ok();
                    }

                    info!(
                        "  issue {}/{} — {} new authorized comment(s)",
                        issue.repo,
                        issue.number,
                        new_comments.len()
                    );

                    let ctx = serde_json::json!({
                        "repo": issue.repo,
                        "number": issue.number,
                        "title": issue.title,
                        "author": issue.author.map(|a| a.login),
                        "type": "Issue",
                        "url": issue.url,
                        "body": new_comments.last().map_or("", |c| c.body.as_str()),
                        "comments": new_comments.iter().map(|c| json!({
                            "author": c.author.as_ref().map(|a| a.login.as_str()),
                            "body": c.body,
                        })).collect::<Vec<_>>(),
                        "attribution": config.attribution_context(),
                    });

                    let config = config.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let label = format!(
                            "{}-issue-comment-{}",
                            ctx["repo"].as_str().unwrap(),
                            ctx["number"]
                        );
                        let dir = match ensure_task_dir(&config, &label).await {
                            Ok(d) => d,
                            Err(e) => {
                                error!("[{label}] task dir failed: {e:#}");
                                return;
                            }
                        };
                        let _ = launch_opencode(&config, &dir, &ctx.to_string(), "mention", &label)
                            .await;
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
                    if cursor
                        .as_ref()
                        .is_some_and(|c| c.last_head_sha == pr.head_ref_oid)
                    {
                        continue;
                    }

                    let ic = fetch_pr_issue_comments(&config, &pr.repo, pr.number)
                        .await
                        .unwrap_or_default();
                    let rc = fetch_pr_review_comments(&config, &pr.repo, pr.number)
                        .await
                        .unwrap_or_default();
                    let rv = fetch_pr_reviews(&config, &pr.repo, pr.number)
                        .await
                        .unwrap_or_default();

                    let max_cid = cursor.as_ref().map_or(0, |c| c.last_comment_id);
                    let max_rid = cursor.as_ref().map_or(0, |c| c.last_review_id);

                    let new_ic: Vec<_> = ic
                        .iter()
                        .filter(|c| c.id > max_cid && is_authorized(c.author.as_ref(), &config))
                        .cloned()
                        .collect();
                    let new_rc: Vec<_> = rc
                        .iter()
                        .filter(|c| c.id > max_cid && is_authorized(c.author.as_ref(), &config))
                        .cloned()
                        .collect();
                    let new_rv: Vec<_> = rv
                        .iter()
                        .filter(|r| r.id > max_rid && is_authorized(r.user.as_ref(), &config))
                        .cloned()
                        .collect();

                    if new_ic.is_empty() && new_rc.is_empty() && new_rv.is_empty() {
                        info!(
                            "[pr-feedback] skip {}/{} — {} ic, {} rc, {} rv total; {} ic, {} rc, {} rv after cursor; 0 authorized",
                            pr.repo,
                            pr.number,
                            ic.len(),
                            rc.len(),
                            rv.len(),
                            ic.iter().filter(|c| c.id > max_cid).count(),
                            rc.iter().filter(|c| c.id > max_cid).count(),
                            rv.iter().filter(|r| r.id > max_rid).count()
                        );
                        continue;
                    }

                    let new_max_comment = new_ic
                        .iter()
                        .chain(new_rc.iter())
                        .map(|c| c.id)
                        .max()
                        .unwrap_or(max_cid)
                        .max(max_cid);
                    let new_max_review = new_rv
                        .iter()
                        .map(|r| r.id)
                        .max()
                        .unwrap_or(max_rid)
                        .max(max_rid);

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
                        "attribution": config.attribution_context(),
                    });

                    info!(
                        "[pr-feedback] pr {}/{} — {} new authorized comment(s) ({} ic, {} rc, {} rv)",
                        pr.repo,
                        pr.number,
                        new_ic.len() + new_rc.len() + new_rv.len(),
                        new_ic.len(),
                        new_rc.len(),
                        new_rv.len()
                    );

                    add_eyes_reaction(&config, &pr.repo, pr.number).await;

                    let config = config.clone();
                    let state = Arc::clone(&state);
                    let state_file = config.state_file.clone();
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let label =
                            format!("{}-pr-{}", ctx["repo"].as_str().unwrap(), ctx["pr_number"]);
                        let dir = match ensure_task_dir(&config, &label).await {
                            Ok(d) => d,
                            Err(e) => {
                                error!("[{label}] task dir failed: {e:#}");
                                return;
                            }
                        };
                        if matches!(
                            launch_opencode(&config, &dir, &ctx.to_string(), "pr-feedback", &label)
                                .await,
                            Ok(true)
                        ) {
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

                        info!(
                            "[mentions] scanning {} authorized user issue(s) for comment mentions",
                            issues.len()
                        );

                        for issue in &issues {
                            let comments =
                                fetch_pr_issue_comments(&config, &issue.repo, issue.number)
                                    .await
                                    .unwrap_or_default();
                            for comment in &comments {
                                let ckey = format!(
                                    "{}#{}#comment-{}",
                                    issue.repo, issue.number, comment.id
                                );
                                {
                                    let s = state.lock().unwrap();
                                    if s.processed_mentions.contains_key(&ckey) {
                                        continue;
                                    }
                                }
                                if !contains_mention(&comment.body, bot) {
                                    continue;
                                }
                                if !is_authorized(comment.author.as_ref(), &config) {
                                    info!(
                                        "[mentions] skip {}#{} comment {} — author not authorized",
                                        issue.repo, issue.number, comment.id
                                    );
                                    continue;
                                }
                                {
                                    let mut s = state.lock().unwrap();
                                    s.processed_mentions
                                        .insert(ckey.clone(), Utc::now().to_rfc3339());
                                    save_state(&config.state_file, &s).ok();
                                }
                                info!(
                                    "[mentions] {}#{} issue comment {} — mention found",
                                    issue.repo, issue.number, comment.id
                                );

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
                                    "attribution": config.attribution_context(),
                                });

                                let label = format!(
                                    "{}-auth-issue-{}-comment-{}",
                                    issue.repo, issue.number, comment.id
                                );
                                let config = config.clone();
                                let permit = semaphore.clone().acquire_owned().await.unwrap();
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
                                        "mention",
                                        &label,
                                    )
                                    .await;
                                });
                            }
                        }
                    }
                    Err(e) => warn!("auth issues fetch failed: {e:#}"),
                }

                match fetch_authorized_prs(&config).await {
                    Ok(prs) => {
                        let bot = &config.bot_username;

                        info!(
                            "[mentions] scanning {} authorized user PR(s) for comment mentions",
                            prs.len()
                        );

                        for pr in &prs {
                            let comments = fetch_pr_issue_comments(&config, &pr.repo, pr.number)
                                .await
                                .unwrap_or_default();
                            for comment in &comments {
                                let ckey =
                                    format!("{}#{}#comment-{}", pr.repo, pr.number, comment.id);
                                {
                                    let s = state.lock().unwrap();
                                    if s.processed_mentions.contains_key(&ckey) {
                                        continue;
                                    }
                                }
                                if !contains_mention(&comment.body, bot) {
                                    continue;
                                }
                                if !is_authorized(comment.author.as_ref(), &config) {
                                    info!(
                                        "[mentions] skip {}#{} pr comment {} — author not authorized",
                                        pr.repo, pr.number, comment.id
                                    );
                                    continue;
                                }
                                {
                                    let mut s = state.lock().unwrap();
                                    s.processed_mentions
                                        .insert(ckey.clone(), Utc::now().to_rfc3339());
                                    save_state(&config.state_file, &s).ok();
                                }
                                info!(
                                    "[mentions] {}#{} pr comment {} — mention found",
                                    pr.repo, pr.number, comment.id
                                );

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
                                    "attribution": config.attribution_context(),
                                });

                                let label = format!(
                                    "{}-auth-pr-{}-comment-{}",
                                    pr.repo, pr.number, comment.id
                                );
                                let config = config.clone();
                                let permit = semaphore.clone().acquire_owned().await.unwrap();
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
                                        "mention",
                                        &label,
                                    )
                                    .await;
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
                            if contained {
                                false
                            } else if let Some(ref body) = item.body {
                                if contains_mention(body, bot)
                                    && is_authorized(item.user.as_ref(), &config)
                                {
                                    drop(s);
                                    {
                                        let mut s = state.lock().unwrap();
                                        s.processed_mentions
                                            .insert(body_key.clone(), Utc::now().to_rfc3339());
                                    }
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        };

                        if should_dispatch_body {
                            let body = item.body.as_ref().unwrap();
                            info!("  mention {repo}#{num} ({kind} body)");

                            add_eyes_reaction(&config, &repo, num).await;

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
                                "attribution": config.attribution_context(),
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
                                    Err(e) => {
                                        error!("[{label}] task dir failed: {e:#}");
                                        return;
                                    }
                                };
                                if matches!(
                                    launch_opencode(
                                        &config,
                                        &dir,
                                        &ctx.to_string(),
                                        "mention",
                                        &label
                                    )
                                    .await,
                                    Ok(true)
                                ) {
                                    let s = state.lock().unwrap();
                                    save_state(&state_file, &s).ok();
                                } else {
                                    let mut s = state.lock().unwrap();
                                    s.processed_mentions.remove(&key);
                                    save_state(&state_file, &s).ok();
                                }
                            });
                        }

                        let comments = fetch_pr_issue_comments(&config, &repo, num)
                            .await
                            .unwrap_or_default();
                        info!(
                            "[mentions] {repo}#{num} — {} comment(s) to scan",
                            comments.len()
                        );
                        for comment in &comments {
                            let ckey = format!("{repo}#{num}#comment-{}", comment.id);
                            let should_dispatch_comment = {
                                let s = state.lock().unwrap();
                                let already_processed = s.processed_mentions.contains_key(&ckey);
                                if already_processed {
                                    false
                                } else if contains_mention(&comment.body, bot)
                                    && is_authorized(comment.author.as_ref(), &config)
                                {
                                    drop(s);
                                    {
                                        let mut s = state.lock().unwrap();
                                        s.processed_mentions
                                            .insert(ckey.clone(), Utc::now().to_rfc3339());
                                    }
                                    true
                                } else {
                                    false
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
                                    "attribution": config.attribution_context(),
                                });

                                let label = format!(
                                    "{}-{}-{}-comment-{}",
                                    repo,
                                    kind.to_lowercase(),
                                    num,
                                    comment.id
                                );
                                let config = config.clone();
                                let state = Arc::clone(&state);
                                let state_file = config.state_file.clone();
                                let key = ckey;
                                let permit = semaphore.clone().acquire_owned().await.unwrap();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let dir = match ensure_task_dir(&config, &label).await {
                                        Ok(d) => d,
                                        Err(e) => {
                                            error!("[{label}] task dir failed: {e:#}");
                                            return;
                                        }
                                    };
                                    if matches!(
                                        launch_opencode(
                                            &config,
                                            &dir,
                                            &ctx.to_string(),
                                            "mention",
                                            &label
                                        )
                                        .await,
                                        Ok(true)
                                    ) {
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

        // ── 4. Health checks (interleaved with short sleeps) ──
        let health_interval = config.health_check_interval_secs;
        let mut elapsed = 0u64;
        while elapsed < config.poll_interval_secs {
            let remaining = config.poll_interval_secs - elapsed;
            let sleep_dur = health_interval.min(remaining);
            info!(
                "sleeping {}s (health check in {sleep_dur}s, next poll in {remaining}s)",
                sleep_dur
            );
            tokio::time::sleep(Duration::from_secs(sleep_dur)).await;
            elapsed += sleep_dur;
            if elapsed >= config.poll_interval_secs {
                break;
            }
            info!("── health check ──");
            run_health_checks(&config, &mut state.lock().unwrap(), &semaphore).await;
        }
    }
}
