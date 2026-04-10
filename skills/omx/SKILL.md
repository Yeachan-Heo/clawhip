# Legacy wrapper helper note

This helper directory is no longer the recommended public workflow.

Use provider-native Codex or Claude hooks, keep project metadata in `.clawhip/project.json`,
and send local verification payloads through:

```bash
clawhip native hook --provider codex --file payload.json
clawhip native hook --provider claude --file payload.json
```

tmux monitoring remains available for keyword/stale alerts, but provider-native hook registration
is now the primary integration path.

If you need to re-submit a prompt into an already-running tmux-backed provider session, use:

```bash
clawhip deliver --session <tmux-session> --prompt "..." --max-enters 4
```
