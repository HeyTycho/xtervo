#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# W-07 稳定性 soak：在独立 Xvfb 上循环施压并采样资源，最后输出 PASS/FAIL
# 与 CSV/Markdown 报告。只操作本脚本启动的 Xvfb/Openbox/xtervo，绝不碰 :1、
# Xephyr 或其它进程；不修改 core/UI 行为。
#
# 用法：
#   DURATION=2m  ./scripts/soak.sh                 # 快速门禁（脚本自身验证）
#   DURATION=30m ./scripts/soak.sh                 # 默认长跑
#   DURATION=15m BIN=$PWD/target/release/xtervo ./scripts/soak.sh
#
# 可配置（环境变量）：
#   DURATION             默认 30m；支持 30s / 2m / 15m / 1h 或纯秒数
#   BIN                  默认 $ROOT/target/release/xtervo
#   SOAK_DISPLAY         默认 :143（被占用则直接失败，不 kill 占用者）
#   SOAK_SCREEN          默认 1280x800x24
#   SOAK_OUT             默认 $ROOT/.logs/soak/<时间戳>
#   SOAK_SAMPLE_INTERVAL 默认 10s
#   SOAK_WARMUP          预热秒数（默认 duration/3，夹在 30..180）
#   SOAK_TABS            总 Tab 数，默认 22（>20）
#   SOAK_XVFB_BIN / SOAK_OPENBOX_BIN / SOAK_XWIN / SOAK_XCLICK / SOAK_XKEY
#   SOAK_KEY_DELAY       每次 XTest 注入后的等待，默认 0.10s
#   SOAK_NO_SHOTS=1      不做截图
#
# 退出码：0 = PASS；1 = FAIL（存活/panic/OOM/内存持续增长/场景未覆盖）；
#         2 = 环境或前置条件错误（不是产品失败）。
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2

DURATION=${DURATION:-30m}
DISPLAY_NUM=${SOAK_DISPLAY:-:143}
SCREEN=${SOAK_SCREEN:-1280x800x24}
SAMPLE_INTERVAL=${SOAK_SAMPLE_INTERVAL:-10}
TABS=${SOAK_TABS:-22}
KEY_DELAY=${SOAK_KEY_DELAY:-0.10}
NO_SHOTS=${SOAK_NO_SHOTS:-0}
BIN=${BIN:-$ROOT/target/release/xtervo}

OUT=${SOAK_OUT:-"$ROOT/.logs/soak/$(date +%Y%m%d-%H%M%S)"}
APP_LOG=$OUT/app.log
CSV=$OUT/samples.csv
XTERVO_LOG_DIR=$OUT/logs
XTERVO_LOG=
REPORT=$OUT/report.md
SUMMARY=$OUT/summary.json
START_ISO=$(date -Is)
COMMIT=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)
BRANCH=$(git -C "$ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)

XVFB_PID=
OP_PID=
APP_PID=
WIN=
SAMPLER_PID=
APP_START_EPOCH=0
APP_EXIT=
RESIZE_ATTEMPTS=0
RESIZE_OK=0
SENT_LINES=0
SHELL_LAG=0
TABS_OBSERVED=0
SPLITS_OBSERVED=0
INTERRUPTED=0
XVFB_ALIVE_AT_END=0
FATAL_MARKERS=0
LOG_ERRORS=0
LAST_ELAPSED=0

log() { printf '[soak %s] %s\n' "$(date +%H:%M:%S)" "$*"; }
warn() { printf '[soak %s] WARN: %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
die() {
    printf '[soak] 环境错误: %s\n' "$*" >&2
    exit 2
}
on_signal() {
    INTERRUPTED=1
    log "收到中断信号，正在清理（只 kill 本脚本 PID）"
    exit 130
}
trap on_signal INT TERM

duration_to_seconds() {
    local raw=$1 n unit
    case "$raw" in
        *h) n=${raw%h}; unit=3600 ;;
        *m) n=${raw%m}; unit=60 ;;
        *s) n=${raw%s}; unit=1 ;;
        *) n=$raw; unit=1 ;;
    esac
    case "$n" in
        ''|*[!0-9]*) return 1 ;;
    esac
    printf '%s\n' $((n * unit))
}

DURATION_S=$(duration_to_seconds "$DURATION") || die "DURATION 无法解析: $DURATION（例：2m/30m/90s）"
[ "$DURATION_S" -ge 10 ] || die "DURATION 至少 10s"
WARMUP_S=${SOAK_WARMUP:-$((DURATION_S / 3))}
[ "$WARMUP_S" -lt 30 ] && WARMUP_S=30
[ "$WARMUP_S" -gt 180 ] && WARMUP_S=180
[ "$WARMUP_S" -ge "$DURATION_S" ] && WARMUP_S=$((DURATION_S / 2))

find_perf() {
    local common main
    common=$(git -C "$ROOT" rev-parse --git-common-dir 2>/dev/null) || return 0
    main=$(cd "$(dirname "$common")" && pwd)
    printf '%s\n' "$main/.perf"
}
PERF=${XTERVO_PERF_DIR:-$(find_perf)}

find_xvfb() {
    [ -n "${SOAK_XVFB_BIN:-}" ] && [ -x "$SOAK_XVFB_BIN" ] && { printf '%s\n' "$SOAK_XVFB_BIN"; return; }
    command -v Xvfb >/dev/null 2>&1 && { command -v Xvfb; return; }
    [ -x "$PERF/xvfb/usr/bin/Xvfb" ] && { printf '%s\n' "$PERF/xvfb/usr/bin/Xvfb"; return; }
    return 1
}
find_openbox() {
    [ -n "${SOAK_OPENBOX_BIN:-}" ] && [ -x "$SOAK_OPENBOX_BIN" ] && { printf '%s\n' "$SOAK_OPENBOX_BIN"; return; }
    command -v openbox >/dev/null 2>&1 && { command -v openbox; return; }
    [ -x "$PERF/openbox/usr/bin/openbox" ] && { printf '%s\n' "$PERF/openbox/usr/bin/openbox"; return; }
    return 1
}
find_helper() {  # <name> <explicit var value>
    local name=$1 explicit=${2:-} cand
    if [ -n "$explicit" ] && [ -x "$explicit" ]; then printf '%s\n' "$explicit"; return; fi
    for cand in "$(dirname "$BIN")/$name" "$ROOT/target/release/$name" "$ROOT/target/debug/$name"; do
        [ -x "$cand" ] && { printf '%s\n' "$cand"; return; }
    done
    return 1
}

XVFB_BIN=$(find_xvfb) || die "找不到 Xvfb（SOAK_XVFB_BIN 可指定）"
OPENBOX_BIN=$(find_openbox || true)
XWIN=$(find_helper xwin "${SOAK_XWIN:-}") || die "找不到 xwin（先构建 --bin xwin）"
XCLICK=$(find_helper xclick "${SOAK_XCLICK:-}") || die "找不到 xclick（先构建 --bin xclick）"
XKEY=$(find_helper xkey "${SOAK_XKEY:-}") || die "找不到 xkey（先构建 --bin xkey）"

