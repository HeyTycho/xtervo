# Tervo 多窗口 Phase 1/2 实现状态

状态：**Phase 1 + Phase 2 已完成（feature flag）**  
分支：`agent/mw-runtime`  
worktree：`.worktrees/mw-runtime`  
基线：`1c5971e`（`merge: multi-window Slint/WGPU spike evidence`）  
运行日期：2026-11-13  
范围：WindowRegistry/SharedServices 边界、单窗口回归、flag 下同进程第二窗口、GPU 状态隔离。  
非目标：单实例 ControlServer、session v2 多窗口恢复、插件/MCP 完整 per-window scope、打包/合 main。

---

## 0. 结果（TL;DR）

```text
Stage A  WindowId + WindowRuntime + WindowRegistry + SharedServices
         单窗口行为不变；546 个 app 单测全绿
Stage B  TERVO_MULTI_WINDOW_INPROC=1
         同 event loop 两个 MainWindow，各自 tabs/panes/focus/models
         关闭 B 不影响 A；插件一份；MCP 一个 socket
         WGPU device/queue 共享；关 B 后 A 仍 GPU ready 且继续出帧
```

关键文件：

```text
crates/app/src/runtime.rs       WindowId / WindowRegistry / SharedServices
crates/app/src/secondary.rs     第二窗口创建 + 核心回调
crates/app/src/main.rs          build_window_state / with_window 路由 / MCP 窗口命令
crates/app/src/gpu_present.rs   per-WindowId device 引用计数
crates/app/src/gpu_native.rs    per-WindowId atlas/target 引用计数
scripts/e2e_multi_window_inproc.sh
scripts/e2e_multi_window_gpu.sh
scripts/soak_multi_window_inproc.sh
```

---

## 1. 运行时边界

### 1.1 WindowId / WindowRuntime / WindowRegistry

```rust
pub struct WindowId(u64);          // primary 从 1 开始，单调递增，不复用
pub struct WindowRuntime {
    id: WindowId,
    ui: MainWindow,
    state: Rc<RefCell<AppState>>,
    timers: RefCell<Vec<slint::Timer>>,
    title: RefCell<String>,
    last_focused_pane: Cell<Option<PaneId>>,
}
pub struct WindowRegistry {
    windows: HashMap<WindowId, Rc<WindowRuntime>>,
    active: Option<WindowId>,
    shared: Rc<SharedServices>,
}
```

- 所有 UI/Slint/`AppState` 访问都在 Slint UI 线程。
- `WindowRuntime` 拥有窗口私有的 MainWindow、models、tabs、panes、focus、
  renderer/GPU surface、geometry、timers。
- `WindowRegistry` 只做 `HashMap` + 活动窗口；不持有共享服务生命周期。
- `PaneId` 改为**全进程唯一**（`SharedServices::alloc_pane_id`）；插件/MCP/session
  不再出现两个窗口的 pane 都从 1 开始。

### 1.2 路由 API

```rust
with_window(window_id, |&mut AppState| ...)   // 显式路由（新代码）
with_state(&Rc<RefCell<AppState>>, ...)        // 构建/回调作用域
with_active_window(...)                        // 旧 with_app 的缺省
with_all_windows(|window_id, ...| ...)         // 配置/快照广播
window_for_pane(pane_id)                       // PTY 线程只带 pane id
```

`with_state/with_window` 进入时设置 thread-local 当前窗口作用域，退出恢复。
因此回调内部继续调用旧 `with_app()` 会沿当前窗口路由，不会盲猜全局 `APP`。
`APP` 单例已删除；没有作用域时 `with_app` **固定回退 primary**（primary 已关闭
才落到 active），所以 primary 的旧回调不会在 B 聚焦时误改 B。新回调一律使用
`with_state/with_window`。

### 1.3 SharedServices

```rust
pub struct SharedServices {
    cfg: Shared<Config>,          // 唯一配置；watcher 替换后所有窗口可见
    themes: Shared<ThemeRegistry>,
    plugins: Shared<PluginStore>, // 每插件一个子进程，与窗口数无关
    vault: Shared<Vault>,         // 一次解锁，所有窗口共享
    profiles: Shared<ProfileStore>,
    mcp: Arc<McpRuntime>,         // 一个 token / 一个 live socket
    session: Shared<SessionService>,
    next_pane_id: Cell<u64>,
}
```

