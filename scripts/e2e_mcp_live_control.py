#!/usr/bin/env python3
"""Black-box MCP live-control e2e: app live socket + MCP stdio bridge.

Requires a display (the shell wrapper starts Xvfb) and a release `tervo`.
Writes artifacts under target/mcp-live-e2e; exits nonzero on any assertion.
"""
import json
import os
import pathlib
import socket
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
BIN = pathlib.Path(os.environ.get("TERVO_BIN", ROOT / "target/release/tervo"))
OUT = pathlib.Path(os.environ.get("E2E_OUT", ROOT / "target/mcp-live-e2e"))
OUT.mkdir(parents=True, exist_ok=True)
SOCK = OUT / "diag.sock"
# Remove a stale socket from a previous run; Unix socket paths survive the
# process and would otherwise make `exists()` lie about listener readiness.
try:
    SOCK.unlink()
except FileNotFoundError:
    pass
TOKEN = "mcp-e2e-token"
APP_LOG = OUT / "app.log"

env = os.environ.copy()
for key in ("SLINT_BACKEND", "TERVO_GPU", "TERVO_GPU_NATIVE", "TERVO_CPU"):
    env.pop(key, None)
env.update({
    "SLINT_WGPU_CPU": "1",
    "TERVO_DIAG_SOCKET": str(SOCK),
    "TERVO_DIAG_TOKEN": TOKEN,
    "TERVO_DIAG_ALLOW_WRITE": "1",
    "TERVO_DIAG_ALLOW_DESTRUCTIVE": "1",
    "TERVO_CONFIG": str(OUT / "config.toml"),
    "TERVO_LOG_DIR": str(OUT),
    "XDG_CONFIG_HOME": str(OUT / "xdg"),
    "XDG_DATA_HOME": str(OUT / "data"),
    "XDG_CACHE_HOME": str(OUT / "cache"),
})

app = subprocess.Popen(
    [str(BIN), "--cols", "80", "--rows", "24", "--shell", "/bin/bash"],
    env=env, stdout=open(APP_LOG, "w"), stderr=subprocess.STDOUT,
)
mcp = None
try:
    for _ in range(300):
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            probe.connect(str(SOCK))
        except OSError:
            probe.close()
            time.sleep(0.05)
            continue
        probe.close()
        break
    else:
        raise SystemExit("live socket did not become connectable")

    mcp = subprocess.Popen(
        [str(BIN), "diag", "serve", "--socket", str(SOCK), "--token", TOKEN],
        env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True,
    )

    def rpc(obj):
        mcp.stdin.write(json.dumps(obj) + "\n")
        mcp.stdin.flush()
        line = mcp.stdout.readline()
        if not line:
            raise SystemExit("MCP server closed stdout")
        return json.loads(line)

    init = rpc({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    assert init["result"]["protocolVersion"]

    tools = rpc({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    names = {tool["name"] for tool in tools["result"]["tools"]}
    required = {
        "tervo.live_list", "tervo.live_output", "tervo.live_run",
        "tervo.live_send_text", "tervo.live_send_key", "tervo.live_new_tab",
        "tervo.live_split", "tervo.live_focus", "tervo.live_scroll",
        "tervo.live_close_pane", "tervo.live_close_tab", "tervo.live_kill",
    }
    missing = required - names
    assert not missing, f"missing MCP tools: {sorted(missing)}"

    listed = rpc({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                  "params": {"name": "tervo.live_list", "arguments": {}}})
    assert not listed["result"].get("isError"), listed
    data = json.loads(listed["result"]["content"][0]["text"])
    assert data["panes"], "no panes in live_list"
    pane = data["panes"][0]["pane_id"]

    run = rpc({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
               "params": {"name": "tervo.live_run",
                          "arguments": {"pane_id": pane, "command": "printf MCP_OK"}}})
    assert not run["result"].get("isError"), run
    time.sleep(1.2)
    output = rpc({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                  "params": {"name": "tervo.live_output",
                             "arguments": {"pane_id": pane, "max_bytes": 65536}}})
    text = json.loads(output["result"]["content"][0]["text"])["text"]
    assert "MCP_OK" in text, text[:400]
    print("PASS MCP live control")
    print(f"artifacts: {OUT}")
finally:
    if mcp is not None:
        try:
            mcp.terminate()
        except OSError:
            pass
    app.terminate()
    try:
        app.wait(timeout=5)
    except subprocess.TimeoutExpired:
        app.kill()
