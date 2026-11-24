#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""xtervo AI Chat reference process plugin.

Protocol: JSON Lines V1 over stdin/stdout (same as any runtime="process" plugin).
AI convention (API v2): host sends

    {"type":"command","id":N,"command":"ai.chat",
     "args":[],
     "payload":{"messages":[{"role":"user","content":"..."}],"model":"echo-1"}}

and this plugin replies with an existing result line, optionally using the
nested text form:

    {"type":"result","id":N,"ok":true,"result":{"text":"..."}}

It is deterministic, offline, and has no credentials. A real AI plugin would
own its API key (for example via its own environment/config file) and call its
provider here; the host never stores or forwards credentials.
"""

import json
import sys


def reply(message_id, text, ok=True):
    line = {
        "type": "result",
        "id": message_id,
        "ok": ok,
        "result": {"text": text},
    }
    print(json.dumps(line, ensure_ascii=False), flush=True)


def handle_ai_chat(message):
    payload = message.get("payload") or {}
    messages = payload.get("messages") or []
    model = payload.get("model") or "(plugin default)"
    user_messages = [
        item for item in messages if item.get("role") == "user" and item.get("content")
    ]
    if not user_messages:
        reply(message["id"], "ai-echo: no user message in payload", ok=False)
        return
    last = str(user_messages[-1]["content"])
    # Deterministic transform: no randomness, no network, no key.
    text = "ai-echo[{model}] {count} message(s): {last}".format(
        model=model,
        count=len(messages),
        last=last,
    )
    reply(message["id"], text)


def main():
    for line in sys.stdin:
        try:
            message = json.loads(line)
        except Exception:
            # Bad lines are the host's signal to disable the plugin after 5;
            # do not crash on a single malformed line.
            continue
        kind = message.get("type")
        if kind == "init":
            print(json.dumps({"type": "ready", "api": 2}), flush=True)
        elif kind == "command":
            if message.get("command") == "ai.chat":
                handle_ai_chat(message)
            elif message.get("command") == "ping":
                reply(message["id"], "pong")
            else:
                reply(message["id"], "unknown command", ok=False)
        elif kind in ("status", "event"):
            # This sample contributes neither status nor events; ignore safely.
            continue
        elif kind == "shutdown":
            break


if __name__ == "__main__":
    main()
