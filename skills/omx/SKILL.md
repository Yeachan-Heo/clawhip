# clawhip × OMX (oh-my-codex)

Launch [OMX](https://github.com/Yeachan-Heo/oh-my-codex) coding sessions with native clawhip event routing and automatic Discord notifications.

## What you get

- Native lifecycle emits via `clawhip omx hook`
- clawhip-side normalization of richer native OMC/OMX events into the canonical `session.*` routing contract
- Native OMX hook bridge assets in `integrations/omx/` for first-class OMX plugin forwarding
- Session keyword alerts (error, PR created, complete, etc.)
- Stale session detection (no output for N minutes)
- All notifications routed to the correct Discord channel
- Direct Slack/Discord notifications inside OMC/OMX should be treated as deprecated in clawhip-integrated setups

## Prerequisites

- [clawhip](https://github.com/Yeachan-Heo/clawhip) installed and daemon running
- [OMX](https://github.com/Yeachan-Heo/oh-my-codex) installed
- tmux

## Usage

### Create a session

```bash
./create.sh <session-name> <worktree-path> [prompt] [channel-id] [mention]
```

```bash
# Basic — uses clawhip default channel
./create.sh issue-123 ~/my-project/worktrees/issue-123

# Start a session and auto-send an initial prompt after the TUI initializes
./create.sh issue-123 ~/my-project/worktrees/issue-123 "Fix the bug in src/main.rs and create a PR to dev"

# With prompt, specific channel, and mention
./create.sh issue-123 ~/my-project/worktrees/issue-123 "Fix the bug in src/main.rs and create a PR to dev" 1234567890 "<@user-id>"
```

`create.sh` now emits native clawhip v1 hook envelopes directly from the OMX shell session via `clawhip omx hook`, so you no longer need a separate lifecycle watcher command. If you pass a prompt, the script waits 10 seconds for the TUI to initialize, then sends the prompt via `tmux send-keys -l` before pressing Enter.

For a first-class `.omx/hooks/*.mjs` bridge, see [`integrations/omx/`](../../integrations/omx/).

### Send a prompt

```bash
./prompt.sh <session-name> "Fix the bug in src/main.rs and create a PR to dev"
```

`prompt.sh` sends prompt text in tmux literal mode (`send-keys -l`) and presses Enter separately so quotes, punctuation, and leading dashes are preserved exactly.

### Monitor output

```bash
./tail.sh <session-name> [lines]
```

## Customization

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CLAWHIP_OMX_KEYWORDS` | `error,Error,FAILED,PR created,panic,complete` | Comma-separated keywords to monitor |
| `CLAWHIP_OMX_STALE_MIN` | `30` | Minutes before stale alert |
| `CLAWHIP_OMX_FLAGS` | `--madmax` | Extra flags passed to `omx` |
| `CLAWHIP_OMX_ENV` | *(empty)* | Extra env vars prepended to omx command (e.g. `FOO=1 BAR=2`) |
| `CLAWHIP_OMX_PROJECT` | detected from the git common dir (fallback: worktree name) | Override the project name sent in lifecycle events |

### Config defaults

Set defaults in `~/.clawhip/config.toml`:

```toml
[skills.omx]
channel = "1234567890"
mention = "<@your-user-id>"
keywords = "error,Error,FAILED,PR created,complete"
stale_minutes = 30
flags = "--madmax"
```
