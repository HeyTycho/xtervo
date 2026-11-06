#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# URL hover / 手型指针 / 本地路径 Ctrl+click e2e。
#
# 在独立 headless Xvfb 上运行（默认 :128），只 kill 本脚本启动的 Xvfb/OpenBox/app，
# 不占用 :1 / 其它 display，也不 pkill 其他进程。
#
# 覆盖：
#   1) hover http URL → `[link] hover` 日志 + 系统指针切成手型（XFixes 光标图 hash 变化）
#   2) 移开 → 指针恢复默认
#   3) OSC 8 单元格 hover 命中显式 URI
#   4) 裸 IP:port hover 不切手型（不误判）
#   5) Ctrl+click 已存在文件 → 假 xdg-open 收到绝对路径
#   6) Ctrl+click 不存在/无权限路径 → 不 spawn xdg-open，底部提示 + 截图
#   7) 右键菜单仍能识别链接（Open/Copy Link 入口）
#
# 用法：
#   CARGO_TARGET_DIR=$PWD/target cargo build --offline -p tervo-app --bin tervo --bin xclick --bin xwin
#   ./scripts/e2e_url_paths.sh
#
# 依赖：Xvfb、OpenBox（仓库 .perf/，可选）、xwd、ffmpeg、gcc + libXfixes（光标断言，
#       缺 gcc 时自动跳过光标图断言，仍校验日志）。不读取/修改宿主桌面。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/url-paths}
DISPLAY_NUM=${E2E_DISPLAY:-:128}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${E2E_BIN:-$TARGET_DIR/debug/tervo}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)

for f in "$BIN" "$XCLICK" "$XWIN"; do
    [ -x "$f" ] || {
        echo "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xclick --bin xwin" >&2
        exit 2
    }
done
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
command -v xdpyinfo >/dev/null || { echo "缺少 xdpyinfo" >&2; exit 2; }
command -v xwd >/dev/null || { echo "缺少 xwd" >&2; exit 2; }

