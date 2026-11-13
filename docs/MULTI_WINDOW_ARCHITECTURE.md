# Tervo 多窗口目标架构

状态：**Phase 1/2 已实现（feature flag）**  
实现分支：`agent/mw-runtime`；细节/证据见 [`MULTI_WINDOW_RUNTIME.md`](MULTI_WINDOW_RUNTIME.md)。  
当前默认仍是单窗口；`TERVO_MULTI_WINDOW_INPROC=1` 下支持同进程第二窗口。  
`--window` 多进程路径保留为兼容/回退。  
本文回答：如果要把多窗口做成产品能力，最好的方案是什么。

---

## 0. 结论

推荐目标：

```text
单进程、多窗口（Single Process, Multi-Window）
+ 单实例主进程
+ 共享服务层
+ 每窗口独立 WindowRuntime
+ 全局 WindowId / PaneId 路由
```

不再把 `tervo --window` 当作产品形态，而是：

```text
第一次启动           → 成为 primary，创建窗口 1
再次启动 tervo       → 通过 Unix control socket 请求 primary 打开窗口 2
Ctrl+Shift+N         → 同一个 primary 内创建窗口
关闭一个窗口         → 只销毁该窗口的 PTY/UI/GPU surface
关闭最后一个窗口     → primary 保存 session、关闭插件、退出
```

一句话：

```text
窗口是进程内的独立运行时，不是独立进程。
```

---

## 1. 为什么不是继续多进程

当前 `--window` MVP：

```text
每个窗口 = 独立 Tervo 进程
每个进程 = 一套 AppState / PTY / 插件宿主 / MCP socket / GPU 资源
```

看起来隔离，实际上制造了共享状态竞争：

| 资源 | 当前多进程行为 |
|---|---|
| config.toml | 共享，最后写入者覆盖 |
| plugins-v3-state.toml | 共享，无锁，授权可能互相覆盖 |
| 插件进程 | 每个窗口各启动一份，同一插件跑 N 次 |
| MCP socket | 一个 runtime dir 只能一个，第二个窗口不可控 |
| session.json | 只有主窗口写，子窗口不写；多主实例互相覆盖 |
| Vault | 每进程独立解锁，互不继承 |
| 全局热键 | 每个实例都注册，冲突 |
| GPU atlas/target | 每窗口一份，内存近似翻倍 |

这些不是“小问题”，而是**架构上缺少窗口身份和共享服务所有权**。

继续多进程就必须补：

```text
跨进程 broker
实例选举
状态文件锁/合并
Plugin RPC 路由
MCP 多实例路由
Tab 跨窗口 IPC
```

这是把桌面终端做成一个小型分布式系统，复杂度高于单进程多窗口。

---

## 2. 目标架构

```text
┌──────────────────────── Tervo primary process ────────────────────────┐
│                                                                       │
│  Slint event loop (UI thread)                                         │
│                                                                       │
│  WindowRegistry                                                       │
│    WindowId(1) ── WindowRuntime                                       │
│    WindowId(2) ── WindowRuntime                                       │
│    ...                                                                │
│                                                                       │
│  Shared services (UI thread ownership)                                │
│    ConfigStore / ThemeRegistry                                        │
│    PluginHost  (one process per plugin, not per window)                │
│    McpHost     (one socket/token per user)                            │
│    SessionManager                                                     │
│    VaultService                                                       │
│    GlobalHotkeyService                                                │
│    ControlServer (single-instance IPC)                                │
│                                                                       │
└───────────────────────────────────────────────────────────────────────┘
          │                    │                         │
          ▼                    ▼                         ▼
   pane actor threads    plugin superviors         MCP client bridge
   (PTY/vte/frame)       (already isolated)        (stdio → socket)
```

### 2.1 WindowRuntime

每个窗口拥有：

```text
WindowId
Slint MainWindow 实例
Workspace（tabs / splits / focus）
HashMap<PaneId, PaneRuntime>
per-window renderer / GPU surface / damage state
geometry / maximized / fullscreen / DPR
last_focused_pane
window title
```

每个窗口不拥有：

```text
Config / ThemeRegistry
PluginHost
MCP socket/token
Vault unlock state
Session file writer
Global hotkey registration
```

### 2.2 Shared services

共享服务只有一个实例，原因是它们本身就是“每用户”资源：

```text
配置只有一个
插件启用状态只有一份
MCP 一个 token/socket
Vault 一次解锁
session 一份
全局热键一组
```

共享服务不直接持有窗口 UI；它们通过 `WindowId` 路由。

---

## 3. ID 与所有权

### 3.1 ID

```text
WindowId(u64)   primary 内单调递增，不复用
PaneId(u64)     全进程唯一（沿用现有 u64 命名空间）
TabId           窗口内唯一即可；跨窗口移动时由 WindowRegistry 重分配
```

