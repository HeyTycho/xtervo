#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Run one mw-spike mode on an isolated Xvfb display and capture evidence.
#
# Usage:
#   crates/mw-spike/run_xvfb.sh wgpu
#   crates/mw-spike/run_xvfb.sh software
#   crates/mw-spike/run_xvfb.sh mainwindow
#
# Environment:
#   MW_SPIKE_DISPLAY    X display (default :88; refuses :1 and occupied displays)
#   MW_SPIKE_BIN        prebuilt binary (default <repo>/target/debug/mw-spike)
#   MW_SPIKE_XVFB       Xvfb binary (default <main-root>/.perf/xvfb/usr/bin/Xvfb)
#   MW_SPIKE_XWIN       xwin helper (default <repo>/target/debug/xwin)
#   MW_SPIKE_HOLD_MS    observation window after PASS (default 12000)
#
# The script only kills the Xvfb/Openbox/app processes it started. It never
# reuses or kills the user's :1 display. Outputs:
#   docs/evidence/mw-spike/<mode>/
#     run.log                 full spike stdout/stderr
#     x11_windows.txt         xwininfo -root -tree while both windows exist
#     focus_<A|B>.txt         _NET_ACTIVE_WINDOW + mapped _NET_WM_NAME
#     desktop_both.png        root screenshot (Openbox decorations included)
#     desktop_focus_<A|B>.png root screenshot after activating each window
#     *.png                   per-window take_snapshot() images
set -uo pipefail

MODE=${1:-wgpu}
case "$MODE" in
    wgpu|software|mainwindow) ;;
    *) echo "usage: $0 wgpu|software|mainwindow" >&2; exit 2 ;;
esac

ROOT=$(git rev-parse --show-toplevel)
COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
TARGET=${CARGO_TARGET_DIR:-$MAIN_ROOT/target}
CARGO_HOME=${CARGO_HOME:-$MAIN_ROOT/.cargo}
export CARGO_HOME CARGO_TARGET_DIR="$TARGET" CARGO_NET_OFFLINE=true

XVFB=${MW_SPIKE_XVFB:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
[ -n "$XVFB" ] && [ -x "$XVFB" ] || {
    echo "Xvfb not found: set MW_SPIKE_XVFB or install xvfb" >&2
    exit 2
}
OPENBOX=${MW_SPIKE_OPENBOX:-$PERF/openbox/usr/bin/openbox}
OPENBOX_LIB=${MW_SPIKE_OPENBOX_LIB:-$PERF/openbox/usr/lib/x86_64-linux-gnu}
BIN=${MW_SPIKE_BIN:-$TARGET/debug/mw-spike}
XWIN=${MW_SPIKE_XWIN:-$TARGET/debug/xwin}
for tool in xdpyinfo xwininfo xprop xwd ffmpeg; do
    command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 2; }
done

DISPLAY_NUM=${MW_SPIKE_DISPLAY:-:88}
if [ "$DISPLAY_NUM" = ":1" ] || [ "$DISPLAY_NUM" = ":1.0" ]; then
    echo "refusing to use the user's :1 display" >&2
    exit 2
fi
case "$DISPLAY_NUM" in
    :[0-9]*) ;;
    *) echo "MW_SPIKE_DISPLAY must look like :88 (got '$DISPLAY_NUM')" >&2; exit 2 ;;
esac
if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM is already in use; refusing to touch it" >&2
    exit 2
fi

OUT="$ROOT/docs/evidence/mw-spike/$MODE"
mkdir -p "$OUT"
rm -f "$OUT"/desktop_*.png "$OUT"/snapshots_*.png
LOG="$OUT/run.log"
: >"$LOG"

if [ ! -x "$BIN" ]; then
    echo "building mw-spike (shared target: $TARGET)"
    ( cd "$ROOT" && cargo build -p mw-spike ) || exit 2
fi
if [ ! -x "$XWIN" ]; then
    echo "xwin helper missing; build once with:" >&2
    echo "  CARGO_HOME=$CARGO_HOME CARGO_TARGET_DIR=$TARGET cargo build -p xtervo-app --bin xwin" >&2
    exit 2
fi

XVFB_PID=""
OB_PID=""
APP_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

export DISPLAY="$DISPLAY_NUM"
export XMODIFIERS=@im=none
"$XVFB" "$DISPLAY_NUM" -screen 0 1600x1000x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
ready=0
for _ in $(seq 1 100); do
    if xdpyinfo >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.05
done
[ "$ready" = 1 ] || { echo "Xvfb failed to start; see $OUT/xvfb.log" >&2; exit 3; }

if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH="$OPENBOX_LIB" "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
fi

# winit-software for the component/baseline modes; WGPU30 for the GPU mode.
if [ "$MODE" = "wgpu" ]; then
    export SLINT_WGPU_CPU=1
else
    export SLINT_BACKEND=winit-software
fi

