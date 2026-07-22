# pr-bot

A GitHub bot that watches for issues, PR feedback, and @mentions from an authorized user, then launches AI coding agents ([opencode](https://github.com/anomalyco/opencode)) to handle each task using declarative workflow files.

## How it works

The bot polls GitHub on a configurable interval for four kinds of events and periodically checks existing work for stalled tasks:

| Event | Trigger |
|-------|---------|
| **New issue** | An issue authored by the authorized user is assigned to the bot |
| **Issue comments** | The authorized user comments on an issue the bot already opened |
| **PR feedback** | The authorized user comments on or reviews a PR the bot authored |
| **Mentions** | The authorized user @-mentions the bot in an issue/PR body or comment |
| **Health checks** | A bot-authored PR or assigned issue remains stalled past the configured grace period |

For each event, the bot creates an isolated task directory and spawns `opencode` with JSON context and a workflow file that describes how to handle the task. Workflows are plain markdown files — you write what the agent should do, and the bot wires up the triggers.

Polling cursors and completed tasks are tracked in a JSON state file to prevent duplicate processing.

## Setup

### Prerequisites

- [Rust](https://rustup.rs/) with edition 2024 support
- [opencode](https://github.com/anomalyco/opencode)
- [`gh` CLI](https://cli.github.com/) authenticated with a GitHub token that has repo/issue/PR scope

### Install

```bash
git clone https://github.com/johnsaigle/pr-bot.git
cd pr-bot
cargo build --release
```

### Configure

Copy the example config and fill in your details:

```bash
mkdir -p ~/.config/pr-bot
cp config.example.toml ~/.config/pr-bot/config.toml
cp -R workflows ~/.config/pr-bot/
```

Edit `~/.config/pr-bot/config.toml`:

```toml
bot_username = "my-coder-bot"
authorized_user = "your-real-username"

# Optional — defaults shown
# workflows_dir = "~/.config/pr-bot/workflows"
# cache_dir = "~/.cache/pr-bot"
# state_file = "~/.config/pr-bot/state.json"
# poll_interval_secs = 300
# health_check_interval_secs = 60
# health_check_grace_period_secs = 1209600 # 14 days
# task_timeout_secs = 1800
# max_concurrent = 3
# model = "anthropic/claude-sonnet-4"
# poll_mentions = true

[attribution]
enabled = true
```

- `bot_username` — the GitHub account the bot runs as
- `authorized_user` — the human whose commands the bot will listen to (gates everything)
- `workflows_dir` — where workflow `.md` files live
- `poll_mentions` — enable @mention scanning via GitHub search + comment-thread scanning
- `health_check_interval_secs` — how often health checks run between normal polling cycles
- `health_check_grace_period_secs` — minimum age before health checks follow up on recent work
- `attribution` — append a configurable LLM attribution signature to agent-authored comments and commits

### Workflow files

Place `.md` workflow files in your workflows directory. Four are included:

- `health-check.md` — follows up on stalled PRs and assigned issues
- `mention.md` — handles @mentions anywhere in issues/PRs
- `new-issue.md` — handles newly assigned issues
- `pr-feedback.md` — handles review feedback on the bot's open PRs

You can extend or replace these to customize agent behavior.

### Run

```bash
PR_BOT_CONFIG=~/.config/pr-bot/config.toml cargo run --release
```

Or set `PR_BOT_CONFIG` in your environment and run the binary directly.
