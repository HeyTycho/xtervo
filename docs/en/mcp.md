# MCP

MCP (Model Context Protocol) lets a local agent or editor read and optionally control a running xtervo instance. It is built into xtervo, not a plugin, and has two surfaces:

- **Live control** over a local Unix socket owned by the xtervo process.
- **Offline diagnostics** over the `xtervo diag serve` stdio bridge, which needs no running app.

Live control is enabled by default but **read-only by default**. Writes must be granted explicitly, per permission group.

```text
MCP client
  ⇄ JSON-RPC 2.0 over stdio (one JSON object per line)
xtervo diag serve
  ⇄ local live Unix socket (0600 + state token)
xtervo app (embedded socket listener)
```

The socket is the only live-control boundary. It never gives an MCP client direct filesystem, Vault, environment-variable, or host-process access. Offline tools are separately restricted to explicit, bounded inputs. There is no network listener.

## Configuration

MCP settings are flat keys in `~/.config/xtervo/config.toml` (or `$XDG_CONFIG_HOME/xtervo/config.toml`; `XTERVO_CONFIG` overrides the path):

```toml
# Master switch. false = no live socket, no live tools.
mcp_enabled = true
# Four permission groups. Only read is on by default.
mcp_read = true
mcp_input = false
mcp_tabs = false
mcp_destructive = false
```

The keys are additive. Older configuration files without them load with `mcp_enabled = true`, `mcp_read = true`, and the other groups `false`; no schema version bump or migration is needed.

The same controls are in **Settings → Privacy → MCP live control**: the master switch, the four group switches, the socket and token paths, and readiness/error status. Changes are written to the config and applied immediately; no restart is required.

`mcp_enabled = false` is a hard off: no live socket is created, even if `XTERVO_DIAG_SOCKET` is set. Only the offline diagnostic tools remain available.

For tests and CI, these environment overrides still widen permissions:

```text
XTERVO_DIAG_ALLOW_WRITE=1         -> input + tabs
XTERVO_DIAG_ALLOW_DESTRUCTIVE=1   -> destructive
```

They cannot bypass `mcp_enabled = false`. Do not export them in a normal desktop session unless you intend to widen every MCP client.

## Permission groups

| Group | Config key | Socket commands | Default |
| --- | --- | --- | --- |
| read | `mcp_read` | `capabilities`, `snapshot`, `list`, `output` | on |
| input | `mcp_input` | `send_text`, `send_key`, `run` | off |
| tabs | `mcp_tabs` | `new_tab`, `new_window`, `split`, `focus`, `scroll` | off |
| destructive | `mcp_destructive` | `close_pane`, `close_tab`, `close_window`, `kill` | off |

`destructive` is an additional gate, never a standalone grant:

```text
close_pane / close_tab / close_window = tabs AND destructive
kill                                  = input AND destructive
```

Turning on `mcp_destructive` alone grants nothing. `close_window` additionally refuses to close the last window.

The app is the authority for permissions. On every `tools/list`, the bridge probes the app's `capabilities` and lists only the live tools the app currently allows. On every `tools/call`, the app validates the request again, so a stale tool list cannot bypass a tightened grant.

## Socket and token

| Item | Path | Mode |
| --- | --- | --- |
| Live socket | `$XDG_RUNTIME_DIR/xtervo/mcp.sock` | `0600` |
| Socket fallback (no runtime dir) | `$XDG_STATE_HOME/xtervo/mcp.sock`, or `~/.local/state/xtervo/mcp.sock` | `0600` |
| Socket fallback (path too long) | `$TMPDIR/xtervo-<uid>/mcp.sock` | `0600` |
| State token | `$XDG_STATE_HOME/xtervo/mcp.token`, or `~/.local/state/xtervo/mcp.token` | `0600` |

A Unix socket path is limited to about 108 bytes. xtervo uses the runtime path when it fits, then the state path, then the temp fallback (paths longer than about 100 bytes are rejected before bind).

The token is 32 random bytes encoded as 64 hex characters. It is generated once, reused across runs, and required on every request. Its value is never written to the config, application logs, or feedback bundles, and it is never echoed in an error.

- `xtervo diag serve` with no arguments reads the config and state paths and discovers both socket and token automatically.
- `--socket PATH` / `--token T` and `XTERVO_DIAG_SOCKET` / `XTERVO_DIAG_TOKEN` are explicit overrides for tests, CI, or non-default directories. They do not bypass `mcp_enabled = false`.
- A socket is replaced only when the old path no longer accepts a connection. A socket that still accepts connections is left to its owning xtervo instance; a second instance logs the conflict instead of stealing it.
- A wrong or missing token returns `unauthorized`. A connection failure names the socket path. If the capability probe fails, the bridge hides all live tools.