`Shared<T>` 是 UI 线程内的 `Rc<RefCell<T>>` 句柄，`Deref/DerefMut` 让既有
`st.cfg.*` 调用点不需要改成 borrow 链；安全约束写在 `runtime.rs` 类型文档：
只在 UI 线程、不得在 `&mut T` 生命周期内再取同一 `T`。

第二窗口只通过句柄注入，不会重新 `Config::load` / `PluginStore::load` /
`Vault::open_default` / `ProfileStore::load`，不会启动插件进程。

---

## 2. Feature flag 与操作

```bash
# 允许 Ctrl+Shift+N / window.new 在同进程创建第二窗口
TERVO_MULTI_WINDOW_INPROC=1 tervo

# e2e/证据：启动即打开第二个窗口（仍需 INPROC=1）
TERVO_MULTI_WINDOW_INPROC=1 TERVO_MULTI_WINDOW_SECOND=1 tervo
```

关闭：窗口收到 `CloseRequested` 时，如果 registry 中还有其它窗口，只移除该
`WindowRuntime`（停止 PTY、释放 models/timers/GPU 引用）并 hide；A 继续运行。
关闭最后一个窗口仍走 Slint 默认 hide -> 事件循环退出 -> 主流程保存 session。

MCP（`live_*`）：

```text
list                       -> windows[] / active_window_id / 每窗口 panes
* 命令 + window_id          -> 显式路由；缺省 active_window_id
new_window                 -> 同进程开第二窗口（要求 flag）
close_window               -> 关闭指定窗口（要求 tabs + destructive；拒绝关最后一个）
```

插件事件 payload 增加 additive `window_id`；旧插件忽略即可。`command.invoke` /
`terminal.read/write` 的 per-window 参数仍在下一阶段（见 §6）。

---

## 3. GPU 状态隔离

修复前：`gpu_present::install/teardown` 和 `gpu_native::install/teardown` 是纯全局
单例；第二个窗口 setup 会清空 frames/atlas，任意窗口 teardown 会把 `READY=false`，
关闭 B 会让 A 掉回 CPU。

现在：

```text
gpu_present::install_for(WindowId, device, queue)
  - 同一 device/queue：只登记窗口，保留 frames/READY
  - device 变化：才丢弃旧纹理
gpu_present::teardown_for(WindowId)
  - 移除该窗口 + 该窗口 pane 纹理
  - windows 非空：保留共享 device/纹理；为空才 READY=false
gpu_present::retain_for(WindowId, active_panes)
  - 按所有窗口 active pane 并集回收，避免按单窗口误删
gpu_native 同上（CLIENTS/PANES_BY_WINDOW）
```

`render_probe::install_for(WindowId, ...)` 把 `RenderingSetup/RenderingTeardown`
按窗口路由；Slint 1.18 的 WM close 默认只 hide、不保证立即 teardown，因此
应用关闭窗口时显式调用 `teardown_for`，保证引用计数正确。

实测（`target/multi-window-gpu/report.json`）：

```text
before close: present_ready=true present_windows=[1,2] native_frames=16
after close B: present_ready=true present_windows=[1]  native_frames=16
A 输出后:     native_frames 16 -> 17
日志: window-2 rendering teardown：保留共享 device/纹理（1 个窗口仍存活）
```

---

## 4. Session 与关闭行为

- primary 仍写单窗口 session（v2 单窗口格式）；`--window`/第二窗口不写 marker。
- 第二窗口 `opts.window=true`，不参与 autosave/退出快照。
- 2s autosave 与退出保存仍只由 primary 执行；窗口 B 关闭不会改 session。
- kill -9 单窗口恢复已实测：2s autosave 后 kill -9，重启恢复 3 个 tab。

已知限制（下一阶段）：

```text
- 多窗口 session v2 聚合保存/恢复尚未接入：两个窗口的布局不会跨重启恢复。
- primary 关闭而 second 存活时，config watcher/autosave 随 primary AppState
  一起停止；shared cfg 仍在，但热更新/自动保存需要移到 SharedServices。
- 关闭 primary 后 session 退出钩子仍以 primary opts 判断，可能不写多窗口布局。
```

