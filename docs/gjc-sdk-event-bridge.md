# GJC SDK Event Bridge (#324)

The bridge maps authoritative GJC SDK state into stable, public-safe Clawhip
events. It owns **no transport, polling loop, durable lane state, pane
scraping, key injection, or credentials**:

- #322 (transport/discovery) and #323 (control plane) produce authoritative
  evidence;
- #325 (durable reconciler) owns persistence, restart reconciliation, and the
  cadence that feeds snapshots;
- #324 (this module) only reduces typed snapshots into events.

## Ingress

```
POST /api/gjc/bridge
Content-Type: application/json

{ ...one authoritative state snapshot... }
```

The body is one SDK response payload (the `payload` value of the #322 typed
websocket response envelope). It is mapped through
`gjc_sdk_events::snapshot_from_response_payload`:

- unknown fields are ignored (additive contract);
- malformed payloads or a missing/blank `session_id` fail closed with `400`
  and a bounded `gjc_bridge_*` error code.

The daemon reduces the snapshot through a process-wide `GjcEventBridge` and
enqueues every emitted event through the normal accept path: event ledger →
router → sinks (Discord/Slack/local-file). The response reports what happened:

```json
{
  "ok": true,
  "session_id": "sess-1",
  "revision": 3,
  "duplicate": false,
  "stale": false,
  "emitted": [{"type": "workflow.question", "event_id": "gjc-workflow-question-…"}],
  "rejected": [],
  "totals": {"snapshots": 7, "duplicates": 2, "stale": 1, "emitted": 4}
}
```

## Snapshot schema (typed input)

| Field | Type | Notes |
| --- | --- | --- |
| `session_id` | string, required | Bounded to 128 chars after sanitization. |
| `revision` | u64, required | Monotonic session-state revision; drives stale/duplicate suppression. |
| `turn` | object? | `{id, state: idle\|active\|waiting_input\|complete\|failed, attempt, error_summary}`. |
| `prompt` | object? | `{command_id, status: accepted\|progressing}` — acceptance evidence for the submitted control command. |
| `gate` | object? | `{id, kind: ask\|workflow, revision, status: open\|resolved, summary}` — question/gate episode with the identifiers #323 answers against. |
| `model`, `profile` | string? | Public-safe identifiers; changes are announced once per transition. |
| `endpoint` | object? | `{health: ok\|degraded\|failed, detail}` — no URLs, no credentials. |
| `repo_name`, `repo_path`, `worktree_path`, `branch` | string? | Routing identity copied onto every emitted event. |
| `observed_at` | RFC3339 string? | Preserved as `event_timestamp`. |
| `summary` | string? | Optional public-safe lane summary. |

## Transition mapping

| Authoritative transition | Clawhip event |
| --- | --- |
| First active observation of a session | `session.started` |
| Prompt accepted (`prompt.status=accepted`, new `command_id`) | `session.prompt-submitted` |
| Retry (`turn.attempt` increases) | `session.retry-needed` |
| Ask gate opens (new gate id or higher gate revision) | `workflow.question` |
| Workflow gate opens | `workflow.gate` |
| Turn completes / fails (once per turn id) | `session.finished` / `session.failed` |
| Model or profile change (after baseline) | `session.model-changed` |
| Endpoint degrades/fails (per episode; degraded→failed escalates) | `session.endpoint-failed` |

Prompt *progression* ticks (`status=progressing`, unchanged revisions) are
tracked but deliberately emit no event: they would be noise on Discord.

## Dedupe and restart semantics

- Snapshots with a lower `revision` than the tracked watermark are **stale**
  (suppressed); equal `revision` is a **duplicate** (suppressed).
- Gate episodes are tracked per gate id: reopening the same gate requires a
  higher gate revision; a different gate id starts a new episode.
- Every event carries a deterministic `event_id` (FNV-1a over
  `kind|session|revision|turn|gate|transition`) plus `idempotency_key`.
  The event ledger dedupes on this identity, so replaying the same
  authoritative feed after a daemon restart produces ledger-level duplicates:
  no second Discord delivery.
- Known limitation: an endpoint-failure episode that spans a daemon restart
  can re-notify once, because the bridge keeps its alert flag in memory.
  #325's durable reconciler closes this gap by seeding last observed state.

## Sibling integration (landed surfaces)

- **#323 control plane** (`src/gjc/`): authoritative session state arrives as
  `gjc::model::SessionQuery`. `snapshot_from_session_query` maps it onto the
  bridge snapshot — turn statuses map `Queued`/`Running`→active,
  `Succeeded`→complete, `Failed`→failed, `Aborted`→idle; a `Ready`
  workflow gate opens an episode and the most recently raised non-ready gate
  resolves it, with gate revisions synthesized from `raised_at`. Callers
  supply monotonic lane revision plus a `GjcSnapshotIdentity` for routing.
- **#325 reconciler** (`src/gjc_lane.rs`): owns durable polling, the lane
  store, restart reconciliation, and emits its own `gjc.lane.status`
  bookkeeping events. It does not duplicate bridge notifications; feeding the
  bridge is one call into `snapshot_from_session_query` (or one HTTP push per
  observation to `POST /api/gjc/bridge`) from the reconciler's poll pass.
- The daemon keeps both seams live: control-plane endpoints under `/api/gjc/*`
  (#323) and the bridge ingress `/api/gjc/bridge` (#324). A full-section
  `GET /api/gjc/session/{session}` read reduces through the bridge from the
  same authoritative evidence it returns (partial `sections` reads skip the
  bridge); only registered lanes feed, with revision and routing identity
  taken from the durable lane store.

## Public safety

Emitted payloads are whitelist-only: bounded identifiers, collapsed control
characters, truncated summaries (240 chars). Raw prompts, tokens, endpoint
URLs, and free-form payload passthrough never enter bridge output.

## Routing and rendering

- `workflow.question` matches the setup-owned route shape
  (`event = "workflow.question"`, `repo_name` filter) created by
  `clawhip setup --question`; it renders as a compact/alert question line with
  the answer identifiers (`question_id`, `turn_id`, `gate_revision`,
  `command_id`) that #323's answer verb consumes.
- `workflow.gate` renders as a gate line with the same identifiers.
- `session.*` bridge kinds reuse the standard session renderer.
- Question, gate, and endpoint-failure kinds bypass the routine Discord batch
  window so operator-facing alerts deliver immediately; `workflow.question`,
  `workflow.gate`, and `session.endpoint-failed` map to high priority.
