# MCP Terminal Control

Status (2026-11-13): MCP is enabled by default but **read-only by default**.
The live socket is created automatically at
`$XDG_RUNTIME_DIR/tervo/mcp.sock` (0600); the state token lives at
`$XDG_STATE_HOME/tervo/mcp.token` (0600). Grants are grouped and hot-applied in
Settings → Privacy. The live socket is embedded in Tervo core; it is not a
plugin and plugins do not host MCP.

Implemented live tools:

```text
read:  live_capabilities, live_snapshot, live_list, live_output
input: live_send_text, live_send_key, live_run
tabs:  live_new_tab, live_split, live_focus, live_scroll
L2:    live_close_pane, live_close_tab, live_kill
```

`live_close_pane`/`live_close_tab` require **tabs + destructive**;
`live_kill` requires **input + destructive**. `live_paste`, `live_resize`,
`live_metrics`, `live_rename` are not implemented/exposed.

## 1. Architecture

MCP is not embedded in the PTY. Tervo exposes a local live control socket, and
the MCP stdio bridge translates JSON-RPC tools into socket commands:

```text
external harness
  -> MCP stdio (tervo diag serve)
     -> local Unix socket (0600, token)
        -> Tervo UI/event loop
           -> AppState: tabs/panes/PTY
```

The socket is the only control boundary. MCP never gets direct filesystem,
Vault, environment-variable, or host-process access.

`tervo diag serve` uses the app as the authority for `tools/list`:

```text
tools/list
  -> capability probe ({"cmd":"capabilities","probe":true}, token)
     -> app returns commands filtered by mcp_enabled + read/input/tabs/destructive
        -> bridge lists only those live tools
```

The server no longer decides tool visibility from its own
`TERVO_DIAG_ALLOW_WRITE`/`TERVO_DIAG_ALLOW_DESTRUCTIVE`. The app also validates
every `tools/call`, so a stale tool list cannot bypass a tightened grant.

## 2. Permission groups

Config is flat TOML (`~/.config/tervo/config.toml`); defaults keep older config
files compatible (additive fields, no schema bump):

```toml
mcp_enabled = true       # master; false = no socket, no live tools
mcp_read = true          # capabilities/snapshot/list/output
mcp_input = false        # send_text/send_key/run
mcp_tabs = false         # new_tab/new_window/split/focus/scroll
mcp_destructive = false  # close_pane/close_tab/close_window/kill (additional gate)
```

| Group | Commands | Notes |
|---|---|---|
| read | `capabilities`, `snapshot`, `list`, `output` | default on |
| input | `send_text`, `send_key`, `run` | default off |
| tabs | `new_tab`, `new_window`, `split`, `focus`, `scroll` | default off |
| destructive | `close_pane`, `close_tab`, `close_window`, `kill` | default off; needs base group |

Safe default for destructive:

```text
close_pane / close_tab / close_window = tabs && destructive
kill                   = input && destructive
```

Turning on `mcp_destructive` alone grants nothing.

Multi-window additive fields: `list` returns `windows[]` + `active_window_id`;
most commands accept `window_id` (default active). `new_window` opens the
in-process second window when `TERVO_MULTI_WINDOW_INPROC=1`; `close_window`
refuses to close the last window. `WindowId`/`PaneId` are explicit routes, not
inferred from the active UI.

Old env overrides remain for tests/CI:

```text
TERVO_DIAG_ALLOW_WRITE=1         -> input + tabs
TERVO_DIAG_ALLOW_DESTRUCTIVE=1   -> destructive
```

They cannot bypass `mcp_enabled=false`. Do not export them in a normal desktop
session unless you intend to widen every MCP client.

### Settings UI

Settings → **Privacy → MCP live control** exposes:

- master switch;
- read/input/tabs/destructive switches;
- live socket path and state token path;
- enabled/readiness/error status.

Changes are saved to config and applied immediately (socket supervisor and
per-request app checks); no Tervo restart.

## 3. Default socket and state token

```text
socket: $XDG_RUNTIME_DIR/tervo/mcp.sock              (0600)
        fallback without XDG_RUNTIME_DIR:
        $XDG_STATE_HOME/tervo/mcp.sock
        (or ~/.local/state/tervo/mcp.sock / temp fallback)
        paths longer than ~100 bytes also fall back to the
        state/temp path (Unix socket length limit)
token:  $XDG_STATE_HOME/tervo/mcp.token              (0600)
        or ~/.local/state/tervo/mcp.token
```

- Tervo creates the socket automatically when `mcp_enabled=true`.
- The token is generated with 32 random bytes (hex) and reused across runs.
- The token value is never written to config, logs, or feedback bundles.
- `TERVO_DIAG_SOCKET` / `TERVO_DIAG_TOKEN` are explicit overrides for
  tests/CI; they do not override `mcp_enabled=false`.
- `tervo diag serve` with no arguments reads config/state and discovers both.
- A stale socket is replaced only after a connection probe fails; an active
  socket owned by another Tervo instance is left alone.
- Wrong/missing token -> `unauthorized`; connection failures name the socket
  path. `tools/list` hides live tools when the probe fails.

## 4. Operation tiers

### L0: read-only (default)

