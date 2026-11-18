#!/usr/bin/env bash
# 在**宿主机**（有 GPU / 有真实桌面会话）上验证 UI-WIN-005 与 GPU 路径。
#
# 我在 agent 沙箱里看不到 /dev/dri 与 NVIDIA 设备节点（bwrap 隔离），
# 所以这一步需要你在自己的终端里跑；脚本只读，不会改配置。
#
# 用法：
#   ./scripts/verify_gpu_host.sh            # 用 target/release/tervo（没有就构建）
#   BIN=/path/to/tervo ./scripts/verify_gpu_host.sh
set -euo pipefail
cd "$(dirname "$0")/.."

CHECK=0
for arg in "$@"; do
    case "$arg" in
        --check) CHECK=1 ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

OUT=".logs/gpu-host-verify"
mkdir -p "$OUT"
BIN=${BIN:-target/release/tervo}

say() { printf '\n== %s ==\n' "$*"; }

say "环境"
{
    date
    echo "DISPLAY=${DISPLAY:-} WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-} XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-}"
    echo "-- nvidia-smi"
    nvidia-smi -L 2>&1 || echo "nvidia-smi: 不可用"
    nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv 2>&1 || true
    echo "-- /dev/dri"
    ls -l /dev/dri 2>&1 || echo "无 /dev/dri"
    echo "-- glxinfo"
    command -v glxinfo >/dev/null && glxinfo -B 2>&1 | sed -n '1,18p' || echo "无 glxinfo"
    echo "-- vulkaninfo"
    command -v vulkaninfo >/dev/null && vulkaninfo --summary 2>/dev/null | sed -n '1,25p' || echo "无 vulkaninfo"
} 2>&1 | tee "$OUT/env.log"

if [ ! -x "$BIN" ]; then
    say "构建 release"
    export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"
    cargo build --release --offline -p tervo-app
fi

say "GPU-native 运行 20s（采集帧统计）"
# 只开一个临时实例目录，避免污染你正在用的会话
timeout 25 env \
    TERVO_GPU_NATIVE=1 TERVO_GPU_NATIVE_DEBUG=1 \
    TERVO_INSTANCE_DIR=/tmp/tervo-gpu-verify \
    "$BIN" --cols 160 --rows 48 >"$OUT/run.log" 2>&1 || true

echo "-- 后端/回退关键字"
grep -nEi "backend|renderer|gpu-native|device|adapter|exec 自动回退|winit-software|fallback|error" \
    "$OUT/run.log" | head -40 || true
echo "-- 最后 20 行"
tail -20 "$OUT/run.log"

say "渲染基准 bench_render（release）"
./scripts/bench_render.sh 2>&1 | tee "$OUT/bench_render.log" | tail -25