[ -x "$BIN" ] || die "缺少 BIN=$BIN；先 cargo build --release -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey"
command -v xdpyinfo >/dev/null 2>&1 || die "缺少 xdpyinfo（x11-utils）"
command -v xwininfo >/dev/null 2>&1 || die "缺少 xwininfo（x11-utils）"
command -v xprop >/dev/null 2>&1 || die "缺少 xprop（x11-utils）"
command -v python3 >/dev/null 2>&1 || die "缺少 python3（生成负载/分析 CSV）"
command -v awk >/dev/null 2>&1 || die "缺少 awk"
command -v sha256sum >/dev/null 2>&1 || die "缺少 sha256sum"

BIN=$(readlink -f "$BIN")
BIN_SHA=$(sha256sum "$BIN" | awk '{print $1}')
BIN_SIZE=$(stat -c %s "$BIN" 2>/dev/null || echo 0)
CLK_TCK=$(getconf CLK_TCK 2>/dev/null || echo 100)

mkdir -p "$OUT" "$OUT/shots" "$OUT/home" "$XTERVO_LOG_DIR"
STOP_FILE=$OUT/.stop-sampler
PHASE_FILE=$OUT/phase
APP_DEAD_FILE=$OUT/APP-DEAD
: > "$PHASE_FILE"

# 只对本脚本记录且 cmdline 匹配的 PID 发信号，避免 PID 复用误伤。
kill_own() {  # <pid> [SIG]
    local pid=${1:-} sig=${2:-TERM} cmd
    [ -n "$pid" ] || return 0
    [ -r "/proc/$pid/cmdline" ] || return 0
    cmd=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)
    case "$cmd" in
        *xtervo*|*Xvfb*|*openbox*|*soak.sh*) kill "-$sig" "$pid" 2>/dev/null || true ;;
    esac
}

cleanup() {
    trap - EXIT
    touch "$STOP_FILE" 2>/dev/null || true
    if [ -n "$SAMPLER_PID" ]; then
        kill "$SAMPLER_PID" 2>/dev/null || true
        wait "$SAMPLER_PID" 2>/dev/null || true
    fi
    if [ -n "$APP_PID" ]; then
        kill_own "$APP_PID" TERM
        for _ in $(seq 1 30); do
            kill -0 "$APP_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill_own "$APP_PID" KILL
        wait "$APP_PID" 2>/dev/null || true
    fi
    if [ -n "$OP_PID" ]; then
        kill_own "$OP_PID" TERM
        wait "$OP_PID" 2>/dev/null || true
    fi
    if [ -n "$XVFB_PID" ]; then
        kill_own "$XVFB_PID" TERM
        for _ in $(seq 1 20); do
            kill -0 "$XVFB_PID" 2>/dev/null || break
            sleep 0.05
        done
        kill_own "$XVFB_PID" KILL
        wait "$XVFB_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    die "$DISPLAY_NUM 已被占用；本脚本绝不 kill 占用者，请换 SOAK_DISPLAY"
fi

log "worktree=$ROOT"
log "branch=$BRANCH commit=$COMMIT"
log "BIN=$BIN"
log "BIN_SHA256=$BIN_SHA size=$BIN_SIZE"
log "duration=${DURATION_S}s warmup=${WARMUP_S}s sample=${SAMPLE_INTERVAL}s display=$DISPLAY_NUM screen=$SCREEN tabs=$TABS"
log "输出目录: $OUT"

# ---------------------------------------------------------------------------
# 负载文件与受控 shell（所有动作都走真实 PTY，不直接调用 core API）
# ---------------------------------------------------------------------------
cat > "$OUT/shell.sh" <<'SHELL'
#!/bin/sh
# soak 受控 shell：从 PTY stdin 读命令，输出真正的终端字节流。
printf 'SOAK-READY\n' > "$SOAK_DIR/shell-ready"
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$SOAK_DIR/shell-input.log"
    case "$line" in
        flood*)
            n=${line#flood}
            n=${n# }
            case "$n" in ''|*[!0-9]*) n=1 ;; esac
            i=0
            while [ "$i" -lt "$n" ]; do
                cat "$SOAK_DIR/flood.txt"
                i=$((i + 1))
            done
            ;;
        seqburst*)
            seq 1 200000
            ;;
        scroll*)
            awk 'BEGIN { for (i = 1; i <= 240; i++) printf "scroll line %06d | abcdefghijklmnopqrstuvwxyz 0123456789\n", i }'
            ;;
        sixel*)
            cat "$SOAK_DIR/sixel-a.six" "$SOAK_DIR/sixel-b.six"
            ;;
        iip*)
            cat "$SOAK_DIR/iip-a.bin" "$SOAK_DIR/iip-b.bin"
            ;;
        emoji*)
            cat "$SOAK_DIR/emoji.txt"
            ;;
        ansi*)
            cat "$SOAK_DIR/ansi.txt"
            ;;
        shell-exit)
            break
            ;;
    esac
    printf 'SOAK-DONE %s\n' "$line" >> "$SOAK_DIR/shell-done.log"
done
SHELL
chmod +x "$OUT/shell.sh"

python3 - "$OUT" "$ROOT/assets/icon.png" <<'PY'
import base64
import os
import struct
import sys
import zlib

out = sys.argv[1]
icon = sys.argv[2] if len(sys.argv) > 2 else ""


def png_rgba(width, height, pixel):
    raw = b"".join(b"\x00" + bytes(pixel) * width for _ in range(height))

    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def write_iip(path, png, args):
    payload = base64.b64encode(png)
    head = f"\x1b]1337;File={args};size={len(png)};name={base64.b64encode(b'image.png').decode()}:".encode()
    with open(path, "wb") as f:
        f.write(head + payload + b"\x07")


# 反复输出的大文本（约 1.9 MB）：含 256 色/truecolor SGR，解析与渲染都吃满。
with open(os.path.join(out, "flood.txt"), "w", encoding="utf-8") as f:
    for i in range(20000):
        f.write(
            "\x1b[38;5;%dmflood %06d | abcdefghijklmnopqrstuvwxyz 0123456789 "
            "\x1b[38;2;%d;%d;%dmTOKEN-%06d\x1b[0m\n"
            % (i % 256, i, i % 255, (i * 7) % 255, (i * 13) % 255, i)
        )

with open(os.path.join(out, "ansi.txt"), "w", encoding="utf-8") as f:
    for i in range(12000):
        f.write(
            "\x1b[%d;%dH\x1b[4%dmansi %05d \x1b[0m"
            % (i % 30 + 1, i % 100 + 1, i % 8, i)
        )
    f.write("\x1b[0m\n")

with open(os.path.join(out, "emoji.txt"), "w", encoding="utf-8") as f:
    line = (
        "emoji 👨‍👩‍👧‍👦 🇨🇳 👍🏽 🧑‍💻 ❤️ ✨ 漢字 "
        "Z̸̧̛̪̎a̶̢̛l̷̢̛g̸̡̛ơ̶̧ 1️⃣ 🏳️‍🌈 👩🏾‍🚀\n"
    )
    for i in range(120):
        f.write("%03d %s" % (i, line))

# 合法 Sixel：A 为 120x72 红/蓝条纹，B 为 64x48 四色条带。
sixel_a = bytearray(b'\x1bPq"1;1;120;72#0;2;100;0;0#1;2;0;0;100')
for band in range(12):
    sixel_a += (b"#0" if band % 2 == 0 else b"#1") + b"!120~$-"
sixel_a += b"\x1b\\"
with open(os.path.join(out, "sixel-a.six"), "wb") as f:
    f.write(sixel_a)

