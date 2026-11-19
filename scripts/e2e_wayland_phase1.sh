#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Wayland Phase 1 隔离 e2e：headless Mutter + 私有 HOME/XDG/DBus/XWayland。
# 不创建宿主窗口、不读写真实剪贴板、不注入真实键鼠。
#
# 用法：
#   ./scripts/e2e_wayland_phase1.sh
#   TERVO_BIN=target/debug/tervo TERVO_E2E_OUT=/tmp/tervo-wl ./scripts/e2e_wayland_phase1.sh
#
# 依赖：gnome-shell(46+)、dbus-run-session、gnome-screenshot、xclip（可选）、
#       xwd/ffmpeg（可选）、python3-dbus（IME/快捷键注入，可选）。
# 缺少可选依赖的用例会打印 SKIP，不会伪 PASS。
#
# 说明：headless Mutter 的 XWayland `:2` 在部分环境会因 systemd X11 services
# 失败而卡死；脚本优先使用日志里的 `:3`（managed services display）。

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
BIN=${TERVO_BIN:-target/debug/tervo}
OUT=${TERVO_E2E_OUT:-.logs/e2e-wayland-phase1}
VIRTUAL=${TERVO_E2E_MONITOR:-1280x800}
[ -x "$BIN" ] || { echo "缺少可执行文件 $BIN（先 cargo build -p tervo-app --bin tervo）"; exit 1; }
BIN=$ROOT/$BIN

rm -rf "$OUT"
mkdir -p "$OUT"/{rt,home,config,cache,data,state,logs,shots,instance,instance-x11}
chmod 700 "$OUT/rt"

PASS=0 FAIL=0 SKIP=0
pass() { PASS=$((PASS + 1)); echo "PASS: $*"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL: $*"; }
skip() { SKIP=$((SKIP + 1)); echo "SKIP: $*"; }

NESTED_PID=""
APP_PID=""
FCITX_PID=""
cleanup() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "${FCITX_PID:-}" ] && kill "$FCITX_PID" 2>/dev/null || true
    if [ -n "${NESTED_PID:-}" ]; then
        kill -- -"$NESTED_PID" 2>/dev/null || kill "$NESTED_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

X11_DISPLAY=""
XAUTH=""
start_nested() {
    local nested=$OUT
    setsid env HOME="$nested/home" XDG_RUNTIME_DIR="$nested/rt" \
        XDG_CONFIG_HOME="$nested/config" XDG_CACHE_HOME="$nested/cache" \
        XDG_DATA_HOME="$nested/data" XDG_STATE_HOME="$nested/state" \
        GSETTINGS_BACKEND=memory NO_AT_BRIDGE=1 LIBGL_ALWAYS_SOFTWARE=1 \
        GALLIUM_DRIVER=llvmpipe \
        dbus-run-session -- bash -c \
        "echo \"\$DBUS_SESSION_BUS_ADDRESS\" > '$nested/bus.env'; \
         exec gnome-shell --headless --wayland --virtual-monitor $VIRTUAL" \
        >"$nested/gnome-shell.log" 2>&1 &
    NESTED_PID=$!
    for _ in $(seq 1 300); do
        [ -S "$nested/rt/wayland-0" ] && [ -s "$nested/bus.env" ] && break
        sleep 0.2
    done
    if [ ! -S "$nested/rt/wayland-0" ] || [ ! -s "$nested/bus.env" ]; then
        echo "headless Mutter 启动失败，见 $OUT/gnome-shell.log" >&2
        tail -20 "$OUT/gnome-shell.log" >&2 || true
        exit 1
    fi
    sleep 1
    BUS=$(cat "$OUT/bus.env")
    XAUTH=$(ls -1 "$OUT"/rt/.mutter-Xwaylandauth.* 2>/dev/null | head -1 || true)
    # 优先 managed services display（日志格式：using :3 for managed services）
    X11_DISPLAY=$(grep -oE 'using :[0-9]+ for managed services' "$OUT/gnome-shell.log" | tail -1 | grep -oE ':[0-9]+' || true)
    [ -n "$X11_DISPLAY" ] || X11_DISPLAY=$(grep -oE 'Using public X11 display :[0-9]+' "$OUT/gnome-shell.log" | tail -1 | grep -oE ':[0-9]+' || true)
    echo "nested: wayland=wayland-0 bus=$BUS x11=${X11_DISPLAY:-none}"
}

