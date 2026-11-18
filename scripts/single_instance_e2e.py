#!/usr/bin/env python3
"""Black-box acceptance for the single-instance + in-process multi-window product path.

Driven by ``scripts/e2e_single_instance_multi_window.sh``. The wrapper owns the
Xvfb and the first ``tervo`` process; this harness makes real second/third CLI
calls, injects Ctrl+Shift+N, closes windows through WM_DELETE_WINDOW and asserts
there is still exactly one primary process.
"""

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
import time


def fail(message: str) -> None:
    raise AssertionError(message)


def run(cmd, env=None, timeout=20):
    return subprocess.run(
        cmd, env=env, capture_output=True, text=True, timeout=timeout
    )


def window_viewable(win: str, display: str) -> bool:
    out = run(["xwininfo", "-id", win], env={**os.environ, "DISPLAY": display})
    return out.returncode == 0 and "Map State: IsViewable" in out.stdout


def x11_windows(display: str):
    out = run(["xwininfo", "-root", "-tree"], env={**os.environ, "DISPLAY": display}).stdout
    found = {}
    for line in out.splitlines():
        match = re.search(r'(0x[0-9a-f]+)\s+"([^"]*)"', line)
        if not match:
            continue
        win, title = match.group(1), match.group(2)
        # tree 里 unmap 的窗口仍存在；关闭一个窗口后必须按 Map State 过滤，
        # 否则会一直把 hide 掉的窗口算进去。
        if "Tervo" in title and window_viewable(win, display):
            found[win] = title
    return found


def wait_windows(display: str, count: int, timeout=12.0):
    deadline = time.time() + timeout
    last = {}
    while time.time() < deadline:
        last = x11_windows(display)
        if len(last) == count:
            return last
        time.sleep(0.1)
    fail(f"timeout waiting for {count} Tervo windows; last={last}")


def window_pid(win: str, display: str) -> int | None:
    out = run(["xprop", "-id", win, "_NET_WM_PID"], env={**os.environ, "DISPLAY": display}).stdout
    match = re.search(r"=\s*(\d+)", out)
    return int(match.group(1)) if match else None


def instance_processes(binary: str, runtime_dir: str):
    """PIDs whose cmdline is this tervo binary and env carries this runtime dir."""
    binary = os.path.realpath(binary)
    needle = f"XDG_RUNTIME_DIR={runtime_dir}".encode()
    pids = []
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            cmdline = (entry / "cmdline").read_bytes()
            environ = (entry / "environ").read_bytes()
        except OSError:
            continue
        argv0 = cmdline.split(b"\0", 1)[0]
        if not argv0:
            continue
        try:
            actual = os.path.realpath(os.fsdecode(argv0))
        except (OSError, UnicodeError):
            continue
        if actual == binary and needle in environ:
            pids.append(int(entry.name))
    return sorted(pids)


def wait_instance_processes(binary: str, runtime_dir: str, count: int, timeout=8.0):
    deadline = time.time() + timeout
    last = []
    while time.time() < deadline:
        last = instance_processes(binary, runtime_dir)
        if len(last) == count:
            return last
        time.sleep(0.1)
    fail(f"expected {count} primary tervo process(es), last={last}")


def focused(display: str, win: str) -> None:
    result = run(["xwin", win, "--focus"], env={**os.environ, "DISPLAY": display})
    if result.returncode != 0:
        fail(f"xwin --focus {win} failed: {result.stderr}")
    time.sleep(0.25)


def close(display: str, win: str) -> None:
    result = run(["xwin", win, "--close"], env={**os.environ, "DISPLAY": display})
    if result.returncode != 0:
        fail(f"xwin --close {win} failed: {result.stderr}")