sixel_b = bytearray(b'\x1bPq"1;1;64;48#0;2;100;0;0#1;2;0;100;0#2;2;0;0;100#3;2;100;100;0')
for band in range(8):
    sixel_b += ("#%d" % (band % 4)).encode() + b"!64~$-"
sixel_b += b"\x1b\\"
with open(os.path.join(out, "sixel-b.six"), "wb") as f:
    f.write(sixel_b)

if icon and os.path.isfile(icon):
    icon_png = open(icon, "rb").read()
else:
    icon_png = png_rgba(64, 64, (255, 80, 20, 255))
write_iip(os.path.join(out, "iip-a.bin"), icon_png, "inline=1")
write_iip(
    os.path.join(out, "iip-b.bin"),
    png_rgba(96, 64, (20, 160, 255, 255)),
    "inline=1;width=16;height=10",
)
PY

# ---------------------------------------------------------------------------
# 启动 Xvfb / Openbox / xtervo
# ---------------------------------------------------------------------------
cat > "$OUT/config.toml" <<CFG
version = 1
theme = "tokyo-night"
font_size = 13
scrollback = 2000
sixel = true
sixel_scrolling = true
iip = true
desktop_notifications = false
confirm_close = false
confirm_multiline_paste = false
restore_session_on_startup = false
CFG

export DISPLAY="$DISPLAY_NUM"
export HOME="$OUT/home"
export XDG_CACHE_HOME="$OUT/home/.cache"
export XDG_STATE_HOME="$OUT/home/.local/state"
export XMODIFIERS=@im=none
# XTERVO_GPU=1 时必须让 app 自己 require_wgpu_30；显式 SLINT_BACKEND 会让它
# 走 Skia/OpenGL 而不是 WGPU，soak 就测错了路径。
if [ "${XTERVO_GPU:-0}" = "1" ]; then
    unset SLINT_BACKEND
else
    export SLINT_BACKEND=${SLINT_BACKEND:-winit-software}
fi
export XTERVO_CONFIG="$OUT/config.toml"
export XTERVO_LOG_DIR
export TERM=${TERM:-xterm-256color}
export SOAK_DIR="$OUT"
mkdir -p "$HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME"

log "启动 Xvfb $DISPLAY_NUM ($SCREEN)"
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 200); do
    DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 || die "Xvfb $DISPLAY_NUM 启动失败（见 $OUT/xvfb.log）"

if [ -n "$OPENBOX_BIN" ]; then
    OB_LIB=${SOAK_OPENBOX_LIB:-$PERF/openbox/usr/lib/x86_64-linux-gnu}
    OB_DATA=${SOAK_OPENBOX_DATA:-$PERF/openbox/usr/share}
    OB_ENV=("HOME=$HOME" "XDG_CACHE_HOME=$XDG_CACHE_HOME")
    [ -d "$OB_LIB" ] && OB_ENV+=("LD_LIBRARY_PATH=$OB_LIB")
    [ -d "$OB_DATA" ] && OB_ENV+=("XDG_DATA_DIRS=$OB_DATA")
    env DISPLAY="$DISPLAY_NUM" "${OB_ENV[@]}" "$OPENBOX_BIN" --sm-disable >"$OUT/openbox.log" 2>&1 &
    OP_PID=$!
    for _ in $(seq 1 200); do
        DISPLAY="$DISPLAY_NUM" xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id' && break
        sleep 0.05
    done
    if DISPLAY="$DISPLAY_NUM" xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id'; then
        log "Openbox pid=$OP_PID"
    else
        warn "Openbox 未接管 $DISPLAY_NUM；resize 可能不生效（见 $OUT/openbox.log）"
    fi
else
    warn "找不到 Openbox；继续运行，但 resize 场景可能失败"
fi

log "启动 xtervo（title=xtervo-Soak，--shell /bin/sh -- shell.sh）"
APP_START_EPOCH=$(date +%s)
env DISPLAY="$DISPLAY_NUM" HOME="$HOME" XMODIFIERS=@im=none \
    XTERVO_LOG_DIR="$XTERVO_LOG_DIR" XTERVO_CONFIG="$XTERVO_CONFIG" XTERVO_LOG=1 \
    SOAK_DIR="$OUT" "$BIN" --cols 100 --rows 30 --title xtervo-Soak \
    --shell /bin/sh -- "$OUT/shell.sh" >"$APP_LOG" 2>&1 &
APP_PID=$!

for _ in $(seq 1 300); do
    for id in $(DISPLAY="$DISPLAY_NUM" xwininfo -root -tree 2>/dev/null | awk '/"xtervo-Soak"/ {print $1}'); do
        pid=$(DISPLAY="$DISPLAY_NUM" xprop -id "$id" _NET_WM_PID 2>/dev/null | awk '{print $NF}')
        if [ "$pid" = "$APP_PID" ]; then WIN=$id; break; fi
    done
    [ -n "$WIN" ] && break
    kill -0 "$APP_PID" 2>/dev/null || break
    sleep 0.1
done
if [ -z "$WIN" ]; then
    tail -30 "$APP_LOG" >&2 || true
    die "30s 内没找到 pid=$APP_PID 的 xtervo-Soak 窗口（见 $APP_LOG）"
fi
log "xtervo pid=$APP_PID win=$WIN"

for _ in $(seq 1 100); do
    [ -f "$OUT/shell-ready" ] && break
    sleep 0.1
done
[ -f "$OUT/shell-ready" ] || warn "受控 shell 未写出 shell-ready；继续但输入可能不生效"

# 固定到左上角并设置初始大小；之后所有坐标都按窗口实际位置计算。
"$XWIN" "$WIN" 0 0 1100 700 >/dev/null 2>&1 || true
sleep 0.6
read -r WIN_X WIN_Y WIN_W WIN_H <<<"$(xwininfo -id "$WIN" 2>/dev/null | awk '
    /Absolute upper-left X:/ {x=$NF}
    /Absolute upper-left Y:/ {y=$NF}
    /Width:/ {w=$2}
    /Height:/ {h=$2}
    END {print x, y, w, h}')"
log "窗口初始位置 ${WIN_X:-?},${WIN_Y:-?} 尺寸 ${WIN_W:-?}x${WIN_H:-?}"

XTERVO_LOG=$(ls -t "$XTERVO_LOG_DIR"/xtervo-*.log 2>/dev/null | head -1 || true)
if [ -z "$XTERVO_LOG" ]; then
    XTERVO_LOG=$APP_LOG
    warn "没有按次日志文件，回落到 app.log"
fi
log "xtervo log: $XTERVO_LOG"

geom() {
    DISPLAY="$DISPLAY_NUM" xwininfo -id "$WIN" 2>/dev/null | awk '
        /Absolute upper-left X:/ {x=$NF}
        /Absolute upper-left Y:/ {y=$NF}
        /Width:/ {w=$2}
        /Height:/ {h=$2}
        END {print x, y, w, h}'
}

set_phase() { printf '%s\n' "$1" > "$PHASE_FILE"; }

shot() {
    [ "$NO_SHOTS" = 1 ] && return 0
    command -v ffmpeg >/dev/null 2>&1 || return 0
    local size=${SCREEN%x*}
    ffmpeg -loglevel error -y -f x11grab -video_size "$size" -i "$DISPLAY_NUM" \
        -frames:v 1 "$OUT/shots/$1.png" >/dev/null 2>&1 || true
}

