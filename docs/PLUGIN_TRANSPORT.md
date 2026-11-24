# 传输插件（Transport Plugin）设计

状态：**进行中（2026-11-17）**。POC 第一步已落地：`crates/core/src/transport.rs`
的 `PaneTransport` trait + `SocketPairTransport` 与读写分离的 `SplitTransport`
（3 条单测），**尚未接线**；`SplitTransport` 是为 PTY（读/写两条线程各持句柄）与
插件 socket 共用同一条 trait 铺的路；
PTY 读循环也已抽成 `pump_reader<R: Read>`（单测覆盖字节转发 + `Ok(0)` 收尾），
这就是将来喂插件 transport 的那个函数；下一步是宿主侧权限/审计/限流，再把
`PaneHandle` 的读端换成 `PaneTransport`。
目标：把 SSH / Serial / Telnet 做成**官方、可卸载**的插件，核心只保留一条通用传输 seam。

## 1. 为什么现在还不能“直接做成插件”

Plugin API v3 给插件的能力边界（`docs/PLUGIN_V3.md`）：

```text
terminal.read    → 读当前 pane 的上下文文本（每次 ask）
terminal.write   → 往 pane 写 ≤4096 字节文本（每次 ask）
ui.notify/status/render、声明式 panel/settings、credentials=own、事件
```

**没有**“由插件提供一个 pane 的字节后端”的能力：插件进程无法把字节流灌进 `Term`。
而 Profile 的远程/串口类型现在是**进程级桥接**：

```text
ssh    → Command::new("ssh")      （crates/app/src/profile.rs:354）
telnet → Command::new("telnet")   （:359）
serial → Command::new("socat")    （:369）
```

也就是说后端是“PTY 里跑一个外部客户端”，换后端只能在 core 里做。
→ 结论：要做成插件，核心必须先加一条**薄 seam**。

## 2. seam 设计（核心一次性改动）

```text
core:
  trait PaneTransport {
      fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
      fn write(&mut self, data: &[u8]) -> io::Result<usize>;
      fn resize(&mut self, cols: u16, rows: u16);
      fn close(&mut self);
  }
  Pane 现有路径 = PtyTransport（默认，行为零变化）
  外部后端     = PluginTransport（宿主代管 socket + 监督插件进程）
```

- **控制面**：复用现有 JSON-RPC。新增 `transport.open`（带 profile 参数）→ 宿主建
  socketpair 并传 fd（`SCM_RIGHTS`），`transport.resize`、`transport.close`；
  事件 `transport.closed` / `transport.error`。
- **数据面**：独立 `SOCK_SEQPACKET`（长度前缀），**不走 JSON-RPC 行协议**——
  行协议是 256 KiB/行 + 文本 JSON，扛不住终端突发流量。
- **权限**：新增 `permissions.terminal_transport`。首次启用确认；每次 `transport.open`
  写审计；单 pane 读/写缓冲各 ≤ 256 KiB，超限断开而不是丢字节。
- **生命周期**：沿用 v3 supervisor（握手/命令/ping 超时、崩溃退避重启、重启上限）。
  插件退出 → 该 transport 的 pane 明确报“连接后端已退出”，可重连，不卡 UI。
- **安全边界**：transport 只能读写自己打开的那个 pane；不能跨 pane；日志脱敏。

## 3. 官方插件划分

| 插件 | 后端 | 复用 v3 能力 |
|---|---|---|
| `xtervo-plugin-ssh` | russh（纯 Rust）；可留 system-ssh 回退模式 | 声明式面板做 Connection Manager；settings 做 Host Key/agent/X11/算法；`credentials=own` 存密码；事件做连接状态 |
| `xtervo-plugin-serial` | `serialport` | 设备枚举列表 + 参数编辑面板 + 拔插事件 |
| `xtervo-plugin-telnet`（可并入 ssh 包） | TCP + IAC 协商 | 同一 transport，协商在插件内完成 |

卸载即“没有 SSH/串口”：核心二进制不链接 russh / serialport，也不走相关启动路径。

## 4. 验收门槛（先定数字，再写实现）

```text
吞吐  `dd` 50 MiB 打到 pane，本机 ≥ 20 MiB/s（socket 数据面）
延迟  按键回显 p95 ≤ 5 ms，且相对 PTY 路径劣化 ≤ 2 ms
内存  单 transport 有界缓冲（读/写各 ≤ 256 KiB），30m soak 无增长
崩溃  kill -9 插件 → pane 报错可重连、UI 不卡、无僵尸进程
卸载  禁用插件后既有 pane 的处理策略必须写死（只读保留 或 友好关闭）
```

## 5. 迁移计划

```text
POC（S）   core seam + 测试插件把 socketpair 当 transport，跑通 pane 字节流与现有 e2e
收口（M）  权限/审计/限流、transport.* 协议、错误态、单元与集成测试
ssh（L）   先 system-ssh 薄模式跑通 UX，再换 russh；Connection Manager 面板
serial（M）+ telnet（S）
```

## 6. 对内部行动清单（`docs/internal/ACTION_ITEMS.md`）的影响

- `ACT-039/042` → 改为“官方 ssh 插件”（核心只负责 seam 与权限）。
- `ACT-043/044/045` → 改为“官方 serial / telnet 插件”。
- `ACT-041`（Connection Manager）→ 由插件声明面板承担，核心只提供面板容器。
- 新增 `ACT-052`：Transport seam（POC + 权限/审计/限流）。
