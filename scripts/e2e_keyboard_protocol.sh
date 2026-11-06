#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 键盘协议真实 e2e：独立 headless Xvfb :123 + 真实 tervo + 伪 TUI。
#
# 覆盖：
#   1. kitty：query → 收 CSI ? 0 u / push flags 后 CSI ? 3 u；
#      Ctrl+A / b / Shift+Up 的 CSI-u 编码与 press/release 配对。
#   2. modifyOtherKeys：CSI > 4 ; 2 m 后 Ctrl+A → CSI 27;5;97~，Shift+Up → CSI 1;2A。
#   3. 普通 shell：无协议 query、按键与旧编码逐字节一致。
#   4. keyboard_protocol="off"：query 不应答，按键仍是旧编码。
#
# 只 kill 本脚本启动的 Xvfb/OpenBox/app；不碰 :1 / 其它 display / 不 pkill。
# 用法：cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
#       ./scripts/e2e_keyboard_protocol.sh
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/keyboard-protocol}
mkdir -p "$OUT" "$OUT/obhome" /tmp/.X11-unix

BIN_DIR=${E2E_BIN_DIR:-$ROOT/target/debug}
BIN=${E2E_BIN:-$BIN_DIR/tervo}
XWIN=${E2E_XWIN:-$BIN_DIR/xwin}
XCLICK=${E2E_XCLICK:-$BIN_DIR/xclick}
XKEY=${E2E_XKEY:-$BIN_DIR/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done
PY=${PYTHON:-$(command -v python3)}
[ -x "$PY" ] || { echo "找不到 python3" >&2; exit 2; }

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

DISPLAY_NUM=${E2E_DISPLAY:-:123}
export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/obhome
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM 已被占用；为避免干扰其他会话，本脚本退出。" >&2
    exit 2
fi

XVFB_PID=""
OB_PID=""
APP_PID=""
WIN=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "== 启动 Xvfb $DISPLAY_NUM =="
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }

if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
fi

stop_app() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=""
    sleep 0.3
}

# run_case <tag> <mode> <keyboard_protocol>
run_case() {
    local tag=$1 mode=$2 proto=$3
    local dir="$OUT/$tag"
    rm -rf "$dir"; mkdir -p "$dir"
    printf 'theme = "tokyo-night"\nfont_size = 14\nkeyboard_protocol = "%s"\n' "$proto" > "$dir/config.toml"
    TERVO_CONFIG="$dir/config.toml" TERVO_LOG_DIR="$dir/logs" TERVO_KEYLOG=1 \
        "$BIN" --cols 80 --rows 20 --shell "$PY" -- "$ROOT/scripts/keyboard_protocol_tui.py" "$mode" "$dir" \
        >"$dir/app.log" 2>&1 &
    APP_PID=$!
    for _ in $(seq 1 150); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && [ -s "$dir/ready" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "[$tag] 找不到 Tervo 窗口 / ready；见 $dir/app.log" >&2; exit 2; }
    "$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
    # 点击 pane，确保焦点在隐藏 IME TextInput（按键入口）上。
    WIN_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    WIN_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    WIN_W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    WIN_H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
    "$XCLICK" "$((WIN_X + WIN_W / 2))" "$((WIN_Y + 32 + (WIN_H - 32) / 2))" >/dev/null
    sleep 0.5
    # Ctrl+A（不应触发任何内置快捷键）、普通 b、Shift+Up。
    "$XKEY" a --ctrl >/dev/null
    sleep 0.1
    "$XKEY" b >/dev/null
    sleep 0.1
    "$XKEY" --key up --shift >/dev/null
    # 伪 TUI 持续读 PTY 输入；done 标记后先排空再落盘。
    touch "$dir/done"
    for _ in $(seq 1 240); do [ -s "$dir/keys.bin" ] && break; sleep 0.05; done
    [ -s "$dir/keys.bin" ] || { echo "[$tag] 没等到 keys.bin；见 $dir/app.log" >&2; exit 2; }
    stop_app
}

check_file() {
    local tag=$1 name=$2 want_hex=$3
    local got
    got=$(od -An -tx1 -v "$OUT/$tag/$name" | tr -d ' \n')
    if [ "$got" != "$want_hex" ]; then
        echo "[$tag] $name 不匹配" >&2
        echo "  want: $want_hex" >&2
        echo "  got : $got" >&2
        echo "  keylog:" >&2
        grep -E '\[key\]|\[send\]|\[ime\]' "$OUT/$tag/app.log" >&2 || true
        exit 1
    fi
    echo "[$tag] $name OK ($want_hex)"
}

# 期望值（hex）：
# kitty query: CSI ? 0 u + CSI ? 3 u
KITTY_RESP=1b5b3f30751b5b3f3375
# Ctrl+A press/release + b + Shift+Up press/release
KITTY_KEYS=1b5b39373b35751b5b39373b353a3375621b5b313b32411b5b313b323a3341
# modify query: CSI > 4 ; 2 m
MODIFY_RESP=1b5b3e343b326d
# Ctrl+A（level 2）+ b + Shift+Up
MODIFY_KEYS=1b5b32373b353b39377e621b5b313b3241
# legacy：Ctrl+A=0x01 + b + Up(无修饰)
LEGACY_KEYS=01621b5b41

echo "== 1) kitty protocol =="
run_case kitty kitty auto
check_file kitty responses.bin "$KITTY_RESP"
check_file kitty keys.bin "$KITTY_KEYS"

echo "== 2) modifyOtherKeys level 2 =="
run_case modify modify auto
check_file modify responses.bin "$MODIFY_RESP"
check_file modify keys.bin "$MODIFY_KEYS"

echo "== 3) 普通 shell（无协议请求） =="
run_case plain plain auto
check_file plain responses.bin ""
check_file plain keys.bin "$LEGACY_KEYS"

echo "== 4) keyboard_protocol = off =="
run_case off off off
check_file off responses.bin ""
check_file off keys.bin "$LEGACY_KEYS"

echo "PASS: 键盘协议 e2e 全部通过（日志 $OUT）"
