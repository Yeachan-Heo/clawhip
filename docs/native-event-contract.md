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
clawhip native hook --provider claude-code --file payload.json
cat payload.json | clawhip native hook --provider codex
```

This keeps local verification, fixture testing, and provider-side forwarding on one public
surface.

## Stable base routing fields

When the provider payload and project metadata make them available, clawhip preserves these base
fields for routing:

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

### Notes

- `.clawhip/project.json` is the preferred place for project identity that should survive across
  providers.
- Detached worktrees should still carry `.clawhip/project.json`; otherwise the raw worktree
  basename can leak into `repo_name` and break project-level routes.
- `project` / `repo_name` should be the authority for project-level routing.
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
- `repo_name`
- `project`
- `branch`
- `tool_name`

Avoid routing on rendered message text.
Prefer normalized event families such as `session.*` and `tool.*`; provider-native ingress does
not route through a separate `native.*` family after normalization.
Treat `session` / `tmux_session` prefixes as debugging hints only, because real launcher names like
`omx-clawhip-dev-*` can differ from the project that should receive the alert.

Recommended route shape:

```toml
[[routes]]
event = "session.*"
filter = { provider = "codex", repo_name = "clawhip" }
channel = "1480171113253175356"
format = "compact"

[[routes]]
event = "tool.*"
filter = { provider = "claude-code", project = "clawhip" }
channel = "1480171113253175356"
format = "compact"
```

## Formatting guidance

Default clawhip formatting should stay low-noise:

- compact: one-line lifecycle/status summary plus key metadata
- inline: dense room-safe summary
- alert: same payload with urgency framing
- raw: debug output for contract validation

## Migration note

Provider-native configuration is now the supported setup path.

1. Codex or Claude owns hook registration plus scope precedence
2. clawhip ingests the provider payload through `clawhip native hook`
3. clawhip loads project metadata plus additive augmenters
4. clawhip owns channel routing, mentions, formatting, and delivery

That keeps notification policy in one place and avoids duplicated integrations.
