# Plugin API v3

Status: **v3 is the only plugin protocol xtervo supports**. The host loads only
manifests with `api = 3` and `host_abi = 3`; v1/v2 manifests are **incompatible and
will not run**, and the settings page shows only `unsupported` plus a migration hint.
There is no dual-protocol compatibility layer in the code.

Implementation: `crates/app/src/plugin_v3/` (`manifest` / `protocol` / `state` /
`supervisor` / `tests`) and `crates/app/src/plugin_wasm.rs` (the WASM hot path). The
old `crates/app/src/plugin.rs` is kept as a legacy file that is no longer compiled;
it is not registered and not started.

- process: JSON-RPC 2.0 line-delimited stdio, one dedicated child process per enabled plugin.
- wasm: a Wasmtime instance with no imports, allowed only on the `terminal.output_filter` hot path.
- UI: everything is rendered by the host; plugins can only supply declarative documents.
- permissions: capability gate + user confirmation; process is not an OS sandbox, wasm is a narrow sandbox.
- MCP: a built-in xtervo feature, **not a plugin capability**; this protocol has no plugin MCP server/proxy.

---

## 1. Security model (read it literally)

**Process plugins** are ordinary child processes. They run as the current user and
have all of that user's system permissions. **WASM plugins** do not spawn processes and
have no imports; they only get a terminal-output byte buffer written by the host. Even
so, they can still rewrite/delete terminal display (display spoofing), so an enable
confirmation is still necessary.

What the host can do:

1. **First-enable confirmation**: show the permissions declared in the manifest; the
   process is not started until the user clicks "Allow and enable". When the permission
   set changes, the old confirmation is invalidated and must be repeated.
2. **Capability gate**: runtime checks only for host-brokered APIs:
   - `terminal.read` → `permissions.terminal_context`, and **every** request requires a
     user Allow/Deny;
   - `terminal.write` → `permissions.terminal_write`, ask every time;
   - `clipboard.read` / `clipboard.write` → `permissions.clipboard`, ask every time.
3. **No secrets**: the host never returns Vault contents, plaintext environment
   variables, API keys or other credentials. `credentials.get` always returns
   `-32002 unsupported`. A plugin that needs credentials must manage them itself
   (`credentials = "own"` is only a declaration).
4. **Environment variable allowlist**: when spawning a plugin the host calls
   `env_clear()` and passes through only `PATH` / `HOME` / `LANG` / `LC_ALL` /
   `TMPDIR`, then sets `XTERVO_PLUGIN_ID` / `XTERVO_PLUGIN_API` /
   `XTERVO_HOST_VERSION`.
5. **Input/output limits**: 256 KiB per line; 16 MiB total stdout; 8 KiB stderr tail;
   exceeding a limit restarts/stops the plugin as a protocol error.
6. **Lifecycle isolation**: an independent supervisor per plugin; a crash, bad JSON or
   timeout only affects that plugin. On Linux child processes get
   `PR_SET_PDEATHSIG=SIGKILL` and their own process group, so a SIGKILLed xtervo leaves
   no orphaned plugins.

**Extra WASM boundaries** (see section 6): module load rejects any import; there is no
WASI/file/network/process/clock/random host; linear memory is capped at 16 MiB and a
single file at 8 MiB; every call has a fuel + epoch wall-clock limit; input/output are
256 KiB each. Out-of-bounds access, traps, timeouts and failed memory growth all fall
back to the original bytes and trip that filter until the next Reload.

**What the host does not do for process plugins**: it does not stop a plugin from
reading/writing files directly, accessing the network, launching child processes, or
reading the environment/config it can access. The `filesystem` / `network` / `process` /
`credentials` permissions are only "declaration + shown at confirmation + diagnostic
record", not kernel-level enforcement. Use a container/dedicated user when you need
strong isolation; xtervo does not provide an OS sandbox for process plugins.

---

## 2. Directories and discovery

The plugin directory defaults to:

```text
$XDG_CONFIG_HOME/xtervo/plugins/          # or ~/.config/xtervo/plugins/
```

It can be overridden with `XTERVO_PLUGINS=/path/to/plugins`. Each v3 plugin must be a
subdirectory:

```text
<plugins>/<dir>/
  plugin.toml      # required
  main.py          # process entry (executable/script; must be inside the plugin directory)
  # or filter.wasm  # wasm entry (≤8 MiB; executable bit not required)
```

Discovery rules:

- The subdirectory contains `plugin.toml`: read and parse it strictly.
- Legacy flat manifests `*.toml` at the directory top level: shown as `unsupported`,
  never executed.
- `api = 1/2` or a missing `api`: shown as `unsupported`; the reason includes a
  migration hint.
- `api = 3` but invalid fields/`host_abi`/`entry`: shown as `invalid` + reason; never
  executed.
- Duplicate `id`: the first one is kept, the rest are marked invalid.
- `entry` / `theme` allow only relative paths inside the plugin directory; absolute
  paths, `..` and backslashes are rejected; after canonicalize the path must still be
  inside the plugin directory (to prevent symlink escape). A process `entry`
  additionally requires a regular file + Unix executable bit; a wasm `entry` must be
  `.wasm`, ≤8 MiB, and does not require the executable bit.

Enabled state and permission confirmation are persisted in:

```text
<config dir>/plugins-v3-state.toml
```