max_tabs_log() {
    grep -ao 'tabs=[0-9]*' "$XTERVO_LOG" 2>/dev/null | sed 's/tabs=//' | sort -n | tail -1
}

# ---------------------------------------------------------------------------
# 采样器：RSS/PSS/CPU/线程/FD/子进程/存活/日志错误
# ---------------------------------------------------------------------------
sampler() {
    local prev_ticks= prev_ns= log_off=0 fatal=0 errs=0
    printf 'elapsed_s,ts,rss_kb,pss_kb,cpu_pct,threads,fds,children,app_alive,xvfb_alive,win_alive,log_fatal,log_errors,mem_available_kb,phase\n' > "$CSV"
    while [ ! -f "$STOP_FILE" ]; do
        local now_epoch now_ns elapsed alive=0 xalive=0 walive=0
        local rss=0 pss=0 threads=0 fds=0 children=0 cpu=0.0 ticks= mem_avail=0
        local size=0 chunk= n=0
        now_epoch=$(date +%s)
        now_ns=$(date +%s%N)
        elapsed=$((now_epoch - APP_START_EPOCH))
        LAST_ELAPSED=$elapsed
        if kill -0 "$APP_PID" 2>/dev/null; then
            alive=1
            if [ -r "/proc/$APP_PID/smaps_rollup" ]; then
                read -r rss pss <<<"$(awk '/^Rss:/{r=$2} /^Pss:/{p=$2} END{print r+0, p+0}' "/proc/$APP_PID/smaps_rollup" 2>/dev/null)"
            elif [ -r "/proc/$APP_PID/status" ]; then
                rss=$(awk '/^VmRSS:/{print $2}' "/proc/$APP_PID/status" 2>/dev/null)
                pss=$rss
            fi
            threads=$(awk '/^Threads:/{print $2}' "/proc/$APP_PID/status" 2>/dev/null)
            fds=$(ls "/proc/$APP_PID/fd" 2>/dev/null | wc -l)
            children=$(ps --ppid "$APP_PID" -o pid= 2>/dev/null | wc -l)
            ticks=$(awk '{print $14 + $15}' "/proc/$APP_PID/stat" 2>/dev/null)
            if [ -n "$prev_ticks" ] && [ -n "$ticks" ] && [ "$prev_ticks" != "$ticks" ]; then
                cpu=$(awk -v d=$((ticks - prev_ticks)) -v ns=$((now_ns - prev_ns)) -v hz="$CLK_TCK" \
                    'BEGIN { if (ns > 0) printf "%.1f", (d / hz) / (ns / 1000000000.0) * 100; else print "0.0" }')
            fi
            prev_ticks=$ticks
            prev_ns=$now_ns
        else
            touch "$APP_DEAD_FILE" 2>/dev/null || true
        fi
        if kill -0 "$XVFB_PID" 2>/dev/null; then xalive=1; fi
        if [ -n "$WIN" ] && DISPLAY="$DISPLAY_NUM" xwininfo -id "$WIN" >/dev/null 2>&1; then walive=1; fi

        size=$(stat -c %s "$APP_LOG" 2>/dev/null || echo 0)
        if [ "$size" -gt "$log_off" ]; then
            chunk=$(tail -c +$((log_off + 1)) "$APP_LOG" 2>/dev/null || true)
            if [ -n "$chunk" ]; then
                n=$(printf '%s\n' "$chunk" | grep -aciE '\[panic\]|panicked|out of memory|memory allocation.*fail|fatal|segmentation|SIGSEGV' || true)
                fatal=$((fatal + n))
                n=$(printf '%s\n' "$chunk" | grep -aciE '\berror\b|\bfailed\b' || true)
                errs=$((errs + n))
            fi
            log_off=$size
        fi
        mem_avail=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo 2>/dev/null)
        printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
            "$elapsed" "$(date +%H:%M:%S)" "$rss" "$pss" "$cpu" "$threads" "$fds" "$children" \
            "$alive" "$xalive" "$walive" "$fatal" "$errs" "${mem_avail:-0}" "$(cat "$PHASE_FILE" 2>/dev/null)" >> "$CSV"
        printf '[soak] %4ds rss=%6.1fMB pss=%6.1fMB cpu=%5s%% thr=%4s fd=%3s child=%2s fatal=%s err=%s alive=%s x=%s phase=%s\n' \
            "$elapsed" "$(awk -v v="$rss" 'BEGIN{printf "%.1f", v/1024}')" \
            "$(awk -v v="$pss" 'BEGIN{printf "%.1f", v/1024}')" \
            "$cpu" "$threads" "$fds" "$children" "$fatal" "$errs" "$alive" "$xalive" "$(cat "$PHASE_FILE" 2>/dev/null)"
        sleep "$SAMPLE_INTERVAL"
    done
}

touch "$STOP_FILE"  # 采样器启动前先清掉上一次的停止标记
rm -f "$STOP_FILE"
sampler &
SAMPLER_PID=$!

# ---------------------------------------------------------------------------
# 交互动作
# ---------------------------------------------------------------------------
press() {
    "$XKEY" "$@" >/dev/null 2>&1 || true
    sleep "$KEY_DELAY"
}
type_text() {
    local text=$1
    "$XKEY" "$text" >/dev/null 2>&1 || true
    "$XKEY" --key enter >/dev/null 2>&1 || true
}
done_count() {
    if [ -f "$OUT/shell-done.log" ]; then wc -l < "$OUT/shell-done.log"; else echo 0; fi
}
wait_done() {
    local target=$SENT_LINES i d
    for i in $(seq 1 60); do
        d=$(done_count)
        [ "$d" -ge "$target" ] && return 0
        sleep 0.1
    done
    SHELL_LAG=$((SHELL_LAG + 1))
    warn "shell 未在 6s 内确认第 $target 条命令（done=$d）；继续"
    return 0
}
send_cmd() {
    type_text "$1"
    SENT_LINES=$((SENT_LINES + 1))
    wait_done
}

click_at() { "$XCLICK" "$1" "$2" "${3:-1}" >/dev/null 2>&1 || true; sleep 0.08; }

wheel() {  # <button> <count>
    local button=$1 count=$2 x y w h i px py
    read -r x y w h <<<"$(geom)"
    px=$((x + w / 2))
    py=$((y + h / 2))
    for i in $(seq 1 "$count"); do
        "$XCLICK" "$px" "$py" "$button" >/dev/null 2>&1 || true
    done
}
scrollbar_drag() {
    local x y w h
    read -r x y w h <<<"$(geom)"
    "$XCLICK" $((x + w - 6)) $((y + 100)) drag $((x + w - 6)) $((y + h / 2)) >/dev/null 2>&1 || true
    sleep 0.2
}

copy_once() {
    local x y w h
    read -r x y w h <<<"$(geom)"
    "$XCLICK" $((x + 24)) $((y + 120)) drag $((x + w / 2)) $((y + h - 100)) >/dev/null 2>&1 || true
    sleep 0.2
    press c --ctrl --shift
    sleep 0.3
}

find_once() {  # 每次打开/关闭搜索；只在首个 cycle 输入，避免查询串累积
    press f --ctrl --shift
    sleep 0.2
    if [ "${1:-0}" -eq 0 ]; then
        "$XKEY" scroll >/dev/null 2>&1 || true
        sleep 0.4
    fi
    press --key escape
    sleep 0.2
}

