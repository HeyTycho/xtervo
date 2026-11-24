# WASM output filter — Plugin API v3 example

A real hot-path terminal filter: `[xtervo-wasm]` in PTY output becomes
`WASM-FILTER-OK` before the bytes reach Sixel/IIP/Kitty/vte parsing.

This is **not** a process plugin. It has no commands, events, panels, settings,
WASI, filesystem, network, or process access. It only implements
`terminal.output_filter`.

## Files

| file | purpose |
|---|---|
| `plugin.toml` | `api = 3`, `host_abi = 3`, `runtime = "wasm"`, `terminal.output_filter = true` |
| `filter.wat` | readable source of the filter |
| `filter.wasm` | assembled module committed for convenience (463 bytes) |
| `build.sh` | reassemble `filter.wat` with `wasm-tools` or `wat2wasm` |

The filter scans the current chunk for the 12-byte literal `[xtervo-wasm]`,
writes `WASM-FILTER-OK` (14 bytes), and copies every other byte unchanged. The
output buffer is at linear-memory offset 0; input starts at 262144, so output
can never overwrite bytes that have not been read.

## Build and install

```bash
cd docs/examples/plugin-wasm-filter
./build.sh                       # optional: filter.wasm is already committed
mkdir -p ~/.config/xtervo/plugins
cp -r . ~/.config/xtervo/plugins/wasm-filter
```

Start xtervo → **Settings → Plugins** → the row shows `runtime = wasm` →
**Enable** → review `terminal.output_filter` → **Allow and enable**.

```bash
printf 'hello [xtervo-wasm]\n'
# terminal shows: hello WASM-FILTER-OK
```

Disable the plugin in the same running xtervo; the next output is unchanged.
No pane or xtervo restart is needed.

Or point xtervo at this examples root:

```bash
XTERVO_PLUGINS=$PWD/docs/examples XTERVO_CONFIG=/tmp/xtervo-v3-wasm.toml \
  cargo run -p xtervo-app --bin xtervo
```

## ABI and limits

- exports: `memory`, `alloc(i32)->i32`, `filter(i32,i32)->i32`;
- no imports (WASI or otherwise);
- memory maximum ≤256 pages (16 MiB);
- input/output ≤256 KiB per call;
- 16,000,000 fuel and ~20 ms epoch deadline per call;
- alloc must return a pointer ≥262144; host verifies every address;
- trap/timeout/oversize/out-of-bounds → original bytes pass through and the
  filter is faulted until Reload/Restart.

The filter receives PTY chunks (currently about 32 KiB). The example only
matches patterns inside one chunk; a stateful filter can keep state in linear
memory across calls.

## Security

The module runs in a no-import Wasmtime sandbox. It cannot read files, network,
processes, environment, Vault, clipboard, or other panes. It **can** rewrite or
delete terminal output by design, so only enable modules you trust. xtervo has
no marketplace, signature verification, or automatic updates.

MCP is a built-in xtervo feature and is **not** available to plugins.

See [`../../PLUGIN_V3.md`](../../PLUGIN_V3.md) section 6 for the full ABI,
resource/error semantics, tests, and limitations.
