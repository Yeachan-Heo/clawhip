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

## What shipped in v0.3.0

- **Typed event model** — incoming events are normalized and validated into typed envelopes before dispatch.
- **Multi-delivery router** — one event can resolve to zero, one, or many deliveries instead of stopping at the first match.
- **Source extraction** — git, GitHub, and tmux monitoring now run as explicit sources feeding the daemon queue.
- **Sink/render split** — rendering is separated from transport; v0.3.0 ships with the Discord sink and default renderer.
- **Config compatibility** — `[providers.discord]` is the preferred config surface, while legacy `[discord]` still loads.

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the release architecture that ships in v0.3.0.

## Good to use together

clawhip pairs well with coding session tools that run in tmux:

### [OMX (oh-my-codex)](https://github.com/Yeachan-Heo/oh-my-codex)

OpenAI Codex wrapper with native clawhip session launch. Launch monitored coding sessions with the native launcher first:

```bash
clawhip omx launch \
  --session clawhip-issue-123-fix-routing \
  --workdir ~/Workspace/clawhip.worktrees/clawhip-issue-123-fix-routing \
  --mention "<@your-user-id>" \
  --skip-hook-check \
  "$ralph Fix issue #123 and open a PR to dev"

# inspect active daemon-known watches later
clawhip tmux list
```

Use `clawhip tmux new` / `clawhip tmux watch` only as fallback or launcher-debugging paths.
See [`skills/omx/`](skills/omx/) for ready-to-use scripts.
Native OMC/OMX routing now prefers the normalized [`session.*` contract](docs/native-event-contract.md); legacy `agent.*` wrapper emits remain supported for compatibility.

### [OMC (oh-my-claudecode)](https://github.com/Yeachan-Heo/oh-my-claudecode)

Claude Code wrapper with native clawhip session launch:

```bash
clawhip omc \
  --session clawhip-issue-456-build-feature \
  --workdir ~/Workspace/clawhip.worktrees/clawhip-issue-456-build-feature \
  --mention "<@your-user-id>" \
  --skip-hook-check \
  "Implement issue #456 and open a PR to dev"
```

Use `clawhip tmux new` only when manually recovering or debugging launcher behavior.
See [`skills/omc/`](skills/omc/) for ready-to-use scripts.
Direct Slack/Discord notifications inside OMC/OMX should be treated as deprecated; emit native events and let clawhip own routing, mention policy, and formatting.

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

Input sources in v0.3.0:
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

Canonical native routing for OMC/OMX uses `session.*` events after clawhip normalization.

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

### 11. Native session launch + tmux fallback

Preferred input:
```bash
clawhip omx launch \
  --session <project-issue-session-name> \
  --workdir <verified-worktree-path> \
  --mention '<@id>' \
  --skip-hook-check \
  "$ralph prompt here"

clawhip omc \
  --session <project-issue-session-name> \
  --workdir <verified-worktree-path> \
  --mention '<@id>' \
  --skip-hook-check \
  "implementation prompt here"

clawhip tmux list
```

Fallback/debug input:
```bash
clawhip tmux new -s <session> \
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
  --mention '<@id>' \
  --keywords 'error,PR created,FAILED,complete' \
  --stale-minutes 10 \
  --format alert \
  --retry-enter true
```

Behavior:
- native `omx launch` / `omc` should be the normal path for coding sessions
- native launch emits project metadata and registers tmux monitoring automatically
- `tmux new` / `tmux watch` are fallback paths for debugging or manual recovery
- `tmux list` shows active daemon-known watches with source, registration timestamp, and parent-process info
- final delivery still goes through daemon routing

Routing note:
- session names are labels for operators, not routing authority
- project metadata should be the source of truth for routing
- broad prefix monitors like `clawhip*` are dangerous because they can overlap with launcher-registered watches and create stale/keyword noise

Verification:
- launch a native OMC/OMX session
- verify the tmux pane is actually alive
- confirm routed delivery in Discord
- if alert text conflicts with pane reality, trust the pane and inspect monitor registrations

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
