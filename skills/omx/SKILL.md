# clawhip × OMX (oh-my-codex)

Launch [OMX](https://github.com/Yeachan-Heo/oh-my-codex) coding sessions with automatic Discord notifications via clawhip.

## What you get

- Native `agent.started`, `agent.finished`, and `agent.failed` lifecycle events via `clawhip emit`
- Session keyword alerts (error, PR created, complete, etc.)
- Stale session detection (no output for N minutes)
- All notifications routed to the correct Discord channel

## Prerequisites

- [clawhip](https://github.com/Yeachan-Heo/clawhip) installed and daemon running
- [OMX](https://github.com/Yeachan-Heo/oh-my-codex) installed
- tmux

## Usage

### Create a session

```bash
./create.sh <session-name> <worktree-path> [channel-id] [mention]
```

```bash
# Basic — uses clawhip default channel
./create.sh issue-123 ~/my-project/worktrees/issue-123

# With specific channel and mention
./create.sh issue-123 ~/my-project/worktrees/issue-123 1234567890 "<@user-id>"
```

`create.sh` now emits lifecycle notifications directly from the OMX shell session, so you no longer need a separate lifecycle watcher command.

### Send a prompt

```bash
./prompt.sh <session-name> "Fix the bug in src/main.rs and create a PR to dev"
```

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
