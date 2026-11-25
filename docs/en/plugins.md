# Plugins

xtervo plugins extend the terminal without changing its core. They can add commands and shortcuts, a status-bar line, host-rendered panels and settings, terminal context-menu items, themes, and — as the transport seam matures — pane connection backends. xtervo works as an ordinary local terminal with no plugins installed.

The only supported plugin protocol is **Plugin API v3**. For methods, payloads, error codes and the declarative UI schema, see [Plugin API v3](plugin-api-v3.md).

## What plugins can do

| Capability | Declared in `plugin.toml` | What you see |
|---|---|---|
| Commands | `[[commands]]` and `commands = true` under `[contributes]` | Command Palette entries using the declared `title`; full ID `plugin.<plugin id>.<command id>` |
| Shortcuts | `keybinding` on a command | An optional default chord, which can be overridden in Settings → Keys |
| Status bar | `status = true` under `[contributes]` | One line per plugin, updated by `status.get` every 2 s or pushed with `ui.status`; clicking it runs its command |
| Panels | `panels = ["<id>"]` under `[contributes]` | A host-rendered panel opened with `Panel…` |
| Settings | `settings = true` under `[contributes]` | A host-rendered settings form opened with `Settings…` |
| Context menu | `menu = true` on a command | An extra item at the end of the terminal right-click menu |
| Events | `events = ["startup", "tab-open", "tab-close", "pane-focus"]` under `[contributes]` | Notifications about startup, tabs and focus |
| Theme | `theme = "theme.toml"` under `[contributes]` | A theme added at xtervo startup (not hot-reloaded) |
| Output filter (WASM only) | `runtime = "wasm"` and `[permissions.terminal] output_filter = true` | A narrow byte filter in the terminal output path |

Panels and settings are declarative documents (`text`, `markdown`, `list`, `input`, `button`, `select`, `progress`) rendered entirely by the host; plugins cannot ship Slint, HTML or arbitrary layout. The current UI exposes one `Panel…` entry per plugin (the first declared panel ID).

**Transports.** The core has a pane transport seam (`PaneTransport`, `Backend::Plugin`) that lets a plugin-backed backend supply a pane's bytes instead of a PTY, and the official SSH plugin POC uses it. The v3 process protocol does not expose transport methods yet: `transport.open` and `permissions.terminal_transport` are still being designed, so third-party transport plugins are not supported today. See the [transport plugin design (Chinese)](https://github.com/HeyTycho/xtervo/blob/main/docs/PLUGIN_TRANSPORT.md).

MCP is built into xtervo; it is not a plugin capability ([MCP control](mcp.md)).

## Compatibility: API v3 only

- Both `api = 3` and `host_abi = 3` are required. The handshake must also report `protocol_version` 3.
- v1/v2 manifests are not loaded and are never executed. Settings → Plugins lists them as `unsupported` with a migration hint; there is no v1/v2 compatibility layer.
- Legacy flat top-level `*.toml` manifests are also `unsupported`.
- Parsing is strict: an unknown field, a bad `host_abi`, or an invalid `entry` makes the plugin `invalid`, and it is not executed. Duplicate IDs keep the first plugin and mark the rest invalid.
- `runtime` is `"process"` (a long-running child process speaking JSON-RPC 2.0 over line-delimited stdio) or `"wasm"` (a narrow, import-free output filter). The old `oneshot` runtime was removed with v1.

## Package layout

```text
my-plugin/
├── plugin.toml          # required manifest
├── plugin.toml.sig      # optional Ed25519 signature
├── main.py              # entrypoint; executable on Unix for process plugins
└── theme.toml           # optional, only if contributed
```

Required manifest fields:

| Field | Rules |
|---|---|
| `api` | Must be `3` |
| `host_abi` | Must be `3` |
| `id` | Starts with `[A-Za-z0-9]`; `A-Za-z0-9._-` only; ≤64 characters; unique |
| `name` | Non-empty; ≤128 characters |
| `version` | Non-empty; ≤64 characters; not interpreted as semver |
| `runtime` | `"process"` or `"wasm"` |
| `entry` | Relative path inside the plugin directory |

`description` is optional (≤512 characters). A process `entry` must be a regular executable file inside the plugin directory (Unix needs the execute bit); a WASM `entry` must be a `.wasm` file no larger than 8 MiB and does not need the execute bit. `plugin.toml` must be at most 64 KiB and cannot be a symlink. Absolute paths, `..`, backslashes and symlink escapes are rejected.

