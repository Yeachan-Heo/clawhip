# Event Contract v1

This document freezes clawhip's v1 native event contract for OMC/OMX integrations.

## Status

- **Schema version:** `1`
- **Stability:** stable
- **Compatibility policy:** v1 allows backward-compatible additive fields only. Existing field meanings, event names, and required metadata semantics are frozen.

## Canonical event family

clawhip routes native operational events on the `session.*` family:

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

For backward compatibility, clawhip still accepts:

- `agent.started`
- `agent.blocked`
- `agent.finished`
- `agent.failed`

Those legacy events cross-match with the corresponding first four `session.*` events.

## Frozen normalized_event names

Upstream OMX hook payloads must use these hyphenated `normalized_event` values:

- `started`
- `blocked`
- `finished`
- `failed`
- `retry-needed`
- `pr-created`
- `test-started`
- `test-finished`
- `test-failed`
- `handoff-needed`

> Note: underscore spellings such as `retry_needed`, `pr_created`, `test_started`, `test_finished`, `test_failed`, and `handoff_needed` appear in issue prose but are **not** the frozen wire-format contract.
>
> Raw OMX hook events such as `pre-tool-use` / `post-tool-use` are also **not** frozen v1 canonical events. If OMX wants clawhip routing for a tool operation, it must map that operation onto one of the supported normalized events above and carry tool details as metadata.

## OMX hook envelope format

clawhip accepts OMX hook envelope JSON with `schema_version = "1"`.

Recommended native daemon ingress for this envelope:

- `POST /api/omx/hook`
- `POST /omx/hook`

This lets native OMX integrations forward the frozen v1 envelope directly without translating it into clawhip's generic `/event` payload shape first.

```json
{
  "schema_version": "1",
  "event": "notify",
  "timestamp": "2026-03-31T09:00:00Z",
  "channel": "alerts",
  "mention": "@ops",
  "context": {
    "normalized_event": "test-failed",
    "agent_name": "omx",
    "session_name": "issue-65-event-contract",
    "session_id": "sess-65",
    "project": "clawhip",
    "repo_path": "/repo/clawhip",
    "branch": "feat/issue-65-event-contract",
    "issue_number": 65,
    "pr_number": 72,
    "pr_url": "https://github.com/Yeachan-Heo/clawhip/pull/72",
    "command": "cargo test",
    "tool_name": "Bash",
    "status": "failed",
    "summary": "tests failed",
    "error_summary": "1 test failed"
  }
}
```

### Envelope fields

Top-level fields:

| Field | Required | Notes |
| --- | --- | --- |
| `schema_version` | yes | Must be the string `"1"`. |
| `event` | no | May be a generic upstream event name such as `notify`; clawhip resolves the canonical route from `context.normalized_event`. |
| `timestamp` | no | Preserved by normalization as `event_timestamp` when present. |
| `channel` | no | Optional channel hint. |
| `mention` | no | Optional fallback mention if not supplied in `context`. |
| `context` | yes | Native routing and metadata payload. |

Context fields:

| Field | Required | Notes |
| --- | --- | --- |
| `normalized_event` | yes | One of the 10 frozen names above. |
| `agent_name` | yes | Native source/agent label, e.g. `omx`. |
| `status` | yes | Human-meaningful status for the event. |
| `session_name` | event-specific | Strongly recommended for all session events; required for stable routing summaries. |
| `session_id` | no | Optional correlation/session identifier. |
| `project` | no | Compatibility field for existing agent routes. |
| `repo_path` | event-specific | Required whenever the repo/worktree is known. |
| `branch` | event-specific | Required whenever the branch is known. |
| `issue_number` | no | Include when known. |
| `pr_number` | no | Include when known. |
| `pr_url` | no | Include when known. |
| `command` | no | Optional command context. |
| `tool_name` | no | Optional tool context. |
| `summary` | no | Short human-readable summary. |
| `error_summary` | failure-specific | Required for failed or blocked states when an actionable error exists. |

## Required metadata by normalized_event

| normalized_event | Required metadata |
| --- | --- |
| `started` | `agent_name`, `status`, `session_name` |
| `blocked` | `agent_name`, `status`, `session_name` |
| `finished` | `agent_name`, `status`, `session_name` |
| `failed` | `agent_name`, `status`, `session_name`, `error_summary` when failure details exist |
| `retry-needed` | `agent_name`, `status`, `session_name`, `summary` or `error_summary` |
| `pr-created` | `agent_name`, `status`, `session_name`, `pr_number` or `pr_url` |
| `test-started` | `agent_name`, `status`, `session_name`, `command` or `tool_name` when known |
| `test-finished` | `agent_name`, `status`, `session_name` |
| `test-failed` | `agent_name`, `status`, `session_name`, `error_summary` |
| `handoff-needed` | `agent_name`, `status`, `session_name`, `summary` |

## Context field surface clawhip models natively

clawhip's native Rust event surface models these context fields on `AgentEvent`:

- `agent_name`
- `session_name`
- `status`
- `normalized_event`
- `session_id`
- `project`
- `repo_path`
- `branch`
- `issue_number`
- `pr_number`
- `pr_url`
- `command`
- `tool_name`
- `elapsed_secs`
- `summary`
- `error_summary`
- `error_message`
- `mention`

Fields remain optional in Rust unless legacy compatibility or the wire format requires them universally.

## Backward compatibility

- The original four typed variants remain stable: started, blocked, finished, failed.
- Legacy `agent.*` inputs continue to deserialize into those same four variants.
- Canonical `session.started|blocked|finished|failed` also map to those four variants.
- The additional six canonical events are modeled as distinct typed variants.
- Unknown out-of-contract events continue to degrade safely to `Custom`.

## Relationship to the older native contract note

`docs/native-event-contract.md` remains a higher-level migration and routing note.
This document is the frozen v1 source of truth for event names, envelope format, metadata fields, and versioning policy.
