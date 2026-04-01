#!/usr/bin/env bash
set -euo pipefail

TARGET_DIR="${1:-$PWD/.omx/hooks}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$TARGET_DIR"
install -m 0644 "$SCRIPT_DIR/clawhip-sdk.mjs" "$TARGET_DIR/clawhip-sdk.mjs"
install -m 0644 "$SCRIPT_DIR/clawhip-hook.mjs" "$TARGET_DIR/clawhip.mjs"

echo "Installed clawhip OMX hook bridge into $TARGET_DIR"
echo "Next: validate with 'omx hooks validate' and test with 'omx hooks test' from your OMX workspace."
