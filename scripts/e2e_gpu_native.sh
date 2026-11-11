#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# TERVO_GPU_NATIVE=1 vertical-slice acceptance on an isolated Xvfb display.
#
# Runs three real app processes:
#   1. cpu      - SLINT_BACKEND=winit-software, CPU terminal framebuffer
#   2. gpu      - TERVO_GPU_NATIVE=1 + SLINT_WGPU_CPU=1 (Slint WGPU30)
#   3. fallback - TERVO_GPU_NATIVE=1 but no WGPU device (software backend),
#                 must stay on the CPU path without claiming GPU frames
#
# Then diffs the terminal region of take_snapshot() between CPU and GPU,
# checks the GPU log for draw calls / atlas uploads / frame counters, and
# verifies the raw RGBA readback is opaque (PPM drops alpha).
#
# Never uses the user's :1 display. Defaults to :77 and refuses a busy display.
#
# Usage:
#   TERVO_BIN=target/debug/tervo scripts/e2e_gpu_native.sh
#   TERVO_BIN=/abs/path/tervo DISPLAY_NUM=:78 scripts/e2e_gpu_native.sh
#
# Output: $TERVO_GPU_NATIVE_OUT (default <workspace>/target/gpu-native-e2e)
#   cpu/ gpu/ fallback/  app.log + snap.before.ppm
#   gpu/offscreen-*.ppm + .rgba   direct GPU target readback (alpha evidence)
#   summary.txt / diff.txt / *.png
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
if [[ -e "/tmp/.X11-unix/X${display#:}" ]]; then
    echo "display $display is already in use; refusing to touch it" >&2
    exit 2
fi

out="${TERVO_GPU_NATIVE_OUT:-$root/target/gpu-native-e2e}"
rm -rf "$out"
mkdir -p "$out"/{cpu,gpu,fallback,config,cache}

bin="${TERVO_BIN:-$root/target/debug/tervo}"
if [[ ! -x "$bin" ]]; then
    echo "TERVO_BIN=$bin is not executable; build it first (cargo build -p tervo-app --bin tervo)" >&2
    exit 2
fi

# Worktrees do not carry the gitignored .tmp-gpu-agent/.perf downloads;
# resolve them from the main worktree root as well.
common_root="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
xvfb="${TERVO_XVFB:-}"
if [[ -z "$xvfb" ]]; then
    for candidate in \
        "$(command -v Xvfb 2>/dev/null || true)" \
        "$root/.xvfb-local/usr/bin/Xvfb" \
        "$root/.perf/xvfb/usr/bin/Xvfb" \
        "$root/.tmp-gpu-agent/xvfb/usr/bin/Xvfb" \
        "$common_root/.xvfb-local/usr/bin/Xvfb" \
        "$common_root/.perf/xvfb/usr/bin/Xvfb" \
        "$common_root/.tmp-gpu-agent/xvfb/usr/bin/Xvfb"; do
        if [[ -n "$candidate" && -x "$candidate" ]]; then
            xvfb="$candidate"
            break
        fi
    done
fi
if [[ -z "$xvfb" ]]; then
    echo "Xvfb not found: install it or set TERVO_XVFB" >&2
    exit 2
fi

cat >"$out/fixture.sh" <<'FIXTURE'
#!/bin/sh
printf '\033[2J\033[H\033[?25l'
printf '\033[1;97mTERVO GPU NATIVE FIXTURE\033[0m\n'
printf 'ascii: The quick brown fox 0123456789\n'
printf '\033[31mred\033[0m \033[42mgreen-bg\033[0m \033[4munderline\033[0m \033[2mdim\033[0m \033[8mhidden\033[0m\n'
printf 'CJK: \344\275\240\345\245\275\344\270\226\347\225\214 \346\227\245\346\234\254\350\252\236\n'
printf 'symbols: \342\230\203 \342\230\205 \342\231\245\n'
printf '\033[41m  RED-BG  \033[0m \033[44m  BLUE-BG  \033[0m END\n'
# 80x6 solid red Sixel: one below-text image, parsed after the first frame.
printf '\033Pq#0;2;100;0;0#0~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~\033\\'
printf '\nIMAGE-AFTER\n'
sleep 2
printf 'PING\n'
exec sleep 300
FIXTURE
chmod +x "$out/fixture.sh"