def primary_pid(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--pid", type=int, required=True)
    ap.add_argument("--display", required=True)
    ap.add_argument("--runtime-dir", required=True)
    ap.add_argument("--session", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--app-log", required=True)
    args = ap.parse_args()

    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    report = {"checks": [], "ok": False}

    def check(name, detail=None):
        report["checks"].append({"name": name, "detail": detail})
        print(f"PASS {name}" + (f": {detail}" if detail else ""))

    env = os.environ.copy()
    env["DISPLAY"] = args.display
    runtime_dir = str(pathlib.Path(args.runtime_dir).resolve())
    env["XDG_RUNTIME_DIR"] = runtime_dir
    for key in (
        "TERVO_MULTI_WINDOW_INPROC",
        "TERVO_MULTI_WINDOW_LEGACY",
        "TERVO_MULTI_WINDOW_SECOND",
        "TERVO_SINGLE_INSTANCE",
        "TERVO_INSTANCE_DIR",
    ):
        env.pop(key, None)

    binary = os.path.realpath(args.binary)
    control_sock = pathlib.Path(runtime_dir) / "tervo" / "control.sock"
    token_path = pathlib.Path(runtime_dir) / "tervo" / "instance.token"
    deadline = time.time() + 15
    while time.time() < deadline and not (
        control_sock.is_socket() and token_path.is_file()
    ):
        time.sleep(0.05)
    if not control_sock.is_socket() or not token_path.is_file():
        fail(f"primary control socket/token missing under {runtime_dir}/tervo")
    check("primary_control_socket", str(control_sock))

    wins = wait_windows(args.display, 1)
    first = next(iter(wins))
    assert window_pid(first, args.display) == args.pid, "first window must belong to primary"
    marker = pathlib.Path(args.session).with_suffix(".running")
    if not marker.exists():
        fail(f"primary must own the session running marker: {marker}")

    def assert_marker_owner():
        text = marker.read_text(errors="replace")
        match = re.search(r"pid=(\d+)", text)
        if not match or int(match.group(1)) != args.pid:
            fail(f"running marker must stay owned by primary pid {args.pid}: {text!r}")

    assert_marker_owner()
    check("primary_window", {"id": first, "title": wins[first], "marker": str(marker)})

    # 1) Second plain ``tervo`` call must forward and exit 0 without spawning.
    second = run(
        [binary, "--cols", "80", "--rows", "24", "--shell", "/bin/sh"],
        env=env,
    )
    if second.returncode != 0:
        fail(f"second tervo exited {second.returncode}: {second.stderr}")
    if "secondary: open_window ack" not in (second.stderr + second.stdout):
        fail(f"second tervo did not log forwarded ack: {second.stderr!r}")
    wins = wait_windows(args.display, 2)
    assert all(
        window_pid(win, args.display) == args.pid for win in wins
    ), f"both windows must share one process: {wins}"
    wait_instance_processes(binary, runtime_dir, 1)
    check("second_call_forwards", {"windows": sorted(wins), "pid": args.pid})

    # 2) ``--window`` compatibility shares the same forwarding path.
    third = run([binary, "--window"], env=env)
    if third.returncode != 0:
        fail(f"--window tervo exited {third.returncode}: {third.stderr}")
    if "secondary: open_window ack" not in (third.stderr + third.stdout):
        fail(f"--window did not forward: {third.stderr!r}")
    wins = wait_windows(args.display, 3)
    assert_marker_owner()
    check("window_flag_forwards", {"windows": sorted(wins)})
    log_lines = pathlib.Path(args.app_log).read_text(errors="replace").splitlines()
    created = [
        index
        for index, line in enumerate(log_lines)
        if "[ui] in-process window created" in line
    ]
    acked = [
        index
        for index, line in enumerate(log_lines)
        if "[instance] open_window -> in-process" in line
    ]
    if len(acked) < 2:
        fail(f"expected 2 forwarded open_window acks, log has {acked}")
    for ordinal, line_index in enumerate(acked, start=1):
        before = sum(1 for created_index in created if created_index < line_index)
        if before < ordinal:
            fail(
                "primary acked open_window before the UI created the window: "
                f"ack#{ordinal} at line {line_index}, created lines={created}"
            )
    if "new window pid=" in "\n".join(log_lines):
        fail("default path must not spawn a legacy subprocess window")
    check("ack_after_ui_window", {"acks": len(acked), "created": len(created)})

    # 3) Ctrl+Shift+N creates a same-process window without spawning.
    # 所有窗口标题统一为 "Tervo"；primary 用启动时拿到的 XID，不能按标题猜。
    primary_win = first
    if primary_win not in wins:
        fail(f"primary window XID missing: {primary_win} not in {sorted(wins)}")
    focused(args.display, primary_win)
    key = run(["xkey", "n", "--ctrl", "--shift"], env=env)
    if key.returncode != 0:
        fail(f"xkey Ctrl+Shift+N failed: {key.stderr}")
    wins = wait_windows(args.display, 4)
    wait_instance_processes(binary, runtime_dir, 1)
    check("ctrl_shift_n_inproc", {"windows": sorted(wins), "pid": args.pid})

    # 3b) A non-primary window's last tab must close only that window.
    target = sorted(w for w in wins if w != primary_win)[0]
    focused(args.display, target)
    key = run(["xkey", "w", "--ctrl"], env=env)
    if key.returncode != 0:
        fail(f"xkey Ctrl+W failed: {key.stderr}")
    wins = wait_windows(args.display, 3)
    assert primary_pid(args.pid), "primary exited after closing a non-primary window's last tab"
    check("last_tab_closes_only_window", {"target": target, "remaining": sorted(wins)})

    # 4) Wrong token is rejected with a clear error and no window.
    original = token_path.read_bytes()
    token_path.write_bytes(b"deadbeef" * 8 + b"\n")
    token_path.chmod(0o600)
    try:
        bad = run([binary, "--window"], env=env)
    finally:
        token_path.write_bytes(original)
        token_path.chmod(0o600)
    if bad.returncode == 0:
        fail("wrong instance token must not exit 0")
    if "unauthorized" not in (bad.stderr + bad.stdout):
        fail(f"wrong token error is not explicit: {bad.stderr!r}")
    # 3b 已关掉一个非 primary 窗口（4 → 3）；错误 token 不能新增窗口。
    assert len(x11_windows(args.display)) == 3, "rejected request must not create a window"
    check("wrong_token_rejected", {"exit": bad.returncode})

    # 5) Close non-primary windows one by one; the primary process must stay alive.
    # 所有窗口标题都统一为 "Tervo"，不能再用 `Tervo — N` 做键；按 XID 排除 primary。
    # 留一个非 primary 窗口给步骤 6；其余逐个关闭。
    others = [w for w in wins if w != primary_win]
    for win in others[:-1]:
        close(args.display, win)
        wins = wait_windows(args.display, len(wins) - 1)
        assert primary_pid(args.pid), f"primary exited after closing {win}"
        check("close_keeps_primary", {"closed": win, "remaining": sorted(wins)})

    # 6) Closing the original primary window keeps the remaining window alive.
    close(args.display, primary_win)
    wait_windows(args.display, 1)
    assert primary_pid(args.pid), "process must stay alive when the primary window closes"
    check("close_primary_keeps_others", {"remaining": 1})

    # 7) Close the last window; the primary process must exit (checked by the
    # shell wrapper, which owns the child and can collect its status).
    remaining = wait_windows(args.display, 1)
    for win in list(remaining):
        close(args.display, win)
    deadline = time.time() + 8
    while time.time() < deadline and x11_windows(args.display):
        time.sleep(0.1)
    if x11_windows(args.display):
        fail(f"windows did not close: {x11_windows(args.display)}")
    check("close_last_window_quits", {"windows": 0})

    report["ok"] = True
    (out / "report.json").write_text(json.dumps(report, indent=2))
    print("PASS single-instance multi-window e2e")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, subprocess.TimeoutExpired) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