if [ "$CHECK" = "1" ]; then
    say "判定（--check）"
    fail=0
    ok()  { printf 'PASS  %s\n' "$1"; }
    bad() { printf 'FAIL  %s\n' "$1"; fail=$((fail+1)); }

    # 1) 宿主机是否真的有可用 GPU
    if nvidia-smi -L >/dev/null 2>&1 && [ -n "$(nvidia-smi -L 2>/dev/null)" ]; then
        gpu_name=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)
        ok "GPU 可见：${gpu_name:-nvidia}"
    elif grep -qi "deviceName" "$OUT/env.log" 2>/dev/null; then
        ok "GPU 可见（vulkaninfo deviceName）"
    elif ls /dev/dri/renderD* >/dev/null 2>&1; then
        ok "GPU 可见（/dev/dri/renderD*）"
    else
        bad "宿主机看不到 GPU（nvidia-smi / vulkaninfo / /dev/dri 均无）"
    fi

    # 2) 没有回退到软件渲染
    renderer=$(grep -oE "actual_renderer=[A-Za-z0-9_-]+" "$OUT/run.log" 2>/dev/null | tail -1 | cut -d= -f2)
    if grep -qE "exec 自动回退 winit-software|\[gpu-native\] 失败，自动回退 CPU" "$OUT/run.log" 2>/dev/null; then
        bad "日志出现 GPU 回退（exec 自动回退 / gpu-native 失败）"
        grep -nE "exec 自动回退|失败，自动回退 CPU" "$OUT/run.log" | head -3 | sed 's/^/      /'
    elif [ "$renderer" = "winit-software" ]; then
        bad "actual_renderer=winit-software（当前是软件渲染）"
    else
        ok "未回退：actual_renderer=${renderer:-未记录}"
    fi

    # 3) GPU-native 链路真的建立（adapter/backend）
    if grep -q "\[gpu-native\] TERVO_GPU_NATIVE=1" "$OUT/run.log" 2>/dev/null; then
        line=$(grep -m1 "\[gpu-native\] TERVO_GPU_NATIVE=1" "$OUT/run.log")
        ok "GPU-native 生效：$(printf '%s' "$line" | grep -oE 'adapter=[^ ]+ backend=[^ ]+ device_type=[^ ]+' | head -1)"
    elif printf '%s' "${renderer:-}" | grep -qiE "skia|wgpu|gl|gpu"; then
        if grep -q "WGPU 显示链路初始化失败" "$OUT/run.log" 2>/dev/null; then
            echo "WARN  WGPU 显示链路初始化失败，但 Skia GPU renderer 仍生效（TERVO_GPU_NATIVE 属实验路径）"
        fi
        ok "GPU 渲染生效：actual_renderer=${renderer}"
    else
        bad "没有 GPU 生效证据（renderer=${renderer:-未记录}，也无 gpu-native 行）"
    fi

    # 4) 渲染基准阈值（damage P95 与 full 200x50）
    bench="$OUT/bench_render.log"
    value_of() { # $1=精确 scenario $2=field
        awk -v sc="$1" -v f="$2" '
            {
                hit = 0
                for (i = 1; i <= NF; i++) if ($i == ("scenario=" sc)) hit = 1
                if (hit) for (i = 1; i <= NF; i++) if ($i ~ ("^" f "=")) { split($i, a, "="); print a[2]; exit }
            }
        ' "$bench" 2>/dev/null
    }
    if [ -s "$bench" ] && grep -q "\[bench\] scenario=" "$bench"; then
        dmg=$(value_of "single_row_damage" "draw_p95_us")
        full=$(value_of "full_200x50" "total_p50_us")
        [ -z "$dmg" ] && dmg=$(value_of "typing_burst_damage" "draw_p95_us")
        if [ -n "$dmg" ] && awk -v v="$dmg" 'BEGIN{exit !(v < 500)}'; then
            ok "damage 路径 draw_p95=${dmg}µs（阈值 <500µs）"
        else
            bad "damage 路径 draw_p95=${dmg:-缺失}µs（阈值 <500µs）"
        fi
        if [ -n "$full" ] && awk -v v="$full" 'BEGIN{exit !(v < 5000)}'; then
            ok "full 200x50 total_p50=${full}µs（阈值 <5000µs）"
        else
            bad "full 200x50 total_p50=${full:-缺失}µs（阈值 <5000µs）"
        fi
    else
        bad "bench_render 没有输出（$bench 为空或缺 [bench] scenario= 行）"
    fi

    echo
    if [ "$fail" -eq 0 ]; then
        echo "RESULT: PASS（UI-WIN-005 的真机 GPU 部分可转 implemented）"
        echo "证据：$OUT/{env.log,run.log,bench_render.log}"
        exit 0
    else
        echo "RESULT: FAIL（$fail 项未通过，把上面 PASS/FAIL 与 $OUT/run.log 尾部贴出来）"
        exit 1
    fi
fi

say "结论提示"
cat <<'EOF'
判定要点：
1) env.log 里 nvidia-smi / vulkaninfo 正常，且 run.log 没有 "exec 自动回退 winit-software"；
2) run.log 里出现 GPU-native 的 adapter/device 信息，帧统计有 draw calls；
3) bench_render 的 damage P95 应在数十微秒量级、full 200x50 在 1–2 ms 量级（见 docs/PERFORMANCE.md §2.5）；
4) 若都满足，UI-WIN-005 的真机 GPU 部分即可在矩阵里转 implemented（把本目录日志路径写进 Test 列）。
产物：.logs/gpu-host-verify/{env.log,run.log,bench_render.log}

想直接看 PASS/FAIL：./scripts/verify_gpu_host.sh --check
EOF
