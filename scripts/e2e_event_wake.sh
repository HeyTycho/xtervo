#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# P1-07 事件唤醒 e2e（headless Xvfb :138）：
#   1. 同一 binary 跑两遍：
#        wake   = 默认事件唤醒（CoreEvent / 插件活动 / SFTP 结果 → Slint 事件循环）
#        before = TERVO_EVENT_WAKE=0，纯 400ms 轮询（旧行为，A/B 对照）
#   2. 语义事件延迟：OSC 标题 ×6、BEL、OSC52 query；每项日志带 epoch ms，
#      与 shell 里 `date +%s%3N` 的发送时间戳直接相减。
#   3. 插件 startup 活动、SFTP 上传完成：后台线程写结果后唤醒 UI 的延迟。
#   4. 不丢/不重复：每个事件在日志里必须恰好出现一次。
#   5. 空闲：连续两行 `[wake]` 统计里 core/app 唤醒计数不增长，400ms 兜底仍在跑；
#      同时对比两遍的空闲 CPU ticks / voluntary context switches。
#
# 安全：只启动/回收本脚本自己的 Xvfb、app、xsel；不 pkill、不碰 :1。
#
# 前置构建：
#   CARGO_HOME=... cargo build -p tervo-app --bin tervo --bin xwin --bin xclick
# 用法：
#   ./scripts/e2e_event_wake.sh
#   E2E_DISPLAY=:138 E2E_OUT=.logs/event-wake ./scripts/e2e_event_wake.sh

set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/event-wake}
DISPLAY_NUM=${E2E_DISPLAY:-:138}
SCREEN=${E2E_SCREEN:-1280x900x24}

TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${E2E_BIN:-$TARGET_DIR/debug/tervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
for f in "$BIN" "$XWIN" "$XCLICK"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick" >&2; exit 2; }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }
for tool in xdpyinfo xwininfo xprop xsel; do
    command -v "$tool" >/dev/null 2>&1 || { echo "缺少依赖：$tool" >&2; exit 2; }
done

rm -rf "$OUT/wake" "$OUT/before"
mkdir -p "$OUT"
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

if DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1; then
    echo "复用已有 display $DISPLAY_NUM（测试结束不关闭）"
else
    "$XVFB_BIN" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp \
        >"$OUT/xvfb.log" 2>&1 &
    XVFB_PID=$!
    OWN_DISPLAY=1
    for _ in $(seq 1 100); do DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
    DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1 || fail "Xvfb $DISPLAY_NUM 启动失败"
fi
export DISPLAY=$DISPLAY_NUM

# 固定测试材料：pane 脚本按固定间隔发 OSC 标题/BEL/OSC52 query；
# 插件 startup 完成和假 sftp 传输完成各写一个 epoch ms 时间戳。
PANE_SCRIPT_TMPL='DIR=$1
stty raw -echo
mark() { printf "%s %s\n" "$1" "$(date +%s%3N)" >> "$DIR/stamps"; }
sleep 1.5
i=1
while [ "$i" -le 6 ]; do
    mark "title$i"
    printf "\033]0;wake-title-%s\007" "$i"
    sleep 0.3
    i=$((i + 1))
done
mark bell
printf "\007"
sleep 0.3
mark osc52
printf "\033]52;c;?\007"
sleep 0.3
timeout --foreground 3 cat > "$DIR/query.bin"
sleep 40
'
PLUGIN_TOML='name = "latency"
version = "1.0.0"
api = 1
entry = "run.sh"
events = ["startup"]
'
PROFILES_TOML='version = 2
next_id = 2

[[profiles]]
id = 1
name = "fake-ssh"
kind = "ssh"
host = "example.invalid"
port = 22
user = "tester"
auth = "agent"
'
CONFIG_TOML='version = 1
theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false
confirm_multiline_paste = false
osc52 = "allow"
visual_bell = true
audible_bell = false
'

# 假 sftp：List 返回固定条目；上传/下载写进度行并在退出前记录完成时间戳。
make_sftp() { # <dir>
    local dir=$1
    cat > "$dir/bin/sftp" <<SFTP
#!/bin/sh
input=\$(cat)
case "\$input" in
    *"ls -1"*)
        printf 'probe-one\nprobe-two\n'
        ;;
    *)
        printf 'fake.bin 10%% 1KB 1.0MB/s --:-- ETA\r'
        sleep 0.05
        printf 'fake.bin 100%% 2KB 1.0MB/s 00:00\r'
        date +%s%3N > "$dir/sftp.stamp"
        ;;