## Requests and window routing

Each socket connection sends one JSON object per line. Command parameters must be inside `args`; top-level fields other than `id`, `cmd`, and `token` are ignored.

```json
{"id": 3, "cmd": "split", "token": "<state token>", "args": {"pane_id": 1, "direction": "right"}}
```

Responses are always `{"id", "ok", "data"|"error"}`. An error never includes the token or the command text.

Every live command accepts an optional `args.window_id`:

- Omitted: the active window is used.
- Explicit: the ID must exist in the window registry, or the request fails with `unknown window_id`. There is no silent fallback to another window.
- A pane that does not exist in the routed window fails with `unknown pane N in window W`; write commands never return `ok: true` for a pane they did not touch.
- Responses echo the routed `window_id`, and pane objects from `list` carry their own `window_id`.

`list` returns `windows[]` and `active_window_id`; each window contains its pane IDs, titles, grids, rectangles, and focus state. `close_window` requires an explicit `window_id` and refuses the last window.

The app log records each live action as `[mcp] action=... window=... pane=... arg_bytes=...`. It never records command text or tokens.

## Tools

### Offline tools

These are available regardless of the live socket or `mcp_enabled`:

| Tool | Input | Output | Notes |
| --- | --- | --- | --- |
| `xtervo.diag_env` | none | version, display, config/profiles paths (and a log path when the process has one) | Does not return environment-variable values |
| `xtervo.diag_snapshot` | `command`, `shell`, `cols`, `rows`, `wait_ms` | frame, cursor, images | Starts a one-shot PTY and runs the caller's command; defaults `/bin/sh`, 120×32, 250 ms |
| `xtervo.diag_replay` | `input`, `cols`, `rows`, `wait_ms` | frame | Replays raw bytes through a one-shot PTY; defaults 120×32, 400 ms |
| `xtervo.doctor_check` | `environment`, `frame`, `metrics`, `symptom`, `details`, `repro_steps`, `expected`, `actual` | `xtervo.diagnostic_case.v1` JSON | Deterministic, offline, read-only, no shell |
| `xtervo.issue_draft` | `case` (a complete diagnostic case) | `markdown`, title, labels | Fixed doctor issue template; no model, no network |
| `xtervo.metrics_read` | `log_dir` (optional) | file/byte/line/error counts | Reads only the tail of `xtervo-*.log`; never returns raw lines |

`environment`, `frame`, and `metrics` may be inline JSON objects or paths to JSON files under `$HOME` or `/tmp`; each file is limited to 1 MiB, and a full MCP request line is also limited to 1 MiB. `doctor_check` and `issue_draft` are fully offline and do not start a shell. The only offline tool that runs a caller-provided command is `diag_snapshot`.

`metrics_read` reads at most eight regular `xtervo-*.log` files and only the last 256 KiB of each. It returns names, sizes, scanned byte/line counts, and counts of lines containing `error`, `panic`, or `fatal`; it never returns the lines themselves. Raw content of the logs is therefore not exposed through MCP.

### Live tools

A live tool appears in `tools/list` only when `mcp_enabled = true` and its group is granted.

| Tool | Group | Operation |
| --- | --- | --- |
| `xtervo.live_capabilities` | read | Current groups, allowed commands, and read-only/write state |
| `xtervo.live_snapshot` | read | Frame summaries for all panes: cursor, modes, damage, image rectangles |
| `xtervo.live_list` | read | `windows[]`, `active_window_id`, and pane IDs/titles/grids/rectangles/focus |
| `xtervo.live_output` | read | Bounded visible pane text tail |
| `xtervo.live_send_text` | input | Write text without pressing Enter (max 64 KiB) |
| `xtervo.live_send_key` | input | Send one of: `Enter`, `Tab`, `Escape`, `Ctrl+C`, `Ctrl+D`, `Ctrl+Z`, `Up`, `Down`, `Left`, `Right` |
| `xtervo.live_run` | input | Write one command line plus Enter; no shell wrapper is added |
| `xtervo.live_new_tab` | tabs | Create a tab |
| `xtervo.live_new_window` | tabs | Open a second in-process window |
| `xtervo.live_split` | tabs | Split a pane right or down |
| `xtervo.live_focus` | tabs | Focus a pane |
| `xtervo.live_scroll` | tabs | Scroll a pane's scrollback by signed lines |
| `xtervo.live_close_pane` | tabs + destructive | Close a pane |
| `xtervo.live_close_tab` | tabs + destructive | Close the tab containing a pane |
| `xtervo.live_close_window` | tabs + destructive | Close a window by `window_id`; never the last |
| `xtervo.live_kill` | input + destructive | Send Ctrl+C to a pane |