settings_open_count() { grep -ac 'run command settings.open' "$XTERVO_LOG" 2>/dev/null || true; }
settings_cat_count() { grep -ac 'settings category=' "$XTERVO_LOG" 2>/dev/null || true; }

open_settings() {
    local before attempt
    before=$(settings_open_count)
    for attempt in 1 2; do
        press p --ctrl --shift
        sleep 0.5
        "$XKEY" settings >/dev/null 2>&1 || true
        sleep 0.5
        press --key enter
        for _ in $(seq 1 25); do
            [ "$(settings_open_count)" -gt "$before" ] && return 0
            sleep 0.1
        done
        warn "Settings 打开未确认（attempt=$attempt）；重试"
        press --key escape
        sleep 0.3
    done
    return 1
}

settings_test() {
    local cy
    resize_window 1100 700
    open_settings || { warn "跳过 Settings 切换"; return 0; }
    for cy in 139 175 211 285 321 357 431 467; do
        read -r WIN_X WIN_Y WIN_W WIN_H <<<"$(geom)"
        click_at $((WIN_X + 100)) $((WIN_Y + cy))
        sleep 0.18
    done
    shot settings
    press --key escape
    sleep 0.4
}

resize_window() {  # <w> <h>
    local w=$1 h=$2 x y cw ch
    RESIZE_ATTEMPTS=$((RESIZE_ATTEMPTS + 1))
    "$XWIN" "$WIN" 0 0 "$w" "$h" >/dev/null 2>&1 || true
    sleep 0.5
    read -r x y cw ch <<<"$(geom)"
    if [ "$cw" = "$w" ] && [ "$ch" = "$h" ]; then
        RESIZE_OK=$((RESIZE_OK + 1))
        WIN_X=$x; WIN_Y=$y; WIN_W=$cw; WIN_H=$ch
    else
        warn "resize ${w}x${h} 未生效（实际 ${cw:-?}x${ch:-?}）"
    fi
}

# ---------------------------------------------------------------------------
# 场景：先建 22 Tab + 4 pane，再循环洪峰/滚动/图片/emoji/复制/搜索/resize
# ---------------------------------------------------------------------------
set_phase setup
"$XWIN" "$WIN" 0 0 >/dev/null 2>&1 || true
sleep 0.3

log "创建 $TABS 个 Tab（含初始 1 个）..."
for _ in $(seq 2 "$TABS"); do
    press t --ctrl --shift
    sleep 0.06
done
sleep 2

log "在活动 Tab 内分屏 3 次（2 右 + 1 下）..."
press e --ctrl --shift
press e --ctrl --shift
press d --ctrl --shift
sleep 1.5

TABS_OBSERVED=$(max_tabs_log)
[ -n "${TABS_OBSERVED:-}" ] || TABS_OBSERVED=0
SPLITS_OBSERVED=$(grep -ac '\[ws\] split pane=' "$XTERVO_LOG" 2>/dev/null || true)
[ -n "$SPLITS_OBSERVED" ] || SPLITS_OBSERVED=0
log "日志观测: tabs=$TABS_OBSERVED splits=$SPLITS_OBSERVED"
shot start

[ "$TABS_OBSERVED" -ge "$TABS" ] || warn "Tab 数未达预期 $TABS（实际 $TABS_OBSERVED）"
[ "$SPLITS_OBSERVED" -ge 3 ] || warn "分屏数未达预期 3（实际 $SPLITS_OBSERVED）"

CYCLE=0
while :; do
    now=$(date +%s)
    elapsed=$((now - APP_START_EPOCH))
    LAST_ELAPSED=$elapsed
    [ "$elapsed" -ge "$DURATION_S" ] && break
    if [ -f "$APP_DEAD_FILE" ] || ! kill -0 "$APP_PID" 2>/dev/null; then
        warn "xtervo 在 ${elapsed}s 退出/死亡，停止场景循环"
        break
    fi
    if [ -f "$STOP_FILE" ]; then break; fi

    log "cycle=$CYCLE elapsed=${elapsed}s"
    set_phase flood
    send_cmd "flood 2"
    sleep 0.5
    set_phase seq
    send_cmd "seqburst"
    set_phase images
    send_cmd "sixel"
    send_cmd "iip"
    set_phase emoji
    send_cmd "emoji"
    set_phase scroll
    wheel 5 12
    wheel 4 8
    if [ $((CYCLE % 2)) -eq 0 ]; then scrollbar_drag; fi
    set_phase copy-search
    copy_once
    find_once "$CYCLE"
    set_phase tabs
    press --key tab --ctrl
    sleep 0.15
    send_cmd "flood 1"
    press --key tab --ctrl
    sleep 0.15
    send_cmd "emoji"
    set_phase panes
    press --key right --ctrl --shift
    send_cmd "ansi"
    press --key right --ctrl --shift
    set_phase resize
    resize_window 900 620
    resize_window 1180 740
    if [ $((CYCLE % 2)) -eq 0 ]; then
        set_phase settings
        settings_test
    fi
    CYCLE=$((CYCLE + 1))
    sleep 0.4
done

set_phase finish
sleep 1
shot end
LAST_ELAPSED=$(( $(date +%s) - APP_START_EPOCH ))
touch "$STOP_FILE"
if [ -n "$SAMPLER_PID" ]; then
    wait "$SAMPLER_PID" 2>/dev/null || true
    SAMPLER_PID=
fi
sleep 0.2

# 取最后一次采样的告警计数；CSV 采样器已停，下面只读。
LAST_CSV=$(tail -1 "$CSV" 2>/dev/null || true)
if [ -n "$LAST_CSV" ]; then
    FATAL_MARKERS=$(printf '%s\n' "$LAST_CSV" | awk -F, '{print $12}')
    LOG_ERRORS=$(printf '%s\n' "$LAST_CSV" | awk -F, '{print $13}')
fi

XVFB_ALIVE_AT_END=0
kill -0 "$XVFB_PID" 2>/dev/null && XVFB_ALIVE_AT_END=1
APP_ALIVE_AT_END=0
kill -0 "$APP_PID" 2>/dev/null && APP_ALIVE_AT_END=1

log "停止 xtervo（SIGTERM，只 kill pid=$APP_PID）"
kill_own "$APP_PID" TERM
for _ in $(seq 1 50); do
    kill -0 "$APP_PID" 2>/dev/null || break
    sleep 0.1
done
kill_own "$APP_PID" KILL
wait "$APP_PID" 2>/dev/null
APP_EXIT=$?

XVFB_ERRORS=$(grep -aciE '\(EE\)|fatal|error' "$OUT/xvfb.log" 2>/dev/null || true)
[ -n "$XVFB_ERRORS" ] || XVFB_ERRORS=0