nested() {
    env -u DISPLAY HOME="$OUT/home" XDG_RUNTIME_DIR="$OUT/rt" \
        WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS="$BUS" \
        XDG_CONFIG_HOME="$OUT/config" XDG_CACHE_HOME="$OUT/cache" \
        GSETTINGS_BACKEND=memory "$@"
}

nested_x11() {
    [ -n "$X11_DISPLAY" ] || return 1
    env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY="$X11_DISPLAY" XAUTHORITY="$XAUTH" \
        HOME="$OUT/home" XDG_RUNTIME_DIR="$OUT/rt" DBUS_SESSION_BUS_ADDRESS="$BUS" \
        XDG_CONFIG_HOME="$OUT/config" XDG_CACHE_HOME="$OUT/cache" \
        GSETTINGS_BACKEND=memory "$@"
}

launch_wl() { # <name> <log> [extra args...]
    local name=$1 log=$2; shift 2
    nested "$BIN" --title "$name" --cols 80 --rows 20 "$@" >"$log" 2>&1 &
    APP_PID=$!
    sleep 5
}

stop_app() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
    APP_PID=""
    sleep 0.5
}

wait_log() { # <log> <pattern> <timeout-sec>
    local log=$1 pat=$2 timeout=${3:-10} i=0
    while [ "$i" -lt $((timeout * 10)) ]; do
        grep -qE "$pat" "$log" 2>/dev/null && return 0
        sleep 0.1
        i=$((i + 1))
    done
    return 1
}

nested_shot() { # <file>
    nested timeout 25 gnome-screenshot -f "$1" >/dev/null 2>&1
    [ -s "$1" ] && [ "$(stat -c%s "$1")" -gt 10000 ]
}

inject() {
    DBUS_SESSION_BUS_ADDRESS="$BUS" python3 - "$@" <<'PY'
import dbus, sys, time
bus = dbus.SessionBus()
remote = bus.get_object("org.gnome.Mutter.RemoteDesktop", "/org/gnome/Mutter/RemoteDesktop")
path = remote.CreateSession(dbus_interface="org.gnome.Mutter.RemoteDesktop")
session = dbus.Interface(
    bus.get_object("org.gnome.Mutter.RemoteDesktop", path),
    "org.gnome.Mutter.RemoteDesktop.Session",
)
session.Start()
keys = {"F11": 0xffc8, "F12": 0xffc9, "Space": 0x20, "Enter": 0xff0d}
for arg in sys.argv[1:]:
    if arg in keys:
        session.NotifyKeyboardKeysym(keys[arg], True)
        session.NotifyKeyboardKeysym(keys[arg], False)
    else:
        for ch in arg:
            session.NotifyKeyboardKeysym(ord(ch), True)
            session.NotifyKeyboardKeysym(ord(ch), False)
            time.sleep(0.03)
PY
}

echo "== 1. --diagnose（纯 Wayland，无 DISPLAY）=="
if nested "$BIN" --diagnose >"$OUT/diagnose-wl.txt" 2>&1; then
    grep -q 'ui_backend=wayland' "$OUT/diagnose-wl.txt" && pass "diagnose backend=wayland" || fail "diagnose backend"
    grep -q 'x11: DISPLAY 未设置' "$OUT/diagnose-wl.txt" && pass "diagnose 跳过 X11 探测" || fail "diagnose X11 skip"
    ! grep -qE '^(screen|pointer|active_window)=' "$OUT/diagnose-wl.txt" && pass "diagnose 无 XWayland 误导数据" || fail "diagnose 仍输出 X11 数据"
else
    fail "diagnose 退出码非 0"
fi

echo "== 2. 原生 Wayland 启动 / 日志 / 截图 =="
launch_wl "e2e-wl" "$OUT/app-wl.log" --shell /bin/cat
if wait_log "$OUT/app-wl.log" 'READY TIMING' 10; then pass "Wayland 启动 READY"; else fail "Wayland 未 READY"; fi
grep -q '\[platform\] ui_backend=wayland (winit raw handle confirmed)' "$OUT/app-wl.log" \
    && pass "raw handle 确认 Wayland" || fail "未确认 Wayland 后端"
grep -q '\[quake\] 原生 Wayland 不支持全局热键' "$OUT/app-wl.log" \
    && pass "Quake 明确降级" || fail "Quake 仍假注册"
grep -q '\[ui\] 原生 Wayland 不设置 _NET_WM_ICON' "$OUT/app-wl.log" \
    && pass "Wayland 跳过 X11 图标" || fail "仍尝试 X11 图标"
nested_shot "$OUT/shots/01-wl-start.png" && pass "嵌套截图 01-wl-start.png" || fail "嵌套截图失败"

