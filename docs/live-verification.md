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
export CLAWHIP_CHANNEL=TEST_CHANNEL_ID
export CLAWHIP_DAEMON_URL=http://127.0.0.1:25294
export CLAWHIP_BOT_TOKEN='<discord-bot-token>'
export CLAWHIP_MENTION='@maintainer-or-team'
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
- generic ingestion via `clawhip native hook --provider <codex|claude>`

Operational flow:

1. Enable provider-native hooks in a real Codex or Claude Code workspace:
   - Codex: `clawhip hooks install --provider codex --scope global` or `--scope project` (matching the official Codex `hooks.json` search locations)
   - Claude Code: `clawhip hooks install --provider claude-code --scope global`
2. Pipe one representative Codex payload through the generic native ingress:

```bash
printf '%s\n' '{
  "session_id": "sess-65",
  "cwd": "/repo/clawhip",
  "event": "SessionStart"
}' | clawhip native hook --provider codex
```

3. Confirm clawhip accepts it and renders a stable lifecycle message with project/repo context.
4. Repeat with a representative Claude payload:

```bash
printf '%s\n' '{
  "session_id": "sess-65",
  "cwd": "/repo/clawhip",
  "event": "SessionStart"
}' | clawhip native hook --provider claude
```

5. Confirm both providers normalize into the same shared route family.
6. Send representative payloads for `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, and `Stop`.
7. Confirm additive augmentation still preserves the base routing keys when `.clawhip/hooks/` is enabled.

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

Restart persistence smoke:

1. Start the daemon and register a runtime watch with `clawhip tmux watch <session> --keyword <word>` or the wrapper path.
2. Confirm `clawhip tmux list` shows the session and `tmux-watch-registry.json` exists beside the cron state file.
3. Restart the daemon without killing the tmux session.
4. Confirm `clawhip tmux list` still shows the watch and `/health` includes `tmux.registry_state.status` as `loaded` with a nonzero durable runtime count.
5. Emit the watched keyword in the tmux pane and confirm routed Discord delivery.
6. Kill the tmux session or send a terminal session event, then confirm the watch disappears and the registry file no longer rehydrates it after another restart.

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
- delivery landed in the configured test channel, confirming the configured wildcard webhook route was active

## Sender-identity acceptance smoke (bot-token deployments)

Transport success (`discord_send_success`, `token_source=config`) proves the
token is valid and delivery worked — it does **not** prove *which bot* sent the
message. During the 2026-08-21 recovery, a wrong-but-valid token delivered
messages as the wrong bot while every transport signal stayed green. The
acceptance smoke for bot-token deployments therefore has two mandatory legs:

1. **API identity equality** — `GET /users/@me` with the effective token must
   resolve to exactly the operator-configured expected bot ID.
2. **Channel author readback** — a delivered test message read back from the
   channel must be authored by that same expected bot.

### Setup

Configure the expected stable bot ID (the dedicated Clawhip bot's application
ID from the Discord Developer Portal — never a token):

```toml
[providers.discord]
token = "<bot-token>"
expected_bot_id = "<expected-discord-bot-id>"
```

### Leg 1 — API identity equality (fail-closed preflight)

```bash
clawhip config verify-sender-identity
clawhip config verify-sender-identity --json
```

- exits `0` **only** when the observed stable bot ID equals `expected_bot_id`
- prints expected and observed bot IDs on mismatch — never the token
- exits non-zero for mismatch, absent expectation, no token, invalid
  credential, rate limit, malformed response, or transport failure

A wrong-but-valid token fails this leg with
`sender_identity_mismatch` even though transport would succeed. That is the
point.

### Leg 2 — Channel author readback

1. Deliver a uniquely-marked test message to the target channel.
2. Read the message back from the channel (Discord client or
   `GET /channels/<id>/messages`).
3. Confirm the message **author** is the expected bot ID, not merely that the
   message exists.

A smoke that only confirms arrival is insufficient: a wrong-but-valid token
arrives fine, authored by the wrong bot.

### Acceptance

The smoke passes only when both legs pass. Any other combination — identity
verified but authored by another bot, or transport green but identity mismatch
— is a failure state requiring the token to be corrected before the deployment
is called healthy.
