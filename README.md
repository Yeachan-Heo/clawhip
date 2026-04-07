# clawhip

<p align="center">
  <img src="assets/clawhip-mascot.jpg" width="400" alt="clawhip mascot" />
</p>

<p align="center">
  <a href="https://crates.io/crates/clawhip"><img src="https://img.shields.io/crates/v/clawhip.svg" alt="crates.io" /></a>
  <a href="https://github.com/Yeachan-Heo/clawhip/stargazers"><img src="https://img.shields.io/github/stars/Yeachan-Heo/clawhip?style=social" alt="GitHub stars" /></a>
</p>

> **⭐ Optional support:** the interactive repo-local install paths (`./install.sh` and `clawhip install` from a clone) can offer to star this repo after a successful install when `gh` is installed and authenticated. Skip it with `--skip-star-prompt` or `CLAWHIP_SKIP_STAR_PROMPT=1`.

clawhip is a daemon-first Discord notification router with a typed event pipeline, extracted sources, and a clean renderer/sink split.

Human install pitch:

```text
Just tag @openclaw and say: install this https://github.com/Yeachan-Heo/clawhip
```

Then OpenClaw should:
- clone the repo
- run `install.sh`
- read `SKILL.md` and attach the skill
- scaffold config / presets
- start the daemon
- run live verification for issue / PR / git / tmux / install flows

## What shipped in v0.4.0

- **Install lifecycle polish** — repo-local installs, `clawhip install`, `clawhip update`, and `clawhip uninstall` are documented and aligned for clone-local operator workflows.
- **Optional GitHub support prompt** — interactive install flows can offer an explicit opt-in GitHub star prompt, with `--skip-star-prompt` and `CLAWHIP_SKIP_STAR_PROMPT=1` available on both installer surfaces.
- **Filesystem memory scaffolds** — `clawhip memory init` and `clawhip memory status` bootstrap and inspect the filesystem-offloaded memory layout for repos and workspaces.
- **Native session contract polish** — OMC/OMX payload normalization now prefers the lower-noise `session.*` route family while keeping legacy `agent.*` compatibility.
- **Config compatibility** — `[providers.discord]` remains the preferred config surface, while legacy `[discord]` still loads.

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the release architecture that ships in v0.4.0.

## Good to use together

clawhip pairs well with coding session tools that run in tmux:

### [OMX (oh-my-codex)](https://github.com/Yeachan-Heo/oh-my-codex)

OpenAI Codex wrapper with auto-monitoring. Launch monitored coding sessions:

```bash
clawhip tmux new -s issue-123 \
  --channel YOUR_CHANNEL_ID \
  --mention "<@your-user-id>" \
  --keywords "error,PR created,complete" \
  -- 'source ~/.zshrc && omx --madmax'

# or attach monitoring to an existing tmux session
clawhip tmux watch -s issue-123 \
  --channel YOUR_CHANNEL_ID \
  --mention "<@your-user-id>" \
  --keywords "error,PR created,complete"

# inspect active daemon-known watches later
clawhip tmux list
```

See [`skills/omx/`](skills/omx/) for ready-to-use scripts.
Recommended default clawhip + OMX setup: install the native bridge from [`integrations/omx/`](integrations/omx/) and forward the frozen hook envelope via `clawhip omx hook` when the CLI is available, with `POST /api/omx/hook` as the daemon fallback.
Native OMC/OMX routing now prefers the normalized [`session.*` contract](docs/native-event-contract.md); legacy `agent.*` wrapper emits remain supported for compatibility only.

### [OMC (oh-my-claudecode)](https://github.com/Yeachan-Heo/oh-my-claudecode)

Claude Code wrapper with auto-monitoring. Launch monitored coding sessions:

```bash
clawhip tmux new -s issue-456 \
  --channel YOUR_CHANNEL_ID \
  --mention "<@your-user-id>" \
  --keywords "error,PR created,complete" \
  -- 'source ~/.zshrc && omc --openclaw --madmax'
```

See [`skills/omc/`](skills/omc/) for ready-to-use scripts.
Direct Slack/Discord notifications inside OMC/OMX should be treated as deprecated; emit native events and let clawhip own routing, mention policy, and formatting.

#### Gajae operator setup → verify → fix

For OMC/OMX-integrated setups, clawhip is the source of truth for routing doctrine and troubleshooting. Keep session skills focused on launch mechanics, then use clawhip docs for the operational rails:

- quick entrypoint: this README section
- detailed OMX runbook: [`integrations/omx/README.md`](integrations/omx/README.md)
- native routing/reference contract: [`docs/native-event-contract.md`](docs/native-event-contract.md)

**Setup**
1. Confirm the daemon you plan to use is the one you expect:
   ```bash
   clawhip --version
   clawhip status
   ```
2. For OMX, install the native hook bridge into the target workspace, then validate it:
   ```bash
   ./integrations/omx/install-hook.sh /path/to/repo/.omx/hooks
   omx hooks validate
   omx hooks test
   ```
3. Add an explicit native session route. Use `event = "session.*"` and filter on `repo_name`, not `repo`:
   ```toml
   [[routes]]
   event = "session.*"
   filter = { tool = "omx", repo_name = "clawhip" }
   channel = "1480171113253175356"
   format = "compact"
   ```
   For OMC-native session traffic, keep the same event family and switch the tool filter to `omc` when needed.