# 最终动作计数（日志完整后解析）。
TABS_OBSERVED=$(max_tabs_log)
[ -n "${TABS_OBSERVED:-}" ] || TABS_OBSERVED=0
SPLITS_OBSERVED=$(grep -ac '\[ws\] split pane=' "$XTERVO_LOG" 2>/dev/null || true)
[ -n "$SPLITS_OBSERVED" ] || SPLITS_OBSERVED=0
SETTINGS_OPENS=$(grep -ac 'run command settings.open' "$XTERVO_LOG" 2>/dev/null || true)
SETTINGS_SWITCHES=$(grep -ac 'settings category=' "$XTERVO_LOG" 2>/dev/null || true)
FIND_OPENS=$(grep -ac '\[ui\] find open' "$XTERVO_LOG" 2>/dev/null || true)
COPIES=$(grep -ac '\[clipboard\] 复制' "$XTERVO_LOG" 2>/dev/null || true)

export SOAK_OUT="$OUT" SOAK_ROOT="$ROOT" SOAK_DURATION_S="$DURATION_S" SOAK_WARMUP_S="$WARMUP_S" \
    SOAK_COMMIT="$COMMIT" SOAK_BRANCH="$BRANCH" SOAK_BIN="$BIN" SOAK_BIN_SHA="$BIN_SHA" \
    SOAK_BIN_SIZE="$BIN_SIZE" SOAK_DISPLAY="$DISPLAY_NUM" SOAK_SCREEN="$SCREEN" \
    SOAK_START_ISO="$START_ISO" SOAK_XVFB_ALIVE="$XVFB_ALIVE_AT_END" SOAK_APP_ALIVE="$APP_ALIVE_AT_END" \
    SOAK_APP_EXIT="$APP_EXIT" SOAK_RESIZE_ATTEMPTS="$RESIZE_ATTEMPTS" SOAK_RESIZE_OK="$RESIZE_OK" \
    SOAK_SENT="$SENT_LINES" SOAK_SHELL_LAG="$SHELL_LAG" SOAK_TABS="$TABS_OBSERVED" \
    SOAK_SPLITS="$SPLITS_OBSERVED" SOAK_SETTINGS_OPENS="$SETTINGS_OPENS" \
    SOAK_SETTINGS_SWITCHES="$SETTINGS_SWITCHES" SOAK_FIND_OPENS="$FIND_OPENS" \
    SOAK_COPIES="$COPIES" SOAK_FATAL="$FATAL_MARKERS" SOAK_LOG_ERRORS="$LOG_ERRORS" \
    SOAK_XVFB_ERRORS="$XVFB_ERRORS" SOAK_INTERRUPTED="$INTERRUPTED" SOAK_LAST_ELAPSED="$LAST_ELAPSED" \
    SOAK_SAMPLE_INTERVAL="$SAMPLE_INTERVAL" SOAK_CYCLES="$CYCLE" \
    SOAK_XTERVO_LOG="$XTERVO_LOG"
python3 - <<'PY'
import csv
import json
import math
import os
import re
import statistics
import sys

out = os.environ["SOAK_OUT"]
root = os.environ["SOAK_ROOT"]
csv_path = os.path.join(out, "samples.csv")
duration_s = int(os.environ["SOAK_DURATION_S"])
warmup_s = int(os.environ["SOAK_WARMUP_S"])
interval = int(os.environ["SOAK_SAMPLE_INTERVAL"])
commit = os.environ["SOAK_COMMIT"]
branch = os.environ["SOAK_BRANCH"]
bin_path = os.environ["SOAK_BIN"]
bin_sha = os.environ["SOAK_BIN_SHA"]
bin_size = int(os.environ["SOAK_BIN_SIZE"])
display = os.environ["SOAK_DISPLAY"]
screen = os.environ["SOAK_SCREEN"]
start_iso = os.environ["SOAK_START_ISO"]
xvfb_alive = os.environ["SOAK_XVFB_ALIVE"] == "1"
app_alive = os.environ["SOAK_APP_ALIVE"] == "1"
app_exit = os.environ["SOAK_APP_EXIT"]
resize_attempts = int(os.environ["SOAK_RESIZE_ATTEMPTS"])
resize_ok = int(os.environ["SOAK_RESIZE_OK"])
sent = int(os.environ["SOAK_SENT"])
shell_lag = int(os.environ["SOAK_SHELL_LAG"])
tabs = int(os.environ["SOAK_TABS"])
splits = int(os.environ["SOAK_SPLITS"])
settings_opens = int(os.environ["SOAK_SETTINGS_OPENS"])
settings_switches = int(os.environ["SOAK_SETTINGS_SWITCHES"])
find_opens = int(os.environ["SOAK_FIND_OPENS"])
copies = int(os.environ["SOAK_COPIES"])
fatal = int(os.environ["SOAK_FATAL"] or 0)
log_errors = int(os.environ["SOAK_LOG_ERRORS"] or 0)
xvfb_errors = int(os.environ["SOAK_XVFB_ERRORS"] or 0)
interrupted = os.environ["SOAK_INTERRUPTED"] == "1"
last_elapsed = int(os.environ["SOAK_LAST_ELAPSED"])
cycles = int(os.environ["SOAK_CYCLES"])
xtervo_log = os.environ["SOAK_XTERVO_LOG"]

