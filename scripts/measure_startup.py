#!/usr/bin/env python3
"""Phase 0 资源基线：冷启动到首帧上屏、空闲 RSS、空闲 CPU。

用法：
    python3 scripts/measure_startup.py --runs 5
    python3 scripts/measure_startup.py --rss-only
    python3 scripts/measure_startup.py --rss-only --bin target/debug/rusttabby-app

冷启动定义：spawn 进程 → X11 Damage 事件首次到达（像素真的进了 X server）。
应用内部同时打印 FIRST_DAMAGE_US（main 入口起算），两者之差就是 exec/动态链接开销。
"""
import argparse
import json
import os
import signal
import statistics
import subprocess
import sys
import time

BIN = "target/release/rusttabby-app"


def read_vm_rss_kb(pid: int) -> int:
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except FileNotFoundError:
        pass
    return -1


def read_cpu_ticks(pid: int) -> int:
    try:
        with open(f"/proc/{pid}/stat") as f:
            parts = f.read().split()
        return int(parts[13]) + int(parts[14])
    except (FileNotFoundError, IndexError):
        return -1


def children_rss_kb(pid: int) -> int:
    total = 0
    try:
        with open(f"/proc/{pid}/task/{pid}/children") as f:
            kids = f.read().split()
    except FileNotFoundError:
        return 0
    for kid in kids:
        rss = read_vm_rss_kb(int(kid))
        if rss > 0:
            total += rss
    return total


def measure_cold_start(bin_path: str, runs: int, env: dict, extra: list) -> dict:
    wall, internal = [], []
    for _ in range(runs):
        t0 = time.monotonic_ns()
        p = subprocess.Popen(
            [bin_path] + extra,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=env,
        )
        line = ""
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            line = p.stdout.readline()
            if not line or "FIRST_DAMAGE_US" in line or "FIRST_DAMAGE " in line:
                break
        t1 = time.monotonic_ns()
        p.wait(timeout=20)
        if "FIRST_DAMAGE_US" not in line:
            print(f"warn: 异常输出 {line!r}", file=sys.stderr)
            continue
        wall.append((t1 - t0) / 1e6)
        internal.append(int(line.split()[1]) / 1000.0)
    return {
        "runs": len(wall),
        "cold_start_ms": round(statistics.median(wall), 1) if wall else -1,
        "cold_start_min_ms": round(min(wall), 1) if wall else -1,
        "cold_start_max_ms": round(max(wall), 1) if wall else -1,
        "internal_first_frame_ms": round(statistics.median(internal), 1) if internal else -1,
    }


def measure_idle(bin_path: str, env: dict, extra: list, settle_s: float, window_s: float) -> dict:
    p = subprocess.Popen([bin_path] + extra, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    time.sleep(settle_s)
    rss = read_vm_rss_kb(p.pid)
    kids = children_rss_kb(p.pid)
    c0 = read_cpu_ticks(p.pid)
    time.sleep(window_s)
    c1 = read_cpu_ticks(p.pid)
    hz = os.sysconf("SC_CLK_TCK")
    cpu_pct = (c1 - c0) / hz / window_s * 100.0 if c0 >= 0 and c1 >= 0 else -1
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    return {
        "idle_rss_mb": round(rss / 1024.0, 1) if rss > 0 else -1,
        "shell_child_rss_mb": round(kids / 1024.0, 1),
        "idle_cpu_pct": round(cpu_pct, 2),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=32)
    ap.add_argument("--scrollback", type=int, default=2000)
    ap.add_argument("--backend", default="winit-software")
    ap.add_argument("--bin", default=BIN)
    ap.add_argument("--rss-only", action="store_true")
    args = ap.parse_args()

    env = dict(os.environ)
    env["SLINT_BACKEND"] = args.backend
    extra = [
        "--cols", str(args.cols),
        "--rows", str(args.rows),
        "--scrollback", str(args.scrollback),
    ]

    out = {"backend": args.backend, "grid": f"{args.cols}x{args.rows}"}
    if not args.rss_only:
        out.update(measure_cold_start(args.bin, args.runs, env, extra + ["--exit-after-first"]))
    out.update(measure_idle(args.bin, env, extra, 4.0, 3.0))
    print(json.dumps(out, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
