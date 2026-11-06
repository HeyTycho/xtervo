#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 光标外观验收截图：block / underline / ibeam / blink on-off / bold-is-bright。
#
# - 固定 headless Xvfb :117，不占用 :1、不启动 Xephyr、不 pkill；
# - 只回收本脚本启动的 app 与 Xvfb 进程；
# - 每个 case 单独写 TERVO_CONFIG 并重启 app，输出 PNG 到 .logs/cursor-style/。
#
# 用法：
#   cargo build -p tervo-app --bin tervo
#   scripts/cursor_style_shot.sh [输出目录]
#
# 环境变量：
#   CURSOR_SHOT_BIN      tervo 可执行文件（默认 $ROOT/target/debug/tervo）
#   CURSOR_SHOT_DISPLAY  X display（默认 :117；已占用则直接失败，绝不复用/ kill）
#   CURSOR_SHOT_XVFB     Xvfb 可执行文件（默认 .perf/xvfb/usr/bin/Xvfb，再退回 PATH）
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT_DIR=${1:-"$ROOT/.logs/cursor-style"}
mkdir -p "$OUT_DIR"
OUT_DIR=$(cd "$OUT_DIR" && pwd)
BIN=${CURSOR_SHOT_BIN:-"$ROOT/target/debug/tervo"}
[ -x "$BIN" ] || {
    echo "缺少 $BIN；先 cargo build -p tervo-app --bin tervo" >&2
    exit 1
}

XVFB=${CURSOR_SHOT_XVFB:-}
if [ -z "$XVFB" ]; then
    for cand in "$ROOT/.perf/xvfb/usr/bin/Xvfb" "$ROOT/../../.perf/xvfb/usr/bin/Xvfb"; do
        [ -x "$cand" ] && { XVFB=$cand; break; }
    done
fi
[ -z "$XVFB" ] && XVFB=$(command -v Xvfb || true)
[ -n "$XVFB" ] && [ -x "$XVFB" ] || {
    echo "找不到 Xvfb（CURSOR_SHOT_XVFB 可指定）" >&2
    exit 1
}
command -v xwd >/dev/null || { echo "缺少 xwd（x11-apps）" >&2; exit 1; }
command -v xwininfo >/dev/null || { echo "缺少 xwininfo（x11-utils）" >&2; exit 1; }
command -v python3 >/dev/null || { echo "缺少 python3" >&2; exit 1; }

DISPLAY_NUM=${CURSOR_SHOT_DISPLAY:-:117}
if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    echo "$DISPLAY_NUM 已被占用；本脚本不会复用或 kill 别人的 X，换 CURSOR_SHOT_DISPLAY" >&2
    exit 1
fi

XVFB_LOG="$OUT_DIR/cursor-xvfb.log"
APP_LOG="$OUT_DIR/cursor-app.log"
DEMO="$OUT_DIR/cursor-demo.sh"
CFG="$OUT_DIR/cursor-config.toml"
: >"$APP_LOG"
: >"$XVFB_LOG"

# 光标放在第 2 行第 4 列 = 'D'，这样 block 的“反色字符”也能被截图看到。
cat >"$DEMO" <<'DEMO_EOF'
#!/bin/bash
printf '\033[2J\033[H'
printf 'cursor style / blink / bold demo\n'
printf 'ABCDEFGHIJKLMNOPQRSTUVWXYZ\n'
printf '\033[1;31mnormal-red\033[0m  \033[1;31mBOLD-RED\033[0m  \033[1mBOLD-DEFAULT\033[0m\n'
printf '\033[2;4H'
sleep 600
DEMO_EOF
chmod +x "$DEMO"

APID=
XPID=
cleanup() {
    [ -n "${APID:-}" ] && kill "$APID" 2>/dev/null || true
    [ -n "${XPID:-}" ] && kill "$XPID" 2>/dev/null || true
    rm -f "$DEMO" "$CFG" "$CFG.v0.bak"
}
trap cleanup EXIT INT TERM

DISPLAY="$DISPLAY_NUM" "$XVFB" "$DISPLAY_NUM" -screen 0 900x620x24 -ac -nolisten tcp \
    >"$XVFB_LOG" 2>&1 &
XPID=$!
for _ in $(seq 1 100); do
    DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 || {
    echo "Xvfb $DISPLAY_NUM 启动失败（$XVFB_LOG）" >&2
    exit 1
}

# case 名 | cursor_shape | cursor_blink | 额外配置 | 冻结的闪烁相位
# 相位冻结由 TERVO_CURSOR_BLINK_PHASE 测试钩子完成，截图不会赌计时器相位。
run_case() {
    local name=$1 shape=$2 blink=$3 extra=$4 phase=${5:-}
    cat >"$CFG" <<EOF
version = 1
theme = "yaru-dark"
cursor_shape = "$shape"
cursor_blink = "$blink"
cursor_color = "#ff5f56"
cursor_text_color = "#101010"
$extra
EOF
    local envs=(
        "DISPLAY=$DISPLAY_NUM"
        "XMODIFIERS=@im=none"
        "TERVO_LOG_DIR=$OUT_DIR"
        "TERVO_CONFIG=$CFG"
    )
    if [ -n "$phase" ]; then
        envs+=("TERVO_CURSOR_BLINK_PHASE=$phase")
    fi
    env "${envs[@]}" "$BIN" --cols 84 --rows 18 --font-size 16 --shell /bin/bash -- \
        -c "$DEMO" >>"$APP_LOG" 2>&1 &
    APID=$!

    local win=
    for _ in $(seq 1 120); do
        win=$(DISPLAY="$DISPLAY_NUM" xwininfo -root -tree 2>/dev/null \
            | grep -iE '"tervo" "tervo"' | awk '{print $1}' | head -1 || true)
        [ -n "$win" ] && break
        sleep 0.05
    done
    [ -n "$win" ] || {
        echo "case $name：没找到 tervo 窗口（$APP_LOG）" >&2
        return 1
    }
    sleep 2.2
    local xwd="$OUT_DIR/$name.xwd"
    DISPLAY="$DISPLAY_NUM" xwd -id "$win" -silent -out "$xwd"
    python3 "$ROOT/scripts/xwd2png.py" "$xwd" "$OUT_DIR/$name.png" >/dev/null
    rm -f "$xwd"
    echo "case $name -> $OUT_DIR/$name.png"
    kill "$APID" 2>/dev/null || true
    wait "$APID" 2>/dev/null || true
    APID=
}

EXTRA_BOLD_ON='bold_is_bright = true'
EXTRA_BOLD_OFF='bold_is_bright = false'
run_case block-blink-on block on "$EXTRA_BOLD_ON" on
run_case block-blink-off block on "$EXTRA_BOLD_ON" off
run_case block-config-off block off "$EXTRA_BOLD_ON"
run_case underline-blink-on underline on "$EXTRA_BOLD_ON" on
run_case ibeam-blink-on ibeam on "$EXTRA_BOLD_ON" on
run_case bold-is-bright-on block off "$EXTRA_BOLD_ON"
run_case bold-is-bright-off block off "$EXTRA_BOLD_OFF"

echo "截图目录：$OUT_DIR"
