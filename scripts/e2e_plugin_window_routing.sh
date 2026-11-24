#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 插件 v3 per-window 路由黑盒验收（Xvfb + Openbox）：
#   - 同一插件进程服务两个窗口（MCP 一个 socket）
#   - command.invoke 带发起窗口；插件 terminal.read 指定 window_id，
#     ask 只出现在目标窗口，Allow 后读取该窗口的 pane
#   - 两个窗口各自显示不同 panel（ui.render window_id），互不覆盖
#   - ui.status 按窗口显示
#   - 关闭窗口清理该窗口的 surface/status，插件进程仍是一份
#   - MCP audit 日志带 window_id；最后一个窗口拒绝 close_window
#
# 只启动/回收本脚本自己的 Xvfb/Openbox/app；display 被占用时直接失败。
# 用法：./scripts/e2e_plugin_window_routing.sh

set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${E2E_OUT:-$ROOT/target/plugin-window-routing}
DISPLAY_NUM=${E2E_DISPLAY:-:153}
SCREEN=${E2E_SCREEN:-1280x800x24}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${XTERVO_BIN:-$TARGET_DIR/debug/xtervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
XKEY=${E2E_XKEY:-$TARGET_DIR/debug/xkey}
TOKEN="plugin-window-e2e-token"

XVFB_PID=""
OB_PID=""
APP_PID=""

cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || fail "缺少 $f；先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey"
done
for tool in xdpyinfo xwininfo xwd ffmpeg xwininfo; do
    command -v "$tool" >/dev/null || fail "缺少依赖：$tool"
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB=${E2E_XVFB:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
[ -x "$XVFB" ] || fail "找不到 Xvfb"
OPENBOX=${XTERVO_OPENBOX_BIN:-$PERF/openbox/usr/bin/openbox}
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    fail "$DISPLAY_NUM 已被占用；本脚本不会 kill 别人的 display"
fi

rm -rf "$OUT"
mkdir -p "$OUT/logs" "$OUT/runtime" "$OUT/config" "$OUT/home" "$OUT/plugins/winroute" "$OUT/shots"
chmod 700 "$OUT/runtime"

"$XVFB" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do
    DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 || fail "Xvfb 启动失败（$OUT/xvfb.log）"
export DISPLAY="$DISPLAY_NUM"

if [ -x "$OPENBOX" ]; then
    HOME="$OUT/home" XDG_CACHE_HOME="$OUT/home/.cache" \
        XDG_CONFIG_HOME="$OUT/home/.config" XDG_DATA_HOME="$OUT/home/.local/share" \
        XDG_DATA_DIRS="$PERF/openbox/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}" \
        XDG_CONFIG_DIRS="$PERF/openbox/etc/xdg:${XDG_CONFIG_DIRS:-/etc/xdg}" \
        LD_LIBRARY_PATH="$OPENBOX_LIB" "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
else
    fail "缺少 Openbox（e2e 需要 WM 提供输入焦点）：$OPENBOX"
fi

# ---------------------------------------------------------------- plugin fixture
cat >"$OUT/plugins/winroute/plugin.toml" <<'TOML'
api = 3
host_abi = 3
id = "dev.xtervo.winroute"
name = "Window Route Probe"
version = "1.0.0"
runtime = "process"
entry = "main.py"

[permissions]
filesystem = "none"
network = false
process = false
clipboard = false
terminal_context = true
terminal_write = false
credentials = "none"

[contributes]
commands = true
status = true
events = ["startup"]
panels = ["main"]
settings = true
assistant = false

[[commands]]
id = "read-window"
title = "Read Window"

[[commands]]
id = "panel-window"
title = "Panel Window"

[[commands]]
id = "status-window"
title = "Status Window"
TOML

cat >"$OUT/plugins/winroute/main.py" <<'PY'
#!/usr/bin/env python3
"""v3 plugin that only acts on the window_id supplied by the host."""
import json
import os
import sys

LOG = "e2e-plugin.log"
_pending = {}
_next = 100


def log(kind, **fields):
    fields["kind"] = kind
    with open(LOG, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(fields, sort_keys=True) + "\n")
        handle.flush()


def send(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def respond(rid, result):
    send({"jsonrpc": "2.0", "id": rid, "result": result})


def request(method, params):
    global _next
    rid = _next
    _next += 1
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    return rid


def on_response(msg):
    pending = _pending.pop(msg.get("id"), None)
    if pending is None:
        return
    kind, command_id, window = pending
    if "error" in msg:
        error = msg["error"]
        log("response-error", method=kind, window=window, message=str(error.get("message")))
        respond(command_id, {"ok": False, "message": "%s: %s" % (kind, error.get("message", "error"))})
        return
    result = msg.get("result") or {}
    if kind == "terminal.read":
        text = str(result.get("text", ""))
        log("read", window=window, text=text, truncated=bool(result.get("truncated")))
        # 宿主把通知只显示在目标窗口；文本用于 e2e 断言窗格内容。
        send({"jsonrpc": "2.0", "method": "ui.notify", "params": {
            "text": "READWIN[%s]:%s" % (window, text),
            "level": "info",
            "window_id": window,
        }})
        respond(command_id, {"ok": True, "message": "read window %s" % window})
    elif kind == "ui.render":
        log("panel", window=window)
        respond(command_id, {"ok": True, "message": "panel window %s" % window})
    else:
        respond(command_id, {"ok": True, "message": str(kind)})


def on_request(msg):
    method = msg.get("method")
    rid = msg.get("id")
    params = msg.get("params") or {}
    if method == "initialize":
        log("initialize")
        respond(rid, {"protocol_version": 3})
    elif method == "initialized":
        log("initialized")
    elif method == "ping":
        respond(rid, {"seq": params.get("seq", 0)})
    elif method == "status.get":
        respond(rid, {"text": "ready", "tooltip": "window-route"})
    elif method == "shutdown":
        respond(rid, {"ok": True})
        sys.exit(0)
    elif method == "surface.render":
        log("surface-render", window=params.get("window_id"), surface=params.get("surface"))
        respond(rid, {"version": 1, "title": "Surface %s" % params.get("window_id"),
                      "elements": [{"type": "text", "text": "surface-%s" % params.get("window_id")}]})
    elif method == "ui.action":
        log("ui-action", window=params.get("window_id"), element=params.get("element"))
    elif method == "event.deliver":
        payload = params.get("payload") or {}
        log("event", event=params.get("event"), window=payload.get("window_id"))
    elif method == "command.invoke":
        command = params.get("command")
        window = params.get("window_id")
        log("command", command=command, window=window)
        if command == "read-window":
            body = {"max_lines": 20, "max_bytes": 8192, "window_id": window}
            pending = request("terminal.read", body)
            _pending[pending] = ("terminal.read", rid, window)
        elif command == "panel-window":
            body = {
                "surface": "panel",
                "panel": "main",
                "window_id": window,
                "title": "PANEL[%s]" % window,
                "elements": [{"type": "text", "text": "panel-window-%s" % window}],
            }
            pending = request("ui.render", body)
            _pending[pending] = ("ui.render", rid, window)
        elif command == "status-window":
            send({"jsonrpc": "2.0", "method": "ui.status", "params": {
                "text": "status-window-%s" % window,
                "window_id": window,
            }})
            respond(rid, {"ok": True, "message": "status window %s" % window})
        else:
            respond(rid, {"ok": False, "message": "unknown command"})
    else:
        if rid is not None:
            send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unknown"}})


def main():
    log("started", pid=os.getpid())
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(message, dict):
            continue
        if "method" in message:
            on_request(message)
        else:
            on_response(message)


if __name__ == "__main__":
    main()
PY
chmod +x "$OUT/plugins/winroute/main.py"

cat >"$OUT/config/plugins-v3-state.toml" <<'TOML'
version = 1

[plugins."dev.xtervo.winroute"]
enabled = true
permissions_approved = true
permissions = ["terminal_context"]
TOML

cat >"$OUT/config/config.toml" <<'TOML'
version = 1
restore_session_on_startup = false
confirm_close = false
theme = "tokyo-night"
font_size = 13
mcp_enabled = true
mcp_read = true
mcp_input = false
mcp_tabs = true
mcp_destructive = true
TOML

export XDG_RUNTIME_DIR="$OUT/runtime"
export XDG_CONFIG_HOME="$OUT/config"
export XDG_STATE_HOME="$OUT/state"
export XTERVO_CONFIG="$OUT/config/config.toml"
export XTERVO_LOG_DIR="$OUT/logs"
export XTERVO_SESSION="$OUT/session.json"
export XTERVO_PROFILES="$OUT/profiles.toml"
export XTERVO_PLUGINS="$OUT/plugins"
export XTERVO_DIAG_SOCKET="$OUT/live.sock"
export XTERVO_DIAG_TOKEN="$TOKEN"
export SLINT_BACKEND=winit-software
export XMODIFIERS=@im=none
export HOME="$OUT/home"

XTERVO_MULTI_WINDOW_INPROC=1 XTERVO_MULTI_WINDOW_SECOND=1 \
    "$BIN" --cols 100 --rows 30 --shell /bin/sh \
    >"$OUT/app.log" 2>&1 &
APP_PID=$!

for _ in $(seq 1 300); do
    [ -S "$OUT/live.sock" ] && break
    sleep 0.05
done
[ -S "$OUT/live.sock" ] || fail "live socket 未就绪（见 $OUT/app.log）"

python3 "$ROOT/scripts/plugin_window_e2e.py" \
    --socket "$OUT/live.sock" --token "$TOKEN" --app-pid "$APP_PID" \
    --display "$DISPLAY_NUM" --xwin "$XWIN" --xclick "$XCLICK" --xkey "$XKEY" \
    --log "$OUT/app.log" --plugin-log "$OUT/plugins/winroute/e2e-plugin.log" --out "$OUT" \
    --plugin "$OUT/plugins/winroute/main.py" || fail "插件 per-window e2e 断言失败"
pass "plugin per-window routing"

kill "$APP_PID" 2>/dev/null
wait "$APP_PID" 2>/dev/null
APP_PID=""