Only `id` / `enabled` / a permission fingerprint (`permissions_approved` +
`permissions = [...]`) are stored; credentials, paths and environment variables are
not. If the state file is corrupt, the host logs it and starts with empty state without
deleting the original file.

---

## 3. Manifest

Full example (`docs/examples/plugin-v3-echo/plugin.toml`):

```toml
api = 3
host_abi = 3
id = "dev.xtervo.echo"
name = "Echo (v3 example)"
version = "1.0.0"
runtime = "process"          # process | wasm; oneshot belongs to the removed v1
entry = "main.py"
description = "Reference plugin"

[permissions]
filesystem = "plugin"        # none | plugin | home (declaration; not enforcement)
network = false              # declaration
process = false              # declaration
clipboard = false            # gate for clipboard.read/write
terminal_context = true      # gate for terminal.read (still asks every time)
terminal_write = false       # gate for terminal.write (still asks every time)
credentials = "own"          # none | own (the host never provides plaintext credentials)
# WASM only: terminal.output_filter = true (see section 6); forbidden for process

[contributes]
commands = true              # must be true when [[commands]] is present
status = true                # allows status.get + ui.status
events = ["startup", "tab-open", "tab-close", "pane-focus"]
theme = "theme.toml"         # optional; loaded at startup, not hot-swapped at runtime
panels = ["echo"]            # declarative panel IDs
settings = true              # allows surface.render(settings)
assistant = false            # bool or a [contributes.assistant] table; placeholder only

[[commands]]
id = "greet"
title = "Echo: Greet"
description = "Return a hello message"

[[commands]]
id = "panel"
title = "Echo: Render panel"
menu = true                  # optional: appended as a terminal context-menu decoration item (UI-TERM-013)
```

### 3.1 Top-level fields

| Field | Required | Rule |
|---|---|---|
| `api` | yes | must be `3`; 1/2 are directly unsupported |
| `host_abi` | yes | must be `3` |
| `id` | yes | starts with `[A-Za-z0-9]`, only `A-Za-z0-9._-`, ≤64 chars, globally unique |
| `name` | yes | non-empty, ≤128 chars |
| `version` | yes | non-empty, ≤64 chars |
| `runtime` | yes | `"process"` or `"wasm"` |
| `entry` | yes | relative path inside the plugin directory; process must be executable, wasm must be `.wasm` (≤8 MiB, executable bit not required) |
| `description` | no | ≤512 chars |
| `permissions` | no | see above |
| `contributes` | no | see above |
| `commands` | no | `id`/`title` required per entry, `description`/`keybinding`/`when` optional (see 3.4); `menu = true` appends the command to the terminal context menu (the host decides position/style, and the click callback receives the full command ID); `id` unique |

`runtime = "wasm"` allows only `[permissions.terminal] output_filter = true`, and must
not declare commands/status/events/theme/panels/settings/assistant; see section 6 for
details.

Unknown fields make the entire manifest `invalid` (strict mode, so typos do not
silently take effect). A command's full ID is `plugin.<plugin id>.<command id>`, for
example `plugin.dev.xtervo.echo.greet`.

### 3.2 `assistant` placeholder

`assistant` supports two forms:

```toml
assistant = false
assistant = true
# or
[contributes.assistant]
name = "Optional display name"
description = "Placeholder metadata only"
```

The host only validates/displays it. **The v3 MVP does not implement an AI provider,
does not start AI requests and does not forward API keys.** Declaring assistant gives a
plugin no AI capability.

### 3.3 Theme

`contributes.theme` is a relative TOML theme inside the plugin directory. The theme of
an enabled plugin whose permissions are confirmed is merged into the theme list when
xtervo starts; enable/reload does not hot-swap a theme at runtime (restarting xtervo is
required). A missing theme only logs a `theme` error and does not block commands.

### 3.4 Command keybindings and `when` contexts

`[[commands]]` also accepts two optional fields that control how a command is offered.
Both are validated strictly: a malformed value makes the whole manifest `invalid`.

```toml
[[commands]]
id = "greet"
title = "Echo: Greet"
keybinding = "ctrl+shift+g"               # optional; a single chord
when = "terminalFocus && !settingsOpen"   # optional; condition, ≤512 bytes
```

- `keybinding` is a single chord such as `ctrl+shift+g` (no multi-chord sequences).
  It must use `Ctrl`/`Alt`/`Meta` or be an `F1`–`F24` key, otherwise the manifest is
  rejected because the chord would steal terminal input. The host lists it on the
  Hotkeys page as the command's default binding, and the user can override or reset it
  like a built-in command. An empty value means no default chord; the command is still
  available in the Palette.
- `when` is a condition the host evaluates with the current UI context. Supported
  variables are `terminalFocus`, `settingsOpen`, `paletteOpen`, `tabSearchOpen`,
  `sidebarOpen`, `tabCount`, `paneCount`, `activeTabPinned`, and the literals `true` /
  `false`. The expression supports `&&`, `||`, `!`, parentheses and integer comparisons
  (`<`, `<=`, `>`, `>=`, `==`, `!=`), for example `terminalFocus && !settingsOpen` or
  `tabCount >= 2`.
- `when` is at most 512 bytes. Unknown variables and syntax errors make the manifest
  `invalid`; a typo is not silently treated as false.
- While `when` is false, the command is not listed in the Palette and its keybinding
  does not trigger.

---

