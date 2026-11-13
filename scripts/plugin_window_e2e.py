#!/usr/bin/env python3
"""Black-box plugin v3 per-window routing acceptance.

Driven by ``scripts/e2e_plugin_window_routing.sh``. Talks to the real app
through the live control socket and XTest; the plugin is a real Python v3
subprocess. No mocks.
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

from PIL import Image


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
        return json.loads(line)

    def ok(self, cmd: str, args: dict | None = None):
        reply = self.call(cmd, args)
        assert reply.get("ok"), f"{cmd} failed: {reply}"
        return reply["data"]


def wait_for(pred, timeout=10.0, label="condition", interval=0.05):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = pred()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timeout waiting for {label}; last={last!r}")


def count_processes(needle: str):
    """Count plugin processes.

    The supervisor starts the script with ``cwd = plugin dir`` and a shebang;
    the exact argv shape depends on /usr/bin/env, so match either the cmdline
    or the process cwd + main.py.
    """
    plugin_dir = os.path.realpath(os.path.dirname(needle))
    own_pid = os.getpid()
    count = 0
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit() or entry.name == str(own_pid):
            continue
        try:
            raw = (entry / "cmdline").read_bytes()
        except OSError:
            continue
        # 本 driver 的 argv 也包含插件路径；只认真正的解释器进程。
        if b"plugin_window_e2e.py" in raw:
            continue
        if needle.encode() in raw:
            count += 1
            continue
        try:
            cwd = os.readlink(entry / "cwd")
        except OSError:
            continue
        if os.path.realpath(cwd) == plugin_dir and b"main.py" in raw:
            count += 1
    return count


class X11:
    def __init__(self, display: str, xwin: str, xclick: str, xkey: str):
        self.display = display
        self.xwin = xwin
        self.xclick = xclick
        self.xkey = xkey
        self.env = {**os.environ, "DISPLAY": display}

    def run(self, *args, **kwargs):
        return subprocess.run(list(args), env=self.env, capture_output=True, text=True, **kwargs)

    def windows(self):
        out = self.run("xwininfo", "-root", "-tree").stdout
        found = {}
        for line in out.splitlines():
            match = re.search(r'(0x[0-9a-f]+) "([^"]*)"', line)
            if not match:
                continue
            win, title = match.group(1), match.group(2)
            found.setdefault(title, win)
        return found

    def geometry(self, win: str):
        out = self.run("xwininfo", "-id", win).stdout
        values = {}
        for key in ("Absolute upper-left X", "Absolute upper-left Y", "Width", "Height"):
            match = re.search(rf"{re.escape(key)}:\s+(\d+)", out)
            if match:
                values[key] = int(match.group(1))
        return values

    def focus(self, win: str):
        self.run(self.xwin, win, "--focus")

    def click(self, x: int, y: int):
        self.run(self.xclick, str(x), str(y))

    def key(self, *args):
        self.run(self.xkey, *args)


def shot(env, win: str, path: pathlib.Path):
    xwd = path.with_suffix(".xwd")
    png = path.with_suffix(".png")
    subprocess.run(["xwd", "-silent", "-id", win, "-out", str(xwd)], env=env, check=True)
    subprocess.run(
        ["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-f", "xwd_pipe",
         "-i", str(xwd), str(png)],
        env=env,
        check=True,
    )
    xwd.unlink(missing_ok=True)
    return png


def approval_allow_point(png: pathlib.Path):
    """Locate the top-right approval card by its danger border color."""
    image = Image.open(png).convert("RGB")
    width, height = image.size
    pixels = image.load()
    points = []
    for y in range(0, min(height, 520)):
        for x in range(max(0, width - 520), width):
            r, g, b = pixels[x, y]
            if r > 150 and r > g + 50 and r > b + 30:
                points.append((x, y))
    if not points:
        raise AssertionError(f"approval card border not found in {png}")
    xs = [point[0] for point in points]
    ys = [point[1] for point in points]
    left, right = min(xs), max(xs)
    top, bottom = min(ys), max(ys)
    if right - left < 300 or bottom - top < 80:
        raise AssertionError(f"approval card too small: {(left, top, right, bottom)}")
    # Allow once = 左半边按钮；按钮行距卡片底部约 12px padding。
    return left + (right - left) // 4, bottom - 22


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", required=True)
    ap.add_argument("--token", required=True)
    ap.add_argument("--app-pid", type=int, required=True)
    ap.add_argument("--display", required=True)
    ap.add_argument("--xwin", required=True)
    ap.add_argument("--xclick", required=True)
    ap.add_argument("--xkey", required=True)
    ap.add_argument("--log", required=True)
    ap.add_argument("--plugin-log", required=True)
    ap.add_argument("--plugin", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    log_path = pathlib.Path(args.log)
    plugin_log = pathlib.Path(args.plugin_log)
    report = {"checks": [], "ok": False}

    def check(name, detail=None):
        report["checks"].append({"name": name, "detail": detail})
        print(f"PASS {name}" + (f": {detail}" if detail else ""))

    def log_text():
        return log_path.read_text(errors="replace")

    def wait_log(needle, timeout=10.0, label=None):
        return wait_for(
            lambda: needle in log_text(),
            timeout=timeout,
            label=label or f"log {needle}",
        )

    def plugin_entries():
        if not plugin_log.exists():
            return []
        entries = []
        for line in plugin_log.read_text(errors="replace").splitlines():
            try:
                entries.append(json.loads(line))
            except json.JSONDecodeError:
                continue
        return entries

    def wait_plugin(predicate, timeout=10.0, label="plugin condition"):
        return wait_for(
            lambda: next((entry for entry in plugin_entries() if predicate(entry)), None),
            timeout=timeout,
            label=label,
        )

    live = Live(args.socket, args.token)
    x11 = X11(args.display, args.xwin, args.xclick, args.xkey)

    # ---- 两个窗口 + 一个插件进程
    data = wait_for(
        lambda: (lambda d: d if len(d.get("windows", [])) == 2 else None)(
            (live.call("list") or {}).get("data") or {}
        ),
        label="two windows",
    )
    ids = sorted(window["window_id"] for window in data["windows"])
    assert ids == [1, 2], ids
    wins = wait_for(
        lambda: (lambda w: w if "Tervo" in w and "Tervo — 2" in w else None)(x11.windows()),
        label="two X11 windows",
    )
    win1, win2 = wins["Tervo"], wins["Tervo — 2"]
    wait_log("initialize/initialized", timeout=15, label="plugin handshake")
    try:
        plugin_processes = wait_for(
            lambda: (lambda n: n if n == 1 else None)(count_processes(args.plugin)),
            timeout=5.0,
            label="one plugin process",
        )
    except AssertionError:
        for entry in pathlib.Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                raw = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
                cwd = os.readlink(entry / "cwd")
            except OSError:
                continue
            if "main.py" in raw or "winroute" in raw or "winroute" in cwd:
                print(f"DEBUG proc {entry.name} cwd={cwd} cmd={raw}", file=sys.stderr)
        raise
    check("one_plugin_process_two_windows", {"windows": ids, "processes": plugin_processes})

    def pane_ids():
        listing = live.ok("list")
        out = {}
        for window in listing["windows"]:
            focused = [pane for pane in window["panes"] if pane.get("focused")]
            out[window["window_id"]] = (focused or window["panes"])[0]["pane_id"]
        return out

    panes = pane_ids()

    def prepare_window(win: str, marker: str):
        x11.focus(win)
        time.sleep(0.2)
        geom = x11.geometry(win)
        x11.click(geom["Absolute upper-left X"] + 120, geom["Absolute upper-left Y"] + 90)
        time.sleep(0.2)
        x11.key(f"echo {marker}")
        x11.key("--key", "enter")
        time.sleep(0.8)

    prepare_window(win1, "W1_MARK")
    prepare_window(win2, "W2_MARK")

    def run_palette(win: str, query: str):
        x11.focus(win)
        time.sleep(0.2)
        geom = x11.geometry(win)
        x11.click(geom["Absolute upper-left X"] + 120, geom["Absolute upper-left Y"] + 90)
        time.sleep(0.2)
        x11.key("p", "--ctrl", "--shift")
        time.sleep(0.5)
        x11.key(query)
        time.sleep(0.5)
        x11.key("--key", "enter")
        time.sleep(0.5)

    def approve(win: str, tag: str):
        png = shot(x11.env, win, out / "shots" / f"approval-{tag}.png")
        x, y = approval_allow_point(png)
        geom = x11.geometry(win)
        x11.click(geom["Absolute upper-left X"] + x, geom["Absolute upper-left Y"] + y)
        time.sleep(0.4)

    # ---- 窗口 1：命令 → ask 只出现在窗口 1 → Allow → 读取窗口 1 pane
    run_palette(win1, "read window")
    wait_log("[plugin-v3] dev.tervo.winroute: ask terminal.read window=Some(1)", label="window 1 ask")
    assert len(live.ok("list")["windows"]) == 2
    approve(win1, "w1")
    wait_plugin(
        lambda entry: entry.get("kind") == "read" and entry.get("window") == 1,
        label="window 1 read",
    )
    read1 = [
        entry
        for entry in plugin_entries()
        if entry.get("kind") == "read" and entry.get("window") == 1
    ][-1]
    assert "W1_MARK" in read1["text"], read1
    assert "W2_MARK" not in read1["text"], read1
    wait_log("terminal.read applied window=window-1", label="window 1 read applied")
    check("window1_read_routes_to_window1", {"bytes": len(read1["text"])})

    # ---- 窗口 2：同一插件进程，ask/读取都路由到窗口 2
    run_palette(win2, "read window")
    wait_log("[plugin-v3] dev.tervo.winroute: ask terminal.read window=Some(2)", label="window 2 ask")
    approve(win2, "w2")
    wait_plugin(
        lambda entry: entry.get("kind") == "read" and entry.get("window") == 2,
        label="window 2 read",
    )
    read2 = [
        entry
        for entry in plugin_entries()
        if entry.get("kind") == "read" and entry.get("window") == 2
    ][-1]
    assert "W2_MARK" in read2["text"], read2
    assert "W1_MARK" not in read2["text"], read2
    wait_log("terminal.read applied window=window-2", label="window 2 read applied")
    commands = [entry for entry in plugin_entries() if entry.get("kind") == "command"]
    assert any(entry.get("window") == 1 for entry in commands), commands
    assert any(entry.get("window") == 2 for entry in commands), commands
    check("window2_read_routes_to_window2", {"bytes": len(read2["text"])})

    # ---- 两个窗口各自显示不同 panel；一个窗口的 ui.render 不覆盖另一个
    run_palette(win1, "panel window")
    wait_log(
        "[plugin-v3] dev.tervo.winroute surface=panel:main window=window-1 opened by ui.render",
        label="panel in window 1",
    )
    run_palette(win2, "panel window")
    wait_log(
        "[plugin-v3] dev.tervo.winroute surface=panel:main window=window-2 opened by ui.render",
        label="panel in window 2",
    )
    plugin_panels = {
        entry.get("window") for entry in plugin_entries() if entry.get("kind") == "panel"
    }
    assert plugin_panels == {1, 2}, plugin_panels
    check("independent_panels_per_window", {"panels": sorted(plugin_panels)})

    # ---- 状态栏按窗口
    run_palette(win1, "status window")
    wait_log('[plugin-v3] status window=window-1 text="Window Route Probe: status-window-1"', label="status 1")
    run_palette(win2, "status window")
    wait_log('[plugin-v3] status window=window-2 text="Window Route Probe: status-window-2"', label="status 2")
    check("status_per_window", {"window_1": "status-window-1", "window_2": "status-window-2"})

    # ---- MCP：显式 window_id 校验 + 响应/audit 带窗口
    out1 = live.ok("output", {"window_id": 1, "pane_id": panes[1]})
    assert out1.get("window_id") == 1, out1
    assert "W1_MARK" in out1["text"] and "W2_MARK" not in out1["text"], out1
    out2 = live.ok("output", {"window_id": 2, "pane_id": panes[2]})
    assert out2.get("window_id") == 2, out2
    assert "W2_MARK" in out2["text"] and "W1_MARK" not in out2["text"], out2
    cross = live.call("output", {"window_id": 1, "pane_id": panes[2]})
    assert not cross.get("ok"), cross
    unknown = live.call("output", {"window_id": 99, "pane_id": panes[1]})
    assert not unknown.get("ok"), unknown
    filtered = live.ok("list", {"window_id": 2})
    assert filtered["panes"] and all(pane["window_id"] == 2 for pane in filtered["panes"]), filtered
    check("mcp_explicit_window_validation", {"unknown": unknown.get("error")})

    # ---- 关闭窗口 2：清理它的 surface/status；插件进程仍是一份
    live.ok("close_window", {"window_id": 2})
    wait_log("[plugin-v3] forget window=2", label="window 2 cleanup")
    wait_for(
        lambda: len((live.call("list") or {}).get("data", {}).get("windows", [])) == 1,
        label="window 2 closed",
    )
    assert count_processes(args.plugin) == 1
    assert os.kill(args.app_pid, 0) is None
    last = live.call("close_window", {"window_id": 1})
    assert not last.get("ok"), last
    assert "last window" in last.get("error", ""), last
    assert len(live.ok("list")["windows"]) == 1
    check("close_window_cleanup_and_last_refusal", {"error": last.get("error")})

    # ---- audit 日志含 window_id
    text = log_text()
    for needle in (
        "[mcp] action=output window=window-1",
        "[mcp] action=output window=window-2",
        "[mcp] action=close_window window=window-2",
        "[mcp] action=output window=active",
    ):
        if needle not in text and needle != "[mcp] action=output window=active":
            raise AssertionError(f"missing MCP audit line: {needle}")
    check("mcp_audit_has_window_id")

    report["ok"] = True
    (out / "report.json").write_text(json.dumps(report, indent=2))
    print("PASS plugin per-window E2E")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssertionError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
