#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# WM live resize e2e：隔离 Xvfb + Openbox + xclick，只操作自己启动的进程。
#
# 用法：
#   scripts/e2e_wm_resize.sh
#   E2E_WM_DISPLAY=:109 E2E_XVFB_BIN=/path/Xvfb \
#   E2E_OPENBOX_BIN=/path/openbox E2E_OPENBOX_LIB=/path/lib \
#   E2E_OPENBOX_DATA=/path/share scripts/e2e_wm_resize.sh
#
# 前置：`cargo build --release -p tervo-app --bin tervo --bin xclick`。
# 约束：不占用 :1，不启动 Xephyr，不 pkill；只 kill 本脚本记录的 pid。
# 若 WM 未声明 _NET_WM_MOVERESIZE，打印 SKIP 并退出 2；不会声称 live resize 通过。
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DISPLAY_NUM=${E2E_WM_DISPLAY:-:105}
SCREEN=${E2E_WM_SCREEN:-1280x800x24}
LOG_DIR=${E2E_WM_LOG_DIR:-"$ROOT/.logs/wm-resize-e2e"}
TERVO_BIN=${E2E_BIN:-"$ROOT/target/release/tervo"}
XCLICK_BIN=${E2E_XCLICK:-"$ROOT/target/release/xclick"}

