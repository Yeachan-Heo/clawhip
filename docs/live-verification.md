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

### Native OMC / OMX contract

- legacy wrapper `agent.*` emits
- normalized `session.*` contract from OMC/OMX payloads

Operational flow:

1. Emit a legacy compatibility event such as `clawhip emit agent.finished --agent omx --session issue-65 --project clawhip --elapsed 42`.
2. Confirm clawhip accepts it and renders a stable lifecycle message.
3. Post one representative OMC payload carrying `signal.routeKey` to `/event`.
4. Confirm clawhip normalizes it into the expected `session.*` route family.
5. Post one representative OMX payload carrying `context.normalized_event` to `/event`.
6. Confirm the rendered message stays low-noise and includes normalized metadata like repo/session/issue/PR when present.
7. Pipe the same OMX payload through the native OMX CLI ingress and confirm acceptance:

```bash
printf '%s\n' '{
  "schema_version": "1",
  "event": "session-start",
  "timestamp": "2026-04-01T22:00:00Z",
  "context": {
    "normalized_event": "started",
    "agent_name": "omx",
    "session_name": "issue-65-native-sdk",
    "status": "started"
  }
}' | clawhip omx hook
```

8. Post the same OMX payload to the native OMX daemon ingress and confirm acceptance:

```bash
curl -sS -X POST http://127.0.0.1:25294/api/omx/hook \
  -H 'content-type: application/json' \
  -d '{
    "schema_version": "1",
    "event": "session-start",
    "timestamp": "2026-04-01T22:00:00Z",
    "context": {
      "normalized_event": "started",
      "agent_name": "omx",
      "session_name": "issue-65-native-sdk",
      "status": "started"
    }
  }'
```

### tmux presets

- keyword detection
- stale detection
- tmux wrapper registration path

Operational flow:

1. Use a monitored tmux session name that matches the route filter.
2. Print a configured keyword (`error`, `FAILED`, `PR created`, etc).
3. Confirm the keyword notification in Discord.
4. Leave the session idle beyond the stale threshold.
5. Confirm the stale notification in Discord.
6. Launch a session via `clawhip tmux new ...`.
7. Confirm wrapper registration + keyword/stale delivery.

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