---

## 5. 验证与证据

### 5.1 单窗口回归

```bash
CARGO_HOME=$PWD/../../.cargo \
CARGO_TARGET_DIR=$PWD/../../target \
CARGO_NET_OFFLINE=true cargo test -p tervo-app --bin tervo
# test result: ok. 546 passed; 0 failed; 1 ignored
```

### 5.2 双窗口 e2e

```bash
CARGO_TARGET_DIR=$PWD/../../target ./scripts/e2e_multi_window_inproc.sh
```

断言（全部 pass，证据 `target/multi-window-e2e/report.json`）：

```text
两个 MainWindow / 单进程 / pane id 全局唯一
窗口 1、2 独立 tabs（2/2）
两个 PTY 输出互不串台；跨窗口 pane 查询返回 unknown pane
X11 聚焦 A/B 时 active_window_id 跟随切换
MCP close_window 关 B 后剩余 1 个窗口、A 可继续 new_tab
插件进程只 1 个
MCP live socket ready 日志 1 次、socket 文件 1 个
kill -9 单窗口恢复 3 个 tab
```

### 5.3 GPU 双窗口

```bash
CARGO_TARGET_DIR=$PWD/../../target ./scripts/e2e_multi_window_gpu.sh
```

见 §3。环境是 llvmpipe/Vulkan CPU adapter，不是硬件 GPU 性能证据。

### 5.4 2m 双窗口 soak

```bash
CARGO_TARGET_DIR=$PWD/../../target ./scripts/soak_multi_window_inproc.sh
```

2 个窗口持续输出，5s 采样 RSS/线程/FD；要求窗口存活、RSS 增长 ≤80MiB、
线程 ≤+8、FD ≤+24。最终实现实测（`docs/evidence/mw-runtime/soak/`）：

```text
120s：rss +3520KiB，threads 21 -> 21，fds 28 -> 28，2 窗口存活
```

未跑 15m/30m；按 AGENTS.md 触及渲染/循环应在合并前升级。

---

## 6. 已知限制 / 下一阶段入口

### 6.1 第二窗口 UI 回调范围

第二窗口已注册终端核心回调：

```text
key_input / key_release / ime_commit
pane_clicked / mouse_down/move/up / scrolled / divider_dragged
tab_clicked / tab_closed / new_tab
resized / native focus / close / IME
```

设置页、插件面板、profile 编辑器、SFTP/Quake/菜单等完整回调仍只在 primary
注册块。第二窗口能跑终端与 tab/split（快捷键路径），但设置面板交互不完整。
下一阶段：把 primary 回调注册块抽成 `register_window_callbacks(wid, state)`，
每个闭包显式带 WindowId；这项是机械重构，但需要逐回调 e2e。

### 6.2 插件/MCP 协议

已完成：

```text
plugin event payload + window_id（additive）
MCP list/new_window/close_window + window_id 参数
PaneId 全局唯一
```

未完成：

```text
command.invoke / terminal.read / terminal.write 的 window_id 透传
插件 panel/surface 的 (window_id, surface) 两级 key
MCP audit 记录 window_id（当前只记 pane_id）
```

### 6.3 Session v2 多窗口

下一阶段入口：

```text
runtime::capture_snapshot_v2()
  -> session::save_v2()
  -> 启动时按 windows[] 先建 geometry/窗口，再 restore tabs
WindowSnapshot 已包含 id/geometry/maximized/active_tab/tabs
```

### 6.4 ControlServer / 单实例

`instance.rs` 原型已存在但未接入启动流程；`--window` 仍是多进程兼容路径。
下一阶段把 `tervo` 的 secondary 转发到 primary `open_window`，再把
`window.new` 默认切到 in-process。

---

## 7. 提交记录

```text
95ff490 refactor(app): introduce WindowRegistry/SharedServices (single window)
7d576b4 feat(app): in-process second window behind TERVO_MULTI_WINDOW_INPROC
e39f6a0 docs(mw-runtime): architecture status, MCP window tools, evidence
（最终 HEAD：with_app 固定回退 primary、补 MCP window 工具、刷新证据；见 git log）
```

未合 main、未 tag、未打包、未改 `dist-preview`。
