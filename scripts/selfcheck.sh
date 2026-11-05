#!/usr/bin/env bash
# 一键自检 / 迭代回归：
#   build → 单测 → VT 语义契约 → 启动预算 + 空闲 RSS →（可选）IME 冒烟
# 输出 .logs/selfcheck/<时间戳>/summary.json，并追加 history.jsonl 供趋势对比。
#
# 用法：
#   ./scripts/selfcheck.sh            # 常规自检
#   RUN_IME=1 ./scripts/selfcheck.sh  # 额外跑中文 IME 冒烟（需要 X11 + fcitx/ibus）
#   PROFILE=debug ./scripts/selfcheck.sh
set -euo pipefail
cd "$(dirname "$0")/.."

export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"
PROFILE="${PROFILE:-release}"
BIN="target/$PROFILE/rusttabby-app"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT=".logs/selfcheck/$STAMP"
mkdir -p "$OUT"

FAIL=0
run() {
    local name="$1"
    shift
    printf '== %-16s ==\n' "$name"
    if "$@" >"$OUT/$name.log" 2>&1; then
        echo "PASS $name"
    else
        echo "FAIL $name  (日志: $OUT/$name.log)"
        FAIL=1
    fi
}

case "$PROFILE" in
    release) run build cargo build --release -p rusttabby-app ;;
    debug) run build cargo build -p rusttabby-app ;;
    *) echo "PROFILE 只能是 release/debug"; exit 2 ;;
esac
run core-tests cargo test -p rusttabby-core
run vt-conformance cargo test -p rusttabby-core --test vt_conformance
run app-tests cargo test -p rusttabby-app --bin rusttabby-app

# ---- 启动预算：3 次冷启动，取中位数 ----
READY_MED="null"
FIRST_MED="null"
if [ -n "${DISPLAY:-}" ] && [ -x "$BIN" ]; then
    printf '== %-16s ==\n' "startup"
    READIES=()
    FIRSTS=()
    for i in 1 2 3; do
        CFG="$OUT/cfg-$i.toml"
        printf 'theme = "auto"\n' >"$CFG"
        if timeout 30 env RUSTTABBY_CONFIG="$CFG" "$BIN" --cols 60 --rows 12 \
            --exit-after-first >"$OUT/startup-$i.log" 2>&1; then
            r=$(awk '/READY TIMING/{for (i=1;i<=NF;i++) if ($i ~ /^total_us=/) {split($i,a,"="); print a[2]}}' "$OUT/startup-$i.log")
            f=$(awk '/FIRST_DAMAGE_US/{print $2}' "$OUT/startup-$i.log")
            [ -n "$r" ] && READIES+=("$r")
            [ -n "$f" ] && FIRSTS+=("$f")
        else
            echo "FAIL startup run $i"
            FAIL=1
        fi
    done
    if [ "${#READIES[@]}" -eq 0 ] || [ "${#FIRSTS[@]}" -eq 0 ]; then
        echo "FAIL startup  (没有解析到 READY TIMING / FIRST_DAMAGE_US，日志: $OUT/startup-*.log)"
        FAIL=1
    fi
    if [ "${#READIES[@]}" -gt 0 ] && [ "${#FIRSTS[@]}" -gt 0 ]; then
        eval "$(python3 - "${READIES[@]}" -- "${FIRSTS[@]}" <<'PY'
import sys, statistics
args = sys.argv[1:]
sep = args.index('--')
ready = [float(x) for x in args[:sep]]
first = [float(x) for x in args[sep + 1:]]
print(f"READY_MED={statistics.median(ready)/1000:.1f}")
print(f"FIRST_MED={statistics.median(first)/1000:.1f}")
PY
)"
        echo "startup: ready=${READY_MED}ms first_damage=${FIRST_MED}ms"
    fi
else
    echo "SKIP startup (DISPLAY 未设置或二进制不存在)"
fi

# ---- 空闲 RSS：字体惰性加载后的常驻内存预算 ----
IDLE_RSS="null"
if [ -n "${DISPLAY:-}" ] && [ -x "$BIN" ]; then
    printf '== %-16s ==\n' "idle-rss"
    if timeout 60 python3 scripts/measure_startup.py --rss-only --bin "$BIN" \
        >"$OUT/idle-rss.log" 2>&1; then
        IDLE_RSS=$(python3 - "$OUT/idle-rss.log" <<'PY'
import json, sys

# 脚本正常只输出一行 JSON；兼容 stderr 警告混入日志，取最后一行解析。
try:
    lines = open(sys.argv[1], errors="ignore").read().strip().splitlines()
    value = json.loads(lines[-1]).get("idle_rss_mb", -1) if lines else -1
except (OSError, ValueError, AttributeError):
    value = -1
print("null" if value is None or value <= 0 else value)
PY
)
        if [ "$IDLE_RSS" = "null" ]; then
            echo "FAIL idle-rss  (未能读取空闲 RSS，日志: $OUT/idle-rss.log)"
            FAIL=1
        else
            echo "idle-rss: idle_rss_mb=${IDLE_RSS}"
        fi
    else
        echo "FAIL idle-rss  (日志: $OUT/idle-rss.log)"
        FAIL=1
    fi