4. Leave `[defaults].channel` configured only as a fallback safety net, not as your primary session-routing policy.

**Verify**
- Trigger a real OMC/OMX session event and confirm it lands in the intended route channel.
- If the event lands in the default channel instead, treat that as a route miss first.
- If `clawhip cron run` is part of your operator flow, verify `[[cron.jobs]]` exists before treating the command as meaningful.

**Fix**
- **daemon version/runtime mismatch** → reinstall or update clawhip, then restart the daemon so the running binary matches the hook/runtime you just installed.
- **hook installed but no native session delivery** → verify both the hook install and a matching `session.*` route exist.
- **wrong repo filter** → replace `repo = "..."` with `repo_name = "..."` for native session routes.

## Recipes

### Dev-channel follow-up cron for Clawdbot

One practical pattern is:

```text
system cron -> clawhip send -> Discord dev channel -> Clawdbot follows up on open PRs/issues
```

This works well when you want a lightweight scheduler that nudges your dev channels every 30 minutes without keeping a gateway/LLM session open just for reminders.

Example follow-up script:

```bash
#!/usr/bin/env bash
set -euo pipefail

# dev-followup.sh
# Send a periodic follow-up to active dev channels.

CHANNELS=(
  "1480171113253175356|clawhip"
  "1480171113253175357|gaebal-gajae-api"
  "1480171113253175358|worker-ops"
)

MENTION="<@1465264645320474637>"

for entry in "${CHANNELS[@]}"; do
  IFS='|' read -r channel_id project_name <<< "$entry"

  clawhip send \
    --channel "$channel_id" \
    --message "🔄 **[$project_name] Dev follow-up** $MENTION — check open PRs/issues, review open blockers, merge anything ready, and continue any stalled work."
done
```

You can also send one-off nudges manually:

```bash
clawhip send \
  --channel 1480171113253175356 \
  --message "🔄 **[clawhip] Dev follow-up** <@1465264645320474637> — check open PRs/issues, review blockers, and continue anything stalled."

clawhip send \
  --channel 1480171113253175357 \
  --message "🔄 **[gaebal-gajae-api] PR sweep** <@1465264645320474637> — review open PRs, merge anything ready, and post blockers on anything stuck."
```

Example system cron config:

```crontab
SHELL=/bin/bash
PATH=/usr/local/bin:/usr/bin:/bin

*/30 * * * * bellman /home/bellman/bin/dev-followup.sh >> /tmp/dev-followup.log 2>&1
```

Operational notes:
- keep one channel entry per active repo/project
- mention your Clawdbot/OpenClaw bot user so the bot actually wakes up and acts
- use plain operational language like "check open PRs/issues", "review blockers", and "continue stalled work"
- this keeps scheduling outside the agent loop: cron handles timing, clawhip handles delivery, Discord handles the handoff
- if you want `clawhip cron run` to do anything useful, define `[[cron.jobs]]` first; an empty cron section gives clawhip nothing meaningful to execute

## Filesystem-offloaded memory pattern

clawhip now documents a Claw OS-style memory pattern where `MEMORY.md` is the hot pointer/index layer and detailed memory lives in structured filesystem shards under `memory/`.

Use this when you want:

- a small, fast memory surface for agents
- durable project/channel/daily memory in files
- explicit read/write routing instead of one giant note
- ongoing memory refactoring as part of operations

Start here:

- [docs/memory-offload-architecture.md](docs/memory-offload-architecture.md)
- [docs/memory-offload-guide.md](docs/memory-offload-guide.md)
- [docs/examples/MEMORY.example.md](docs/examples/MEMORY.example.md)
- [skills/memory-offload/SKILL.md](skills/memory-offload/SKILL.md)

Runtime support now includes a small bootstrap/inspection surface:

```bash
# create a scaffold in the current repo
clawhip memory init --project clawhip --channel discord-alerts --agent codex

# inspect whether the expected files and directories exist
clawhip memory status --project clawhip --channel discord-alerts --agent codex
```

`clawhip memory init` creates `MEMORY.md`, `memory/README.md`, a daily shard, a project shard,
topic files, and optional channel/agent shards without overwriting existing files unless you pass
`--force`.

## Plugin architecture

clawhip now includes a simple `plugins/` directory for tool-specific shell bridges.
Each plugin lives in its own subdirectory with:

- `plugin.toml` for lightweight metadata
- `bridge.sh` for shell hook entrypoints

Built-in starter plugins:

- `plugins/codex/`
- `plugins/claude-code/`

Native OMX hook-forwarding assets are bundled separately under:

- `integrations/omx/`

List installed plugins with:

```bash
clawhip plugin list
```

## Description

Operational spec for OpenClaw / Clawdbot agents consuming this repo.

Repo role:
- executable/runtime repo
- daemon-first Discord notification gateway
- typed event routing runtime
- extracted Git/GitHub/tmux source monitors
- multi-delivery router with renderer/sink separation
- lifecycle surface: install / update / uninstall / start / status

Attachment model:
- repo = runtime
- `SKILL.md` = attach/install/usage instructions for OpenClaw / Clawdbot
- README = agent-facing operational spec

Default daemon endpoint:
- `http://127.0.0.1:25294`

## OpenClaw install contract

Primary install entrypoint:

```text
@openclaw install <repo_url>
```

