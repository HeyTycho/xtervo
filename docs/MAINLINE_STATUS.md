# Tervo Mainline Status / Mainline Audit

Snapshot: 2026-11-12, branch `master`, HEAD `737e70a`
Purpose: one place for supported features, fixes, and branch merge decisions.
The historical inventory stays in [`FEATURE_MATRIX.md`](FEATURE_MATRIX.md);
this file is the current mainline truth.

## Supported surface (mainline)

### Terminal / UI

- Multi-tab, split panes, profiles, session restore, multi-window (`--window`).
- Settings tab: appearance/theme, terminal, window, keys, profiles, plugins,
  Vault, Help; settings search and jump.
- Search/find, read-only, reset, save contents, selection/copy/paste, URL/OSC 8
  hover and Ctrl+click paths, context menu (flat menu currently), Quake/F12,
  fullscreen, background image/opacity/dim.
- X11 native window; maximize uses the state machine below.

### Rendering

- Default path: GPU-native WGPU30 + Scene (`Frame -> Scene -> WGPU`).
- CPU/Skia fallback, `TERVO_CPU=1`; no adapter/render failure falls back to CPU.
- Glyph atlas text, CJK/symbol fallback, wide/spacer/hidden/dim/selection/search.
- Sixel, iTerm2 IIP, Kitty graphics; source registry, crop/clip, z-order,
  below/above-text composition; Sixel bytes/2J ordering fixed.
- Cursor remains a Slint overlay; selection highlight is Scene/CPU text color.
- Damaged frames use a scissor/Load path for text and image passes; no-damage
  redraws reuse the previous GPU target. Partial-vs-full pixel test lives in
  `crates/render-wgpu/tests/gpu_text.rs`; HUD exposes partial/full/reuse counts.
- Render HUD: Settings -> Terminal -> Images -> Render HUD, or
  `TERVO_RENDER_HUD=1`; FPS/frame time/renderer/GPU draws/atlas/stage0/grid.

### Diagnostics / automation

- `tervo diag env|snapshot|replay|trace|metrics|observability|scene-diff|
  trace-layer|doctor`.
- `tervo diag doctor [--issue]`: deterministic Doctor CLI.
- Settings -> Help -> Run Doctor: deterministic findings/evidence/issue UI.
- `tervo diag serve`: MCP stdio server. Read-only tools:
  `diag_env`, `diag_snapshot`, `diag_replay`, `doctor_check`, `issue_draft`,
  `metrics_read`; live read tools `live_capabilities`, `live_snapshot`, `live_list`,
  `live_output`; write-gated tools `live_send_text`, `live_send_key`, `live_run`,
  `live_focus`, `live_new_tab`, `live_split`, `live_scroll`; destructive-gated
  `live_close_pane`, `live_close_tab`, `live_kill`. MCP is embedded in core, not a plugin.
- P0 filter byte-conservation audit and trace-layer; real dsh raw replay.
- Doctor core: `tervo.diagnostic_case.v1`, evidence bundle/hash/redaction,
  issue template, status/health checks.

### Plugins / integrations

- Plugin API v3 only: process-isolated, JSON-RPC 2.0, api=3/host_abi=3,
  permissions + first-enable confirmation, supervisor, events/status/theme,
  declarative panels/settings, enable/disable/reload/restart. v1/v2 entries are
  shown unsupported and never spawned.
- Feedback button: settings → Help → Feedback creates a redacted local
  tar.gz and opens a configured issue page; no auto API/MR.
- MCP is the external agent/tool surface; it is not a plugin capability.
- SSH/SFTP/Vault/Profiles integrations already in the product surface.

## Fixes merged in the current cycle

