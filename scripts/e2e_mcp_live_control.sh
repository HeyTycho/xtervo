#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
XVFB=${E2E_XVFB_BIN:-$ROOT/.perf/xvfb/usr/bin/Xvfb}
[ -x "$XVFB" ] || { echo "Xvfb not found: $XVFB" >&2; exit 1; }
OUT=${E2E_OUT:-$ROOT/target/mcp-live-e2e}
mkdir -p "$OUT"
DISPLAY_NUM=${DISPLAY_NUM:-:49}
"$XVFB" "$DISPLAY_NUM" -screen 0 1280x800x24 -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XP=$!
trap 'kill "$XP" 2>/dev/null || true' EXIT
sleep 0.7
TERVO_BIN=${TERVO_BIN:-$ROOT/target/release/tervo} \
DISPLAY="$DISPLAY_NUM" E2E_OUT="$OUT" \
python3 "$ROOT/scripts/e2e_mcp_live_control.py"