Install sources are a directory, `.zip` (store/deflate only), `.tar`, `.tar.gz` or `.tgz`. The package must contain `plugin.toml` at its root, or exactly one top-level directory that contains it. The installer rejects symlinks, hardlinks, device/FIFO entries and non-UTF-8 names, and enforces 2,048 files, 4,096 total entries, 16 MiB per file and 64 MiB unpacked. Preserve mode `0755` on the entrypoint when publishing, or the install is rejected.

## Install, update, and roll back

```bash
# install from a directory or archive
xtervo plugins install /path/to/plugin
xtervo plugins install /path/to/plugin.zip

# validate without installing
xtervo plugins verify /path/to/plugin.zip

# list installed plugins with source and signature status
xtervo plugins list

# update an installed plugin (the package must have the same id)
xtervo plugins update /path/to/new-version.zip

# inspect and use rollback backups
xtervo plugins backups dev.xtervo.echo
xtervo plugins rollback dev.xtervo.echo
xtervo plugins rollback dev.xtervo.echo --to 1.0.0

# keep two backups instead of the default three
xtervo plugins update pkg.zip --keep 2
```

- `install` works for a first install or a reinstall of the same ID; `update` requires that ID to be installed already. The installer does not order versions: a downgrade is accepted and the same version is treated as a reinstall, so check the package yourself.
- Installation writes a staging copy, validates the manifest, paths, size limits and signature, then swaps directories into place. If validation fails, the old version is untouched.
- The previous version is kept under `.backup/<id>/`; the default is 3 backups (`--keep` accepts 1–10). Rollback re-validates the backup against the current signature policy.
- `--force` is only for a target directory owned by another ID or an unparseable manifest; it backs up and overwrites. Normal upgrades do not need it.
- Exit codes: `0` success, `1` validation/install/rollback failure, `2` usage error.
- The CLI exits before initializing Slint, so it works without `DISPLAY`; it never starts plugin processes.

The same operations are available in Settings → Plugins: enter a path or use `Choose file…` / `Choose folder…`, then `Install`, `Update…` or `Rollback to <version>`. Install, update and rollback do not restart xtervo. If an enabled plugin is updated, only that plugin is reloaded; a permission change requires confirmation again.

## Install locations and state

```text
$XDG_CONFIG_HOME/xtervo/                  # ~/.config/xtervo
├── plugin-signing.pub                    # optional default Ed25519 public key
├── plugin-signing.key                    # private key from keygen (0600; never distribute)
├── plugins-v3-state.toml                 # enablement and permission fingerprints
└── plugins/
    ├── <id>/                             # installed plugin
    │   ├── plugin.toml
    │   ├── plugin.toml.sig               # optional signature
    │   ├── .xtervo-install.toml          # local source/signature metadata
    │   └── main.py
    ├── .staging/                         # install/validate workspace
    └── .backup/<id>/                     # previous versions
```

`XTERVO_PLUGINS` overrides the entire plugins root; enablement state stays next to `config.toml`. Directories whose names start with `.` are skipped during discovery, so `.staging` and `.backup` never appear as plugins.

`plugins-v3-state.toml` stores only the plugin `id`, `enabled`, and the permission fingerprint (`permissions_approved` plus `permissions`). It stores no credentials, paths or environment variables. If the file is corrupt, xtervo logs the error and starts from an empty state without deleting the file. `.xtervo-install.toml` is local metadata for showing source and signature status; it is not signed, and signature checks always read `plugin.toml` again from its raw bytes.

To enable a plugin, open Settings → Plugins and choose `Enable`; the first enable requires reviewing the declared permissions and choosing `Allow and enable`. `Disable` stops the process but keeps the approval, so re-enabling an unchanged plugin does not ask again. `Reload` re-reads and validates the manifest; `Restart` restarts the supervisor and resets the automatic restart counter. None of these actions restarts xtervo.

## Signatures and permission checks

### Ed25519 signature

A signature covers the **raw bytes of `plugin.toml`** (no normalization of whitespace or line endings). `plugin.toml.sig` contains:

```text
xtervo-plugin-signature-v1
<base64(64-byte Ed25519 signature)>
```

Generate a local key pair and sign a plugin:

```bash
xtervo plugins keygen
xtervo plugins sign /path/to/plugin --key ~/.config/xtervo/plugin-signing.key
```

The public key is looked up in this order: `--pubkey`, then `XTERVO_PLUGIN_PUBKEY` (key text, file path, or `@path`), then `$XDG_CONFIG_HOME/xtervo/plugin-signing.pub`. With `XTERVO_PLUGINS`, the default public key is the file next to that plugins root (`<plugins root>/../plugin-signing.pub`).

Signature checks are optional by default. Require them with any of:

- `XTERVO_PLUGIN_REQUIRE_SIGNATURE=1`
- `plugin_require_signature = true` in `config.toml`
- the `Require Ed25519 signature` switch in Settings → Plugins

An unsigned plugin is allowed when signatures are not required and is shown as `unsigned`; a valid signature with a configured key is `verified`. A bad or tampered signature is always rejected, even when signatures are optional, and a signature without a usable key is rejected. If signatures are required but no key is configured, installation and rollback fail with an error.

Signing proves only that the holder of the private key signed those manifest bytes. It does not cover the entrypoint or any other resource, and there is no revocation, expiry, key rotation, online key distribution or trust chain. The installer never fetches keys from the network. For full package integrity, publish a digest in the manifest or verify the package externally.

### Permission checks

On first enable, the host shows the permissions declared in `[permissions]` and warns that they are not an OS sandbox; nothing starts until the user chooses `Allow and enable`. If the permission set changes, the old approval is invalidated and the plugin shows `permission confirmation required`.

- `terminal_context` allows `terminal.read`; every request still opens an Allow/Deny prompt (60 s without an answer counts as denied) and only reads the focused terminal's tail.
- `terminal_write` allows `terminal.write`; every request asks first, and text is capped at 4,096 bytes.
- `clipboard` allows `clipboard.read` and `clipboard.write`; every request asks first, and only text is supported.
- `credentials = "none"` or `"own"` is a declaration. The host never provides secrets; `credentials.get` always returns `-32002 unsupported`.
- `filesystem`, `network` and `process` are declarations shown at enable time and recorded for diagnostics. They are not enforced against the process.

The host clears the plugin environment and passes only `PATH`, `HOME`, `LANG`, `LC_ALL`, `TMPDIR`, plus `XTERVO_PLUGIN_ID`, `XTERVO_PLUGIN_API` and `XTERVO_HOST_VERSION`. Protocol lines are limited to 256 KiB, stdout to 16 MiB and the retained stderr tail to 8 KiB.

## Security model

Process plugins are ordinary child processes that run with your user account and your full system permissions. Manifest permissions are capability declarations for host-mediated APIs plus a one-time confirmation; they do not stop a process plugin from reading files, using the network, spawning processes or reading anything your user can read. xtervo does not provide an OS sandbox for process plugins.

WASM plugins (`runtime = "wasm"`) are narrower: no imports, so no WASI, files, network, process, clock, random or environment access; 16 MiB of linear memory; and fuel plus wall-clock limits per call. On any failure the original bytes pass through. However, an output filter can rewrite or delete terminal output — that is its purpose — so enabling one means trusting it to alter what you see. This is memory/execution isolation, not a general OS sandbox.

Install and enable only plugins you trust. Signatures attest to manifest provenance only; they do not audit the code. Keep credentials out of plugin directories and prefer plugins that manage their own credentials. For strong isolation, run xtervo in a container or as a dedicated user.

## Minimal example

```toml
api = 3
host_abi = 3
id = "dev.example.hello"
name = "Hello"
version = "1.0.0"
runtime = "process"
entry = "main.sh"

[contributes]
commands = true

[[commands]]
id = "greet"
title = "Hello: Greet"
```

Make the entrypoint executable and install the directory:

```bash
chmod +x main.sh
xtervo plugins install .
```

Then open Settings → Plugins, choose `Enable`, review the permissions and choose `Allow and enable`. `Hello: Greet` appears in the Command Palette; its full command ID is `plugin.dev.example.hello.greet`. The entrypoint must implement the v3 handshake described in [Plugin API v3](plugin-api-v3.md).

## Run the echo example

The complete reference plugin in `docs/examples/plugin-v3-echo/` uses only the Python standard library and demonstrates commands, status, events, panels, settings and an ask-gated terminal read.