"$xvfb" "$display" -screen 0 1600x900x24 -nolisten tcp >"$out/xvfb.log" 2>&1 &
xvfb_pid=$!
cleanup() {
    kill "$xvfb_pid" 2>/dev/null || true
    wait "$xvfb_pid" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 100); do
    DISPLAY="$display" xdpyinfo >/dev/null 2>&1 && break
    if ! kill -0 "$xvfb_pid" 2>/dev/null; then
        echo "Xvfb exited before becoming ready; log: $out/xvfb.log" >&2
        exit 3
    fi
    sleep 0.1
done
DISPLAY="$display" xdpyinfo >/dev/null 2>&1 || {
    echo "Xvfb on $display did not become ready; log: $out/xvfb.log" >&2
    exit 3
}

run_app() {
    local mode="$1"
    shift
    env -u SLINT_BACKEND -u TERVO_GPU -u TERVO_GPU_NATIVE \
        DISPLAY="$display" SLINT_WGPU_CPU=1 \
        TERVO_CONFIG="$out/config/cfg-$mode.toml" \
        TERVO_LOG_DIR="$out/$mode" \
        XDG_CONFIG_HOME="$out/config" \
        XDG_DATA_HOME="$out/config" \
        XDG_CACHE_HOME="$out/cache" \
        "$@" \
        timeout 60 "$bin" --shell "$out/fixture.sh" --cols 80 --rows 24 \
        --title "gpu-native-$mode" >"$out/$mode/app.log" 2>&1
    local rc=$?
    if [[ $rc -ne 0 ]]; then
        echo "app mode=$mode exited rc=$rc; see $out/$mode/app.log" >&2
        exit 4
    fi
}

echo "running CPU baseline on isolated display $display"
run_app cpu SLINT_BACKEND=winit-software \
    TERVO_GPU_SNAPSHOT="$out/cpu/snap" TERVO_GPU_SNAPSHOT_EXIT=1

echo "running TERVO_GPU_NATIVE=1 (WGPU30)"
# llvmpipe can take ~2.5s to present its first WGPU frame; give the imported
# texture time to be sampled before take_snapshot.
run_app gpu TERVO_GPU_NATIVE=1 TERVO_GPU_NATIVE_DEBUG=1 \
    TERVO_GPU_SNAPSHOT_DELAY_MS=6000 \
    TERVO_GPU_NATIVE_DUMP="$out/gpu/offscreen" \
    TERVO_GPU_SNAPSHOT="$out/gpu/snap" TERVO_GPU_SNAPSHOT_EXIT=1

echo "running failure path (GPU requested, no WGPU device)"
run_app fallback TERVO_GPU_NATIVE=1 SLINT_BACKEND=winit-software \
    TERVO_GPU_SNAPSHOT="$out/fallback/snap" TERVO_GPU_SNAPSHOT_EXIT=1

fail=0
fail_msg() { echo "FAIL: $*" >&2; fail=1; }

# --- GPU path must be real and counted -------------------------------------
gpu_snapshot_line=$(grep -h "\[gpu-snapshot\] path=.*gpu/snap" "$out/gpu/app.log" || true)
[[ -n "$gpu_snapshot_line" ]] || fail_msg "GPU run has no take_snapshot log line"

native_frames=$(sed -n 's/.*gpu_native_frames=\([0-9]*\).*/\1/p' <<<"$gpu_snapshot_line" | head -1)
native_draws=$(sed -n 's/.* draw_calls=\([0-9]*\).*/\1/p' <<<"$gpu_snapshot_line" | head -1)
native_nonbg=$(sed -n 's/.* nonbg=\([0-9]*\).*/\1/p' <<<"$gpu_snapshot_line" | head -1)
if [[ -z "${native_frames:-}" || "$native_frames" -lt 1 ]]; then
    fail_msg "GPU run produced no GPU-native frames (gpu_native_frames=${native_frames:-missing})"
fi
if [[ -z "${native_draws:-}" || "$native_draws" -lt 1 ]]; then
    fail_msg "GPU run reported no draw calls"
fi
if [[ -z "${native_nonbg:-}" || "$native_nonbg" -lt 1000 ]]; then
    fail_msg "GPU snapshot is effectively empty (nonbg=${native_nonbg:-missing})"