mkdir -p "$OUT" "$OUT/logs" "$OUT/obhome" /tmp/.X11-unix
APP_LOG=$OUT/app.log
XDG_LOG=$OUT/xdg-open.log
rm -f "$APP_LOG" "$XDG_LOG" "$OUT"/*.xwd
export DISPLAY=$DISPLAY_NUM

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM 已被占用；为避免干扰其他会话，本脚本退出。" >&2
    exit 2
fi

TMP=/tmp/tve-$$
rm -rf "$TMP"
mkdir -p "$TMP/fakebin"
trap 'kill "${APP_PID:-}" "${OB_PID:-}" "${XVFB_PID:-}" 2>/dev/null || true; rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------- 测试夹具
OK_FILE=$TMP/ok.txt
NO_FILE=$TMP/no.txt
DENY_FILE=$TMP/deny.txt
printf 'hello from e2e\n' >"$OK_FILE"
rm -f "$NO_FILE"
printf 'secret\n' >"$DENY_FILE"
chmod 000 "$DENY_FILE" || true

LINE="URL https://example.com/x P $OK_FILE M $NO_FILE D $DENY_FILE"
printf '%s\n' "$LINE" >"$TMP/line.txt"
export TMP
cat >"$TMP/run.sh" <<'SCRIPT'
cat "$TMP/line.txt"
printf 'IP 127.0.0.1:8080\n'
printf '\033]8;;https://osc.example/x\033\\OSC\033]8;;\033\\\n'
sleep 600
SCRIPT

cat >"$TMP/fakebin/xdg-open" <<'SCRIPT'
#!/bin/sh
printf '%s\n' "$*" >> "$TV_E2E_XDG_LOG"
exit 0
SCRIPT
chmod +x "$TMP/fakebin/xdg-open"
export TV_E2E_XDG_LOG=$XDG_LOG

printf 'theme = "tokyo-night"\nfont_size = 15\nconfirm_close = false\n' >"$OUT/live.toml"

# 光标图探针：XFixesGetCursorImage 的尺寸/hash/opaque 像素数；可选输出放大 PPM。
CURSOR_PROBE=$OUT/cursor_probe
CURSOR_OK=0
cat >"$OUT/cursor_probe.c" <<'EOF'
#include <X11/Xlib.h>
#include <X11/extensions/Xfixes.h>
#include <stdio.h>
#include <stdint.h>
int main(int argc, char **argv) {
    Display *d = XOpenDisplay(NULL);
    if (!d) { puts("cursor name=(no-display)"); return 2; }
    int ev, er;
    if (!XFixesQueryExtension(d, &ev, &er)) { puts("cursor name=(no-xfixes)"); return 3; }
    XFixesCursorImage *c = XFixesGetCursorImage(d);
    if (!c) { puts("cursor name=(no-cursor)"); return 4; }
    uint64_t h = 1469598103934665603ULL;
    unsigned opaque = 0;
    for (unsigned i = 0; i < c->width * c->height; i++) {
        uint32_t p = (uint32_t)c->pixels[i];
        h ^= p; h *= 1099511628211ULL;
        if ((p >> 24) & 0xff) opaque++;
    }
    printf("cursor size=%ux%u serial=%lu hash=%016llx opaque=%u\n",
           c->width, c->height, c->cursor_serial, (unsigned long long)h, opaque);
    if (argc > 1) {
        const int scale = 8;
        int w = c->width * scale, ht = c->height * scale;
        FILE *f = fopen(argv[1], "wb");
        if (f) {
            fprintf(f, "P6\n%d %d\n255\n", w, ht);
            for (int y = 0; y < ht; y++) for (int x = 0; x < w; x++) {
                uint32_t p = (uint32_t)c->pixels[(y / scale) * c->width + (x / scale)];
                unsigned a = (p >> 24) & 0xff, r = (p >> 16) & 0xff, g = (p >> 8) & 0xff, b = p & 0xff;
                unsigned char px[3] = {
                    (unsigned char)((r * a + 255 * (255 - a)) / 255),
                    (unsigned char)((g * a + 255 * (255 - a)) / 255),
                    (unsigned char)((b * a + 255 * (255 - a)) / 255),
                };
                fwrite(px, 1, 3, f);
            }
            fclose(f);
        }
    }
    XFree(c);
    XCloseDisplay(d);
    return 0;
}
EOF
if command -v gcc >/dev/null 2>&1 && gcc "$OUT/cursor_probe.c" -lXfixes -lX11 -o "$CURSOR_PROBE" 2>"$OUT/cursor_probe.build.log"; then
    CURSOR_OK=1
else
    echo "提示：无法构建 XFixes 光标探针，跳过光标图断言（仍校验 [link] 日志）"
fi

# ---------------------------------------------------------------- 启动环境
XVFB_PID=""
OB_PID=""
APP_PID=""
echo "== 启动 Xvfb $DISPLAY_NUM =="
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }

OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu
if [ -x "$OPENBOX" ]; then
    HOME=$OUT/obhome LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
else
    echo "提示：未找到仓库 .perf/openbox，继续无 WM 运行（窗口位置由 xwin 固定）"
fi

echo "== 启动 Tervo（140 列，测试输出见 $APP_LOG） =="
HOME=$OUT/obhome PATH="$TMP/fakebin:$PATH" XMODIFIERS=@im=none SLINT_BACKEND=winit-software \
    TERVO_CONFIG="$OUT/live.toml" TERVO_LOG_DIR="$OUT/logs" TERVO_MOUSELOG=1 \
    "$BIN" --cols 140 --rows 24 --shell /bin/sh -- "$TMP/run.sh" >"$APP_LOG" 2>&1 &
APP_PID=$!

WIN=""
for _ in $(seq 1 150); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.1
done
[ -n "$WIN" ] || { echo "找不到 Tervo 窗口；见 $APP_LOG" >&2; tail -60 "$APP_LOG" >&2; exit 2; }
"$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
sleep 1.0
WIN=$(xwininfo -root -tree | grep -i '"tervo"' | head -1 | awk '{print $1}')
AX=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
AY=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
COLS=140
ROWS=24
CW=$((W / COLS))
CH=$(((H - 32) / ROWS))
[ "$CW" -ge 4 ] && [ "$CH" -ge 6 ] || { echo "窗口尺寸异常：${W}x${H}" >&2; exit 2; }
echo "  window=$WIN at $AX,$AY ${W}x${H} cell=${CW}x${CH}"

# ---------------------------------------------------------------- 工具函数
fail() {
    echo "FAIL: $*" >&2
    echo "--- $APP_LOG（末尾 100 行）---" >&2
    tail -100 "$APP_LOG" >&2
    exit 1
}
wait_log() { # <literal>
    local needle=$1
    for _ in $(seq 1 100); do
        grep -Fq -- "$needle" "$APP_LOG" && return 0
        sleep 0.05
    done
    fail "日志里等不到：$needle"
}
assert_log() { grep -Fq -- "$1" "$APP_LOG" || fail "日志缺少：$1"; }
refute_log() { ! grep -Fq -- "$1" "$APP_LOG" || fail "日志不应出现：$1"; }
assert_xdg_has() { grep -Fq -- "$1" "$XDG_LOG" 2>/dev/null || fail "xdg-open 未收到：$1"; }
assert_xdg_absent() {
    ! grep -Fq -- "$1" "$XDG_LOG" 2>/dev/null || fail "xdg-open 不应收到：$1"
}
shot() {
    xwd -silent -id "$WIN" -out "$OUT/$1.xwd"
    if command -v ffmpeg >/dev/null 2>&1; then
        ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$1.xwd" "$OUT/$1.png"
        rm -f "$OUT/$1.xwd"
    fi
}
# 截整屏并叠加 X 光标（xwd 不含光标）；失败时退回窗口截图。
shot_with_cursor() {
    local name=$1
    if command -v ffmpeg >/dev/null 2>&1; then
        if ffmpeg -y -hide_banner -loglevel error -f x11grab -draw_mouse 1 \
            -video_size "${W}x${H}" -i "$DISPLAY_NUM+$AX,$AY" -frames:v 1 "$OUT/$name.png" \
            >/dev/null 2>&1; then
            return 0
        fi
    fi
    shot "$name"
}
click_at() { # <col> <row> [button] [ctrl]
    local x=$((AX + $1 * CW + CW / 2))
    local y=$((AY + 32 + $2 * CH + CH / 2))
    "$XCLICK" "$x" "$y" "${3:-1}" ${4:-} >/dev/null
}
col_of() { # <line> <needle> → 0 基显示列
    local pre=${1%%"$2"*}
    echo "${#pre}"
}
cursor_line() {
    [ "$CURSOR_OK" = 1 ] || return 1
    "$CURSOR_PROBE" "$@"
}
cursor_hash() {
    cursor_line | sed -n 's/.*hash=\([0-9a-f]*\).*/\1/p'
}

