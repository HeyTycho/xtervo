#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# GPU 全局状态回归：两个窗口共享 WGPU30 device/queue；关闭 B 后 A 仍是 GPU
# ready（present_windows 只剩 A），且 A 的 PTY 输出还能让 native_frames 增长。
# 需要 CPU Vulkan adapter（本机 headless: SLINT_WGPU_CPU=1）；无 WGPU 时明确 FAIL。
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${E2E_OUT:-$ROOT/target/multi-window-gpu}
DISPLAY_NUM=${E2E_DISPLAY:-:153}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${XTERVO_BIN:-$TARGET_DIR/debug/xtervo}
TOKEN="mw-gpu-token"
APP_PID=""
XVFB_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
[ -x "$BIN" ] || fail "缺少 $BIN"
if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then fail "$DISPLAY_NUM 已被占用"; fi
COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB=${E2E_XVFB:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
[ -x "$XVFB" ] || fail "找不到 Xvfb"
rm -rf "$OUT"; mkdir -p "$OUT/logs" "$OUT/runtime" "$OUT/config"; chmod 700 "$OUT/runtime"
"$XVFB" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
export DISPLAY="$DISPLAY_NUM" XDG_RUNTIME_DIR="$OUT/runtime" XDG_CONFIG_HOME="$OUT/config" XDG_STATE_HOME="$OUT/state"
export XTERVO_CONFIG="$OUT/cfg.toml" XTERVO_LOG_DIR="$OUT/logs" XTERVO_SESSION="$OUT/session.json" XTERVO_PROFILES="$OUT/profiles.toml"
export XTERVO_DIAG_SOCKET="$OUT/live.sock" XTERVO_DIAG_TOKEN="$TOKEN" XTERVO_DIAG_ALLOW_WRITE=1 XTERVO_DIAG_ALLOW_DESTRUCTIVE=1
export XTERVO_MULTI_WINDOW_INPROC=1 XTERVO_MULTI_WINDOW_SECOND=1 XTERVO_GPU=1 SLINT_WGPU_CPU=1 XMODIFIERS=@im=none
unset SLINT_BACKEND || true
"$BIN" --cols 80 --rows 24 --shell /bin/sh >"$OUT/app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 300); do [ -S "$OUT/live.sock" ] && break; sleep 0.05; done
[ -S "$OUT/live.sock" ] || fail "live socket 未就绪"
sleep 3
python3 - "$OUT/live.sock" "$TOKEN" "$APP_PID" "$OUT" <<'PY'
import json, os, pathlib, socket, sys, time
sock_path, token, pid, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), pathlib.Path(sys.argv[4])
def call(cmd, args=None):
    s = socket.socket(socket.AF_UNIX); s.settimeout(4); s.connect(sock_path)
    s.sendall((json.dumps({"token": token, "cmd": cmd, "args": args or {}}) + "\n").encode())
    reply = json.loads(s.makefile().readline()); s.close()
    return reply
def ok(cmd, args=None):
    r = call(cmd, args); assert r.get("ok"), (cmd, r); return r["data"]
def gpu():
    return ok("snapshot")["gpu"]
def wait(pred, label, timeout=10):
    end = time.time() + timeout
    while time.time() < end:
        try:
            v = pred()
            if v: return v
        except Exception:
            pass
        time.sleep(0.2)
    raise AssertionError("timeout: " + label)

first = wait(lambda: (lambda g: g if g.get("present_ready") and g.get("native_ready") else None)(gpu()), "gpu ready")
assert first["present_windows"] == [1, 2], first
wait(lambda: gpu()["native_frames"] > 0, "native frames")
before = gpu()
ok("close_window", {"window_id": 2})
after = wait(lambda: (lambda g: g if g["present_windows"] == [1] else None)(gpu()), "gpu window removed")
assert after["present_ready"] and after["native_ready"], after
assert len(ok("list")["windows"]) == 1
pane = ok("list")["windows"][0]["panes"][0]["pane_id"]
ok("run", {"window_id": 1, "pane_id": pane, "command": "echo GPU_A_STILL_ALIVE"})
time.sleep(1.5)
later = gpu()
assert later["native_frames"] > after["native_frames"], (after, later)
report = {"before": before, "after_close": after, "later": later, "ok": True}
(out / "report.json").write_text(json.dumps(report, indent=2))
print("PASS GPU isolation: present_windows", before["present_windows"], "->", after["present_windows"],
      "native_frames", after["native_frames"], "->", later["native_frames"])
PY
[ $? -eq 0 ] || fail "GPU 断言失败（见 $OUT/app.log）"
grep -q 'window-2 rendering teardown：保留共享 device' "$OUT/app.log" || fail "缺少 GPU teardown 保留日志"
grep -q 'window-1 RenderingSetup：复用同一 WGPU30 device/queue' "$OUT/app.log" || fail "缺少共享 device 日志"
grep -E 'RenderingSetup|teardown|共享' "$OUT/app.log" >"$OUT/evidence.txt" || true
echo "evidence: $OUT"
echo "PASS multi-window GPU isolation"
