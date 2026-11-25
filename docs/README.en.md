# xtervo documentation (English index)

[简体中文](README.md) | **English**

This is the English entry point for the detailed documentation. Most design and
implementation documents are currently written in **Chinese**; this index gives an
English title and a one-line summary for each of them so you can find the right file.
Translations are welcome — see [CONTRIBUTING.en.md](../CONTRIBUTING.en.md).

Start with the root [README.en.md](../README.en.md) for install, usage, shortcuts and
known limitations.

## Getting started

| Document | Summary |
|---|---|
| [Verification handbook (Chinese)](VERIFICATION.md) | Number reconciliation, build/test/soak gates, step-by-step checks for three environment blockers, and a feature smoke checklist. |
| [Architecture (Chinese)](ARCHITECTURE.md) | Target architecture plus the current implementation boundary. |
| [Product principles and roadmap (Chinese)](PRODUCT_PRINCIPLES.md) | Product north star: positioning, non-goals, and decision priorities. |
| [UI / visual design spec (Chinese)](UI_DESIGN.md) | “The terminal is paper, the UI is the margin”: tokens, spacing, icons, menus, motion, bilingual layout. |
| [Licensing and compliance (Chinese)](LICENSING.md) | GPL-3.0-only, Slint 1.18 licensing, third-party notices and trademark usage. |

## User guides

| Document | Summary |
|---|---|
| [Plugin v3: local install / update / rollback / signature checks (Chinese)](PLUGIN_INSTALL.md) | Local plugin package management only; no hosted marketplace, cloud sync, online key distribution or trust chain. |
| [Plugin system: goals, runtimes, capability types and market research (Chinese)](PLUGIN_SYSTEM.md) | Design for the plugin host; V1 shipped the stable external-process subset. |
| [Plugin API v1/v2 (legacy, Chinese)](PLUGIN_API.md) | Historical v1/v2 protocol; the host only loads v3 manifests now. |
| [Plugin API v3 (Chinese)](PLUGIN_V3.md) | The only supported plugin protocol: `api = 3`, `host_abi = 3`, process JSON-RPC and narrow WASM output filters. |
| [Transport plugin design (Chinese)](PLUGIN_TRANSPORT.md) | `PaneTransport`/`SocketPairTransport`/`SplitTransport` seam used by SSH/Serial/Telnet plugins. |
| [SSH / SFTP / Vault productization (Chinese)](SSH_SFTP.md) | `profiles.toml` v2 fields, SSH options, SFTP queue and Vault integration. |
| [Official SSH plugin POC (Chinese)](SSH_PLUGIN_POC.md) | Running an SSH pane on the `Backend::Plugin` transport seam; scope and current limits. |
| [Vault: encrypted credentials and SSH askpass (Chinese)](VAULT.md) | Encrypted credential store, background KDF and askpass flow. |
| [MCP bridge: offline diagnostics + live control (Chinese)](MCP.md) | Configuration, tools and security model for the MCP server. |
| [MCP terminal control (Chinese)](MCP_TERMINAL_CONTROL.md) | Live control API and permissions. |
| [Diagnostics CLI (Chinese)](DIAGNOSTICS.md) | `xtervo diag ...`, Doctor findings and evidence. |
| [Logging and diagnostics (Chinese)](LOGGING.md) | Per-run log files, what to collect for bug reports. |
| [Stability soak (Chinese)](SOAK.md) | Reproducible long-running stress test with RSS/PSS, threads and FDs. |
| [Versioning and releases (Chinese)](RELEASING.md) | SemVer rules, immutable annotated tags, reproducible artifacts and the release checklist. |
| [Keyboard / IME gaps (Chinese)](KEYBOARD_LIMITATIONS.md) | Known limitations of Slint key events and IME behavior. |
| [Workspace shell: sidebar / toolbar / status area (Chinese)](WORKSPACE_SHELL.md) | Sidebar, status pills and workspace management UI. |
| [Observability (Chinese)](OBSERVABILITY.md) | Backend-neutral metrics and CLI schema. |
| [AI diagnostician (Chinese)](AI_DIAGNOSTICIAN.md) | Direction document for assisted diagnostics (not a chat box). |
| [Feedback / bug reports (Chinese)](FEEDBACK.md) | Built-in feedback path in Settings → Help → Feedback. |

## Terminal and UI behavior