esac
exit 0
SFTP
    chmod +x "$dir/bin/sftp"
}

make_plugin() { # <dir>
    local dir=$1
    mkdir -p "$dir/plugins/latency"
    printf '%s' "$PLUGIN_TOML" > "$dir/plugins/latency/plugin.toml"
    cat > "$dir/plugins/latency/run.sh" <<PLUGIN
#!/bin/sh
read -r _payload
sleep 2
date +%s%3N > "$dir/plugin.stamp"
printf 'plugin-latency-ok\n'
exit 0
PLUGIN
    chmod +x "$dir/plugins/latency/run.sh"
}

start_app() { # <dir> <wake|before>
    local dir=$1 mode=$2
    mkdir -p "$dir/home" "$dir/logs" "$dir/bin"
    printf '%s' "$CONFIG_TOML" > "$dir/config.toml"
    printf '%s' "$PROFILES_TOML" > "$dir/profiles.toml"
    printf '%s' "$PANE_SCRIPT_TMPL" > "$dir/pane.sh"
    chmod +x "$dir/pane.sh"
    make_sftp "$dir"
    make_plugin "$dir"
    rm -f "$dir/stamps" "$dir/query.bin" "$dir/plugin.stamp" "$dir/sftp.stamp"
    # 写系统剪贴板，OSC52 query 才能回非空内容（同时验证回包真正写回 PTY）。
    DISPLAY=$DISPLAY_NUM HOME="$dir/home" printf 'wake-clip' | \
        DISPLAY=$DISPLAY_NUM HOME="$dir/home" xsel -b -i
    if [ "$mode" = before ]; then
        export TERVO_EVENT_WAKE=0
    else
        unset TERVO_EVENT_WAKE || true
    fi
    DISPLAY=$DISPLAY_NUM HOME="$dir/home" XMODIFIERS=@im=none SLINT_BACKEND=winit-software \
        TERVO_LOG_DIR="$dir/logs" TERVO_CONFIG="$dir/config.toml" \
        TERVO_PROFILES="$dir/profiles.toml" TERVO_PLUGINS="$dir/plugins" \
        TERVO_WAKE_STATS=1 PATH="$dir/bin:$PATH" \
        "$BIN" --cols 100 --rows 28 --shell /bin/sh -- -c "$dir/pane.sh $dir" \
        >"$dir/app.out" 2>&1 &
    APP_PID=$!
    echo "$APP_PID" > "$dir/pid"
    local win=""
    for _ in $(seq 1 100); do
        win=$(xwininfo -root -tree 2>/dev/null | grep -iE '"tervo" "tervo"' | awk '{print $1}' | head -1 || true)
        [ -n "$win" ] && break
        sleep 0.1
    done
    [ -n "$win" ] || fail "app 窗口未出现（$dir/app.out）"
    "$XWIN" "$win" 20 20 >/dev/null 2>&1 || true
    sleep 0.4
    echo "$win" > "$dir/win"
}

stop_app() { # <dir>
    local dir=$1
    local pid; pid=$(cat "$dir/pid" 2>/dev/null || true)
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    [ -n "$pid" ] && wait "$pid" 2>/dev/null || true
    APP_PID=""
}

app_log() { ls -t "$1"/logs/tervo-*.log 2>/dev/null | head -1; }