## 4. JSON-RPC 2.0 lifecycle

### 4.1 Transport

- One JSON object per line on stdin/stdout; each line must end with `\n`.
- Empty lines are ignored.
- Single message limit **256 KiB**; over-long lines count as bad lines and are truncated.
- `jsonrpc` must be `"2.0"`.
- A request must have `method` + `id` (number or string); a notification has only
  `method`; a response must echo the **numeric id** assigned by the host.
- Request/notification must not contain `result`/`error`.
- Bad JSON / bad structure / over-long lines accumulate to **5** (test-configurable),
  then the supervisor restarts the plugin; beyond the automatic restart limit the
  plugin goes to `Failed`.

### 4.2 Startup handshake

Immediately after spawning, the host sends:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol_version":3,
  "host":{"name":"xtervo","version":"0.3.1","abi":3},
  "plugin":{"id":"dev.xtervo.echo","name":"Echo (v3 example)"},
  "permissions":{
    "granted":["filesystem=plugin","terminal_context","credentials=own"],
    "terminal_read":"ask",
    "terminal_write":"ask",
    "clipboard":"denied",
    "credentials":"never-host-provided"
  },
  "contributes":{
    "commands":["greet","panel"],
    "status":true,
    "events":["startup","tab-open"],
    "panels":["echo"],
    "settings":true,
    "assistant":false
  }
}}
```

The plugin must return:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol_version":3}}
```

`protocol_version` must be `3`; otherwise the handshake fails. The host then sends:

```json
{"jsonrpc":"2.0","method":"initialized","params":{}}
```

Handshake timeout defaults to **5s**. Timeout/error is handled as a crash: backoff
restart, at most 3 times.

### 4.3 Host → plugin methods

| method | type | params | result |
|---|---|---|---|
| `initialize` | request | see above | `{protocol_version:3}` |
| `initialized` | notification | `{}` | none |
| `command.invoke` | request | `{command, args:[], context:{host_version,api}}` | any JSON; `{ok, message}` recommended |
| `event.deliver` | notification | `{event, payload}` | none |
| `status.get` | request | `{}` | `{text, tooltip?, command?}` |
| `ping` | request | `{seq}` | any JSON |
| `surface.render` | request | `{surface:"panel"\|"settings", panel?}` | declarative UI document |
| `ui.action` | notification | `{surface, panel?, element, action, value?}` | none |
| `shutdown` | request | `{}` | any JSON; the host then closes the process |

`command.invoke` timeout defaults to **10s**; on timeout it reports `timeout` and
restarts the plugin process.
`status.get` runs every **2s** with a **2s** timeout; a status timeout only discards
that result and does not kill the process.
`ping` runs every **30s** with a **5s** timeout; 2 consecutive ping timeouts cause a
restart.
`surface.render` timeout is **5s**.

Multi-window (additive):

- `command.invoke.params.window_id` = the window that issued the command;
- `surface.render.params.window_id` / `ui.action.params.window_id` = the window that
  surface belongs to; plugins should use this value when returning per-window content;
- when `window_id` is missing the host falls back to the current active window. Window
  ids start at 1 and increase monotonically; they are not reused after close; an
  explicit id that does not exist does not fall through to another window.

`status.get` / `ui.status` result:

```json
{"text":"Echo v3 ready","tooltip":"optional","command":"greet"}
```

`text` ≤200 chars, `tooltip` ≤512, `command` must be a short command ID declared in the
manifest; otherwise it is rejected. The status bar shows at most one line per plugin;
multiple plugins are merged with ` · `.

`shutdown`: the host waits at most **1s**; if the process does not exit, it SIGKILLs the
whole process group.

### 4.4 Plugin → host methods

Plugins can send requests (with an id) or notifications. The host currently requires
plugin → host requests to use a **numeric id** (string ids receive `-32600`);
notifications have no id. Methods that need an asynchronous ask can only be requests
(not notifications).

| method | type | capability/confirmation | returns |
|---|---|---|---|
| `ui.notify` | request/notification | none (rate-limited to 20 per 10s, ≤500 chars) | `{}` |
| `ui.status` | request/notification | `contributes.status=true` | `{}` |
| `ui.render` | request | corresponding `panels`/`settings` declaration | `{accepted:true}` |
| `context.version` | request | none | `{api,host_abi,protocol_version,jsonrpc,host_version,transport}` |
| `context.config_metadata` | request | none (read-only non-sensitive fields) | see below |
| `terminal.read` | request | `terminal_context` + ask every time | `{text,truncated,max_lines,max_bytes}` |
| `terminal.write` | request | `terminal_write` + ask every time | `{}` |
| `clipboard.read` | request | `clipboard` + ask every time | `{text}` |
| `clipboard.write` | request | `clipboard` + ask every time | `{}` |
| `credentials.get` | request | never | always `-32002 unsupported` |

The params of `terminal.read` / `terminal.write` / `clipboard.read` /
`clipboard.write` / `ui.notify` / `ui.status` / `ui.render` may all carry an optional
`window_id` (integer ≥1, defaults to active):

- an ask (terminal/clipboard) records the window; Allow/Deny is shown only in that
  window, and the action is performed only on that window's focused terminal;
- `ui.notify` / `ui.status` / `ui.render` display only in the target window;
- when `ui.render` carries an explicit `window_id` and the target window has not yet
  opened the plugin's surface, the host opens it in that window; it does not overwrite
  a panel already open in another window;
