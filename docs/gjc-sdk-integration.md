# Clawhip ↔ GJC SDK Integration — Operator Guide

Status: staged on lane `test/issue-326-gjc-sdk-e2e` (issue #326, parent epic #321).
The endpoint discovery/transport layer itself landed with #322
(`src/gjc_sdk.rs`); control-plane (#323), event bridge (#324), and durable
reconciliation (#325) are sibling-owned. **Open regression #328 tracks the
transport against the real GJC SDK v3 endpoint** — the live-session steps in
the dogfood checklist below stay gated until #328's repaired dev merge.

## 1. What this integration is

Clawhip consumes Gajae-Code session events through the GJC SDK endpoint:

- **Discovery** — the SDK CLI publishes one owner-only metadata file per live
  session at `<worktree>/.gjc/state/sdk/<session-id>.json`
  (`{"version":1,"sessionId":…,"url":"ws://127.0.0.1:<port>/","token":…,"pid":…}`).
  Clawhip never scans unrelated roots; every trust-boundary directory is
  validated as a real directory, and stale sessions are reported via pid
  liveness (`Live > Stale > Malformed > NoMetadata`).
- **Transport** — authenticated loopback websocket (`ws://` with IPv4/IPv6
  loopback literals only). The token travels as a `?token=` query credential
  plus bearer header. Requests are correlated envelopes; the server answers
  with correlated responses and emits lifecycle notifications out of band.
- **Event bridge** — session notifications (questions, progress, lifecycle)
  flow through clawhip subscriptions into routed deliveries.
- **Diagnostics** — read-only `clawhip gjc inspect [--probe] [--json]`
  reports discovery status and an optional hello/request probe without any
  control verbs.

Non-goals (epic-wide): no pane scraping as authority, no tmux `send-keys`
control, no credential or raw prompt leakage, no non-loopback endpoints.

## 2. Setup

### Opt-in flag

```sh
clawhip setup --gjc-sdk
```

This flips `[gjc].enabled = true`. It is idempotent and writes no endpoints,
tokens, or secrets into the config file — those stay in the 0600 worktree
metadata file managed by the SDK side.

```toml
[gjc]
enabled = true
```

Legacy configs without `[gjc]` keep parsing byte-stable; absent means off.

### Question delivery (existing surface)

```sh
clawhip setup --question-channel <CHANNEL_ID>   # or --question-fallback
```

This creates the setup-owned `gjc-question` websocket subscription
(endpoint env `GJC_QUESTION_WS`) routing `workflow.question` events to your
channel. Point the env at the session's authenticated ws URL when driving it
manually.

## 3. Health diagnostics

`GET /health` (and `clawhip status`) exposes a public-safe `gjc` block:

```json
{"gjc": {"enabled": true, "question_subscription": true}}
```

It intentionally carries flags only — never endpoint URLs, ports, token env
names, or token values. For endpoint-level diagnostics use
`clawhip gjc inspect --probe --json`, which reports `status`
(`live|stale|malformed|no_metadata`) and redacted probe results
(`hello_connection_id`, `request_ok`, `request_correlated`, reason codes).

## 4. Redaction guarantees

- The auth token exists only inside the 0600 metadata file; config files,
  logs, health output, and error paths never contain it.
- Subscription frames are projected field-by-field: only the configured
  projection keys survive; any `token`/`endpoint`/`authorization`-like keys
  riding alongside are dropped before the event stream and rejected if an
  adapter echoes them back.
- Transport errors render as stable reason codes
  (`endpoint_unavailable`, `endpoint_unauthorized`, `timeout`,
  `connection_closed`, `frame_rejected`, `correlation_mismatch`,
  `retry_exhausted`, …) with no URL or token material.

## 5. Migration notes

- Configs from ≤0.6.x parse unchanged; `[gjc]` defaults to disabled.
- The earlier draft surface (`discovery_roots`, `discovery_path`,
  `auth_token_env`) was removed during reconciliation with the landed #322
  contract — deny-unknown-fields rejects those keys loudly instead of
  silently ignoring them.
- If you previously pointed `GJC_QUESTION_WS` at an unauthenticated socket,
  switch to the authenticated URL published by the SDK metadata file.

## 6. Troubleshooting

| Symptom | Inspect | Likely cause / fix |
| --- | --- | --- |
| `inspect` reports `no_metadata` | `ls -la .gjc/state/sdk/` | No live session published metadata; start the GJC session in this worktree. |
| `stale` | `kill -0 <pid>` | Session process exited without cleanup; retire the stale metadata file. |
| `malformed` | validate JSON schema | Metadata edited by hand or truncated; republish from the SDK CLI. |
| Probe `endpoint_unauthorized` | token mismatch | The `?token=` credential does not match the metadata file; republish. |
| Probe `timeout` / `connection_closed` | endpoint liveness | Endpoint died mid-exchange; see #328 for the open v3 regression. |
| Subscription `retry_exhausted` | `GET /api/subscriptions/<name>` | Endpoint unreachable for the full reconnect budget; restart the daemon after the endpoint returns. |
| Question delivered without summary | projection keys | Projection must select `/questionId` and `/summary` from notification frames. |

## 7. Compatibility

- Rust stable; edition 2024; no new runtime dependencies beyond the `axum`
  `ws` feature (same tokio-tungstenite major already pinned in Cargo.lock).
- Old configs parse unchanged; unknown `[gjc]` fields fail closed.
- Daemon restart reconciles in-memory ownership: lanes registered by a dead
  daemon do not ghost-resurrect; deliveries are not replayed.

## 8. Live dogfood checklist (bounded)

Preconditions: real GJC session in this worktree; dev head with #328's
transport repair merged; `[gjc] enabled = true`; question route configured.

1. `clawhip gjc inspect --probe --json` → expect `"status":"live"` with
   correlated probe results.
2. Start the daemon with the question subscription endpoint env set.
3. Submit one bounded prompt to the live session through the supported
   control surface (sibling #323 scope; until merged, drive prompts from the
   GJC side only).
4. Observe progress notifications in the session; confirm no prompt text is
   echoed into clawhip logs.
5. Trigger one workflow gate; confirm `workflow.question` reaches the
   configured channel with projected `question_id`/`summary` only.
6. Answer the gate from the channel-side workflow; confirm acceptance.
7. Let the turn complete; confirm the completion notification and that the
   lane retires cleanly.
8. Restart the daemon; confirm `/health` is green, `/api/lane` shows no ghost
   ownership, and no deliveries replay.
9. Record the run: dev head, inspect output, delivery sample, timestamps.

Steps 3–7 require the repaired transport (#328); until then this checklist
is exercisable only against the deterministic fake endpoint in
`tests/gjc_fake_server_contract.rs` and `tests/gjc_sdk_full_loop_e2e.rs`.
