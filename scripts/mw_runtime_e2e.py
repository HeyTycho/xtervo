#!/usr/bin/env python3
"""Black-box multi-window (in-process) acceptance checks.

Driven by ``scripts/e2e_multi_window_inproc.sh``; talks to the app's live
control socket and X11 directly. No mocks: every assertion uses the real
process started by the shell wrapper.
"""

import argparse
import json
import os
import pathlib
import re
import socket
import subprocess
import sys
import time


class Live:
    def __init__(self, path: str, token: str):
        self.path = path
        self.token = token

    def call(self, cmd: str, args: dict | None = None):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(3.0)
        sock.connect(self.path)
        payload = {"token": self.token, "cmd": cmd, "args": args or {}}
        sock.sendall((json.dumps(payload) + "\n").encode())
        line = sock.makefile().readline()
        sock.close()
        if not line:
            raise AssertionError(f"empty response for {cmd}")
        reply = json.loads(line)
        if not reply.get("ok"):
            return reply
        return reply

    def ok(self, cmd: str, args: dict | None = None):
        reply = self.call(cmd, args)
        assert reply.get("ok"), f"{cmd} failed: {reply}"
        return reply["data"]


def list_windows(live: Live):
    return live.ok("list")


def window_by_id(data, window_id):
    for window in data["windows"]:
        if window["window_id"] == window_id:
            return window
    raise AssertionError(f"window {window_id} missing: {data}")


def wait_for(pred, timeout=8.0, label="condition"):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = pred()
        if last:
            return last
        time.sleep(0.1)
    raise AssertionError(f"timeout waiting for {label}; last={last!r}")