fi
grep -q "\[gpu-native\] pane=.*draw_calls=" "$out/gpu/app.log" || \
    fail_msg "GPU run has no [gpu-native] frame log"
grep -q "image_quads=1" "$out/gpu/app.log" || \
    fail_msg "GPU Scene did not compose the Frame.images Sixel quad"
grep -q "image_uploads_total=1" "$out/gpu/app.log" || \
    fail_msg "GPU image registry did not upload the source exactly once"
# Cumulative registry uploads (snapshot line), independent of which frame
# happens to be the last one.
gpu_snapshot_line=$(grep -h "\[gpu-snapshot\] path=.*gpu/snap" "$out/gpu/app.log" || true)
native_image_uploads=$(sed -n 's/.* image_uploads=\([0-9]*\).*/\1/p' <<<"$gpu_snapshot_line" | head -1)
if [[ -z "${native_image_uploads:-}" || "$native_image_uploads" -lt 1 ]]; then
    fail_msg "GPU image registry reported no cumulative uploads"
fi
grep -q "gpu-native\] 失败" "$out/gpu/app.log" && \
    fail_msg "GPU run reported an unexpected failure fallback"

# Direct offscreen readback: PPM + raw RGBA (alpha proof)
rgba=$(find "$out/gpu" -maxdepth 1 -name 'offscreen-*.ppm.rgba' | head -1)
if [[ -z "$rgba" ]]; then
    fail_msg "no raw RGBA readback artifact (TERVO_GPU_NATIVE_DUMP)"
else
    alpha=$(python3 - "$rgba" <<'PY'
import sys
from pathlib import Path
data = Path(sys.argv[1]).read_bytes()
alphas = data[3::4]
print(f"min={min(alphas)} max={max(alphas)} non255={sum(a != 255 for a in alphas)} count={len(alphas)}")
PY
)
    echo "offscreen alpha: $alpha" | tee "$out/alpha.txt"
    grep -q "min=255 max=255 non255=0" "$out/alpha.txt" || \
        fail_msg "GPU target has non-opaque pixels: $alpha"
fi

# --- Failure path must stay CPU, not fake GPU ------------------------------
if grep -q "\[gpu-native\] pane=" "$out/fallback/app.log"; then
    fail_msg "fallback run claimed GPU-native frames without a WGPU device"
fi
if grep -q "gpu_native_frames=[1-9]" "$out/fallback/app.log"; then
    fail_msg "fallback snapshot reported GPU-native frames"
fi

# --- Pixel diff (terminal region only; tab bar excluded) -------------------
if ! python3 - "$out" <<'PY' | tee "$out/diff.txt"
import struct
import sys
import zlib
from pathlib import Path

out = Path(sys.argv[1])
TOP = 32  # TABBAR_H: terminal starts below the tab bar

def read_ppm(path):
    data = path.read_bytes()
    assert data[:2] == b"P6", f"{path} is not binary PPM"
    fields = []
    i = 2
    while len(fields) < 3:
        while i < len(data) and data[i:i+1].isspace():
            i += 1
        j = i
        while j < len(data) and not data[j:j+1].isspace():
            j += 1
        fields.append(int(data[i:j]))
        i = j
    i += 1
    w, h, _ = fields
    return w, h, data[i:i + w * h * 3]

def write_png(path, w, h, rgb):
    raw = b"".join(b"\x00" + rgb[y*w*3:(y+1)*w*3] for y in range(h))
    def chunk(tag, payload):
        body = struct.pack(">I", len(payload)) + tag + payload
        return body + struct.pack(">I", zlib.crc32(tag + payload) & 0xffffffff)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 6))
        + chunk(b"IEND", b"")
    )

def red_pixels(rgb):
    return sum(
        1 for k in range(0, len(rgb), 3)
        if rgb[k] > 180 and rgb[k+1] < 90 and rgb[k+2] < 90
    )

