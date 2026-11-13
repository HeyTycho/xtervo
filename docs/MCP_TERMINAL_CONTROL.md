# MCP Terminal Control

Status: L0/L1/L2 implemented and exercised by external Python harnesses
(2026-11-12). L2 is gated by a separate destructiveness switch. The live socket
is embedded in Tervo core; it is not a plugin and plugins do not host MCP.

Implemented live tools:

```text
read:  live_capabilities, live_snapshot, live_list, live_output
write: live_send_text, live_send_key, live_run, live_focus,
       live_new_tab, live_split, live_scroll
L2:    live_close_pane, live_close_tab, live_kill
```

L1 requires `TERVO_DIAG_ALLOW_WRITE=1`; L2 additionally requires
`TERVO_DIAG_ALLOW_DESTRUCTIVE=1`. Both require a token. The MCP server only lists
tools whose switch is enabled. `rename`/`resize` are not implemented yet.

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

## 2. Operation tiers

### L0: read-only (default)

| Tool | Operation |
|---|---|
| `tervo.live_capabilities` | Supported operations, limits, version |
| `tervo.live_list` | Windows/tabs/panes, active pane, dimensions, title, cwd, process state |
| `tervo.live_snapshot` | Frame summary: cursor, modes, damage, image rectangles |
| `tervo.live_output` | Bounded output tail/range (not full scrollback) |
| `tervo.live_metrics` | **planned / not exposed yet** |

Read limits:

```text
max output bytes per call
max lines per call
no environment-variable values
no API keys/tokens/passwords
no unbounded scrollback dump
```

### L1: terminal control (requires `TERVO_DIAG_ALLOW_WRITE=1` + token)

| Tool | Operation |
|---|---|
| `tervo.live_send_text` | Write text to a pane (no automatic Enter) |
| `tervo.live_paste` | **planned / not exposed yet** |
| `tervo.live_send_key` | Send key/chord: Enter, Ctrl+C, Ctrl+D, arrows, F-keys |
| `tervo.live_run` | Write one command line + Enter; optional output/exit wait |
| `tervo.live_focus` | Focus a tab/pane |
| `tervo.live_scroll` | Scroll a pane's scrollback |
| `tervo.live_resize` | **planned / not exposed yet** |
| `tervo.live_new_tab` | Create a tab |
| `tervo.live_split` | Split a pane horizontally/vertically |

This is effectively shell execution through the terminal. Required controls:

```text
per-harness token
explicit write switch
audit entry for every action
argument-length and rate limits
unknown pane/tab -> explicit error, never guess
```

### L2: destructive (per-action confirmation)

| Tool | Operation |
|---|---|
| `tervo.live_close_pane` | Close a pane |
| `tervo.live_close_tab` | Close a tab |
| `tervo.live_kill` | Send interrupt/terminate to a PTY |
| `tervo.live_rename` | **planned / not exposed yet** |

L2 is disabled by default or requires an explicit confirmation for each call.

## 3. Example harness flow

```text
tervo.live_list
  -> find a shell pane
tervo.live_send_text { pane, text: "cargo test" }
tervo.live_send_key  { pane, key: "Enter" }
tervo.live_output    { pane, max_bytes: 65536 }
  -> read result
tervo.live_split     { pane, direction: "vertical" }
  -> run a dev server in the new pane
tervo.live_focus     { pane: original }
tervo.live_close_pane{ pane: created }
```

MCP request shape:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"tervo.live_list","arguments":{}}}
```

## 4. Security model

```text
socket mode 0600, Unix local only
token required when configured
TERVO_DIAG_ALLOW_WRITE=1 required for L1/L2
per-harness capability scopes
read/write/destructive tiers
audit: who, when, pane, action, argument bytes, result
argument and rate limits
no host filesystem / Vault / env values / host process execution
no automatic issue creation or git push
```

Defaults:

```text
L0: enabled
L1: disabled until write switch + token
L2: disabled or per-call confirmation
```

## 5. Explicitly not allowed

```text
read arbitrary host files
read Vault/credentials/environment values
execute host commands outside the terminal PTY
modify user config/plugins/repository
automatically submit issues or push code
unbounded scrollback reads
connect without a token
write PTY while the write switch is off
```

## 6. Current vs planned

Current mainline:

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
live write:
  tervo.live_send_text
  tervo.live_send_key
  tervo.live_run
  tervo.live_focus
  tervo.live_new_tab
  tervo.live_split
  tervo.live_scroll
live destructive:
  tervo.live_close_pane
  tervo.live_close_tab
  tervo.live_kill
```

Not exposed yet: `live_paste`, `live_resize`, `live_metrics`, `live_rename`.
Per-harness token scopes and durable audit retention are still follow-up work;
the app log records action/pane/argument bytes.
Evidence: external Python harness connects to `TERVO_DIAG_SOCKET`, runs
`list -> send_text -> send_key Enter -> output`, creates a tab, verifies
wrong-token rejection, and the MCP stdio bridge e2e passes.

## 7. Environment and startup

Tervo（GUI）和 MCP server 是两个进程。先带 live socket 环境启动 Tervo：

```bash
export TERVO_DIAG_SOCKET="${XDG_RUNTIME_DIR:-/tmp}/tervo-diag.sock"
export TERVO_DIAG_TOKEN="$(head -c 24 /dev/urandom | base64 | tr -d '\n')"
export TERVO_DIAG_ALLOW_WRITE=1          # only for L1
export TERVO_DIAG_ALLOW_DESTRUCTIVE=0    # only for L2; keep 0 unless needed
tervo
```

MCP client 配置（示例值要换成实际绝对路径/token）：

```json
{
  "mcpServers": {
    "tervo": {
      "command": "/home/you/.local/bin/tervo",
      "args": [
        "diag", "serve",
        "--socket", "/run/user/1000/tervo-diag.sock",
        "--token", "same-token-as-the-app"
      ],
      "env": { "TERVO_DIAG_ALLOW_WRITE": "1" }
    }
  }
}
```

MCP server 的 `TERVO_DIAG_ALLOW_WRITE` 必须也设置，否则 `tools/list` 不会出现
写工具；L2 还需要在 MCP server 环境里设置 `TERVO_DIAG_ALLOW_DESTRUCTIVE=1`。

MCP server 走 stdio，由客户端显式 spawn；没有网络监听。

## 8. Acceptance gates

```text
read-only list/snapshot/output
send_text + Enter -> output contains expected token
send_key Ctrl+C -> process interrupt
new_tab/split/focus/scroll
close/kill requires destructive switch + token
wrong token -> rejected
write switch off -> write tools rejected
argument/rate limits enforced
audit entries contain no secrets
external harness and MCP stdio e2e pass
```

`live_paste`/`live_resize`/`live_metrics`/`live_rename` are planned, not shipped.
