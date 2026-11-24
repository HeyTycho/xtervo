#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 查找 UX e2e：匹配计数 / Enter・Shift+Enter / 非法 regex / 鼠标编辑 /
# Ctrl+A/C/V（仅搜索框）/ Esc 后焦点归还 / Ctrl+F 打开即聚焦。
#
# 在独立 headless Xvfb 上运行（默认 :127），只 kill 本脚本启动的 Xvfb/OpenBox/app，
# 不占用 :1、不启动 Xephyr、不 pkill 其他进程。
#
# 用法：
#   CARGO_TARGET_DIR=$PWD/.target-search ./scripts/e2e_search_ux.sh
#   （或显式 E2E_BIN/E2E_XWIN/E2E_XCLICK/E2E_XKEY 指向已构建二进制）
#
# 依赖：Xvfb、OpenBox（仓库 .perf/）、xwd、ffmpeg、xsel。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/search-ux}
DISPLAY_NUM=${E2E_DISPLAY:-:127}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${E2E_BIN:-$TARGET_DIR/debug/xtervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
XKEY=${E2E_XKEY:-$TARGET_DIR/debug/xkey}
mkdir -p "$OUT" "$OUT/home" "$OUT/logs" /tmp/.X11-unix

for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done
command -v xsel >/dev/null || { echo "缺少 xsel（Ctrl+C 剪贴板断言需要）" >&2; exit 2; }

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
export XTERVO_CONFIG=$OUT/config.toml
export XTERVO_LOG_DIR=$OUT/logs

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM 已被占用；本脚本退出以避免干扰。" >&2
    exit 2
fi

XVFB_PID=""
OB_PID=""
APP_PID=""
CLIP_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
    [ -n "$CLIP_PID" ] && kill "$CLIP_PID" 2>/dev/null || true
}
trap cleanup EXIT

cat > "$OUT/config.toml" <<'CFG'
theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false
confirm_multiline_paste = false
CFG
cat > "$OUT/shell.sh" <<SHELL
#!/bin/sh
printf 'alpha one\\nbeta alpha\\nALPHA three\\ngamma\\nREADY\\n'
while IFS= read -r line; do
  printf '%s\\n' "\$line" >> "$OUT/input.txt"
  printf 'OUT:%s\\n' "\$line"
