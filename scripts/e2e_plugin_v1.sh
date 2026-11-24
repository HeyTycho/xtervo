#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 插件宿主黑盒验收（Plugin API v3，headless Xvfb :142）。
#
# 脚本名保留 e2e_plugin_v1.sh（历史路径/文档引用），fixture 已全部迁移到 v3：
#   - 常驻 process 插件：JSON-RPC 2.0 initialize/initialized 握手、命令结果浮层、
#     周期性 status.get/ui.status 状态栏、声明式 Theme Pack
#   - 崩溃插件：退避重启到上限（3 次）后 Failed，设置页显示原因
#   - 超时插件：command.invoke 超时上报 timeout，并重启该插件进程
#   - 坏 JSON 插件：坏行累计到 5 次后重启，超过重启上限后 Failed
#   - 持久化禁用插件：不启动、不派发命令（状态文件 enabled=false）
#
# 只启动/回收本脚本自己的 Xvfb/Openbox/app；display 被占用时复用、不 kill
# 别人的进程。运行前可设置独立 runtime：
#   rt=$PWD/.tmp/rt-plugin-v3; mkdir -p -m 700 "$rt"
#   env -u XTERVO_INSTANCE_DIR XDG_RUNTIME_DIR="$rt" E2E_DISPLAY=:161 \
#       ./scripts/e2e_plugin_v1.sh
#
# 前置构建：
#   CARGO_HOME=... CARGO_TARGET_DIR=... cargo build -p xtervo-app --bin xtervo \
#       --bin xwin --bin xclick --bin xkey
# 用法：
#   ./scripts/e2e_plugin_v1.sh
#   E2E_DISPLAY=:142 E2E_OUT=.logs/plugin-v3 ./scripts/e2e_plugin_v1.sh