log() { printf '%s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
skip() { printf 'SKIP: %s\n' "$*" >&2; exit 2; }

find_xvfb() {
    if [ -n "${E2E_XVFB_BIN:-}" ]; then printf '%s\n' "$E2E_XVFB_BIN"; return 0; fi
    if command -v Xvfb >/dev/null 2>&1; then command -v Xvfb; return 0; fi
    [ -x "$ROOT/.perf/xvfb/usr/bin/Xvfb" ] && { printf '%s\n' "$ROOT/.perf/xvfb/usr/bin/Xvfb"; return 0; }
    return 1
}

find_openbox() {
    if [ -n "${E2E_OPENBOX_BIN:-}" ]; then printf '%s\n' "$E2E_OPENBOX_BIN"; return 0; fi
    if command -v openbox >/dev/null 2>&1; then command -v openbox; return 0; fi
    [ -x "$ROOT/.perf/openbox/usr/bin/openbox" ] && { printf '%s\n' "$ROOT/.perf/openbox/usr/bin/openbox"; return 0; }
    return 1
}

XVFB=$(find_xvfb) || fail "找不到 Xvfb（可用 E2E_XVFB_BIN 指定）"
OPENBOX=$(find_openbox) || fail "找不到 openbox（可用 E2E_OPENBOX_BIN 指定）"
[ -x "$TERVO_BIN" ] || fail "缺少 $TERVO_BIN；先跑 cargo build --release -p tervo-app --bin tervo"
[ -x "$XCLICK_BIN" ] || fail "缺少 $XCLICK_BIN；先跑 cargo build --release -p tervo-app --bin xclick"
command -v xwininfo >/dev/null 2>&1 || fail "缺少 xwininfo（x11-utils）"
command -v xprop >/dev/null 2>&1 || fail "缺少 xprop（x11-utils）"

# 不 kill 别人的 X：目标 display 已存在就直接失败，换一个 E2E_WM_DISPLAY。
if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    fail "$DISPLAY_NUM 已被占用；用 E2E_WM_DISPLAY 指定空闲 display（本脚本绝不 kill 它）"
fi

mkdir -p "$LOG_DIR/app" "$LOG_DIR/ob-home"
# xclick 等子进程必须指向本脚本的 display，不能继承宿主 DISPLAY。
export DISPLAY="$DISPLAY_NUM"
OB_CACHE=${E2E_OB_CACHE:-"/tmp/tervo-obcache-$$"}
mkdir -p "$OB_CACHE"
chmod 700 "$OB_CACHE" 2>/dev/null || true

OB_LIB=${E2E_OPENBOX_LIB:-}
OB_DATA=${E2E_OPENBOX_DATA:-}
if [ -z "$OB_LIB" ] && [ -d "$ROOT/.perf/openbox/usr/lib/x86_64-linux-gnu" ]; then
    OB_LIB="$ROOT/.perf/openbox/usr/lib/x86_64-linux-gnu"
fi
if [ -z "$OB_DATA" ] && [ -d "$ROOT/.perf/openbox/usr/share" ]; then
    OB_DATA="$ROOT/.perf/openbox/usr/share"
fi
OB_ENV=("HOME=$LOG_DIR/ob-home" "XDG_CACHE_HOME=$OB_CACHE")
[ -n "$OB_LIB" ] && OB_ENV+=("LD_LIBRARY_PATH=$OB_LIB")
[ -n "$OB_DATA" ] && OB_ENV+=("XDG_DATA_DIRS=$OB_DATA")

XVFB_PID=
OB_PID=
APP_PID=
cleanup() {
    # 只 kill 本脚本记录的 pid；顺序：app → WM → Xvfb。
    for pid in "$APP_PID" "$OB_PID" "$XVFB_PID"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT INT TERM

log "启动 Xvfb $DISPLAY_NUM ($SCREEN)"
DISPLAY="$DISPLAY_NUM" "$XVFB" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp \
    >"$LOG_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do
    DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 || fail "Xvfb $DISPLAY_NUM 启动失败（$LOG_DIR/xvfb.log）"

log "启动 Openbox"
env DISPLAY="$DISPLAY_NUM" "${OB_ENV[@]}" "$OPENBOX" --sm-disable \
    >"$LOG_DIR/openbox.log" 2>&1 &
OB_PID=$!
log "Openbox pid=$OB_PID"
for _ in $(seq 1 100); do
    DISPLAY="$DISPLAY_NUM" xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q "window id" && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q "window id" \
    || fail "Openbox 未接管 $DISPLAY_NUM（$LOG_DIR/openbox.log）"
if ! DISPLAY="$DISPLAY_NUM" xprop -root _NET_SUPPORTED 2>/dev/null | grep -q _NET_WM_MOVERESIZE; then
    skip "WM 的 _NET_SUPPORTED 不含 _NET_WM_MOVERESIZE；保留自绘回退，不声称 live resize 通过"
fi

log "启动 Tervo（--cols 60 --rows 20，预期 540x472）"
env DISPLAY="$DISPLAY_NUM" XMODIFIERS=@im=none \
    TERVO_LOG_DIR="$LOG_DIR/app" TERVO_CONFIG="$LOG_DIR/tervo.toml" \
    "$TERVO_BIN" --cols 60 --rows 20 >"$LOG_DIR/app.stderr" 2>&1 &
APP_PID=$!

WIN=
for _ in $(seq 1 100); do
    for id in $(DISPLAY="$DISPLAY_NUM" xwininfo -root -tree 2>/dev/null \
        | awk '/"Tervo"/ {print $1}'); do
        pid=$(DISPLAY="$DISPLAY_NUM" xprop -id "$id" _NET_WM_PID 2>/dev/null \
            | awk '{print $NF}')
        if [ "$pid" = "$APP_PID" ]; then WIN=$id; break; fi
    done
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || fail "找不到 pid=$APP_PID 的 Tervo 窗口（$LOG_DIR/app.stderr）"

geom() {
    DISPLAY="$DISPLAY_NUM" xwininfo -id "$WIN" | awk '
        /Absolute upper-left X:/ {x=$NF}
        /Absolute upper-left Y:/ {y=$NF}
        /Width:/ {w=$2}
        /Height:/ {h=$2}
        END {print x, y, w, h}'
}
# Openbox 在窗口 map 后可能再晚几十毫秒才完成 placement；等几何连续稳定再拖，
# 否则拖拽坐标会按旧位置算，点到窗口外。
prev=
stable=0
for _ in $(seq 1 100); do
    cur=$(geom)
    if [ "$cur" = "$prev" ]; then stable=$((stable + 1)); else stable=0; fi
    [ "$stable" -ge 6 ] && break
    prev=$cur
    sleep 0.05
done
read -r x y w h <<<"$(geom)"
log "初始窗口 win=$WIN ${w}x${h} at $x,$y"
[ "$w" -ge 400 ] && [ "$h" -ge 300 ] || fail "初始窗口尺寸异常：${w}x${h}"

LOG=$(ls -t "$LOG_DIR/app"/tervo-*.log 2>/dev/null | head -1)
[ -n "$LOG" ] || fail "没有 Tervo 日志（$LOG_DIR/app）"

# 1) 右边缘：540x472 → 700x472（+160）。
log "拖右边缘 x=$((x + w - 2)),y=$((y + h / 2)) +160"
"$XCLICK_BIN" "$((x + w - 2))" "$((y + h / 2))" drag "$((x + w - 2 + 160))" "$((y + h / 2))" \
    >"$LOG_DIR/xclick-right.log" 2>&1
sleep 0.8
read -r x y w2 h2 <<<"$(geom)"
log "右边缘后 ${w2}x${h2} at $x,$y"
[ "$w2" -ge $((w + 140)) ] || fail "右边缘未 resize：$w -> $w2"
grep -q "WM live resize dir=3 source=winit" "$LOG" \
    || fail "日志缺少 'WM live resize dir=3 source=winit'（$LOG）"

# 2) 右下角：再 +100,+100，验证 direction=4 与高度 live 变化。
x2=$((x + w2 - 3))
y2=$((y + h2 - 3))
log "拖右下角 x=$x2,y=$y2 +100,+100"
"$XCLICK_BIN" "$x2" "$y2" drag "$((x2 + 100))" "$((y2 + 100))" \
    >"$LOG_DIR/xclick-corner.log" 2>&1
sleep 0.8
read -r x3 y3 w3 h3 <<<"$(geom)"
log "右下角后 ${w3}x${h3} at $x3,$y3"
[ "$w3" -ge $((w2 + 80)) ] || fail "右下角宽度未 resize：$w2 -> $w3"
[ "$h3" -ge $((h2 + 80)) ] || fail "右下角高度未 resize：$h2 -> $h3"
grep -q "WM live resize dir=4 source=winit" "$LOG" \
    || fail "日志缺少 'WM live resize dir=4 source=winit'（$LOG）"
if grep -q "回退自绘\|未接管指针" "$LOG"; then
    fail "WM 路径出现了回退日志，不能声称 live resize 全程走 WM（$LOG）"
fi

if command -v xwd >/dev/null 2>&1 && [ -f "$ROOT/scripts/xwd2png.py" ]; then
    DISPLAY="$DISPLAY_NUM" xwd -id "$WIN" -silent -out "$LOG_DIR/wm-resize.xwd" 2>/dev/null \
        && python3 "$ROOT/scripts/xwd2png.py" "$LOG_DIR/wm-resize.xwd" "$LOG_DIR/wm-resize.png" \
        >/dev/null 2>&1 || true
fi

log "PASS: WM live resize ${w}x${h} -> ${w2}x${h2} -> ${w3}x${h3}"
log "日志: $LOG"
log "截图: $LOG_DIR/wm-resize.png（.logs 已忽略）"
