#!/usr/bin/env python3
"""Record (and optionally inject) the bytes on the two sides of a PTY.

The e2e harness runs dsh-tui *inside* Tervo. This proxy sits between Tervo's
PTY slave (our stdin/stdout) and a fresh PTY that dsh-tui owns, so every byte
in both directions is recorded:

  * ``host-to-app.raw`` — keystrokes, bracketed-paste payloads, DA replies
    (``ESC [ ? 62 ; 4 ; 9 ; 22 c`` proves Tervo advertised Sixel).
  * ``app-to-host.raw``  — dsh-tui's UI output, including Sixel DCS frames.

A line-delimited JSON metadata file (``proxy-events.jsonl``) records chunk
boundaries and timestamps, so the raw files can be replayed/ordered without
buffering the whole session in memory.

Control socket (``inject.sock`` in the log directory) accepts one JSON object
per line and lets the harness write bytes directly into dsh-tui's stdin. It is
used for the transcript-thumbnail click: a physical X click lands on Tervo's
own image lightbox before dsh-tui can see it (documented blocker), so the
harness injects the equivalent SGR mouse report at the PTY boundary instead.
Messages:
    {"write_b64": "<base64 bytes>"}   write bytes to the child PTY
    {"resize": [cols, rows]}          resize the child PTY
    {"ping": true}                    reply {"ok": true}

Usage:
    e2e_dsh_pty_proxy.py <logdir> <cols> <rows> <command> [args...]

Exits with the child's exit status. This file is test tooling: it intentionally
contains no product code.
"""

from __future__ import annotations

import base64
import errno
import fcntl
import json
import os
import pty
import select
import signal
import socket
import struct
import sys
import termios
import time
import tty


def _set_winsize(fd: int, cols: int, rows: int) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def main() -> int:
    if len(sys.argv) < 5:
        print(__doc__, file=sys.stderr)
        return 2
    logdir, cols_s, rows_s, cmd = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4:]
    cols, rows = int(cols_s), int(rows_s)
    os.makedirs(logdir, exist_ok=True)

    host_raw = open(os.path.join(logdir, "host-to-app.raw"), "wb", buffering=0)
    app_raw = open(os.path.join(logdir, "app-to-host.raw"), "wb", buffering=0)
    events = open(os.path.join(logdir, "proxy-events.jsonl"), "w", encoding="utf-8")

    sock_path = os.path.join(logdir, "inject.sock")
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path)
    srv.listen(4)
    srv.setblocking(False)
    clients: dict[socket.socket, bytes] = {}

    def event(direction: str, length: int) -> None:
        events.write(
            json.dumps({"t": time.time(), "dir": direction, "len": length}) + "\n"
        )
        events.flush()

    pid, master = pty.fork()
    if pid == 0:  # child
        try:
            _set_winsize(0, cols, rows)
        except OSError:
            pass
        try:
            os.execvp(cmd[0], cmd)
        finally:
            os._exit(127)

    saved_attrs = None
    try:
        saved_attrs = termios.tcgetattr(0)
        tty.setraw(0)
    except termios.error:
        saved_attrs = None

    resize_pending = False

    def on_winch(_signum, _frame):
        nonlocal resize_pending
        resize_pending = True

    signal.signal(signal.SIGWINCH, on_winch)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))

    os.set_blocking(master, False)
    child_status = 0
    try:
        while True:
            if resize_pending:
                resize_pending = False
                try:
                    data = fcntl.ioctl(
                        0, termios.TIOCGWINSZ, struct.pack("HHHH", rows, cols, 0, 0)
                    )
                    c, r = struct.unpack("HHHH", data)[1], struct.unpack("HHHH", data)[0]
                    if c and r:
                        _set_winsize(master, c, r)
                except OSError:
                    pass

            readers = [master, srv] + list(clients)
            try:
                ready, _, _ = select.select(readers, [], [], 0.05)
            except InterruptedError:
                continue
            except OSError as exc:
                if exc.errno == errno.EINTR:
                    continue
                raise

            for fd in ready:
                if fd is srv:
                    try:
                        conn, _ = srv.accept()
                        conn.setblocking(False)
                        clients[conn] = b""
                    except BlockingIOError:
                        pass
                    continue
                if fd is master:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as exc:
                        if exc.errno in (errno.EIO, errno.EBADF):
                            chunk = b""
                        else:
                            raise
                    if not chunk:
                        # Child closed its PTY; wait for the exit status.
                        _, status = os.waitpid(pid, 0)
                        child_status = status
                        return _exit_status(child_status)
                    app_raw.write(chunk)
                    event("app->host", len(chunk))
                    try:
                        os.write(1, chunk)
                    except OSError:
                        pass
                    continue

                # Control/injection client.
                conn = fd
                try:
                    data = conn.recv(65536)
                except BlockingIOError:
                    continue
                except OSError:
                    data = b""
                if not data:
                    conn.close()
                    clients.pop(conn, None)
                    continue
                buf = clients.get(conn, b"") + data
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    if not line.strip():
                        continue
                    try:
                        msg = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if "resize" in msg:
                        c, r = msg["resize"]
                        _set_winsize(master, int(c), int(r))
                    if "write_b64" in msg:
                        payload = base64.b64decode(msg["write_b64"])
                        os.write(master, payload)
                        event("inject->app", len(payload))
                    if msg.get("ping"):
                        conn.sendall(b'{"ok": true}\n')
                clients[conn] = buf

            # stdin -> child
            try:
                ready_in, _, _ = select.select([0], [], [], 0)
            except (InterruptedError, OSError):
                ready_in = []
            if ready_in:
                try:
                    chunk = os.read(0, 65536)
                except OSError as exc:
                    if exc.errno == errno.EIO:
                        chunk = b""
                    else:
                        raise
                if not chunk:
                    # Tervo went away; hang up the child and exit.
                    try:
                        os.kill(pid, signal.SIGHUP)
                    except ProcessLookupError:
                        pass
                    try:
                        _, status = os.waitpid(pid, 0)
                        return _exit_status(status)
                    except ChildProcessError:
                        return 0
                host_raw.write(chunk)
                event("host->app", len(chunk))
                try:
                    os.write(master, chunk)
                except OSError:
                    pass
    finally:
        try:
            srv.close()
            os.unlink(sock_path)
        except OSError:
            pass
        host_raw.close()
        app_raw.close()
        events.close()
        if saved_attrs is not None:
            try:
                termios.tcsetattr(0, termios.TCSANOW, saved_attrs)
            except termios.error:
                pass


def _exit_status(status: int) -> int:
    if os.WIFEXITED(status):
        return os.WEXITSTATUS(status)
    if os.WIFSIGNALED(status):
        return 128 + os.WTERMSIG(status)
    return 0


if __name__ == "__main__":
    sys.exit(main())
