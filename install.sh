#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEMD=0
for arg in "$@"; do
  case "$arg" in
    --systemd) SYSTEMD=1 ;;
    *) echo "unknown arg: $arg" >&2; exit 1 ;;
  esac
done

cd "$REPO_ROOT"
echo "[clawhip] installing from $REPO_ROOT"
cargo install --path . --force
mkdir -p "$HOME/.clawhip"
echo "[clawhip] ensured config dir $HOME/.clawhip"

if [[ "$SYSTEMD" == "1" ]]; then
  sudo cp deploy/clawhip.service /etc/systemd/system/clawhip.service
  sudo systemctl daemon-reload
  sudo systemctl enable --now clawhip
  echo "[clawhip] systemd unit installed and started"
fi

echo "[clawhip] install complete"
