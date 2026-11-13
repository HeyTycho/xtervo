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

已接入产品（v2 多窗口，不再只是数据层）：

保存：

```text
SharedServices 持有 2s autosave Timer（不再挂在 primary AppState）
  -> runtime::capture_restorable_snapshot_v2()
  -> session::save_v2()（tmp + fsync + rename + 上一代 .bak）
写入 windows[]：id / geometry / maximized / active_tab / tabs(含 splits/focus/cwd/profile)
窗口新建、关闭（WM close / MCP close_window / 关闭按钮）立即触发一次保存
只有 primary 进程是 writer；--window 子进程 writer=false，不写 marker/文件
恢复期间 restoring=true 抑制 autosave，避免半恢复布局覆盖源快照
```

恢复：

```text
启动读 session::load_v2()
  -> restorable_windows(): 按 id 排序、跳过空窗口、clamp active_tab、上限 16
  -> primary 建快照第一个窗口的 tabs/splits/focus + geometry/maximized
  -> 延迟 600ms 后再建 windows[1..]（先让 primary maximize 状态机绑定 XID）
  -> 全部窗口就绪后 session::clear()（消费一次）
v1 单窗口文件由 session::parse_snapshot 迁移为单窗口 v2（geometry=UNKNOWN，用 --cols/--rows）
空/坏快照：load_v2 返回 None -> 普通新 Tab；部分窗口失败：跳过该窗口/该 Tab，
  单窗口一个 Tab 都没成功时回退 new_tab_with_default，不整机失败
--restore 与 restore_session_on_startup 语义保留；--window 永不恢复
```

生命周期：

```text
autosave Timer / config watcher 都归 SharedServices
  -> primary 关闭而 second 存活时，session writer 与 config 热更新继续运行
  -> 关最后一个窗口才退出事件循环、mark_clean、最终 save_session_on_exit
关闭窗口统一走 close_window_now：remove_window -> GPU teardown -> hide/drop
  -> 立即保存剩余窗口布局；AppState 借用内只 defer 到事件循环，不 drop 借用中的 state
```

已知限制：

```text
- 第二窗口仍只注册终端/tab/split/close/maximize 核心回调；设置页/插件面板
  的完整回调仍是 primary 注册块（见 §6.1）。
- 第二窗口不启动 tervo-maximize X11 状态机：Slint 1.18 的
  WinitWindowAccessor 在多窗口启动阶段可能返回另一个窗口的 X11 handle。
  第二窗口用 winit is_maximized 50ms 轮询回写 maximized；primary 仍走状态机。
- 恢复窗口按快照 id 排序创建；WindowId 是新进程内重新分配的，不复用旧 id。
- 多窗口恢复路径不依赖 TERVO_MULTI_WINDOW_INPROC（产品路径）；该开关只控制
  手动“新建窗口”是否走 in-process。
```

---

## 5. 验证与证据

### 5.1 单窗口回归

```bash
CARGO_HOME=$PWD/../../.cargo \
CARGO_TARGET_DIR=$PWD/../../target \
CARGO_NET_OFFLINE=true cargo test -p tervo-app --bin tervo
# test result: ok. 549 passed; 0 failed; 1 ignored
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

### 5.5 多窗口 session v2 e2e

```bash
CARGO_TARGET_DIR=$PWD/../../target ./scripts/e2e_multi_window_session.sh
```

Xvfb + Openbox；真实 `tervo` 进程 + live socket + `xwin` 改窗口几何/最大化：

```text
开 2 窗口、2/3 tabs、不同 geometry、窗口 2 最大化
2s autosave 后 session.json version=2/windows=2，geometry/maximized/active_tab 正确
kill -9 primary，重启自动恢复 2 窗口：tabs 2/3、active_tab 1/2、geometry/maximized
primary 关闭后 second 继续 autosave（1 窗口 4 tabs）且 config 热更新继续生效
--restore 在 restore_session_on_startup=false 时仍恢复 2 窗口
v1 快照迁移为单窗口（2 tabs，active=1）
空窗口 + active_tab=99 的部分快照：跳空窗口、clamp active_tab、进程不失败
primary 最大化快照走 X11 状态机恢复
restore_session_on_startup=false 且无 --restore 时保持新布局
primary 关闭后 running marker 保留；最后一个窗口 WM close -> 退出码 0、
  running marker 清除、session.json 写回 1 窗口/4 tabs
```

证据：`docs/evidence/mw-session-integration/`（report.json + soak 采样 + 关键日志）。

### 5.6 2m 多窗口 session soak

与 §5.4 相同的双窗口 soak，但启用 `restore_session_on_startup=true`，确认
autosave 与两个窗口共存时 RSS/线程/FD 门禁不变。实测：

```text
120s：rss +3940KiB，threads 21 -> 21，fds 28 -> 28，2 窗口存活
```

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

已完成（本分支）：

```text
SharedServices 2s autosave -> runtime::capture_restorable_snapshot_v2()
  -> session::save_v2()（含 fsync/backup）
启动 session::load_v2() -> SnapshotV2::restorable_windows()
  -> primary + 其余窗口按 windows[] 恢复 geometry/maximized/active_tab/tabs/splits/focus
v1 迁移单窗口；空/坏/部分窗口回退可用窗口；--restore/配置开关语义保留
窗口关闭/新建立即保存；primary 关闭后 autosave/config watcher 继续运行
```

后续（不属于本分支）：

```text
第二窗口完整回调注册（§6.1）后，可把 tervo-maximize 状态机也按 WindowId 接入；
当前第二窗口最大化用 winit is_maximized 轮询回写。
```


### 6.4 ControlServer / 单实例

`instance.rs` 原型已存在但未接入启动流程；`--window` 仍是多进程兼容路径。
下一阶段把 `tervo` 的 secondary 转发到 primary `open_window`，再把
`window.new` 默认切到 in-process。

---

## 7. 提交记录

```text
85a4994 merge: WindowRegistry/SharedServices and in-process multi-window phase 1/2
（本分支最终 HEAD：feat(session): wire multi-window v2 save/restore into the
product —— SharedServices autosave/watcher、v2 全窗口恢复、session e2e；
见 git log）
```

未合 main、未 tag、未打包、未改 `dist-preview`。