Expected downstream automation behavior:

1. clone repo
2. run `./install.sh`
3. read `SKILL.md`
4. attach skill
5. scaffold config / presets
6. start daemon
7. run live verification

Reference repo URL:

```text
https://github.com/Yeachan-Heo/clawhip
```

## Discord bot setup

> **Recommended:** Create a dedicated Discord bot for clawhip. Do not reuse your Clawdbot / OpenClaw bot token.

clawhip sends high-volume notifications (commits, PRs, tmux keyword alerts, stale warnings). Using a separate bot keeps these cleanly separated from your AI chat bot.

1. Create a new bot at [Discord Developer Portal](https://discord.com/developers/applications) (e.g. "CCNotifier")
2. Copy the bot token
3. Invite to your server with **Send Messages** permission
4. Set the token in config:

```toml
[providers.discord]
token = "your-dedicated-clawhip-bot-token"
default_channel = "your-default-channel-id"

[dispatch]
routine_batch_window_secs = 5
ci_batch_window_secs = 300
```

Legacy `[discord]` config is still accepted and normalized at load time.

`[dispatch].routine_batch_window_secs` controls the default Discord-only routine burst batch window. Leave it unset to keep the 5-second default, or set it to `0` to disable routine batching entirely. In v1, grouped routine bursts suppress route/event mentions for 2+ items, while explicit failure/stale/CI paths still bypass the routine batcher.

`[dispatch].ci_batch_window_secs` controls how long clawhip waits before flushing a GitHub CI batch summary. Leave it unset to keep the 30-second default, or increase it for longer workflows that finish jobs over several minutes.

## Discord webhook setup

Webhook mode works without a bot token.

Quick start:

```bash
clawhip setup --webhook "https://discord.com/api/webhooks/..."
```

Route example:

```toml
[[routes]]
event = "tmux.keyword"
sink = "discord"
webhook = "https://discord.com/api/webhooks/..."
```

## Slack webhook setup

Slack webhook routes work without a bot token.

1. In Slack, open the app settings for your workspace and enable **Incoming Webhooks**
2. Add a new webhook to the channel you want clawhip to notify
3. Copy the generated `https://hooks.slack.com/services/...` URL into a route

Route examples:

```toml
[[routes]]
event = "git.commit"
filter = { repo = "my-app" }
slack_webhook = "https://hooks.slack.com/services/T.../B.../xxx"
format = "compact"

[[routes]]
event = "tmux.keyword"
sink = "slack"
webhook = "https://hooks.slack.com/services/T.../B.../yyy"
format = "alert"
```

## System model

```text
[CLI / webhook / git / GitHub / tmux]
              -> [sources]
              -> [mpsc queue]
              -> [dispatcher]
              -> [router -> renderer -> Discord/Slack sink]
              -> [Discord REST / Slack webhook delivery]
```

Input sources in v0.4.0:
- CLI thin clients and custom events
- GitHub webhook ingress plus GitHub polling source
- git monitor source
- tmux monitor source
- `clawhip tmux new` / `clawhip tmux watch` registration path

## Input -> behavior -> verification

### 1. Custom client event

Input:
```bash
clawhip send --channel <id> --message "text"
```

Behavior:
- POST to daemon `/api/event`
- daemon routes event
- Discord message emitted

Verification:
- `clawhip status`
- inspect configured Discord channel for rendered payload

### 2. GitHub issue preset family

Input:
- GitHub webhook `issues.opened`
- built-in GitHub issue monitor detection
- CLI thin client `clawhip github issue-opened ...`

Behavior:
- emit `github.issue-opened`
- route via `github.*`
- apply repo filter
- prepend route mention if configured
- send to Discord

Verification:
- create real issue
- confirm final Discord body contains:
  - repo
  - issue number
  - title
  - mention when configured

### 3. GitHub issue commented preset

Input:
- GitHub webhook `issue_comment.created`
- built-in GitHub issue monitor comment delta

Behavior:
- emit `github.issue-commented`
- route via `github.*`
- apply repo filter
- prepend route mention if configured

Verification:
- add real issue comment
- confirm final Discord message body in target channel

### 4. GitHub issue closed preset

Input:
- GitHub webhook `issues.closed`
- built-in GitHub issue monitor state transition

Behavior:
- emit `github.issue-closed`
- route via `github.*`
- apply repo filter
- prepend route mention if configured

Verification:
- close real issue
- confirm final Discord message body in target channel

### 5. GitHub PR preset family

Input:
- GitHub webhook `pull_request.*`
- built-in PR monitor state changes
- CLI thin client `clawhip github pr-status-changed ...`

Behavior:
- emit `github.pr-status-changed`
- route via `github.*`
- apply repo filter
- prepend route mention if configured

Verification:
- open real PR
- merge / close PR
- confirm final Discord message body in target channel

### 6. Git commit preset family

Input:
- built-in git monitor polling local repo
- CLI thin client `clawhip git commit ...`

Behavior:
- emit `git.commit`
- route through git/github family matching
- preserve repo-based route filtering
- prepend route mention if configured

Verification:
- create real empty commit in monitored repo
- confirm final Discord body contains commit summary and mention

### 7. Native OMC / OMX session contract

Canonical native routing for OMC/OMX uses `session.*` events after clawhip normalization. For new OMX integrations, the default/recommended path is the native bridge in [`integrations/omx/`](integrations/omx/) forwarding the frozen v1 envelope to `clawhip omx hook` or `POST /api/omx/hook`; keep `agent.*` wrapper emits only for compatibility.

Accepted upstream inputs:
- legacy wrapper emits like `agent.started` / `agent.finished` / `agent.failed`
- OMC command/HTTP payloads with `signal.routeKey`
- OMX hook payloads with `context.normalized_event`
- native OMX hook-envelope CLI ingress via `clawhip omx hook`
- native OMX hook-envelope POSTs to `POST /api/omx/hook`

Canonical normalized events:
- `session.started`
- `session.blocked`
- `session.finished`
- `session.failed`
- `session.retry-needed`
- `session.pr-created`
- `session.test-started`
- `session.test-finished`
- `session.test-failed`
- `session.handoff-needed`

Normalized metadata (when upstream provides it):
- `tool`
- `session_name`
- `session_id`
- `repo_name`
- `repo_path`
- `worktree_path`
- `branch`
- `issue_number`
- `pr_number`
- `pr_url`
- `command`
- `tool_name`
- `test_runner`
- `status`
- `summary`
- `error_message`
- `event_timestamp`
- `raw_event`
- `contract_event`

Route guidance:
- prefer `session.*` for new native OMC/OMX routes
- `agent.*` remains supported for clawhip-local wrapper compatibility
- `agent.started|blocked|finished|failed` and `session.started|blocked|finished|failed` cross-match in routing for backward compatibility
- prefer route filters like `tool`, `repo_name`, `session_name`, `issue_number`, and `branch` over brittle message parsing
- for OMX hook plugins, prefer forwarding the frozen v1 envelope to `POST /api/omx/hook` instead of translating it into a generic `/event` payload

Native OMX bridge assets live in [`integrations/omx/`](integrations/omx/).

See [`docs/native-event-contract.md`](docs/native-event-contract.md) for the full normalization/deprecation notes.
See [`integrations/omx/README.md`](integrations/omx/README.md) for the OMX-native SDK/helper surface.

### 8. Agent lifecycle preset family

Input:
```bash
clawhip agent started --name worker-1 --session sess-123 --project my-repo
clawhip agent blocked --name worker-1 --summary "waiting for review"
clawhip agent finished --name worker-1 --elapsed 300 --summary "PR created"
clawhip agent failed --name worker-1 --error "build failed"
```

Behavior:
- emit `agent.started`, `agent.blocked`, `agent.finished`, or `agent.failed`
- route via `agent.*`
- apply optional project/session filters
- include status / elapsed / summary / error details in rendered messages
- prepend route mention if configured

Verification:
- send each CLI event against a running daemon
- confirm final Discord body contains agent name and lifecycle state
- confirm `agent.*` route rules match each event type

### 9. tmux keyword preset

Input:
- built-in tmux monitor detects configured keyword
- CLI thin client `clawhip tmux keyword ...`

Behavior:
- emit `tmux.keyword`
- route via `tmux.*`
- prepend route mention if configured

Verification:
- print configured keyword in real monitored tmux session
- confirm final Discord body in target channel

### 10. tmux stale preset

Input:
- built-in tmux stale detection
- CLI thin client `clawhip tmux stale ...`

Behavior:
- emit `tmux.stale`
- route via `tmux.*`
- prepend route mention if configured

Verification:
- let real tmux session idle past threshold
- confirm final Discord body in target channel

### 11. tmux wrapper / watch preset

Input:
```bash
clawhip tmux new -s <session> \
  --channel <id> \
  --mention '<@id>' \
  --keywords 'error,PR created,FAILED,complete' \
  --stale-minutes 10 \
  --format alert \
  --retry-enter true \
  --retry-enter-count 4 \
  --retry-enter-delay-ms 250 \
  --shell /bin/zsh \
  -- command args

clawhip tmux watch -s <existing-session> \
  --channel <id> \
  --mention '<@id>' \
  --keywords 'error,PR created,FAILED,complete' \
  --stale-minutes 10 \
  --format alert \
  --retry-enter true

clawhip tmux list
```

Behavior:
- `tmux new` creates a tmux session using the user's default shell (or `--shell` override)
- `tmux new` sends the requested command into the session, retrying Enter for TUI apps by default with exponential backoff (`--retry-enter=false` disables it, `--retry-enter-count` / `--retry-enter-delay-ms` tune retries)
- when `tmux new` omits `--channel`, clawhip reuses the first matching Discord route channel for the session name before falling back to `defaults.channel`
- `tmux watch` attaches monitoring to an already-running tmux session
- both commands register the session with the daemon and emit an audit log line with launch options, timestamp, and parent-process provenance
- `tmux list` shows active daemon-known watches with source, registration timestamp, and parent-process info
- daemon monitors keyword/stale events
- final delivery goes through daemon routing

Verification:
- run wrapper or watch an existing session
- emit keyword in pane
- confirm Discord message body and mention

### 13. tmux snapshot summarization preset

Input:
- built-in config monitor with `summarize = true` on a `[[monitors.tmux.sessions]]` entry

Behavior:
- on pane content change, spawn a non-blocking background task (does NOT stall the poll loop)
- call the configured summarizer backend
- emit `tmux.content_changed` with `content_mode = "raw"` or `"summary"` depending on backend
- `raw` mode: edits a single living Discord message per session with the latest terminal output
- `summary` mode: posts a new Discord message per AI summary (historical record)
- `tmux.heartbeat`: edits the same Discord message per session while session is idle
- `tmux.waiting_for_input`: edits the same Discord message per session while waiting for user input

Verification:
- generate ≥ `min_new_lines` new lines in the monitored session
- confirm `📋` (raw) or `🤖` (AI summary) message in target channel
- confirm heartbeat `💓` message updates in-place after idle interval
- confirm `⏳` waiting message updates in-place when session blocks on input

#### Session config reference

```toml
[[monitors.tmux.sessions]]
session = "my-session"
channel = "YOUR_CHANNEL_ID"

# ── Summarization ─────────────────────────────────────────────────────────────
summarize = true                        # enable snapshot summarization
summarizer = "gemini:gemini-2.5-flash"  # see backend options below
summarize_interval_mins = 5             # min minutes between LLM calls (0 = no throttle)
min_new_lines = 3                       # min net new lines before triggering (0 = no filter)

# ── Input detection ───────────────────────────────────────────────────────────
detect_waiting = true    # emit tmux.waiting_for_input when session blocks on input

# ── Heartbeat ─────────────────────────────────────────────────────────────────
heartbeat_mins = 5       # emit tmux.heartbeat after this many idle minutes (0 = disabled)

# ── Keywords (real-time, always fires regardless of summarize settings) ────────
keywords = ["error", "complete", "done", "fail"]
keyword_window_secs = 30  # debounce window; deduplicates across flushes
```

#### Summarizer backends

| Config value | Description | Requires |
|---|---|---|
| `raw` | Passes latest terminal output directly (no LLM) | nothing |
| `gemini` / `gemini:<model>` | Gemini CLI subprocess | `gemini` CLI in PATH |
| `openrouter:<model>` | OpenRouter API | `OPENROUTER_API_KEY` env var |
| `openai:<model>` | OpenAI or any OpenAI-compatible endpoint | `OPENAI_API_KEY` env var |
| `openai-compatible:<model>` | Same as `openai:` | `OPENAI_API_KEY` env var |

For non-OpenRouter OpenAI-compatible endpoints (xAI Grok, Together AI, Ollama, etc.), set `OPENAI_BASE_URL` before starting the daemon:

```bash
export OPENAI_BASE_URL="https://api.x.ai/v1"
export OPENAI_API_KEY="your-key"
# config: summarizer = "openai:grok-3"
```

Default models when no model is specified: Gemini → `gemini-2.5-flash`, OpenRouter → `openai/gpt-4o-mini`, OpenAI → `gpt-4o-mini`.

#### Discord delivery behavior

| Event | Discord behavior | Key |
|---|---|---|
| `tmux.content_changed` (raw) | **Edit in-place** | one message per session |
| `tmux.content_changed` (AI summary) | **New message** | historical record |
| `tmux.heartbeat` | **Edit in-place** | one message per session |
| `tmux.waiting_for_input` | **Edit in-place** | one message per session |
| `tmux.keyword` | **New message** | real-time, deduped across windows |

Keywords fire in real-time regardless of `summarize_interval_mins`. The same `(keyword, line)` pair is never repeated across consecutive keyword messages.

### 12. install lifecycle preset

Input:
```bash
./install.sh
clawhip install
clawhip update --restart
clawhip uninstall --remove-systemd --remove-config
```

Behavior:
- install binary from git clone
- ensure config dir exists
- optional systemd install
- optional post-install GitHub star prompt on interactive local installs
- update rebuilds/reinstalls and optionally restarts daemon
- uninstall removes runtime artifacts

Verification:
- `clawhip --help`
- `clawhip status`
- `systemctl status clawhip` when systemd-enabled

## Preset event families

### GitHub family
- `github.issue-opened`
- `github.issue-commented`
- `github.issue-closed`
- `github.pr-status-changed`

### Git family
- `git.commit`
- `git.branch-changed`

### Agent family
- `agent.started`
- `agent.blocked`
- `agent.finished`
- `agent.failed`

### Native session family
- `session.started`
- `session.blocked`
- `session.finished`
- `session.failed`
- `session.retry-needed`
- `session.pr-created`
- `session.test-started`
- `session.test-finished`
- `session.test-failed`
- `session.handoff-needed`

### tmux family
- `tmux.keyword`
- `tmux.stale`
- `tmux.content_changed` — pane content changed; carries `content_mode: "raw" | "summary"`
- `tmux.heartbeat` — session idle for configured interval; **edits** the previous heartbeat message in-place
- `tmux.waiting_for_input` — session is blocking on user input; **edits** previous waiting message in-place

## Route contract

Config file:

```text
~/.clawhip/config.toml
```

Route model:

```toml
[[routes]]
event = "github.*"
filter = { repo = "clawhip" }
sink = "discord"
channel = "1480171113253175356"
mention = "<@1465264645320474637>"
format = "compact"
allow_dynamic_tokens = false

[[routes]]
event = "session.*"
filter = { tool = "omx", repo_name = "clawhip" }
sink = "discord"
channel = "1480171113253175356"
format = "compact"
allow_dynamic_tokens = false

[[routes]]
event = "agent.*"
filter = { project = "clawhip" }
sink = "discord"
channel = "1480171113253175356"
format = "alert"
allow_dynamic_tokens = false
```

Resolution rules:
1. event family match
2. payload filter match
3. route sink / target / format / template / mention applied
4. default fallback used if route fields absent

## Dynamic token contract

Only for routes with:

```toml
allow_dynamic_tokens = true
```

Supported tokens:
- `{repo}`
- `{number}`
- `{title}`
- `{session}`
- `{keyword}`
- `{sh:...}`
- `{tmux_tail:session:lines}`
- `{file_tail:/path:lines}`
- `{env:NAME}`
- `{now}`
- `{iso_time}`

Safety:
- allowlisted token kinds only
- route-level opt-in only
- short timeout
- output cap

## Install surface

### From crates.io

```bash
cargo install clawhip
```

Published at [crates.io/crates/clawhip](https://crates.io/crates/clawhip). Requires Rust toolchain.

### Prebuilt binary installer (recommended, no Rust needed)

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Yeachan-Heo/clawhip/releases/latest/download/clawhip-installer.sh | sh
```

This installs the latest prebuilt `clawhip` binary from GitHub Releases into `$CARGO_HOME/bin` (typically `~/.cargo/bin`).

Release artifacts are generated for these Rust target triples: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, and `aarch64-apple-darwin`.

### Repo-local install

```bash
./install.sh
./install.sh --systemd
```

`install.sh` now tries the latest prebuilt release first and falls back to `cargo install --path . --force` when a matching release asset is unavailable. If Cargo is needed for the fallback path but not installed, the script prints Rustup setup instructions. When `--systemd` is used, the installed binary is also copied to `/usr/local/bin/clawhip` so the bundled service unit can start it.

In interactive terminals, both the repo-local installer and `clawhip install` may offer an optional post-install GitHub star prompt via authenticated `gh api` access. It never runs automatically, is skipped when `gh` is missing or unauthenticated, and can be disabled with `./install.sh --skip-star-prompt`, `clawhip install --skip-star-prompt`, or `CLAWHIP_SKIP_STAR_PROMPT=1`.

### Runtime lifecycle commands

```bash
clawhip install
clawhip install --systemd
clawhip install --skip-star-prompt
clawhip update --restart
clawhip uninstall
clawhip uninstall --remove-systemd --remove-config
```

`clawhip install` now matches the repo-local installer's optional GitHub star prompt behavior: it only appears in interactive terminals, is skipped when `gh` is missing or unauthenticated, never stars automatically, and can be disabled with `clawhip install --skip-star-prompt` or `CLAWHIP_SKIP_STAR_PROMPT=1 clawhip install`.

## systemd contract

Unit file:

```text
deploy/clawhip.service
```

Expected install path:
- copy to `/etc/systemd/system/clawhip.service`
- `systemctl daemon-reload`
- `systemctl enable --now clawhip`

## Live verification runbook

Use:
- `docs/live-verification.md`
- `scripts/live-verify-default-presets.sh`

Required live sign-off presets:
- issue opened
- issue commented
- issue closed
- PR opened
- PR status changed
- PR merged
- git commit
- agent started / blocked / finished / failed
- tmux keyword
- tmux stale
- tmux wrapper
- tmux watch
- install/update/uninstall

## Complete config reference

Config file: `~/.clawhip/config.toml` (override with `CLAWHIP_CONFIG=/path/to/config.toml`)

### Top-level structure

```toml
[providers.discord]    # Discord bot token and default channel
[providers.gemini]     # Gemini API key (alternative to env var)
[providers.openrouter] # OpenRouter API key (alternative to env var)
[providers.openai]     # OpenAI API key and base URL (alternative to env vars)
[daemon]               # Bind host, port, base URL
[defaults]             # Fallback channel and format
[dispatch]             # Batch window tuning
[[routes]]             # Event routing rules (one per [[routes]] block)
[monitors]             # Poll interval, GitHub token, git/tmux/workspace sources
[[monitors.git.repos]] # Git repo monitors (one per block)
[[monitors.tmux.sessions]] # Tmux session monitors (one per block)
[[monitors.workspace]] # Workspace file-change monitors (one per block)
[cron]                 # Cron scheduler config
[[cron.jobs]]          # Cron jobs (one per block)
```

### `[providers.discord]`

```toml
[providers.discord]
token = "Bot-token-here"
default_channel = "1480171113253175356"
```

Legacy `[discord]` is accepted and merged at load time. Conflicting values between `[discord]` and `[providers.discord]` are a load error.

Token resolution order: config file → `DISCORD_TOKEN` → `CLAWHIP_DISCORD_BOT_TOKEN` → `DISCORD_BOT_TOKEN`.

### `[providers.gemini]`, `[providers.openrouter]`, `[providers.openai]`

Store API keys in config instead of (or in addition to) environment variables:

```toml
[providers.gemini]
api_key = "AIza..."

[providers.openrouter]
api_key = "sk-or-..."

[providers.openai]
api_key = "sk-..."
base_url = "https://api.x.ai/v1"  # optional; override for xAI, Ollama, etc.
```

Environment variable equivalents: `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`.

### `[daemon]`

```toml
[daemon]
bind_host = "0.0.0.0"        # default: 0.0.0.0
port = 25294                  # default: 25294
base_url = "http://127.0.0.1:25294"  # default: http://127.0.0.1:<port>
```

`base_url` is used by thin-client CLI commands to reach the daemon. Change it when running the daemon on a remote host or non-default port.

### `[defaults]`

```toml
[defaults]
channel = "1480171113253175356"  # fallback channel when no route matches
format = "compact"               # fallback format: "compact" | "alert" | "raw" (default: compact)
```

### `[dispatch]`

```toml
[dispatch]
routine_batch_window_secs = 5    # Discord routine burst batch window (default: 5; 0 = disable)
ci_batch_window_secs = 30        # GitHub CI batch flush window (default: 30)
```

`routine_batch_window_secs`: groups routine deliveries (commits, keywords, agent events) into a single Discord message within the window. Set to `0` to disable batching entirely. Mentions are suppressed when 2+ items are batched together.

`ci_batch_window_secs`: how long clawhip waits before flushing a GitHub CI job batch. Increase for workflows with jobs that run over several minutes.

### `[[routes]]`

```toml
[[routes]]
event = "tmux.*"               # required; glob matched against event kind
filter = { session = "dev" }   # optional; payload key/value filters
sink = "discord"               # "discord" (default) | "slack"
channel = "1480171113253175356" # Discord channel ID (bot-token mode)
webhook = "https://discord.com/api/webhooks/..."  # Discord webhook (no token needed)
slack_webhook = "https://hooks.slack.com/services/..."  # Slack webhook
mention = "<@1465264645320474637>"  # prepended to rendered message
format = "compact"             # "compact" | "alert" | "raw"; overrides defaults.format
template = "{{session}}: {{content}}"  # custom template string; overrides format
allow_dynamic_tokens = false   # enable {sh:...}, {env:...}, {tmux_tail:...} expansion
```

Filter keys for session events: `tool`, `repo_name`, `session_name`, `issue_number`, `branch`.
Filter keys for git/github events: `repo`, `repo_name`.

Routes are evaluated in order; the first match wins. Events with no matching route fall back to `defaults.channel`.

### `[monitors]`

```toml
[monitors]
poll_interval_secs = 5           # global poll interval for all sources (default: 5)
github_token = "ghp_..."         # GitHub API token for issue/PR polling
github_api_base = "https://api.github.com"  # override for GitHub Enterprise
```

`github_token` is also read from the `GITHUB_TOKEN` environment variable.

### `[[monitors.git.repos]]`

```toml
[[monitors.git.repos]]
path = "/home/user/projects/myapp"  # required; local repo path
name = "myapp"                      # optional; used in event payloads and route filters
remote = "origin"                   # default: "origin"
github_repo = "owner/myapp"        # optional; enables GitHub issue/PR polling
emit_commits = true                 # emit git.commit on new commits (default: true)
emit_branch_changes = true          # emit git.branch-changed on branch switches (default: true)
emit_issue_opened = true            # emit github.issue-opened via polling (default: true)
emit_pr_status = false              # emit github.pr-status-changed via polling (default: false)
channel = "1480171113253175356"    # optional; override channel for this repo's events
mention = "<@123>"                  # optional; mention for this repo's events
format = "compact"                  # optional; format override
```

### `[[monitors.tmux.sessions]]`

```toml
[[monitors.tmux.sessions]]
session = "my-session"             # required; tmux session name or glob (e.g. "rcc-*")
channel = "1480171113253175356"    # required; Discord channel for this session's events
mention = "<@123>"                 # optional; mention prepended to events
format = "compact"                 # optional; "compact" | "alert" | "raw"

# ── Keyword detection ─────────────────────────────────────────────────────────
keywords = ["error", "fail", "complete"]  # keywords to watch for in pane output
keyword_window_secs = 30          # dedup window: same (keyword, line) pair suppressed within window (default: 30)

# ── Stale detection ───────────────────────────────────────────────────────────
stale_minutes = 10                # minutes of inactivity before tmux.stale fires (default: 10; 0 = disable)
stale_interval = 0                # alias for stale_minutes; overrides stale_minutes when set

# ── Summarization ─────────────────────────────────────────────────────────────
summarize = false                 # enable pane snapshot summarization (default: false)
summarizer = "gemini:gemini-2.5-flash"  # backend (see table below)
min_new_lines = 0                 # min net new lines required to trigger summarization (0 = no filter)
summarize_interval_mins = 0       # min minutes between LLM calls (0 = no throttle)
summary_interval = 0              # alias for summarize_interval_mins; overrides when set

# ── Heartbeat ─────────────────────────────────────────────────────────────────
heartbeat_mins = 0                # minutes of idle before tmux.heartbeat fires (0 = disable)
heartbeat_interval = 0            # alias for heartbeat_mins; overrides when set

# ── Input detection ───────────────────────────────────────────────────────────
detect_waiting = false            # emit tmux.waiting_for_input when pane blocks on input (default: false)
waiting_interval = 0              # cooldown minutes between waiting alerts (0 = no cooldown)

# ── Mention filtering ─────────────────────────────────────────────────────────
mention_on = []                   # restrict @mention to specific event kinds
                                  # values: "keyword" | "waiting_for_input" | "content_changed"
                                  #         | "stale" | "heartbeat"
                                  # empty = mention applies to all events (default)

# ── Dashboard pinning ─────────────────────────────────────────────────────────
# Dashboard is a set of up to 5 pinned Discord messages edited in-place.
# Enabled automatically when any pin_* = true.
pin_status = true                 # pin status/heartbeat slot (default: true)
pin_summary = true                # pin AI summary slot (default: true)
pin_alerts = true                 # pin waiting-for-input alert slot (default: true)
pin_activity = true               # pin rolling activity log slot (default: true)
pin_keywords = false              # pin rolling keyword hits slot (default: false)
activity_interval = 0             # cooldown minutes between activity log updates (0 = no cooldown)
```

#### Summarizer backends

| Config value | Description | Requires |
|---|---|---|
| `raw` | Latest terminal output verbatim (no LLM) | nothing |
| `gemini` / `gemini:<model>` | Gemini CLI subprocess | `gemini` CLI in PATH |
| `openrouter:<model>` | OpenRouter API | `OPENROUTER_API_KEY` or `[providers.openrouter]` |
| `openai:<model>` | OpenAI API | `OPENAI_API_KEY` or `[providers.openai]` |
| `openai-compatible:<model>` | Any OpenAI-compatible endpoint | `OPENAI_API_KEY` + `OPENAI_BASE_URL` |

Default models when no model suffix is given: Gemini → `gemini-2.5-flash`, OpenRouter → `openai/gpt-4o-mini`, OpenAI → `gpt-4o-mini`.

#### Dashboard delivery behavior

| Event | Discord behavior | Slot |
|---|---|---|
| `tmux.content_changed` (raw) | Edit in-place | summary |
| `tmux.content_changed` (AI summary) | New message | — |
| `tmux.heartbeat` | Edit in-place | status |
| `tmux.waiting_for_input` | Edit in-place | alert |
| `tmux.waiting_resolved` | Edit in-place (✅) | alert |
| `tmux.keyword` (pin_keywords=true) | Edit in-place rolling log | keywords |
| `tmux.keyword` (pin_keywords=false) | New message | — |
| `tmux.stale` | New message | — |
| `tmux.session_ended` | Unpin all slots, clear dashboard | — |

Dashboard message IDs are persisted to `~/.clawhip/dashboard.json` so in-place edits survive daemon restarts.

### `[[monitors.workspace]]`

Watches filesystem paths for file changes and emits `workspace.*` events.

```toml
[[monitors.workspace]]
path = "/home/user/projects/myapp"  # required; root path to watch
watch_dirs = [".omc", ".claude"]    # subdirs to watch (default: [".omc", ".claude"])
discover_worktrees = false           # auto-discover git worktrees under path (default: false)
channel = "1480171113253175356"     # optional; channel override
mention = "<@123>"                   # optional; mention override
format = "compact"                   # optional; format override
events = ["skill.activated"]         # optional; restrict to specific event kinds (empty = all)
poll_interval_secs = 5               # optional; override global poll_interval_secs for this monitor
debounce_ms = 500                    # deduplicate rapid file changes within this window (default: 500)
```

### `[[cron.jobs]]`

Scheduled message delivery using cron expressions.

```toml
[cron]
poll_interval_secs = 30  # how often clawhip checks the cron schedule (default: 30)

[[cron.jobs]]
id = "daily-standup"             # required; unique job identifier
kind = "custom-message"          # required; currently only "custom-message" is supported
schedule = "0 9 * * 1-5"         # required; cron expression (minute hour dom month dow)
timezone = "UTC"                  # default: "UTC" (only UTC accepted; others rejected)
enabled = true                   # default: true; set false to disable without removing
message = "🌅 Morning standup reminder — check open PRs and blockers."
channel = "1480171113253175356"  # optional; override defaults.channel for this job
mention = "<@123>"               # optional; mention prepended to message
format = "compact"               # optional; format override
```

Cron schedule syntax supports standard five-field expressions including lists (`1,3,5`), ranges (`1-5`), and steps (`*/15`).

Job IDs must be unique within the config. Duplicate IDs are rejected at load time.

### Environment variables

| Variable | Purpose |
|---|---|
| `CLAWHIP_CONFIG` | Override config file path |
| `DISCORD_TOKEN` | Discord bot token (highest priority env source) |
| `CLAWHIP_DISCORD_BOT_TOKEN` | Discord bot token (second priority) |
| `DISCORD_BOT_TOKEN` | Discord bot token (third priority) |
| `GITHUB_TOKEN` | GitHub API token for issue/PR polling |
| `GEMINI_API_KEY` | Gemini API key for summarization |
| `OPENROUTER_API_KEY` | OpenRouter API key for summarization |
| `OPENAI_API_KEY` | OpenAI API key for summarization |
| `OPENAI_BASE_URL` | OpenAI-compatible base URL (e.g. xAI, Ollama) |
| `CLAWHIP_SKIP_STAR_PROMPT` | Set to `1` to skip post-install GitHub star prompt |

Config file values take precedence over environment variables for API keys. Discord token resolution always prefers env vars over config.

## Minimal operational commands

```bash
clawhip                 # start daemon
clawhip status          # daemon health
clawhip config          # config management
clawhip send ...        # thin client custom event
clawhip github ...      # thin client GitHub event
clawhip git ...         # thin client git event
clawhip agent ...       # thin client agent lifecycle event
clawhip omx hook ...    # native OMX hook-envelope thin client
clawhip tmux ...        # thin client / wrapper surface
clawhip plugin list     # list installed/bundled shell-hook plugins
```