wait_log() { # <log> <grep-pattern> <seconds>
    local log=$1 pat=$2 secs=$3
    local end=$((SECONDS + secs))
    while [ $SECONDS -lt $end ]; do
        grep -q -- "$pat" "$log" 2>/dev/null && return 0
        sleep 0.05
    done
    return 1
}

# 打开 TabBar 菜单 → SFTP 面板 → Upload（面板无列表选择也能上传）。
# 坐标与 scripts/e2e_p0_blocking.sh 一致；菜单 y 由 app 日志反推。
trigger_sftp_upload() { # <dir> <log>
    local dir=$1 log=$2
    local win; win=$(cat "$dir/win")
    local ox oy w h
    ox=$(xwininfo -id "$win" | awk '/Absolute upper-left X/{print $NF}')
    oy=$(xwininfo -id "$win" | awk '/Absolute upper-left Y/{print $NF}')
    w=$(xwininfo -id "$win" | awk '/Width:/{print $2}')
    h=$(xwininfo -id "$win" | awk '/Height:/{print $2}')
    [ -n "$ox" ] && [ -n "$oy" ] && [ -n "$w" ] && [ -n "$h" ] || fail "读不到窗口几何"
    local mx_local=$(( w > 900 ? 700 : w - 300 ))
    "$XCLICK" $((ox + mx_local)) $((oy + 16)) 3 >/dev/null 2>&1
    sleep 0.3
    local menu_line
    menu_line=$(grep -o '\[ui\] tabbar menu at [0-9]*,[0-9]*' "$log" | tail -1)
    [ -n "$menu_line" ] || fail "没有打开 TabBar 菜单"
    local menu_x menu_y menu_left
    menu_x=$(echo "$menu_line" | sed -E 's/.* at ([0-9]+),([0-9]+)/\1/')
    menu_y=$(echo "$menu_line" | sed -E 's/.* at ([0-9]+),([0-9]+)/\2/')
    menu_left=$(( menu_x < w - 180 ? menu_x : w - 180 ))
    "$XCLICK" $((ox + menu_left + 85)) $((oy + menu_y + 4 + 9 * 30 + 14)) 1 >/dev/null 2>&1
    wait_log "$log" '\[sftp\] 打开面板' 5 || fail "SFTP 面板没有打开"
    local panel_x=$(( (w - 460) / 2 ))
    local panel_y=$(( (h - 490) / 2 ))
    # Upload 按钮中心：面板内 x=162..252、y=366..390。
    "$XCLICK" $((ox + panel_x + 207)) $((oy + panel_y + 378)) 1 >/dev/null 2>&1
}

# 从 /proc 读 CPU ticks（utime+stime，100Hz）与 voluntary context switches。
proc_cpu_ticks() {
    awk '{print $14 + $15}' "/proc/$1/stat" 2>/dev/null || echo 0
}
proc_voluntary_cs() {
    awk '/^voluntary_ctxt_switches/{print $2}' "/proc/$1/status" 2>/dev/null || echo 0
}

