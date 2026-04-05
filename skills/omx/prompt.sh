#!/bin/bash
# clawhip × OMX — Send a prompt to an existing OMX session
# Usage: prompt.sh <session-name> "<prompt-text>"

set -euo pipefail

# Ensure Homebrew and Cargo bins are available (OpenClaw exec may not inherit full PATH)
export PATH="/opt/homebrew/bin:/opt/homebrew/sbin:$HOME/.cargo/bin:$PATH"

SESSION="${1:?Usage: $0 <session-name> \"<prompt-text>\"}"
PROMPT="${2:?Usage: $0 <session-name> \"<prompt-text>\"}"

if ! tmux has-session -t "$SESSION" 2>/dev/null; then
  echo "❌ Session not found: $SESSION"
  exit 1
fi

# Send the prompt text literally, then press Enter separately
tmux send-keys -t "$SESSION" -l "$PROMPT"
tmux send-keys -t "$SESSION" Enter

echo "✓ Sent to $SESSION (unverified): ${PROMPT:0:80}..."
