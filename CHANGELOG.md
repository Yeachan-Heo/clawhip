# Changelog

## 0.6.2 - 2026-04-09

### Highlights

- ship the patched root `SKILL.md` so repo-level setup guidance finally matches the provider-native hook model already used by Codex and Claude
- add regression coverage to keep the root skill aligned with the real hook install targets and routing metadata expectations

### Documentation and tooling

- document `clawhip hooks install --all --scope global --force` directly in the root skill
- document the canonical local install targets:
  - `~/.clawhip/hooks/native-hook.mjs`
  - `~/.codex/hooks.json`
  - `~/.claude/settings.json`
- add root-level guidance for `.clawhip/project.json`, `repo_name` / `project` route filters, and the detached-worktree metadata caveat

### Upgrade notes

- crate version is now `0.6.2`
- if you use the repo root `SKILL.md` as your operator/setup guide, refresh to this release before onboarding new local installs
- root skill examples now prefer provider-native `repo_name` / `project` routing over wrapper-era session-prefix assumptions

## 0.6.1 - 2026-04-09

### Highlights

- verify that provider-native Codex and Claude hook emission works locally against the real clawhip daemon path
- confirm the remaining local routing confusion is primarily caused by route-filter / metadata assumptions, not daemon-core native-hook routing
- update provider-native route guidance so detached-worktree metadata, repo/project filtering, and session/tool family routing are documented together

### Documentation and tooling

- refresh README, native-event docs, live-verification docs, and OMC/OMX skill surfaces to match the verified provider-native flow
- replace stale wrapper-era examples such as `tool=omx` route assumptions with provider-native routing examples using session/tool families plus provider/repo/project metadata
- clarify canonical `claude-code` naming while noting where `claude` is only a CLI shorthand/alias

### Upgrade notes

- crate version is now `0.6.1`
- if local provider-native events appear to route to the wrong Discord destination, test narrower `repo_name` / `project` filters before broad session-prefix filters like `omx-*`
- detached worktrees can change `repo_name` assumptions unless `.clawhip/project.json` is present, so route guidance now explicitly calls that out

## 0.6.0 - 2026-04-09

### Highlights

- replace the old bespoke OMC/OMX wrapper glue with provider-native Codex + Claude hook registration built around `clawhip native hook`
- preserve prompt-submit and stop lifecycle events in the provider-native flow so real interactive sessions emit the same critical lifecycle signals clawhip expects
- fix Codex native-hook installation/detection to use `~/.codex/hooks.json` as the real hook registry surface instead of treating `.codex/config.toml` as installed-state evidence
- add tmux session/pane metadata to native hook payloads, including pane id, pane tty, attached state, and client count
- preserve the tmux metadata through normalization and rendering so local hook logs stay attributable to the originating tmux pane/session

### Documentation and tooling

- update public docs to describe the shared provider-native hook contract instead of the older wrapper/launcher-specific hook assets
- remove the checked-in OMC/OMX wrapper helper scripts and legacy hook assets that were superseded by provider-owned hook registration
- add provider-native docs regression coverage so the new installation guidance remains consistent

### Upgrade notes

- crate version is now `0.6.0`
- rerun `clawhip hooks install --all --scope global --force` to install the latest global Codex/Claude hook wiring into `~/.codex/hooks.json` and `~/.claude/settings.json`
- Codex hook installation now targets `~/.codex/hooks.json`; `.codex/config.toml` remains a general config surface and should not be treated as the hook registry
- if you were relying on the older wrapper-specific OMC/OMX launch helpers or checked-in wrapper hook assets, migrate to provider-owned hook registration plus `clawhip native hook` for local verification

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
