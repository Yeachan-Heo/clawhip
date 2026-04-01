#!/bin/bash
# clawhip × OMX — Create a monitored OMX tmux session
# Usage: create.sh <session-name> <worktree-path> [prompt] [channel-id] [mention]

set -euo pipefail

SESSION="${1:?Usage: $0 <session-name> <worktree-path> [prompt] [channel-id] [mention]}"
WORKDIR="${2:?Usage: $0 <session-name> <worktree-path> [prompt] [channel-id] [mention]}"
PROMPT="${3:-}"
CHANNEL="${4:-}"
MENTION="${5:-}"

KEYWORDS="${CLAWHIP_OMX_KEYWORDS:-error,Error,FAILED,PR created,panic,complete}"
STALE_MIN="${CLAWHIP_OMX_STALE_MIN:-30}"
OMX_FLAGS="${CLAWHIP_OMX_FLAGS:---madmax}"
OMX_ENV="${CLAWHIP_OMX_ENV:-}"

if [ ! -d "$WORKDIR" ]; then
  echo "❌ Directory not found: $WORKDIR"
  exit 1
fi

detect_project() {
  local common_dir
  common_dir="$(git -C "$WORKDIR" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
  if [ -n "$common_dir" ]; then
    basename "$(dirname "$common_dir")"
  else
    basename "$WORKDIR"
  fi
}

PROJECT="${CLAWHIP_OMX_PROJECT:-$(detect_project)}"

# Build clawhip tmux new args
ARGS=(
  tmux new
  -s "$SESSION"
  -c "$WORKDIR"
  --keywords "$KEYWORDS"
  --stale-minutes "$STALE_MIN"
)

[ -n "$CHANNEL" ] && ARGS+=(--channel "$CHANNEL")
[ -n "$MENTION" ] && ARGS+=(--mention "$MENTION")

quote() {
  printf '%q' "$1"
}

# Build the OMX command with native clawhip hook-envelope lifecycle emits.
OMX_CMD=$(cat <<EOF
source ~/.zshrc
START_TS=\$(date +%s)
REPO_ROOT=\$(git -C $(quote "$WORKDIR") rev-parse --show-toplevel 2>/dev/null || printf %s $(quote "$WORKDIR"))
BRANCH=\$(git -C $(quote "$WORKDIR") rev-parse --abbrev-ref HEAD 2>/dev/null || true)
emit_omx_event() {
  local raw_event="\$1"
  local normalized_event="\$2"
  local status="\$3"
  local summary="\${4:-}"
  local error_summary="\${5:-}"
  local elapsed="\${6:-}"
  CLAWHIP_EVENT="\$raw_event" \\
  CLAWHIP_NORMALIZED_EVENT="\$normalized_event" \\
  CLAWHIP_STATUS="\$status" \\
  CLAWHIP_SUMMARY="\$summary" \\
  CLAWHIP_ERROR_SUMMARY="\$error_summary" \\
  CLAWHIP_ELAPSED="\$elapsed" \\
  CLAWHIP_SESSION=$(quote "$SESSION") \\
  CLAWHIP_PROJECT=$(quote "$PROJECT") \\
  CLAWHIP_REPO_PATH="\$REPO_ROOT" \\
  CLAWHIP_WORKTREE_PATH=$(quote "$WORKDIR") \\
  CLAWHIP_BRANCH="\$BRANCH" \\
  CLAWHIP_CHANNEL=$(quote "$CHANNEL") \\
  CLAWHIP_MENTION=$(quote "$MENTION") \\
  node <<'NODE' | clawhip omx hook || true
const clean = (value) => (typeof value === 'string' ? value.trim() : '');
const number = (value) => {
  const parsed = Number.parseInt(clean(value), 10);
  return Number.isFinite(parsed) ? parsed : undefined;
};
const payload = {
  schema_version: '1',
  event: clean(process.env.CLAWHIP_EVENT) || 'notify',
  timestamp: new Date().toISOString(),
  source: 'native',
  context: {
    normalized_event: clean(process.env.CLAWHIP_NORMALIZED_EVENT),
    agent_name: 'omx',
    session_name: clean(process.env.CLAWHIP_SESSION),
    status: clean(process.env.CLAWHIP_STATUS),
    project: clean(process.env.CLAWHIP_PROJECT),
    repo_path: clean(process.env.CLAWHIP_REPO_PATH),
    worktree_path: clean(process.env.CLAWHIP_WORKTREE_PATH),
    branch: clean(process.env.CLAWHIP_BRANCH),
  },
  session_id: clean(process.env.CLAWHIP_SESSION),
};
const issueNumber = number(process.env.CLAWHIP_SESSION.match(/issue-(\\d+)/)?.[1] ?? '');
if (issueNumber !== undefined) payload.context.issue_number = issueNumber;
const summary = clean(process.env.CLAWHIP_SUMMARY);
if (summary) payload.context.summary = summary;
const errorSummary = clean(process.env.CLAWHIP_ERROR_SUMMARY);
if (errorSummary) payload.context.error_summary = errorSummary;
const elapsed = number(process.env.CLAWHIP_ELAPSED);
if (elapsed !== undefined) payload.context.elapsed_secs = elapsed;
const channel = clean(process.env.CLAWHIP_CHANNEL);
if (channel) payload.channel = channel;
const mention = clean(process.env.CLAWHIP_MENTION);
if (mention) payload.mention = mention;
process.stdout.write(JSON.stringify(payload));
NODE
}
cleanup() {
  local exit_code=\$?
  local elapsed=\$(( \$(date +%s) - START_TS ))
  if [ "\$exit_code" -eq 0 ]; then
    emit_omx_event session-end finished finished "session finished" "" "\$elapsed"
  else
    emit_omx_event session-end failed failed "session failed" "exit \$exit_code" "\$elapsed"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM
emit_omx_event session-start started started "session started"
${OMX_ENV:+$OMX_ENV }omx $OMX_FLAGS
EOF
)

ARGS+=(-- "$OMX_CMD")

# Launch
nohup clawhip "${ARGS[@]}" &>/dev/null &

echo "✓ Created session: $SESSION in $WORKDIR (clawhip monitored)"
echo "  Project: $PROJECT"
echo "  Monitor: tmux attach -t $SESSION"
echo "  Tail:    $(dirname "$0")/tail.sh $SESSION"

if [ -n "$PROMPT" ]; then
  sleep 10
  tmux send-keys -t "$SESSION" -l "$PROMPT"
  tmux send-keys -t "$SESSION" Enter
  echo "  Prompt: sent literal text after 10s init delay"
fi