try:
    with open(csv_path, newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
except OSError:
    rows = []


def fnum(row, key, default=0.0):
    try:
        return float(row.get(key) or default)
    except ValueError:
        return default


def inum(row, key, default=0):
    try:
        return int(float(row.get(key) or default))
    except ValueError:
        return default


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else 0.0


def regress(points):
    if len(points) < 3:
        return 0.0, 0.0
    n = len(points)
    mx = sum(x for x, _ in points) / n
    my = sum(y for _, y in points) / n
    var = sum((x - mx) ** 2 for x, _ in points)
    if var == 0:
        return 0.0, 0.0
    slope = sum((x - mx) * (y - my) for x, y in points) / var
    ss_tot = sum((y - my) ** 2 for _, y in points)
    ss_res = sum((y - (my + slope * (x - mx))) ** 2 for x, y in points)
    r2 = 1.0 - ss_res / ss_tot if ss_tot > 0 else 0.0
    return slope, max(0.0, min(1.0, r2))


issues = []
warnings = []

if not rows:
    issues.append("没有采样数据（app 可能未启动/采样器失败）")
    post = []
else:
    post = [r for r in rows if inum(r, "elapsed_s") >= warmup_s]
    if len(post) < 3:
        post = rows
    if len(post) < 3:
        issues.append(f"预热后采样不足（{len(post)} < 3），无法判断趋势")

rss_mb = [fnum(r, "rss_kb") / 1024.0 for r in post]
pss_mb = [fnum(r, "pss_kb") / 1024.0 for r in post]
cpu = [fnum(r, "cpu_pct") for r in post]
threads = [inum(r, "threads") for r in post]
fds = [inum(r, "fds") for r in post]
children = [inum(r, "children") for r in post]
rss_base = median(rss_mb[:5])
rss_end = median(rss_mb[-5:])
pss_base = median(pss_mb[:5])
pss_end = median(pss_mb[-5:])
rss_growth = rss_end - rss_base
rss_peak = max(rss_mb) if rss_mb else 0.0
pss_peak = max(pss_mb) if pss_mb else 0.0
rss_growth_pct = (rss_growth / rss_base * 100.0) if rss_base > 0 else 0.0
slope, r2 = regress([(inum(r, "elapsed_s") / 60.0, fnum(r, "rss_kb") / 1024.0) for r in post])
thr_base = median(threads[:5])
thr_end = median(threads[-5:])
fd_base = median(fds[:5])
fd_end = median(fds[-5:])
child_base = median(children[:5])
child_end = median(children[-5:])
avg_cpu = statistics.mean(cpu) if cpu else 0.0
max_cpu = max(cpu) if cpu else 0.0

bad_samples = [r for r in rows if inum(r, "app_alive") != 1 or inum(r, "xvfb_alive") != 1]
if bad_samples:
    first_bad = bad_samples[0]
    issues.append(
        "采样中出现 app/Xvfb 死亡：t=%ss app_alive=%s xvfb_alive=%s"
        % (first_bad.get("elapsed_s"), first_bad.get("app_alive"), first_bad.get("xvfb_alive"))
    )
if not app_alive:
    issues.append("结束时 xtervo 已不在运行（APP_ALIVE=0）")
if not xvfb_alive:
    issues.append("结束时 Xvfb 已不在运行（XVFB_ALIVE=0）")
if fatal > 0:
    issues.append(f"日志出现 panic/OOM/fatal 标记 {fatal} 次")
if interrupted:
    issues.append("收到中断信号，运行未完整结束")
if not rows or last_elapsed < duration_s - 2:
    issues.append(f"实际运行 {last_elapsed}s < DURATION {duration_s}s")
if tabs < 22:
    issues.append(f"Tab 覆盖不足：日志观测 {tabs} < 22")
if splits < 3:
    issues.append(f"分屏覆盖不足：日志观测 {splits} < 3")
if settings_opens < 1:
    issues.append("Settings 未打开（场景未覆盖）")
if settings_switches < 4:
    issues.append(f"Settings 分类切换不足：{settings_switches} < 4")
if find_opens < 1:
    issues.append("搜索未打开（场景未覆盖）")
if resize_ok < 1:
    issues.append(f"窗口 resize 未成功（{resize_ok}/{resize_attempts}）")
if rss_base > 0 and rss_growth > 96 and slope > 2.0 and r2 > 0.55:
    issues.append(
        f"RSS 持续增长：{rss_growth:.1f} MiB（{rss_growth_pct:.1f}%），斜率 {slope:.2f} MiB/min，R²={r2:.2f}"
    )
elif rss_growth > 192:
    issues.append(f"RSS 绝对增长过大：{rss_growth:.1f} MiB")

if rss_growth > 32 or (slope > 1.0 and r2 > 0.55):
    warnings.append(f"RSS 增长需关注：{rss_growth:.1f} MiB，斜率 {slope:.2f} MiB/min，R²={r2:.2f}")
_strict = os.environ.get("SOAK_STRICT", "0") == "1"
if thr_end - thr_base > 32:
    _msg = f"线程数增长 {int(thr_end - thr_base)}（{int(thr_base)} -> {int(thr_end)}）"
    (issues if _strict else warnings).append(_msg)
if fd_end - fd_base > 64:
    _msg = f"FD 增长 {int(fd_end - fd_base)}（{int(fd_base)} -> {int(fd_end)}）"
    (issues if _strict else warnings).append(_msg)
if log_errors > 0:
    warnings.append(f"日志出现 error/failed {log_errors} 次（不一定是致命错误，见 app.log）")
if xvfb_errors > 0:
    warnings.append(f"Xvfb 日志出现 error/fatal {xvfb_errors} 行")
if copies < 1:
    warnings.append("复制场景未在日志中确认（拖选可能没选中文本）")
if shell_lag > max(5, sent // 10):
    warnings.append(f"受控 shell 确认滞后 {shell_lag}/{sent} 次（可能是输入焦点/UI 卡顿）")
if last_elapsed < 300:
    warnings.append(f"本次仅 {last_elapsed}s 的短跑；内存斜率只作门禁参考，不能外推 24h")

verdict = "PASS" if not issues else "FAIL"

# 采样摘要：最多 24 行，均匀抽取，保留首尾。
summary_rows = []
if rows:
    if len(rows) <= 24:
        summary_rows = rows
    else:
        step = max(1, len(rows) // 23)
        summary_rows = rows[::step]
        if summary_rows[-1] != rows[-1]:
            summary_rows.append(rows[-1])
    summary_rows = summary_rows[-25:]

def fmt_pct(value):
    return "%.1f%%" % value


lines = []
lines.append("# xtervo 稳定性 soak 报告（W-07）")
lines.append("")
lines.append(f"- 结论：**{verdict}**")
lines.append(f"- 开始：`{start_iso}`")
lines.append(f"- worktree：`{root}`")
lines.append(f"- 分支 / commit：`{branch}` / `{commit}`")
lines.append(f"- 二进制：`{bin_path}`")
lines.append(f"- SHA256：`{bin_sha}`（{bin_size} bytes）")
lines.append(f"- 显示：`{display}`（Xvfb {screen}，无 :1、无 Xephyr、无 pkill）")
lines.append(f"- 配置：22 Tab、4 pane、scrollback=2000、winit-software、Sixel+IIP 开")
lines.append(f"- 时长：请求 {duration_s}s，实际采样到 {last_elapsed}s；完成 {cycles} 个压力 cycle")
lines.append(f"- 采样：{len(rows)} 次，每 {interval}s；预热基线取 t≥{warmup_s}s 的前 5 个样本")
lines.append("")
lines.append("> 说明：这是 headless Xvfb + 软件渲染的 **长时压测**，不是 24h 结论；"
             "本报告只覆盖本次实际运行的时长。")
lines.append("")
lines.append("## 1. 场景覆盖")
lines.append("")
lines.append("| 场景 | 观测值 / 证据 |")
lines.append("|---|---|")
lines.append(f"| 输出洪峰（cat 大文本 / seq 20 万行） | 发送受控命令 {sent} 条；shell 未确认 {shell_lag} 条 |")
lines.append(f"| 20+ Tab | 日志最大 `tabs={tabs}`（目标 22） |")
lines.append(f"| 分屏 | `[ws] split pane=` {splits} 次（目标 3） |")
lines.append(f"| 滚轮 / 滚动条 | 每 cycle 12 下 + 8 上，偶数 cycle 拖滚动条 |")
lines.append(f"| Sixel / IIP | 每 cycle 各 2 张（条纹 + 尺寸参数变体） |")
lines.append(f"| emoji / 宽字符 / 组合字符 | 每 cycle 120 行，含 ZWJ/旗帜/肤色/组合标记 |")
lines.append(f"| Settings 打开 / 分类切换 | 打开 {settings_opens} 次，分类切换 {settings_switches} 次 |")
lines.append(f"| 复制 | `[clipboard] 复制` {copies} 次 |")
lines.append(f"| 搜索 | `[ui] find open` {find_opens} 次 |")
lines.append(f"| 窗口 resize | 成功 {resize_ok}/{resize_attempts} 次（900x620、1180x740、1100x700） |")
lines.append("")
lines.append("## 2. 资源趋势（预热后）")
lines.append("")
lines.append("| 指标 | 基线（前 5 样本中位） | 结束（后 5 样本中位） | 变化 | 线性斜率 |")
lines.append("|---|---|---|---|---|")
lines.append(f"| RSS | {rss_base:.1f} MiB | {rss_end:.1f} MiB | {rss_growth:+.1f} MiB（{rss_growth_pct:+.1f}%） | {slope:+.2f} MiB/min（R²={r2:.2f}） |")
lines.append(f"| PSS | {pss_base:.1f} MiB | {pss_end:.1f} MiB | {pss_end - pss_base:+.1f} MiB | — |")
lines.append(f"| 峰值 RSS/PSS | {rss_peak:.1f} / {pss_peak:.1f} MiB | — | — | — |")
lines.append(f"| CPU | 平均 {avg_cpu:.1f}% | 峰值 {max_cpu:.1f}% | — | — |")
lines.append(f"| 线程 | {int(thr_base)} | {int(thr_end)} | {int(thr_end - thr_base):+d} | — |")
lines.append(f"| FD | {int(fd_base)} | {int(fd_end)} | {int(fd_end - fd_base):+d} | — |")
lines.append(f"| 子进程（PTY shell） | {int(child_base)} | {int(child_end)} | {int(child_end - child_base):+d} | — |")
lines.append("")
lines.append("## 3. 采样摘要")
lines.append("")
lines.append("| 时间 | RSS MiB | PSS MiB | CPU | 线程 | FD | 子进程 | 阶段 |")
lines.append("|---|---|---|---|---|---|---|---|")
for r in summary_rows:
    lines.append(
        "| %ss | %.1f | %.1f | %s | %s | %s | %s | %s |"
        % (
            r.get("elapsed_s", "?"),
            fnum(r, "rss_kb") / 1024.0,
            fnum(r, "pss_kb") / 1024.0,
            fmt_pct(fnum(r, "cpu_pct")),
            r.get("threads", "?"),
            r.get("fds", "?"),
            r.get("children", "?"),
            r.get("phase", ""),
        )
    )
lines.append("")
lines.append("## 4. 判定明细")
lines.append("")
lines.append(f"- 硬失败：{len(issues)} 项")
for item in issues:
    lines.append(f"  - FAIL: {item}")
lines.append(f"- 告警：{len(warnings)} 项")
for item in warnings:
    lines.append(f"  - WARN: {item}")
lines.append("")
lines.append("## 5. 限制")
lines.append("")
lines.append("- 未运行 24h；不能把本次结果当作 24h/72h 稳定性结论。")
lines.append("- headless Xvfb + Mesa/llvmpipe + `winit-software`；未验证真实 GPU、Wayland/compositor、IME、音频。")
lines.append("- 单窗口 22 Tab + 4 pane；未覆盖多窗口、SSH/SFTP/Serial/Telnet、插件、Vault 解锁等外部 I/O。")
lines.append("- 滚动/复制/搜索通过 XTest 注入；Xvfb 没有真实输入设备/剪贴板管理器，复制只验证应用日志与堆栈，不等价于真机剪贴板体验。")
lines.append("- 内存判定是启发式阈值（RSS/PSS + 线性斜率 + R²），短跑波动会被标注为 WARN；泄漏最终需更长跑 + 堆分析确认。")
lines.append("- 整个脚本只 kill 自己记录的 Xvfb/Openbox/xtervo PID；不占用 :1、不启动 Xephyr、不使用 pkill。")
lines.append("")
lines.append("## 6. 复现")
lines.append("")
lines.append("```bash")
lines.append(f"cd {root}")
lines.append(f"DURATION=2m  ./scripts/soak.sh   # 快速门禁")
lines.append(f"DURATION=30m BIN=$PWD/target/release/xtervo ./scripts/soak.sh")
lines.append("```")
lines.append("")
lines.append("## 7. 证据文件")
lines.append("")
lines.append(f"- `samples.csv`：逐次采样（RSS/PSS/CPU/线程/FD/存活/日志错误）")
lines.append(f"- `app.log` / `logs/xtervo-*.log`：应用 stderr 与按次日志")
lines.append(f"- `xvfb.log` / `openbox.log`：X server / WM")
lines.append(f"- `shots/`：start/settings/end 截图（如环境有 ffmpeg）")
lines.append(f"- `summary.json`：机器可读判定")
lines.append("")

with open(os.path.join(out, "report.md"), "w", encoding="utf-8") as f:
    f.write("\n".join(lines) + "\n")

summary = {
    "verdict": verdict,
    "issues": issues,
    "warnings": warnings,
    "duration_requested_s": duration_s,
    "duration_actual_s": last_elapsed,
    "cycles": cycles,
    "samples": len(rows),
    "warmup_s": warmup_s,
    "display": display,
    "screen": screen,
    "commit": commit,
    "branch": branch,
    "bin": bin_path,
    "bin_sha256": bin_sha,
    "rss_base_mib": round(rss_base, 3),
    "rss_end_mib": round(rss_end, 3),
    "rss_growth_mib": round(rss_growth, 3),
    "rss_growth_pct": round(rss_growth_pct, 3),
    "rss_slope_mib_per_min": round(slope, 4),
    "rss_r2": round(r2, 4),
    "pss_base_mib": round(pss_base, 3),
    "pss_end_mib": round(pss_end, 3),
    "rss_peak_mib": round(rss_peak, 3),
    "pss_peak_mib": round(pss_peak, 3),
    "cpu_avg_pct": round(avg_cpu, 3),
    "cpu_max_pct": round(max_cpu, 3),
    "threads_base": int(thr_base),
    "threads_end": int(thr_end),
    "tabs": tabs,
    "splits": splits,
    "settings_opens": settings_opens,
    "settings_switches": settings_switches,
    "find_opens": find_opens,
    "copies": copies,
    "resize_ok": resize_ok,
    "resize_attempts": resize_attempts,
    "sent_commands": sent,
    "shell_lag": shell_lag,
    "fatal_markers": fatal,
    "log_errors": log_errors,
    "xvfb_errors": xvfb_errors,
    "app_exit": app_exit,
}
with open(os.path.join(out, "summary.json"), "w", encoding="utf-8") as f:
    json.dump(summary, f, ensure_ascii=False, indent=2)
    f.write("\n")

print("")
print("================ soak summary ================")
print(f"verdict       : {verdict}")
print(f"duration      : requested {duration_s}s / actual {last_elapsed}s")
print(f"samples       : {len(rows)} (warmup {warmup_s}s)")
print(f"RSS           : {rss_base:.1f} -> {rss_end:.1f} MiB ({rss_growth:+.1f} MiB, {slope:+.2f} MiB/min, R2={r2:.2f})")
print(f"PSS           : {pss_base:.1f} -> {pss_end:.1f} MiB (peak {pss_peak:.1f})")
print(f"CPU           : avg {avg_cpu:.1f}% / max {max_cpu:.1f}%")
print(f"threads/fds   : {int(thr_base)} -> {int(thr_end)} / {int(fd_base)} -> {int(fd_end)}")
print(f"coverage      : tabs={tabs} splits={splits} settings={settings_opens}/{settings_switches} find={find_opens} copy={copies} resize={resize_ok}/{resize_attempts}")
print(f"commands      : sent={sent} shell_lag={shell_lag}")
print(f"log           : fatal={fatal} errors={log_errors} xvfb_errors={xvfb_errors}")
print(f"report        : {os.path.join(out, 'report.md')}")
print(f"csv           : {csv_path}")
print("=============================================")
sys.exit(0 if verdict == "PASS" else 1)
PY
ANALYSIS_RC=$?

if [ "$ANALYSIS_RC" -eq 0 ]; then
    log "PASS  report=$REPORT csv=$CSV json=$SUMMARY"
    exit 0
fi
log "FAIL  report=$REPORT csv=$CSV json=$SUMMARY"
exit 1
