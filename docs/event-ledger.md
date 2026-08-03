# Durable event ledger

Clawhip can retain an opt-in, append-only ledger of normalized **public-safe event metadata**. The ledger is written before route resolution and delivery. It never stores rendered messages, raw event payloads, prompts, command output, Discord content, tokens, webhooks, or arbitrary payload fields.

## Enablement

Old configurations remain unchanged because the ledger is disabled by default.

```toml
[ledger]
enabled = true
# path = "/var/lib/clawhip/event-ledger" # defaults beside daemon state
raw_retention_days = 7
summary_retention_days = 90
compaction_interval_secs = 3600
max_records = 100000
max_record_bytes = 8192
max_keywords = 16
max_keyword_bytes = 48
max_query_results = 200
max_records_per_compaction = 5000
```

The daemon fails closed for an enabled ledger: a record that requests raw/private retention, violates bounds, or cannot be flushed is not routed or delivered. A duplicate idempotency key is not appended or delivered again. Disabled ledgers create no files and preserve prior routing behavior.

## Stored contract

Each JSONL record contains only:

- schema version, public record id, and hashed dedupe key;
- canonical event type, timestamp, and bounded source label;
- optional repository, worktree, and session identity;
- bounded public HTTP(S) source links, excluding credential-bearing webhook/bot endpoint forms;
- bounded normalized categorical keywords.

Projection is closed and allowlisted. Adding a field to an incoming event does not make it durable. Explicit `raw`, `raw_payload`, `private_payload`, `retain_raw`, or `store_raw` requests and known credential-bearing source URLs are rejected before delivery.

Daily raw segments live under `raw/events-<utc-day>.jsonl`. Indexes are rebuilt from retained records at startup and are hard-capped by `max_records`.

## Query and status

The API is loopback-only:

- `GET /api/ledger/status`
- `GET /api/ledger/query?repo=owner/repo&worktree=/path&session_id=s1&event_type=session.finished&since=...&until=...&keywords=finished,main&limit=50`

The CLI exposes the same surface:

```sh
clawhip ledger status
clawhip ledger query --repo owner/repo --session-id s1 --event-type session.finished --keywords finished,main --limit 50
clawhip ledger verify
```

All filters are intersected. Time bounds are inclusive. Query limits are capped by `max_query_results`. Event-type filters use the normalized canonical type (for example, an incoming `agent.finished` lifecycle event is stored as `session.finished`). `verify` scans retained raw segments and summary shards and revalidates segment-day placement, hashes, timestamps, identity bounds, keyword normalization, source-link safety, shard identity, counts, and temporal invariants without printing private input.

## Dedupe and failure replay

When a caller supplies `idempotency_key` or `event_id`, its value is hashed and used as the durable dedupe identity. Subscription-supplied idempotency keys are namespaced by canonical event type and bounded subscription name, so reconnect replay dedupes within one feed without colliding with another feed using the same upstream key. Normalization marks internally generated event UUIDs so they do not masquerade as caller identifiers; identifier-less retries instead hash the canonical event type and bounded allowlisted public scalar identity fields, including numeric occurrence fields such as issue or pull-request number. Built-in custom sends and subscription frames receive distinct occurrence event ids. Subscription adapters may provide an explicit stable `idempotency_key` in their restricted public-safe output when reconnect replay suppression is required; identical frames without that key remain distinct occurrences. The dedupe set is rebuilt after restart and after summary pruning from retained raw records and the newest compacted summary shards, capped by `max_records`; `/api/ledger/status` reports `dedupe_history_cap_applied` if older summary history cannot fit in that operational bound.

- append succeeds: routing may continue;
- duplicate: no second append and no second delivery side effect;
- validation or append failure: no delivery, content-free error only;
- crash after append and before delivery: replay with the same key is suppressed. The ledger proves receipt but intentionally cannot replay private message content.

Sources should supply stable event ids whenever retryable delivery is required.

## Compaction and retention

On the dispatcher cadence, records older than `raw_retention_days` are grouped by UTC day, repository, worktree, and session. Clawhip writes deterministic summary shards containing event counts, first/last timestamps, bounded top keywords, up to 64 source record ids and hashed dedupe keys, and public source links. Groups larger than 64 records are split into deterministic bounded shards. Each shard is flushed and atomically renamed before eligible raw segments are removed.

At most `max_records_per_compaction` records are processed per pass. Summary files older than `summary_retention_days` are deleted even when a pass has no newly eligible raw records, and in-memory dedupe history is refreshed immediately so expired keys stop suppressing replay. Failed shard writes preserve the raw source segment for the next pass.

## Privacy and operations

Treat the configured ledger directory as public operational metadata, not as a secret store. Repository names, worktree paths, session ids, event types, timestamps, and public URLs may be visible there. Keep secrets and private text in source systems and link to access-controlled sources rather than copying content.

Monitor `/api/ledger/status` for append failures, rejections, duplicate counts, compaction progress, and degraded startup state. Run `clawhip ledger verify` after disk incidents or manual file recovery.