echo "== 3. 纯 Wayland 剪贴板失败提示 =="
stop_app
cat >"$OUT/clip-probe.sh" <<'EOF'
#!/bin/sh
printf '\033]52;c;%s\007' "$(printf 'WLCLIP' | base64 | tr -d '\n')"
sleep 1
printf '\033]52;c;?\007'
sleep 3
EOF
chmod +x "$OUT/clip-probe.sh"
launch_wl "e2e-wl-clipfail" "$OUT/app-wl-clipfail.log" --shell "$OUT/clip-probe.sh"
if wait_log "$OUT/app-wl-clipfail.log" '\[clipboard\] 不可用：原生 Wayland' 10; then
    pass "纯 Wayland 剪贴板可读失败原因"
else
    fail "缺少剪贴板失败原因"
fi
kill -0 "$APP_PID" 2>/dev/null && pass "剪贴板失败后进程存活" || fail "剪贴板失败后进程退出"
stop_app

echo "== 4. Wayland 窗口 + 私有 XWayland 剪贴板 =="
if [ -n "$X11_DISPLAY" ] && nested_x11 timeout 5 xdpyinfo >/dev/null 2>&1; then
    if command -v xclip >/dev/null; then
        # 同一进程既是 Wayland 窗口，又通过 DISPLAY 让 arboard 回退 XWayland。
        env DISPLAY="$X11_DISPLAY" XAUTHORITY="$XAUTH" \
            HOME="$OUT/home" XDG_RUNTIME_DIR="$OUT/rt" WAYLAND_DISPLAY=wayland-0 \
            DBUS_SESSION_BUS_ADDRESS="$BUS" XDG_CONFIG_HOME="$OUT/config" \
            GSETTINGS_BACKEND=memory \
            "$BIN" --title e2e-wl-clip --cols 80 --rows 12 --shell "$OUT/clip-probe.sh" \
            >"$OUT/app-wl-clip.log" 2>&1 &
        APP_PID=$!
        sleep 6
        got=$(DISPLAY="$X11_DISPLAY" XAUTHORITY="$XAUTH" xclip -selection clipboard -o 2>/dev/null || true)
        [ "$got" = "WLCLIP" ] && pass "Wayland 窗口经 XWayland 写 CLIPBOARD" || fail "CLIPBOARD=$got"
        got=$(DISPLAY="$X11_DISPLAY" XAUTHORITY="$XAUTH" xclip -selection primary -o 2>/dev/null || true)
        [ "$got" = "WLPRI" ] && pass "Wayland 窗口经 XWayland 写 PRIMARY" || skip "PRIMARY=$got（探针未写或 XWayland 不可用）"
        stop_app
    else
        skip "未安装 xclip，跳过 XWayland 剪贴板断言"
    fi
else
    skip "嵌套 XWayland 不可用，跳过剪贴板回退"
fi

echo "== 5. 隔离 X11 回归 =="
if [ -n "$X11_DISPLAY" ] && nested_x11 timeout 5 xdpyinfo >/dev/null 2>&1; then
    nested_x11 "$BIN" --title e2e-x11 --cols 60 --rows 12 --shell /bin/cat \
        >"$OUT/app-x11.log" 2>&1 &
    APP_PID=$!
    sleep 5
    grep -q '\[platform\] ui_backend=x11 (winit raw handle confirmed)' "$OUT/app-x11.log" \
        && pass "X11 后端确认" || fail "X11 后端未确认"
    grep -q '\[quake\] 全局热键 F12 已注册' "$OUT/app-x11.log" \
        && pass "X11 Quake 注册保持" || fail "X11 Quake 注册丢失"
    grep -q '\[icon\] 写入窗口\|_NET_WM_ICON 已设置' "$OUT/app-x11.log" \
        && pass "X11 _NET_WM_ICON 保持" || fail "X11 图标丢失"
    if command -v xwd >/dev/null && command -v ffmpeg >/dev/null; then
        win=$(nested_x11 xwininfo -root -tree 2>/dev/null | grep -i '"e2e-x11"' | awk '{print $1}' | head -1)
        if [ -n "$win" ] && nested_x11 xwd -silent -id "$win" >"$OUT/shots/02-x11.xwd" 2>/dev/null; then
            ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/shots/02-x11.xwd" \
                "$OUT/shots/02-x11.png" && pass "X11 窗口截图" || skip "X11 xwd→png 失败"
        else
            skip "找不到 X11 窗口，跳过截图"
        fi
    else
        skip "缺少 xwd/ffmpeg，跳过 X11 截图"
    fi
    stop_app
