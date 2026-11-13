#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 单进程多窗口（Phase 2）黑盒验收：
#   - TERVO_MULTI_WINDOW_INPROC=1 + TERVO_MULTI_WINDOW_SECOND=1 启动两个 MainWindow
#   - 两个窗口独立 tabs/panes/focus/models；pane id 全局唯一
#   - 关掉 B 后 A 继续可用，进程不退出
#   - 插件进程只有一份、MCP live socket 只有一个
#   - 单窗口 kill -9 -> 重启自动恢复至少一个窗口
#
# 只启动/回收本脚本自己的 Xvfb/app；display 被占用时直接失败。
# 用法：./scripts/e2e_multi_window_inproc.sh
#      E2E_OUT=target/multi-window-e2e E2E_DISPLAY=:151 ./scripts/e2e_multi_window_inproc.sh

set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${E2E_OUT:-$ROOT/target/multi-window-e2e}
DISPLAY_NUM=${E2E_DISPLAY:-:151}
SCREEN=${E2E_SCREEN:-1280x800x24}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${TERVO_BIN:-$TARGET_DIR/debug/tervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
TOKEN="mw-e2e-token"

XVFB_PID=""
APP_PID=""
RECOVERY_PID=""

cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
    [ -n "$RECOVERY_PID" ] && kill "$RECOVERY_PID" 2>/dev/null
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

[ -x "$BIN" ] || fail "缺少 $BIN；先 cargo build -p tervo-app --bin tervo --bin xwin"
[ -x "$XWIN" ] || fail "缺少 $XWIN"
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
mkdir -p "$OUT/logs" "$OUT/runtime" "$OUT/config" "$OUT/plugins/mwprobe" "$OUT/shots"
chmod 700 "$OUT/runtime"

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

export DISPLAY="$DISPLAY_NUM"
export XDG_RUNTIME_DIR="$OUT/runtime"
export XDG_CONFIG_HOME="$OUT/config"
export XDG_STATE_HOME="$OUT/state"
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

# ---------------------------------------------------------------- multi window
TERVO_MULTI_WINDOW_INPROC=1 TERVO_MULTI_WINDOW_SECOND=1 \
    "$BIN" --cols 80 --rows 24 --shell /bin/sh \
    >"$OUT/app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 200); do
    [ -S "$OUT/live.sock" ] && break
    sleep 0.05
done
[ -S "$OUT/live.sock" ] || fail "live socket 未就绪（见 $OUT/app.log）"
sleep 1

python3 "$ROOT/scripts/mw_runtime_e2e.py" \
    --socket "$OUT/live.sock" --token "$TOKEN" \
    --xwin "$XWIN" --display "$DISPLAY_NUM" --pid "$APP_PID" \
    --log "$OUT/app.log" --out "$OUT" \
    --plugin "$OUT/plugins/mwprobe/plugin.py" || fail "多窗口 e2e 断言失败"
pass "multi-window e2e"

grep -E 'in-process window created|shared_plugins|mcp_socket|close requested|remaining' \
    "$OUT/app.log" >"$OUT/evidence.txt" || true
kill "$APP_PID" 2>/dev/null
wait "$APP_PID" 2>/dev/null
APP_PID=""

# ---------------------------------------------------------------- kill -9 recovery
REC=$OUT/recovery
mkdir -p "$REC/logs" "$REC/runtime" "$REC/config"
chmod 700 "$REC/runtime"
cat >"$REC/config/config.toml" <<'TOML'
restore_session_on_startup = true
cols = 80
rows = 24
shell = "/bin/sh"
TOML
cat >"$REC/config/plugins-v3-state.toml" <<'TOML'
version = 1
TOML

export XDG_RUNTIME_DIR="$REC/runtime"
export XDG_CONFIG_HOME="$REC/config"
export TERVO_CONFIG="$REC/config/config.toml"
export TERVO_LOG_DIR="$REC/logs"
export TERVO_SESSION="$REC/session.json"
export TERVO_DIAG_SOCKET="$REC/live.sock"
unset TERVO_MULTI_WINDOW_INPROC TERVO_MULTI_WINDOW_SECOND SLINT_BACKEND

start_recovery() {
    "$BIN" --cols 80 --rows 24 --shell /bin/sh \
        >>"$REC/app.log" 2>&1 &
    RECOVERY_PID=$!
    for _ in $(seq 1 200); do
        [ -S "$REC/live.sock" ] && break
        sleep 0.05
    done
    [ -S "$REC/live.sock" ] || fail "recovery live socket 未就绪"
    sleep 1
}

live_recovery() {
    python3 - "$REC/live.sock" "$TOKEN" "$1" "$2" <<'PY'
import json, socket, sys
sock, token, cmd, args = sys.argv[1], sys.argv[2], sys.argv[3], json.loads(sys.argv[4])
s = socket.socket(socket.AF_UNIX)
s.connect(sock)
s.sendall((json.dumps({"token": token, "cmd": cmd, "args": args}) + "\n").encode())
print(s.makefile().readline().strip())
PY
}

start_recovery
live_recovery new_tab '{}' >/dev/null
live_recovery new_tab '{}' >/dev/null
sleep 2.5
# kill -9：不写 running marker 的干净退出，留下 autosave 供下次恢复。
kill -9 "$RECOVERY_PID" 2>/dev/null
wait "$RECOVERY_PID" 2>/dev/null
RECOVERY_PID=""
sleep 0.5
[ -f "$REC/session.json" ] || fail "kill -9 前没有 session autosave"
start_recovery
TABS=$(live_recovery list '{}' | python3 -c 'import json,sys; d=json.load(sys.stdin)["data"]; print(sum(w["tabs"] for w in d["windows"]))')
[ "$TABS" -ge 2 ] || fail "kill -9 重启后只恢复 $TABS 个 tab（期望 >=2）"
printf 'restored_tabs=%s\nautosaved_before_kill=1\n' "$TABS" >"$REC/recovery.txt"
grep -E '异常退出|autosave|已按快照恢复' "$REC/app.log" >"$REC/recovery.log" || true
pass "kill -9 single-window recovery ($TABS tabs)"
kill "$RECOVERY_PID" 2>/dev/null
wait "$RECOVERY_PID" 2>/dev/null
RECOVERY_PID=""

echo "evidence: $OUT"
echo "PASS multi-window in-process e2e + recovery"
