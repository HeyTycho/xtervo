# Tervo 多窗口 Phase 1/2 实现状态

状态：**Phase 1/2 已完成；Phase 3（单实例接入 + 同进程多窗口默认）见 §8**  
分支：Phase 1/2 `agent/mw-runtime`；Phase 3 `agent/mw-instance-integration`  
worktree：Phase 3 `.worktrees/mw-instance-integration`  
基线：Phase 1/2 `1c5971e`；Phase 3 `85a4994`（`merge: WindowRegistry/SharedServices and in-process multi-window phase 1/2`）  
运行日期：2026-11-13  
范围：WindowRegistry/SharedServices 边界、单窗口回归、同进程第二窗口、GPU 状态隔离、单实例 IPC 产品接入。  
非目标：session v2 多窗口恢复（其他分支）、插件/MCP 完整 per-window scope、打包/合 main。

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

## 2. 默认路径与开关

Phase 3 起**默认路径**（不需要任何 env）：

```bash
tervo                    # 第一个进程成为 primary，监听 control.sock
tervo                    # 第二个进程转发 open_window，收到 ack 后退出 0
tervo --window           # 兼容：同样优先转发 primary
# Ctrl+Shift+N / window.new / MCP new_window 默认同进程新建 MainWindow
```

旧行为只保留为显式 fallback / 测试：

```bash
TERVO_MULTI_WINDOW_INPROC=0 tervo   # window.new 回退到旧子进程（legacy）
TERVO_MULTI_WINDOW_LEGACY=1 tervo   # 同上，语义更明确
TERVO_MULTI_WINDOW_SECOND=1 tervo   # 仅 e2e：启动即建第二个同进程窗口
TERVO_SINGLE_INSTANCE=0 tervo       # 仅排障/测试：跳过 lock/control server
TERVO_INSTANCE_DIR=/path tervo      # 仅测试/隔离：覆盖 control 目录
```

`TERVO_MULTI_WINDOW_INPROC=1` 仍然被接受，但不再是必需开关；默认就是新架构。
legacy 子进程由 `window::new_window_command` 注入 `TERVO_SINGLE_INSTANCE=0`，
避免它反过来被 primary 当成 secondary 再转发。

关闭（Phase 3 产品语义）：窗口收到 `CloseRequested` 时，如果 registry 中还有
其它窗口，只移除该 `WindowRuntime`（停止 PTY、释放 models/timers/GPU 引用）并
hide；其它窗口继续运行，包括 primary 窗口先关的情况。关闭最后一个窗口仍走
Slint 默认 hide -> 事件循环退出 -> 主流程保存 session 并清理 running marker。
session marker/autosave 只由 primary 进程持有；secondary 在写 marker 之前就已
转发并退出。

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
CARGO_TARGET_DIR=$PWD/target ./scripts/e2e_multi_window_inproc.sh
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
CARGO_TARGET_DIR=$PWD/target ./scripts/soak_multi_window_inproc.sh
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

已接入产品启动流程（Phase 3）：`tervo` 启动时先 `InstanceLock::try_acquire`；
primary 在 Slint 平台/WindowRegistry 就绪后 `PrimaryServer::start_with_lock`；
secondary 转发 `open_window`，primary 的 handler 用 `invoke_from_event_loop`
把建窗请求交给 UI 线程，窗口 `show()` 完成后才 ack。`--window` 同路径优先
转发；`window.new`/Ctrl+Shift+N 默认同进程。见 §8。

---

## 7. 提交记录

```text
95ff490 refactor(app): introduce WindowRegistry/SharedServices (single window)
7d576b4 feat(app): in-process second window behind TERVO_MULTI_WINDOW_INPROC
e39f6a0 docs(mw-runtime): architecture status, MCP window tools, evidence
（最终 HEAD：with_app 固定回退 primary、补 MCP window 工具、刷新证据；见 git log）
```

未合 main、未 tag、未打包、未改 `dist-preview`。


---

## 8. Phase 3：单实例接入 + 同进程多窗口默认（2026-11-13）

分支：`agent/mw-instance-integration`（基于 main `85a4994`，未合 main）

### 8.1 启动流程

