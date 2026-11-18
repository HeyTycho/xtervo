#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# UI-PANE-010 + SESS-007 端到端验收（headless Xvfb）。
#
# 覆盖：
#   1. 启动即显示连接状态 pill（本地 Connected），进程退出后转 Disconnected/Failed；
#   2. 命令面板列出 Workspace 的打开/重命名/删除/导出动态命令；
#   3. 设置页 Workspaces 列表（名字 / pane 数 / 创建时间）；
#   4. 重命名 → workspaces.toml 立即更新，打开的 Tab 不受影响；
#   5. 导出 JSON → 文件可解析且布局/比例/pane 数一致；
#   6. 删除 → 二次确认、只删记录、不关闭会话；最终列表只剩另一个工作区。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey --offline
#   ./scripts/e2e_workspace_ui.sh
# 依赖：Xvfb（.perf/ 或系统）、xwd、python3 + Pillow、xwin/xclick/xkey。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${WORKSPACE_E2E_OUT:-$ROOT/.logs/workspace-ui/e2e}
mkdir -p "$OUT/shots" "$OUT/home" "$OUT/logs" "$OUT/instance" /tmp/.X11-unix
rm -f "$OUT"/shots/*.png "$OUT"/app_*.log "$OUT"/tervo-workspace-*.json

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

BIN=${TERVO_BIN:-$ROOT/target/debug/tervo}
# 输入工具可以用主工作树已构建的二进制（worktree 不重复编译工具）。
XWIN=${XWIN_BIN:-$ROOT/target/debug/xwin}
[ -x "$XWIN" ] || XWIN=$MAIN_ROOT/target/debug/xwin
XCLICK=${XCLICK_BIN:-$ROOT/target/debug/xclick}
[ -x "$XCLICK" ] || XCLICK=$MAIN_ROOT/target/debug/xclick
XKEY=${XKEY_BIN:-$ROOT/target/debug/xkey}
[ -x "$XKEY" ] || XKEY=$MAIN_ROOT/target/debug/xkey
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f（先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey）" >&2; exit 2; }
done

export DISPLAY=${E2E_DISPLAY:-:145}
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
unset TERVO_SETTINGS_LEGACY
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
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}')
    [ -n "$WIN" ] || { echo "找不到 Tervo 窗口" >&2; exit 2; }
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
}

start_app() {  # <tag>
    local tag=$1
    local log="$OUT/app_$tag.log"
    : >"$log"
    TERVO_CONFIG="$OUT/config.toml" \
    TERVO_PROFILES="$OUT/profiles.toml" \
    TERVO_WORKSPACES="$OUT/workspaces.toml" \
    TERVO_SESSION="$OUT/session.json" \
    TERVO_INSTANCE_DIR="$OUT/instance" \
    TERVO_LOG_DIR="$OUT/logs" \
    TERVO_CURSOR_BLINK_PHASE=on \
        "$BIN" --cols 120 --rows 32 --shell /bin/sh >"$log" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 200); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "启动失败（$tag）：$log" >&2; exit 2; }
    "$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
    sleep 1.0
    refresh_geom
    if [ "$W" != 1080 ] || [ "$H" != 736 ]; then
        echo "窗口 ${W}x${H} 与坐标假设 1080x736 不符（cell 9x22 / 120x32）" >&2
        exit 2
    fi
    echo "  app=$tag window=${W}x${H} at ${ABS_X},${ABS_Y}"
}

shot() {
    local name=$1
    refresh_geom
    xwd -silent -id "$WIN" -out "$OUT/shots/$name.xwd"
    python3 "$MAIN_ROOT/scripts/xwd2png.py" "$OUT/shots/$name.xwd" "$OUT/shots/$name.png" >/dev/null
    rm -f "$OUT/shots/$name.xwd"
    echo "  shot $OUT/shots/$name.png"
}

click() { "$XCLICK" "$((ABS_X + $1))" "$((ABS_Y + $2))" >/dev/null; sleep "${3:-0.35}"; }
focus_term() { "$XWIN" Tervo >/dev/null 2>&1 || true; sleep 0.15; }
type_text() { focus_term; "$XKEY" "$1" >/dev/null; }
enter() { "$XKEY" --key enter >/dev/null; }
escape() { "$XKEY" --key escape >/dev/null; }
key() { "$XKEY" "$@" >/dev/null; }
wheel_down() { click 600 500 5; sleep 0.25; }

palette() {  # <query> [screenshot]
    focus_term
    click 600 300 0.15
    key p --ctrl --shift
    sleep 0.4
    key "$1"
    sleep 0.4
    if [ -n "${2:-}" ]; then shot "$2"; fi
}

palette_pick() {  # <query> <行号>：结果按标题排序；直接点目标行（方向键在
    # TextInput 里不可靠）。palette 弹窗 x=(W-420)/2、y=56，行高 26+6。
    local tag row=$2
    tag=$(printf '%s' "$1" | tr ' :' '__')
    palette "$1" "debug-palette-$tag"
    click 540 $((102 + row * 32 + 13)) 0.7
}

fail() { echo "FAIL: $*" >&2; exit 2; }

# ---------------------------------------------------------------- 准备配置
cat >"$OUT/config.toml" <<'EOF'
version = 1
language = "en"
theme = "yaru-dark"
font_size = 15
scrollback = 2000
confirm_close = false
restore_session_on_startup = false
reconnect_max_attempts = 3
reconnect_backoff_ms = [2000, 5000, 10000]
EOF
cat >"$OUT/profiles.toml" <<'EOF'
version = 2
next_id = 1
EOF
cat >"$OUT/workspaces.toml" <<'EOF'
version = 1

[[workspaces]]
name = "two-up"
created_at = 1700000000

[workspaces.tree]
kind = "split"
dir = "row"
ratio = 0.5

[workspaces.tree.a]
kind = "leaf"
cwd = "/tmp"

[workspaces.tree.b]
kind = "leaf"
cwd = "/var"

[[workspaces]]
name = "solo"
created_at = 1700000100

[workspaces.tree]
kind = "leaf"
cwd = "/etc"
EOF

echo "== 1. 启动：连接状态 pill + 工作区列表加载 =="
start_app A
shot 01-start-connected
grep -Fq '[connection] pane=1 state=Connected remote=false source=spawn' "$OUT/app_A.log" \
    || fail "缺少本地 pane Connected 状态日志"
grep -Fq '[workspace] 列表 rows=2' "$OUT/app_A.log" || fail "工作区列表未加载 2 条"

echo "== 2. 本地会话 exit 0 → Disconnected =="
type_text exit; enter
sleep 1.4
shot 02-disconnected
grep -Fq '[connection] pane=1 state=Disconnected' "$OUT/app_A.log" \
    || fail "exit 0 后状态不是 Disconnected"
key t --ctrl
sleep 1.0

echo "== 3. 非零退出 → Failed =="
type_text 'exit 3'; enter
sleep 1.4
shot 03-failed
grep -Fq 'state=Failed' "$OUT/app_A.log" || fail "exit 3 后状态不是 Failed"
key t --ctrl
sleep 1.0

echo "== 4. 命令面板工作区动态命令 =="
palette 'Workspace' 04-palette-workspaces
escape
click 600 300 0.2

echo "== 5. 打开 solo 工作区 =="
palette_pick 'Workspace: solo' 2
grep -Fq '[workspace] 已打开「solo」' "$OUT/app_A.log" || fail "workspace.open 未执行"
shot 05-opened-solo

echo "== 6. 重命名 two-up → renamed =="
palette_pick 'Rename Workspace: two-up' 0
grep -Fq '[workspace] 重命名弹窗 name="two-up"' "$OUT/app_A.log" || fail "重命名弹窗未打开"
shot 06-rename-dialog
key a --ctrl
type_text renamed
enter
sleep 0.8
grep -Fq 'name = "renamed"' "$OUT/workspaces.toml" || fail "workspaces.toml 未写入新名字"
if grep -Fq 'name = "two-up"' "$OUT/workspaces.toml"; then fail "旧名字仍在 workspaces.toml"; fi
grep -Fq '[workspace] 已重命名 "two-up" -> "renamed"' "$OUT/app_A.log" || fail "缺少重命名日志"
shot 07-renamed

echo "== 6b. 打开 renamed，随后删除它（验证“删除正在使用的工作区”安全）=="
palette_pick 'Workspace: renamed' 2
grep -Fq '[workspace] 已打开「renamed」' "$OUT/app_A.log" || fail "无法打开 renamed 工作区"
shot 07b-opened-renamed

echo "== 7. 导出 JSON =="
palette_pick 'Export Workspace JSON: renamed' 0
grep -Fq '[workspace] 导出打开 name="renamed"' "$OUT/app_A.log" || fail "导出弹窗未打开"
shot 08-export-dialog
enter
sleep 0.9
EXPORT="$HOME/tervo-workspace-renamed.json"
[ -f "$EXPORT" ] || fail "导出文件不存在：$EXPORT"
python3 - "$EXPORT" <<'PYJSON'
import json, sys
ws = json.load(open(sys.argv[1]))
assert ws["name"] == "renamed", ws
assert ws["created_at"] == 1700000000, ws
assert ws["tree"]["kind"] == "split", ws
assert abs(ws["tree"]["ratio"] - 0.5) < 1e-6, ws
leaves = []
def walk(t):
    if t["kind"] == "leaf":
        leaves.append(t.get("cwd"))
        return
    walk(t["a"]); walk(t["b"])
walk(ws["tree"])
assert leaves == ["/tmp", "/var"], leaves
print("  export ok: name=%s panes=%s cwds=%s" % (ws["name"], len(leaves), leaves))
PYJSON

echo "== 8. 删除确认 → 只删记录不关 Tab =="
palette_pick 'Delete Workspace: renamed' 0
grep -Fq '[workspace] 删除确认 name="renamed"' "$OUT/app_A.log" || fail "删除确认未打开"
shot 09-delete-confirm
# 确认弹窗 430x190 居中；Delete 按钮在右下（root 坐标约 651,419）。
click 695 433 0.8
grep -Fq '[workspace] 已删除 "renamed"' "$OUT/app_A.log" || fail "删除未执行"
if grep -Fq 'name = "renamed"' "$OUT/workspaces.toml"; then fail "删除后 workspaces.toml 仍有记录"; fi
grep -Fq '[workspace] 列表 rows=1' "$OUT/app_A.log" || fail "删除后列表未刷新为 1 条"
kill -0 "$APP_PID" 2>/dev/null || fail "删除正在使用的工作区后应用退出"
# 删除记录后，由它打开的 renamed Tab 仍在（截图 10 的标签栏可见）。
shot 10-after-delete

echo "== 9. 设置页最终工作区列表 =="
click "$((W - 118))" 16 0.8
click 100 320 0.8
wheel_down; wheel_down; wheel_down
shot 11-workspaces-list

printf 'ok\n' >"$OUT/RESULT"
echo "PASS: workspace UI + connection state e2e (shots: $OUT/shots)"
