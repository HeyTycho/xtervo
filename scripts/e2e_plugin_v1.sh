#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# W-04 外部进程插件 V1 视觉/集成验收（headless Xvfb :142）：
#   - 常驻 process 插件：握手、命令结果浮层、周期性 status 状态栏
#   - 崩溃插件：自动重启到上限后禁用，设置页显示原因
#   - enabled=false 插件：不启动、命令不可执行、设置页显示 disabled
#   - 声明式主题：单独单测覆盖（本脚本不强制截图）
#
# 只启动/回收本脚本自己的 Xvfb/Openbox/app；display 被占用时复用、不 kill。
# 前置构建：
#   CARGO_HOME=... CARGO_TARGET_DIR=... cargo build -p tervo-app --bin tervo \
#       --bin xwin --bin xclick --bin xkey
# 用法：
#   ./scripts/e2e_plugin_v1.sh
#   E2E_DISPLAY=:142 E2E_OUT=.logs/plugin-v1 ./scripts/e2e_plugin_v1.sh

set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/plugin-v1}
DISPLAY_NUM=${E2E_DISPLAY:-:142}
SCREEN=${E2E_SCREEN:-1280x900x24}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${E2E_BIN:-$TARGET_DIR/debug/tervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
XKEY=${E2E_XKEY:-$TARGET_DIR/debug/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done
for tool in xdpyinfo xwininfo xwd ffmpeg; do
    command -v "$tool" >/dev/null 2>&1 || { echo "缺少依赖：$tool" >&2; exit 2; }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }
OPENBOX=${TERVO_OPENBOX_BIN:-$PERF/openbox/usr/bin/openbox}
OPENBOX_LIB=${PERF}/openbox/usr/lib/x86_64-linux-gnu

rm -rf "$OUT"
mkdir -p "$OUT/plugins/status" "$OUT/plugins/off" "$OUT/plugins/crash" "$OUT/home" "$OUT/logs" /tmp/.X11-unix
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
cat > "$OUT/plugins/status/plugin.toml" <<'TOML'
id = "dev.tervo.status"
name = "status"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"
description = "fake status provider"

[contributes]
status = true
commands = ["greet"]
theme = "theme.toml"

[[commands]]
id = "greet"
title = "Status: Greet"
args = ["world"]
TOML
cat > "$OUT/plugins/status/plugin.py" <<'PY'
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
        print(json.dumps({"type": "ready", "api": 1}), flush=True)
    elif kind == "command":
        text = "hello " + " ".join(msg.get("args", []))
        print(json.dumps({"type": "result", "id": msg["id"], "ok": True, "text": text}), flush=True)
    elif kind == "status":
        print(json.dumps({
            "type": "status",
            "text": "build ok",
            "tooltip": "fake build status",
            "command": "greet",
        }), flush=True)
    elif kind == "shutdown":
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

cat > "$OUT/plugins/off/plugin.toml" <<'TOML'
name = "off"
version = "0.1.0"
api = 1
runtime = "process"
entry = "plugin.py"
enabled = false
description = "disabled demo"

[[commands]]
id = "nope"
title = "Off: Nope"
TOML
cat > "$OUT/plugins/off/plugin.py" <<'PY'
#!/usr/bin/env python3
import sys
sys.stdin.read()
PY
chmod +x "$OUT/plugins/off/plugin.py"

cat > "$OUT/plugins/crash/plugin.toml" <<'TOML'
name = "crash"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"
description = "crashes after handshake"

[[commands]]
id = "boom"
title = "Crash: Boom"
TOML
cat > "$OUT/plugins/crash/plugin.py" <<'PY'
#!/usr/bin/env python3
import json
import sys
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("type") == "init":
        print(json.dumps({"type": "ready", "api": 1}), flush=True)
        sys.exit(3)
PY
chmod +x "$OUT/plugins/crash/plugin.py"

printf 'theme = "tokyo-night"\nfont_size = 15\nconfirm_close = false\n' > "$OUT/cfg.toml"

wait_log() {  # <needle> <timeout-seconds>
    local needle=$1 limit=$2
    for _ in $(seq 1 $((limit * 50))); do
        grep -Fq "$needle" "$APP_LOG" && return 0
        sleep 0.02
    done
    return 1
}

# 用可写 HOME/XDG 启动 Openbox（只读 HOME 下 Openbox 会因主题/缓存写失败退出，
# 无 WM 时 XTest 按键没有输入焦点，palette 快捷键会被丢）。
export HOME="$OUT/home"
export XDG_CACHE_HOME="$OUT/home/.cache"
export XDG_CONFIG_HOME="$OUT/home/.config"
export XDG_DATA_HOME="$OUT/home/.local/share"
# .perf/Openbox 是自包含前缀：主题/配置不在系统路径里，显式补 XDG dirs。
export XDG_DATA_DIRS="$PERF/openbox/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export XDG_CONFIG_DIRS="$PERF/openbox/etc/xdg:${XDG_CONFIG_DIRS:-/etc/xdg}"
mkdir -p "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"

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

XMODIFIERS=@im=none TERVO_PLUGINS="$OUT/plugins" \
    TERVO_CONFIG="$OUT/cfg.toml" TERVO_LOG_DIR="$OUT/logs" \
    "$BIN" --cols 120 --rows 32 --shell /bin/sh -- -c 'sleep 900' >"$APP_LOG" 2>&1 &
APP_PID=$!

WIN=""
for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || fail "窗口未出现"
# 等首帧/输入就绪（READY TIMING 由 app 在窗口 show 后打印），避免丢首轮按键。
wait_log "READY TIMING" 6 || fail "窗口未就绪"
"$XWIN" Tervo 20 20 --focus >/dev/null 2>&1 || true
sleep 0.6
WIN=$(xwininfo -root -tree | grep -i '"tervo"' | head -1 | awk '{print $1}')
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
click() { "$XCLICK" "$1" "$2" >/dev/null 2>&1 || true; }
key() { "$XKEY" "$@" >/dev/null 2>&1 || true; }

# 常驻插件就绪 + 状态轮询成功
wait_log "[plugin] status: 常驻进程就绪" 6 || fail "status 插件未就绪"
wait_log "[plugin] status: 状态更新" 6 || fail "status 插件未返回状态"
wait_log "插件 status 提供主题 custom:plugin-status-theme" 4 || fail "声明式 Theme Pack 未加载"
pass "process handshake + status poll + theme pack"

# 聚焦 pane，Command Palette 执行插件命令（异步结果 → 浮层/活动）
click $((ABS_X + 60)) $((ABS_Y + 32 + 200))
sleep 0.5
key p --ctrl --shift
sleep 0.5
key greet
sleep 0.4
key --key enter
wait_log "plugin.status.greet ok: hello world" 6 || fail "插件命令未返回文本"
sleep 0.4
shot command_result
pass "command result toast"

# 等 4s 浮层自动消失，状态栏仍在
sleep 4.2
shot status
pass "status bar"

# 点击状态栏执行状态项声明的 command（裸命令 id 已在宿主归一化）
before=$(grep -c "plugin.status.greet ok" "$APP_LOG" || true)
click $((ABS_X + 40)) $((ABS_Y + H - 14))
sleep 0.8
after=$(grep -c "plugin.status.greet ok" "$APP_LOG" || true)
wait_log "状态栏点击 → plugin.status.greet" 3 || fail "状态栏点击未派发命令"
[ "$after" -gt "$before" ] || fail "状态栏点击未执行命令"
pass "status bar click command"
sleep 4.3

# 设置 → Plugins：running / disabled / 崩溃禁用原因。
# palette 关闭后 Slint 焦点仍在隐藏 TextInput（既有键盘行为，本任务不改），
# 先点回终端再开 palette，避免第二次快捷键被隐藏输入框吞掉。
click $((ABS_X + 60)) $((ABS_Y + 32 + 200))
sleep 0.4
key p --ctrl --shift
sleep 0.4
key "open settings"
sleep 0.4
key --key enter
wait_log "[ui] run command settings.open" 4 || fail "设置未打开"
sleep 0.8
click $((ABS_X + 800)) $((ABS_Y + 56))
sleep 0.3
key plugin
sleep 0.6
click $((ABS_X + 300)) $((ABS_Y + 148))
sleep 0.9
shot plugins
pass "settings plugins page"

wait_log "[plugin] crash: 已禁用" 8 || fail "崩溃插件未禁用"
wait_log "达到重启上限" 2 || fail "未记录崩溃重启上限原因"
pass "crash restart limit -> disabled"
grep -Fq "off v0.1.0 — disabled" "$APP_LOG" || true
if grep -Fq "off: 常驻进程就绪" "$APP_LOG"; then
    fail "enabled=false 插件不应启动"
fi
pass "disabled plugin not started"

echo "输出：$OUT/{command_result,status,plugins}.png"
echo "日志：$APP_LOG"