```bash
mkdir -p ~/.config/xtervo/plugins
cp -r docs/examples/plugin-v3-echo ~/.config/xtervo/plugins/echo
chmod +x ~/.config/xtervo/plugins/echo/main.py
```

Start xtervo, open Settings → Plugins, choose `Enable`, review the permissions (`filesystem=plugin`, `terminal_context`, `credentials=own`) and choose `Allow and enable`. The commands are available in the Command Palette as `Echo: Greet`, `Echo: Host context`, `Echo: Read terminal tail`, `Echo: Render panel` and `Echo: Notify`.

Alternatively, point xtervo at the examples directory without copying anything:

```bash
XTERVO_PLUGINS=$PWD/docs/examples XTERVO_CONFIG=/tmp/xtervo-v3.toml cargo run -p xtervo-app --bin xtervo
```

The directory name does not matter; the stable identifier is `id = "dev.xtervo.echo"`.

## Troubleshooting

The Settings → Plugins row shows the current status, the latest error, `restart_count`, the normalized permissions, and `source`/`signature`. Use `Restart` to reset the supervisor after a failure, `Reload` after editing the manifest, and `Open folder` to inspect the installed files.

| Status | Meaning |
|---|---|
| `starting` | The plugin is starting; the 5 s handshake has not completed yet. |
| `running` | The handshake succeeded; commands and status are available. |
| `restarting` | The supervisor is restarting after a crash, handshake failure, too many malformed lines or a command timeout. |
| `stopped` | The plugin is not running. |
| `failed` | The restart limit (3 attempts with 200/400/800 ms backoff) or spawn failed. Read `error`, fix the cause, then `Restart`. |
| `disabled` | The plugin is installed but not enabled. |
| `permission confirmation required` | The manifest permissions changed; enable and confirm again. |
| `unsupported` | A v1/v2 or legacy flat manifest. It is never executed. |
| `invalid` | The manifest, entrypoint or package layout is invalid; the error text gives the reason. |

Other supervisor timeouts: commands 10 s, status 2 s (a status timeout drops only that result), ping every 30 s with a 5 s timeout (two consecutive misses restart the plugin), and surface renders 5 s. A failure affects only that plugin; the terminal and other plugins keep running.

Common problems:

- **Entrypoint is not executable.** Install fails or the plugin cannot start. Run `chmod +x` on the entry file (Unix).
- **Logs break the protocol.** The plugin must write one JSON object per line to stdout and send diagnostics to stderr only. Stray `print`/`echo` output causes bad lines; after 5 bad lines the supervisor restarts the plugin.
- **Handshake never completes.** Answer `initialize` with `{"protocol_version":3}`; the timeout is 5 s.
- **Command is missing.** The plugin must be enabled with confirmed permissions, and any `when` context must be true. Commands without a `keybinding` have no shortcut; they remain available in the Command Palette, and in the context menu when `menu = true`.
- **A capability is denied.** The plugin must declare the permission and the user must allow each `terminal.read`, `terminal.write` or clipboard request. Credentials are never supplied by the host.
- **Status is `failed` after a crash loop.** The supervisor restarts at most 3 times, then stops. Check `error` and the log, fix the plugin, and use `Restart`.
- **Plugins are not loaded at all.** After 3 or more consecutive unclean exits, xtervo starts with plugins suppressed and logs `[safe-mode]`. Exit cleanly and restart.

Logs live in `~/.local/state/xtervo/logs/` (following `XDG_STATE_HOME`); one file is written per launch and the last 8 are kept. Plugin stderr appears there as `[plugin-v3] <id> stderr: …` and the tail is also shown in the Settings row. `XTERVO_LOG=0` disables file logging.

## See also

- [Plugin API v3](plugin-api-v3.md): methods, parameters, errors, limits and the declarative UI schema
- [Shortcuts](shortcuts.md): rebinding plugin commands in Settings → Keys
- [MCP control](mcp.md): the built-in control channel that is not a plugin API
- Chinese design documents: [Plugin API v3](https://github.com/HeyTycho/xtervo/blob/main/docs/PLUGIN_V3.md), [local install and signing](https://github.com/HeyTycho/xtervo/blob/main/docs/PLUGIN_INSTALL.md), [transport plugins](https://github.com/HeyTycho/xtervo/blob/main/docs/PLUGIN_TRANSPORT.md)
