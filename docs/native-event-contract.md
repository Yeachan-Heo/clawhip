# Native OMC / OMX Event Contract

> Frozen contract reference: see [docs/event-contract-v1.md](event-contract-v1.md) for the stable v1 wire-format specification. This document remains the higher-level routing and migration guide.

This document is the clawhip-side normalization contract for native OMC/OMX operational events.

## Goal

clawhip should be the single routing and formatting layer.
OMC/OMX should emit machine-readable native events, not send direct Slack/Discord notifications.

## Canonical routing surface

Prefer routing on these canonical clawhip events:

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

For backward compatibility, clawhip still accepts legacy local wrapper emits:

- `agent.started`
- `agent.blocked`
- `agent.finished`
- `agent.failed`

`agent.started|blocked|finished|failed` and `session.started|blocked|finished|failed` intentionally cross-match in routing so existing configs do not break.

## Accepted upstream native inputs

clawhip normalizes already-merged OMC/OMX payloads from these upstream surfaces:

- OMC payloads that include `signal.routeKey`
- OMX payloads that include `context.normalized_event`
- native OMX hook-envelope CLI ingress via `clawhip omx hook`
- native OMX hook-envelope POSTs to clawhip's `/api/omx/hook` / `/omx/hook` ingress
- legacy local `clawhip emit agent.* ...` wrapper emits from `skills/omc/create.sh` and `skills/omx/create.sh`

Not every upstream raw event becomes a canonical session event. Low-signal/raw hook events should stay as secondary/debug inputs. New routes should target `session.*`.

In particular, raw OMX hook events such as `pre-tool-use` / `post-tool-use` are not clawhip v1 canonical route keys. When OMX wants clawhip-native routing for tool activity, it should map the activity onto an existing v1 event family (`session.failed`, `session.test-*`, `session.pr-created`, etc.) and keep tool details in metadata like `command`, `tool_name`, and `error_summary`.

## Normalized metadata

When upstream provides it, clawhip normalizes these fields onto the top-level payload:

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

### Notes

- `tool` is normalized to `omc` or `omx` when clawhip can infer it.
- `status` is backfilled for legacy `agent.*` emits so generic `clawhip emit agent.finished --agent omx ...` remains valid.
- OMC `context.projectName` backfills both compatibility `project` and canonical `repo_name`.
- OMC `context.projectPath` backfills `repo_path` and `worktree_path` when no more specific path fields are present.
- `issue_number` may be inferred from session/worktree/branch names like `issue-65` when upstream did not send it explicitly.
- `pr_number` may be inferred from `pr_url`.
- `raw_event` is retained only when clawhip had to rename the incoming event.
- `contract_event` is the canonical normalized event after clawhip ingestion.
- raw OMX tool events such as `pre-tool-use` / `post-tool-use` are not new clawhip v1 canonical events; map them to the frozen `session.*` family via metadata when they represent PR/test/failure/handoff semantics.

## Upstream-to-canonical mapping

### OMC-style route keys

| Upstream signal | Canonical clawhip event |
| --- | --- |
| `session.started` | `session.started` |
| `session.finished` | `session.finished` |
| `session.idle` | `session.blocked` |
| `question.requested` | `session.blocked` |
| `test.started` | `session.test-started` |
| `test.finished` | `session.test-finished` |
| `test.failed` | `session.test-failed` |
| `pull-request.created` | `session.pr-created` |
| `pull-request.failed` | `session.failed` |
| `tool.failed` | `session.failed` |

### OMX-style normalized events

| Upstream normalized event | Canonical clawhip event |
| --- | --- |
| `started` | `session.started` |
| `blocked` | `session.blocked` |
| `finished` | `session.finished` |
| `failed` | `session.failed` |
| `retry-needed` | `session.retry-needed` |
| `pr-created` | `session.pr-created` |
| `test-started` | `session.test-started` |
| `test-finished` | `session.test-finished` |
| `test-failed` | `session.test-failed` |
| `handoff-needed` | `session.handoff-needed` |

## Routing guidance

Project metadata should be the routing authority.

- Treat `project` / normalized `repo_name` as the source of truth for project-level routing.
- Treat `session_name` as an operator label, not the authoritative project classifier.
- Do not encode long-term routing doctrine around session-prefix glob hacks like `clawhip-*` when the launcher already knows the real project.
- If routing behavior depends on prefix today, that is a compatibility bug to remove, not a convention to preserve.
- Avoid overlapping broad tmux config monitors with launcher-registered per-session watches; duplicated watch layers can cause conflicting stale windows, ghost watches, and noisy keyword alerts.

Recommended route filters:

```toml
[[routes]]
event = "session.*"
filter = { tool = "omx", repo_name = "clawhip" }
channel = "1480171113253175356"
format = "compact"
```

Prefer filters on structured metadata such as:

- `tool`
- `repo_name`
- `session_name`
- `issue_number`
- `pr_number`
- `branch`

Avoid routing on rendered message text.

## Formatting guidance

Default clawhip session formatting is intentionally low-noise:

- compact: stable one-line status plus the most useful metadata
- inline: dense channel-safe summaries for busy rooms
- alert: same content with alert prefix for high-priority channels
- raw: original normalized payload for debugging

This contract is designed so real-world routing can stay stable even if OMC/OMX keep evolving their internal raw hook/event names.

## Deprecation note

Direct platform notifications from inside OMC/OMX are deprecated for clawhip-integrated setups.

Preferred model:

1. OMC/OMX emit native operational events
2. clawhip ingests them directly (for OMX, `clawhip omx hook` and `/api/omx/hook` are the native ingress surfaces)
3. clawhip normalizes them
4. clawhip owns channel routing, mentions, formatting, and webhook delivery

That keeps notification policy in one place and avoids duplicate/noisy Discord or Slack messages.
