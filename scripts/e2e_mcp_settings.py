#!/usr/bin/env python3
"""Black-box MCP config/permission e2e.

Verifies the scheduled MCP settings work end to end:
  * default startup creates $XDG_RUNTIME_DIR/tervo/mcp.sock (0600) and
    $XDG_STATE_HOME/tervo/mcp.token (0600) with no TERVO_DIAG_SOCKET env;
  * `tervo diag serve` with no args discovers both and lists only read tools;
  * input/tabs/destructive tools are rejected by default;
  * editing config hot-applies grants/tightening without restarting Tervo;
  * tools/list follows app capabilities (same bridge process);
  * master off removes the socket and all live tools;
  * master on recreates it; wrong token is rejected;
  * mcp_enabled=false at startup creates no socket;
  * missing XDG_RUNTIME_DIR falls back to the state dir socket.

Requires a display; scripts/e2e_mcp_settings.sh starts Xvfb.
"""
import json
import os
import pathlib
import select
import socket
import stat
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
BIN = pathlib.Path(os.environ.get("TERVO_BIN", ROOT / "target/release/tervo"))
OUT = pathlib.Path(os.environ.get("E2E_OUT", ROOT / ".logs/mcp-settings-e2e"))
OUT.mkdir(parents=True, exist_ok=True)
HOME = OUT / "home"
# Unix socket paths are limited to ~108 bytes; the worktree artifact path is
# longer, so keep the runtime dirs on the (short) temp path used by this e2e.
STATE = pathlib.Path(f"/tmp/tervo-mcp-e2e-state-{os.getpid()}")
RUN = pathlib.Path(f"/tmp/tervo-mcp-e2e-run-{os.getpid()}")
LOGS = OUT / "logs"
CONFIG = OUT / "config.toml"
for directory in (HOME, STATE, RUN, LOGS):
    directory.mkdir(parents=True, exist_ok=True)
RUN.chmod(0o700)
STATE.chmod(0o700)
SOCKET = RUN / "tervo" / "mcp.sock"
TOKEN_FILE = STATE / "tervo" / "mcp.token"


def write_config(**flags):
    lines = ["version = 1", 'language = "en"']
    for key, value in flags.items():
        lines.append(f"{key} = {'true' if value else 'false'}")
    CONFIG.write_text("\n".join(lines) + "\n")


def base_env(**extra):
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("TERVO_DIAG_") or key.startswith("TERVO_GPU"):
            env.pop(key, None)
    for key in ("SLINT_BACKEND", "XDG_RUNTIME_DIR", "XDG_STATE_HOME", "XDG_CONFIG_HOME"):
        env.pop(key, None)
    env.update(
        {
            "SLINT_BACKEND": "winit-software",
            "HOME": str(HOME),
            "XDG_STATE_HOME": str(STATE),
            "XDG_RUNTIME_DIR": str(RUN),
            "XDG_CACHE_HOME": str(OUT / "cache"),
            "XDG_CONFIG_HOME": str(OUT / "xdg"),
            "TERVO_CONFIG": str(CONFIG),
            "TERVO_LOG_DIR": str(LOGS),
        }
    )
    env.update(extra)
    return env


def wait_until(predicate, description, timeout=8.0, interval=0.1):
    deadline = time.time() + timeout
    last_error = None
    while time.time() < deadline:
        try:
            if predicate():
                return
        except OSError as error:
            last_error = error
        time.sleep(interval)
    raise AssertionError(f"timeout waiting for {description}; last_error={last_error!r}")


def connectable(path):
    try:
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        probe.settimeout(0.5)
        probe.connect(str(path))
        probe.close()
        return True
    except OSError:
        return False


def raw(request, path=SOCKET, timeout=4.0):
    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stream.settimeout(timeout)
    stream.connect(str(path))
    stream.sendall((json.dumps(request) + "\n").encode())
    data = b""
    while not data.endswith(b"\n"):
        chunk = stream.recv(65536)
        if not chunk:
            break
        data += chunk
    stream.close()
    return json.loads(data.decode().strip())


