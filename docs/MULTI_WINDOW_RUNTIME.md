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
插件/MCP per-window 分支为第二窗口补了 palette + ask Allow/Deny 的最小回调
（见 §6.2），其余仍由共享回调注册处理。
下一阶段：把 primary 回调注册块抽成 `register_window_callbacks(wid, state)`，
每个闭包显式带 WindowId；这项是机械重构，但需要逐回调 e2e。

> Phase 3.5 已解决：179 个回调已抽成共享注册函数，闭包经
> `runtime::with_window(window_id, ...)` 显式路由。见 §9。

### 6.2 插件/MCP 协议

已完成：

```text
插件事件 payload + window_id（additive）
command.invoke.window_id = 发起命令的窗口
插件 -> 宿主 terminal.read/write、clipboard.read/write、ui.notify、
  ui.status、ui.render 支持 window_id（缺省 supervisor 解析为 active）
宿主 -> 插件 surface.render / ui.action 回传 window_id
terminal/clipboard ask 记录 window_id；Allow/Deny 只在请求窗口展示/执行
surface/status key：(window_id, surface) / (plugin, window_id)
关闭窗口清理该窗口 surface/status/notice 并取消未决 ask
MCP 所有 live 工具校验显式 window_id；缺省 active；audit 行包含 window
MCP list 返回 windows[] + active_window_id；close_window 拒绝最后一个窗口
PaneId 全局唯一
```

说明：

```text
- 显式 window_id 不存在时：插件 request 返回错误/ask 不会被别的窗口执行；
  notification/ui.render 由目标窗口消费，其它窗口不显示。
- 插件 push ui.render 到显式目标窗口且该窗口未打开 surface 时，在该窗口
  显示；不会覆盖另一个窗口已打开的 panel。
- 第二窗口的 palette/ask 回调是本分支补的最小集合；完整设置页回调仍见 §6.1。
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

已接入产品启动流程（Phase 3）：`tervo` 启动时先 `InstanceLock::try_acquire`；
primary 在 Slint 平台/WindowRegistry 就绪后 `PrimaryServer::start_with_lock`；
secondary 转发 `open_window`，primary 的 handler 用 `invoke_from_event_loop`
把建窗请求交给 UI 线程，窗口 `show()` 完成后才 ack。`--window` 同路径优先
转发；`window.new`/Ctrl+Shift+N 默认同进程。见 §8。

---

## 7. 提交记录

```text
85a4994 merge: WindowRegistry/SharedServices and in-process multi-window phase 1/2
（本分支最终 HEAD：feat(session): wire multi-window v2 save/restore into the
product —— SharedServices autosave/watcher、v2 全窗口恢复、session e2e；
见 git log）

per-window routing 分支（基于 48fef38）：
feat(plugin/mcp): per-window routing for plugin host calls and live tools
—— command.invoke/read/write/clipboard/notify/status/render + (window,surface)
两级 key、ask 窗口路由、MCP 全工具 window 校验/audit、双窗口插件 + MCP e2e
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

### 8.5 插件/MCP per-window 路由（本分支）

```bash
# 单测（worktree 私有 target，避免并行 agent 覆盖二进制）
CARGO_HOME=$PWD/../../.cargo CARGO_TARGET_DIR=$PWD/target \
  cargo test -p tervo-app --bin tervo
# 558 passed; 0 failed; 1 ignored（554 基线 + 4 新增）

# 插件双窗口 e2e：真实 Python v3 子进程 + XTest 点击 Allow
./scripts/e2e_plugin_window_routing.sh
# PASS：一个插件进程；command.invoke 窗口；两窗口分别 read 自己的 pane；
#       两窗口各自 panel 不互相覆盖；status 按窗口；关窗口清理；
#       MCP 显式 window 校验/审计；拒绝关闭最后一个窗口

# 多窗口 MCP e2e（扩展所有 live 工具显式 window 校验 + audit + 默认 active）
TERVO_INSTANCE_DIR=/tmp/tervo-mw-e2e-test E2E_OUT=/tmp/tervo-mw-e2e-out \
  CARGO_TARGET_DIR=$PWD/target ./scripts/e2e_multi_window_inproc.sh
# PASS：input/focus/split/new_tab/close_pane/close_tab + 全部未知 window 拒绝 +
#       [mcp] audit window=window-N + 缺省 window=active + kill -9 恢复

# 2m 内存门禁
SOAK_SECONDS=120 CARGO_TARGET_DIR=$PWD/target ./scripts/soak_multi_window_inproc.sh
# PASS 2m：rss +5.2MiB（两轮 3.5–5.2MiB）；threads 22->22；fds 30->30；2 窗口存活
```

