#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 中文输入 / CJK 渲染冒烟测试。
#
# 前提：DISPLAY 可用、fcitx5（或 ibus）的 XIM 在跑、当前输入法是拼音。
# 流程：启动 tervo（shell = cat）→ 激活窗口 → xkey 注入 nihao + 空格
#       → 断言 keylog 里出现 UTF-8 的“你好”、截图保存到 .logs/ime_smoke.png。
#
# 用法：./scripts/check_ime.sh [release|debug]
set -euo pipefail

cd "$(dirname "$0")/.."
PROFILE="${1:-release}"
BIN="./target/$PROFILE/tervo"
XKEY="./target/$PROFILE/xkey"
XWIN="./target/$PROFILE/xwin"
for f in "$BIN" "$XKEY" "$XWIN"; do
    [ -x "$f" ] || { echo "缺少 $f，请先 cargo build --$PROFILE -p tervo"; exit 1; }
done

mkdir -p .logs
CFG=$(mktemp)
LOG=".logs/ime_smoke.log"
printf 'theme = "tokyo-night"\nfont_size = 15\n' > "$CFG"

cleanup() {
    [ -n "${APP:-}" ] && kill "$APP" 2>/dev/null || true
    rm -f "$CFG"
}
trap cleanup EXIT

SLINT_BACKEND=winit-software TERVO_CONFIG="$CFG" TERVO_KEYLOG=1 \
    "$BIN" --cols 80 --rows 20 --shell /bin/cat > "$LOG" 2>&1 &
APP=$!
sleep 1.5

"$XWIN" "Tervo" >/dev/null 2>&1 || { echo "找不到 Tervo 窗口"; exit 1; }
sleep 0.4

"$XKEY" "nihao"
sleep 0.4
"$XKEY" " "
sleep 1

WIN=$("$XWIN" "Tervo" 2>/dev/null | grep -o '0x[0-9a-f]*' | head -1 || true)
if [ -n "$WIN" ] && command -v xwd >/dev/null && command -v ffmpeg >/dev/null; then
    xwd -silent -id "$WIN" > .logs/ime_smoke.xwd
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i .logs/ime_smoke.xwd .logs/ime_smoke.png
fi

echo "--- keylog ---"
grep -E '\[key\]|\[send\]' "$LOG" || true

if grep -qE '\[(key|ime)\] "你好"' "$LOG"; then
    echo "PASS: 中文上屏字节已写入 PTY"
else
    echo "FAIL: 没有捕捉到中文提交。检查 fcitx5 XIM / 输入法是否处于拼音模式"
    echo "      （XMODIFIERS=${XMODIFIERS:-未设置}）"
    exit 2
fi
