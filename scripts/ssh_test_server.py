#!/usr/bin/env python3
"""最小 SSH 测试服务：key 认证 + shell + direct-tcpip（端口转发），用于本机端到端验收。

依赖：python3-paramiko（Ubuntu 自带包即可）。

用法（在仓库根目录）：
    # 1. 准备密钥与 known_hosts（仅首次）
    mkdir -p /tmp/rt-ssh/.ssh
    ssh-keygen -t ed25519 -f /tmp/rt-ssh/id_ed25519 -N "" -q
    ssh-keygen -t ed25519 -f /tmp/rt-ssh/host_ed25519 -N "" -q
    python3 scripts/ssh_test_server.py --host-key /tmp/rt-ssh/host_ed25519 --port 2222 &
    # 服务启动后打印公钥，可据此写 known_hosts：
    #   [127.0.0.1]:2222 ssh-ed25519 <host_pub_b64 内容>

    # 2. Profile（profiles.toml）：
    #   kind="ssh", host="127.0.0.1", port=2222, user="test", auth="key",
    #   key_path="/tmp/rt-ssh/id_ed25519",
    #   extra_args=["-o","UserKnownHostsFile=/tmp/rt-ssh/.ssh/known_hosts","-o","StrictHostKeyChecking=yes"],
    #   local_forwards=["19000:127.0.0.1:9000"]

    # 3. 验证转发（应用里打开该 Profile 后）：
    printf 'ping\n' | socat - TCP:127.0.0.1:19000     # 期望输出 ECHO:ping

脚本会同时起一个 9000 端口的 ECHO 服务，转发目标就是它。
"""
import argparse
import socket
import threading
import time

import paramiko

DESTINATIONS = {}


class TestServer(paramiko.ServerInterface):
    def check_auth_publickey(self, username, key):
        return paramiko.AUTH_SUCCESSFUL if username == "test" else paramiko.AUTH_FAILED

    def get_allowed_auths(self, username):
        return "publickey"

    def check_channel_request(self, kind, chanid):
        return paramiko.OPEN_SUCCEEDED

    def check_channel_pty_request(self, term, width, height, pw, ph, modes):
        return True

    def check_channel_shell_request(self, channel):
        return True

    def check_channel_direct_tcpip_request(self, chanid, origin, destination):
        DESTINATIONS[chanid] = destination
        print(f"[ssh-test] forward request {chanid} -> {destination}", flush=True)
        return paramiko.OPEN_SUCCEEDED


def bridge(chan, host, port):
    """把转发通道和真实目标连起来；客户端半关闭时只 shutdown 写方向，等回包。"""
    try:
        sock = socket.create_connection((host, port), timeout=5)
    except OSError as e:
        print(f"[ssh-test] target connect failed: {e}", flush=True)
        chan.close()
        return
    done = threading.Event()

    def pump():
        try:
            while True:
                data = sock.recv(4096)
                if not data:
                    break
                chan.sendall(data)
        except OSError:
            pass
        done.set()

    threading.Thread(target=pump, daemon=True).start()
    try:
        while True:
            data = chan.recv(4096)
            if not data:
                break
            sock.sendall(data)
        sock.shutdown(socket.SHUT_WR)
        done.wait(3)
    except OSError:
        pass
    sock.close()
    chan.close()


def serve_shell(chan):
    time.sleep(0.3)
    chan.send(b"SSH-TEST-OK\r\nremote$ ")
    try:
        while True:
            data = chan.recv(4096)
            if not data:
                break
            chan.send(data)
    except OSError:
        pass


def echo_server(port):
    sock = socket.socket()
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", port))
    sock.listen(5)
    print(f"[ssh-test] echo server on 127.0.0.1:{port}", flush=True)
    while True:
        conn, _ = sock.accept()
        try:
            while True:
                data = conn.recv(4096)
                if not data:
                    break
                conn.sendall(b"ECHO:" + data)
        except OSError:
            pass
        conn.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--echo-port", type=int, default=9000)
    ap.add_argument("--host-key", required=True, help="ed25519 私钥文件路径")
    args = ap.parse_args()

    host_key = paramiko.Ed25519Key(filename=args.host_key)
    print(f"[ssh-test] host key: ssh-ed25519 {host_key.get_base64()}", flush=True)

    threading.Thread(target=echo_server, args=(args.echo_port,), daemon=True).start()

    sock = socket.socket()
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", args.port))
    sock.listen(5)
    print(f"[ssh-test] listening on 127.0.0.1:{args.port}", flush=True)

    while True:
        conn, _ = sock.accept()
        transport = paramiko.Transport(conn)
        transport.add_server_key(host_key)
        try:
            transport.start_server(server=TestServer())
        except paramiko.SSHException as e:
            print(f"[ssh-test] handshake failed: {e}", flush=True)
            continue
        while True:
            chan = transport.accept(0.5)
            if chan is None:
                if not transport.is_active():
                    break
                continue
            cid = chan.get_id()
            if cid in DESTINATIONS:
                threading.Thread(target=bridge, args=(chan, *DESTINATIONS[cid]), daemon=True).start()
            else:
                threading.Thread(target=serve_shell, args=(chan,), daemon=True).start()


if __name__ == "__main__":
    main()
