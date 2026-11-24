# Echo — Plugin API v3 reference example

This directory is a complete, executable Plugin API v3 process plugin. It uses
only the Python standard library and demonstrates:

- `api = 3` / `host_abi = 3` / `runtime = "process"` manifest;
- `initialize` / `initialized` JSON-RPC 2.0 handshake;
- commands (`greet`, `context`, `read-terminal`, `panel`, `notify`);
- `status.get` + `ui.status`, `ping`, `shutdown`;
- events (`startup`, `tab-open`, `tab-close`, `pane-focus`);
- declarative panel + settings documents (host renders every widget);
- `ui.notify`, `context.version`, `terminal.read` (per-request Allow/Deny);
- `ui.action` callbacks from host-rendered buttons/inputs/selects.

## Install

```bash
mkdir -p ~/.config/xtervo/plugins
cp -r docs/examples/plugin-v3-echo ~/.config/xtervo/plugins/echo
chmod +x ~/.config/xtervo/plugins/echo/main.py
```

Start xtervo → **Settings → Plugins** → **Enable** → review the permissions
(`filesystem=plugin`, `terminal_context`, `credentials=own`) → **Allow and
enable**. Commands appear in the Command Palette as `Echo: …`.

Or point xtervo at a plugins root:

```bash
XTERVO_PLUGINS=$PWD/docs/examples XTERVO_CONFIG=/tmp/xtervo-v3.toml \
  cargo run -p xtervo-app --bin xtervo
```

The directory name (`plugin-v3-echo`) does not matter; `id = "dev.xtervo.echo"`
is the stable plugin ID.

## Security

This plugin runs as your user with your full system permissions. Permissions in
the manifest are capability declarations for host-mediated APIs plus a one-time
user confirmation — **not an OS sandbox**. The host never provides Vault or
environment secrets. See [`../../PLUGIN_V3.md`](../../PLUGIN_V3.md).

MCP is a built-in xtervo feature and is **not** available as a plugin
capability.

## Protocol notes

Read [`../../PLUGIN_V3.md`](../../PLUGIN_V3.md) for the complete method table,
error codes, limits, declarative UI schema, and migration notes. The plugin
writes one compact JSON object per line to stdout and must never write logs to
stdout (use stderr).
