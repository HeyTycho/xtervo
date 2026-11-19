#!/usr/bin/env python3
"""Black-box acceptance for multi-window session v2 save/restore.

Driven by ``scripts/e2e_multi_window_session.sh``. Starts the real ``tervo``
binary under Xvfb (+ Openbox for a real WM), drives it through the existing live
control socket, moves/maximizes native windows through the ``xwin`` helper, and
verifies the on-disk v2 snapshot plus the restored windows after kill -9.

Checks:
  * 2 windows, different tab counts, geometry + maximized saved in v2.
  * kill -9 -> auto restore recreates both windows/tabs/active_tab/geometry.
  * primary closed while second stays: autosave keeps writing, config watcher
    keeps hot-reloading, and a later restart restores the surviving window.
  * ``--restore`` restores a multi-window v2 snapshot even when the config
    switch is off.
  * v1 single-window snapshot migrates to one v2 window.
  * empty/partial v2 windows are skipped without failing the whole startup.
"""

import argparse
import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import time

TOKEN = "mw-session-e2e-token"


class Live:
    def __init__(self, path: pathlib.Path):
        self.path = path

    def call(self, cmd: str, args: dict | None = None):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(3.0)
        sock.connect(str(self.path))
        payload = {"token": TOKEN, "cmd": cmd, "args": args or {}}
        sock.sendall((json.dumps(payload) + "\n").encode())
        line = sock.makefile().readline()
        sock.close()
        if not line:
            raise AssertionError(f"empty live response for {cmd}")
        return json.loads(line)

    def ok(self, cmd: str, args: dict | None = None):
        reply = self.call(cmd, args)
        if not reply.get("ok"):
            raise AssertionError(f"{cmd} failed: {reply}")
        return reply.get("data")


class App:
    def __init__(
        self,
        proc: subprocess.Popen,
        run_dir: pathlib.Path,
        log_path: pathlib.Path,
        socket_path: pathlib.Path,
    ):
        self.proc = proc
        self.run_dir = run_dir
        self.log_path = log_path
        self.socket_path = socket_path
        self.live = Live(socket_path)
        self._log = None

    def log_text(self) -> str:
        try:
            return self.log_path.read_text(errors="replace")
        except OSError:
            return ""

    def windows(self) -> dict[int, dict]:
        data = self.live.ok("list")
        return {window["window_id"]: window for window in data["windows"]}

    def stop(self, kill9: bool = True) -> None:
        if self.proc.poll() is None:
            try:
                self.proc.send_signal(9 if kill9 else 15)
            except ProcessLookupError:
                pass
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        if self._log is not None:
            self._log.close()
            self._log = None
        try:
            self.socket_path.unlink()
        except FileNotFoundError:
            pass
        # X 连接关闭后窗口消失需要一点时间，避免下一轮抓到旧窗口。
        time.sleep(0.25)


def wait_for(pred, timeout=12.0, label="condition", interval=0.1):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            last = pred()
            if last:
                return last
        except (AssertionError, OSError, socket.timeout, json.JSONDecodeError) as error:
            last = error
        time.sleep(interval)
    raise AssertionError(f"timeout waiting for {label}; last={last!r}")


def write_config(run_dir: pathlib.Path, restore: bool, extra: list[str] | None = None) -> None:
    lines = [
        f"restore_session_on_startup = {'true' if restore else 'false'}",
        "cols = 80",
        "rows = 24",
        'shell = "/bin/sh"',
        "confirm_close = false",
    ]
    lines.extend(extra or [])
    (run_dir / "config.toml").write_text("\n".join(lines) + "\n")


def prepare_run_dir(out: pathlib.Path, name: str) -> pathlib.Path:
    run_dir = out / name
    for sub in ("logs", "runtime", "confighome", "state", "plugins"):
        (run_dir / sub).mkdir(parents=True, exist_ok=True)
    os.chmod(run_dir / "runtime", 0o700)
    shutil.rmtree(run_dir / "plugins", ignore_errors=True)
    (run_dir / "plugins").mkdir(parents=True, exist_ok=True)
    return run_dir


