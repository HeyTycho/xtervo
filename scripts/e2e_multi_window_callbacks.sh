#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 双窗口完整回调验收（Phase 3.5）：
#   primary 与 secondary 共用 register_window_callbacks：
#   - 窗口 1 点齿轮打开设置 + 点侧栏进入插件页
#   - 窗口 2 Ctrl+Shift+P 命令面板 -> 搜索 settings -> Enter 打开设置 -> 插件页
#   - 两个窗口的 settings-category / settings_results / palette / plugin 模型互不串台
#   - SharedServices（cfg/plugins/vault/profiles/MCP/session）只有一份
#   - 关闭 primary 后窗口 2 的设置/插件页/命令面板/new_tab 仍可用
#
# 用法：./scripts/e2e_multi_window_callbacks.sh
# 可覆盖：E2E_OUT / E2E_DISPLAY / E2E_SCREEN / TERVO_BIN / E2E_INSTANCE_DIR

set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${E2E_OUT:-$ROOT/target/multi-window-callbacks}
DISPLAY_NUM=${E2E_DISPLAY:-:152}
SCREEN=${E2E_SCREEN:-2400x1000x24}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${TERVO_BIN:-$TARGET_DIR/debug/tervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
XKEY=${E2E_XKEY:-$TARGET_DIR/debug/xkey}
INSTANCE_DIR=${E2E_INSTANCE_DIR:-/tmp/tervo-mwc-callbacks-$$}
TOKEN="mw-callbacks-token"

XVFB_PID=""
APP_PID=""

cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || fail "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey"
done
command -v xdpyinfo >/dev/null || fail "缺少 xdpyinfo"
command -v xwininfo >/dev/null || fail "缺少 xwininfo"

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB=${E2E_XVFB:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
[ -x "$XVFB" ] || fail "找不到 Xvfb"

if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    fail "$DISPLAY_NUM 已被占用；本脚本不会 kill 别人的 display"
fi

rm -rf "$OUT"
mkdir -p "$OUT/logs" "$OUT/runtime" "$OUT/config" "$OUT/plugins/mwprobe"
chmod 700 "$OUT/runtime"
rm -rf "$INSTANCE_DIR"
mkdir -p "$INSTANCE_DIR"
chmod 700 "$INSTANCE_DIR"

"$XVFB" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do
    DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 || fail "Xvfb 启动失败（$OUT/xvfb.log）"

# ---------------------------------------------------------------- plugin fixture
cat >"$OUT/plugins/mwprobe/plugin.toml" <<'TOML'
api = 3
host_abi = 3
id = "dev.tervo.mwprobe"
name = "MW Probe"
version = "1.0.0"
runtime = "process"
entry = "plugin.py"
TOML
cat >"$OUT/plugins/mwprobe/plugin.py" <<'PY'
#!/usr/bin/env python3
import json
import sys

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except Exception:
        continue
    kind = msg.get("type")
    if kind == "init":
        print(json.dumps({"type": "ready", "api": 3}), flush=True)
    elif kind == "shutdown":
        break
PY
chmod +x "$OUT/plugins/mwprobe/plugin.py"
cat >"$OUT/config/plugins-v3-state.toml" <<'TOML'
version = 1

[plugins."dev.tervo.mwprobe"]
enabled = true
permissions_approved = true
permissions = []
TOML
cat >"$OUT/cfg.toml" <<'TOML'
version = 1
language = "en"
theme = "tokyo-night"
font_size = 15
cols = 120
rows = 32
confirm_close = false
shell = "/bin/sh"
TOML

export DISPLAY="$DISPLAY_NUM"
export XDG_RUNTIME_DIR="$OUT/runtime"
export XDG_CONFIG_HOME="$OUT/config"
export XDG_STATE_HOME="$OUT/state"
export TERVO_INSTANCE_DIR="$INSTANCE_DIR"
export TERVO_CONFIG="$OUT/cfg.toml"
export TERVO_LOG_DIR="$OUT/logs"
export TERVO_SESSION="$OUT/session.json"
export TERVO_PROFILES="$OUT/profiles.toml"
export TERVO_PLUGINS="$OUT/plugins"
export TERVO_DIAG_SOCKET="$OUT/live.sock"
export TERVO_DIAG_TOKEN="$TOKEN"
export TERVO_DIAG_ALLOW_WRITE=1
export TERVO_DIAG_ALLOW_DESTRUCTIVE=1
export SLINT_BACKEND=winit-software
export XMODIFIERS=@im=none

# ---------------------------------------------------------------- two windows
TERVO_MULTI_WINDOW_INPROC=1 TERVO_MULTI_WINDOW_SECOND=1 \
    "$BIN" --cols 120 --rows 32 --shell /bin/sh \
    >"$OUT/app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 240); do
    [ -S "$OUT/live.sock" ] && break
    sleep 0.05
done
[ -S "$OUT/live.sock" ] || fail "live socket 未就绪（见 $OUT/app.log）"
sleep 1.5

python3 "$ROOT/scripts/mw_callbacks_e2e.py" \
    --socket "$OUT/live.sock" --token "$TOKEN" \
    --xwin "$XWIN" --xclick "$XCLICK" --xkey "$XKEY" \
    --display "$DISPLAY_NUM" --pid "$APP_PID" \
    --log "$OUT/app.log" --out "$OUT" \
    --plugin "$OUT/plugins/mwprobe/plugin.py" || fail "双窗口回调 e2e 断言失败"
pass "multi-window callbacks e2e"

grep -E 'register window callbacks|settings category|palette open|settings open=|close' \
    "$OUT/app.log" >"$OUT/evidence.txt" || true
echo "evidence: $OUT"
echo "PASS multi-window shared callbacks"
