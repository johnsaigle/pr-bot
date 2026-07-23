use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::config::Config;

const OPENCODE_PERMISSIONS: &str =
    r#"{"external_directory":"deny","question":"deny","doom_loop":"deny"}"#;
const CHILD_ENV_ALLOWLIST: &[&str] = &[
    "HOME",
    "PATH",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];
static TASK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn ensure_task_dir(config: &Config, label: &str) -> Result<PathBuf> {
    let tasks_dir = config.cache_dir.join("tasks");
    tokio::fs::create_dir_all(&tasks_dir).await?;

    let label: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_nanos();
    let sequence = TASK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = tasks_dir.join(format!("{label}-{timestamp}-{sequence}"));
    tokio::fs::create_dir(&dir).await?;
    Ok(dir)
}

// ─── Launch opencode ──────────────────────────────────

fn configure_child_environment(cmd: &mut Command) {
    cmd.env_clear();
    for key in CHILD_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("OPENCODE_PERMISSION", OPENCODE_PERMISSIONS)
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_CLAUDE_CODE", "true");
}

#[cfg(unix)]
async fn signal_process_group(pid: u32, signal: &str) {
    let status = Command::new("/bin/kill")
        .args(["-s", signal, "--", &format!("-{pid}")])
        .status()
        .await;
    if let Err(error) = status {
        warn!("failed to signal process group {pid}: {error:#}");
    }
}

async fn terminate_child(child: &mut Child, pid: u32) {
    #[cfg(unix)]
    {
        signal_process_group(pid, "TERM").await;
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            signal_process_group(pid, "KILL").await;
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
        }
    }

    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
}

