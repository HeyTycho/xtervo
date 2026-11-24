#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 在隔离的 Xvfb 上运行 GPU 显示链路验收 example。
#
# 硬规则：绝不使用用户的 :1 窗口/进程。默认 :77；若目标 display 已被占用则
# 直接报错退出，不 kill 任何已有 X server。
#
# 用法：
#   scripts/gpu-terminal-spike.sh
#   DISPLAY_NUM=:78 XTERVO_XVFB=/path/to/Xvfb scripts/gpu-terminal-spike.sh
#
# 本机没有系统 Xvfb 时，可先用 apt-get download + dpkg-deb -x 解到
# `.xvfb-local/`（见 docs/archive/GPU_TERMINAL_SLINT_SPIKE.md），脚本会自动使用它。
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

display="${DISPLAY_NUM:-:77}"
if [[ "$display" == ":1" || "$display" == ":1.0" ]]; then
    echo "refusing to use the user's :1 display" >&2
    exit 2
fi
if [[ ! "$display" =~ ^:[0-9]+$ ]]; then
    echo "DISPLAY_NUM must look like :77 (got '$display')" >&2
    exit 2
fi

xvfb="${XTERVO_XVFB:-}"
if [[ -z "$xvfb" ]]; then
    if command -v Xvfb >/dev/null 2>&1; then
        xvfb="$(command -v Xvfb)"
    elif [[ -x "$root/.xvfb-local/usr/bin/Xvfb" ]]; then
        xvfb="$root/.xvfb-local/usr/bin/Xvfb"
    else
        echo "Xvfb not found: install it, set XTERVO_XVFB, or extract the deb to .xvfb-local/" >&2
        exit 2
    fi
fi

if [[ -e "/tmp/.X11-unix/X${display#:}" ]]; then
    echo "display $display is already in use; refusing to touch it" >&2
    exit 2
fi

out="${XTERVO_GPU_SPIKE_OUT:-$root/docs/evidence/gpu-spike}"
mkdir -p "$out"

log="${XTERVO_XVFB_LOG:-$out/xvfb.log}"
"$xvfb" "$display" -screen 0 1280x720x24 -nolisten tcp >"$log" 2>&1 &
xvfb_pid=$!
cleanup() {
    kill "$xvfb_pid" 2>/dev/null || true
    wait "$xvfb_pid" 2>/dev/null || true
}
trap cleanup EXIT

ready=0
for _ in $(seq 1 100); do
    if DISPLAY="$display" xdpyinfo >/dev/null 2>&1; then
        ready=1
        break
    fi
    if ! kill -0 "$xvfb_pid" 2>/dev/null; then
        echo "Xvfb exited before becoming ready; log: $log" >&2
        exit 3
    fi
    sleep 0.1
done
if [[ "$ready" != 1 ]]; then
    echo "Xvfb on $display did not become ready; log: $log" >&2
    exit 3
fi

echo "running gpu_terminal_spike on isolated display $display (xvfb=$xvfb)"
DISPLAY="$display" SLINT_WGPU_CPU=1 XTERVO_GPU_SPIKE_OUT="$out" \
    cargo run -p xtervo-app --example gpu_terminal_spike -- "$@"
