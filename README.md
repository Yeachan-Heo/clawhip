# clawhip

<p align="center">
  <img src="assets/clawhip-mascot.jpg" width="400" alt="clawhip mascot" />
</p>

<p align="center">
  <a href="https://crates.io/crates/clawhip"><img src="https://img.shields.io/crates/v/clawhip.svg" alt="crates.io" /></a>
  <a href="https://github.com/Yeachan-Heo/clawhip/stargazers"><img src="https://img.shields.io/github/stars/Yeachan-Heo/clawhip?style=social" alt="GitHub stars" /></a>
</p>

> **⭐ Optional support:** the interactive repo-local install paths (`./install.sh` and `clawhip install` from a clone) can offer to star this repo after a successful install when `gh` is installed and authenticated. Skip it with `--skip-star-prompt` or `CLAWHIP_SKIP_STAR_PROMPT=1`.

**gajae-claw (clawhip) is the control plane for agents:** route events from GitHub, Discord, tmux, and other tools to the right human or agent, record what happened, and separate automatic actions from approval-required actions.

## Start with recipes

Use clawhip when you have an event, a destination, and a policy for whether the next step can happen automatically or needs a human/operator approval. These recipes use placeholder channel IDs and names; replace them with your own public-safe values.

### Recipe 1: PR opened -> notify the maintainer/project channel

When GitHub reports a pull request event, route it to the project channel so the right maintainer sees it.

```toml
# ~/.clawhip/config.toml
[[routes]]
event = "github.pr-status-changed"
filter = { repo = "my-app" }
sink = "discord"
channel = "PROJECT_CHANNEL_ID"
format = "compact"
```

Example event shape:

```json
{
  "source": "github",
  "event": "pull_request.opened",
  "repo": "my-app",
  "pr": 42,
  "action": "notify",
  "target": "PROJECT_CHANNEL_ID"
}
```

Result: clawhip records the routed event and posts a compact PR notification to the project channel.

### Recipe 2: CI failed twice -> summarize and escalate

Let routine CI status flow to the project channel, but reserve an escalation route for repeated failures. Your CI watcher or automation can emit the second-failure event after it observes two failed runs for the same PR or branch.

```toml
# ~/.clawhip/config.toml
[[routes]]
event = "github.ci-failed"
filter = { repo = "my-app" }
sink = "discord"
channel = "PROJECT_CHANNEL_ID"
format = "compact"

[[routes]]
event = "ci.failed-twice"
filter = { repo = "my-app" }
sink = "discord"
channel = "ESCALATION_CHANNEL_ID"
format = "alert"
```

Example escalation payload:

```json
{
  "source": "ci-watcher",
  "event": "ci.failed-twice",
  "repo": "my-app",
  "branch": "feature/auth-flow",
  "summary": "CI failed twice on the same PR; test logs point at integration/auth_test.",
  "action": "summarize_and_escalate",
  "target": "ESCALATION_CHANNEL_ID"
}
```

Result: normal failures stay low-noise; repeated failures get a short summary in the escalation channel.

### Recipe 3: cleanup, merge, or config change requested -> require operator approval

Some requests should notify an operator instead of letting an agent act immediately. Send those events to an approval channel and keep the requested action in the message body.

```toml
# ~/.clawhip/config.toml
[[routes]]
event = "agent.approval-requested"
filter = { repo = "my-app" }
sink = "discord"
channel = "APPROVAL_CHANNEL_ID"
format = "alert"
```

Example approval event:

```json
{
  "source": "agent",
  "event": "agent.approval-requested",
  "repo": "my-app",
  "request": "merge PR #42 after checks pass",
  "reason": "merge changes the shared branch and should be operator-approved",
  "policy": "approval_required",
  "action": "notify_operator",
  "target": "APPROVAL_CHANNEL_ID"
}
```

Result: clawhip records the request and alerts an operator; the agent waits for explicit approval before cleanup, merge, or config-changing work.

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

## Provider-native hooks for Codex + Claude

clawhip no longer treats provider-specific launch wrappers as the public integration surface.
Codex and Claude own session launch plus hook registration; clawhip stays the routing,
normalization, and delivery layer.

Shared v1 hook events:

- `SessionStart`
- `PreToolUse`
- `PostToolUse`
- `UserPromptSubmit`
- `Stop`

Local ingress for sample payloads and manual verification:

```bash
clawhip native hook --provider codex --file payload.json
clawhip native hook --provider claude --file payload.json
cat payload.json | clawhip native hook --provider codex
```

Recommended installation model:

- install the shared clawhip bridge in `~/.clawhip/hooks/native-hook.mjs`
- for Codex, align with the official hook contract: use either `~/.codex/hooks.json` or `<repo>/.codex/hooks.json`
- for Claude Code, install the provider-native hook config globally in `~/.claude/settings.json`
- keep provider config in the provider-owned config files
- keep routing metadata in `.clawhip/project.json`
- use `.clawhip/hooks/` only for additive augmentation such as frontmatter or recent context

clawhip still pairs well with tmux when you want keyword/stale monitoring, but tmux is now
optional and no longer the primary hook-registration surface.

For tmux-backed recovery into an already-running hooked session, use:

```bash
clawhip deliver --session <tmux-session> --prompt "..." --max-enters 4
```

`clawhip deliver` validates repo-local prompt-submit hook setup, confirms the target pane is an
active Codex/Claude (including OMC/OMX wrapper) session, then retries Enter until
`.clawhip/state/prompt-submit.json` changes or the bounded retry limit is reached.

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

Bounded setup presets also support:

```bash
clawhip setup \
  --bot-token "discord-bot-token" \
  --default-channel "1234567890" \
  --default-format alert \
  --daemon-base-url "http://127.0.0.1:25294"
```

`clawhip setup` stays non-interactive and intentionally limited to five presets only:
- Discord webhook quickstart route
- Discord bot token
- Default channel
- Default message format
- Daemon base URL

Advanced routes and monitor definitions are still edited manually in the config file or revisited through the bounded `clawhip config` editor surface.

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

### 7. Provider-native session contract

Canonical native routing now starts from provider-native Codex and Claude hook payloads and
enters clawhip through `clawhip native hook`.

Shared v1 hook events:
- `SessionStart`
- `PreToolUse`
- `PostToolUse`
- `UserPromptSubmit`
- `Stop`

Stable routing metadata (when available):
- `provider`
- `event`
- `session_id`
- `directory`
- `worktree_path`
- `repo_name`
- `project`
- `branch`
- `tool_name`
- `command`
- `summary`
- `event_timestamp`

Augmentation rules:
- provider input + clawhip project metadata define the immutable base contract
- `.clawhip/hooks/` scripts may only add fields or enrich message/context
- augmenters must not remove or overwrite base routing keys

Route guidance:
- prefer filters like `provider`, `event`, `repo_name`, `project`, and `branch`
- avoid route logic that depends on rendered message text
- keep provider-specific extras out of the shared v1 route surface until explicitly adopted

See [`docs/native-event-contract.md`](docs/native-event-contract.md) for the routing/augmentation
guide and [`docs/event-contract-v1.md`](docs/event-contract-v1.md) for the frozen shared-event
reference.

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

### 11. Provider-native sessions + tmux fallback

Preferred input:
```bash
clawhip native hook --provider codex --file payload.json
clawhip native hook --provider claude --file payload.json
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

clawhip deliver \
  --session <existing-session> \
  --prompt "continue from the latest blocker and open a PR to dev" \
  --max-enters 4
```

Behavior:
- Codex and Claude should own session launch and hook registration
- `clawhip native hook` is the local thin-client ingress for provider payloads
- `tmux new` / `tmux watch` are fallback paths for debugging or manual recovery
- `deliver` is the prompt recovery path for an already-running hooked tmux-backed provider session
- `tmux list` shows active daemon-known watches with source, registration timestamp, and parent-process info
- final delivery still goes through daemon routing
- `deliver` refuses arbitrary shells and requires prompt-submit-aware hook setup (`clawhip hooks install --provider codex --scope global|project` for Codex, with the bridge in `~/.clawhip`, or `clawhip hooks install --provider claude-code --scope global` for Claude Code)