- when `window_id` does not exist the request returns an error; notifications are
  dropped and are not displayed across windows.

`context.config_metadata` returns only:

```json
{"theme":"tokyo-night","language":"zh","font_size":13.0,"tabbar_position":"top","plugin_api":3}
```

It does not include environment variables, Vault, profile passwords, cwd, command line
or file contents.

`terminal.read` params are clamped:

```json
{"max_lines":80,"max_bytes":32768}
```

`max_lines` 1..500, `max_bytes` 1..65536; the host returns only the tail text of the
**focused terminal**, without cwd/env/Vault. Every request shows an Allow once / Deny
card in the UI; with no response after the default **60s** it times out and is denied.

`terminal.write` `text` is ≤4096 bytes; the confirmation card shows an escaped preview.
After Allow, the text is fed to the focused terminal's input (equivalent to the user
typing; may contain newlines).

`clipboard.write` `text` is ≤64K chars; `clipboard.read` returns text only, not images.

Error response example:

```json
{"jsonrpc":"2.0","id":42,"error":{
  "code":-32001,
  "message":"缺少 permissions.terminal_context（capability gate）"
}}
```

### 4.5 Error codes

| code | meaning |
|---|---|
| `-32700` | JSON parse error (the host mainly counts bad lines) |
| `-32600` | invalid request |
| `-32601` | method not found |
| `-32602` | invalid params |
| `-32603` | internal error |
| `-32001` | permission denied (capability gate) |
| `-32002` | unsupported |
| `-32003` | denied by user |
| `-32004` | unavailable (no focused terminal, no clipboard, or the process has exited) |
| `-32005` | rate limited |
| `-32006` | permission not approved |
| `-32007` | request timeout (ask timeout, etc.) |

---

## 5. Permissions, confirmation and runtime gates

The normalized permission set (fixed order) is generated from the manifest:

```text
filesystem=none|plugin|home
network
process
clipboard
terminal_context
terminal_write
terminal.output_filter        # only when runtime = "wasm"
credentials=none|own
```

First Enable:

1. the settings page shows the permissions declared by the plugin plus a "not an OS
   sandbox" warning;
2. after the user clicks **Allow and enable**, the permission fingerprint is written
   and the plugin starts;
3. clicking Cancel does not change the state.

At runtime:

- `terminal.read` without `terminal_context` → `-32001`, and the activity log records
  `denied`.
- `terminal.write` without `terminal_write` → `-32001`.
- clipboard requests without `clipboard` → `-32001`.
- with the permission granted, every terminal/clipboard request still needs a user
  Allow; Deny → `-32003 denied by user`.
- manifest permission changes → reload stops the old process, the old confirmation is
  invalidated, the status shows `permission confirmation required`, and the plugin runs
  only after Enable/confirmation again.
- all denials are logged (`[plugin-v3] ... 拒绝 <method>`) and shown in the activity
  list; sensitive argument bodies are not logged.

---

## 6. WASM hot-path runtime (`runtime = "wasm"`)

WASM is a **new runtime** in v3, not a replacement for process: commands, events,
status, panels, supervisor and MCP (embedded in the host, not a plugin) of the existing
process v3 all stay unchanged.

Only one narrow capability is implemented today: the **terminal-output hot-path filter
`terminal.output_filter`**. There are no plugin commands, no arbitrary UI, no
file/network/process access, and no marketplace/signing. It is disabled by default; the
first Enable requires the user to confirm the `terminal.output_filter` permission.

### 6.1 Manifest and enabling

```toml
api = 3
host_abi = 3
id = "dev.xtervo.wasm-filter"
name = "WASM output filter"
version = "1.0.0"
runtime = "wasm"
entry = "filter.wasm"           # inside the plugin directory, ≤8 MiB, executable bit not required

[permissions.terminal]
output_filter = true            # the only allowed WASM permission

# commands/status/events/theme/panels/settings/assistant under [contributes] are not allowed
```

It can also be written as `terminal.output_filter = true` under `[permissions]`. Rules:

- `api` must be 3 and `host_abi` must be 3;
- `runtime = "wasm"` must declare `terminal.output_filter = true`;
- declaring filesystem/network/process/clipboard/terminal_context/terminal_write/
  credentials or any process contribution point → the entire manifest is `invalid` and
  is never loaded;
- `entry` must be a `.wasm` regular file inside the plugin directory; after
  canonicalize it must not escape, and symlink escapes are rejected; maximum 8 MiB;
- the permission fingerprint includes `terminal.output_filter`; the default is
  `enabled = false`, and the first Enable shows the permission and waits for
  **Allow and enable**. After a permission/manifest change the old confirmation is
  invalidated.

The Plugins row in settings shows `runtime` (`process` / `wasm`), status, error and the
generic `Enable` / `Disable` / `Reload` / `Restart` / `Open folder` actions; a WASM
failure reason is shown in `error`. None of these actions restarts xtervo.

### 6.2 Narrow ABI

The module must export, and may rely on, only the following ABI; signatures and memory
limits are verified at load time:

| export | type/constraint | semantics |
|---|---|---|
| `memory` | linear memory; must declare a maximum, ≤256 pages (16 MiB) | host reads/writes input/output |
| `alloc` | `(len: i32) -> ptr: i32` | allocates space for input; return value must be ≥262144 |
| `filter` | `(ptr: i32, len: i32) -> out_len: i32` | processes input, writes output at fixed offset 0 |