done
SHELL
chmod +x "$OUT/shell.sh"
rm -f "$OUT/input.txt" "$OUT/app.log" "$OUT"/*.png

echo "== 启动 Xvfb $DISPLAY_NUM =="
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }

if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
else
    echo "提示：未找到仓库 .perf/openbox，继续无 WM 运行" >&2
fi

"$BIN" --cols 80 --rows 20 --title xtervo --shell /bin/sh -- "$OUT/shell.sh" >"$OUT/app.log" 2>&1 &
APP_PID=$!
WIN=""
for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || { echo "找不到 xtervo 窗口；见 $OUT/app.log" >&2; tail -40 "$OUT/app.log" >&2; exit 2; }
"$XWIN" xtervo 20 20 >/dev/null 2>&1 || true
sleep 1.2

read -r ABS_X ABS_Y W H < <(xwininfo -id "$WIN" | awk '
    /Absolute upper-left X/{ax=$NF}
    /Absolute upper-left Y/{ay=$NF}
    /Width:/{w=$2}
    /Height:/{h=$2}
    END{print ax, ay, w, h}')
echo "  window=$WIN at $ABS_X,$ABS_Y ${W}x${H}"

key() { "$XKEY" "$@" >/dev/null 2>&1; sleep 0.16; }
click() { "$XCLICK" "$@" >/dev/null 2>&1; sleep 0.22; }
shot() {
    xwd -silent -id "$WIN" -out "$OUT/$1.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$1.xwd" "$OUT/$1.png"
    rm -f "$OUT/$1.xwd"
}
fail() {
    echo "FAIL: $*" >&2
    echo "--- app log tail ---" >&2
    tail -120 "$OUT/app.log" >&2
    exit 1
}
expect_log() { grep -Fq -- "$1" "$OUT/app.log" || fail "日志缺少: $1"; }
last_query() {
    grep -o '\[ui\] find query="[^"]*"' "$OUT/app.log" | tail -1 | sed -E 's/.*query="(.*)"/\1/'
}

# 查找条：右上角 x = W-388、宽 360；tabbar 高 32 + 8px 间距。
BAR_X=$((ABS_X + W - 388))
BAR_Y=$((ABS_Y + 32 + 8))
# 第一行：padding 7 + 放大镜 + 8px 间距；输入框文本起点约 BAR_X+29。
INPUT_Y=$((BAR_Y + 17))
INPUT_LEFT=$((BAR_X + 33))
INPUT_RIGHT=$((BAR_X + 280))
# 第二行开关：FindToggle 86x20，间距 6。
TOGGLE_Y=$((BAR_Y + 7 + 20 + 4 + 10))
CASE_X=$((BAR_X + 7 + 43))
REGEX_X=$((BAR_X + 7 + 86 + 6 + 43))
PANE_X=$((ABS_X + 300))
PANE_Y=$((ABS_Y + 32 + 220))

focus_pane() {
    "$XWIN" xtervo 20 20 >/dev/null 2>&1 || true
    sleep 0.12
    click "$PANE_X" "$PANE_Y"
    sleep 0.15
}
focus_pane

echo "== 1) Ctrl+F 打开并聚焦；输入 alpha（增量搜索落在 2/3） =="
key f --ctrl --shift
sleep 0.35
key alpha
sleep 0.6
expect_log 'find query="alpha" forward=true case=false regex=false hit=SearchHit'
grep -Fq 'count=2/3' "$OUT/app.log" || fail "输入 alpha 后没有显示 2/3"
shot count_2of3

echo "== 2) Enter → 3/3；Shift+Enter → 2/3 =="
key --key enter
sleep 0.5
expect_log 'find query="alpha" forward=true case=false regex=false hit=SearchHit'
grep -Fq 'count=3/3' "$OUT/app.log" || fail "Enter 后没有 3/3"
shot count_3of3
key --key enter --shift
sleep 0.5
expect_log 'find query="alpha" forward=false case=false regex=false hit=SearchHit'
grep -Fq 'count=2/3' "$OUT/app.log" || fail "Shift+Enter 后没有回到 2/3"

echo "== 3) 鼠标点击移动光标（左端插入 X，右端插入 Y） =="
click "$INPUT_LEFT" "$INPUT_Y"
key x
sleep 0.4
[ "$(last_query)" = "xalpha" ] || fail "左端点击后查询应为 xalpha，实际 $(last_query)"
click "$INPUT_RIGHT" "$INPUT_Y"
key y
sleep 0.4
[ "$(last_query)" = "xalphay" ] || fail "右端点击后查询应为 xalphay，实际 $(last_query)"
shot mouse_caret

echo "== 4) 鼠标拖选 + Ctrl+C =="
"$XCLICK" "$INPUT_LEFT" "$INPUT_Y" drag "$INPUT_RIGHT" "$INPUT_Y" >/dev/null 2>&1
sleep 0.4
shot mouse_select
SELECTED=$(last_query)
[ -n "$SELECTED" ] || fail "拖选前查询为空"
key a --ctrl
sleep 0.2
key c --ctrl
sleep 0.4
CLIP=$(timeout 2 xsel -b -o 2>/dev/null || true)
[ "$CLIP" = "$SELECTED" ] || fail "Ctrl+C 剪贴板应为 '$SELECTED'，实际 '$CLIP'"

echo "== 5) Ctrl+V 恢复 + Ctrl+A 全选替换 =="
key --key backspace
sleep 0.3
key v --ctrl
sleep 0.4
[ "$(last_query)" = "$SELECTED" ] || fail "Ctrl+V 后查询应为 '$SELECTED'，实际 $(last_query)"
key a --ctrl
sleep 0.2
key alpha
sleep 0.4
[ "$(last_query)" = "alpha" ] || fail "Ctrl+A 后输入应完全替换为 alpha，实际 $(last_query)"
grep -Fq 'count=2/3' "$OUT/app.log" || fail "Ctrl+A 替换后没有重新计数"

echo "== 6) 0 命中 → 0/0 =="
key a --ctrl
sleep 0.2
key zzz
sleep 0.5
expect_log 'hit=none count=0/0'
shot count_0of0

echo "== 7) Match case / Match regex 开关 + 非法 regex =="
click "$CASE_X" "$TOGGLE_Y"
sleep 0.35
expect_log 'find option match_case=true regex=false'
click "$CASE_X" "$TOGGLE_Y"
sleep 0.35
expect_log 'find option match_case=false regex=false'
click "$REGEX_X" "$TOGGLE_Y"
sleep 0.4
expect_log 'find option match_case=false regex=true'
key a --ctrl
sleep 0.2
key '['
sleep 0.5
expect_log 'error=invalid regex'
shot invalid_regex
# 回到合法状态，避免后续 Esc/焦点检查受红框干扰
click "$REGEX_X" "$TOGGLE_Y"
sleep 0.4
expect_log 'find option match_case=false regex=false'
# 清空查询，确保下面的 Ctrl+F 重开从空框开始。
key a --ctrl
sleep 0.2
key --key backspace
sleep 0.3

echo "== 8) Esc 关闭，焦点归还终端；输入不泄漏到搜索框 =="
key --key escape
sleep 0.4
key echo
key ' '
key FOCUS_OK
key --key enter
sleep 0.7
for _ in $(seq 1 20); do
    grep -Fq 'FOCUS_OK' "$OUT/input.txt" 2>/dev/null && break
    sleep 0.1
done
grep -Fq 'echo FOCUS_OK' "$OUT/input.txt" || fail "Esc 后终端没有收到 echo FOCUS_OK"
if grep -Fq 'alpha' "$OUT/input.txt"; then
    fail "搜索框文本泄漏进终端输入: $(cat "$OUT/input.txt")"
fi
shot terminal_focus

echo "== 9) 再次 Ctrl+F 打开即聚焦，直接输入 alpha =="
before=$(grep -c 'find query="alpha"' "$OUT/app.log" || true)
key f --ctrl --shift
sleep 0.4
key alpha
sleep 0.5
after=$(grep -c 'find query="alpha"' "$OUT/app.log" || true)
[ "$after" -gt "$before" ] || fail "Ctrl+F 重开后直接输入没有产生 alpha 查询"
grep -Fq 'count=2/3' "$OUT/app.log" || fail "重开后没有计数"
shot reopen_focus
key --key escape
sleep 0.3

echo "PASS: search UX e2e"
echo "screenshots:"
ls -1 "$OUT"/*.png
echo "key log:"
grep -E '\[ui\] find' "$OUT/app.log" | tail -30