PaneId 全局唯一很重要：

```text
plugin terminal.read/write
MCP live_* 
session
事件路由
```

都不能靠“当前窗口”猜。

### 3.2 线程所有权

```text
UI thread:
  WindowRegistry
  WindowRuntime
  Shared services
  Slint callbacks

per-pane actor threads:
  PTY / vte / frame

plugin supervisor threads:
  JSON-RPC / lifecycle

MCP socket threads:
  只做 JSONL 编解码，所有 AppState 操作 invoke 回 UI thread
```

禁止：

```text
全局 Rc<RefCell<AppState>>
跨窗口共享一个可变 Workspace
在 socket 线程直接访问 UI 状态
```

替换为：

```text
with_window(window_id, |&mut WindowRuntime|)
with_shared(|&mut SharedServices|)
```

回调闭包捕获 `WindowId`；PaneId 仍然全局唯一。

---

## 4. 单实例与窗口生命周期

### 4.1 启动

```text
tervo
  ├─ 尝试 flock($XDG_RUNTIME_DIR/tervo/instance.lock)
  ├─ 成功 → primary
  │    ├─ 启动 UI/共享服务
  │    ├─ 创建 WindowId(1)
  │    └─ 监听 control.sock（0600）
  └─ 失败 → secondary
       ├─ 连接 control.sock
       ├─ 发送 open_window(profile?, cwd?)
       └─ 退出 0
```

没有 primary 时，secondary 连接失败可重试一次；仍失败则明确报错，不静默启动第二个主实例。

### 4.2 新窗口

```text
Ctrl+Shift+N
  → ControlServer/命令直接调用 WindowRegistry::create_window()
  → 新 WindowRuntime
  → 新 Slint MainWindow
```

`--window` 兼容：

```text
tervo --window
  → 如果 primary 存在，转发 open_window
  → 如果没有 primary，按普通 primary 启动（只开一个窗口）
```

不再 `current_exe().spawn(--window)`。

### 4.3 关闭

```text
关闭一个窗口
  → 停止该窗口所有 pane actor
  → 释放该窗口 GPU surface/target
  → 从 WindowRegistry 移除
  → 共享服务/插件/MCP 继续存活

关闭最后一个窗口
  → 保存 session（所有窗口）
  → 停止插件
  → 关闭 MCP socket
  → 清除 running marker
  → 退出 primary
```

### 4.4 异常退出

```text
primary 持有 session.running marker
任何窗口崩溃 / primary panic / SIGKILL
  → marker 残留
  → 下次 primary 启动检测异常退出
  → 恢复所有窗口的最近 autosave
```

子窗口不再单独写/清 marker，因为不再有子窗口进程。

---

## 5. Session v2

当前 session 只有一份窗口布局。目标：

```json
{
  "version": 2,
  "windows": [
    {
      "id": 1,
      "geometry": { "x": 80, "y": 40, "w": 1200, "h": 800 },
      "maximized": false,
      "active_tab": 0,
      "tabs": [
        {
          "title": "Terminal 1",
          "focused": 0,
          "pinned": false,
          "tree": { "kind": "leaf", "cwd": "/home/me", "profile_id": 3 }
        }
      ]
    }
  ]
}
```

规则：

```text
primary 每 2s autosave 发生变化的窗口集合
聚合文件一次原子写 + fsync
保留上一代 session.json.bak
关闭一个窗口也触发保存（不是退出时才保存）
恢复时先按 geometry 创建窗口，再恢复 tabs/splits
窗口顺序稳定 sorted by WindowId
```

兼容：

```text
v1（单窗口）读取 → 包装成 windows[0]
v2 写出；旧版本 Tervo 不承诺读 v2
```

不恢复：

```text
scrollback
屏幕内容
正在运行的进程
SSH 自动重连
```

---

## 6. 插件 v3 的窗口路由

插件仍然是**每插件一个进程**，不是每窗口一个进程。

### 6.1 协议增量

在现有 v3 JSON-RPC 上增加可选 `window_id`：

```json
{
  "method": "event.deliver",
  "params": {
    "event": "pane-focus",
    "window_id": 2,
    "pane_id": 17,
    "payload": {}
  }
}
```

所有需要窗口上下文的入口：

```text
command.invoke      + window_id
event.deliver       + window_id
terminal.read       + window_id
terminal.write      + window_id
surface.render      + window_id
ui.notify/status    + window_id（可缺省 = active window）
```

兼容策略：

```text
window_id 缺失 → 当前 active window（旧插件行为不变）
新插件应始终读取 context.window_id
v3 保持 host_abi=3，window_id 是 additive 字段
```