| Fix | Commit/merge |
|---|---|
| Default GPU-native; stage 0 no longer default; CPU escape hatch | `78a5a3e` |
| Sixel `Pass` before `Image`, prevents `2J` clearing a just-placed image | `c01dd30` |
| GPU target imported after render, first-present snapshot race | `e40f7b8` + latest e2e delay |
| GPU images: registry/crop/clip/below-above z-order | `8db464f`, `5e7dd99` |
| Sixel/Kitty/IIP filter conservation and trace-layer | `be14f08`, `108407f` |
| Industrial maximize state machine | `8db8347`, merged to master |
| MCP doctor/metrics/resources tools | `fed5a0e`, merge `50aa1b0` |
| Render HUD + settings toggle/search | `b6f7d71`, `79f32c9`, `5d02d39`, `b43664d` |
| Deterministic Doctor CLI + Settings Run Doctor | `a627230`, `7b18b24` |
| Selection cleared on wheel/scrollbar scrolling | `b6333fd` |
| No-damage GPU target reuse + damage contract test | `da0a195`, `0d141c1` |
| Kitty placement ID written as positive i32-safe range | cherry-pick `502e490` (content from `1b9d39b`) |
| Ubuntu Dock duplicate New Window: action id `new-window` | `fe9d4f2` |
| Damage-scissor contract/tests/gates | `843da05` |
| Text damage scissor + partial-vs-full test | `e12101d` |
| Image damage scissor + HUD partial/full counters | `28c89f8` |
| MCP live terminal control (L0/L1/L2 gated) | `b232a4e`, `d016613`, `fcbe98c` |
| Feedback bundle/issue page | `b3b862c`, `50d4bc2`, `737e70a` |
| Plugin API v3 host | `d5bdbbd`, `af098bc`, merge `4174519` |
| MCP e2e stale-socket fix | `fdfae65` |

## Branch merge audit (2026-11-12)

Content already on mainline:

| Branch | Decision |
|---|---|
| `agent/gpu-native`, `agent/gpu-images` | merged |
| `agent/filter-audit`, `agent/trace-layer`, `agent/doctor-core` | merged |
| `agent/max-controller` | merged; revert/reapply history is expected |
| `agent/mcp` | merged |
| `agent/kitty-graphics` | content cherry-picked as `502e490`; branch ancestry still unmerged |
| `agent/plugin-v3` | merged as `4174519` (v3-only host, v1/v2 unsupported) |
| `agent/scroll-render` | superseded by mainline `7cf220b` (`row_top.max(top)` image landing fix); no code merge needed |
| `feat/config-hotreload` | superseded by mainline `1228748`; `config_watch.rs` is present |
| `feat/mem-converge` | image eviction fix `66ae0c6` already mainline; only optional memprobe instrumentation remains unmerged |
| `feat/soak` | superseded by newer mainline soak script (`37b2a75`, `f6ae438`) |
| `test/dsh-images` | test-only harness branch, known 14/15 with zoom failure; not merged into mainline to avoid adding a known-failing test |

Do not delete unmerged branches/worktrees without reviewing their commits; commits are
retained, but dirty worktrees may contain uncommitted evidence:

```text
.worktrees/font-share       glyph.rs modified (font sharing experiment)
.worktrees/image-before     render.rs modified (A/B experiment)
.worktrees/memdiag          main.rs modified + memprobe.rs (memory instrumentation)
.worktrees/maximize-click-repro  evidence/test artifacts
```

## Known gaps (do not claim done)

- Selection behavior is "clear on wheel/scrollbar", not RowId-anchored content.
- AI provider/model/streaming and full AI Doctor conversation are not implemented;
  only deterministic Doctor + MCP tools. AI Doctor is a future plugin.
- Plugin v3 has no OS sandbox; permissions are capability gates + confirmation.
  No market/signing/install-update, no plugin MCP, no arbitrary Slint UI.
- WASM hot-path runtime is implemented only as a no-import
  `terminal.output_filter` (Wasmtime, `runtime = "wasm"`); no plugin commands,
  UI, WASI, market, signing, or arbitrary WASM APIs.
- Wayland native, color emoji/shaping, atlas eviction, 24h soak remain open.
- MCP write tools require env switches and a token; per-harness scopes/audit
  retention are still minimal.
- MCP terminal-control design and planned write tools: [`MCP_TERMINAL_CONTROL.md`](MCP_TERMINAL_CONTROL.md).

## Release rules

`v0.3.1` is the canonical existing release. `v0.3.1-gpu-rc1..6` are local
non-standard previews and must not be pushed as releases. The next functional
version is `0.4.0`; use `v0.4.0-rc.N` for pre-releases and build from an exact
clean tag with `scripts/package.sh`.
