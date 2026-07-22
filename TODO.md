# TODO

## P0: Security

- [ ] Replace `--dangerously-skip-permissions` with task-specific capabilities for read-only analysis, comments, PR updates, and pushes.
- [ ] Isolate agent credentials using a sanitized environment and short-lived, repository-scoped GitHub tokens or an audited credential broker.
- [ ] Add prompt-injection and credential-exfiltration evals using malicious issues, comments, repository files, logs, and dependency metadata.

## P1: Reliability

- [ ] Persist a task lifecycle (`queued`, `running`, `succeeded`, `failed`) with stable task IDs, attempts, timestamps, and artifact URLs.
- [ ] Verify the expected GitHub artifact after each run before advancing event cursors or marking work complete.
- [ ] Make cursor updates failure-safe across every dispatch path so failed launches and incomplete runs can retry.
- [ ] Prevent concurrent agents from working on the same issue or PR with a per-thread claim or lock.

## P1: Observability

- [ ] Record structured run data: trigger, workflow, model, repository, thread, context hash, duration, exit reason, and observed artifacts.
- [ ] Convert production failures and undesirable agent behavior into replayable regression evals.
- [ ] Add progress reporting for long-running tasks and distinguish timeouts, policy denials, agent failures, and missing artifacts.

## P2: Collaboration

- [ ] Preserve issue and PR context across runs so authorized participants can steer one durable session instead of launching isolated agents.
- [ ] Expand proactive health checks to failing CI, blocked merge queues, stale reviews, and unfulfilled bot promises.
- [ ] Generalize `authorized_user` into role- and repository-scoped authorization before supporting multiple collaborators.

## P2: Memory

- [ ] Add auditable repository and thread memory for project conventions, test commands, decisions, known flaky tests, and user preferences.
- [ ] Require trusted provenance for memory updates so untrusted content cannot create persistent instructions.

## P3: Workflows

- [ ] Evaluate leaner, model-specific workflows while retaining hard safety invariants such as fork-only pushes and worktree boundaries.
- [ ] Use completion rate, artifact quality, policy violations, and regression evals to assess workflow changes.