| Document | Summary |
|---|---|
| [Terminal basics: bell / scroll policy / scrollbar / Backspace · Delete (Chinese)](terminal-behavior.md) | Settings → Terminal; values are written to `~/.config/xtervo/config.toml` and applied live. |
| [Terminal menu and find (Chinese)](terminal-menu-find.md) | Match case/regex, Read Only, Reset, Save Contents. |
| [Terminal UI basics (Chinese)](terminal-ui-basics.md) | Scroll position, visual bell and drag-and-drop. |
| [Themes: built-in, custom and importing existing palettes (Chinese)](themes.md) | 12 built-in themes; iTerm2/Windows Terminal/Alacritty/kitty/Xresources/YAML import. |
| [Tabby theme research and xtervo theme plan (Chinese)](tabby-themes.md) | Research notes on Tabby's theme layering; reference only, not a compatibility target. |
| [Stable interaction baseline: window / tab bar (Chinese)](STABLE.md) | Behaviors verified on 2026-11-06; do not experiment on them directly on main. |
| [UI interaction matrix (Chinese)](UI_INTERACTION_MATRIX.md) | Step-by-step manual matrix with screenshots and child-process counts. |
| [UI shell: tabs / splits / context menu (Chinese)](ui-shell.md) | UI shell structure and interactions. |

## Design and internals

| Document | Summary |
|---|---|
| [Vendored alacritty_terminal RowId patch (English)](ALACRITTY_ROW_ID.md) | The one focused change in `vendor/alacritty_terminal` relative to upstream 0.26.1. |
| [Inline image compositing v2 (Chinese)](IMAGE_COMPOSITING.md) | Why the old compositing model was wrong and how the framebuffer path works. |
| [Image engine contract v1 (English)](IMAGE_ENGINE_CONTRACT.md) | Frozen interface between the terminal core and the image engine. |
| [Font fallback, color emoji and wide characters (Chinese)](FONT_FALLBACK.md) | Candidate fonts by purpose and fallback order. |
| [Multi-monitor / dock support (Chinese)](MULTI_MONITOR.md) | Basic multi-monitor behavior and constraints. |
| [Multi-window architecture (Chinese)](MULTI_WINDOW_ARCHITECTURE.md) | Target design: one process, multiple windows, shared services. |
| [Multi-window runtime status (Chinese)](MULTI_WINDOW_RUNTIME.md) | Phase 1–4 implementation status, session v2 and window routing. |
| [GPU damage scissor plan (English)](GPU_DAMAGE_PLAN.md) | Scissor/damage plan for the GPU path. |
| [GPU rendering performance and baseline (Chinese)](GPU_PERFORMANCE.md) | Acceptance targets and measured baselines for the GPU pipeline. |
| [Platform abstraction and GPU roadmap (Chinese)](PLATFORM_GPU.md) | Platform boundary and the GPU rendering direction. |
| [Rendering roadmap (Chinese)](RENDERING_ROADMAP.md) | Long-term renderer goals and milestones. |
| [PaneHandle transport wiring plan (Chinese)](TRANSPORT_WIRING.md) | Wiring the transport seam into panes. |

## Status and reports

| Document | Summary |
|---|---|
| [Feature matrix (Chinese)](FEATURE_MATRIX.md) | Historical audit of features with status and evidence; not a roadmap. |
| [Closure report (Chinese)](CLOSURE.md) | How the feature matrix closes, with weighted completion. |
| [Mainline status / audit (English)](MAINLINE_STATUS.md) | Current supported surface, fixes and merge decisions on `main`. |
| [Performance and packaging baseline (Chinese)](PERFORMANCE.md) | Reproducible performance/size numbers and gates. |
| [Stability report (Chinese)](STABILITY_REPORT.md) | Stability verification results. |
| [Scope review (Chinese)](SCOPE_REVIEW.md) | What counts as core and what can be left out. |
| [Core-to-80 checklist (Chinese)](CORE_TO_80.md) | Seventeen items to bring the core terminal from 69% to 80%+. |

## Internal process notes

These are historical snapshots, not user documentation. Current facts live in
`MAINLINE_STATUS.md` / `FEATURE_MATRIX.md`.

| Document | Summary |
|---|---|
| [Internal process records (Chinese)](internal/README.md) | Kick-off sheets, nightly progress, task board, action items and review backlog. |

## Archive

Historical reports and one-off analyses: [`archive/`](archive/).
Notable entries:

| Document | Summary |
|---|---|
| [Render residue postmortem 2026-11-11 (Chinese)](archive/POSTMORTEM_2026-11-11_RENDER_RESIDUE.md) | Render-residue debugging discipline and root cause. |
| [Input focus postmortem 2026-11-18 (Chinese)](archive/POSTMORTEM_2026-11-18_INPUT_FOCUS.md) | Profile menu focus trap and the fix. |
| [Wayland native spike (Chinese)](archive/WAYLAND_SPIKE.md) | Native Wayland feasibility findings. |
| [Final delivery report (Chinese)](archive/FINAL_REPORT.md) | Historical delivery snapshot (numbers are outdated). |
| [Manual retest checklist (Chinese)](archive/RETEST_CHECKLIST.md) | Template for manual acceptance on real hardware. |

## Other

| Document | Summary |
|---|---|
| [dsh-tui image preview e2e (Chinese)](DSH_IMAGE_PREVIEW_E2E.md) | Isolated harness that verifies image preview without touching the real session. |