def start_app(
    args,
    out: pathlib.Path,
    name: str,
    *,
    inproc: bool,
    extra_args: list[str] | None = None,
    log_name: str = "app.log",
) -> App:
    run_dir = prepare_run_dir(out, name)
    log_path = run_dir / "logs" / log_name
    # AF_UNIX sun_path 上限 108 字节；worktree 下的 target 路径太长，socket 放 /tmp。
    socket_path = pathlib.Path("/tmp") / (
        f"tervo-mws-{os.getpid()}-{name}-{int(time.time() * 1000)}.sock"
    )
    try:
        socket_path.unlink()
    except FileNotFoundError:
        pass
    env = os.environ.copy()
    env.update(
        {
            "DISPLAY": args.display,
            "XDG_RUNTIME_DIR": str(run_dir / "runtime"),
            "XDG_CONFIG_HOME": str(run_dir / "confighome"),
            "XDG_STATE_HOME": str(run_dir / "state"),
            "TERVO_CONFIG": str(run_dir / "config.toml"),
            "TERVO_LOG_DIR": str(run_dir / "logs"),
            "TERVO_SESSION": str(run_dir / "session.json"),
            "TERVO_PROFILES": str(run_dir / "profiles.toml"),
            "TERVO_PLUGINS": str(run_dir / "plugins"),
            "TERVO_DIAG_SOCKET": str(socket_path),
            "TERVO_DIAG_TOKEN": TOKEN,
            "TERVO_DIAG_ALLOW_WRITE": "1",
            "TERVO_DIAG_ALLOW_DESTRUCTIVE": "1",
            "SLINT_BACKEND": "winit-software",
            "XMODIFIERS": "@im=none",
        }
    )
    if inproc:
        env["TERVO_MULTI_WINDOW_INPROC"] = "1"
    else:
        env.pop("TERVO_MULTI_WINDOW_INPROC", None)
    env.pop("TERVO_MULTI_WINDOW_SECOND", None)
    log = open(log_path, "ab", buffering=0)
    cmd = [
        args.bin,
        "--cols",
        "80",
        "--rows",
        "24",
        "--shell",
        "/bin/sh",
        *(extra_args or []),
    ]
    proc = subprocess.Popen(
        cmd,
        env=env,
        stdout=log,
        stderr=log,
        start_new_session=True,
    )
    app = App(proc, run_dir, log_path, socket_path)
    app._log = log

    def ready():
        if proc.poll() is not None:
            tail = log_path.read_text(errors="replace")[-4000:]
            raise AssertionError(f"app exited early rc={proc.returncode}\n{tail}")
        return app.live.call("list").get("ok")

    try:
        wait_for(ready, timeout=15.0, label=f"{name} live socket")
    except AssertionError:
        app.stop(kill9=True)
        raise
    return app


def x11_windows(display: str, pid: int) -> dict[str, str]:
    out = subprocess.check_output(
        ["xwininfo", "-root", "-tree"],
        env={**os.environ, "DISPLAY": display},
        text=True,
    )
    candidates = []
    for line in out.splitlines():
        match = re.search(r'(0x[0-9a-f]+) "([^"]*)"', line)
        if match and "Tervo" in match.group(2):
            candidates.append((match.group(1), match.group(2)))
    by_title: dict[str, str] = {}
    for win, title in candidates:
        prop = subprocess.run(
            ["xprop", "-id", win, "_NET_WM_PID"],
            env={**os.environ, "DISPLAY": display},
            text=True,
            capture_output=True,
            check=False,
        ).stdout
        match = re.search(r"=\s*(\d+)", prop)
        if match and int(match.group(1)) == pid:
            by_title[title] = win
    if by_title:
        return by_title
    # Slint 没有写 _NET_WM_PID 时退回标题匹配（仍优先精确标题）。
    return {title: win for win, title in candidates}


def x11_window_ids(display: str, pid: int) -> list[str]:
    """同进程多窗口标题统一为 Tervo；按 _NET_WM_PID 返回排序后的 XID 列表。"""
    out = subprocess.check_output(
        ["xwininfo", "-root", "-tree"],
        env={**os.environ, "DISPLAY": display},
        text=True,
    )
    ids: list[str] = []
    for line in out.splitlines():
        match = re.search(r'(0x[0-9a-f]+) "([^"]*)"', line)
        if not match or "Tervo" not in match.group(2):
            continue
        win = match.group(1)
        prop = subprocess.run(
            ["xprop", "-id", win, "_NET_WM_PID"],
            env={**os.environ, "DISPLAY": display},
            text=True,
            capture_output=True,
            check=False,
        ).stdout
        pm = re.search(r"=\s*(\d+)", prop)
        if pm and int(pm.group(1)) == pid:
            ids.append(win)
    return sorted(set(ids), key=lambda w: int(w, 16))


def xwin(args, win: str, *rest: str) -> None:
    subprocess.run(
        [args.xwin, win, *rest],
        env={**os.environ, "DISPLAY": args.display},
        check=True,
        capture_output=True,
        text=True,
    )
    time.sleep(0.15)


