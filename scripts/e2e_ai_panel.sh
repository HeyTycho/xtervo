#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# AI Chat 面板 headless Xvfb 冒烟（无网络、无 API Key）：
#   - Settings → Plugins → Open AI Chat 能打开宿主渲染的面板
#   - docs/examples/ai-echo 作为 api=2 process 插件出现在列表
#   - 输入 prompt → Enter → ai.chat 走同一 supervisor，结果追加一次
#   - 活动/日志只记字符数，不记录 prompt/回复正文
#   - Close 返回设置页
#
# 前置构建：
#   CARGO_HOME=... CARGO_TARGET_DIR=... cargo build -p tervo-app \
#       --bin tervo --bin xwin --bin xclick --bin xkey
# 用法：
#   ./scripts/e2e_ai_panel.sh
#   E2E_DISPLAY=:146 E2E_OUT=.logs/ai-panel ./scripts/e2e_ai_panel.sh

set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/ai-panel}
DISPLAY_NUM=${E2E_DISPLAY:-:146}
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
mkdir -p "$OUT/plugins" "$OUT/home" "$OUT/logs" /tmp/.X11-unix
cp -r "$ROOT/docs/examples/ai-echo" "$OUT/plugins/ai-echo"
chmod +x "$OUT/plugins/ai-echo/plugin.py"
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
wait_log() {  # <needle> <timeout-seconds>
    local needle=$1 limit=$2
    for _ in $(seq 1 $((limit * 50))); do
        grep -Fq "$needle" "$APP_LOG" && return 0
        sleep 0.02
    done
    return 1
}

printf 'theme = "tokyo-night"\nconfirm_close = false\nlanguage = "en"\n' > "$OUT/cfg.toml"
export HOME="$OUT/home"
export XDG_CACHE_HOME="$OUT/home/.cache"
export XDG_CONFIG_HOME="$OUT/home/.config"
export XDG_DATA_HOME="$OUT/home/.local/share"
export XDG_DATA_DIRS="$PERF/openbox/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export XDG_CONFIG_DIRS="$PERF/openbox/etc/xdg:${XDG_CONFIG_DIRS:-/etc/xdg}"
mkdir -p "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"

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
wait_log "READY TIMING" 6 || fail "窗口未就绪"
"$XWIN" Tervo 20 20 --focus >/dev/null 2>&1 || true
sleep 0.7
WIN=$(xwininfo -root -tree | grep -i '"tervo"' | head -1 | awk '{print $1}')
ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
shot() {
    xwd -silent -id "$WIN" -out "$OUT/$1.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$1.xwd" "$OUT/$1.png"
    rm -f "$OUT/$1.xwd"
    echo "  shot $OUT/$1.png"
}
click() { "$XCLICK" $((ABS_X + $1)) $((ABS_Y + $2)) >/dev/null 2>&1 || true; }
key() { "$XKEY" "$@" >/dev/null 2>&1 || true; }

wait_log "[plugin] ai-echo: 常驻进程就绪" 6 || fail "ai-echo 插件未就绪"

# Settings → 搜 "ai chat" → 打开 Plugins 页的 AI 卡片。
click 60 232
sleep 0.3
key p --ctrl --shift
sleep 0.4
key "open settings"
sleep 0.4
key --key enter
wait_log "[ui] run command settings.open" 4 || fail "设置未打开"
sleep 0.9
click 800 56
sleep 0.2
key "ai chat"
sleep 0.6
click 300 148
wait_log "[ui] settings goto cat=5" 4 || fail "AI 搜索项没有跳到 Plugins"
sleep 0.7
shot settings-ai-card
click 650 209
wait_log "[ui] ai panel open plugins=1" 4 || fail "AI 面板未打开"
sleep 0.4
shot panel-empty

# 输入 prompt（Enter 提交），插件返回确定性 echo；只追加一次。
click 500 267
sleep 0.2
key "hello"
sleep 0.2
key --key enter
wait_log "[plugin] ai.chat submitted plugin=ai-echo messages=1" 4 || fail "ai.chat 未提交"
wait_log "[plugin] ai-echo: ai.chat ok" 6 || fail "ai-echo 未返回"
wait_log "[plugin] ai.chat result plugin=ai-echo" 6 || fail "UI 未收到 AI 结果"
sleep 0.5
shot panel-result
pass "AI panel opens and routes ai.chat through the process supervisor"

if grep -Fq "hello" "$APP_LOG"; then
    fail "prompt 正文不应写入应用日志"
fi
pass "prompt/response text is not logged"

# Close 返回设置页（不关设置 Tab）。
click 853 108
wait_log "[ui] ai panel closed" 4 || fail "AI 面板未关闭"
sleep 0.5
shot panel-closed
pass "Close returns to Settings"

echo "输出：$OUT/{settings-ai-card,panel-empty,panel-result,panel-closed}.png"
echo "日志：$APP_LOG"
