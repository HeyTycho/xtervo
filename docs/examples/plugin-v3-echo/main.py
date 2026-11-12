#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Reference Tervo Plugin API v3 process plugin.

Protocol: JSON-RPC 2.0, one UTF-8 JSON object per line on stdin/stdout.
The host starts this process, sends initialize/initialized, then commands,
events, status polls and pings. This file intentionally uses only the Python
standard library so it can be copied into ~/.config/tervo/plugins/echo/.

Notes:
- Permissions are capability gates for host-mediated APIs, not an OS sandbox.
- terminal.read/terminal.write are always ask/allow; this example only reads.
- The host never provides Vault/environment secrets. This plugin owns nothing.
"""

import json
import os
import sys

PROTOCOL_VERSION = 3
_next_id = 100
_pending = {}
_panel_open = False
_settings = {"name": "world", "mode": "friendly", "progress": 0.35}


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def respond(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def respond_error(request_id, code, message):
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": code, "message": message},
        }
    )


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def request(method, params):
    global _next_id
    request_id = _next_id
    _next_id += 1
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }
    )
    return request_id


def panel_doc(title="Echo panel"):
    return {
        "version": 1,
        "title": title,
        "elements": [
            {"type": "markdown", "text": "## Echo v3\n**Declarative UI**, rendered by the host."},
            {"type": "text", "text": "No Slint/WASM/HTML is accepted from plugins."},
            {"type": "list", "items": ["commands", "status", "events", "panels", "settings"]},
            {
                "type": "input",
                "id": "name",
                "label": "Name",
                "value": _settings["name"],
                "placeholder": "type a name",
            },
            {
                "type": "select",
                "id": "mode",
                "label": "Mode",
                "value": _settings["mode"],
                "options": ["friendly", "quiet", "loud"],
            },
            {"type": "progress", "label": "Progress (host-rendered)", "value": _settings["progress"]},
            {"type": "button", "id": "refresh", "label": "Send updated panel"},
            {"type": "button", "id": "context", "label": "Ask host version"},
        ],
    }


def settings_doc():
    return {
        "version": 1,
        "title": "Echo settings",
        "elements": [
            {"type": "text", "text": "Settings are a declarative schema; all widgets are host-owned."},
            {
                "type": "input",
                "id": "name",
                "label": "Display name",
                "value": _settings["name"],
            },
            {
                "type": "select",
                "id": "mode",
                "label": "Mode",
                "value": _settings["mode"],
                "options": ["friendly", "quiet", "loud"],
            },
            {"type": "progress", "label": "Sample progress", "value": _settings["progress"]},
            {"type": "button", "id": "save", "label": "Save (demo)"},
        ],
    }


def push_panel():
    request(
        "ui.render",
        {
            "surface": "panel",
            "panel": "echo",
            "elements": panel_doc()["elements"],
            "title": "Echo panel",
        },
    )


def handle_response(message):
    pending = _pending.pop(message.get("id"), None)
    if pending is None:
        return
    kind, command_id = pending
    if "error" in message:
        error = message["error"]
        respond(
            command_id,
            {"ok": False, "message": f"{kind} failed: {error.get('message', 'error')}"},
        )
        return
    result = message.get("result")
    if kind == "terminal.read":
        text = (result or {}).get("text", "")
        respond(command_id, {"ok": True, "message": f"terminal tail ({len(text)} chars):\n{text}"})
    elif kind == "terminal.write":
        respond(command_id, {"ok": True, "message": "terminal.write accepted"})
    elif kind == "context.version":
        respond(
            command_id,
            {
                "ok": True,
                "message": "host=%s api=%s abi=%s protocol=%s"
                % (
                    result.get("host_version"),
                    result.get("api"),
                    result.get("host_abi"),
                    result.get("protocol_version"),
                ),
            },
        )
    elif kind == "ui.render":
        respond(command_id, {"ok": True, "message": "panel rendered by host"})
    elif kind == "context.status":
        notify(
            "ui.notify",
            {"text": "Host version: %s" % (result or {}).get("host_version"), "level": "info"},
        )
    else:
        respond(command_id, {"ok": True, "message": json.dumps(result)[:500]})


def handle_command(request_id, params):
    command = params.get("command", "")
    if command == "greet":
        respond(request_id, {"ok": True, "message": "Hello from Echo Plugin API v3"})
    elif command == "notify":
        notify("ui.notify", {"text": "Echo says hi", "level": "info"})
        respond(request_id, {"ok": True, "message": "notification sent"})
    elif command == "context":
        host_id = request("context.version", {})
        _pending[host_id] = ("context.version", request_id)
    elif command == "read-terminal":
        host_id = request("terminal.read", {"max_lines": 20, "max_bytes": 8192})
        _pending[host_id] = ("terminal.read", request_id)
    elif command == "panel":
        push_panel()
        respond(request_id, {"ok": True, "message": "ui.render request sent"})
    else:
        respond_error(request_id, -32602, "unknown command %r" % command)


def handle_request(message):
    method = message.get("method", "")
    request_id = message.get("id")
    params = message.get("params") or {}
    if method == "initialize":
        respond(request_id, {"protocol_version": PROTOCOL_VERSION, "name": "echo"})
    elif method == "initialized":
        notify("ui.status", {"text": "Echo v3 ready", "command": "greet"})
    elif method == "command.invoke":
        handle_command(request_id, params)
    elif method == "status.get":
        respond(
            request_id,
            {
                "text": "Echo v3 ready",
                "tooltip": "Reference plugin: status.get / ui.status",
                "command": "greet",
            },
        )
    elif method == "ping":
        respond(request_id, {"seq": params.get("seq", 0)})
    elif method == "surface.render":
        surface = params.get("surface")
        if surface == "settings":
            respond(request_id, settings_doc())
        else:
            respond(request_id, panel_doc())
    elif method == "shutdown":
        respond(request_id, {"ok": True})
        sys.exit(0)
    elif method == "ui.action" and request_id is None:
        handle_action(params)
    elif method == "event.deliver" and request_id is None:
        event = params.get("event")
        if event == "tab-open":
            notify("ui.status", {"text": "Echo v3 ready", "command": "greet"})
    else:
        if request_id is not None:
            respond_error(request_id, -32601, "unknown method %r" % method)


def handle_action(params):
    element = params.get("element")
    action = params.get("action")
    value = params.get("value")
    if element in ("name", "mode") and action == "change" and value is not None:
        _settings[element] = value
        surface = params.get("surface", "panel")
        surface = surface.split(":", 1)[-1] if surface.startswith("panel:") else surface
        doc = settings_doc() if surface == "settings" else panel_doc()
        request(
            "ui.render",
            {
                "surface": "settings" if surface == "settings" else "panel",
                "panel": "echo",
                "title": doc["title"],
                "elements": doc["elements"],
            },
        )
    elif element == "refresh":
        push_panel()
    elif element == "context":
        # Reuse context.version and report through status.
        host_id = request("context.version", {})
        _pending[host_id] = ("context.status", 0)
    elif element == "save":
        notify("ui.notify", {"text": "Echo settings saved (demo)", "level": "info"})


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            # Never answer malformed input; the host counts bad lines and will
            # restart this process after its configured limit.
            continue
        if not isinstance(message, dict):
            continue
        if "method" not in message:
            handle_response(message)
        else:
            handle_request(message)


if __name__ == "__main__":
    # The host clears the environment except a safe allowlist; do not rely on
    # API keys being present. plugins own their credentials.
    os.chdir(os.path.dirname(os.path.abspath(__file__)))
    main()
