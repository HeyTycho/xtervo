#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# P0 SFTP 超时 e2e：假 sftp sleep 30，验证 10s 硬超时 + kill，且窗口仍响应。
#
# 在独立 headless Xvfb（默认 :133）上运行；只 kill 本脚本启动的 Xvfb/app。
# 用法：
#   cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick
#   ./scripts/e2e_p0_blocking.sh
# 环境变量：E2E_BIN、E2E_OUT、E2E_DISPLAY、XTERVO_PERF_DIR、E2E_XVFB_BIN。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/p0-blocking}
DISPLAY_NUM=${E2E_DISPLAY:-:133}
mkdir -p "$OUT" "$OUT/home" "$OUT/bin" "$OUT/logs" "$OUT/sftp-root"

TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${E2E_BIN:-$TARGET_DIR/debug/xtervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
for f in "$BIN" "$XWIN" "$XCLICK"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick" >&2; exit 2; }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }

export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
export XTERVO_CONFIG=$OUT/config.toml
export XTERVO_PROFILES=$OUT/profiles.toml
export XTERVO_LOG_DIR=$OUT/logs

XVFB_PID=""
APP_PID=""
OWN_DISPLAY=0
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    if [ "$OWN_DISPLAY" = 1 ] && [ -n "$XVFB_PID" ]; then
        kill "$XVFB_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

cat > "$OUT/config.toml" <<'CFG'
theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false
confirm_multiline_paste = false
CFG
cat > "$OUT/profiles.toml" <<'PROF'
version = 2
next_id = 2

[[profiles]]
id = 1
name = "fake-ssh"
kind = "ssh"
host = "example.invalid"
port = 22
user = "tester"
auth = "agent"
PROF
# 假 sftp：记录 pid 后 exec sleep 30；超时必须 kill 掉它。
cat > "$OUT/bin/sftp" <<SFTP
#!/bin/sh
echo \$\$ > "$OUT/sftp.pid"
exec sleep 30
SFTP
chmod +x "$OUT/bin/sftp"
rm -f "$OUT/sftp.pid" "$OUT/app.log"

if xdpyinfo >/dev/null 2>&1; then
    echo "复用已有 display $DISPLAY_NUM（测试结束不关闭）"
else
    echo "启动 Xvfb $DISPLAY_NUM"
    ("$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1280x900x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
     echo $! >"$OUT/xvfb.pid")
    XVFB_PID=$(cat "$OUT/xvfb.pid")
    OWN_DISPLAY=1
    for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
    xdpyinfo >/dev/null 2>&1 || fail "Xvfb $DISPLAY_NUM 启动失败"
fi

PATH="$OUT/bin:$PATH" "$BIN" >"$OUT/app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 100); do
    win=$(xwininfo -root -tree 2>/dev/null | grep -iE '"xtervo" "xtervo"' | awk '{print $1}' | head -1 || true)
    [ -n "${win:-}" ] && break
    sleep 0.1
done
[ -n "${win:-}" ] || fail "app 窗口未出现（日志 $OUT/app.log）"
"$XWIN" "$win" 20 20 >/dev/null
sleep 0.4

ox=$(xwininfo -id "$win" | awk '/Absolute upper-left X/{print $NF}')
oy=$(xwininfo -id "$win" | awk '/Absolute upper-left Y/{print $NF}')
w=$(xwininfo -id "$win" | awk '/Width:/{print $2}')
h=$(xwininfo -id "$win" | awk '/Height:/{print $2}')
[ -n "$ox" ] && [ -n "$oy" ] && [ -n "$w" ] && [ -n "$h" ] || fail "读不到窗口几何"

# 1) TabBar 空白右键 → 窗口菜单；从日志反推菜单坐标，点 SFTP（第 10 行，0-based=9）。
mx_local=$(( w > 900 ? 700 : w - 300 ))
"$XCLICK" $((ox + mx_local)) $((oy + 16)) 3 >/dev/null 2>&1
sleep 0.3
menu_line=$(grep -o '\[ui\] tabbar menu at [0-9]*,[0-9]*' "$OUT/app.log" | tail -1)
[ -n "$menu_line" ] || fail "没有打开 TabBar 菜单"
menu_x=$(echo "$menu_line" | sed -E 's/.* at ([0-9]+),([0-9]+)/\1/')
menu_y=$(echo "$menu_line" | sed -E 's/.* at ([0-9]+),([0-9]+)/\2/')
menu_left=$(( menu_x < w - 180 ? menu_x : w - 180 ))
"$XCLICK" $((ox + menu_left + 85)) $((oy + menu_y + 4 + 9 * 30 + 14)) 1 >/dev/null 2>&1
sleep 0.4
grep -q '\[sftp\] 打开面板' "$OUT/app.log" || fail "SFTP 面板没有打开"

# 2) W-02 后 SFTP 面板 480x610；点 List 按钮中心 (56,121)。
panel_x=$(( (w - 480) / 2 ))
panel_y=$(( (h - 610) / 2 < 8 ? 8 : (h - 610) / 2 ))
"$XCLICK" $((ox + panel_x + 56)) $((oy + panel_y + 121)) 1 >/dev/null 2>&1

for _ in $(seq 1 50); do
    grep -q '\[sftp\] 后台操作开始' "$OUT/app.log" && break
    sleep 0.1
done
grep -q '\[sftp\] 后台操作开始' "$OUT/app.log" || fail "List 没有进入后台任务"
start_ms=$(date +%s%3N)
pass "List 已后台启动（UI 回调立即返回）"

# 3) sftp 卡死期间用鼠标开 TabBar 菜单 → New Tab：窗口必须仍处理 UI 事件。
sleep 1.5
menus_before=$(grep -c '\[ui\] tabbar menu at' "$OUT/app.log" || true)
"$XCLICK" $((ox + mx_local)) $((oy + 16)) 3 >/dev/null 2>&1
menu2_open=0
for _ in $(seq 1 30); do
    menus_now=$(grep -c '\[ui\] tabbar menu at' "$OUT/app.log" || true)
    if [ "$menus_now" -gt "$menus_before" ]; then menu2_open=1; break; fi
    sleep 0.1
done
[ "$menu2_open" = 1 ] || fail "sftp 卡死期间 TabBar 菜单打不开（UI 冻结）"
menu2_line=$(grep -o '\[ui\] tabbar menu at [0-9]*,[0-9]*' "$OUT/app.log" | tail -1)
menu2_x=$(echo "$menu2_line" | sed -E 's/.* at ([0-9]+),([0-9]+)/\1/')
menu2_y=$(echo "$menu2_line" | sed -E 's/.* at ([0-9]+),([0-9]+)/\2/')
menu2_left=$(( menu2_x < w - 180 ? menu2_x : w - 180 ))
tabs_before=$(grep -c '\[ws\] new tab' "$OUT/app.log" || true)
"$XCLICK" $((ox + menu2_left + 85)) $((oy + menu2_y + 4 + 1 * 30 + 14)) 1 >/dev/null 2>&1
newtab_ok=0
for _ in $(seq 1 30); do
    tabs_now=$(grep -c '\[ws\] new tab' "$OUT/app.log" || true)
    if [ "$tabs_now" -gt "$tabs_before" ]; then newtab_ok=1; break; fi
    sleep 0.1
done
[ "$newtab_ok" = 1 ] || fail "sftp 卡死期间 New Tab 没有执行（UI 冻结）"
pass "sftp 卡死期间窗口仍响应（菜单 + New Tab 已执行）"

# 4) 10s 硬超时：后台任务必须发结果、UI 进程存活、假 sftp 被 kill。
for _ in $(seq 1 130); do
    grep -q '后台操作完成' "$OUT/app.log" && break
    sleep 0.1
done
grep -q '后台操作完成' "$OUT/app.log" || fail "10s 后没有后台结果"
grep -q '超时（10s）' "$OUT/app.log" || fail "结果不是 10s 超时文案"
end_ms=$(date +%s%3N)
elapsed_ms=$(( end_ms - start_ms ))
[ "$elapsed_ms" -ge 9000 ] || fail "超时过早: ${elapsed_ms}ms"
[ "$elapsed_ms" -le 12000 ] || fail "超时太晚: ${elapsed_ms}ms"
kill -0 "$APP_PID" 2>/dev/null || fail "app 在超时后退出"
pass "sftp 10s 超时并返回结果（${elapsed_ms}ms）"

if [ -f "$OUT/sftp.pid" ]; then
    sftp_pid=$(cat "$OUT/sftp.pid")
    if kill -0 "$sftp_pid" 2>/dev/null; then
        fail "假 sftp 子进程未被 kill（pid=$sftp_pid）"
    fi
    pass "假 sftp 子进程已被 kill"
else
    echo "WARN: 没读到假 sftp pid，跳过 kill 校验"
fi
grep -q '后台操作完成: list / 失败: sftp 超时' "$OUT/app.log" ||
    fail "结果摘要不是可解释的 list 超时"
pass "超时原因可解释（日志摘要）"

echo "截图/日志目录: $OUT"
echo "全部 PASS"
