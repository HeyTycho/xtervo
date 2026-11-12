#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 最大化状态机隔离验收：独立 Xvfb + Openbox + 独立 HOME；拒绝 :1。
#
# 覆盖：
#   单屏 max/restore 两轮；伪双屏 _NET_WORKAREA 5120x1413
#   快速 6 连点 / 慢速 6 连点
#   双实例独立
#   外部 WM 改变 _NET_WM_STATE 后点按钮
#   启动时已最大化
#   无 WM（明确失败、不崩溃、click 仍翻 desired）
#
# 只跑 release binary：BIN 必须位于 */release/*，否则直接失败。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DISPLAY_NUM=${DISPLAY_NUM:-:98}
NO_WM_DISPLAY=${NO_WM_DISPLAY:-:99}
for display in "$DISPLAY_NUM" "$NO_WM_DISPLAY"; do
    if [ "$display" = ":1" ]; then
        echo "ERROR: 不允许在用户 :1 上测试" >&2
        exit 2
    fi
done
if [ "$DISPLAY_NUM" = "$NO_WM_DISPLAY" ]; then
    echo "ERROR: DISPLAY_NUM 与 NO_WM_DISPLAY 不能相同" >&2
    exit 2
fi

BIN=${BIN:-$ROOT/target/release/tervo}
case "$BIN" in
    */release/*) ;;
    *)
        echo "ERROR: BIN 必须是 release 构建（当前 $BIN）；不接受 debug-only PASS" >&2
        exit 2
        ;;
esac
if [ ! -x "$BIN" ]; then
    echo "ERROR: release binary 不存在: $BIN（先 cargo build -p tervo-app --release）" >&2
    exit 2
fi

# `.perf` 是仓库根目录未跟踪的测试依赖；worktree 里通常不存在，回退到主 worktree。
PERF_DIR=${PERF_DIR:-$ROOT/.perf}
if [ ! -d "$PERF_DIR" ]; then
    MAIN_WORKTREE=$(git -C "$ROOT" worktree list --porcelain 2>/dev/null \
        | sed -n 's/^worktree //p' | sed -n '1p')
    if [ -n "$MAIN_WORKTREE" ] && [ -d "$MAIN_WORKTREE/.perf" ]; then
        PERF_DIR="$MAIN_WORKTREE/.perf"
    fi
fi
XVFB=${XVFB:-$PERF_DIR/xvfb/usr/bin/Xvfb}
OPENBOX=${OPENBOX:-$PERF_DIR/openbox/usr/bin/openbox}
OPENBOX_LIB=$PERF_DIR/openbox/usr/lib/x86_64-linux-gnu
OPENBOX_DATA=$PERF_DIR/openbox/usr/share
for path in "$XVFB" "$OPENBOX"; do
    if [ ! -x "$path" ]; then
        echo "ERROR: 缺少隔离测试依赖: $path" >&2
        exit 2
    fi
done

OUT=${SOAK_OUT:-$ROOT/.logs/maximize-state-machine}
rm -rf "$OUT"
mkdir -p "$OUT/openbox-home/.config/openbox"
cat >"$OUT/openbox-home/.config/openbox/rc.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<openbox_config xmlns="http://openbox.org/3.4/rc">
  <theme><name>Clearlooks</name><titleLayout>NLIMC</titleLayout></theme>
  <desktops><number>1</number></desktops>
  <resize><drawContents>yes</drawContents></resize>
  <keyboard></keyboard>
  <mouse></mouse>
  <applications>
    <application title="TervoStartMax"><maximized>yes</maximized></application>
  </applications>
</openbox_config>
XML

PIDS=()
OPENBOX_PID=
cleanup() {
    for pid in "${PIDS[@]:-}"; do
        # timeout 场景可能把 Openbox SIGSTOP；先 CONT 再 TERM，避免 wait 卡死。
        kill -CONT "$pid" 2>/dev/null || true
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT

start_xvfb() {
    local display=$1 screen=$2 log=$3
    if DISPLAY="$display" xwininfo -root >/dev/null 2>&1; then
        echo "ERROR: DISPLAY $display 已被占用；拒绝复用（隔离环境必须独占）" >&2
        exit 2
    fi
    "$XVFB" "$display" -screen 0 "$screen" -ac -nolisten tcp >"$log" 2>&1 &
    PIDS+=($!)
    for _ in $(seq 1 100); do
        if DISPLAY="$display" xwininfo -root >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.05
    done
    echo "ERROR: Xvfb $display 启动失败（$log）" >&2
    exit 2
}

start_openbox() {
    DISPLAY="$DISPLAY_NUM" HOME="$OUT/openbox-home" \
        XDG_DATA_DIRS="$OPENBOX_DATA:/usr/share" LD_LIBRARY_PATH="$OPENBOX_LIB" \
        "$OPENBOX" --sm-disable >"$OUT/openbox.log" 2>&1 &
    OPENBOX_PID=$!
    PIDS+=("$OPENBOX_PID")
    sleep 1
    if ! DISPLAY="$DISPLAY_NUM" xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id'; then
        echo "ERROR: Openbox 未声明 _NET_SUPPORTING_WM_CHECK（$OUT/openbox.log）" >&2
        exit 2
    fi
}

APP_PID=
start_app() {  # <slug> <title> <home> <display>
    local slug=$1 title=$2 home=$3 display=$4
    mkdir -p "$home/.config/tervo"
    DISPLAY="$display" HOME="$home" XDG_CONFIG_HOME="$home/.config" \
        TERVO_CONFIG="$home/config.toml" \
        "$BIN" --title "$title" --cols 70 --rows 30 \
        --shell /bin/bash -- -c 'sleep 300' >"$OUT/$slug.log" 2>&1 &
    APP_PID=$!
    PIDS+=("$APP_PID")
    echo "$APP_PID" >"$OUT/$slug.pid"
}

run_on() {  # <display> <helper args...>
    local display=$1
    shift
    DISPLAY="$display" python3 "$ROOT/scripts/e2e_maximize_state_machine.py" "$@"
}

echo "== maximize state machine e2e =="
echo "BIN=$BIN"
BIN_HASH=$(sha256sum "$BIN" | awk '{print $1}')
echo "sha256=$BIN_HASH"
echo "commit=$(git -C "$ROOT" rev-parse --short HEAD) branch=$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)"
echo "DISPLAY_NUM=$DISPLAY_NUM NO_WM_DISPLAY=$NO_WM_DISPLAY OUT=$OUT"

start_xvfb "$DISPLAY_NUM" "1280x800x24" "$OUT/xvfb-wm.log"
start_xvfb "$NO_WM_DISPLAY" "1280x800x24" "$OUT/xvfb-nowm.log"
start_openbox

# ---- A：单屏 / 伪双屏 / 快慢点击 ----
start_app app-a TervoA "$OUT/home-a" "$DISPLAY_NUM"
PID_A=$(cat "$OUT/app-a.pid")
run_on "$DISPLAY_NUM" single --pid "$PID_A" --title TervoA
run_on "$DISPLAY_NUM" fast --pid "$PID_A" --title TervoA --count 6 --interval 0.05 \
    --log "$OUT/app-a.log"
run_on "$DISPLAY_NUM" slow --pid "$PID_A" --title TervoA --count 6 --interval 1.2

# ---- 双实例 ----
start_app app-b TervoB "$OUT/home-b" "$DISPLAY_NUM"
PID_B=$(cat "$OUT/app-b.pid")
run_on "$DISPLAY_NUM" two --pid-a "$PID_A" --title-a TervoA \
    --pid-b "$PID_B" --title-b TervoB

# ---- 外部 WM 改变 _NET_WM_STATE 后点按钮 ----
run_on "$DISPLAY_NUM" external --pid "$PID_A" --title TervoA

# ---- PropertyNotify 丢失/延迟：SIGSTOP 冻结 WM，请求超时只报错不改 target ----
run_on "$DISPLAY_NUM" timeout --pid "$PID_A" --title TervoA \
    --openbox-pid "$OPENBOX_PID" --log "$OUT/app-a.log"

# B 不再需要；释放内存/避免窗口干扰。
kill "$PID_B" 2>/dev/null || true
wait "$PID_B" 2>/dev/null || true

# ---- 启动时已最大化（Openbox rc.xml 规则）----
start_app app-startmax TervoStartMax "$OUT/home-startmax" "$DISPLAY_NUM"
PID_C=$(cat "$OUT/app-startmax.pid")
run_on "$DISPLAY_NUM" startup-max --pid "$PID_C" --title TervoStartMax
kill "$PID_C" 2>/dev/null || true
wait "$PID_C" 2>/dev/null || true

# ---- 无 WM ----
start_app app-nowm TervoNoWm "$OUT/home-nowm" "$NO_WM_DISPLAY"
PID_D=$(cat "$OUT/app-nowm.pid")
run_on "$NO_WM_DISPLAY" no-wm --pid "$PID_D" --title TervoNoWm \
    --log "$OUT/app-nowm.log"
kill "$PID_D" 2>/dev/null || true
wait "$PID_D" 2>/dev/null || true

# ---- 证据汇总：失败错误不得静默 ----
CLICKS=$(grep -c '\[maximize\] click target=' "$OUT/app-a.log" || true)
STATES=$(grep -c '\[maximize\] state desired=' "$OUT/app-a.log" || true)
SENT=$(grep -c '\[maximize\] request target=' "$OUT/app-a.log" || true)
NOWM_FAIL=$(grep -c '\[maximize\] request failed' "$OUT/app-nowm.log" || true)
echo "evidence: clicks=$CLICKS state-transitions=$STATES requests-sent=$SENT no-wm-failures=$NOWM_FAIL"
[ "$CLICKS" -ge 20 ] || {
    echo "ERROR: controller click 日志过少（$CLICKS），点击可能没有到达 UI" >&2
    exit 1
}
[ "$NOWM_FAIL" -ge 1 ] || {
    echo "ERROR: 无 WM 场景没有 request failed 日志" >&2
    exit 1
}

kill "$PID_A" 2>/dev/null || true
wait "$PID_A" 2>/dev/null || true

AFTER_HASH=$(sha256sum "$BIN" | awk '{print $1}')
if [ "$AFTER_HASH" != "$BIN_HASH" ]; then
    echo "ERROR: 测试期间 BIN 被替换（$BIN_HASH -> $AFTER_HASH），结果不可信" >&2
    exit 1
fi
echo "artifact-stable sha256=$AFTER_HASH"
echo "PASS maximize state machine isolated e2e (release)"
echo "logs: $OUT"