### 6.2 插件 UI

```text
插件 panel 文档 → (window_id, surface) 两级 key
同一插件可以在两个窗口显示不同 panel 数据
host 只渲染当前窗口收到的文档
```

状态栏/通知：

```text
显式 window_id → 只在该窗口显示
缺省 → active/last-focused window
```

### 6.3 权限

权限仍是插件级：

```text
terminal.read 允许
  ≠ 允许读所有窗口
  每次请求仍带 window_id + 用户确认/scope
```

第一版：

```text
权限组全局
terminal.read 只允许请求目标窗口的焦点 pane
不允许跨窗口批量读
```

后续再加 per-window scope。

---

## 7. MCP 的窗口路由

MCP 仍然是一个本地 stdio bridge + 一个 live socket。

### 7.1 tools

```text
live_list
  → { windows: [{window_id, tabs, panes}], active_window_id }

live_output / live_send_text / live_run / live_focus / ...
  → arguments 增加 window_id
  → 缺省 = active_window_id
```

新增：

```text
live_new_window
live_close_window
```

`live_close_window` 属于 `windows + destructive` 组，关闭窗口前必须显式确认。

### 7.2 安全

```text
一个 token
一个 socket
audit 记录 window_id + pane_id + action
不允许通过 MCP 绕过窗口边界读另一个窗口的 pane
```

MCP 仍是宿主内嵌功能，不是插件能力。

---

## 8. GPU / 渲染

单进程多窗口下：

```text
一个 WGPU device/queue
每个窗口一个 surface
每个 pane 一个离屏 target
glyph atlas 共享
theme 共享
```

关键约束：

```text
WindowId + PaneId 作为 target key
一个窗口 GPU 失败 → 该窗口 CPU fallback，不影响其它窗口
窗口 resize/DPR 变化只重建该窗口 surface/target
不得让一个窗口的 damage 复用另一个窗口的 image
```

需要技术 spike 验证：

```text
Slint 1.x 多 MainWindow 实例
+ 同一个 WGPU30 device/queue
+ 每个窗口独立 surface
```

如果 Slint 多窗口 + WGPU 多 surface 在目标版本不可用，退回 **primary + window broker**（见第 12 节），但不要把 broker 当作第一选择。

---

## 9. 配置、Vault、热键

```text
ConfigStore        一个 watcher，所有窗口读取同一份
VaultService       一次解锁，所有窗口共享；锁定时不泄漏明文
GlobalHotkey       只注册一次，dispatch 到 last-focused window
ThemeRegistry      一份
PluginHost         一份
```

窗口自己的“外观覆盖”仍属于 WindowRuntime/Profile，不写入全局 config。

配置并发问题自然消失：

```text
一个进程 = 一个 writer
```

---

## 10. 迁移路线

### Phase 0：先止血（1–2 天）

不改多窗口模型，先消除最危险的多主实例行为：

```text
单实例 flock
第二个主实例只提示“已有 Tervo，使用 New Window”
--window 继续可用但标记为 legacy
session marker 只由主实例管理
MCP socket 第二个实例不抢占，明确提示
```

### Phase 1：抽 WindowRuntime（不改变行为）

```text
从 AppState 拆出窗口拥有的字段：
  ws / panes / pane_models / renderer / geometry / focus / title
保留 SharedServices：
  cfg / registry / plugins / mcp / vault / hotkeys
单窗口仍可运行
```

### Phase 2：WindowRegistry + 第二个窗口（feature flag）

```text
WindowRegistry { HashMap<WindowId, WindowRuntime> }
所有 Slint callback 捕获 WindowId
with_app → with_window
第二个窗口通过 flag 创建
验证 Slint 多窗口 + WGPU 多 surface
```

### Phase 3：单实例 ControlServer

```text
Unix socket 0600 + token
tervo → open_window
--window → 兼容转发
移除 current_exe().spawn(--window)
```

### Phase 4：Session v2 + 插件/MCP 路由

```text
windows[] 快照
window_id 进入 plugin/MCP 协议
窗口关闭/恢复/多显示器
```

### Phase 5：产品化

```text
Tab 在窗口间移动
窗口关闭不影响其它窗口
kill -9 恢复全部窗口
30m/24h + 多窗口 soak
```

每个阶段独立可回滚；Phase 2 的 flag 未通过就停在 Phase 1/0。

---

## 11. 验收标准

