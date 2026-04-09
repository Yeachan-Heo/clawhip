# Live verification workflow for built-in presets

This document is for **real operational verification**, not mock-only tests.

## Preconditions

- running `clawhip` daemon
- real Discord bot token with access to the test channel
- real GitHub auth (`gh auth status` should succeed)
- tmux installed locally
- route filters configured for the target repo/session/channel

Recommended environment:

```bash
export CLAWHIP_REPO=Yeachan-Heo/clawhip
export CLAWHIP_CHANNEL=1480171113253175356
export CLAWHIP_DAEMON_URL=http://127.0.0.1:25294
export CLAWHIP_BOT_TOKEN='<discord-bot-token>'
export CLAWHIP_MENTION='<@1465264645320474637>'
```

## Real built-in preset checklist

### GitHub issue presets

- issue opened
- issue commented
- issue closed

Operational flow:

1. Create a real issue in the target repo.
2. Wait for daemon monitor pickup or webhook delivery.
3. Confirm a real Discord message arrives in the configured test channel.
4. Add a real comment to the issue.
5. Confirm the issue-commented message arrives.
6. Close the issue.
7. Confirm the issue-closed message arrives.

### GitHub PR presets

- PR opened
- PR status changed
- PR merged

Operational flow:

1. Create a temporary base branch and feature branch.
2. Push the feature branch.
3. Open a real PR against the temporary base branch.
4. Confirm the PR-opened / status-changed message arrives.
5. Merge the temporary PR.
6. Confirm the merged status message arrives.
7. Delete temporary branches if desired.

### Provider-native Codex + Claude contract

- shared event set: `SessionStart`, `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`
- generic ingestion via `clawhip native hook --provider <codex|claude-code>` (`claude` remains an alias)

Operational flow:

1. Enable provider-native hooks at project or global scope in a real Codex workspace.
2. If you are testing from a worktree, verify `.clawhip/project.json` exists there before blaming
   the router; missing project metadata can make `repo_name` reflect the worktree basename instead
   of the real repo.
3. Pipe one representative Codex payload through the generic native ingress:

```bash
printf '%s\n' '{
  "session_id": "sess-65",
  "cwd": "/repo/clawhip",
  "event": "SessionStart"
}' | clawhip native hook --provider codex
```

4. Confirm clawhip accepts it and renders a stable lifecycle message with project/repo context.
5. Repeat with a representative Claude payload:

```bash
printf '%s\n' '{
  "session_id": "sess-65",
  "cwd": "/repo/clawhip",
  "event": "SessionStart"
}' | clawhip native hook --provider claude-code
```

6. Confirm both providers normalize into the same shared route family (`session.*` / `tool.*`),
   not a separate `native.*` route namespace.
7. Send representative payloads for `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, and `Stop`.
8. If delivery misses the expected channel, test a narrow metadata route such as
   `event = "session.*"` with `filter = { repo_name = "clawhip" }` before broadening tmux/session
   prefix filters.
9. Confirm additive augmentation still preserves the base routing keys when `.clawhip/hooks/` is enabled.

### tmux presets

- keyword detection
- stale detection
- tmux wrapper registration path

Operational flow:

1. Launch a real Codex or Claude session with provider-native hooks enabled.
2. Verify the pane is actually alive before trusting any `agent.started` message.
3. Confirm routed delivery in Discord.
4. Print a configured keyword (`error`, `FAILED`, `PR created`, etc) only when intentionally testing keyword behavior.
5. Leave the session idle beyond the stale threshold only when intentionally testing stale behavior.
6. Inspect `clawhip tmux list` to confirm exactly which watch registrations exist.
7. If alert text disagrees with pane reality, treat it as monitor noise and debug registration overlap / stale math before assuming session failure.
8. If the launcher names panes like `omx-clawhip-dev-*`, remember that prefix-only
   `session = "clawhip-*"` rules can miss provider-native session events even when the daemon is
   routing correctly.

## Helper script

A helper script is included:

```bash
scripts/live-verify-default-presets.sh <mode>
```

Available modes:

- `issue-opened`
- `issue-comment`
- `issue-closed`
- `pr-opened`
- `pr-merged`
- `tmux-keyword`
- `tmux-stale`
- `tmux-wrapper`

The script is intentionally conservative: it prints the live workflow and fetches recent Discord messages, but it does not silently mutate production resources without operator intent.

## Verified live run already completed

On March 8, 2026, a real validation was run for the GitHub issue-opened monitor path:

- real issue created on `Yeachan-Heo/clawhip`
- daemon monitor emitted `github.issue-opened`
- real Discord delivery observed with route-level mention prepended
- issue closed after verification

On March 11, 2026, a real validation was run for the custom send path:

- local daemon health/status returned ok on `http://127.0.0.1:25294`
- `cargo run -q -- send --message "🧪 clawhip live verification (...)"` exited successfully
- guild-wide search confirmed actual Discord delivery by the `clawhip` webhook bot
- delivery landed in `#ops` (`1477003109564678174`), confirming the configured wildcard webhook route was active