run_mode() { # <wake|before>
    local mode=$1
    local dir="$OUT/$mode"
    start_app "$dir" "$mode"
    local log; log=$(app_log "$dir")
    [ -n "$log" ] || fail "$mode: 没有日志文件"
    wait_log "$log" 'osc52 query' 15 || fail "$mode: 没有处理 OSC52 query"
    wait_log "$log" 'plugin activity rev=' 10 || fail "$mode: 插件活动没有进 UI"
    trigger_sftp_upload "$dir" "$log"
    wait_log "$log" '传输结束 phase=' 10 || fail "$mode: SFTP 传输没有结束"
    [ -f "$dir/sftp.stamp" ] || fail "$mode: 假 sftp 没有写完成时间戳"

    # 等 10s/15s 两行唤醒统计，再做 8s 空闲 CPU/唤醒采样。
    local end=$((SECONDS + 16))
    while [ $SECONDS -lt $end ]; do
        [ "$(grep -c '\[wake\]' "$log" 2>/dev/null || true)" -ge 4 ] && break
        sleep 0.25
    done
    sleep 1
    local cpu1 cs1 cpu2 cs2
    cpu1=$(proc_cpu_ticks "$APP_PID"); cs1=$(proc_voluntary_cs "$APP_PID")
    sleep 8
    cpu2=$(proc_cpu_ticks "$APP_PID"); cs2=$(proc_voluntary_cs "$APP_PID")
    kill -0 "$APP_PID" 2>/dev/null || fail "$mode: app 在空闲采样期间退出"
    {
        echo "MODE=$mode"
        echo "LOG=$log"
        echo "STAMPS=$dir/stamps"
        echo "QUERY=$dir/query.bin"
        echo "PLUGIN_STAMP=$dir/plugin.stamp"
        echo "SFTP_STAMP=$dir/sftp.stamp"
        echo "IDLE_CPU_TICKS=$((cpu2 - cpu1))"
        echo "IDLE_VOLUNTARY_CS=$((cs2 - cs1))"
    } > "$dir/measure.env"
    cp "$log" "$dir/app.log.copy"
    # 视觉验收：标题/Bell/OSC52 处理完后抓一张窗口图（标题栏显示 wake-title-6）。
    if [ "$mode" = wake ]; then
        local win; win=$(cat "$dir/win")
        if xwd -id "$win" -silent -out "$dir/shot.xwd" 2>/dev/null; then
            python3 "$ROOT/scripts/xwd2png.py" "$dir/shot.xwd" "$OUT/event-wake.png" \
                >/dev/null 2>&1 || true
        fi
    fi
    stop_app "$dir"
}

run_mode wake
run_mode before

# 断言/汇总：Python 读两遍的日志与时间戳，输出 PASS/FAIL 并打印延迟表。
python3 - "$OUT" <<'PY'
import re
import sys
from pathlib import Path

out = Path(sys.argv[1])
failures = []

def parse_env(path):
    env = {}
    for line in path.read_text().splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            env[k] = v
    return env

def one(pattern, text, label, mode):
    matches = re.findall(pattern, text)
    if not matches:
        failures.append(f"{mode}: 日志里找不到 {label}")
        return None
    if len(matches) > 1:
        failures.append(f"{mode}: {label} 被处理了 {len(matches)} 次（应恰好 1 次）")
    return int(matches[0])

def sample(stamps, key):
    m = re.search(rf"^{re.escape(key)} (\d+)$", stamps, re.M)
    return int(m.group(1)) if m else None

