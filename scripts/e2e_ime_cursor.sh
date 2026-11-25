#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# IME 候选点 e2e：候选矩形必须由聚焦 pane 的终端光标决定，鼠标点击不能改变它。
#
# 场景：
#   1. 子进程开启鼠标上报（1000），硬件光标固定在 (0,0)；
#   2. 键入 abc，光标前进到 (0,3)，日志里的 spot 必须跟着变；
#   3. 在远处点击，mouse-reporting 点击不能把 spot 改成点击格（旧 bug）；
#   4. 打开侧栏后，spot 必须仍来自 pane 光标矩形。
#
# 只验证 X11 下 xtervo 自己计算的 spot（XTERVO_IME_LOG）；真实 Wayland + fcitx5
# 的候选窗位置仍必须人工验收，Xvfb 不能替代。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/ime-cursor}
mkdir -p "$OUT/home" "$OUT/logs" "$OUT/rt" /tmp/.X11-unix
chmod 700 "$OUT/rt"

BIN=${E2E_BIN:-$ROOT/target/debug/xtervo}
XWIN=$ROOT/target/debug/xwin
XCLICK=$ROOT/target/debug/xclick
XKEY=$ROOT/target/debug/xkey
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

DISPLAY_NUM=${E2E_DISPLAY:-:139}
export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/home
export XDG_RUNTIME_DIR=$OUT/rt
export XTERVO_INSTANCE_DIR=$OUT/instance
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
export XTERVO_LOG_DIR=$OUT/logs
export XTERVO_IME_LOG=1

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM 已被占用；退出以避免干扰。" >&2
    exit 2
fi

XVFB_PID="" OB_PID="" APP_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

cat > "$OUT/shell.sh" <<'SHELL'
#!/bin/sh
printf '\033[?1000h'
printf '\033[2J\033[H'
printf 'IME-CURSOR-TEST'
printf '\033[1;1Habc'
# 关回显：鼠标上报序列写到 PTY 后不能被 tty echoed 回来，否则测试自己会移动光标。
stty -echo
while IFS= read -r line; do :; done
SHELL
chmod +x "$OUT/shell.sh"

cat > "$OUT/cfg.toml" <<'CFG'
theme = "tokyo-night"
font_size = 15
tabbar_position = "top"
sidebar_visible = false
CFG

echo "== 启动 Xvfb $DISPLAY_NUM =="
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.4
fi

echo "== 启动 xtervo =="
XTERVO_CONFIG=$OUT/cfg.toml "$BIN" --cols 100 --rows 30 --title xtervo \
    --shell /bin/sh -- "$OUT/shell.sh" >"$OUT/parent.log" 2>&1 &
APP_PID=$!

WIN=""
for _ in $(seq 1 120); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}' || true)
    [ -n "${WIN:-}" ] && break
    sleep 0.05
done
[ -n "${WIN:-}" ] || { echo "FAIL: 没找到 xtervo 窗口"; exit 1; }
"$XWIN" xtervo 20 20 900 600 --focus >/dev/null 2>&1 || true
sleep 0.8

# 等待首个 spot；没有它说明日志开关/启动失败。
for _ in $(seq 1 100); do
    grep -q '\[ime\] spot' "$OUT/parent.log" 2>/dev/null && break
    sleep 0.05
done
if ! grep -q '\[ime\] spot' "$OUT/parent.log"; then
    echo "FAIL: 没有 [ime] spot 日志"; exit 1
fi

spot_cell() { grep '\[ime\] spot' "$OUT/parent.log" | tail -1 | sed -n 's/.*cell=\([0-9]*,[0-9]*\).*/\1/p'; }
spot_xy() { grep '\[ime\] spot' "$OUT/parent.log" | tail -1 | sed -n 's/.*win=\([0-9.-]*,[0-9.-]*\).*/\1/p'; }

echo "== 等待启动输出把光标推进到 0,3 =="
sleep 0.5
CELL=$(spot_cell)
echo "spot after output: cell=$CELL"
[ "$CELL" = "0,3" ] || { echo "FAIL: 输出后 spot 应为 0,3，实际 $CELL"; exit 1; }
# X11 无 cursor area：spot y 应在光标格底边（content_y 32 + (0+1)*cell_h 22）。
XY=$(spot_xy)
case "$XY" in
    *,54.0) ;;
    *) echo "FAIL: X11 spot y 应为 54.0（格底边），实际 $XY"; exit 1 ;;
esac

# 窗口绝对位置 + 内容区偏移（top tabbar 32px）：点击远处格。
AX=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
AY=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
echo "== 点击内容区 (300,200) =="
"$XCLICK" $((AX + 300)) $((AY + 32 + 200)) 1 >/dev/null 2>&1
sleep 0.5
CELL=$(spot_cell)
echo "spot after click: cell=$CELL"
if [ "$CELL" != "0,3" ]; then
    echo "FAIL: 点击后候选点被鼠标格污染：$CELL（应为 0,3）"
    exit 1
fi

echo "== 打开侧栏（Ctrl+Shift+B）=="
# 点击后显式把 X 输入焦点还给窗口；Openbox 在无主题 headless 下有时不接管焦点。
"$XWIN" "$WIN" --focus >/dev/null 2>&1 || true
sleep 0.2
BEFORE=$(spot_xy)
# 侧栏快捷键允许重试，避免把“按键没送到”误判成 spot 没重发。
AFTER=$BEFORE
for _ in 1 2 3; do
    "$XKEY" b --ctrl --shift >/dev/null 2>&1
    sleep 0.5
    AFTER=$(spot_xy)
    [ "$AFTER" != "$BEFORE" ] && break
done
echo "spot x before=$BEFORE after=$AFTER"
# 侧栏在左侧，内容原点必须右移 240px，且候选格仍是终端光标格。
CELL=$(spot_cell)
[ "$CELL" = "0,3" ] || { echo "FAIL: 侧栏开合后 spot 漂移：$CELL"; exit 1; }
[ "$AFTER" != "$BEFORE" ] || { echo "FAIL: 侧栏开合后 spot 没有重发（$BEFORE）"; exit 1; }
echo "== 滚动历史（Ctrl+Shift+Home/End）=="
BEFORE_SCROLL=$(spot_cell)
"$XKEY" --key home --ctrl --shift >/dev/null 2>&1
sleep 0.6
TOP=$(spot_cell)
"$XKEY" --key end --ctrl --shift >/dev/null 2>&1
sleep 0.6
BOTTOM=$(spot_cell)
echo "spot before=$BEFORE_SCROLL top=$TOP bottom=$BOTTOM"
[ "$TOP" != "$BEFORE_SCROLL" ] || { echo "FAIL: 上滚后 spot 没跟 Frame 视口光标走"; exit 1; }
[ "$BOTTOM" = "$BEFORE_SCROLL" ] || { echo "FAIL: 回底后 spot 没恢复：$BOTTOM"; exit 1; }

echo "PASS: IME spot 只跟随终端光标（cell=0,3，X11 y=54，点击不改变，侧栏 $BEFORE -> $AFTER，滚动 $BEFORE_SCROLL -> $TOP -> $BOTTOM）"