未合 main、未 tag/打包/市场/AI。

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
- 设置页/插件面板完整回调仍只在 primary 注册块（第二阶段机械重构未做；
  Phase 3.5 已解决，见 §9）。
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

---

## 9. Phase 3.5：完整窗口回调（2026-11-13）

分支：`agent/mw-callbacks`；worktree：`.worktrees/mw-callbacks`；基线：main `9819d95`
（WindowRegistry/单实例/默认同进程多窗口）。
范围：把 primary 的 179 个 `ui.on_*` 回调抽成共享注册函数，让 secondary 成为
完整窗口；不改 session v2 / 单实例协议；未合 main、未 tag、未打包。

### 9.1 结构

```text
crates/app/src/main.rs
  fn register_window_callbacks(ui, state, window_id)
    - 179 个 ui.on_*（键盘/IME/鼠标/滚动/选择/tab/pane 菜单/命令面板/
      搜索/tab search/设置页全部分类与搜索/插件页与 surface/profile/
      SFTP/Vault/hostkey/SSH/paste/rename/save/背景图/主题/光标/粗体/
      字号/回滚/MCP 开关/最大化/quake/窗口关闭…）
    - 每个闭包只捕获 WindowId（Copy），状态访问一律
      runtime::with_window(window_id, ...)
    - 注册时的窗口初始化（主题展示/字号/回滚/quake 开关/render HUD）
      用 runtime::with_state(&state, ...)
  main()
    - registry.insert(primary) 后调用 register_window_callbacks
    - Quake 全局热键 listener 仍是进程级，只注册一次
crates/app/src/secondary.rs
  - 删除私有 install_core_callbacks；
  - WindowRuntime 注册后调用同一个 crate::register_window_callbacks；
  - show() 后启动该窗口的 X11 最大化状态机并同步初始 maximized。
```

回调闭包不持有 `Rc<RefCell<AppState>>`，避免 Slint component ↔ AppState 的
强引用环；窗口关闭时 registry 移除 `WindowRuntime` 即可释放 PTY/模型/timer。
`runtime::with_window` 在窗口已移除时返回 `None`，迟到的定时器不会误改别的窗口。

覆盖计数（源码级单测
`tests::all_primary_callbacks_live_in_shared_registration` 强制）：

```text
ui.on_* registration      179（全部在共享函数内）
with_app(                 0（共享函数内）
runtime::with_window      196 处
```

### 9.2 定时器 / 异步回调

原先定时器回调里无作用域的 `with_app` 会固定落到 primary。现在按窗口路由：

```text
400ms  poll_events_for(state)             每窗口一份（插件/SFTP/Vault/hostkey）
500ms  schedule_mcp_status_refresh(wid)   MCP socket 状态
100ms  sftp_op_timer(wid)                 SFTP 元数据操作
16ms/200ms resize redraw/finalize(wid)
50ms   drag autoscroll(wid)
30ms   bell flash(wid)
cursor blink(wid) / render HUD(wid)
app wake  -> runtime::with_all_windows(plugin/SFTP/Vault 结果)
auto theme watcher -> runtime::with_all_windows(共享主题变化)
```

文件拖放按全局唯一 `PaneId` 用 `runtime::window_for_pane` 路由到所属窗口；
native drop 的 hover 命中测试显式带 `window_id`。

### 9.3 关闭行为

```text
UI ✕ / on_close_window -> request_close
  需要确认 -> 该窗口 confirm（accept 后走同一路径）
  多窗口   -> 立即 remove_window + GPU teardown（路由/计数马上正确）+
              hide；WindowRuntime drop 延后到回调返回（避免在 Slint 回调栈
              上销毁正在执行的 component）
  最后一个 -> quit_event_loop，主流程保存/清理 session
WM CloseRequested -> 既有 install_native_window_events 路径不变
MCP close_window  -> 既有实现不变（要求 window_id + destructive）
```

移除延后到回调返回之后，避免在 Slint 控件回调栈上 drop 最后一个 MainWindow
强引用；被移除窗口的 UI 状态/模型不会再被另一个窗口看到。

### 9.4 双窗口语义