```text
[ ] 两个窗口各自 tabs/panes/focus，互不串台
[ ] 关闭窗口 A，窗口 B 的 PTY/插件/MCP 不受影响
[ ] 关闭最后一个窗口才退出 primary
[ ] Ctrl+Shift+N 在 <200ms 内开新窗口，不 spawn 新进程
[ ] 第二次 `tervo` 不创建第二套插件/ session，只请求 open_window
[ ] 一个插件进程只运行一份
[ ] 插件事件带正确 window_id
[ ] terminal.read 只读目标窗口
[ ] MCP live_list 返回所有窗口
[ ] MCP live_run 指定 window_id 只操作目标
[ ] 关闭窗口/异常退出后 session v2 恢复两个窗口几何和布局
[ ] kill -9 primary 后重启恢复所有窗口
[ ] 一个窗口 GPU 失败只降级该窗口
[ ] 全局热键 dispatch 到 last-focused window
[ ] 2 窗口 30m soak：threads/fds 稳定、插件不重复、MCP 不断
```

---

## 12. 备选方案：primary + window broker

只有在以下硬需求成立时才选：

```text
一个窗口崩溃绝不能影响其它窗口
窗口必须由不同 OS 用户/安全域运行
需要把窗口进程放进不同 cgroup/沙箱
```

架构：

```text
primary service process
  PluginHost / McpHost / Session / Vault / Config
  WindowRegistry（元数据）
  Broker socket
window processes
  Slint window
  Workspace/PTY/GPU
  broker client
```

需要额外实现：

```text
实例选举/租约
IPC 协议与版本
跨进程事件路由
插件 UI 路由
状态文件锁
窗口生命周期/崩溃检测
Tab 跨窗口迁移
```

结论：

```text
broker 是“强隔离”方案，不是默认产品方案。
单进程多窗口是更短、更一致的路径。
```

---

## 13. 风险与决策

| 风险 | 处理 |
|---|---|
| Slint 多 MainWindow 实例/多 WGPU surface 不成熟 | Phase 2 spike；失败则退回 broker |
| UI 线程一个 panic 影响所有窗口 | callback 边界 `catch_unwind`；per-window 关闭；crash autosave |
| 单进程内存上涨 | 共享 atlas/插件；每窗口 target 有界；30m/24h soak |
| 大重构引入终端回归 | 分层迁移；每阶段保留单窗口路径和 flag |
| 插件 window_id 兼容 | additive；缺省 active window |
| 多窗口 session 复杂度 | v2 聚合；每窗口 geometry/tabs；上一代备份 |
| 多显示器/DPR | 每窗口 geometry/DPR；Phase 5 真机矩阵 |

需要产品确认：

```text
1. 默认单实例：第二次 tervo 打开新窗口，还是允许独立实例？推荐前者。
2. 关闭最后一个窗口是否退出进程？推荐是。
3. Tab 跨窗口移动是否首发？推荐 Phase 5。
4. 是否接受“一个窗口 panic 可能影响同进程其它窗口”？推荐用 crash recovery 缓解；
   若不能接受，走 broker。
```

---

## 14. 非目标

```text
不把窗口做成远程会话
不实现跨机器窗口
不把插件变成每窗口一份
不让 MCP 绕过窗口边界
不在第一阶段做 Tab 跨窗口拖拽
不为了多窗口牺牲单窗口终端正确性
```

---

## 15. 当前代码入口

```text
crates/app/src/window.rs       当前 --window 进程 MVP
crates/app/src/main.rs         APP / AppState / with_app
crates/app/src/session.rs      当前单窗口 session
crates/app/src/plugin_v3/      插件宿主
crates/app/src/mcp.rs          MCP 配置/socket
crates/app/src/gpu_native.rs   每 pane target（未来加 WindowId）
crates/app/src/main.slint      MainWindow / 菜单 / 窗口回调
```

实现时先写 Phase 2 的 **双窗口同进程 spike**，不要先改插件/MCP 协议。  
如果 spike 不能稳定跑两个 WGPU surface，再评估 broker；否则按本文路线推进。

---

## 16. Phase 1/2 实现补充（2026-11-13）

已实现：

```text
WindowId / WindowRuntime / WindowRegistry / SharedServices
单窗口路径进入 registry，行为不变
TERVO_MULTI_WINDOW_INPROC=1 同 event loop 第二 MainWindow
两个窗口独立 tabs/panes/focus/models；PaneId 全进程唯一
关闭 B 只销毁 B；A 继续渲染
Config/Theme/PluginHost/MCP/Session/Vault 共享（插件一份、MCP 一个 socket）
gpu_present/gpu_native 按 WindowId 引用计数；关 B 后 A 仍 GPU ready
MCP live_list 返回 windows[]；new_window/close_window 支持 window_id
插件事件 payload additive window_id
```

仍在后续阶段：

```text
session v2 多窗口保存/恢复
第二窗口的完整设置页/插件面板回调
插件 command.invoke / terminal.read 的 per-window 参数
ControlServer 单实例转发
```

详见 `docs/MULTI_WINDOW_RUNTIME.md`。
