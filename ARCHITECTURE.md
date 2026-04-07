# clawhip Architecture — v0.5.x

clawhip is a daemon-first event pipeline for Discord and Slack delivery. This document reflects the architecture on the current development branch.

## Release themes

- typed event model with normalized session contract
- multi-delivery router (Discord bot, Discord webhook, Slack webhook)
- extracted event sources (git, GitHub, tmux, workspace, cron)
- renderer/sink separation
- tmux snapshot summarization with pluggable LLM backends
- dashboard pinning: up to 5 per-session in-place Discord messages
- waiting-for-input detection and resolved state machine
- batch dispatch with per-stream windows

## High-level flow

```text
[CLI / webhook / git / GitHub / tmux / workspace / cron]
              -> [sources]
              -> [mpsc queue]
              -> [dispatcher + batcher]
              -> [router -> renderer -> discord/slack sink]
              -> [Discord REST / Slack webhook delivery]
```

## Core components

### Typed event model (`crate::event`)

The daemon accepts legacy `IncomingEvent` payloads at ingress and normalizes them through `crate::event::compat` into typed internal events. The canonical event families are:

- custom events
- git: `git.commit`, `git.branch-changed`
- github: `github.issue-opened`, `github.issue-commented`, `github.issue-closed`, `github.pr-status-changed`, `github.ci-started`, `github.ci-passed`, `github.ci-failed`, `github.ci-cancelled`
- agent/session: `agent.*` (legacy) and `session.*` (native contract)
- tmux: `tmux.keyword`, `tmux.stale`, `tmux.content_changed`, `tmux.heartbeat`, `tmux.waiting_for_input`, `tmux.session_ended`
- workspace: `workspace.skill.activated`, `workspace.session.*`, `workspace.hud.*`
- cron: `cron.job`

### Sources (`crate::source`)

Each source implements the `Source` trait and feeds a shared Tokio `mpsc` queue:

- `GitSource` — polls configured repos for commit and branch changes
- `GitHubSource` — polls GitHub API for issue and PR state changes; also polls CI run status
- `TmuxSource` — monitors tmux sessions per poll cycle; drives keyword detection, stale detection, heartbeat, summarization, and waiting-for-input state machine
- `WorkspaceSource` — watches filesystem paths for file changes using a debounced poll loop; supports worktree discovery
- `CronSource` — evaluates cron schedules on each tick and emits `cron.job` events for matching jobs

### Dispatcher (`crate::dispatch`)

`Dispatcher` is the queue consumer. For each event it:

1. classifies the event as routine, CI, or bypass
2. buffers routine events in a per-delivery-signature batcher (default 5s window)
3. buffers CI events in a separate CI batcher (default 30s window)
4. bypass events (failures, alerts, stale) skip the batcher entirely
5. resolves deliveries through the router, renders each one, and hands it to the sink
6. continues best-effort when one delivery fails; failed deliveries land in the DLQ

Batch windows are configurable via `[dispatch].routine_batch_window_secs` and `[dispatch].ci_batch_window_secs`. Setting routine window to `0` disables batching.

### Router (`crate::router`)

Resolves **0..N deliveries per event**. Multiple route rules can match the same event; all matches are collected and delivered independently. Each resolved delivery carries its destination target, format, template, mention context, and dynamic-token opt-in flag.

### Renderer (`crate::render`)

Formats event bodies into `compact`, `alert`, `raw`, or custom template output. The renderer is sink-agnostic; the Discord and Slack sinks each receive the same rendered string.

### Sink (`crate::discord`, `crate::slack`)

Two sinks ship:

- **Discord sink** — delivers to a bot-token channel or a Discord webhook; handles 429 rate limiting with retry-after backoff; exhausted retries go to the DLQ
- **Slack sink** — delivers to a Slack incoming webhook URL

## Summarization (`crate::summarize`)

Triggered by `TmuxSource` when pane content changes and `summarize = true` is set. Runs as a non-blocking background task so it never stalls the poll loop.

Backends:

| Spec | Implementation |
|---|---|
| `raw` | Returns truncated pane content verbatim |
| `gemini:<model>` | Shells out to the `gemini` CLI subprocess |
| `openrouter:<model>` | HTTP POST to OpenRouter chat completions API |
| `openai:<model>` / `openai-compatible:<model>` | HTTP POST to OpenAI-compatible chat completions API |

