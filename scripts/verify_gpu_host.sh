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

say "结论提示"
cat <<'EOF'
判定要点：
1) env.log 里 nvidia-smi / vulkaninfo 正常，且 run.log 没有 "exec 自动回退 winit-software"；
2) run.log 里出现 GPU-native 的 adapter/device 信息，帧统计有 draw calls；
3) bench_render 的 damage P95 应在数十微秒量级、full 200x50 在 1–2 ms 量级（见 docs/PERFORMANCE.md §2.5）；
4) 若都满足，UI-WIN-005 的真机 GPU 部分即可在矩阵里转 implemented（把本目录日志路径写进 Test 列）。
产物：.logs/gpu-host-verify/{env.log,run.log,bench_render.log}
EOF