Routing note:
- session names are labels for operators, not routing authority
- project metadata should be the source of truth for routing
- broad prefix monitors like `clawhip*` are dangerous because they can overlap with launcher-registered watches and create stale/keyword noise

Verification:
- launch a real Codex or Claude session with provider-native hooks enabled
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
channel = "PROJECT_CHANNEL_ID"
mention = "@maintainer-or-team"
format = "compact"
allow_dynamic_tokens = false

[[routes]]
event = "session.*"
filter = { tool = "omx", repo_name = "clawhip" }
sink = "discord"
channel = "PROJECT_CHANNEL_ID"
format = "compact"
allow_dynamic_tokens = false

[[routes]]
event = "session.*"
filter = { tool = "omx", repo_name = "clawhip", session_id = "sess-123" }
sink = "discord"
thread = "DISCORD_THREAD_ID"
format = "compact"
allow_dynamic_tokens = false

[[routes]]
event = "agent.*"
filter = { project = "clawhip" }
sink = "discord"
channel = "PROJECT_CHANNEL_ID"
format = "alert"
allow_dynamic_tokens = false
```

Resolution rules:
1. event family match
2. payload filter match
3. route sink / target / format / template / mention applied
4. default fallback used if route fields absent

Discord thread targets:
- Use `thread = "DISCORD_THREAD_ID"` on a Discord route to deliver directly into
  a thread. This is explicit; clawhip does not infer threads from `channel` IDs,
  names, session labels, or payload fields.
- A Discord route may set exactly one delivery target: `channel`, `thread`, or
  `webhook`.
- Thread delivery uses the configured thread ID as the Discord message target.
  If Discord reports the thread as archived, missing, forbidden, or otherwise
  unreachable, clawhip records a concise delivery error and does **not**
  automatically fall back to the parent channel.
- Diagnostics and binding verification only reference configured IDs and status
  outcomes; clawhip does not list or dump private channel/thread inventories.

## Gateway allowlist diagnostics

When clawhip sends through a local Clawdbot gateway, route channel IDs must also
be present in the gateway's Discord allowlist. Check that boundary with:

```bash
clawhip config verify-gateway-allowlist
clawhip config verify-gateway-allowlist --gateway-config ~/.clawdbot/clawdbot.json
clawhip config verify-gateway-allowlist --json
```

The default gateway config path is `~/.clawdbot/clawdbot.json` when `HOME` is
available; use `--gateway-config <path>` for any other local file. The command
reads only `channels.discord.guilds[*].channels.<channel_id>.allow = true`,
compares it with clawhip's configured Discord channel destinations, and exits
non-zero when any destination is missing or not explicitly allowed.

Output is public-safe by design: text and JSON reports include counts, clawhip
source labels, and channel IDs only. They do not dump gateway tokens, webhook
URLs, raw config payloads, or unrelated gateway fields. Webhooks, Slack routes,
localfile routes, and thread-only targets are outside this allowlist check.

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
- `scripts/internal-pr-format-gate.sh` for cheap local format gating before internal PR create/update flows

Required live sign-off presets:
- issue opened
- issue commented
- issue closed
- PR opened
- PR status changed
- PR merged
- git commit
- provider-native shared hook events
- tmux keyword
- tmux stale
- tmux wrapper
- tmux watch
- install/update/uninstall

## Minimal operational commands

```bash
clawhip                 # start daemon
clawhip status          # daemon health
clawhip config          # bounded preset editor / config inspection
clawhip config verify-gateway-allowlist  # check Clawdbot gateway allowlist coverage
clawhip send ...        # thin client custom event
clawhip github ...      # thin client GitHub event
clawhip git ...         # thin client git event
clawhip agent ...       # thin client agent lifecycle event
clawhip native hook ... # provider-native hook thin client
clawhip tmux ...        # thin client / wrapper surface
clawhip plugin list     # list installed/bundled shell-hook plugins
```

## Internal PR fast-path

Before opening or updating an internal PR from a Rust worktree, run:

```bash
scripts/internal-pr-format-gate.sh
```

If you already know the tree just needs formatting, auto-fix first:

```bash
scripts/internal-pr-format-gate.sh --fix
```

This catches the cheapest class of red CI (`cargo fmt` only) locally before PR create/update churn.
