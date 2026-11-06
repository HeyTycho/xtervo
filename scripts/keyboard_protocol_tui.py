#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""伪 TUI：在真实 PTY 里跑键盘协议 query/set，并抓取 app 写回的按键字节。

用法：keyboard_protocol_tui.py <mode> <outdir>
  mode = kitty | modify | plain | off
退出前把：
  outdir/responses.bin  query 应答
  outdir/keys.bin       ready 之后收到的按键字节
写盘；outdir/ready 表示可以注入按键。
"""
import os
import select
import sys
import termios
import time
import tty


def main() -> int:
    mode = sys.argv[1]
    outdir = sys.argv[2]
    os.makedirs(outdir, exist_ok=True)

    fd = 0
    old = termios.tcgetattr(fd)
    tty.setraw(fd)

    def write(data: bytes) -> None:
        os.write(1, data)

    def read_for(seconds: float) -> bytes:
        data = b""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(fd, 256)
            except OSError:
                break
            if not chunk:
                break
            data += chunk
        return data

    def query(payload: bytes, timeout: float = 0.6) -> bytes:
        write(payload)
        return read_for(timeout)

    responses = b""
    try:
        if mode in ("kitty", "off"):
            responses += query(b"\x1b[?u")
            responses += query(b"\x1b[>3u\x1b[?u")
        if mode in ("modify", "off"):
            responses += query(b"\x1b[>4;2m\x1b[?4m")
        with open(os.path.join(outdir, "responses.bin"), "wb") as fh:
            fh.write(responses)
        open(os.path.join(outdir, "ready"), "w").close()
        # 等 e2e 注入完按键后写 done；期间持续读 PTY 输入。
        keys = b""
        done = os.path.join(outdir, "done")
        deadline = time.monotonic() + 15.0
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.05)
            if ready:
                try:
                    keys += os.read(fd, 256)
                except OSError:
                    break
            if os.path.exists(done):
                # 排空 done 之后可能紧跟着的 release/修饰键事件。
                keys += read_for(0.3)
                break
        with open(os.path.join(outdir, "keys.bin"), "wb") as fh:
            fh.write(keys)
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
