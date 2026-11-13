#!/usr/bin/env python3
"""2m dual-window in-process soak. Samples RSS/threads/FDs, drives both PTYs."""

import argparse
import json
import os
import pathlib
import socket
import time


class Live:
    def __init__(self, path, token):
        self.path, self.token = path, token

    def call(self, cmd, args=None):
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(3)
        s.connect(self.path)
        s.sendall((json.dumps({"token": self.token, "cmd": cmd, "args": args or {}}) + "\n").encode())
        line = s.makefile().readline()
        s.close()
        return json.loads(line)

    def ok(self, cmd, args=None):
        r = self.call(cmd, args)
        assert r.get("ok"), (cmd, r)
        return r["data"]


def sample(pid):
    status = pathlib.Path(f"/proc/{pid}/status").read_text()
    rss = next(int(l.split()[1]) for l in status.splitlines() if l.startswith("VmRSS:"))
    threads = next(int(l.split()[1]) for l in status.splitlines() if l.startswith("Threads:"))
    fds = len(list(pathlib.Path(f"/proc/{pid}/fd").iterdir()))
    return {"rss_kb": rss, "threads": threads, "fds": fds}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", required=True)
    ap.add_argument("--token", required=True)
    ap.add_argument("--pid", type=int, required=True)
    ap.add_argument("--seconds", type=int, default=120)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    live = Live(args.socket, args.token)

    deadline = time.time() + args.seconds
    samples = []
    baseline = None
    tick = 0
    while time.time() < deadline:
        data = live.ok("list")
        assert len(data["windows"]) == 2, data
        windows = sorted(data["windows"], key=lambda w: w["window_id"])
        # Alternate load across both windows, proving both stay live.
        for window in windows:
            focused = [p for p in window["panes"] if p.get("focused")]
            pane = (focused or window["panes"])[0]["pane_id"]
            marker = f"MW_SOAK_{tick}_{window['window_id']}"
            reply = live.call(
                "run",
                {
                    "window_id": window["window_id"],
                    "pane_id": pane,
                    "command": f"echo {marker}; i=0; while [ $i -lt 20 ]; do echo line-$i; i=$((i+1)); done",
                },
            )
            assert reply.get("ok"), reply
        s = sample(args.pid)
        s["t"] = round(time.time() - (deadline - args.seconds), 1)
        s["windows"] = len(data["windows"])
        samples.append(s)
        if tick == 0:
            baseline = s
        tick += 1
        time.sleep(5)

    final = samples[-1]
    rss_growth = final["rss_kb"] - baseline["rss_kb"]
    thread_growth = final["threads"] - baseline["threads"]
    fd_growth = final["fds"] - baseline["fds"]
    report = {
        "seconds": args.seconds,
        "samples": samples,
        "baseline": baseline,
        "final": final,
        "rss_growth_kb": rss_growth,
        "thread_growth": thread_growth,
        "fd_growth": fd_growth,
    }
    (out / "samples.json").write_text(json.dumps(report, indent=2))
    csv = out / "samples.csv"
    with csv.open("w") as f:
        f.write("t,rss_kb,threads,fds,windows\n")
        for s in samples:
            f.write(f"{s['t']},{s['rss_kb']},{s['threads']},{s['fds']},{s['windows']}\n")
    assert os.path.isdir(f"/proc/{args.pid}"), "app died during soak"
    assert len(live.ok("list")["windows"]) == 2, "a window disappeared"
    assert rss_growth <= 80 * 1024, f"RSS grew {rss_growth} KiB > 80 MiB"
    assert thread_growth <= 8, f"threads grew {thread_growth}"
    assert fd_growth <= 24, f"fds grew {fd_growth}"
    print(
        f"PASS 2m dual-window soak: rss_growth={rss_growth}KiB "
        f"threads={baseline['threads']}->{final['threads']} "
        f"fds={baseline['fds']}->{final['fds']}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