`live_output` defaults to 65,536 bytes and caps `max_bytes` at 262,144. `live_send_text` and `live_run` reject payloads over 65,536 bytes; `live_run` also rejects CR, LF, and NUL in the command because it adds the Enter itself.

Not implemented or exposed: `xtervo.live_paste`, `xtervo.live_resize`, `xtervo.live_metrics`, `xtervo.live_rename`. Do not depend on them.

### Resources

```text
xtervo://version   version/commit/build/target/display metadata (application/json)
```

`resources/list` returns a static descriptor. `resources/read` returns `contents[].text` as a JSON string. This is the only resource; live terminal content is not exposed as a resource, and subscriptions are not supported.

## Client setup

Claude Code:

```bash
claude mcp add xtervo-diag -- xtervo diag serve
```

Claude Desktop (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "xtervo": {
      "command": "/path/to/xtervo",
      "args": ["diag", "serve"]
    }
  }
}
```

Codex CLI (`~/.codex/config.toml`):

```toml
[mcp_servers.xtervo_diag]
command = "xtervo"
args = ["diag", "serve"]
```

Other harnesses should spawn `xtervo diag serve` and follow the standard MCP stdio sequence: `initialize` → `notifications/initialized` → `tools/list` → `tools/call`. Harnesses without MCP can use `xtervo diag env|snapshot|trace` directly (see [Diagnostics](diagnostics.md)).

`xtervo diag serve` is a built-in CLI subcommand, not a plugin. It uses stdin/stdout for JSON-RPC and writes its own diagnostics to stderr.

## Examples

A typical live flow after granting read, input, and tabs:

```text
xtervo.live_list
  -> find a shell pane
xtervo.live_send_text { pane_id, text: "cargo test" }
xtervo.live_send_key  { pane_id, key: "Enter" }
xtervo.live_output    { pane_id, max_bytes: 65536 }
xtervo.live_split     { pane_id, direction: "right" }
xtervo.live_focus     { pane_id: original }
xtervo.live_close_pane{ pane_id: created }
```

MCP `tools/call` shape:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"xtervo.live_list","arguments":{}}}
```

Offline doctor request:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
  "name":"xtervo.doctor_check","arguments":{
    "environment":{"schema":"xtervo.diag.v1","kind":"env","version":"0.6.1"},
    "symptom":"residual image after scrolling",
    "repro_steps":["open xtervo","scroll output"]
  }
}}
```

Pass the returned case object to `xtervo.issue_draft` as `arguments.case` to render the issue markdown. Offline tools inspect the `xtervo diag serve` process and any files you explicitly pass; they do not attach to the running app.

## Security and permissions

```text
socket mode 0600, local Unix socket only
state token 0600, required for every request
mcp_enabled master switch (false removes the socket)
read/input/tabs/destructive groups, hot-applied
destructive requires its base group
app-side validation of every tools/call
live tools do not read host files, Vault, or environment values
live tools do not execute host processes outside a terminal PTY
no automatic issue creation or git push
token never written to config/logs/feedback and never echoed in errors
```

When input or tabs is granted, an MCP client can effectively execute shell commands through the terminal, create or split panes, and scroll output. Closing panes additionally requires the destructive group. Grant those groups only to clients you trust.

There is no per-harness token scope and no built-in rate limit in this version. The limits are per-request argument lengths and the permission groups. Destructive operations have no per-call confirmation prompt. The app log is the audit trail for action, target window/pane, and argument byte count.

Offline read-only guarantees:

- File paths for JSON inputs and `log_dir` must be under `$HOME` or `/tmp`; paths are canonicalized and symlink escapes are rejected.
- `doctor_check` scans configuration for secret-like keys but reports only rule counts and hashes, never values.
- `metrics_read` returns counts, not raw log lines.
- No offline tool reads Vault contents, API keys, environment-variable values, or full scrollback.
- `diag_snapshot` still executes the shell command explicitly supplied by the caller; that is the documented local diagnostic behavior.
- MCP stdio is local only and has no network listener.

## Limits

- MCP is single-threaded and synchronous per bridge process; there is no call queue or concurrency.
- Live tools depend on the running app. If the capability probe fails or the socket is unavailable, they disappear from `tools/list` and live calls return an explicit error.
- Resources are limited to `xtervo://version`; subscriptions are not supported.
- The token is shared by all clients that can read the `0600` token file. Per-harness tokens, scopes, and rate limits are future work.
- `live_new_window` creates a second in-process window; there is no separate multi-process control plane.
- The `live_output` tail is visible-screen text, not an unbounded scrollback dump.
- Offline tools do not read live app state; they inspect the bridge process and the inputs you provide.

---

Chinese reference: [docs/MCP.md](https://github.com/HeyTycho/xtervo/blob/main/docs/MCP.md).