set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/plugin-v3}
DISPLAY_NUM=${E2E_DISPLAY:-:142}
SCREEN=${E2E_SCREEN:-1280x900x24}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${E2E_BIN:-$TARGET_DIR/debug/xtervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
XKEY=${E2E_XKEY:-$TARGET_DIR/debug/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done
for tool in xdpyinfo xwininfo xwd ffmpeg; do
    command -v "$tool" >/dev/null 2>&1 || { echo "缺少依赖：$tool" >&2; exit 2; }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }
OPENBOX=${XTERVO_OPENBOX_BIN:-$PERF/openbox/usr/bin/openbox}
OPENBOX_LIB=${PERF}/openbox/usr/lib/x86_64-linux-gnu

rm -rf "$OUT"
mkdir -p "$OUT/plugins/status" "$OUT/plugins/off" "$OUT/plugins/crash" \
    "$OUT/plugins/timeout" "$OUT/plugins/bad" "$OUT/home" "$OUT/logs" /tmp/.X11-unix
APP_LOG=$OUT/app.log
: >"$APP_LOG"
XVFB_PID=""
OB_PID=""
APP_PID=""
OWN_DISPLAY=0
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    if [ "$OWN_DISPLAY" = 1 ] && [ -n "$XVFB_PID" ]; then
        kill "$XVFB_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# ---------------------------------------------------------------- fixtures
# 常驻插件：握手 + 命令 + 状态；permissions 按 PLUGIN_V3.md 声明，状态文件
# 里的权限指纹必须与 manifest 完全一致，否则宿主会等待重新确认而不启动。
cat > "$OUT/plugins/status/plugin.toml" <<'TOML'
api = 3
host_abi = 3
id = "dev.xtervo.status"
name = "status"
version = "1.0.0"
runtime = "process"
entry = "plugin.py"
description = "v3 fake status provider"

[permissions]
filesystem = "plugin"
network = false
process = false
clipboard = false
terminal_context = true
terminal_write = false
credentials = "own"

[contributes]
commands = true
status = true
events = ["startup"]
theme = "theme.toml"
assistant = false

[[commands]]
id = "greet"
title = "Status: Greet"
description = "Return a hello message"
TOML
cat > "$OUT/plugins/status/plugin.py" <<'PY'
#!/usr/bin/env python3
"""Minimal Plugin API v3 process plugin (JSON-RPC 2.0, one JSON per line)."""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "started.marker"), "a", encoding="utf-8") as fh:
    fh.write(f"{os.getpid()}\n")


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def respond(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def status_payload():
    return {"text": "build ok", "tooltip": "fake build status", "command": "greet"}


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    if not isinstance(message, dict) or "method" not in message:
        continue
    method = message["method"]
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        respond(request_id, {"protocol_version": 3})
    elif method == "initialized":
        send({"jsonrpc": "2.0", "id": 100, "method": "ui.status", "params": status_payload()})
    elif method == "command.invoke":
        if params.get("command") == "greet":
            respond(request_id, {"ok": True, "message": "hello world"})
        else:
            respond(request_id, {"ok": False, "message": "unknown command"})
    elif method == "status.get":
        respond(request_id, status_payload())
    elif method == "ping":
        respond(request_id, {"seq": params.get("seq", 0)})
    elif method == "shutdown":
        respond(request_id, {"ok": True})
        break
PY
chmod +x "$OUT/plugins/status/plugin.py"
cat > "$OUT/plugins/status/theme.toml" <<'TOML'
[colors.primary]
background = "#101010"
foreground = "#eeeeee"

[colors.cursor]
cursor = "#ffffff"

[colors.normal]
black = "#000000"
red = "#cc0000"
green = "#00cc00"
yellow = "#cccc00"
blue = "#0000cc"
magenta = "#cc00cc"
cyan = "#00cccc"
white = "#cccccc"

[colors.bright]
black = "#555555"
red = "#ff5555"
green = "#55ff55"
yellow = "#ffff55"
blue = "#5555ff"
magenta = "#ff55ff"
cyan = "#55ffff"
white = "#ffffff"
TOML

# 崩溃插件：initialize 应答后，收到 initialized 立刻 exit(3)。
cat > "$OUT/plugins/crash/plugin.toml" <<'TOML'
api = 3
host_abi = 3
id = "dev.xtervo.crash"
name = "crash"
version = "1.0.0"
runtime = "process"
entry = "plugin.py"
description = "crashes after handshake"
TOML
cat > "$OUT/plugins/crash/plugin.py" <<'PY'
#!/usr/bin/env python3
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "started.marker"), "a", encoding="utf-8") as fh:
    fh.write(f"{os.getpid()}\n")


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"protocol_version": 3}})
    elif method == "initialized":
        sys.exit(3)
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"seq": 0}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
        break
PY
chmod +x "$OUT/plugins/crash/plugin.py"

# 超时插件：reply-to-handshake 但永远不回复 command.invoke。
cat > "$OUT/plugins/timeout/plugin.toml" <<'TOML'
api = 3
host_abi = 3
id = "dev.xtervo.timeout"
name = "timeout"
version = "1.0.0"
runtime = "process"
entry = "plugin.py"
description = "never answers command.invoke"

[contributes]
commands = true

[[commands]]
id = "slow"
title = "Timeout: Slow"
description = "command.invoke deliberately never replies"
TOML
cat > "$OUT/plugins/timeout/plugin.py" <<'PY'
#!/usr/bin/env python3
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "started.marker"), "a", encoding="utf-8") as fh:
    fh.write(f"{os.getpid()}\n")


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"protocol_version": 3}})
    elif method == "command.invoke":
        # 故意保持沉默：宿主 10s 后必须上报 timeout 并重启本进程。
        pass
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"seq": 0}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
        break
PY
chmod +x "$OUT/plugins/timeout/plugin.py"

