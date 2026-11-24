#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# v2 AI 插件拒绝回归（Plugin API v3 host，headless Xvfb :146）。
#
# v3 MVP 不实现 AI provider（docs/PLUGIN_V3.md §3.2 assistant 只做占位），
# 宿主只加载 api = 3 / host_abi = 3。本脚本用旧示例 docs/examples/ai-echo
# （api = 2）做回归：
#   - 被发现并明确归类为 unsupported/invalid，绝不执行
#   - 不出现 initialize/initialized 握手、没有 ai.chat、没有 ai-echo 子进程
#   - 设置 → Plugins 页仍能打开并显示该行（截图，状态 unsupported）
#   - 旧的“输入 prompt → ai.chat → 回复”断言已移除：v3 下不存在这条产品路径
#
# 运行前可设置独立 runtime：
#   rt=$PWD/.tmp/rt-ai-panel; mkdir -p -m 700 "$rt"
#   env -u XTERVO_INSTANCE_DIR XDG_RUNTIME_DIR="$rt" E2E_DISPLAY=:162 \
#       ./scripts/e2e_ai_panel.sh
#
# 前置构建：
#   CARGO_HOME=... CARGO_TARGET_DIR=... cargo build -p xtervo-app \
#       --bin xtervo --bin xwin --bin xclick --bin xkey
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
BIN=${E2E_BIN:-$TARGET_DIR/debug/xtervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
XKEY=${E2E_XKEY:-$TARGET_DIR/debug/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done
for tool in xdpyinfo xwininfo xwd ffmpeg pgrep; do
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
mkdir -p "$OUT/plugins" "$OUT/home" "$OUT/logs" /tmp/.X11-unix
cp -r "$ROOT/docs/examples/ai-echo" "$OUT/plugins/ai-echo"
chmod +x "$OUT/plugins/ai-echo/plugin.py"
APP_LOG=$OUT/app.log
: >"$APP_LOG"
[ ! -e "$OUT/plugins-v3-state.toml" ] || { echo "测试目录残留 v3 状态文件" >&2; exit 2; }

# 明确锁定 fixture 是 legacy v2：如果示例被误改成 v3，这个回归就失去意义。
grep -q '^api = 2$' "$OUT/plugins/ai-echo/plugin.toml" \
    || { echo "docs/examples/ai-echo 不再是 api = 2（当前 fixture 已不是 v2 回归）" >&2; exit 2; }

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
export XDG_STATE_HOME="$OUT/home/.local/state"
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-$OUT/runtime}
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
export XDG_DATA_DIRS="$PERF/openbox/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export XDG_CONFIG_DIRS="$PERF/openbox/etc/xdg:${XDG_CONFIG_DIRS:-/etc/xdg}"
mkdir -p "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME"

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
wait_log "READY TIMING" 6 || fail "窗口未就绪"
"$XWIN" xtervo 20 20 --focus >/dev/null 2>&1 || true
sleep 0.7
WIN=$(xwininfo -root -tree | grep -i '"xtervo"' | head -1 | awk '{print $1}')
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

# 发现阶段就把 v2 归到 unsupported/invalid，且启动 0 个。
wait_log "[plugin-v3] 已发现 1 个插件（1 unsupported/invalid），启动 0 个已确认插件，0 个等待权限确认" 8 \
    || fail "v2 ai-echo 没有被归类为 unsupported/invalid"
wait_log "[plugin-v3] 设置页 1 个插件，0 个启用，0 条启用命令，1 unsupported/invalid" 4 \
    || fail "设置页没有把 v2 ai-echo 计为 unsupported/invalid"
pass "v2 ai-echo is unsupported/invalid, 0 started"

# 绝不能执行 v2 协议：没有握手、没有 ai.chat、没有子进程。
sleep 0.5
if grep -Fq "ai-echo: initialize/initialized 完成" "$APP_LOG"; then
    fail "v2 ai-echo 不应与 v3 宿主握手"
fi
if grep -Fq "ai.chat" "$APP_LOG"; then
    fail "v3 宿主不应出现 ai.chat 路径"
fi
if pgrep -f "$OUT/plugins/ai-echo/plugin.py" >/dev/null 2>&1; then
    fail "v2 ai-echo 进程不应被启动"
fi
[ ! -e "$OUT/plugins-v3-state.toml" ] || fail "未被用户启用的 v2 插件不应写入 v3 状态"
pass "v2 plugin never executed (no handshake / ai.chat / process)"

# 设置 → Plugins：旧 v2 行仍在列表里，状态 unsupported（截图证据）。
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
key plugin
sleep 0.6
click 300 148
wait_log "[ui] settings goto cat=5" 4 || fail "Plugins 搜索项没有跳到插件页"
sleep 0.8
shot settings-unsupported
pass "settings -> Plugins shows the unsupported v2 row"

echo "输出：$OUT/settings-unsupported.png"
echo "日志：$APP_LOG"
