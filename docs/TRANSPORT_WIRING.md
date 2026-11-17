# PaneHandle 接线计划（ACT-052 第四步，18:00 起执行）

状态：**计划（2026-11-17 15:30）**。前三步已落地：`PaneTransport` trait、
`SocketPairTransport`、`SplitTransport`、`pump_reader`（4 条单测）。
本文件只描述接下来怎么把 transport 真正接进 pane，顺序按“风险从低到高”。

## 0. 约束

- **PTY 路径行为必须逐字不变**：默认后端仍是 PTY，所有现有 e2e/soak 必须照跑。
- 每步一个小 commit，编译 + 单测绿才继续；任何一步红了就 `git checkout -- .` 回退。
- 触及终端核心 → 每步完成后跑 `cargo test -p tervo-core` + 2m soak。

## 1. 引入后端枚举与 spawn_with_transport（core）

```rust
// crates/core/src/pane.rs
enum Backend {
    Pty,                              // 默认：openpty + spawn_command
    Plugin(Box<dyn PaneTransport>),   // 插件提供字节通道（测试先行）
}

pub fn spawn(config: PaneConfig) -> io::Result<PaneHandle> {
    spawn_with_transport(config, Backend::Pty)   // 现有实现整体搬进去
}

pub fn spawn_with_transport(config: PaneConfig, backend: Backend) -> io::Result<PaneHandle>
```

改动点：
- `openpty` / `CommandBuilder` / `slave.spawn_command` 只在 `Backend::Pty` 分支执行；
  `Backend::Plugin` 跳过，`child = None`。
- 读端：`pump_reader(reader, pty_tx, ctrl_tx)` 统一；`reader` 来自
  `Pty(master.try_clone_reader())` 或 `Plugin(transport)`（`PaneTransport: Read` 需要
  一个薄适配 `TransportReader(Box<dyn PaneTransport>)`，或让 pump_reader 直接吃
  `&mut dyn PaneTransport`——二选一，倾向后者，少一层包装）。
- 写端：`writer_queue` 消费循环里，`Pty` 用 `write_all`，`Plugin` 用
  `transport.write_all`。
- 退出：`Pty` 保持现有 child wait / 退出码语义；`Plugin` 在通道 EOF（`Ok(0)`）
  时发 `Ctrl::PtyClosed`，退出码记为“后端关闭”。

## 2. 测试（先于产品接线）

```rust
#[test]
fn plugin_backend_feeds_vte_and_reports_resize() {
    let (pane_side, plugin_side) = SocketPairTransport::pair().unwrap();
    let handle = spawn_with_transport(cfg, Backend::Plugin(Box::new(pane_side))).unwrap();
    plugin_side.write_all(b"\x1b[31mred\x1b[0m\r\n").unwrap();
    // 轮询 pull 直到出现 'r' 或超时
    handle.resize(120, 40);
    // 断言：frame 里有 "red"、颜色是红；transport 记录了 resize；drop 后 pull 收到 PtyClosed
}
```

这条测试是 ACT-052 的**验收核心**：证明“没有 PTY 的 pane 也能跑通 vte → Frame”。

## 3. App 侧（只加参数，不切产品路径）

```rust
// main.rs create_pane(...)：新增可选 transport 参数，None = PTY（现状）
fn create_pane(..., transport: Option<Box<dyn PaneTransport>>) -> ...
```
所有现有调用点传 `None`；`split/new_tab/profile` 行为不变。真正的插件 pane 入口
（`profile.kind == "plugin-ssh"` 之类）留到 SSH 插件落地时再接。

## 4. 插件数据面协议（Socket 侧）

```text
宿主 <-> 插件进程：AF_UNIX SOCK_SEQPACKET，单帧 = [u32 len][payload]
控制面（open/resize/close/错误）走现有 JSON-RPC；数据面不解析 JSON。
单 pane 读写各 ≤256 KiB 有界；超限断开并报 transport.error，绝不丢字节。
```

权限层（`terminal_transport`）在 App 侧落地：首次启用确认 + 每次 open 审计 +
限流。这一步在 SSH 插件之前完成。

## 5. 验收顺序

```text
1. cargo test -p tervo-core（含新测试）           ← 每步都跑
2. app 578 单测                                   ← 每步都跑
3. DURATION=2m soak                               ← 接线后跑
4. scripts/e2e_split_repaint.py                   ← 确认 split 仍正常
5. 吞吐 bench（PTY 路径不得回退，基线 56.1 MiB/s）
```

## 6. 回退

- 每步独立 commit，`git reset --hard <上一步 commit>` 即可。
- `Backend::Plugin` 只被测试使用，产品代码在 SSH 插件落地前不会走它；
  若接线出问题，删掉 `spawn_with_transport` 的 Plugin 分支即可回到现状。