# 坏 JSON 插件：握手后连续输出 6 行非 JSON，超过 max_bad_lines=5。
cat > "$OUT/plugins/bad/plugin.toml" <<'TOML'
api = 3
host_abi = 3
id = "dev.xtervo.bad"
name = "bad"
version = "1.0.0"
runtime = "process"
entry = "plugin.py"
description = "emits malformed protocol lines"
TOML
cat > "$OUT/plugins/bad/plugin.py" <<'PY'
#!/usr/bin/env python3
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "started.marker"), "a", encoding="utf-8") as fh:
    fh.write(f"{os.getpid()}\n")


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"protocol_version": 3}})
    elif method == "initialized":
        for _ in range(6):
            sys.stdout.write("this-is-not-json\n")
        sys.stdout.flush()
        # 宿主会按坏行上限杀进程并重启；这里保持存活避免 EOF 抢先。
        time.sleep(60)
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"seq": 0}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
        break
PY
chmod +x "$OUT/plugins/bad/plugin.py"

# 持久化禁用插件：v3 的 enabled 在状态文件里，manifest 不认 enabled 字段。
cat > "$OUT/plugins/off/plugin.toml" <<'TOML'
api = 3
host_abi = 3
id = "dev.xtervo.off"
name = "off"
version = "0.1.0"
runtime = "process"
entry = "plugin.py"
description = "disabled in plugins-v3-state.toml"

[contributes]
commands = true

[[commands]]
id = "nope"
title = "Off: Nope"
TOML
cat > "$OUT/plugins/off/plugin.py" <<'PY'
#!/usr/bin/env python3
import os

with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "started.marker"), "a") as fh:
    fh.write(f"{os.getpid()}\n")
PY
chmod +x "$OUT/plugins/off/plugin.py"

cat > "$OUT/plugins-v3-state.toml" <<'TOML'
version = 1

[plugins."dev.xtervo.status"]
enabled = true
permissions_approved = true
permissions = ["filesystem=plugin", "terminal_context", "credentials=own"]

[plugins."dev.xtervo.crash"]
enabled = true
permissions_approved = true
permissions = []

[plugins."dev.xtervo.timeout"]
enabled = true
permissions_approved = true
permissions = []

[plugins."dev.xtervo.bad"]
enabled = true
permissions_approved = true
permissions = []

[plugins."dev.xtervo.off"]
enabled = false
permissions_approved = false
permissions = []
TOML

printf 'theme = "tokyo-night"\nfont_size = 15\nconfirm_close = false\nlanguage = "en"\n' > "$OUT/cfg.toml"

wait_log() {  # <needle> <timeout-seconds>
    local needle=$1 limit=$2
    for _ in $(seq 1 $((limit * 50))); do
        grep -Fq "$needle" "$APP_LOG" && return 0
        sleep 0.02
    done
    return 1
}
wait_re() {  # <extended-regex> <timeout-seconds>
    local pattern=$1 limit=$2
    for _ in $(seq 1 $((limit * 50))); do
        grep -Eq "$pattern" "$APP_LOG" && return 0
        sleep 0.02
    done
    return 1
}
count_log() { grep -Fc "$1" "$APP_LOG" 2>/dev/null || true; }

# 用可写 HOME/XDG 启动 Openbox（只读 HOME 下 Openbox 会因主题/缓存写失败退出，
# 无 WM 时 XTest 按键没有输入焦点，palette 快捷键会被丢）。
export HOME="$OUT/home"
export XDG_CACHE_HOME="$OUT/home/.cache"
export XDG_CONFIG_HOME="$OUT/home/.config"
export XDG_DATA_HOME="$OUT/home/.local/share"
export XDG_STATE_HOME="$OUT/home/.local/state"
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-$OUT/runtime}
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
# .perf/Openbox 是自包含前缀：主题/配置不在系统路径里，显式补 XDG dirs。
export XDG_DATA_DIRS="$PERF/openbox/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export XDG_CONFIG_DIRS="$PERF/openbox/etc/xdg:${XDG_CONFIG_DIRS:-/etc/xdg}"
mkdir -p "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME"

# ---------------------------------------------------------------- display/app
if DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1; then
    echo "复用已有 display $DISPLAY_NUM（测试结束不关闭）"
else
    "$XVFB_BIN" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
    XVFB_PID=$!
    OWN_DISPLAY=1
    for _ in $(seq 1 100); do DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
    DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1 || fail "Xvfb $DISPLAY_NUM 启动失败"