def read_session(path: pathlib.Path) -> dict:
    return json.loads(path.read_text())


def window_map(session: dict) -> dict[int, dict]:
    return {window["id"]: window for window in session["windows"]}


def close_enough(left, right, tolerance=8) -> bool:
    return abs(int(left) - int(right)) <= tolerance


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--bin", required=True)
    parser.add_argument("--xwin", required=True)
    parser.add_argument("--display", required=True)
    args = parser.parse_args()

    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    report = {"checks": [], "ok": False}
    apps: list[App] = []

    def check(name: str, detail=None) -> None:
        report["checks"].append({"name": name, "detail": detail})
        print(f"PASS {name}" + (f": {detail}" if detail else ""))

    try:
        # ---------------------------------------------------------- run 1: save
        run1 = prepare_run_dir(out, "run1")
        write_config(run1, restore=True)
        app = start_app(args, out, "run1", inproc=True, log_name="app1.log")
        apps.append(app)
        assert len(app.windows()) == 1
        app.live.ok("new_window")
        wait_for(lambda: len(app.windows()) == 2, label="window 2 created")
        app.live.ok("new_tab", {"window_id": 1})
        app.live.ok("new_tab", {"window_id": 2})
        app.live.ok("new_tab", {"window_id": 2})
        wait_for(
            lambda: {wid: w["tabs"] for wid, w in app.windows().items()} == {1: 2, 2: 3},
            label="independent tabs",
        )
        wins = x11_window_ids(args.display, app.proc.pid)
        assert len(wins) == 2, wins
        xwin(args, wins[0], "60", "80", "640", "480")
        xwin(args, wins[1], "760", "120", "700", "560")
        wait_for(
            lambda: (
                abs(app.windows()[1]["geometry"]["w"] - 640) <= 2
                and abs(app.windows()[2]["geometry"]["w"] - 700) <= 2
            ),
            label="window geometry applied",
        )
        xwin(args, wins[1], "--maximize")
        wait_for(
            lambda: app.windows()[2]["maximized"] is True,
            timeout=6.0,
            label="window 2 maximized",
        )
        # 2s autosave + settle.
        wait_for(
            lambda: (run1 / "session.json").is_file()
            and window_map(read_session(run1 / "session.json")).keys() == {1, 2},
            timeout=8.0,
            label="v2 autosave",
        )
        time.sleep(2.4)
        live = app.windows()
        session = read_session(run1 / "session.json")
        saved = window_map(session)
        assert session["version"] == 2, session
        assert len(saved) == 2, session
        assert saved[1]["tabs"] and len(saved[1]["tabs"]) == 2, saved[1]
        assert saved[2]["tabs"] and len(saved[2]["tabs"]) == 3, saved[2]
        assert saved[1]["active_tab"] == live[1]["active_tab"], (saved[1], live[1])
        assert saved[2]["active_tab"] == live[2]["active_tab"], (saved[2], live[2])
        for wid in (1, 2):
            for key in ("x", "y", "w", "h"):
                assert close_enough(
                    saved[wid]["geometry"][key], live[wid]["geometry"][key]
                ), (wid, key, saved[wid]["geometry"], live[wid]["geometry"])
        assert saved[2]["maximized"] is True, saved[2]
        assert saved[1]["maximized"] is False, saved[1]
        shutil.copy(run1 / "session.json", out / "saved-two-window.json")
        check(
            "autosave_v2_two_windows",
            {
                "tabs": {wid: len(w["tabs"]) for wid, w in saved.items()},
                "active_tab": {wid: w["active_tab"] for wid, w in saved.items()},
                "geometry": {wid: w["geometry"] for wid, w in saved.items()},
                "maximized": {wid: w["maximized"] for wid, w in saved.items()},
            },
        )
        app.stop(kill9=True)

        # ------------------------------------- run 2: auto restore two windows
        app = start_app(args, out, "run1", inproc=False, log_name="app2.log")
        apps.append(app)
        wait_for(lambda: len(app.windows()) == 2, label="two windows auto-restored")
        live = app.windows()
        assert {wid: w["tabs"] for wid, w in live.items()} == {1: 2, 2: 3}, live
        for wid in (1, 2):
            assert live[wid]["active_tab"] == saved[wid]["active_tab"], (wid, live[wid])
            for key in ("x", "y", "w", "h"):
                assert close_enough(
                    live[wid]["geometry"][key], saved[wid]["geometry"][key], 12
                ), (wid, key, live[wid]["geometry"], saved[wid]["geometry"])
        assert live[2]["maximized"] is True, live[2]
        log2 = app.log_text()
        assert "多窗口恢复完成：2/2" in log2, log2[-3000:]
        assert "in-process window created id=window-2 restored=true" in log2, log2[-3000:]
        check("auto_restore_v2_two_windows", {wid: live[wid]["tabs"] for wid in live})

        # ------------------- primary close: autosave + watcher survive in second
        app.live.ok("close_window", {"window_id": 1})
        wait_for(lambda: set(app.windows()) == {2}, label="primary closed, second alive")
        assert app.proc.poll() is None, "process exited after primary close"
        assert (run1 / "session.running").is_file(), "running marker must survive primary close"
        app.live.ok("new_tab", {"window_id": 2})
        wait_for(lambda: app.windows()[2]["tabs"] == 4, label="new tab in second window")
        write_config(run1, restore=True, extra=["scroll_on_output = true"])
        wait_for(
            lambda: "已套用外部热更新" in app.log_text(),
            timeout=6.0,
            label="config hot reload after primary close",
        )
        check(
            "primary_close_keeps_config_watcher",
            {"remaining": 2, "hot_reload": "scroll_on_output"},
        )
        wait_for(
            lambda: (
                (run1 / "session.json").is_file()
                and list(window_map(read_session(run1 / "session.json"))) == [2]
                and len(window_map(read_session(run1 / "session.json"))[2]["tabs"]) == 4
            ),
            timeout=8.0,
            label="autosave after primary close",
        )
        check(
            "primary_close_keeps_session_writer",
            {"windows": 1, "tabs": 4},
        )
        app.stop(kill9=True)

        # -------------------------- run 3: restart after primary-close snapshot
        app = start_app(args, out, "run1", inproc=False, log_name="app3.log")
        apps.append(app)
        wait_for(lambda: set(app.windows()) == {1}, label="one window restored")
        assert app.windows()[1]["tabs"] == 4, app.windows()[1]
        wait_for(
            lambda: "多窗口恢复完成：1/1" in app.log_text(),
            timeout=4.0,
            label="restore-complete log",
        )
        check("restore_after_primary_close", {"windows": 1, "tabs": 4})

        # 最后一个窗口正常关闭：事件循环退出、最终保存、running marker 清除。
        wins = x11_window_ids(args.display, app.proc.pid)
        assert wins, "no Tervo window after restore"
        xwin(args, wins[0], "--close")
        wait_for(
            lambda: app.proc.poll() is not None,
            timeout=10.0,
            label="last window clean exit",
        )
        assert app.proc.returncode == 0, app.proc.returncode
        wait_for(
            lambda: not (run1 / "session.running").exists(),
            timeout=4.0,
            label="running marker cleared on clean exit",
        )
        saved = window_map(read_session(run1 / "session.json"))
        assert list(saved) == [1], saved
        assert len(saved[1]["tabs"]) == 4, saved[1]
        check(
            "last_window_clean_exit",
            {"exit": 0, "running_marker": "cleared", "windows": 1, "tabs": 4},
        )
        app.stop(kill9=False)

        # ------------------------------------------ run 4: explicit --restore
        run4 = prepare_run_dir(out, "run4")
        shutil.copy(out / "saved-two-window.json", run4 / "session.json")
        write_config(run4, restore=False)
        app = start_app(
            args, out, "run4", inproc=False, extra_args=["--restore"], log_name="app.log"
        )
        apps.append(app)
        wait_for(lambda: len(app.windows()) == 2, label="--restore two windows")
        assert {wid: w["tabs"] for wid, w in app.windows().items()} == {1: 2, 2: 3}
        assert "读取 v2 快照" in app.log_text(), app.log_text()[-3000:]
        wait_for(
            lambda: "多窗口恢复完成：2/2" in app.log_text(),
            timeout=4.0,
            label="--restore complete log",
        )
        check("explicit_restore_flag_two_windows", {"windows": 2, "tabs": [2, 3]})
        app.stop(kill9=True)

        # ---------------------------------------- run 5: v1 single-window compat
        run5 = prepare_run_dir(out, "run5")
        write_config(run5, restore=True)
        (run5 / "session.json").write_text(
            json.dumps(
                {
                    "version": 1,
                    "cols": 100,
                    "rows": 30,
                    "active": 1,
                    "tabs": [
                        {
                            "title": "one",
                            "focused": 0,
                            "tree": {"kind": "leaf", "cwd": "/tmp"},
                        },
                        {
                            "title": "two",
                            "focused": 1,
                            "tree": {
                                "kind": "split",
                                "dir": "col",
                                "ratio": 0.3,
                                "a": {"kind": "leaf", "cwd": "/"},
                                "b": {"kind": "leaf", "cwd": "/home"},
                            },
                        },
                    ],
                }
            )
        )
        app = start_app(args, out, "run5", inproc=False, log_name="app.log")
        apps.append(app)
        wait_for(lambda: len(app.windows()) == 1, label="v1 single window")
        window = app.windows()[1]
        assert window["tabs"] == 2, window
        assert window["active_tab"] == 1, window
        assert "v1 单窗口快照已迁移为 v2" in app.log_text(), app.log_text()[-3000:]
        wait_for(
            lambda: "多窗口恢复完成：1/1" in app.log_text(),
            timeout=4.0,
            label="v1 restore complete log",
        )
        check("v1_snapshot_migrates_single_window", {"tabs": 2, "active_tab": 1})
        app.stop(kill9=True)

        # ----------------------------- run 6: partial/empty windows are skipped
        run6 = prepare_run_dir(out, "run6")
        write_config(run6, restore=True)
        (run6 / "session.json").write_text(
            json.dumps(
                {
                    "version": 2,
                    "windows": [
                        {
                            "id": 1,
                            "geometry": {"x": 0, "y": 0, "w": 0, "h": 0},
                            "maximized": False,
                            "active_tab": 0,
                            "tabs": [],
                        },
                        {
                            "id": 2,
                            "geometry": {"x": 10, "y": 20, "w": 640, "h": 480},
                            "maximized": False,
                            "active_tab": 99,
                            "tabs": [
                                {
                                    "title": "keep",
                                    "focused": 0,
                                    "tree": {"kind": "leaf", "cwd": "/tmp"},
                                }
                            ],
                        },
                    ],
                }
            )
        )
        app = start_app(args, out, "run6", inproc=False, log_name="app.log")
        apps.append(app)
        wait_for(lambda: len(app.windows()) == 1, label="partial snapshot fallback")
        window = app.windows()[1]
        assert window["tabs"] == 1, window
        assert window["active_tab"] == 0, window  # clamped from 99
        assert close_enough(window["geometry"]["w"], 640, 4), window
        assert app.proc.poll() is None
        assert "2 个窗口，其中 1 个可恢复" in app.log_text(), app.log_text()[-3000:]
        wait_for(
            lambda: "多窗口恢复完成：1/1" in app.log_text(),
            timeout=4.0,
            label="partial restore complete log",
        )
        check(
            "partial_snapshot_skips_empty_window",
            {"windows": 1, "tabs": 1, "active_tab": 0},
        )
        app.stop(kill9=True)

        # -------------------- run 8: primary maximized restore (X11 state machine)
        run8 = prepare_run_dir(out, "run8")
        maximized = read_session(out / "saved-two-window.json")
        window_one = window_map(maximized)[1]
        window_one["maximized"] = True
        window_one["geometry"] = {"x": 0, "y": 0, "w": 1600, "h": 1000}
        (run8 / "session.json").write_text(json.dumps(maximized))
        write_config(run8, restore=True)
        app = start_app(args, out, "run8", inproc=False, log_name="app.log")
        apps.append(app)
        wait_for(lambda: len(app.windows()) == 2, label="primary-maximized restore")
        wait_for(
            lambda: app.windows()[1]["maximized"] is True,
            timeout=6.0,
            label="primary maximized restored",
        )
        assert app.windows()[1]["tabs"] == 2
        check(
            "restore_primary_maximized",
            {"window": 1, "maximized": True, "tabs": 2},
        )
        app.stop(kill9=True)

        # ------------------------------ run 7: config switch off means no restore
        run7 = prepare_run_dir(out, "run7")
        shutil.copy(out / "saved-two-window.json", run7 / "session.json")
        write_config(run7, restore=False)
        app = start_app(args, out, "run7", inproc=False, log_name="app.log")
        apps.append(app)
        wait_for(lambda: len(app.windows()) == 1, label="startup without restore")
        window = app.windows()[1]
        assert window["tabs"] == 1, window
        assert "多窗口恢复完成" not in app.log_text(), app.log_text()[-3000:]
        check("restore_switch_off_keeps_fresh_layout", {"tabs": 1})
        app.stop(kill9=True)

        report["ok"] = True
        (out / "report.json").write_text(json.dumps(report, indent=2))
        print("PASS multi-window session v2 e2e")
        return 0
    except AssertionError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        report["error"] = str(error)
        (out / "report.json").write_text(json.dumps(report, indent=2))
        return 1
    finally:
        for app in apps:
            app.stop(kill9=True)


if __name__ == "__main__":
    raise SystemExit(main())
