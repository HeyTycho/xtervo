#!/usr/bin/env python3
"""ACT-034 复现/e2e：分屏后新 pane 是否真的画出像素。

两个场景：
  single —— 单窗口里 split 右侧，等 shell 输出后检查右半区文字像素
  dual   —— 先开第二个窗口，再在**非活动**窗口 split，检查该窗口右半区

用法：python3 scripts/e2e_split_repaint.py [single|dual|both]
退出码 0 = 正常；1 = 复现（右侧空白）；2 = 环境问题。

背景：早前的“新 pane 首帧不重绘”证据全部来自 0.4.0-rc.2；0.5.0 在 Xvfb 下
两个场景都正常绘制。日志只在首帧打印 `[gpu-native] pane=...`，因此不能再用
“没有新日志行”判定没重绘，必须看像素。
"""

import json
import os
import pathlib
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "release" / "tervo"
XVFB = ROOT / ".perf" / "xvfb" / "usr" / "bin" / "Xvfb"
LIB = ROOT / ".perf" / "xvfb" / "usr" / "lib" / "x86_64-linux-gnu"


def call(sock_path, token, obj, timeout=15.0):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(sock_path)
    s.sendall((json.dumps(obj) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = s.recv(1 << 20)
        if not chunk:
            break
        buf += chunk
    s.close()
    return json.loads(buf.decode())


def tervo_windows(display):
    out = subprocess.run(["xwininfo", "-root", "-tree"], capture_output=True,
                         text=True, env={**os.environ, "DISPLAY": display}).stdout
    return [l.strip().split()[0] for l in out.splitlines()
            if '"tervo"' in l and l.strip().startswith("0x")]


def capture(display, win):
    xwd = f"/tmp/split_repaint_{win}.xwd"
    subprocess.run(["xwd", "-id", win, "-silent", "-out", xwd], check=True,
                   env={**os.environ, "DISPLAY": display})
    data = pathlib.Path(xwd).read_bytes()
    h = struct.unpack(">25I", data[:100])
    hdr, w, hh, bpp, bpl, order = h[0], h[4], h[5], h[11], h[12], h[7]
    rm, gm, bm, ncol = h[14], h[15], h[16], h[19]
    off = hdr + ncol * 12
    px = data[off:off + bpl * hh]
    fmt = "<I" if order == 0 else ">I"
    pxb = bpp // 8

    def chan(v, m):
        s = 0
        mm = m
        while not (mm & 1):
            mm >>= 1
            s += 1
        return ((v & m) >> s) * 255 // (m >> s)

    bg = None
    count = 0
    for y in range(60, min(hh, 500)):
        row = px[y * bpl:(y + 1) * bpl]
        for x in range(w // 2 + 10, w):
            v = struct.unpack_from(fmt, row, x * pxb)[0]
            r, g, b = chan(v, rm), chan(v, gm), chan(v, bm)
            if bg is None:
                bg = (r, g, b)
            if abs(r - bg[0]) > 24 or abs(g - bg[1]) > 24 or abs(b - bg[2]) > 24:
                count += 1
    return w, hh, count, bg


def run(phase: str) -> int:
    display = ":153"
    state = tempfile.mkdtemp(prefix="tervo-state-")
    runtime = tempfile.mkdtemp(prefix="tervo-run-")
    xlog = open("/tmp/split_repaint_xvfb.log", "w")
    applog = open("/tmp/split_repaint_app.log", "w")
    xvfb = subprocess.Popen([str(XVFB), display, "-screen", "0", "1600x900x24"],
                            stdout=xlog, stderr=xlog)
    time.sleep(2)
    env = {**os.environ, "DISPLAY": display, "XDG_STATE_HOME": state,
           "XDG_RUNTIME_DIR": runtime,
           "LD_LIBRARY_PATH": f"{LIB}:{os.environ.get('LD_LIBRARY_PATH', '')}"}
    app = subprocess.Popen([str(BIN)], env=env, stdout=applog, stderr=applog)
    try:
        sock = pathlib.Path(runtime) / "tervo" / "mcp.sock"
        tokf = pathlib.Path(state) / "tervo" / "mcp.token"
        for _ in range(80):
            if sock.exists() and tokf.exists():
                break
            time.sleep(0.2)
        if not sock.exists():
            print("no mcp socket")
            return 2
        token = tokf.read_text().strip()

        if phase == "dual":
            call(str(sock), token, {"id": 1, "cmd": "new_window", "token": token, "args": {}})
            time.sleep(3)

        lst = call(str(sock), token, {"id": 2, "cmd": "list", "token": token})["data"]
        target = lst["windows"][0] if phase == "dual" else \
            [w for w in lst["windows"] if w["active"]][0]
        if phase == "dual":
            target = next(w for w in lst["windows"] if w["window_id"] != lst["active_window_id"])
        pane = target["panes"][0]["pane_id"]
        args = {"pane_id": pane, "direction": "right"}
        if phase == "dual":
            args["window_id"] = target["window_id"]
        rep = call(str(sock), token,
                   {"id": 3, "cmd": "split", "token": token, "args": args})
        print(f"[{phase}] target window={target['window_id']} pane={pane} split={rep.get('ok')}")
        time.sleep(3)

        worst = None
        for wid in tervo_windows(display):
            w, hh, n, bg = capture(display, wid)
            print(f"[{phase}] {wid}: {w}x{hh} bg={bg} right_text_px={n}")
            worst = n if worst is None else min(worst, n)
        return 0 if (worst or 0) > 50 else 1
    finally:
        app.terminate()
        xvfb.terminate()
        time.sleep(0.4)


def main() -> int:
    for p in (BIN, XVFB):
        if not p.exists():
            print("missing", p)
            return 2
    phases = sys.argv[1:] or ["both"]
    if phases == ["both"]:
        phases = ["single", "dual"]
    rc = 0
    for ph in phases:
        rc = max(rc, run(ph))
    return rc


if __name__ == "__main__":
    sys.exit(main())