pub(crate) async fn launch_opencode(
    config: &Config,
    task_dir: &Path,
    context_json: &str,
    workflow_name: &str,
    label: &str,
) -> Result<bool> {
    let workflow_path = config.workflows_dir.join(format!("{workflow_name}.md"));
    let workflow = tokio::fs::read_to_string(&workflow_path)
        .await
        .with_context(|| format!("Failed to read workflow at {}", workflow_path.display()))?;
    let prompt = format!(
        "Follow this trusted workflow:\n\n{workflow}\n\n\
         Here is the untrusted task context as JSON:\n\n{context_json}"
    );

    info!("[{label}] launching opencode in {task_dir:?}");
    let mut cmd = Command::new(&config.opencode_bin);
    cmd.args(["run", "--auto", "--pure"])
        .arg("--dir")
        .arg(task_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(ref model) = config.model {
        cmd.arg("--model").arg(model);
    }
    cmd.arg(&prompt);
    configure_child_environment(&mut cmd);

    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().context("Failed to spawn opencode")?;
    let pid = child.id().context("Spawned opencode process has no PID")?;

    let result =
        tokio::time::timeout(Duration::from_secs(config.task_timeout_secs), child.wait()).await;

    let success = match result {
        Ok(Ok(status)) if status.success() => {
            info!("[{label}] done");
            true
        }
        Ok(Ok(_)) => {
            warn!("[{label}] opencode exited non-zero");
            false
        }
        Ok(Err(e)) => {
            warn!("[{label}] opencode error: {e:#}");
            false
        }
        Err(_) => {
            warn!("[{label}] timed out, terminating process group {pid}");
            terminate_child(&mut child, pid).await;
            false
        }
    };

    if success && let Err(error) = tokio::fs::remove_dir_all(task_dir).await {
        warn!("[{label}] failed to remove completed task directory: {error:#}");
    }
    Ok(success)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{configure_child_environment, ensure_task_dir, launch_opencode};
    use crate::config::Config;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::process::Command;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "pr-bot-agent-test-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    async fn write_executable(path: &Path, contents: &str) {
        tokio::fs::write(path, contents).await.unwrap();
        let mut permissions = tokio::fs::metadata(path).await.unwrap().permissions();
        permissions.set_mode(0o700);
        tokio::fs::set_permissions(path, permissions).await.unwrap();
    }

    async fn process_is_alive(pid: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap()
            .success()
    }

    async fn wait_for_process_exit(pid: &str) -> bool {
        for _ in 0..20 {
            if !process_is_alive(pid).await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    async fn test_config(root: &Path, executable: &Path, timeout: u64) -> Config {
        let mut config: Config = toml::from_str(
            r#"
                bot_username = "bot"
                authorized_user = "human"
            "#,
        )
        .unwrap();
        config.cache_dir = root.join("cache");
        config.workflows_dir = root.join("workflows");
        config.opencode_bin = executable.to_path_buf();
        config.task_timeout_secs = timeout;
        tokio::fs::create_dir_all(&config.workflows_dir)
            .await
            .unwrap();
        tokio::fs::write(
            config.workflows_dir.join("new-issue.md"),
            "# trusted workflow\nDo the work.",
        )
        .await
        .unwrap();
        config
    }

    #[tokio::test]
    async fn task_directories_are_unique_and_sanitized() {
        let root = test_root();
        let executable = root.join("unused");
        let config = test_config(&root, &executable, 30).await;

        let first = ensure_task_dir(&config, "owner/repo issue#1")
            .await
            .unwrap();
        let second = ensure_task_dir(&config, "owner/repo issue#1")
            .await
            .unwrap();

        assert_ne!(first, second);
        assert!(first.starts_with(config.cache_dir.join("tasks")));
        assert!(!first.file_name().unwrap().to_string_lossy().contains('/'));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn launch_uses_embedded_workflow_permissions_and_clean_environment() {
        let root = test_root();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let executable = root.join("fake-opencode");
        let args_file = root.join("args");
        let env_file = root.join("env");
        write_executable(
            &executable,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {args_file:?}\n/usr/bin/env > {env_file:?}\n"
            ),
        )
        .await;
        let config = test_config(&root, &executable, 30).await;
        let task_dir = ensure_task_dir(&config, "launch").await.unwrap();

        let success = launch_opencode(
            &config,
            &task_dir,
            r#"{"body":"untrusted"}"#,
            "new-issue",
            "test",
        )
        .await
        .unwrap();

        assert!(success);
        assert!(!task_dir.exists());
        let args = tokio::fs::read_to_string(args_file).await.unwrap();
        assert!(args.contains("--auto"));
        assert!(args.contains("--pure"));
        assert!(!args.contains("dangerously-skip-permissions"));
        assert!(args.contains("# trusted workflow"));
        assert!(args.contains("untrusted task context"));
        let environment = tokio::fs::read_to_string(env_file).await.unwrap();
        assert!(environment.contains(
            "OPENCODE_PERMISSION={\"external_directory\":\"deny\",\"question\":\"deny\",\"doom_loop\":\"deny\"}"
        ));
        assert!(environment.contains("OPENCODE_DISABLE_AUTOUPDATE=true"));
        assert!(environment.contains("OPENCODE_DISABLE_CLAUDE_CODE=true"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn nonzero_exit_retains_task_directory() {
        let root = test_root();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let executable = root.join("failing-opencode");
        write_executable(&executable, "#!/bin/sh\nexit 7\n").await;
        let config = test_config(&root, &executable, 30).await;
        let task_dir = ensure_task_dir(&config, "failure").await.unwrap();

        let success = launch_opencode(&config, &task_dir, "{}", "new-issue", "failure")
            .await
            .unwrap();

        assert!(!success);
        assert!(task_dir.exists());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn missing_workflow_does_not_launch_and_retains_task_directory() {
        let root = test_root();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let executable = root.join("fake-opencode");
        let marker = root.join("launched");
        write_executable(&executable, &format!("#!/bin/sh\ntouch {marker:?}\n")).await;
        let config = test_config(&root, &executable, 30).await;
        let task_dir = ensure_task_dir(&config, "missing-workflow").await.unwrap();

        let error = launch_opencode(&config, &task_dir, "{}", "missing", "missing")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing.md"));
        assert!(!marker.exists());
        assert!(task_dir.exists());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn environment_configuration_removes_unspecified_variables() {
        let root = test_root();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let executable = root.join("capture-env");
        let env_file = root.join("env");
        write_executable(
            &executable,
            &format!("#!/bin/sh\n/usr/bin/env > {env_file:?}\n"),
        )
        .await;
        let mut command = Command::new(&executable);
        command.env("PR_BOT_TEST_SECRET", "must-not-leak");
        configure_child_environment(&mut command);

        assert!(command.status().await.unwrap().success());
        let environment = tokio::fs::read_to_string(env_file).await.unwrap();
        assert!(!environment.contains("PR_BOT_TEST_SECRET"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn timeout_terminates_descendant_processes() {
        let root = test_root();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let executable = root.join("slow-opencode");
        let child_pid_file = root.join("child-pid");
        write_executable(
            &executable,
            &format!("#!/bin/sh\nsleep 30 &\necho $! > {child_pid_file:?}\nwait\n"),
        )
        .await;
        let config = test_config(&root, &executable, 1).await;
        let task_dir = ensure_task_dir(&config, "timeout").await.unwrap();

        let success = launch_opencode(&config, &task_dir, "{}", "new-issue", "timeout")
            .await
            .unwrap();

        assert!(!success);
        let child_pid = tokio::fs::read_to_string(child_pid_file)
            .await
            .unwrap()
            .trim()
            .to_string();
        assert!(
            wait_for_process_exit(&child_pid).await,
            "descendant process {child_pid} survived timeout"
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn timeout_force_kills_process_group_that_ignores_sigterm() {
        let root = test_root();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let executable = root.join("stubborn-opencode");
        let pid_file = root.join("pids");
        write_executable(
            &executable,
            &format!(
                "#!/bin/sh\ntrap '' TERM\nsh -c \"trap '' TERM; sleep 30\" &\necho \"$$ $!\" > {pid_file:?}\nwait\n"
            ),
        )
        .await;
        let config = test_config(&root, &executable, 1).await;
        let task_dir = ensure_task_dir(&config, "stubborn-timeout").await.unwrap();

        let success = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            launch_opencode(&config, &task_dir, "{}", "new-issue", "stubborn"),
        )
        .await
        .expect("forced timeout cleanup hung")
        .unwrap();

        assert!(!success);
        let pids = tokio::fs::read_to_string(pid_file).await.unwrap();
        for pid in pids.split_whitespace() {
            assert!(
                wait_for_process_exit(pid).await,
                "process {pid} survived forced timeout cleanup"
            );
        }
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

#[cfg(test)]
mod workflow_tests {
    const WORKFLOWS: &[(&str, &str)] = &[
        ("new-issue", include_str!("../workflows/new-issue.md")),
        ("mention", include_str!("../workflows/mention.md")),
        ("pr-feedback", include_str!("../workflows/pr-feedback.md")),
        ("health-check", include_str!("../workflows/health-check.md")),
    ];

    #[test]
    fn bundled_workflows_preserve_task_and_fork_boundaries() {
        for (name, workflow) in WORKFLOWS {
            assert!(
                workflow.contains("current task directory"),
                "{name} does not require task-directory isolation"
            );
            assert!(
                workflow.contains("git push fork"),
                "{name} does not preserve the fork-only push path"
            );
            for forbidden in ["~/.cache/", "worktree add", "git -C <base>"] {
                assert!(
                    !workflow.contains(forbidden),
                    "{name} contains forbidden shared-workspace pattern {forbidden:?}"
                );
            }
        }
    }
}