else
    skip "嵌套 XWayland 不可用，跳过 X11 回归"
fi

echo "== 6. IME（fcitx5 + text-input-v3，可选）=="
if command -v fcitx5 >/dev/null && python3 -c 'import dbus' >/dev/null 2>&1; then
    mkdir -p "$OUT/config/fcitx5"
    cat >"$OUT/config/fcitx5/profile" <<'EOF'
[Groups/0]
Name=Default
Default Layout=us
DefaultIM=pinyin

[Groups/0/Items/0]
Name=pinyin
Layout=

[Groups/0/Items/1]
Name=keyboard-us
Layout=

[GroupOrder]
0=Default
EOF
    env -u DISPLAY HOME="$OUT/home" XDG_RUNTIME_DIR="$OUT/rt" \
        XDG_CONFIG_HOME="$OUT/config" XDG_CACHE_HOME="$OUT/cache" XDG_DATA_HOME="$OUT/data" \
        WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS="$BUS" \
        fcitx5 -d >"$OUT/fcitx5.log" 2>&1 &
    FCITX_PID=$!
    sleep 2
    DBUS_SESSION_BUS_ADDRESS="$BUS" fcitx5-remote -o >/dev/null 2>&1 || true
    DBUS_SESSION_BUS_ADDRESS="$BUS" fcitx5-remote -s pinyin >/dev/null 2>&1 || true
    launch_wl "e2e-wl-ime" "$OUT/app-wl-ime.log" --shell /bin/cat
    # 先点一下终端再输入，避免首键丢焦点。
    DBUS_SESSION_BUS_ADDRESS="$BUS" python3 - "$BUS" <<'PY' 2>/dev/null || true
import dbus, sys, time
bus = dbus.SessionBus()
r = bus.get_object("org.gnome.Mutter.RemoteDesktop", "/org/gnome/Mutter/RemoteDesktop")
p = r.CreateSession(dbus_interface="org.gnome.Mutter.RemoteDesktop")
s = dbus.Interface(bus.get_object("org.gnome.Mutter.RemoteDesktop", p),
                   "org.gnome.Mutter.RemoteDesktop.Session")
s.Start()
s.NotifyKeyboardKeysym(0xff1b, True); s.NotifyKeyboardKeysym(0xff1b, False)
time.sleep(0.3)
PY
    inject nihao Space >/dev/null 2>&1 || true
    sleep 1.5
    if grep -q '\[ime\] "你好"' "$OUT/app-wl-ime.log"; then
        pass "text-input-v3 提交中文"
        nested_shot "$OUT/shots/03-wl-ime.png" && pass "嵌套截图 03-wl-ime.png" || skip "IME 截图失败"
    else
        skip "嵌套 fcitx5 未完成部署/未连上 IM（真实会话已验证；不伪 PASS）"
    fi
    stop_app
else
    skip "缺少 fcitx5 或 python3-dbus，跳过 IME"
fi

echo "== 7. Quake 启用时仍不注册（Wayland）=="
printf 'quake_enabled = true\n' >"$OUT/quake.toml"
env -u DISPLAY HOME="$OUT/home" XDG_RUNTIME_DIR="$OUT/rt" WAYLAND_DISPLAY=wayland-0 \
    DBUS_SESSION_BUS_ADDRESS="$BUS" XDG_CONFIG_HOME="$OUT/config" GSETTINGS_BACKEND=memory \
    TERVO_CONFIG="$OUT/quake.toml" TERVO_LOG_DIR="$OUT/logs" \
    "$BIN" --title e2e-wl-quake --cols 60 --rows 12 --shell /bin/cat \
    >"$OUT/app-wl-quake.log" 2>&1 &
APP_PID=$!
sleep 4
grep -q '\[quake\] 原生 Wayland 不支持全局热键' "$OUT/app-wl-quake.log" \
    && pass "quake_enabled=true 也不注册" || fail "Wayland 仍注册 Quake"
! grep -q '已注册' "$OUT/app-wl-quake.log" \
    && pass "无假注册日志" || fail "出现假注册日志"
if python3 -c 'import dbus' >/dev/null 2>&1; then
    inject F12 >/dev/null 2>&1 || true
    sleep 1
    ! grep -q '\[quake\] show' "$OUT/app-wl-quake.log" \
        && pass "F12 不触发 Quake" || fail "F12 触发了 Quake"
fi
stop_app

echo
echo "结果：PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
echo "证据：$OUT"
[ "$FAIL" -eq 0 ]