echo "mode=$MODE display=$DISPLAY_NUM out=$OUT"
"$BIN" "$MODE" --hold-ms "${MW_SPIKE_HOLD_MS:-12000}" --out "$OUT" \
    >>"$LOG" 2>&1 &
APP_PID=$!

TITLE_A="MW-Spike A"
TITLE_B="MW-Spike B"
if [ "$MODE" = "mainwindow" ]; then
    TITLE_A="MW-Spike MainWindow A"
    TITLE_B="MW-Spike MainWindow B"
fi

# Wait until both top-level windows exist.
both=0
for _ in $(seq 1 200); do
    tree=$(xwininfo -root -tree 2>/dev/null || true)
    if printf '%s\n' "$tree" | grep -q "\"$TITLE_A\"" &&
       printf '%s\n' "$tree" | grep -q "\"$TITLE_B\""; then
        both=1
        break
    fi
    sleep 0.05
done
if [ "$both" != 1 ]; then
    echo "both windows did not appear; see $LOG" >&2
    xwininfo -root -tree >"$OUT/x11_windows.txt" 2>&1 || true
    exit 4
fi

# Let the first frame (and, for wgpu, the first WGPU present) land.
sleep 1.5
xwininfo -root -tree >"$OUT/x11_windows.txt" 2>&1 || true

shot_root() {  # <name>
    # xwd -root on Xvfb captures only the root background (child windows are
    # separate drawables), so grab the composed screen through x11grab.
    ffmpeg -y -hide_banner -loglevel error -f x11grab \
        -video_size 1600x1000 -draw_mouse 0 -i "$DISPLAY" -frames:v 1 "$OUT/$1.png"
}
if [ "$MODE" != "wgpu" ]; then
    # WGPU/Vulkan WSI content is not visible to XGetImage on Xvfb (only the
    # root background is returned); the wgpu mode uses Slint's take_snapshot()
    # PNGs instead, tiled into snapshots_side_by_side.png after the run.
    shot_root desktop_both || true
fi

focus_evidence() {  # <title> <tag>
    local title=$1 tag=$2
    {
        echo "== before =="
        xprop -root _NET_ACTIVE_WINDOW 2>&1 || true
        echo "== xwin $title --focus =="
        "$XWIN" "$title" --focus 2>&1 || true
        sleep 0.5
        echo "== after =="
        xprop -root _NET_ACTIVE_WINDOW 2>&1 || true
        active=$(xprop -root _NET_ACTIVE_WINDOW 2>/dev/null | grep -o '0x[0-9a-fA-F]*' | tail -1)
        if [ -n "$active" ]; then
            xprop -id "$active" _NET_WM_NAME WM_NAME 2>&1 || true
        fi
        xwininfo -root -tree 2>/dev/null | grep -E "\"$TITLE_A\"|\"$TITLE_B\"" || true
    } >"$OUT/focus_$tag.txt" 2>&1
    if [ "$MODE" != "wgpu" ]; then
        shot_root "desktop_focus_$tag" || true
    fi
}

focus_evidence "$TITLE_A" A
focus_evidence "$TITLE_B" B

# While the app is still holding both windows, record the active-window state.
{
    echo "== final tree =="
    xwininfo -root -tree 2>&1 || true
    echo "== final active window =="
    xprop -root _NET_ACTIVE_WINDOW 2>&1 || true
} >"$OUT/x11_final.txt" 2>&1

wait "$APP_PID"
RC=$?
APP_PID=""

if [ "$MODE" = "wgpu" ] && [ -f "$OUT/A_initial.png" ] && [ -f "$OUT/B_initial.png" ]; then
    # Compose the two per-window take_snapshot() images so the evidence folder
    # still contains one side-by-side visual of the two live windows.
    ffmpeg -y -hide_banner -loglevel error -i "$OUT/A_initial.png" -i "$OUT/B_initial.png" \
        -filter_complex "[0]pad=520:400:0:30:color=0x101018[a];[1]pad=640:400:0:0:color=0x101018[b];[a][b]hstack=inputs=2" \
        "$OUT/snapshots_side_by_side.png" || true
    if [ -f "$OUT/A_after_resize.png" ] && [ -f "$OUT/B_after_resize.png" ]; then
        ffmpeg -y -hide_banner -loglevel error -i "$OUT/A_after_resize.png" -i "$OUT/B_after_resize.png" \
            -filter_complex "[0]pad=480:360:0:30:color=0x101018[a];[1]pad=600:360:0:0:color=0x101018[b];[a][b]hstack=inputs=2" \
            "$OUT/snapshots_after_resize.png" || true
    fi
fi

if [ "$RC" -eq 0 ] && grep -q "^PASS mw_spike mode=$MODE" "$LOG"; then
    echo "PASS: $MODE (evidence: $OUT)"
    exit 0
fi
echo "FAIL: $MODE rc=$RC; see $LOG" >&2
tail -40 "$LOG" >&2 || true
exit 1