fi
export DISPLAY=$DISPLAY_NUM
if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
fi

XMODIFIERS=@im=none XTERVO_PLUGINS="$OUT/plugins" \
    XTERVO_CONFIG="$OUT/cfg.toml" XTERVO_LOG_DIR="$OUT/logs" \
    "$BIN" --cols 120 --rows 32 --shell /bin/sh -- -c 'sleep 900' >"$APP_LOG" 2>&1 &
APP_PID=$!

WIN=""
for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || fail "窗口未出现"
# 等首帧/输入就绪（READY TIMING 由 app 在窗口 show 后打印），避免丢首轮按键。
wait_log "READY TIMING" 6 || fail "窗口未就绪"
"$XWIN" xtervo 20 20 --focus >/dev/null 2>&1 || true
sleep 0.6
WIN=$(xwininfo -root -tree | grep -i '"xtervo"' | head -1 | awk '{print $1}')
ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
echo "window=$WIN ${W}x${H} at ${ABS_X},${ABS_Y}"

shot() {
    xwd -silent -id "$WIN" -out "$OUT/$1.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$1.xwd" "$OUT/$1.png"
    rm -f "$OUT/$1.xwd"
    echo "  shot $OUT/$1.png"
}
click() { "$XCLICK" $((ABS_X + $1)) $((ABS_Y + $2)) >/dev/null 2>&1 || true; }
key() { "$XKEY" "$@" >/dev/null 2>&1 || true; }

run_palette_command() {  # <query> <full-command-id>
    # Palette 关闭后焦点回到终端，再开下一次，避免隐藏 TextInput 吞快捷键。
    click 60 $((32 + 200))
    sleep 0.4
    key p --ctrl --shift
    sleep 0.5
    key "$1"
    sleep 0.4
    key --key enter
    wait_log "[ui] run command $2" 4 || fail "Palette 未派发命令 $2"
}

# ---------------------------------------------------------------- v3 assertions
wait_log "[plugin-v3] 已发现 5 个插件（0 unsupported/invalid），启动 4 个已确认插件，0 个等待权限确认" 8 \
    || fail "v3 发现/启动统计不符（期望 5 发现、4 启动、0 等待确认）"
wait_log "[plugin-v3] 设置页 5 个插件，4 个启用，2 条启用命令，0 unsupported/invalid" 4 \
    || fail "设置页插件统计不符"
wait_log "dev.xtervo.status: initialize/initialized 完成" 6 || fail "status 插件未完成 v3 握手"
wait_log "[theme] 插件 status 提供主题 custom:plugin-status-theme" 4 || fail "声明式 Theme Pack 未加载"
pass "v3 handshake + settings summary + theme pack"

# initialized 后插件主动 ui.status；状态栏文本通过宿主日志确认。
wait_re '\[plugin-v3\] status window=window-[0-9]+ text="status: build ok"' 6 \
    || fail "v3 状态栏没有收到 ui.status"
pass "status.get / ui.status -> status bar"

# Command Palette 执行 v3 插件命令（异步结果 → 浮层/活动）。
run_palette_command "greet" "plugin.dev.xtervo.status.greet"
wait_log "plugin.dev.xtervo.status.greet ok: hello world" 6 || fail "插件命令未返回文本"
wait_re "\[plugin-v3\] notice window=window-[0-9]+ plugin=dev\.xtervo\.status level=info" 4 \
    || fail "命令结果没有进入宿主 notice"
sleep 0.4
shot command_result
pass "command.invoke + result notice"

# 等 4s 浮层自动消失，状态栏仍在。
sleep 4.2
shot status
wait_re '\[plugin-v3\] status window=window-[0-9]+ text="status: build ok"' 2 || fail "状态栏文本消失"
pass "status bar stays after toast"

