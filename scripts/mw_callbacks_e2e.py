#!/usr/bin/env python3
"""Dual-window UI callback acceptance checks (real X11 + live control socket).

Driven by ``scripts/e2e_multi_window_callbacks.sh``. Every assertion is against
the real app process: XTest clicks/keys go through Slint -> shared callback
registration -> per-window AppState. ``list`` exposes per-window UI/model state
and SharedServices identities so the test can prove windows are independent but
share one config/plugin/vault/MCP.
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

W = 1080
H = 736
GAP = 1120
# 1080x736 layout coordinates (same as scripts/e2e_settings_i18n.sh).
GEAR = (W - 118, 16)
SEARCH = (880, 56)
NAV_X = 100
NAV_HOTKEYS = 280
NAV_PROFILES = 316
NAV_PLUGINS = 352
# settings-category index for the Plugins page (sidebar order differs from index).
PLUGINS_CATEGORY = 5
HOTKEYS_CATEGORY = 4


class Live:
    def __init__(self, path, token):
        self.path = path
        self.token = token

    def call(self, cmd, args=None):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(5.0)
        sock.connect(self.path)
        payload = {"token": self.token, "cmd": cmd, "args": args or {}}
        sock.sendall((json.dumps(payload) + "\n").encode())
        line = sock.makefile().readline()
        sock.close()
        if not line:
            raise AssertionError(f"empty response for {cmd}")
        return json.loads(line)

    def ok(self, cmd, args=None):
        reply = self.call(cmd, args)
        assert reply.get("ok"), f"{cmd} failed: {reply}"
        return reply["data"]


def wait_for(pred, timeout=8.0, label="condition"):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = pred()
        if last:
            return last
        time.sleep(0.1)
    raise AssertionError(f"timeout waiting for {label}; last={last!r}")


def x11_windows(display):
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


def count_processes(needle):
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", required=True)
    ap.add_argument("--token", required=True)
    ap.add_argument("--xwin", required=True)
    ap.add_argument("--xclick", required=True)
    ap.add_argument("--xkey", required=True)
    ap.add_argument("--display", required=True)
    ap.add_argument("--pid", type=int, required=True)
    ap.add_argument("--log", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--plugin", default="")
    args = ap.parse_args()

    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    report = {"checks": [], "ok": False}
    pid = args.pid

    def check(name, detail=None):
        report["checks"].append({"name": name, "detail": detail})
        print(f"PASS {name}" + (f": {detail}" if detail else ""))

    live = Live(args.socket, args.token)

    def list_windows():
        return live.ok("list")

    def by_id(data, window_id):
        for window in data["windows"]:
            if window["window_id"] == window_id:
                return window
        raise AssertionError(f"window {window_id} missing: {data['windows']}")

    def ui_of(window):
        return window["ui"]

    def run(*cmd):
        subprocess.run(cmd, check=False)

    def focus(win):
        # 只重设 X 输入焦点；位置/尺寸在 setup 阶段固定。
        run(args.xwin, win, "--focus")
        time.sleep(0.15)

    def click(x, y):
        run(args.xclick, str(x), str(y))
        time.sleep(0.45)

    def key(text=None, ctrl=False, shift=False, named=None):
        cmd = [args.xkey]
        if named:
            cmd += ["--key", named]
        else:
            cmd += [text]
        if ctrl:
            cmd.append("--ctrl")
        if shift:
            cmd.append("--shift")
        run(*cmd)
        time.sleep(0.35)

    data = wait_for(
        lambda: (lambda d: d if len(d.get("windows", [])) == 2 else None)(
            list_windows()
        ),
        label="two windows",
    )
    assert os.kill(pid, 0) is None
    win1, win2 = 1, 2
    shared = [by_id(data, win1)["shared"], by_id(data, win2)["shared"]]
    assert shared[0] == shared[1], f"shared services differ: {shared}"
    for key_name in ("cfg", "themes", "plugins", "vault", "profiles", "session", "mcp"):
        assert shared[0][key_name] == shared[1][key_name], (key_name, shared)
    assert shared[0]["plugins"] != 0
    check("shared_services_one_copy", shared[0])

    wins = x11_windows(args.display)
    xwin1 = wins.get("Tervo")
    xwin2 = wins.get("Tervo — 2")
    assert xwin1 and xwin2, wins
    run(args.xwin, xwin1, "0", "0", str(W), str(H))
    run(args.xwin, xwin2, str(GAP), "0", str(W), str(H))
    time.sleep(0.8)
    check("two_x11_windows_positioned", {"a": xwin1, "b": xwin2})

    # ---- window 1: 命令面板（真实键盘回调），先于设置页 ----
    focus(xwin1)
    key("p", ctrl=True, shift=True)
    data = wait_for(
        lambda: (lambda d: d if by_id(d, win1)["ui"]["palette_open"] else None)(list_windows()),
        label="window 1 palette open",
    )
    assert not by_id(data, win2)["ui"]["palette_open"], data
    key(named="escape")
    wait_for(
        lambda: not by_id(list_windows(), win1)["ui"]["palette_open"],
        label="window 1 palette closed",
    )
    check("palette_roundtrip_window_1")

    # ---- window 1: 点击标题栏齿轮打开设置（真实鼠标回调）----
    focus(xwin1)
    click(*GEAR)
    data = wait_for(
        lambda: (lambda d: d if by_id(d, win1)["ui"]["settings_open"] else None)(list_windows()),
        label="window 1 settings open",
    )
    assert by_id(data, win1)["ui"]["settings_category"] == 0, data
    assert not by_id(data, win2)["ui"]["settings_open"], data
    assert by_id(data, win1)["ui"]["plugin_rows"] > 0, data
    check("settings_opens_in_window_1_only", by_id(data, win1)["ui"]["settings_open"])

    # ---- window 1: 侧栏进入插件页（Plugins category）----
    click(NAV_X, NAV_PLUGINS)
    data = wait_for(
        lambda: (
            lambda d: d
            if by_id(d, win1)["ui"]["settings_category"] == PLUGINS_CATEGORY
            else None
        )(list_windows()),
        label="window 1 plugin page",
    )
    assert by_id(data, win1)["ui"]["plugin_rows"] > 0, data
    assert not by_id(data, win2)["ui"]["settings_open"], data
    check("plugin_page_window_1", by_id(data, win1)["ui"]["plugin_rows"])

    # ---- window 2: Ctrl+Shift+P -> 搜索 settings -> Enter 打开设置 ----
    focus(xwin2)
    key("p", ctrl=True, shift=True)
    data = wait_for(
        lambda: (lambda d: d if by_id(d, win2)["ui"]["palette_open"] else None)(list_windows()),
        label="window 2 palette open",
    )
    assert not by_id(data, win1)["ui"]["palette_open"], data
    assert by_id(data, win2)["ui"]["palette_items"] > 0, data
    check("palette_opens_in_window_2_only", by_id(data, win2)["ui"]["palette_items"])
    key("settings")
    wait_for(
        lambda: by_id(list_windows(), win2)["ui"]["palette_items"] >= 1,
        label="palette settings filtered",
    )
    key(named="enter")
    data = wait_for(
        lambda: (lambda d: d if by_id(d, win2)["ui"]["settings_open"] else None)(list_windows()),
        label="window 2 settings via palette",
    )
    assert not by_id(data, win2)["ui"]["palette_open"], data
    check("palette_runs_settings_in_window_2", by_id(data, win2)["ui"]["settings_open"])

    # ---- window 2: 插件页 ----
    click(GAP + NAV_X, NAV_PLUGINS)
    data = wait_for(
        lambda: (
            lambda d: d
            if by_id(d, win2)["ui"]["settings_category"] == PLUGINS_CATEGORY
            else None
        )(list_windows()),
        label="window 2 plugin page",
    )
    assert by_id(data, win1)["ui"]["settings_category"] == PLUGINS_CATEGORY, data
    assert by_id(data, win2)["ui"]["plugin_rows"] > 0, data
    check(
        "both_windows_on_plugin_page",
        {
            str(w["window_id"]): (w["ui"]["settings_category"], w["ui"]["plugin_rows"])
            for w in data["windows"]
        },
    )

    # ---- 设置页模型独立：window 1 切到 Hotkeys，window 2 仍是 Plugins ----
    click(NAV_X, NAV_HOTKEYS)
    data = wait_for(
        lambda: (
            lambda d: d
            if by_id(d, win1)["ui"]["settings_category"] == HOTKEYS_CATEGORY
            else None
        )(list_windows()),
        label="window 1 hotkeys page",
    )
    assert by_id(data, win2)["ui"]["settings_category"] == PLUGINS_CATEGORY, data
    check(
        "settings_category_models_independent",
        {"w1": by_id(data, win1)["ui"]["settings_category"],
         "w2": by_id(data, win2)["ui"]["settings_category"]},
    )

    # ---- 设置搜索：window 1 搜索只改 window 1 的 results 模型 ----
    # XTest 不会自己切 X 输入焦点；先显式 focus，否则按键仍落在窗口 2。
    focus(xwin1)
    click(*SEARCH)
    key("sixel")
    data = wait_for(
        lambda: (
            lambda d: d
            if by_id(d, win1)["ui"]["settings_results"] > 0
            and by_id(d, win1)["ui"]["settings_query"] == "sixel"
            else None
        )(list_windows()),
        label="settings search window 1",
    )
    assert by_id(data, win2)["ui"]["settings_results"] == 0, data
    assert by_id(data, win2)["ui"]["settings_query"] == "", data
    check(
        "settings_search_stays_in_window",
        {"w1": by_id(data, win1)["ui"]["settings_results"],
         "w2": by_id(data, win2)["ui"]["settings_results"]},
    )
    focus(xwin1)
    # Esc#1 清空搜索框，Esc#2 关闭设置页（FocusScope 的既有键语义）。
    key(named="escape")
    key(named="escape")
    data = wait_for(
        lambda: (lambda d: d if not by_id(d, win1)["ui"]["settings_open"] else None)(list_windows()),
        label="window 1 settings closed",
    )
    # 关掉设置页不能影响另一个窗口的设置/插件页。
    assert by_id(data, win2)["ui"]["settings_open"], data
    assert by_id(data, win2)["ui"]["settings_category"] == PLUGINS_CATEGORY, data

    # ---- 设置页关闭后命令面板回调仍可用 ----
    focus(xwin1)
    key("p", ctrl=True, shift=True)
    data = wait_for(
        lambda: (lambda d: d if by_id(d, win1)["ui"]["palette_open"] else None)(list_windows()),
        label="window 1 palette after settings",
    )
    assert not by_id(data, win2)["ui"]["palette_open"], data
    key(named="escape")
    wait_for(
        lambda: not by_id(list_windows(), win1)["ui"]["palette_open"],
        label="window 1 palette after settings closed",
    )
    check("palette_roundtrip_after_settings")

    # ---- 关闭 window 1（primary）：window 2 的设置/插件页/命令面板继续可用 ----
    before = by_id(list_windows(), win2)
    # 点自定义 ✕ 按钮（on_close_window -> request_close -> 多窗口只关本窗口），
    # 不是 MCP close_window，也不是 WM 关窗，覆盖真实 UI 回调路径。
    focus(xwin1)
    click(W - 22, 16)
    data = wait_for(
        lambda: (lambda d: d if len(d.get("windows", [])) == 1 else None)(list_windows()),
        label="window 1 closed",
    )
    assert os.kill(pid, 0) is None
    survivor = by_id(data, win2)
    assert survivor["ui"]["settings_open"], survivor
    assert survivor["ui"]["settings_category"] == PLUGINS_CATEGORY, survivor
    assert survivor["ui"]["plugin_rows"] == before["ui"]["plugin_rows"], (before, survivor)
    assert survivor["shared"] == before["shared"], (before, survivor)
    check("close_primary_keeps_window_2_ui", survivor["ui"]["settings_category"])

    # window 2 还活着且回调仍能工作：先 Esc 关设置页（FocusScope 会吞其它键），
    # 再开命令面板 + 新 Tab。
    focus(xwin2)
    key(named="escape")
    wait_for(
        lambda: not by_id(list_windows(), win2)["ui"]["settings_open"],
        label="survivor settings closed",
    )
    focus(xwin2)
    key("p", ctrl=True, shift=True)
    data = wait_for(
        lambda: (lambda d: d if by_id(d, win2)["ui"]["palette_open"] else None)(list_windows()),
        label="remaining window palette",
    )
    key(named="escape")
    wait_for(
        lambda: not by_id(list_windows(), win2)["ui"]["palette_open"],
        label="remaining window palette closed",
    )
    tabs_before = by_id(list_windows(), win2)["tabs"]
    live.ok("new_tab", {"window_id": win2})
    data = list_windows()
    assert by_id(data, win2)["tabs"] == tabs_before + 1, data
    check("remaining_window_accepts_new_tab", by_id(data, win2)["tabs"])

    if args.plugin:
        count = count_processes(args.plugin)
        assert count == 1, f"plugin process count={count}, expected 1"
        check("plugin_host_single_process", {"count": count})

    log_text = pathlib.Path(args.log).read_text(errors="replace")
    assert "register window callbacks" in log_text
    assert log_text.count("register window callbacks") >= 2
    check("both_windows_registered_shared_callbacks", {"register_lines": log_text.count("register window callbacks")})

    report["ok"] = True
    (out / "report.json").write_text(json.dumps(report, indent=2))
    print("PASS multi-window callbacks e2e")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssertionError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