```text
窗口私有：MainWindow、所有 Slint model、tabs/panes/focus、设置页 category/
        搜索结果、命令面板 items、插件页 rows、profile/SFTP/Vault job。
进程共享：Config、ThemeRegistry、PluginStore（每插件一个子进程）、Vault、
        ProfileStore、MCP runtime/socket/token、SessionService。
```

MCP `list` 增加 additive 字段：

```json
{"windows":[{"window_id":1,"shared":{...},"ui":{
  "settings_open":true,"settings_category":5,"settings_results":0,
  "palette_open":false,"palette_items":10,"plugin_rows":1,
  "profiles_open":false,"sftp_open":false,"search_open":false,...}}]}
```

`shared` 是 `SharedServices::identities()`；两个窗口必须相等，e2e 用它证明
配置/插件/Vault/profile/MCP 只有一份。

### 9.5 验证与证据

```bash
CARGO_HOME=$PWD/../../.cargo CARGO_TARGET_DIR=$PWD/../../target \
  CARGO_NET_OFFLINE=true cargo test -p tervo-app --bin tervo
# test result: ok. 553 passed; 0 failed; 1 ignored
# 551 既有 + 2 新增源码级结构测试

CARGO_TARGET_DIR=$PWD/../../target ./scripts/e2e_multi_window_callbacks.sh
# PASS shared_services_one_copy
# PASS settings_opens_in_window_1_only
# PASS plugin_page_window_1
# PASS palette_opens_in_window_2_only / palette_runs_settings_in_window_2
# PASS both_windows_on_plugin_page
# PASS settings_category_models_independent
# PASS settings_search_stays_in_window
# PASS palette_roundtrip_window_1 / after_settings
# PASS close_primary_keeps_window_2_ui
# PASS remaining_window_accepts_new_tab
# PASS plugin_host_single_process / both_windows_registered_shared_callbacks
# evidence: target/multi-window-callbacks/report.json

CARGO_TARGET_DIR=$PWD/../../target ./scripts/e2e_multi_window_inproc.sh
# PASS 既有 Phase 3 多窗口 + kill -9 恢复（未回归）

CARGO_TARGET_DIR=$PWD/../../target ./scripts/soak_multi_window_inproc.sh
# PASS 2m：rss_growth=3628KiB；threads 23->23；fds 31->31

TERVO_BIN_DIR=$PWD/../../target/debug ./scripts/e2e_settings_i18n.sh
# PASS：单窗口设置页/搜索/分类/语言/热更新/右键/TabBar 菜单/legacy 未回归

CARGO_TARGET_DIR=$PWD/../../target ./scripts/e2e_single_instance_multi_window.sh
# PASS：两次 tervo / --window / Ctrl+Shift+N 转发；错误 token 拒绝；
#       close A 保 B；close primary 保其它；close 最后一个退出 0；
#       marker 只属于 primary、退出后清理（未改 instance 协议）
```

### 9.6 已知限制 / 未做

```text
- 15m/30m soak 未跑：本分支改了回调注册与 timer 路由，未改渲染热路径；
  合并 main 前按 AGENTS.md 升级 15m。
- 另一个窗口已打开的设置页不会实时重绘 mirror：配置数据是共享一份
  （窗口 A toggle 后 cfg 对所有窗口生效），但 B 的 Slint 属性/checkbox
  仍显示旧值，重新进入设置页/刷新时一致。主题/背景图/语言这类已写入共享
  cfg 的变更也只在触发窗口立即应用到渲染，其它窗口在下一次主题刷新/重开
  时一致。未做跨窗口属性广播。
- 插件 surface/permissions 仍是窗口私有 UI 状态（AppState），插件宿主共享；
  (window_id, surface) 两级协议 key 未做（见 §6.2）。
- MCP command.invoke / terminal.read/write 的 window_id 透传仍未做。
- session v2 多窗口保存/恢复未改（见 §6.3/其他分支）。
- IME 真实输入法上屏无法在 Xvfb 验证；键盘路径已 e2e，IME 回调与 primary
  共用同一注册函数。
- profile 编辑/SFTP/Vault/hostkey 的逐项交互没有单独 e2e：它们与 primary
  共用同一批已注册闭包，双窗口 e2e 覆盖设置页/命令面板/插件页/关闭语义。
- 第二窗口最大化只接入同一 X11 状态机并同步初始状态；双窗口 EWMH 行为
  未做专门 e2e。
```

### 9.7 提交

```text
feat(mw): share full window callback registration between windows
test(mw): dual-window settings/palette/plugin e2e + phase 3.5 docs
```

未合 main、未 tag、未打包、未改 dist-preview。