class Bridge:
    def __init__(self, args):
        self.stderr_path = OUT / f"bridge-{int(time.time() * 1000)}.log"
        self.stderr_file = open(self.stderr_path, "w")
        self.process = subprocess.Popen(
            [str(BIN), "diag", "serve", *args],
            env=base_env(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
            text=True,
            bufsize=1,
        )
        self.next_id = 1

    def rpc(self, method, params=None):
        request_id = self.next_id
        self.next_id += 1
        request = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            request["params"] = params
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        ready, _, _ = select.select([self.process.stdout], [], [], 8.0)
        if not ready:
            raise AssertionError(
                f"MCP bridge did not answer {method}; stderr={self.stderr()}"
            )
        line = self.process.stdout.readline()
        if not line:
            raise AssertionError(f"MCP bridge closed stdout; stderr={self.stderr()}")
        return json.loads(line)

    def tools(self):
        response = self.rpc("tools/list", {})
        return {tool["name"] for tool in response["result"]["tools"]}

    def call(self, name, arguments=None):
        response = self.rpc(
            "tools/call", {"name": name, "arguments": arguments or {}}
        )
        return response["result"]

    def stderr(self):
        try:
            return self.stderr_path.read_text(errors="replace")[-2000:]
        except OSError:
            return "<unreadable>"

    def close(self):
        try:
            self.process.stdin.close()
        except OSError:
            pass
        try:
            self.process.terminate()
        except OSError:
            pass
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
        self.stderr_file.close()


def is_error(result):
    return result.get("isError") is True


def error_text(result):
    return result.get("content", [{}])[0].get("text", "")


def tool_names(tools):
    return sorted(name for name in tools if name.startswith("tervo.live_"))


def wait_capabilities(flag, value, timeout=8.0):
    def check():
        response = raw({"id": 1, "cmd": "capabilities", "token": token, "probe": True})
        return response.get("ok") is True and response["data"].get(flag) is value

    wait_until(check, f"capabilities {flag}={value}", timeout=timeout)


print(f"artifact dir: {OUT}")
write_config(
    mcp_enabled=True,
    mcp_read=True,
    mcp_input=False,
    mcp_tabs=False,
    mcp_destructive=False,
)
for path in (SOCKET, TOKEN_FILE):
    try:
        path.unlink()
    except FileNotFoundError:
        pass

app = subprocess.Popen(
    [str(BIN), "--cols", "100", "--rows", "24", "--shell", "/bin/bash"],
    env=base_env(),
    stdout=open(OUT / "app.log", "w"),
    stderr=subprocess.STDOUT,
)
bridge = None
second_bridge = None
try:
    wait_until(lambda: SOCKET.exists(), f"default socket {SOCKET}", timeout=15)
    wait_until(lambda: connectable(SOCKET), "default socket connectable")
    wait_until(lambda: TOKEN_FILE.is_file(), f"state token {TOKEN_FILE}")
    socket_mode = stat.S_IMODE(SOCKET.stat().st_mode)
    token_mode = stat.S_IMODE(TOKEN_FILE.stat().st_mode)
    assert socket_mode == 0o600, f"socket mode {oct(socket_mode)} != 0o600"
    assert token_mode == 0o600, f"token mode {oct(token_mode)} != 0o600"
    token = TOKEN_FILE.read_text().strip()
    assert len(token) >= 32, "state token too short"
    app_log = (OUT / "app.log").read_text(errors="replace")
    assert token not in app_log, "app log leaked the state token"
    assert token not in CONFIG.read_text(), "config leaked the state token"
    print("PASS default socket/token: 0600, no env socket, token not logged")

    # Wrong token is rejected even before permissions are considered.
    unauthorized = raw({"id": 1, "cmd": "capabilities", "token": "wrong"})
    assert unauthorized["ok"] is False and unauthorized["error"] == "unauthorized", unauthorized
    capabilities = raw({"id": 2, "cmd": "capabilities", "token": token})
    assert capabilities["ok"] is True, capabilities
    data = capabilities["data"]
    assert data["enabled"] is True and data["read_only"] is True, data
    assert data["commands"] == ["capabilities", "snapshot", "list", "output"], data
    print("PASS raw capabilities: read-only default, wrong token rejected")

    bridge = Bridge([])
    assert bridge.rpc("initialize", {"protocolVersion": "2024-11-05"})["result"]
    tools = bridge.tools()
    required_read = {
        "tervo.live_capabilities",
        "tervo.live_snapshot",
        "tervo.live_list",
        "tervo.live_output",
    }
    assert required_read <= tools, f"missing read tools: {sorted(required_read - tools)}"
    for forbidden in (
        "tervo.live_send_text",
        "tervo.live_send_key",
        "tervo.live_run",
        "tervo.live_new_tab",
        "tervo.live_split",
        "tervo.live_focus",
        "tervo.live_scroll",
        "tervo.live_close_pane",
        "tervo.live_close_tab",
        "tervo.live_kill",
    ):
        assert forbidden not in tools, f"default tool list leaked {forbidden}"
    listed = bridge.call("tervo.live_list")
    assert not is_error(listed), listed
    panes = json.loads(listed["content"][0]["text"])["panes"]
    assert panes, "no panes in live_list"
    pane = panes[0]["pane_id"]
    denied = bridge.call("tervo.live_send_text", {"pane_id": pane, "text": "echo NOPE"})
    assert is_error(denied) and "input disabled" in error_text(denied), denied
    denied = bridge.call("tervo.live_close_pane", {"pane_id": pane})
    assert is_error(denied) and "disabled" in error_text(denied), denied
    print("PASS bridge no-arg discovery: read tools only, write calls rejected")

    # Hot grant input+tabs+destructive from an external config edit.
    write_config(
        mcp_enabled=True,
        mcp_read=True,
        mcp_input=True,
        mcp_tabs=True,
        mcp_destructive=True,
    )
    wait_capabilities("input_enabled", True)
    wait_capabilities("tabs_enabled", True)
    wait_capabilities("destructive_enabled", True)
    tools = bridge.tools()
    for required in (
        "tervo.live_send_text",
        "tervo.live_send_key",
        "tervo.live_run",
        "tervo.live_new_tab",
        "tervo.live_split",
        "tervo.live_focus",
        "tervo.live_scroll",
        "tervo.live_close_pane",
        "tervo.live_close_tab",
        "tervo.live_kill",
    ):
        assert required in tools, f"hot grant did not expose {required}"
    run = bridge.call(
        "tervo.live_run",
        {"pane_id": pane, "command": "printf MCP_SETTINGS_OK"},
    )
    assert not is_error(run), run
    time.sleep(1.0)
    output = bridge.call("tervo.live_output", {"pane_id": pane, "max_bytes": 65536})
    text = json.loads(output["content"][0]["text"])["text"]
    assert "MCP_SETTINGS_OK" in text, text[-800:]
    before = {item["pane_id"] for item in panes}
    new_tab = bridge.call("tervo.live_new_tab")
    assert not is_error(new_tab), new_tab
    wait_until(
        lambda: {
            item["pane_id"]
            for item in json.loads(bridge.call("tervo.live_list")["content"][0]["text"])["panes"]
        }
        - before,
        "new tab pane",
    )
    new_pane = next(
        iter(
            {
                item["pane_id"]
                for item in json.loads(
                    bridge.call("tervo.live_list")["content"][0]["text"]
                )["panes"]
            }
            - before
        )
    )
    closed = bridge.call("tervo.live_close_tab", {"pane_id": new_pane})
    assert not is_error(closed), closed
    print("PASS hot grant: input/tabs/destructive appear and work without restart")

    # Tighten input only: input tools (and kill, which needs input) disappear
    # immediately; close (tabs+destructive) stays.
    write_config(
        mcp_enabled=True,
        mcp_read=True,
        mcp_input=False,
        mcp_tabs=True,
        mcp_destructive=True,
    )
    wait_capabilities("input_enabled", False)
    tools = bridge.tools()
    for forbidden in (
        "tervo.live_send_text",
        "tervo.live_send_key",
        "tervo.live_run",
        "tervo.live_kill",
    ):
        assert forbidden not in tools, f"tightened input still lists {forbidden}"
    assert "tervo.live_close_pane" in tools, "close should remain with tabs+destructive"
    denied = bridge.call("tervo.live_run", {"pane_id": pane, "command": "echo NOPE"})
    assert is_error(denied) and "input disabled" in error_text(denied), denied
    print("PASS hot tighten: input tools vanish, kill depends on input, direct call denied")

    # read=false + input=true: probe still works (metadata only) but read tools vanish.
    write_config(
        mcp_enabled=True,
        mcp_read=False,
        mcp_input=True,
        mcp_tabs=False,
        mcp_destructive=False,
    )
    wait_capabilities("read_enabled", False)
    tools = bridge.tools()
    assert "tervo.live_list" not in tools and "tervo.live_capabilities" not in tools, tools
    assert "tervo.live_send_text" in tools, tools
    denied = bridge.call("tervo.live_list")
    assert is_error(denied) and "read disabled" in error_text(denied), denied
    denied = bridge.call("tervo.live_capabilities")
    assert is_error(denied) and "read disabled" in error_text(denied), denied
    sent = bridge.call("tervo.live_send_text", {"pane_id": pane, "text": "echo READ_OFF"})
    assert not is_error(sent), sent
    print("PASS read=false: only input tools exposed, read calls rejected by app")

    # Restore read-only before the master-off phase.
    write_config(
        mcp_enabled=True,
        mcp_read=True,
        mcp_input=False,
        mcp_tabs=False,
        mcp_destructive=False,
    )
    wait_capabilities("read_enabled", True)

    # Master off: socket is removed, no live tools, direct calls fail clearly.
    write_config(
        mcp_enabled=False,
        mcp_read=True,
        mcp_input=False,
        mcp_tabs=False,
        mcp_destructive=False,
    )
    wait_until(lambda: not SOCKET.exists(), "socket removed after mcp_enabled=false", timeout=8)
    assert not connectable(SOCKET), "socket still connectable after master off"
    tools = bridge.tools()
    assert tool_names(tools) == [], f"master off still lists live tools: {tools}"
    denied = bridge.call("tervo.live_list")
    assert is_error(denied), denied
    assert "socket" in error_text(denied).lower() or "拒绝" in error_text(denied), denied
    print("PASS master off: socket removed, live tools gone, call errors clearly")

    # Master on again in the same app/bridge: socket and read tools come back.
    write_config(
        mcp_enabled=True,
        mcp_read=True,
        mcp_input=False,
        mcp_tabs=False,
        mcp_destructive=False,
    )
    wait_until(lambda: SOCKET.exists() and connectable(SOCKET), "socket recreated", timeout=8)
    tools = bridge.tools()
    assert required_read <= tools, f"read tools did not return: {sorted(required_read - tools)}"
    print("PASS master on: socket/tools recreated without restarting Tervo")

    # Wrong token bridge: capability probe fails, live tools stay hidden, calls fail.
    second_bridge = Bridge(["--socket", str(SOCKET), "--token", "wrong-token"])
    second_bridge.rpc("initialize", {})
    assert tool_names(second_bridge.tools()) == [], "wrong token exposed live tools"
    denied = second_bridge.call("tervo.live_list")
    assert is_error(denied) and "unauthorized" in error_text(denied), denied
    print("PASS wrong token bridge: live tools hidden and tools/call rejected")

    # Missing XDG_RUNTIME_DIR fallback: app + bridge use state-dir socket.
    bridge.close()
    bridge = None
    second_bridge.close()
    second_bridge = None
    app.terminate()
    try:
        app.wait(timeout=5)
    except subprocess.TimeoutExpired:
        app.kill()
    try:
        SOCKET.unlink()
    except FileNotFoundError:
        pass
    fallback_socket = STATE / "tervo" / "mcp.sock"
    try:
        fallback_socket.unlink()
    except FileNotFoundError:
        pass
    env = base_env()
    env.pop("XDG_RUNTIME_DIR", None)
    fallback_app = subprocess.Popen(
        [str(BIN), "--cols", "100", "--rows", "24", "--shell", "/bin/bash"],
        env=env,
        stdout=open(OUT / "app-fallback.log", "w"),
        stderr=subprocess.STDOUT,
    )
    try:
        wait_until(
            lambda: fallback_socket.exists() and connectable(fallback_socket),
            "state-dir fallback socket",
            timeout=15,
        )
        fallback_token = TOKEN_FILE.read_text().strip()
        fallback_cap = raw(
            {"id": 1, "cmd": "capabilities", "token": fallback_token, "probe": True},
            path=fallback_socket,
        )
        assert fallback_cap["ok"] is True, fallback_cap
        assert stat.S_IMODE(fallback_socket.stat().st_mode) == 0o600
        print("PASS fallback: no XDG_RUNTIME_DIR -> $XDG_STATE_HOME/tervo/mcp.sock")
    finally:
        fallback_app.terminate()
        try:
            fallback_app.wait(timeout=5)
        except subprocess.TimeoutExpired:
            fallback_app.kill()
        try:
            fallback_socket.unlink()
        except FileNotFoundError:
            pass

    # Startup with mcp_enabled=false: no socket even when TERVO_DIAG_SOCKET is set
    # by the environment (master switch is authoritative).
    marker = OUT / "must-not-exist.sock"
    try:
        marker.unlink()
    except FileNotFoundError:
        pass
    write_config(
        mcp_enabled=False,
        mcp_read=True,
        mcp_input=False,
        mcp_tabs=False,
        mcp_destructive=False,
    )
    disabled_env = base_env(**{"TERVO_DIAG_SOCKET": str(marker)})
    disabled_app = subprocess.Popen(
        [str(BIN), "--cols", "100", "--rows", "24", "--shell", "/bin/bash"],
        env=disabled_env,
        stdout=open(OUT / "app-disabled.log", "w"),
        stderr=subprocess.STDOUT,
    )
    try:
        time.sleep(1.5)
        assert not marker.exists(), "mcp_enabled=false still created TERVO_DIAG_SOCKET"
        assert not SOCKET.exists(), "mcp_enabled=false still created default socket"
        disabled_bridge = Bridge([])
        assert tool_names(disabled_bridge.tools()) == [], "disabled startup exposed live tools"
        denied = disabled_bridge.call("tervo.live_list")
        assert is_error(denied), denied
        disabled_bridge.close()
        print("PASS startup mcp_enabled=false: no socket, offline-only bridge")
    finally:
        disabled_app.terminate()
        try:
            disabled_app.wait(timeout=5)
        except subprocess.TimeoutExpired:
            disabled_app.kill()

    print(f"PASS: MCP settings e2e; artifacts={OUT}")
finally:
    if second_bridge is not None:
        second_bridge.close()
    if bridge is not None:
        bridge.close()
    app.terminate()
    try:
        app.wait(timeout=5)
    except subprocess.TimeoutExpired:
        app.kill()
