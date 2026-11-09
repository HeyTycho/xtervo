# ai-echo — Tervo AI Chat reference plugin

A minimal **API v2** process plugin used as the test target for Tervo's built-in
AI Chat panel. It is deterministic, offline, and uses **no API key**.

## What it demonstrates

- `api = 2` manifest, `runtime = "process"`, unchanged JSON Lines protocol V1.
- `[contributes.ai]` metadata: `id`, `name`, `model`, `description`.
- The `ai.chat` host convention:
  - request: `{"type":"command","id":N,"command":"ai.chat","args":[],
    "payload":{"messages":[{"role":"user","content":"..."}],"model":"echo-1"}}`
  - result: `{"type":"result","id":N,"ok":true,"result":{"text":"..."}}`
    (the older top-level `"text"` form also works).
- No network, no credentials, no host key storage.

## Install and try

```sh
mkdir -p ~/.config/tervo/plugins
cp -r docs/examples/ai-echo ~/.config/tervo/plugins/ai-echo
chmod +x ~/.config/tervo/plugins/ai-echo/plugin.py
# restart Tervo
```

Then open **Settings → Plugins → AI Chat**, select **AI Echo**, type a prompt,
and press **Send**. The reply is appended once when the plugin returns; Tervo
does not fake token streaming.

To use another plugin directory:

```sh
TERVO_PLUGINS=/path/to/plugins tervo
```

## Replace with a real provider

Keep the same manifest and protocol shape. Put provider/credential handling in
the plugin process itself (for example read your own API key from an environment
variable or config file outside Tervo). The host sends only `messages` and an
optional `model`; it does not know or forward API keys.
