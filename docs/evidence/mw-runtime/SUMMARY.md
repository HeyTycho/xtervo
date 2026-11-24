# Multi-window runtime evidence (2026-11-13)

Branch: `agent/mw-runtime`  
Worktree: `.worktrees/mw-runtime`  
Base: `1c5971e`  
Implementation commits: `95ff490` (Stage A), `7d576b4` (Stage B), plus docs/cleanup commit.

Environment: headless Xvfb (`1280x800x24`), debug build, `SLINT_BACKEND=winit-software`
for e2e/soak; GPU run uses llvmpipe/Vulkan CPU adapter (`SLINT_WGPU_CPU=1`).
No hardware GPU / Wayland / 30m soak coverage.

---

## 1. Unit tests (single-window regression)

```bash
CARGO_HOME=/home/user/AIworkspace/rusttabbyterminal/.cargo \
CARGO_TARGET_DIR=/home/user/AIworkspace/rusttabbyterminal/target \
CARGO_NET_OFFLINE=true cargo test -p xtervo-app --bin xtervo
```

```text
test result: ok. 546 passed; 0 failed; 1 ignored
```

## 2. In-process two windows (`e2e/`)

```bash
CARGO_TARGET_DIR=/home/user/AIworkspace/rusttabbyterminal/target \
  ./scripts/e2e_multi_window_inproc.sh
```

All checks pass (`e2e/report.json`, `e2e/app.log`):

```text
two_mainwindows_one_process   windows=[1,2] panes=[1,2] (unique)
independent_tabs              window 1 tabs=2, window 2 tabs=2
independent_pty_output        pane 3 -> MW_A only; pane 4 -> MW_B only
pane_route_not_global         output(window=1, pane=4) -> unknown pane 4
focus_routes_active_window    X11 focus A/B -> active_window_id 1/2
close_b_keeps_a               MCP close_window(2): remaining=1, A tabs=3
close_primary_keeps_c         new_window=3; close_window(1): C stays, tabs=2
one_plugin_process            plugin.py count=1
one_mcp_socket                "live socket ready" x1, *.sock x1
```

kill -9 recovery (single window, `e2e/recovery.log`):

```text
[session] autosave ...（3 个标签 / 3 个 pane）
kill -9
[session] 检测到上次异常退出；尝试恢复最近 autosave
[session] 已按快照恢复：3 个标签 / 3 个 pane
restored_tabs=3
```

## 3. GPU isolation (`gpu/`)

```bash
CARGO_TARGET_DIR=/home/user/AIworkspace/rusttabbyterminal/target \
  ./scripts/e2e_multi_window_gpu.sh
```

`gpu/report.json`:

```text
before:      present_ready=true present_windows=[1,2] native_frames=16
close B:     present_ready=true present_windows=[1]   native_frames=16
A output:    native_frames 16 -> 17
log:         window-2 rendering teardown：保留共享 device/纹理（1 个窗口仍存活）
             window-1 RenderingSetup：复用同一 WGPU30 device/queue
```

`present_frames=0` is expected: `XTERVO_GPU_NATIVE=1` uses the GPU Scene path, so
stage-0 CPU texture uploads are not driven; `native_frames` is the live counter.

## 4. 2m dual-window soak (`soak/`)

```bash
CARGO_TARGET_DIR=/home/user/AIworkspace/rusttabbyterminal/target \
  ./scripts/soak_multi_window_inproc.sh
```

```text
duration=120s
baseline: rss=100588KiB threads=21 fds=28 windows=2
final:    rss=~104108KiB threads=21 fds=28 windows=2
growth:   rss=+3520KiB, threads=0, fds=0
PASS 2m dual-window soak
```

Samples: `soak/samples.csv`, full JSON `soak/samples.json`.

## 5. Scope / limits

Implemented and tested:

```text
WindowId / WindowRuntime / WindowRegistry / SharedServices
one SharedServices (Config/Theme/PluginHost/MCP/Session/Vault)
feature flag XTERVO_MULTI_WINDOW_INPROC=1
two MainWindows, one event loop, independent tabs/panes/focus/models
close B keeps A; close primary keeps C
global unique PaneId; PTY redraw routed by PaneId
MCP window_id + list/new_window/close_window
plugin events carry additive window_id
per-WindowId GPU present/native install/teardown/retain
```

Not done (next phase, see `docs/MULTI_WINDOW_RUNTIME.md` §6):

```text
multi-window session v2 save/restore
full settings/plugin-panel callbacks in secondary window
plugin command.invoke / terminal.read per-window args
single-instance ControlServer forwarding
15m/30m soak
```