def x11_windows(display: str):
    out = subprocess.run(
        ["xwininfo", "-root", "-tree"],
        env={**os.environ, "DISPLAY": display},
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    found = {}
    for line in out.splitlines():
        match = re.search(r'(0x[0-9a-f]+) "([^"]*)"', line)
        if not match:
            continue
        win, title = match.group(1), match.group(2)
        found.setdefault(title, win)
    return found


def count_processes(needle: str):
    count = 0
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "cmdline").read_bytes()
        except OSError:
            continue
        if needle.encode() in raw:
            count += 1
    return count


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", required=True)
    ap.add_argument("--token", required=True)
    ap.add_argument("--xwin", required=True)
    ap.add_argument("--display", required=True)
    ap.add_argument("--pid", type=int, required=True)
    ap.add_argument("--log", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--plugin", default="")
    args = ap.parse_args()

    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    report = {"checks": [], "ok": False}

    def check(name, detail=None):
        report["checks"].append({"name": name, "detail": detail})
        print(f"PASS {name}" + (f": {detail}" if detail else ""))

    live = Live(args.socket, args.token)

    data = wait_for(
        lambda: (lambda d: d if len(d.get("windows", [])) == 2 else None)(
            (live.call("list") or {}).get("data") or {}
        ),
        label="two windows",
    )
    windows = data["windows"]
    assert data.get("active_window_id") in [w["window_id"] for w in windows]
    ids = sorted(w["window_id"] for w in windows)
    assert ids == [1, 2], ids
    panes = [p["pane_id"] for w in windows for p in w["panes"]]
    assert len(panes) == len(set(panes)), f"pane ids collide across windows: {panes}"
    check("two_mainwindows_one_process", {"windows": ids, "panes": panes})

    # Independent tab creation: window 1 gets a second tab, window 2 stays at 1.
    live.ok("new_tab", {"window_id": 1})
    data = list_windows(live)
    assert window_by_id(data, 1)["tabs"] == 2, data
    assert window_by_id(data, 2)["tabs"] == 1, data
    live.ok("new_tab", {"window_id": 2})
    data = list_windows(live)
    assert window_by_id(data, 1)["tabs"] == 2, data
    assert window_by_id(data, 2)["tabs"] == 2, data
    check("independent_tabs", {w["window_id"]: w["tabs"] for w in data["windows"]})

    def focused_pane(window):
        focused = [p for p in window["panes"] if p.get("focused")]
        return (focused or window["panes"])[0]["pane_id"]

    pane1 = focused_pane(window_by_id(data, 1))
    pane2 = focused_pane(window_by_id(data, 2))
    assert pane1 != pane2
    live.ok("run", {"window_id": 1, "pane_id": pane1, "command": "echo MW_A_MARK"})
    live.ok("run", {"window_id": 2, "pane_id": pane2, "command": "echo MW_B_MARK"})
    time.sleep(2.0)
    out_a = live.ok("output", {"window_id": 1, "pane_id": pane1})["text"]
    out_b = live.ok("output", {"window_id": 2, "pane_id": pane2})["text"]
    assert "MW_A_MARK" in out_a, out_a
    assert "MW_B_MARK" not in out_a, out_a
    assert "MW_B_MARK" in out_b, out_b
    assert "MW_A_MARK" not in out_b, out_b
    check("independent_pty_output", {"pane_a": pane1, "pane_b": pane2})

    cross = live.call("output", {"window_id": 1, "pane_id": pane2})
    assert not cross.get("ok"), f"cross-window pane lookup must fail: {cross}"
    check("pane_route_not_global", cross.get("error"))

    wins = x11_windows(args.display)
    win_a = wins.get("Tervo")
    win_b = wins.get("Tervo — 2")
    assert win_a and win_b, wins
    def active():
        try:
            return live.ok("list").get("active_window_id")
        except Exception:
            return None
    subprocess.run([args.xwin, win_b, "--focus"], check=False)
    wait_for(lambda: active() == 2, label="active window 2")
    subprocess.run([args.xwin, win_a, "--focus"], check=False)
    wait_for(lambda: active() == 1, label="active window 1")
    check("focus_routes_active_window", {"a": win_a, "b": win_b})

    # Close B via MCP live_close_window (window_id route). A must stay alive/usable.
    live.ok("close_window", {"window_id": 2})
    wait_for(lambda: len(live.ok("list")["windows"]) == 1, label="window B closed")
    assert os.kill(args.pid, 0) is None
    data = live.ok("list")
    assert data["windows"][0]["window_id"] == 1, data
    assert data["windows"][0]["tabs"] == 2, data
    live.ok("new_tab", {"window_id": 1})
    assert live.ok("list")["windows"][0]["tabs"] == 3
    check("close_b_keeps_a", {"remaining": 1, "tabs": 3})

    # 反向：开新窗口 C，关掉 primary(1)，C 必须继续可用（A 的 PTY 被回收）。
    new_id = live.ok("new_window")["window_id"]
    assert new_id == 3, new_id
    wait_for(lambda: len(live.ok("list")["windows"]) == 2, label="window C created")
    live.ok("close_window", {"window_id": 1})
    data = wait_for(
        lambda: (lambda d: d if len(d["windows"]) == 1 else None)(live.ok("list")),
        label="primary closed",
    )
    assert data["windows"][0]["window_id"] == new_id, data
    assert os.kill(args.pid, 0) is None
    live.ok("new_tab", {"window_id": new_id})
    assert live.ok("list")["windows"][0]["tabs"] == 2
    check("close_primary_keeps_c", {"remaining": new_id, "tabs": 2})

    # Plugin host / MCP socket are per-process, not per-window.
    if args.plugin:
        count = count_processes(args.plugin)
        assert count == 1, f"plugin process count={count}, expected 1"
        check("one_plugin_process", {"path": args.plugin, "count": count})
    log_text = pathlib.Path(args.log).read_text(errors="replace")
    ready = log_text.count("live socket ready")
    assert ready == 1, f"MCP live socket started {ready} times"
    sockets = list(pathlib.Path(args.socket).parent.glob("*.sock"))
    assert len(sockets) == 1, sockets
    check("one_mcp_socket", {"ready_log_lines": ready, "sockets": [str(s) for s in sockets]})

    report["ok"] = True
    (out / "report.json").write_text(json.dumps(report, indent=2))
    print("PASS multi-window in-process e2e")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssertionError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