URL_COL=$(col_of "$LINE" "https://example.com/x")
OK_COL=$(col_of "$LINE" "$OK_FILE")
NO_COL=$(col_of "$LINE" "$NO_FILE")
DENY_COL=$(col_of "$LINE" "$DENY_FILE")
IP_LINE="IP 127.0.0.1:8080"
IP_COL=$(col_of "$IP_LINE" "127.0.0.1:8080")
echo "  cols: url=$URL_COL ok=$OK_COL missing=$NO_COL deny=$DENY_COL ip=$IP_COL"

# ---------------------------------------------------------------- 1. 默认指针
echo "== 1) 空白处默认指针 =="
click_at 130 5 1
sleep 0.4
INITIAL_HASH=""
if [ "$CURSOR_OK" = 1 ]; then
    cursor_line "$OUT/cursor_initial.ppm" | tee "$OUT/cursor_initial.txt"
    INITIAL_HASH=$(cursor_hash)
    [ -n "$INITIAL_HASH" ] || fail "初始光标 hash 为空"
fi
shot blank
# hover/离开只应切指针，不得触发终端帧重绘；记录 render 日志条数做前后对比。
RENDER_BEFORE=$(grep -c '^\[render\] pane=' "$APP_LOG" 2>/dev/null || true)

# ---------------------------------------------------------------- 2. URL hover
echo "== 2) URL hover：手型 + 日志 =="
click_at $((URL_COL + 3)) 0 1
wait_log "[link] hover pane=1 cell=0,$((URL_COL + 3)) kind=uri"
assert_log 'text="https://example.com/x"'
wait_log "[link] cursor=pointer"
sleep 0.3
if [ "$CURSOR_OK" = 1 ]; then
    cursor_line "$OUT/cursor_pointer.ppm" | tee "$OUT/cursor_pointer.txt"
    POINTER_HASH=$(cursor_hash)
    [ -n "$POINTER_HASH" ] || fail "手型光标 hash 为空"
    [ "$POINTER_HASH" != "$INITIAL_HASH" ] || fail "hover 后系统指针图未变化"
    ffmpeg -y -hide_banner -loglevel error -i "$OUT/cursor_pointer.ppm" "$OUT/cursor_pointer.png" 2>/dev/null || true