A single call:

```text
host:  reset fuel + epoch deadline
host:  ptr = alloc(len), validate 262144 <= ptr, ptr+len <= memory.size
host:  memory[ptr..ptr+len] = PTY bytes
guest: filter(ptr, len) -> out_len
host:  validate 0 <= out_len <= 262144 and out_len <= memory.size
host:  read back memory[0..out_len]
```

- **Output is always at linear-memory offset 0**; the input region starts at offset
  262144, so the 256 KiB output region does not overlap the input and the module's
  output cannot overwrite the input before it has read it.
- Returning `0` is a legal "delete this block of output"; a negative return, more than
  256 KiB, or out-of-bounds is a failure.
- The host calls `alloc` again on every invocation; there is no `dealloc`. Linear
  memory persists across calls, so a stateful filter can keep cross-chunk state in its
  own memory, but total memory is still capped at 16 MiB.
- The module **may not have any imports** (so no WASI, files, network, process, clock,
  random numbers, environment variables). Any import found at load time is rejected.
- A `start` function is allowed, but fuel/epoch are set before instantiation, so an
  infinite loop is cut off.

Invocation happens on the core **pane actor thread**, before Sixel → IIP → Kitty → vte:

```text
PTY read thread
  → bounded queue
  → pane actor: WASM output_filter (bounded call)
  → Sixel → IIP → Kitty → vte → Term → damage → UI pull frame
```

The UI/render thread never waits on WASM; it still gets content only through the
bounded wakeup/frame-pull channel. If the filter is slow or there is a lot of
concurrency, the cost is that pane's parse throughput, not UI responsiveness.

### 6.3 Resource limits and failure semantics

| resource | limit |
|---|---|
| `.wasm` file | 8 MiB |
| linear memory | 16 MiB / 256 pages |
| single input | 256 KiB (the current PTY read block is about 32 KiB) |
| single output | 256 KiB |
| fuel | 16,000,000 instructions |
| epoch wall clock | 4 ticks × 5 ms ≈ 20 ms |
| WASM stack | 512 KiB |
| instances/tables/memories | 1 / 1 / 1 |
| table elements | 1024 |

Failure handling (all of it **must not crash xtervo and must not drop bytes**):

1. filter returns `None`, a trap, a fuel/epoch timeout, a negative length, output over
   the limit, an out-of-bounds alloc pointer / a pointer in the reserved output region,
   `memory.grow` over the limit;
2. core immediately continues down the original pipeline with the **original input
   bytes**;
3. that plugin instance is tripped (faulted) and subsequent calls pass through
   directly, avoiding repeated 20 ms timeouts on every chunk;
4. the settings-page status becomes `failed` and shows the truncated error; the
   activity log/eprintln records the reason;
5. only `Reload` / `Restart` / re-enable rebuilds the instance; `Disable` removes it
   immediately.

Multiple WASM filters can be enabled at once: they run serially in lexicographic order
of plugin id, with each receiving the previous one's output; if one is tripped it is
skipped and the later filters continue. Core applies one more 256 KiB limit check to
the final output; if it is over the limit the entire block is passed through unchanged.

### 6.4 Security model (the honest version)

WASM provides a **memory/execution sandbox**, not a statement that "the plugin is
trusted":

- no imports → the module cannot reach host files, network, processes, environment,
  Vault or the TUI;
- linear-memory cap + host address validation → the module cannot read/write host
  memory out of bounds;
- fuel + epoch → infinite loops/overlong computation are cut off;
- core applies a second cap to the output length → a module cannot make the host
  allocate unbounded memory by returning a huge length;
- every failure falls back to the original bytes → a plugin cannot make the terminal
  lose bytes via a trap;
- single-instance Mutex + single-threaded invocation → no concurrent data races;
  multiple panes queue up.

Risks that remain:

- **display spoofing**: a filter can already rewrite/delete terminal output; enabling
  it authorizes it to affect the display.
- **CPU consumption**: each call is capped at about 20 ms; a malicious plugin can make
  every chunk run to the cap, reducing terminal throughput (the UI is not blocked).
- **no signing/marketplace/sandbox-escape audit**: only modules that the user placed in
  the plugin directory and confirmed are loaded; no claim of "audited" or "security
  certified" is made.
- WASM is not OS-level isolation: Wasmtime/JIT vulnerabilities are outside this
  feature's threat model; use a container/dedicated user when you need strong
  isolation.

### 6.5 Data and privacy

The host only copies PTY output bytes into WASM memory. With no imports, the module
cannot get:

- environment variables, cwd, command line, profile, Vault, API keys;
- clipboard, other panes, window/UI state;
- the file system or network.

Old data that the plugin itself wrote into linear memory remains until the instance is
rebuilt; `Disable` / `Reload` releases the instance and its memory.

### 6.6 Example and real e2e

`docs/examples/plugin-wasm-filter/`:

- `filter.wat`: replaces the literal `[xtervo-wasm]` (12 bytes) with
  `WASM-FILTER-OK` (14 bytes) and copies the remaining bytes byte for byte;
- `filter.wasm`: the assembled example (463 bytes);
- `build.sh`: regenerates it from the WAT with `wasm-tools parse` or `wat2wasm`;
- `plugin.toml`: `runtime = "wasm"` + `terminal.output_filter = true`.