def region_stats(cpu, gpu):
    w, h = cpu[0], cpu[1]
    a, b = cpu[2], gpu[2]
    total = diff = sum_abs = over8 = 0
    ink_a = ink_b = ink_both = 0
    for y in range(TOP, h):
        row = y * w * 3
        for x in range(w):
            i = row + x * 3
            pa, pb = a[i:i+3], b[i:i+3]
            d = abs(pa[0]-pb[0]) + abs(pa[1]-pb[1]) + abs(pa[2]-pb[2])
            total += 1
            sum_abs += d
            diff += d != 0
            over8 += d > 8
            ca = sum(pa) < 390
            cb = sum(pb) < 390
            ink_a += ca
            ink_b += cb
            ink_both += ca and cb
    iou = ink_both / max(1, ink_a + ink_b - ink_both)
    return {
        "w": w, "h": h - TOP, "total": total, "diff": diff,
        "ratio": diff / total, "mae": sum_abs / (total * 3), "over8": over8,
        "ink_cpu": ink_a, "ink_gpu": ink_b, "ink_iou": iou,
    }

cpu = read_ppm(out / "cpu/snap.before.ppm")
gpu = read_ppm(out / "gpu/snap.before.ppm")
fallback = read_ppm(out / "fallback/snap.before.ppm")
if cpu[:2] != gpu[:2] or cpu[:2] != fallback[:2]:
    print(f"FAIL: snapshot sizes differ cpu={cpu[:2]} gpu={gpu[:2]} fallback={fallback[:2]}")
    sys.exit(1)

write_png(out / "cpu.png", cpu[0], cpu[1], cpu[2])
write_png(out / "gpu.png", gpu[0], gpu[1], gpu[2])
write_png(out / "fallback.png", fallback[0], fallback[1], fallback[2])

red_cpu = red_pixels(cpu[2])
red_gpu = red_pixels(gpu[2])
print(f"sixel_red_pixels: cpu={red_cpu} gpu={red_gpu}")
stats = region_stats(cpu, gpu)
print(
    "terminal_diff: "
    f"size={stats['w']}x{stats['h']} differing={stats['diff']} "
    f"ratio={stats['ratio']:.4%} mae={stats['mae']:.3f} "
    f"pixels_over_8={stats['over8']} ink_cpu={stats['ink_cpu']} "
    f"ink_gpu={stats['ink_gpu']} ink_iou={stats['ink_iou']:.3f}"
)
fb = region_stats(cpu, fallback)
print(
    "fallback_vs_cpu: "
    f"differing={fb['diff']} ratio={fb['ratio']:.4%} mae={fb['mae']:.3f}"
)

ok = True
if stats["ratio"] > 0.05:
    print(f"FAIL: GPU vs CPU terminal differs on {stats['ratio']:.2%} pixels (>5%)")
    ok = False
if stats["mae"] > 2.0:
    print(f"FAIL: GPU vs CPU terminal MAE {stats['mae']:.3f} > 2.0")
    ok = False
if stats["ink_iou"] < 0.70:
    print(f"FAIL: GPU vs CPU ink IoU {stats['ink_iou']:.3f} < 0.70")
    ok = False
if fb["ratio"] > 0.01:
    print(f"FAIL: software failure-path snapshot differs from CPU on {fb['ratio']:.2%} pixels")
    ok = False
# The red Sixel block is 80x6 = 480 px; the fixture also has a red [41m
# background and a theme-coloured cursor, so use a loose lower bound plus a
# relative tolerance (cursor blink phase / subpixel AA differ per run).
if red_cpu < 1000:
    print(f"FAIL: CPU baseline did not draw the red Sixel image ({red_cpu} red pixels)")
    ok = False
if red_gpu < 1000:
    print(f"FAIL: GPU path did not draw the red Sixel image ({red_gpu} red pixels)")
    ok = False
if abs(red_cpu - red_gpu) > max(300, red_cpu // 3):
    print(f"FAIL: GPU/CPU Sixel image pixels differ too much (cpu={red_cpu} gpu={red_gpu})")
    ok = False
sys.exit(0 if ok else 1)
PY
then
    fail=1
fi

{
    echo "display=$display"
    echo "bin=$bin"
    echo "gpu_snapshot=$gpu_snapshot_line"
    echo "native_frames=$native_frames native_draw_calls=$native_draws nonbg=$native_nonbg"
    echo "artifacts=$out"
} | tee "$out/summary.txt"

if [[ $fail -ne 0 ]]; then
    echo "FAIL gpu-native e2e (artifacts: $out)" >&2
    exit 1
fi
echo "PASS gpu-native e2e: CPU/GPU/fallback snapshots + counters + alpha (artifacts: $out)"