| Tool | Operation |
|---|---|
| `tervo.live_capabilities` | Group switches, allowed commands, limits/version |
| `tervo.live_snapshot` | Frame summary: cursor, modes, damage, image rectangles |
| `tervo.live_list` | Pane ids, titles, grids, rectangles, focus (no tab list) |
| `tervo.live_output` | Bounded visible output tail (not full scrollback) |

Read limits:

```text
max output bytes per call (capped)
no environment-variable values
no API keys/tokens/passwords
no unbounded scrollback dump
```

### L1: terminal control (`mcp_input` / `mcp_tabs`)

| Tool | Operation |
|---|---|
| `tervo.live_send_text` | Write text to a pane (no automatic Enter) |
| `tervo.live_send_key` | Enter/Escape/Ctrl+C/Ctrl+D/arrows/F-keys subset |
| `tervo.live_run` | Write one command line + Enter; no shell wrapper added |
| `tervo.live_focus` | Focus a pane |
| `tervo.live_scroll` | Scroll a pane's scrollback |
| `tervo.live_new_tab` | Create a tab |
| `tervo.live_new_window` | Open a second in-process window (flag required) |
| `tervo.live_split` | Split a pane right/down |

This is effectively shell execution through the terminal when input is granted.
Required controls:

```text
state token + 0600 socket
explicit per-group grants
app-side validation of every tools/call
argument-length caps
unknown pane/tab -> explicit error, never guess
```

There is no per-harness token scope or built-in rate limit yet; the app log
records action/pane/argument bytes (never argument text or secrets).

### L2: destructive (`mcp_destructive` + base group)

| Tool | Operation |
|---|---|
| `tervo.live_close_pane` | Close a pane |
| `tervo.live_close_tab` | Close a tab |
| `tervo.live_close_window` | Close a window by `window_id` (never the last) |
| `tervo.live_kill` | Send Ctrl+C to a PTY |

All live tools accept an optional `window_id`; `live_list` returns `windows[]`
and `active_window_id`, and each pane carries its `window_id`.

L2 is disabled by default. `close_*` also needs `mcp_tabs`; `kill` also needs
`mcp_input`. There is no per-call confirmation prompt in this version.

## 5. Example harness flow

```text
(default read-only)
tervo.live_list
tervo.live_output
```

After granting input+tabs+destructive:

```text
tervo.live_list
  -> find a shell pane
tervo.live_send_text { pane, text: "cargo test" }
tervo.live_send_key  { pane, key: "Enter" }
tervo.live_output    { pane, max_bytes: 65536 }
tervo.live_split     { pane, direction: "vertical" }
tervo.live_focus     { pane: original }
tervo.live_close_pane{ pane: created }
```

MCP request shape:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"tervo.live_list","arguments":{}}}
```

## 6. Security model

```text
socket mode 0600, Unix local only
state token 0600, required for every request
mcp_enabled master switch (false removes the socket)
read/input/tabs/destructive groups, hot-applied
destructive cannot be granted without its base group
app-side tools/call validation (server tools/list is only a cache)
no host filesystem / Vault / env values / host process execution
no automatic issue creation or git push
token never written to config/logs/feedback; never echoed in errors
```

Defaults:

```text
mcp_enabled = true
read = enabled
input/tabs/destructive = disabled
```

## 7. Explicitly not allowed

```text
read arbitrary host files
read Vault/credentials/environment values
execute host commands outside the terminal PTY
modify user config/plugins/repository
automatically submit issues or push code
unbounded scrollback reads
connect without a token
write/close/kill while the matching group is off
```

## 8. Current vs planned

Current mainline (implemented and covered by tests/e2e):

```text
offline:
  tervo.diag_env
  tervo.diag_snapshot
  tervo.diag_replay
  tervo.doctor_check
  tervo.issue_draft
  tervo.metrics_read
live read:
  tervo.live_capabilities
  tervo.live_snapshot
  tervo.live_list
  tervo.live_output
live input:
  tervo.live_send_text
  tervo.live_send_key
  tervo.live_run
live tabs:
  tervo.live_new_tab
  tervo.live_split
  tervo.live_focus
  tervo.live_scroll
live destructive:
  tervo.live_close_pane
  tervo.live_close_tab
  tervo.live_kill
```

Not exposed yet: `live_paste`, `live_resize`, `live_metrics`, `live_rename`.
Per-harness token scopes and rate limits are still follow-up work.

Evidence (2026-11-13):

```bash
./scripts/e2e_mcp_settings.sh      # default read-only, hot grants, master off/on, token
./scripts/e2e_mcp_live_control.sh  # env-override write path
./scripts/e2e_diag_live.sh         # read-only raw socket + bridge
```

## 9. Acceptance gates

```text
default startup creates 0600 socket + 0600 state token without env
read-only list/snapshot/output work through no-arg `tervo diag serve`
input/destructive tools absent and their calls rejected by default
config edit grants input/tabs/destructive; tools appear and work without restart
tightening config immediately hides/rejects the corresponding calls
mcp_enabled=false removes the socket and all live tools
mcp_enabled=false at startup creates no socket, even with TERVO_DIAG_SOCKET
wrong token -> unauthorized; wrong-token bridge hides live tools
missing socket/connection failure -> explicit error
no token value in app log/config
```