rows = []
for mode in ("wake", "before"):
    env = parse_env(out / mode / "measure.env")
    log = Path(env["LOG"]).read_text()
    stamps = Path(env["STAMPS"]).read_text()
    deltas = {}

    for i in range(1, 7):
        sent = sample(stamps, f"title{i}")
        got = one(
            rf'\[event\] ts_ms=(\d+) pane=\d+ kind=title title="wake-title-{i}"',
            log,
            f"OSC 标题 {i}",
            mode,
        )
        if sent is not None and got is not None:
            deltas[f"title{i}"] = got - sent

    sent = sample(stamps, "bell")
    got = one(r"\[event\] ts_ms=(\d+) pane=\d+ kind=bell", log, "BEL", mode)
    if sent is not None and got is not None:
        deltas["bell"] = got - sent

    sent = sample(stamps, "osc52")
    got = one(r"\[clipboard\] ts_ms=(\d+) osc52 query", log, "OSC52 query", mode)
    if sent is not None and got is not None:
        deltas["osc52"] = got - sent

    plugin_stamp = int(Path(env["PLUGIN_STAMP"]).read_text().strip())
    got = one(r"\[ui\] ts_ms=(\d+) plugin activity rev=\d+", log, "插件活动刷新", mode)
    if got is not None:
        deltas["plugin"] = got - plugin_stamp

    sftp_stamp = int(Path(env["SFTP_STAMP"]).read_text().strip())
    got = one(r"\[sftp\] ts_ms=(\d+) 传输结束 phase=Done", log, "SFTP 传输完成", mode)
    if got is not None:
        deltas["sftp"] = got - sftp_stamp

    if any(v < 0 for v in deltas.values()):
        failures.append(f"{mode}: 存在负延迟（时钟/匹配错误）: {deltas}")

    # 空闲唤醒计数：取最后两行统计；core/app 不应增长，400ms 兜底仍按周期跑。
    stats = [
        (int(a), int(b), int(c))
        for a, b, c in re.findall(
            r"\[wake\] ts_ms=\d+ core_event=(\d+) app_event=(\d+) periodic_400ms=(\d+)",
            log,
        )
    ]
    if len(stats) < 2:
        failures.append(f"{mode}: 唤醒统计不足 2 行（TERVO_WAKE_STATS 未生效）")
        idle = (0, 0, 0, 0, 0)
    else:
        (c1, a1, p1), (c2, a2, p2) = stats[-2], stats[-1]
        dc, da, dp = c2 - c1, a2 - a1, p2 - p1
        if dc != 0 or da != 0:
            failures.append(f"{mode}: 空闲时 core/app 唤醒仍在增长 core={dc} app={da}")
        if not (5 <= dp <= 20):
            failures.append(f"{mode}: 空闲 5s 内 400ms 兜底次数异常: {dp}")
        if mode == "wake":
            if c2 < 8:
                failures.append(f"wake: CoreEvent 唤醒次数不足（{c2}），事件可能仍走轮询")
            if a2 < 1:
                failures.append(f"wake: 后台事件唤醒次数不足（{a2}）")
        else:
            if c2 != 0 or a2 != 0:
                failures.append(f"before: 诊断关闭后不应有唤醒回调 core={c2} app={a2}")
        idle = (dc, da, dp, c1, a1)

    cpu = int(env["IDLE_CPU_TICKS"])
    cs = int(env["IDLE_VOLUNTARY_CS"])
    rows.append((mode, deltas, idle, cpu, cs))

    if mode == "wake":
        limit = 50
        for name, delta in deltas.items():
            if delta >= limit:
                failures.append(f"wake: {name} 延迟 {delta}ms >= {limit}ms（事件唤醒未生效）")
        query = Path(env["QUERY"]).read_bytes()
        # 回包必须真的写回 PTY：OSC 52;c;<base64(wake-clip)> ST
        if b"d2FrZS1jbGlw" not in query:
            failures.append(f"wake: OSC52 回包内容缺失: {query[:80]!r}")
    else:
        core = [v for k, v in deltas.items() if k != "plugin" and k != "sftp"]
        if core and max(core) < 100:
            failures.append(
                f"before: 400ms 轮询对照应出现 >=100ms 延迟，实测 {deltas}"
            )

# CPU：两遍都是同一 binary，只差唤醒路径；after 不应比 before 明显更高。
by_mode = {r[0]: r for r in rows}
if len(by_mode) == 2:
    before_cpu, after_cpu = by_mode["before"][3], by_mode["wake"][3]
    if after_cpu > before_cpu * 1.5 + 10:
        failures.append(
            f"空闲 CPU 退化：before={before_cpu} ticks after={after_cpu} ticks (8s)"
        )

print("== 事件延迟（发送时间戳 → UI 处理日志，ms）")
for mode, deltas, idle, cpu, cs in rows:
    pretty = " ".join(f"{k}={v}" for k, v in deltas.items())
    print(f"  {mode:6s} {pretty}")
    print(
        f"         空闲 core_delta={idle[0]} app_delta={idle[1]} "
        f"periodic_400ms={idle[2]} core_total={idle[3]} app_total={idle[4]}"
    )
    print(f"         CPU={cpu} ticks/8s voluntary_cs={cs}")

if failures:
    print("\nFAIL:")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print("\n全部 PASS")
PY

echo "日志/截图目录: $OUT"
