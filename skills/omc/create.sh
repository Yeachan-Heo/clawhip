#!/bin/bash
# clawhip × OMC — Create a monitored OMC tmux session
# Usage: create.sh <session-name> <worktree-path> [channel-id] [mention]

set -euo pipefail

SESSION="${1:?Usage: $0 <session-name> <worktree-path> [channel-id] [mention]}"
WORKDIR="${2:?Usage: $0 <session-name> <worktree-path> [channel-id] [mention]}"
CHANNEL="${3:-}"
MENTION="${4:-}"

KEYWORDS="${CLAWHIP_OMC_KEYWORDS:-error,Error,FAILED,PR created,panic,complete}"
STALE_MIN="${CLAWHIP_OMC_STALE_MIN:-30}"
OMC_FLAGS="${CLAWHIP_OMC_FLAGS:---openclaw --madmax}"
OMC_ENV="${CLAWHIP_OMC_ENV:-}"

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

PROJECT="${CLAWHIP_OMC_PROJECT:-$(detect_project)}"

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

EMIT_ARGS=()
[ -n "$CHANNEL" ] && EMIT_ARGS+=(--channel "$CHANNEL")
[ -n "$MENTION" ] && EMIT_ARGS+=(--mention "$MENTION")
EMIT_SUFFIX=""
if [ ${#EMIT_ARGS[@]} -gt 0 ]; then
  printf -v EMIT_SUFFIX ' %q' "${EMIT_ARGS[@]}"
fi

quote() {
  printf '%q' "$1"
}

# Build the OMC command with native clawhip lifecycle emits
OMC_CMD=$(cat <<EOF
source ~/.zshrc
START_TS=\$(date +%s)
cleanup() {
  local exit_code=\$?
  local elapsed=\$(( \$(date +%s) - START_TS ))
  if [ "\$exit_code" -eq 0 ]; then
    clawhip emit agent.finished --agent omc --session $(quote "$SESSION") --project $(quote "$PROJECT") --elapsed "\$elapsed"$EMIT_SUFFIX || true
  else
    clawhip emit agent.failed --agent omc --session $(quote "$SESSION") --project $(quote "$PROJECT") --elapsed "\$elapsed" --error "exit \$exit_code"$EMIT_SUFFIX || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM
clawhip emit agent.started --agent omc --session $(quote "$SESSION") --project $(quote "$PROJECT")$EMIT_SUFFIX || true
${OMC_ENV:+$OMC_ENV }omc $OMC_FLAGS --worktree $(quote "$WORKDIR")
EOF
)

ARGS+=(-- "$OMC_CMD")

# Launch
nohup clawhip "${ARGS[@]}" &>/dev/null &

echo "✓ Created session: $SESSION in $WORKDIR (clawhip monitored)"
echo "  Project: $PROJECT"
echo "  Monitor: tmux attach -t $SESSION"
echo "  Tail:    $(dirname "$0")/tail.sh $SESSION"
