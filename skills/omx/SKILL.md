# Legacy wrapper helper note

This helper directory is no longer the recommended public workflow.

Use provider-native Codex or Claude hooks, keep project metadata in `.clawhip/project.json`,
and send local verification payloads through:

```bash
clawhip native hook --provider codex --file payload.json
clawhip native hook --provider claude-code --file payload.json
```

tmux monitoring remains available for keyword/stale alerts, but provider-native hook registration
is now the primary integration path.

Routing note:
- prefer `session.*` / `tool.*` routes filtered on `repo_name` or `project`
- do not assume tmux/session prefixes identify the right Discord route
- in worktrees, missing `.clawhip/project.json` can make `repo_name` unstable
