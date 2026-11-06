#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 终端字形验收截图：ASCII / 中文 / emoji / ZWJ / 组合音标 / 宽字符 / Nerd Font。
#
# - 固定 headless Xvfb（默认 :116），不占用 :1、不启动 Xephyr、不 pkill；
# - 只回收本脚本启动的 app 与 Xvfb 进程；
# - 输出 PNG（以及同名 .xwd）到参数指定的路径，通常放 .logs/。
#
# 用法：
#   cargo build -p tervo-app --bin tervo
#   scripts/font_glyph_shot.sh .logs/font-emoji/after-default.png
#   TERVO_FALLBACK_FONTS=/path/SymbolsNerdFontMono-Regular.ttf \
#     scripts/font_glyph_shot.sh .logs/font-emoji/after-nerd.png
#
# 环境变量：
#   FONT_SHOT_BIN      tervo 可执行文件（默认 $ROOT/target/debug/tervo）
#   FONT_SHOT_DISPLAY  X display（默认 :116；已占用则直接失败，绝不复用/ kill）
#   FONT_SHOT_XVFB     Xvfb 可执行文件（默认 .perf/xvfb/usr/bin/Xvfb，再退回 PATH）
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT_ARG=${1:-"$ROOT/.logs/font-emoji/glyph.png"}
# 先建目录再 cd：全新 checkout 的 .logs/ 还不存在。
mkdir -p "$(dirname "$OUT_ARG")"
OUT_DIR=$(cd "$(dirname "$OUT_ARG")" && pwd)
OUT="$OUT_DIR/$(basename "${OUT_ARG%.png}").png"
XWD="${OUT%.png}.xwd"

BIN=${FONT_SHOT_BIN:-${E2E_BIN:-"$ROOT/target/debug/tervo"}}
[ -x "$BIN" ] || {
    echo "缺少 $BIN；先 cargo build -p tervo-app --bin tervo，或用 FONT_SHOT_BIN 指定" >&2
    exit 1
}

XVFB=${FONT_SHOT_XVFB:-}
if [ -z "$XVFB" ]; then
    for cand in "$ROOT/.perf/xvfb/usr/bin/Xvfb" "$ROOT/../../.perf/xvfb/usr/bin/Xvfb"; do
        [ -x "$cand" ] && { XVFB=$cand; break; }
    done
fi
if [ -z "$XVFB" ]; then
    XVFB=$(command -v Xvfb || true)
fi
[ -n "$XVFB" ] && [ -x "$XVFB" ] || {
    echo "找不到 Xvfb（FONT_SHOT_XVFB 可指定；本仓库用 .perf/xvfb/usr/bin/Xvfb）" >&2
    exit 1
}
command -v xwd >/dev/null || { echo "缺少 xwd（x11-apps）" >&2; exit 1; }
command -v xwininfo >/dev/null || { echo "缺少 xwininfo（x11-utils）" >&2; exit 1; }
command -v python3 >/dev/null || { echo '缺少 python3（xwd2png.py 需要）' >&2; exit 1; }

DISPLAY_NUM=${FONT_SHOT_DISPLAY:-:116}
if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    echo "$DISPLAY_NUM 已被占用；本脚本不会复用或 kill 别人的 X，换 FONT_SHOT_DISPLAY" >&2
    exit 1
fi

DEMO="$OUT_DIR/glyph-demo-$$.sh"
CFG="$OUT_DIR/glyph-shot-$$.toml"
XVFB_LOG="$OUT_DIR/glyph-xvfb-$$.log"
APP_LOG="$OUT_DIR/glyph-app-$$.log"
printf 'theme = "auto"\n' >"$CFG"

# 组合音标用 \u 转义写，避免源文件编码/编辑器差异。
cat >"$DEMO" <<'DEMO_EOF'
#!/bin/bash
printf '\033[1;36m[ASCII]\033[0m The quick brown fox jumps over 0123456789\n'
printf '\033[1;36m[中文]\033[0m 终端字体回退：你好，世界！简体中文測試\n'
printf '\033[1;36m[Emoji]\033[0m 😀 🚀 ❤️ ✔️ 👍 ⚡ 🎉\n'
printf '\033[1;36m[ZWJ]\033[0m 👨‍👩‍👧 👩‍💻 🏳️‍🌈\n'
printf '\033[1;36m[组合音标]\033[0m e\u0301 a\u0300 n\u0303 x\u0301 o\u0308 n\u0303 e\u0301\n'
printf '\033[1;36m[宽字符]\033[0m 你好世界 ＡＢＣ 全角１２３ ｶﾀｶﾅ\n'
printf '\033[1;36m[Nerd]\033[0m \ue709 \ue796 \uf121 \ue0b0 \ue0b2\n'
printf '\033[1;36m[框线]\033[0m ┌─┬─┐ │ ├─┼─┤ └─┴─┘ ▁▂▃▄▅▆▇█\n'
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

DISPLAY="$DISPLAY_NUM" XMODIFIERS=@im=none TERVO_LOG_DIR="$OUT_DIR" TERVO_CONFIG="$CFG" \
    "$BIN" --cols 84 --rows 18 --font-size 16 --shell /bin/bash -- -c "$DEMO" \
    >"$APP_LOG" 2>&1 &
APID=$!

WIN=
for _ in $(seq 1 100); do
    # set -e + pipefail：没找到窗口时 grep 返回 1，必须 `|| true`，否则循环只跑一次。
    WIN=$(DISPLAY="$DISPLAY_NUM" xwininfo -root -tree 2>/dev/null \
        | grep -iE '"tervo" "tervo"' | awk '{print $1}' | head -1 || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || {
    echo "没找到 tervo 窗口（$APP_LOG）" >&2
    DISPLAY="$DISPLAY_NUM" xwininfo -root -tree 2>/dev/null | head -20 >&2
    exit 1
}
# 等首帧 + 字体回退（CJK/emoji 字体首次解析）完成。
sleep 3

DISPLAY="$DISPLAY_NUM" xwd -id "$WIN" -silent -out "$XWD"
python3 "$ROOT/scripts/xwd2png.py" "$XWD" "$OUT" >/dev/null
echo "win=$WIN png=$OUT"