# 点击状态栏执行状态项声明的 command（短 id 由宿主归一化为完整 id）。
before=$(count_log "plugin.dev.xtervo.status.greet ok")
click 40 $((H - 14))
sleep 0.8
wait_log "状态栏点击 → plugin.dev.xtervo.status.greet" 3 || fail "状态栏点击未派发命令"
after=$(count_log "plugin.dev.xtervo.status.greet ok")
[ "$after" -gt "$before" ] || fail "状态栏点击未执行命令"
pass "status bar click command"
sleep 4.3

# 崩溃插件：重启到上限（3 次）后 Failed；设置页原因由运行时状态刷新。
wait_re "dev\\.xtervo\\.crash: .*连续失败，已停止自动重启（上限 3）" 12 \
    || fail "崩溃插件未在重启上限后禁用"
wait_log "dev.xtervo.crash: 插件进程退出（code=3）" 4 || fail "崩溃插件退出码未记录"
crash_starts=$(wc -l < "$OUT/plugins/crash/started.marker")
[ "$crash_starts" -eq 4 ] || fail "崩溃插件应启动 4 次（初始 + 3 次重启），实际 $crash_starts"
pass "crash -> backoff restart limit -> failed"

# 坏 JSON 插件：5 次坏行上限后重启，超过重启上限 Failed。
wait_log "dev.xtervo.bad: 坏 JSON/协议行（1/5）" 8 || fail "坏 JSON 未被计入协议错误"
wait_re "dev\\.xtervo\\.bad: .*连续失败，已停止自动重启（上限 3）" 12 \
    || fail "坏 JSON 插件未在重启上限后禁用"
bad_starts=$(wc -l < "$OUT/plugins/bad/started.marker")
[ "$bad_starts" -eq 4 ] || fail "坏 JSON 插件应启动 4 次（初始 + 3 次重启），实际 $bad_starts"
pass "bad JSON -> protocol restart limit -> failed"

# 超时插件：命令 10s 无响应 -> timeout 上报 + 重启进程（新实例再次握手）。
run_palette_command "timeout" "plugin.dev.xtervo.timeout.slow"
wait_log "plugin.dev.xtervo.timeout.slow timeout: 命令超时" 16 \
    || fail "command.invoke 超时没有上报 timeout"
wait_log "dev.xtervo.timeout: 命令超时（10s），重启插件进程" 4 || fail "命令超时未触发插件重启"
# 重启有 200ms backoff；等新实例完成握手后再数 marker。
for _ in $(seq 1 100); do
    [ "$(count_log "dev.xtervo.timeout: initialize/initialized 完成")" -ge 2 ] && break
    sleep 0.05
done
timeout_starts=$(wc -l < "$OUT/plugins/timeout/started.marker")
[ "$timeout_starts" -ge 2 ] || fail "超时插件没有重启（marker=$timeout_starts）"
timeout_handshakes=$(count_log "dev.xtervo.timeout: initialize/initialized 完成")
[ "$timeout_handshakes" -ge 2 ] || fail "超时重启后没有重新握手（$timeout_handshakes）"
pass "command timeout -> timeout outcome -> restart"

# 持久化禁用：状态文件 enabled=false，进程不启动、命令不可用。
sleep 0.5
[ ! -e "$OUT/plugins/off/started.marker" ] || fail "enabled=false 插件不应启动"
if grep -Fq "dev.xtervo.off: initialize/initialized 完成" "$APP_LOG"; then
    fail "enabled=false 插件不应握手"
fi
pass "disabled plugin not started"

# 设置 → Plugins：failed / disabled / 权限与运行状态由 UI 显示。
click 60 $((32 + 200))
sleep 0.4
key p --ctrl --shift
sleep 0.4
key "open settings"
sleep 0.4
key --key enter
wait_log "[ui] run command settings.open" 4 || fail "设置未打开"
sleep 0.8
click 800 56
sleep 0.3
key plugin
sleep 0.6
click 300 148
wait_log "[ui] settings goto cat=5" 4 || fail "搜索项没有跳到 Plugins 页"
sleep 0.9
shot plugins
pass "settings plugins page"

echo "输出：$OUT/{command_result,status,plugins}.png"
echo "日志：$APP_LOG"