```text
tervo
  -> 无 UI 模式（--version/--help/diag/--diagnose/import/export）先返回
  -> InstanceLock::try_acquire
       Some(lock)  -> primary：继续 Config/MCP/UI/session
       None        -> secondary：OpenWindowRequest{cwd} 转发 open_window
                      -> ack 0 / 明确错误 1（不初始化 Config/MCP/UI/session）
       --restore   -> 等待旧 primary 退出（Restart Session 的竞态）
  -> primary 在 registry 安装后 start_with_lock(handler)
       handler = invoke_from_event_loop(create_inproc_window_for) + 等待 UI
  -> 首个窗口 show / event loop
```

协议仍是 `instance.rs` 的一行 JSON + token；`control.sock`/`instance.token`
0600，目录 0700。secondary 在连接建立前失败（primary 正在启动）会重试；
请求发出后（timeout/protocol/rejected）绝不重试，避免重复开窗。

### 8.2 行为

```text
第二次 tervo / tervo --window -> 同一进程内新 MainWindow，window_id 递增
Ctrl+Shift+N / window.new      -> 同进程新窗口；不再 current_exe().spawn
MCP new_window                 -> 同进程新窗口（不再要求 INPROC）
关闭非最后窗口                 -> remove_window + hide + GPU teardown
关闭 primary 但其它窗口存活     -> 进程继续，其它窗口可继续 new_tab
关闭最后一个窗口               -> event loop 退出 0，主流程保存/清理 session
session running marker         -> 只在 primary 进程存活期间存在
```

### 8.3 验证与证据

```bash
# 用 worktree 私有 target：共享 target 可能被并行 agent 的二进制覆盖。
CARGO_HOME=$PWD/../../.cargo CARGO_TARGET_DIR=$PWD/target \
  CARGO_NET_OFFLINE=true cargo test -p tervo-app --bin tervo
# 551 passed; 0 failed; 1 ignored（546 既有 + 5 新增）

CARGO_TARGET_DIR=$PWD/target ./scripts/e2e_single_instance_multi_window.sh
# PASS：两次 tervo 调用 1 进程 2 窗口；--window 转发；Ctrl+Shift+N；
#       错误 token 拒绝；关 A 保 B；关 primary 保其它；关最后退出 0；
#       marker 只属于 primary、退出后清理
# evidence: target/single-instance-e2e/report.json
#           docs/evidence/mw-instance-integration/

CARGO_TARGET_DIR=$PWD/target ./scripts/e2e_multi_window_inproc.sh
# PASS：多窗口/独立 PTY/焦点/关 B/关 primary/插件单进程/MCP 单 socket/kill -9 恢复

CARGO_TARGET_DIR=$PWD/target ./scripts/soak_multi_window_inproc.sh
# PASS 2m：rss_growth≈2.0MiB；threads 22->22；fds 30->30
# evidence: target/multi-window-soak/samples.json
#           docs/evidence/mw-instance-integration/soak-samples.json
```

MCP e2e：`e2e_mcp_live_control.sh`、`e2e_mcp_settings.sh` 均通过（debug 构建，
Xvfb）。MCP settings 仍覆盖无 `XDG_RUNTIME_DIR` 的 state-dir fallback。

### 8.4 已知限制

```text
- OpenWindowRequest.profile 尚未接入 UI profile 选择：有 profile 时明确报错，
  不静默忽略；CLI 目前只转发 cwd。
- 转发窗口首 Tab 的 cwd 只在没有默认 Profile 时生效；有默认 Profile 时仍按
  Profile 打开（与 Ctrl+T 语义一致）。
- session v2 多窗口聚合保存/恢复仍由其他分支推进；本分支只把既有的
  单窗口 autosave/exit save 接到 primary 进程。
- primary 窗口先关、其它窗口存活时，primary 的 autosave timer 随 WindowRuntime
  停止；进程退出仍由主流程清理 marker，但本次不改 session 数据层，
  剩余窗口（opts.window=true）不会写多窗口快照。
- 设置页/插件面板完整回调仍只在 primary 注册块（第二阶段机械重构未做）。
- legacy fallback（INPROC=0/LEGACY=1）会起独立子进程；它通过
  TERVO_SINGLE_INSTANCE=0 绕过 primary 锁，因此不受单实例约束。只用于
  测试/排障，不是默认路径。
- Xvfb/llvmpipe 环境，不构成硬件 GPU 或真实桌面 WM 证据。
```

### 8.5 提交

```text
feat(instance): wire single-instance IPC and default in-process windows
docs(mw-runtime): phase 3 instance integration evidence
```

未合 main、未 tag、未打包。