else
    echo "SKIP idle-rss (DISPLAY 未设置或二进制不存在)"
fi

# ---- 可选 IME 冒烟 ----
IME="skip"
if [ "${RUN_IME:-0}" = "1" ] && [ -n "${DISPLAY:-}" ]; then
    printf '== %-16s ==\n' "ime"
    if ./scripts/check_ime.sh "$PROFILE" >"$OUT/ime.log" 2>&1; then
        IME="pass"
        echo "PASS ime"
    else
        IME="fail"
        FAIL=1
        echo "FAIL ime  (日志: $OUT/ime.log)"
    fi
fi

# ---- 汇总 + 阈值门禁 ----
if python3 - "$OUT" "$PROFILE" "$READY_MED" "$FIRST_MED" "$IDLE_RSS" "$FAIL" "$IME" <<'PY'
import glob, json, os, re, sys, time

out, profile, ready, first, idle_rss, fail, ime = sys.argv[1:8]
ready = None if ready == "null" else float(ready)
first = None if first == "null" else float(first)
idle_rss = None if idle_rss == "null" else float(idle_rss)
fail = int(fail)

suites = {}
for path in sorted(glob.glob(os.path.join(out, "*.log"))):
    name = os.path.splitext(os.path.basename(path))[0]
    text = open(path, errors="ignore").read()
    # 一个 cargo test 日志里会有多条 "test result:"（lib 单测 + 每个集成测试二进制），
    # 只取第一条会让 tests/smoke.rs、tests/mouse_report.rs 被删了也发现不了。
    matches = re.findall(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed", text)
    if matches:
        suites[name] = {
            "ok": all(m[0] == "ok" for m in matches),
            "passed": sum(int(m[1]) for m in matches),
            "failed": sum(int(m[2]) for m in matches),
        }

size = 0
bin_path = os.path.join("target", profile, "rusttabby-app")
if os.path.exists(bin_path):
    size = os.path.getsize(bin_path)

# 基线门禁：性能回退 / 内存回退 / 测试数量减少都算回归
baseline_path = "docs/selfcheck-baseline.json"
baseline = {}
if os.path.exists(baseline_path):
    baseline = json.load(open(baseline_path))

problems = []
if ready is not None and baseline.get("ready_ms_max") and ready > baseline["ready_ms_max"]:
    problems.append(f"ready {ready:.1f}ms > 基线 {baseline['ready_ms_max']}ms")
if first is not None and baseline.get("first_damage_ms_max") and first > baseline["first_damage_ms_max"]:
    problems.append(f"first_damage {first:.1f}ms > 基线 {baseline['first_damage_ms_max']}ms")
if idle_rss is not None and baseline.get("idle_rss_mb_max") and idle_rss > baseline["idle_rss_mb_max"]:
    problems.append(f"idle_rss {idle_rss:.1f}MB > 基线 {baseline['idle_rss_mb_max']}MB")

mins = {
    "core-tests": baseline.get("min_core_tests", 0),
    "vt-conformance": baseline.get("min_conformance_tests", 0),
    "app-tests": baseline.get("min_app_tests", 0),
}
for name, minimum in mins.items():
    got = suites.get(name, {}).get("passed", 0)
    if minimum and got < minimum:
        problems.append(f"{name} 只剩 {got} 个（基线 {minimum}），疑似删测试")

summary = {
    "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S"),
    "profile": profile,
    "ready_ms": ready,
    "first_damage_ms": first,
    "idle_rss_mb": idle_rss,
    "binary_bytes": size,
    "ime": ime,
    "suites": suites,
    "pass": fail == 0 and not problems,
}
with open(os.path.join(out, "summary.json"), "w") as f:
    json.dump(summary, f, ensure_ascii=False, indent=2)
os.makedirs(".logs/selfcheck", exist_ok=True)
with open(".logs/selfcheck/history.jsonl", "a") as f:
    f.write(json.dumps(summary, ensure_ascii=False) + "\n")

print()
print(f"summary: {os.path.join(out, 'summary.json')}")
print(f"history: .logs/selfcheck/history.jsonl")
print(f"pass={summary['pass']} ready={ready}ms first_damage={first}ms idle_rss={idle_rss}MB size={size}")
if problems:
    print("REGRESSION:")
    for p in problems:
        print(" -", p)
    sys.exit(1)
PY
then
    :
else
    FAIL=1
fi

echo
if [ "$FAIL" -eq 0 ]; then
    echo "SELFCHECK PASS"
else
    echo "SELFCHECK FAIL"
fi
exit "$FAIL"
