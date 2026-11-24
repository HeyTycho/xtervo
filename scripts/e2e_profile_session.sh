#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# W-01 Profile 外观 + 会话恢复端到端验收（headless Xvfb :136）。
#
# 覆盖：
#   1. Profiles→Edit 表单出现外观字段（字号/主题/光标/闪烁/scrollback）；
#   2. 打开 Profile Tab 后字号/主题/光标按 Profile 应用（截图 + 日志）；
#   3. 多 Tab + split + 每个 pane 的 cwd；
#   4. 开启 restore_session_on_startup 后正常退出写快照、重启恢复 Tab/split/cwd；
#   5. 快照损坏时自动恢复失败并回退新 Tab。
#
# 用法：
#   cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey
#   ./scripts/e2e_profile_session.sh
# 依赖：Xvfb/Openbox（仓库 .perf/）、xwd、python3 + Pillow、xwin/xclick/xkey。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${PROFILE_E2E_OUT:-$ROOT/.logs/profile-session/e2e}
mkdir -p "$OUT/shots" "$OUT/home" "$OUT/logs" /tmp/.X11-unix
# 连异常退出标记一起清：残留的 session.running / session.crashes 会让 app
# 启动时走“恢复上次 autosave”分支，和本用例自己构造的快照互相干扰。
rm -f "$OUT"/shots/*.png "$OUT"/app_*.log "$OUT/session.json" "$OUT/session.json.bak" \
    "$OUT/session.running" "$OUT/session.crashes" "$OUT/config.toml"

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

BIN=${XTERVO_BIN:-$MAIN_ROOT/target/debug/xtervo}
XWIN=${XWIN_BIN:-$MAIN_ROOT/target/debug/xwin}
XCLICK=${XCLICK_BIN:-$MAIN_ROOT/target/debug/xclick}
XKEY=${XKEY_BIN:-$MAIN_ROOT/target/debug/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f（先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey）" >&2; exit 2; }
done

export DISPLAY=${E2E_DISPLAY:-:136}
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
unset XTERVO_SETTINGS_LEGACY
if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY 已被占用；先停掉占用它的会话。" >&2
    exit 2
fi

XVFB_PID=""
OB_PID=""
APP_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

"$XVFB_BIN" "$DISPLAY" -screen 0 1600x1000x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }
if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
fi

WIN=""
ABS_X=0
ABS_Y=0
W=0
H=0

refresh_geom() {
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}')
    [ -n "$WIN" ] || { echo "找不到 xtervo 窗口" >&2; exit 2; }
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
}

start_app() {  # <tag>
    local tag=$1
    local log="$OUT/app_$tag.log"
    : >"$log"
    XTERVO_CONFIG="$OUT/config.toml" \
    XTERVO_PROFILES="$OUT/profiles.toml" \
    XTERVO_SESSION="$OUT/session.json" \
    XTERVO_LOG_DIR="$OUT/logs" \
    XTERVO_CURSOR_BLINK_PHASE=on \
        "$BIN" --cols 120 --rows 32 --shell /bin/sh >"$log" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 200); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "启动失败（$tag）：$log" >&2; exit 2; }
    "$XWIN" xtervo 20 20 >/dev/null 2>&1 || true
    sleep 0.9
    refresh_geom
    if [ "$W" != 1080 ] || [ "$H" != 736 ]; then
        echo "窗口 ${W}x${H} 与坐标假设 1080x736 不符（cell 9x22 / 120x32）" >&2
        exit 2
    fi
    echo "  app=$tag window=${W}x${H} at ${ABS_X},${ABS_Y}"
}

stop_app() {  # 正常点关闭按钮：触发退出快照；或 kill=false
    local graceful=${1:-yes}
    [ -n "$APP_PID" ] || return 0
    if [ "$graceful" = yes ]; then
        "$XCLICK" "$((ABS_X + W - 16))" "$((ABS_Y + 16))" >/dev/null 2>&1 || true
    else
        kill "$APP_PID" 2>/dev/null || true
    fi
    for _ in $(seq 1 120); do
        kill -0 "$APP_PID" 2>/dev/null || break
        sleep 0.05
    done
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=""
    sleep 0.4
}

shot() {
    local name=$1
    refresh_geom
    xwd -silent -id "$WIN" -out "$OUT/shots/$name.xwd"
    python3 "$ROOT/scripts/xwd2png.py" "$OUT/shots/$name.xwd" "$OUT/shots/$name.png" >/dev/null
    rm -f "$OUT/shots/$name.xwd"
    echo "  shot $OUT/shots/$name.png"
}

click() { "$XCLICK" "$((ABS_X + $1))" "$((ABS_Y + $2))" >/dev/null; sleep "${3:-0.35}"; }
focus_term() { "$XWIN" xtervo >/dev/null 2>&1 || true; sleep 0.15; }
type_text() { focus_term; "$XKEY" "$1" >/dev/null; }
enter() { "$XKEY" --key enter >/dev/null; }
# Ctrl+Shift+T 是 tab.new 的默认键（Ctrl+T 留给 shell 的 transpose）。
# Xvfb 首帧焦点未就绪时按键会丢，所以按目标 Tab 数轮询重试，而不是固定 sleep。
new_tab_until() {  # <expected-total-tabs>
    local want=$1 count=0
    for _attempt in 1 2 3; do
        "$XKEY" t --ctrl --shift >/dev/null
        for _ in $(seq 1 25); do
            count=$(grep -o '\[ws\] new tab: tabs=[0-9]*' "$OUT/app_A.log" | tail -1 | sed 's/.*=//')
            [ "${count:-0}" -ge "$want" ] && return 0
            sleep 0.1
        done
        focus_term
    done
    echo "新建 Tab 失败：期望 tabs=$want，当前 ${count:-0}" >&2
    return 1
}

# ---------------------------------------------------------------- 准备配置
cat >"$OUT/config.toml" <<'EOF'
version = 1
theme = "tokyo-night"
font_size = 15
scrollback = 2000
confirm_close = false
restore_session_on_startup = false
EOF
cat >"$OUT/profiles.toml" <<'EOF'
version = 2
next_id = 3

[[profiles]]
id = 1
name = "pretty"
kind = "local"
shell = "/bin/sh"
args = []
cwd = "/home/user"
env = []
font_size = 20.0
theme = "catppuccin-latte"
cursor_shape = "underline"
cursor_blink = "off"
scrollback = 5000

[[profiles]]
id = 2
name = "plain"
kind = "local"
shell = "/bin/sh"
EOF

echo "== 1. 启动并打开 Profile 表单 =="
start_app A
shot 01-start
click "$((W - 118))" 16 0.8                 # 设置齿轮
click 100 320 0.8                           # Profiles 分类
click 1005 176 0.8                          # pretty 行 Edit
shot 02-profile-form
# 表单几何（1080x736 窗口）：local 表单 440x490，左上角 (320,123)；
# 类型页 y=123+44=167..191，SSH 按钮中心 (482,179)。当前仍是 local 表单，
# 点的是 local 表单顶部的 SSH 页签；切过去后表单变成 440x700、左上角 (320,18)。
click 482 179 0.3                           # SSH 类型页
# SSH 字段从 form y=116 起、26px 行高 + 5px spacing；Auth 第 4 行中心 y=240，
# 输入框从 x=408 开始。
click 570 240 0.2                           # Auth 输入框
"$XKEY" a --ctrl >/dev/null
type_text password
sleep 0.6
shot 02b-profile-form-ssh
# Cancel 固定在 form-h-50：SSH 表单 y=18+700-50=668..696，中心 (612,682)。
click 612 682 0.4                           # Cancel（SSH 700px 高表单）
click 1005 176 0.8                          # 重新打开 Edit（local 紧凑表单）

echo "== 2. 表单保存外观字段 → 打开 Profile Tab =="
# local 表单 Appearance 区块 y=413：标题 14 + spacing 6 后第一行 433..459，
# History 输入框在右列（x 约 618..744），中心 (681,446)。
click 735 446 0.2                           # History 输入框右侧（光标放到末尾）
for _ in 1 2 3 4; do "$XKEY" --key backspace >/dev/null; done
type_text 6000
# Save 在 local 表单 y=123+440..468=563..591，中心 (700,577)。
click 700 577 0.7                           # Save（local 紧凑表单）
grep -q 'scrollback = 6000' "$OUT/profiles.toml" || {
    echo "Profile 表单保存 scrollback 失败" >&2
    cat "$OUT/profiles.toml" >&2
    exit 2
}
echo "  form save ok: scrollback=6000"
click 300 176 1.6                           # 点 pretty 行 → 新 Tab
click 600 300 0.2
type_text pwd; enter
sleep 0.8
shot 03-profile-applied
grep -q '\[profile\] open "pretty"' "$OUT/app_A.log"
grep -q 'font_size=Some(20.0)' "$OUT/app_A.log"
grep -q 'theme=Some("catppuccin-latte")' "$OUT/app_A.log"
grep -q 'cursor=Some("underline")/Some(false)' "$OUT/app_A.log"

echo "== 3. 构造 3 Tab + split + 不同 cwd =="
click 600 300 0.2
type_text "cd /var"; enter
"$XKEY" e --ctrl --shift >/dev/null
sleep 1.2
click 800 300 0.2
type_text "cd /etc"; enter
new_tab_until 3
click 600 300 0.2
type_text "cd /usr"; enter
sleep 0.6
shot 04-before-restart
grep -q '\[ws\] split pane=2 dir=Row' "$OUT/app_A.log"
grep -q '\[ws\] new tab: tabs=3' "$OUT/app_A.log"

echo "== 4. 开启启动自动恢复 → 正常退出写快照 =="
sed -i 's/^restore_session_on_startup = false/restore_session_on_startup = true/' "$OUT/config.toml"
grep -q '^restore_session_on_startup = true' "$OUT/config.toml"
sleep 1.2
grep -q 'restore_session_on_startup' "$OUT/app_A.log"
stop_app yes
grep -q '\[session\] exit: 已保存' "$OUT/app_A.log"
[ -f "$OUT/session.json" ] || { echo "退出后没有 session.json" >&2; exit 2; }
python3 - "$OUT/session.json" <<'PY'
import json, sys
snap = json.load(open(sys.argv[1]))
# 当前快照是 v2 多窗口结构：顶层 windows[]，单窗口才有 tabs/active_tab。
assert snap["version"] == 2, snap.get("version")
assert len(snap["windows"]) == 1, snap.get("windows")
win = snap["windows"][0]
assert len(win["tabs"]) == 3, win["tabs"]
assert win["active_tab"] == 2, win["active_tab"]
cwds, profiles = [], []
def walk(t):
    if t["kind"] == "leaf":
        cwds.append(t.get("cwd"))
        profiles.append(t.get("profile_id"))
        return 1
    assert t["kind"] == "split", t
    return walk(t["a"]) + walk(t["b"])
leaves = [walk(t["tree"]) for t in win["tabs"]]
# Tab 顺序：原 Terminal 1（1 leaf）、pretty（split 2 leaves）、新 Terminal 2（1 leaf）。
assert leaves == [1, 2, 1], leaves
assert cwds[1:4] == ["/var", "/etc", "/usr"], cwds
assert profiles[1] == 1, "pretty Tab 的第一个 pane 应关联 Profile id=1: %s" % profiles
print("  snapshot ok: tabs=3 leaves=%s cwds=%s profiles=%s" % (leaves, cwds, profiles))
PY

echo "== 5. 重启自动恢复 Tab/split/cwd/Profile =="
start_app B
shot 05-restored
grep -q '\[session\] 已按快照恢复' "$OUT/app_B.log"
grep -q '恢复 local Profile id=1' "$OUT/app_B.log"

# 活动 Tab（第三个）cwd 应为 /usr
click 600 300 0.2
type_text pwd; enter
sleep 0.7
shot 06-restored-tab2-pwd

# 切到 pretty Tab：左 pane /var，右 pane /etc；左 pane 仍是 Profile 浅色/大字号
click 180 16 0.7
click 250 300 0.2
type_text pwd; enter
sleep 0.5
shot 07-restored-split-left-pwd
click 800 300 0.2
type_text pwd; enter
sleep 0.5
shot 08-restored-split-right-pwd

python3 - "$OUT/shots/07-restored-split-left-pwd.png" "$OUT/shots/08-restored-split-right-pwd.png" <<'PY'
import sys
from PIL import Image
def brightness(path, box):
    im = Image.open(path).convert("RGB").crop(box)
    px = list(im.getdata())
    return sum(sum(p) / 3 for p in px) / len(px)
left = brightness(sys.argv[1], (80, 80, 480, 680))
right = brightness(sys.argv[2], (620, 80, 1000, 680))
assert left > 180, f"Profile 主题应使左 pane 为浅色，实际亮度 {left:.1f}"
assert right < 80, f"全局主题右 pane 应为深色，实际亮度 {right:.1f}"
print(f"  pixel check ok: profile-left={left:.1f} global-right={right:.1f}")
PY

echo "== 6. 损坏快照 → 自动恢复失败回退新 Tab =="
stop_app no
# 产品在主快照损坏时会回退 .bak；要验证“没有可用快照时回退新 Tab”，
# 必须连备份一起删掉，否则这里测到的是备份恢复而不是 fallback。
printf '{broken json' >"$OUT/session.json"
rm -f "$OUT/session.json.bak"
start_app C
shot 09-fallback-newtab
grep -q '\[ws\] new tab: tabs=1' "$OUT/app_C.log"
if grep -q '\[session\] 已按快照恢复' "$OUT/app_C.log"; then
    echo "损坏快照不应被恢复" >&2
    exit 2
fi
echo "  fallback ok: 新 Tab 启动"

echo "全部通过；截图在 $OUT/shots"

echo "== 7. Settings → Behavior 自动恢复开关可见 =="
click "$((W - 118))" 16 0.8
click 800 56 0.3
type_text "restore session"
sleep 0.7
shot 10-settings-restore-search
click 300 148 0.9
shot 11-settings-restore-setting
# 搜索跳转后 Behavior 卡片贴到内容区顶部：Restore session 行的开关中心
# 约 (1021,321)。点完轮询配置文件，避免把磁盘写入当同步。
click 1025 321 0.3
for _ in $(seq 1 30); do
    grep -q '^restore_session_on_startup = false' "$OUT/config.toml" && break
    sleep 0.1
done
grep -q '^restore_session_on_startup = false' "$OUT/config.toml"
shot 12-settings-restore-off