The summarizer produces `tmux.content_changed` events with `content_mode: "raw"` (raw passthrough) or `"summary"` (LLM result). Raw mode edits a single living Discord message per session; summary mode posts a new message each time to build a historical record.

## Dashboard pinning (`crate::discord`)

Each monitored tmux session maintains up to 5 pinned Discord messages, edited in-place rather than posting new messages. Slot names and their event sources:

| Slot | Trigger | Default |
|---|---|---|
| `status` | `tmux.heartbeat` | pinned (on) |
| `summary` | `tmux.content_changed` | pinned (on) |
| `alert` | `tmux.waiting_for_input`, `tmux.waiting_resolved` | pinned (on) |
| `activity` | all dashboard events (rolling log) | pinned (on) |
| `keywords` | `tmux.keyword` (rolling log) | not pinned (off) |

Message IDs are persisted to `~/.clawhip/dashboard.json`. On daemon restart, existing message IDs are reused so edits land on the same messages. When a session ends (`tmux.session_ended`), all pinned messages for that session are unpinned via the Discord API and the session entry is cleared from `dashboard.json`.

When a slot message does not yet exist, a new message is posted and its ID is saved. Subsequent events for the same slot edit that message in-place (PATCH).

## Waiting-for-input state machine (`crate::source::tmux`)

`TmuxSource` tracks a per-pane `is_waiting: bool` flag. On each poll cycle, `is_waiting_for_input()` inspects the last 3 non-empty lines of pane content (filtered from `capture-pane -S -200` output, which pads with blank rows) and matches against a list of patterns covering interactive prompts, confirmation dialogs, credential prompts, and Claude Code tool-approval flows.

State transitions:

- `(false, Some(prompt))` → emit `tmux.waiting_for_input`, set `is_waiting = true`
- `(true, None)` → emit `tmux.waiting_resolved`, set `is_waiting = false`
- `(true, Some(_))` — still waiting, suppress duplicate events

The 3-line window is intentional: after one user reply (output line + echoed command + new shell prompt = 3 non-empty lines), the original waiting prompt falls out of the window, triggering the resolved transition.

`tmux.waiting_for_input` and `tmux.waiting_resolved` both carry `dashboard_component = "alert"` when `pin_alerts = true`, routing them to the alert dashboard slot. Resolved events render as `✅ \`session\` — Input received, continuing...`.

## Keyword windowing (`crate::keyword_window`)

`PendingKeywordHits` accumulates keyword hits within a time window. Deduplication is within-window only: the same `(keyword, line)` pair is reported at most once per window. After a window is flushed, the next window starts fresh — identical pairs can fire again in subsequent windows.

When `keyword_window_secs` expires, hits are emitted as a single `tmux.keyword` event. The window is per-session and per-daemon-registration.

## Configuration model (`crate::config`)

Top-level sections:

- `[providers.discord]` / legacy `[discord]` — Discord bot token and default channel
- `[providers.gemini]`, `[providers.openrouter]`, `[providers.openai]` — LLM API keys
- `[daemon]` — bind host, port, base URL
- `[defaults]` — fallback channel and format
- `[dispatch]` — batch window tuning
- `[[routes]]` — event routing rules
- `[monitors]` — global poll interval, GitHub token, source configs
- `[[monitors.git.repos]]` — git repo monitors
- `[[monitors.tmux.sessions]]` — tmux session monitors with full dashboard/summarization config
- `[[monitors.workspace]]` — filesystem change monitors
- `[cron]` + `[[cron.jobs]]` — scheduled message delivery

See README for the complete field-level reference.

## Delivery semantics

- per-source FIFO through the shared queue
- routine events are batched per delivery signature; bypass events skip the batcher
- best-effort multi-delivery; one failed delivery does not block others
- 429 rate-limit responses are retried with retry-after backoff
- exhausted retries go to an in-memory DLQ for observability
- source-level tmux keyword windowing; dispatch is otherwise stateless

## Operational verification

See [`docs/live-verification.md`](docs/live-verification.md) for the full runbook.