```bash
cd docs/examples/plugin-wasm-filter
./build.sh
mkdir -p ~/.config/xtervo/plugins
cp -r . ~/.config/xtervo/plugins/wasm-filter
# Settings → Plugins → Enable → Allow and enable
printf 'hello [xtervo-wasm]\n'
# shows hello WASM-FILTER-OK
```

The same works with `XTERVO_PLUGINS=$PWD/docs/examples ...` pointing at the examples
root. The directory name does not matter; the stable identity is
`id = "dev.xtervo.wasm-filter"`.

Automated real-PTY e2e (not mocks):

- `wasm_filter_transforms_real_pty_output_and_disable_restores_it`: a real `/bin/sh` +
  PTY + pane actor; after enabling, the first line becomes `WASM-FILTER-OK`; after
  disabling on the same pane, the second line shows `[xtervo-wasm]` unchanged, without
  rebuilding the pane;
- `wasm_trap_falls_back_to_original_bytes_in_real_pty`: after a real trap the original
  bytes continue to display, and the settings page shows `failed` with the error;
- `wasm_reload_and_broken_module_error_are_reported_without_crash`: reload of a valid
  module succeeds; a broken module returns an error, stops the old filter and shows the
  error in the settings page; after fixing it, Restart recovers;
- `wasm_first_enable_needs_confirmation_and_is_visible_in_settings`: disabled by
  default; the first Enable returns a permission confirmation, and the settings page
  shows `runtime=wasm`/`running`;
- `wasm_enable_takes_effect_on_an_existing_pane`: Enable after the pane has been
  created; the next line of output on the same pane is transformed immediately, without
  rebuilding the pane.

### 6.7 Tests and commands

```bash
cargo test -p xtervo-app --bin xtervo plugin_wasm
cargo test -p xtervo-app --bin xtervo plugin_v3::tests::wasm
cargo test -p xtervo-core --test output_filter
cargo test -p xtervo-app --bin xtervo
```

Coverage: manifest/permissions/ABI, missing exports/wrong signatures/import rejection,
the uppercase example, WAT/WASM example consistency, traps, fuel/epoch infinite loops,
the 256 KiB input limit/an exact 256 KiB, negative lengths, output over the limit, a
module >8 MiB, low/out-of-bounds alloc pointers, the `memory.grow` limit, full-chain
tripping and passthrough, core panic/None/over-limit fallback, real PTY enable
transform/disable unchanged/trap fallback/reload error.

### 6.8 Non-goals (explicitly not done in this section)

- plugin MCP server/proxy (MCP is embedded in the host, not a plugin capability);
- arbitrary Slint UI, plugin commands/status/events/panels (the wasm runtime only does
  hot-path filtering);
- WASI/file/network/process/clock/random imports;
- marketplace, signing, auto-update, AOT cache;
- AI provider, credentials, Vault access.

---

## 7. Events

Events are declared in `contributes.events` in the manifest; the host delivers only to
plugins that are enabled + permission-confirmed + running. `startup` may be queued
early in startup and delivered after the handshake completes (so it is not lost).

| event | payload |
|---|---|
| `startup` | `{}` |
| `tab-open` | `{"title":"...","kind":"local"}` |
| `tab-close` | `{"title":"..."}` |
| `pane-focus` | `{"pane":<u64 opaque id>}` |

Titles are sanitized to 200 chars; `kind` allows only a short ASCII identifier
(otherwise it falls back to `local`); no cwd, command line, environment variables,
terminal contents or credentials are included. Events are notifications; the host does
not wait for a plugin reply.

---

## 8. Declarative panels / settings

Plugins **cannot** provide Slint/WASM/HTML/arbitrary layouts; they can only return a
fixed schema:

```json
{
  "version": 1,
  "title": "Echo settings",
  "elements": [
    {"type":"text","text":"..."},
    {"type":"markdown","text":"## Title\n**bold**"},
    {"type":"list","items":["a","b"]},
    {"type":"input","id":"name","label":"Name","value":"v","placeholder":"..."},
    {"type":"button","id":"save","label":"Save"},
    {"type":"select","id":"mode","label":"Mode","value":"a","options":["a","b"]},
    {"type":"progress","label":"Progress","value":0.5}
  ]
}
```

Limits:

- at most 100 elements;
- title ≤120, text/markdown ≤4000 chars;
- list ≤50 items, each item ≤500;
- input value ≤1000, placeholder ≤120;
- button label ≤120;
- select 1..32 options, value must be in options;
- progress `0..=1`;
- interactive element ids are globally unique, allow only `[A-Za-z0-9._-]`, ≤64;
- an unknown type or a limit violation → the entire document is rejected and the UI
  shows an error.

Host rendering:

- `text`: plain text with wrapping;
- `markdown`: the host does minimal plain-text rendering (strips headings/bold/
  backticks, shows links as `text (url)`), **does not parse HTML, does not follow
  links, does not load remote images**;
- `list`: multi-line plain text;
- `input`: host TextInput; `edited` sends `ui.action` (`action:"change"`);
- `button`: sends `ui.action` (`action:"click"`);
- `select`: the host computes the next value from the options and sends `ui.action`
  (`action:"change"`);
- `progress`: host progress bar.

Opening a panel/settings page:

- each settings row has `Panel…` (when `panels` is declared) and `Settings…` (when
  `settings` is declared and enable is confirmed);
- the host sends a `surface.render` request; the result goes into the currently open
  card;
- the plugin can push updates at any time with a `ui.render` request (only accepted for
  the currently open `(plugin, surface)`);
- UI actions travel as `ui.action` notifications; the plugin can reply with `ui.render`
  to update.

Current MVP limitation: multiple panels show only the first panel's entry; opening the
others requires a future UI extension.

---

## 9. Plugins settings page

Each row shows:

- `name` / `version` / `api` / `runtime` (`process` | `wasm`)
- `status`: `running` / `starting` / `restarting` / `stopped` / `failed` /
  `disabled` / `permission confirmation required` / `unsupported` / `invalid`
- `error` (crash reason, stderr tail, permission-change notice)
- `permissions` (normalized summary; stated as declarations/capabilities, not a
  sandbox)
- `restart_count`
- `source` / `signature`: local install source and live Ed25519 verification status
- actions: `[Enable]` `[Disable]` `[Reload]` `[Restart]` `[Update…]` /
  `[Rollback to <version>]` / `[Open folder]` (after enable/permission confirmation,
  `[Panel…]` / `[Settings…]` are shown as well)
- top: local install path + `[Choose file…]` / `[Choose folder…]` / `[Install]`, and a
  "Require Ed25519 signature" toggle

Local install/update/rollback, the signature format and security limits are covered in
[Plugins](plugins.md). The installer does not execute plugin code; permission
confirmation is still handled by the v3 host described in this section.

v1/v2 rows: `unsupported`, `api` shows the original version, `error` explains
"Unsupported Plugin API vX; xtervo v3 only loads api = 3 / host_abi = 3 …", and
`Open folder` still works, but no old plugin is executed.

None of these actions **restarts xtervo**:

- Enable: after confirmation, start the supervisor;
- Disable: close the process, keep the permission confirmation; enabling again does not
  ask (when permissions are unchanged);
- Reload: re-read from disk and validate; a permission change stops the plugin and
  requires confirmation again;
- Restart: manually restart the supervisor and reset the automatic-restart count;
- Open folder: `xdg-open` / `open` / `explorer`.

---

## 10. Supervisor behavior

Production defaults:

| item | default |
|---|---|
| handshake timeout | 5s |
| command timeout | 10s (timeout kills/restarts) |
| status poll/timeout | 2s / 2s (timeout only discards) |
| ping poll/timeout | 30s / 5s (2 consecutive failures restart) |
| surface timeout | 5s |
| ask timeout | 60s |
| automatic restarts | 3; backoff 200ms → 400ms → 800ms (cap 2s) |
| bad-line limit | 5 |
| single-line limit | 256 KiB |
| total stdout | 16 MiB |
| stderr tail | 8 KiB |

Failure handling:

- spawn failure → `Failed`, no automatic retry (you can Restart/Reload);
- handshake failure / process exit / bad-JSON limit exceeded / command timeout / output
  over limit → automatic restart;
- beyond the restart limit → `Failed` + reason, commands immediately `unavailable`;
- a failure in any plugin does not affect the terminal or other plugins;
- reload/restart/disable are driven by supervisor messages and do not restart xtervo.

---

## 11. Example plugins

`docs/examples/plugin-v3-echo/`:

```bash
mkdir -p ~/.config/xtervo/plugins
cp -r docs/examples/plugin-v3-echo ~/.config/xtervo/plugins/echo
chmod +x ~/.config/xtervo/plugins/echo/main.py
# start xtervo → Settings → Plugins → Enable → Allow and enable
```

Available commands (Palette):

- `Echo: Greet`
- `Echo: Host context` (`context.version`)
- `Echo: Read terminal tail` (`terminal.read` ask/allow)
- `Echo: Render panel` (declarative panel)
- `Echo: Notify` (`ui.notify`)

The example also implements `status.get`, `ui.status`, `surface.render(settings)`,
`ui.action` and an event log (`events.txt`). It uses only the Python standard library.

You can also point `XTERVO_PLUGINS` at the directory containing the example:

```bash
XTERVO_PLUGINS=$PWD/docs/examples XTERVO_CONFIG=/tmp/xtervo-config.toml cargo run -p xtervo-app --bin xtervo
```

(The example directory is `plugin-v3-echo`; the plugin id is `dev.xtervo.echo`.)

WASM example `docs/examples/plugin-wasm-filter/` (see section 6.6):

```bash
cd docs/examples/plugin-wasm-filter
./build.sh                      # needs wasm-tools or wat2wasm; filter.wasm is also committed
mkdir -p ~/.config/xtervo/plugins
cp -r . ~/.config/xtervo/plugins/wasm-filter
# Settings → Plugins → Enable → Allow and enable
```

---

## 12. Tests

```bash
cargo test -p xtervo-app --bin xtervo plugin_wasm
cargo test -p xtervo-app --bin xtervo plugin_v3::tests::wasm
cargo test -p xtervo-core --test output_filter
cargo test -p xtervo-app --bin xtervo
cargo check --workspace
```

The v3 tests use a real Python child process to cover:

- strict manifest validation / v1/v2 unsupported / invalid host_abi / entry escape;
- first-enable confirmation, state persistence, automatic startup after restart;
- initialize/initialized, command.invoke, status.get, ping, context.version;
- actual delivery of startup/tab-open/pane-focus events;
- bad-JSON restart, command timeout, status timeout not affecting the process,
  over-long lines, total stdout limit;
- terminal.read capability gate + Allow/Deny every time; terminal.write ask;
- clipboard gate (code path), credentials.get always denied;
- the seven declarative UI element types, ui.notify, ui.render;
- reload updating the manifest without restarting the host; permission changes
  requiring confirmation again;
- disable/enable/restart without restarting the host;
- the example manifest parses and its entry is executable;
- WASM: manifest/permissions/ABI, imports/signatures/memory limits,
  trap/fuel/epoch/out-of-bounds, example WAT↔WASM, real PTY enable transform/disable
  unchanged/trap fallback/reload error.

---

## 13. Known limitations (MVP)

- **process has no OS sandbox**; permissions are not enforced isolation. See section 1.
- WASM supports only `terminal.output_filter`: no commands/status/events/panels/
  settings/assistant, and no AOT cache, marketplace, signing or auto-update.
- WASM filtering is **single-instance and serial**: when multiple panes output at the
  same time they queue on this plugin's Mutex, and each call is bounded by the 20 ms
  epoch cap; the UI/render thread is not involved, but under extreme concurrency
  terminal throughput drops.
- WASM filters are invoked per PTY read block (currently about 32 KiB); patterns that
  span chunks require the plugin to keep state in its persistent linear memory. The
  example only matches `[xtervo-wasm]` within a single block.
- A filter can rewrite/delete terminal output (that is the feature itself), so
  displayed content is no longer trustworthy; enable only modules you trust. There is
  no signature verification.
- Plugin commands do not support command arguments (`args` is always empty). They can
  be triggered from the Palette, by their optional `keybinding` (section 3.4), and by
  clicking the status bar.
- Plugin themes are loaded only when xtervo starts; enable/reload at runtime does not
  hot-swap a theme.
- Multiple panels show only the first panel entry; the remaining IDs in
  `contributes.panels` are validated, but the current UI has no selector.
- markdown is minimal plain-text rendering; it does not activate links or render
  HTML/images.
- The status bar shows only one line per plugin; an over-long `status.get` result is
  rejected.
- `assistant` is only a placeholder and **there is no AI provider**; xtervo does not
  store/forward API keys.
- There is no plugin marketplace, signing or arbitrary Slint UI; WASM is not an
  arbitrary plugin runtime.
- **There is no plugin MCP**: MCP is an embedded, host-owned xtervo feature; plugins
  cannot provide/proxy an MCP server or obtain MCP tools through this protocol.
- terminal.read reads only the tail of the focused terminal; it does not provide
  cwd/env/Vault/full history.
- clipboard brokers text only (not images).
- After the automatic restart limit is reached, the user must Restart/Reload; there is
  no infinite restart.
- Child-process environment variables are cleared down to the allowlist; plugins that
  depend on other environment variables need their own launch wrapper script (note that
  the allowlist still does not change the "not a sandbox" fact).
- There is no plugin lock across xtervo instances: when several xtervo instances run at
  once, each may start the same plugin process and write the same state file (last
  writer wins). Single-instance use is recommended for production.
- Process-group/PDEATHSIG cleanup on non-Linux platforms has not been validated to the
  same standard; the primary validated platform is Linux.

---

## 14. Migrating from v1/v2

v1/v2 are not loaded; migration steps:

1. Add `api = 3`, `host_abi = 3` and `runtime = "process"` at the top level of the
   manifest. WASM is a new runtime within v3 and does not replace process; process
   behavior is unchanged.
2. Move `events` / `commands` / `status` / `theme` under `[contributes]`; `[[commands]]`
   keeps only `id` / `title` / `description`.
3. Add `[permissions]` (fields in section 3); the old `fs=... net=...` declarations
   need rewriting. The first enable asks the user to confirm again.
4. The protocol changes from the old xtervo JSON Lines to **JSON-RPC 2.0**:
   - old `ready` handshake → `initialize` request/response + `initialized`;
   - old `{"event":"command",...}` → `command.invoke` request;
   - old `{"event":"...","payload":...}` → `event.deliver` notification;
   - old `status` → `status.get` / `ui.status`;
   - old single-line stdout result → JSON-RPC response/error.
5. The old `oneshot` form is no longer supported; it must become a long-running
   process.
6. UI can only be provided through declarative documents; no bundled Slint/WASM/HTML.

After migration the v1/v2 directories can stay; xtervo shows `unsupported` and does not
execute them. Delete them manually once you are sure they are no longer needed.

---

## 15. Non-goals (explicitly not done in this version)

- plugin MCP server / MCP proxy / plugin tool marketplace;
- hosted plugin marketplace, remote auto-update, online key distribution/trust chain;
- arbitrary Slint UI; arbitrary WASM plugin APIs (only the import-free
  `terminal.output_filter` is supported);
- a built-in AI provider or plugin AI-provider routing;
- a dual-protocol v1/v2 compatibility layer;
- OS-level sandbox / enforced filesystem or network isolation.

MCP is a host-embedded feature (see [MCP](mcp.md) and
[MCP terminal control](https://github.com/HeyTycho/xtervo/blob/main/docs/MCP_TERMINAL_CONTROL.md)),
completely unrelated to the plugin protocol.
