# Provider-native Codex + Claude Hook Contract

> Frozen shared-event reference: see [docs/event-contract-v1.md](event-contract-v1.md).
> This document is the higher-level routing, metadata, and augmentation guide.

clawhip now treats Codex and Claude as the source of truth for hook registration and scope.
clawhip's job is to ingest provider-native hook payloads, normalize them into a stable routing
contract, and handle delivery.

## Goal

clawhip should remain the single routing and formatting layer for provider-native operational
events. Providers should fire hooks; clawhip should normalize, route, and render them.

## Shared v1 hook surface

v1 intentionally supports only the five events shared by Codex and Claude:

- `SessionStart`
- `PreToolUse`
- `PostToolUse`
- `UserPromptSubmit`
- `Stop`

Provider-specific extra events stay out of the shared route surface until clawhip adopts them
explicitly.

## Preferred ingress

Use the generic provider-native thin client:

```bash
clawhip native hook --provider codex --file payload.json
clawhip native hook --provider claude --file payload.json
cat payload.json | clawhip native hook --provider codex
```

This keeps local verification, fixture testing, and provider-side forwarding on one public
surface.

## Stable base routing fields

When the provider payload and git repo/worktree context make them available, clawhip preserves these base
fields for routing:

- `provider`
- `event`
- `session_id`
- `directory`
- `repo_path`
- `worktree_path`
- `repo_name`
- `branch`
- `tool_name`
- `command`
- `summary`
- `event_timestamp`

### Notes

- `repo_path` and `worktree_path` are the authoritative routing identity.
- `repo_name` is convenience metadata only; it must not be the sole collision breaker across repos.
- Inputs outside a git repo/worktree normalize to an explicit `non_git` outcome and are dropped
  before route evaluation or delivery.
- `directory` and `worktree_path` are base context, not optional decorations.
- Tool-specific metadata is additive; it should not replace core routing fields.

## Augmentation model

`.clawhip/hooks/` can enrich the base payload, but only additively.

Allowed augmentation patterns:

- frontmatter or summary enrichment
- recent-context snippets
- provider-specific metadata copies that preserve the shared base fields

Disallowed augmentation patterns:

- removing `provider`, `event`, `directory`, `worktree_path`, `repo_name`, or `project`
- replacing the base payload with a custom schema
- turning provider-specific extra events into shared-route keys without an explicit clawhip
  contract update

## Routing guidance

Prefer filters on structured metadata such as:

- `provider`
- `event`
- `worktree_path`
- `repo_path`
- `repo_name`
- `branch`
- `tool_name`

Use `repo_name` / `project` only as secondary convenience filters after the path-based routing keys.

Avoid routing on rendered message text.

Recommended route shape:

```toml
[[routes]]
event = "native.*"
filter = { provider = "codex", repo_path = "*/clawhip" }
channel = "PROJECT_CHANNEL_ID"
format = "compact"
```

## Formatting guidance

Default clawhip formatting should stay low-noise:

- compact: one-line lifecycle/status summary plus key metadata
- inline: dense room-safe summary
- alert: same payload with urgency framing
- raw: debug output for contract validation

## Migration note

Provider-native global configuration is now the supported setup path.

1. Codex or Claude owns hook registration through the canonical global shared-surface install
2. clawhip ingests the provider payload through `clawhip native hook`
3. clawhip derives routing identity from git repo/worktree context and loads only additive augmenters
4. clawhip owns channel routing, mentions, formatting, and delivery

Legacy repo-local generated hook config/state is no longer supported; `clawhip hooks install --scope project`
now warns and directs users to rerun the global install path without generating repo-local state.