fi
shot_with_cursor hover_url

# ---------------------------------------------------------------- 3. 离开恢复
echo "== 3) 移开：指针恢复默认 =="
click_at 130 5 1
wait_log "[link] hover end pane=1 cell=5,130"
wait_log "[link] cursor=default"
sleep 0.3
if [ "$CURSOR_OK" = 1 ]; then
    cursor_line "$OUT/cursor_default.ppm" | tee "$OUT/cursor_default.txt"
    AFTER_HASH=$(cursor_hash)
    [ "$AFTER_HASH" != "$POINTER_HASH" ] || fail "离开链接后仍是手型"
    DEFAULT_HASH=$AFTER_HASH
    ffmpeg -y -hide_banner -loglevel error -i "$OUT/cursor_default.ppm" "$OUT/cursor_default.png" 2>/dev/null || true
fi
shot after_leave
RENDER_AFTER=$(grep -c '^\[render\] pane=' "$APP_LOG" 2>/dev/null || true)
[ "$RENDER_AFTER" = "$RENDER_BEFORE" ] || \
    fail "hover/离开触发了终端重绘（render $RENDER_BEFORE → $RENDER_AFTER）"
echo "  render lines unchanged: $RENDER_BEFORE"

# ---------------------------------------------------------------- 4. OSC 8
echo "== 4) OSC 8 hover 命中显式 URI =="
refute_log "[link] hover pane=1 cell=2,1"
click_at 1 2 1
wait_log "[link] hover pane=1 cell=2,1 kind=uri"
assert_log 'text="https://osc.example/x"'
click_at 130 5 1
wait_log "[link] hover end pane=1 cell=5,130"

# ---------------------------------------------------------------- 5. 裸 IP 不误判
echo "== 5) 裸 IP:port 不切手型 =="
click_at $((IP_COL + 3)) 1 1
sleep 0.4
refute_log "hover pane=1 cell=1,$((IP_COL + 3))"
if [ "$CURSOR_OK" = 1 ]; then
    [ "$(cursor_hash)" = "$DEFAULT_HASH" ] || fail "裸 IP hover 不应切手型"
fi
shot ip_hover

