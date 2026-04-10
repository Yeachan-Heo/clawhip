# Changelog

## 0.6.4 - 2026-04-10

### Breaking

- replace provider-specific wrapper/launcher docs with the shared provider-native Codex + Claude hook surface
- document `clawhip native hook` as the generic ingress for shared hook payload verification
- move public guidance to provider-native installation, `.clawhip/project.json`, and additive `.clawhip/hooks/` augmentation

### Highlights

- add `clawhip deliver` for prompt-submit-aware prompt recovery into existing hooked tmux-backed provider sessions
- validate repo-local hook setup and active Codex/Claude (including OMC/OMX wrapper) panes before retrying Enter
- record prompt-submit readiness in `.clawhip/state/prompt-submit.json` so delivery can stop once the hook actually fires

### Upgrade notes

- if you were using wrapper-specific launch flows, migrate to provider-owned hook registration plus `clawhip native hook` for local testing
- the shared v1 contract now documents only `SessionStart`, `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, and `Stop`

## 0.5.4 - 2026-04-05

### Highlights

- native OMC/OMX lifecycle hooks with one-shot installer (`clawhip hooks install --omc|--omx|--all`)
- `clawhip omc "prompt"` and `clawhip omx launch "prompt"` for guaranteed prompt delivery with TUI detection
- session-init and session-stop hooks emit `session.started` / `session.finished` / `session.failed` to clawhip daemon
- cleaned up accidentally committed embedded worktree and local agent state from repo history

### Upgrade notes

- crate version is now `0.5.4`
- run `clawhip hooks install --omc` to deploy OMC lifecycle hooks to `~/.claude/hooks/`
- run `clawhip hooks install --omx` for OMX lifecycle hooks
- existing config remains compatible; no migration required

## 0.5.3 - 2026-04-04

### Highlights

- fix `clawhip send --channel` being overridden by route or default channel config
- for `custom` events, the explicit event channel now takes highest priority over route and default channels

### Upgrade notes

- crate version is now `0.5.3`
- existing config remains compatible; no migration required
- if you relied on a catch-all `event = "custom"` route to redirect all `clawhip send` traffic to a specific channel, that route channel will now only apply when `--channel` is not specified

## 0.5.2 - 2026-04-04

### Highlights

- reduced routine Discord burst noise with configurable batching for routine notifications
- allow `stale_minutes = 0` to disable tmux stale detection cleanly
- keep cron startup alive when persisted scheduler state is empty or invalid
- surface source failures as degraded alerts before the daemon appears healthy
- make matched route channels override source-provided channel hints consistently
- quiet invalid git monitor paths so they stop drowning out actionable failures

### Upgrade notes

- crate version is now `0.5.2`
- existing config remains compatible; no schema migration is required for this patch release
- `stale_minutes = 0` is now treated as an explicit disable for tmux stale alerts

## 0.3.0 - 2026-03-09

### Highlights

- introduced the typed internal event model used by the dispatcher pipeline
- generalized routing so one event can fan out to multiple deliveries
- extracted git, GitHub, and tmux monitoring into explicit event sources
- split rendering from transport and shipped the Discord sink on top of that boundary
- kept existing CLI and HTTP event ingress compatible while normalizing into the new architecture

### Upgrade notes

- crate version is now `0.3.0`
- `[providers.discord]` is the preferred config surface; legacy `[discord]` remains compatible
- routes may set `sink = "discord"`; omitting it still defaults to Discord in this release