# ---------------------------------------------------------------- 6. 路径成功
echo "== 6) Ctrl+click 本地文件 → xdg-open 绝对路径 =="
click_at $((OK_COL + 2)) 0 1 ctrl
wait_log "[link] open path source=\"$OK_FILE\""
wait_log "resolved=\"$OK_FILE\""
wait_log "xdg-open ok path=\"$OK_FILE\""
assert_xdg_has "$OK_FILE"
shot path_open_ok

# ---------------------------------------------------------------- 7. 不存在路径
echo "== 7) Ctrl+click 不存在路径 → 底部提示、不 spawn xdg-open =="
BEFORE_XDG=$(wc -l <"$XDG_LOG" 2>/dev/null || echo 0)
click_at $((NO_COL + 2)) 0 1 ctrl
wait_log "无法打开路径 $NO_FILE：路径不存在"
assert_xdg_absent "$NO_FILE"
AFTER_XDG=$(wc -l <"$XDG_LOG" 2>/dev/null || echo 0)
[ "$BEFORE_XDG" = "$AFTER_XDG" ] || fail "不存在路径不应调用 xdg-open"
sleep 0.3
shot path_missing_toast

# ---------------------------------------------------------------- 8. 无权限路径
echo "== 8) Ctrl+click 无权限路径 → 底部提示、不 spawn xdg-open =="
if [ "$(id -u)" = 0 ]; then
    echo "提示：root 的 access(2) 无视权限位，跳过无权限路径用例"
else
    BEFORE_XDG=$(wc -l <"$XDG_LOG" 2>/dev/null || echo 0)
    click_at $((DENY_COL + 2)) 0 1 ctrl
    wait_log "无法打开路径 $DENY_FILE：无权限"
    assert_xdg_absent "$DENY_FILE"
    AFTER_XDG=$(wc -l <"$XDG_LOG" 2>/dev/null || echo 0)
    [ "$BEFORE_XDG" = "$AFTER_XDG" ] || fail "无权限路径不应调用 xdg-open"
    sleep 0.3
    shot path_deny_toast
fi

# ---------------------------------------------------------------- 9. 右键菜单仍识别链接
echo "== 9) 右键路径 Open Link 可打开 =="
BEFORE_XDG=$(wc -l <"$XDG_LOG" 2>/dev/null || echo 0)
click_at $((OK_COL + 2)) 0 3
wait_log "[ui] right click pane=1"
assert_log 'kind=Some("path")'
sleep 0.3
shot menu_path
# 新分类菜单：anchor y + 6 padding + Link 组标题 22 + Open Link 半行 15 = 43。
# 旧脚本用内容区中心，新菜单变高后点到遮罩，会看起来像菜单没打开。
MENU_X=$((OK_COL * CW + 140))
"$XCLICK" $((AX + MENU_X)) $((AY + 32 + 43)) 1 >/dev/null
wait_log "[link] menu open kind=path"
sleep 0.5
AFTER_XDG=$(wc -l <"$XDG_LOG" 2>/dev/null || echo 0)
[ "$AFTER_XDG" -gt "$BEFORE_XDG" ] || fail "右键 Open Link 未调用 xdg-open"
assert_xdg_has "$OK_FILE"
shot menu_path_open

echo "== 10) 右键 URL 仍显示 Open/Copy Link =="
click_at $((URL_COL + 3)) 0 3
# 不能等 "[ui] right click pane=1"：上一步路径菜单已经打过同一前缀；
# 等这个 URL 专属的 kind 行，避免竞态。
wait_log 'kind=Some("uri")'
assert_log "link=true"
sleep 0.4
shot menu_url
click_at 130 5 1 # 点空白关菜单
sleep 0.3

# ---------------------------------------------------------------- 汇总
echo "== PASS: URL hover/指针恢复/OSC 8/裸 IP/本地路径/失败提示/右键菜单 =="
echo "artifacts: $OUT"
echo "  app log: $APP_LOG"
echo "  xdg-open: $XDG_LOG"
ls -1 "$OUT"/*.png 2>/dev/null || true
