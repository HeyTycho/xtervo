# 今晚开工单（功能优先，测试后置）

用户决定：**先把功能堆叠完成，测试/门禁后补**。本文件是 18:00（或即刻开工）后的执行顺序。

## 0. 规则调整（今晚有效）

```text
保留：cargo check 必须过（不能留编译不过的树）、每个功能一个 commit
跳过：新增单测、e2e、2m/15m soak —— 全部记到 §5 的欠账清单
底线：不删既有测试；已有测试必须仍然全绿（回归保护）
```

## 1. 功能堆叠顺序（每项 1 个 commit，做完即提交）

| # | 功能 | 落点 | 预估 |
|---|---|---|---|
| F1 | `Backend::{Pty,Plugin}` + `spawn_with_transport` | `crates/core/src/pane.rs` | 40m |
| F2 | `TransportReader` 适配 + 写端分支（Plugin 走 `transport.write_all`） | 同上 | 30m |
| F3 | 数据面：`SOCK_SEQPACKET` 长度前缀收发（`[u32 len][payload]`） | 新 `crates/app/src/transport_io.rs` | 60m |
| F4 | `terminal_transport` 权限：首次确认 + 审计日志 + 单 pane 256KiB 有界 | `permissions`/plugin host | 45m |
| F5 | SSH 插件骨架：manifest + 启动 + `ssh` 子进程桥接（密码/agent） | `plugins/ssh/` | 90m |
| F6 | Serial 插件（`/dev/tty*` + baud） | `plugins/serial/` | 45m |
| F7 | Telnet 插件（host/port + 简单 IAC 过滤） | `plugins/telnet/` | 45m |
| F8 | Connection Manager：已存连接列表 + 连接/断开/重连 | 侧栏或面板 | 90m |
| F9 | 顶部 Toolbar（先把 IME 坐标链里的 `TABBAR_H` 换成运行时常量） | `main.slint` + `main.rs` | 90m |
| F10 | safe mode 收尾：GPU/WASM 显式降级 + 熔断动作 | `main.rs` | 45m |

合计约 10 小时；F1–F7 是主线，F8–F10 视剩余时间。

## 2. 每项完成判据（不跑测试也能验的）

```text
F1/F2   cargo check 过 + 现有 core 144 单测仍全绿
F3      手写小工具：插件侧发 1MiB，宿主 panic-free 收齐（日志计数）
F4      权限拒绝时日志明确、pane 不创建
F5–F7   真实连一次（本机 sshd / socat 虚拟串口 / telnet 服务）
F8      UI 点一遍，截图
F9      截图 + IME 候选位置 sanity（若无输入法可跳过）
F10     伪造 crash 计数，看日志与 pill
```

## 2.5 本机测试台（已实测，省得半夜找环境）

```bash
sshd        MISSING（本机没有 openssh-server）→ SSH 插件联调三选一：
            a) 用你现有的远端（profiles.toml 里的 staging 之类）
            b) sudo apt install openssh-server 后连 127.0.0.1
            c) 只验证错误路径（ssh 连不存在的端口 → 插件应上报 transport.error）
ssh client  /usr/bin/ssh ✓
socat       /usr/bin/socat ✓  → 串口插件测试用虚拟串口对：
            socat -d -d pty,raw,echo=0 pty,raw,echo=0
            （实测输出 /dev/pts/0 <-> /dev/pts/1）
真实串口    /dev/ttyUSB*、/dev/ttyACM* 都不存在 → 只能靠 socat PTY 对
telnetd     system 版 MISSING，但 busybox 有 telnetd applet ✓ →
            busybox telnetd -l /bin/sh -p 2323
nc          ✓（可以当最简 TCP 回显：nc -lk 127.0.0.1 2324）
```

结论：**Serial 用 socat PTY 对、Telnet 用 busybox telnetd/nc 都能本地闭环；
SSH 用**远端机器**（用户已定）。

本机现状（已核对）：
- `~/.config/tervo/` 下**没有 profiles.toml**（还没有保存过任何 Profile）；
- `~/.ssh/config` 里只有一个 Host：`ssh.example.test`；
- `SSH_AUTH_SOCK=<agent socket>`（agent 可用），`~/.ssh` 有 ed25519/rsa 公钥；
- known_hosts 已配置。

F5 联调就用 `ssh.example.test`（或你临时指定的主机）建 Profile：
F9 步骤前不要动 UI，先用 `ssh` 命令行确认 `BatchMode=yes` 能通，再让插件走同一条路径。

## 3. 中断保护

- 每个 commit 前必须 `cargo check -p tervo-app -p tervo-core` 通过；
- 若某个功能做不完：**回退未提交改动**（`git checkout -- .`），保证树是绿的再收工；
- 每完成一项，在本文件表格里打勾并写 commit 号。

## 4. 18:00 开场指令（复制即用）

```text
读 docs/EVENING_START.md 与 docs/TRANSPORT_WIRING.md，
按 F1 开始堆功能；跳过新增测试与 soak，但 cargo check 与既有测试必须绿。
每完成一项就 commit 并在 EVENING_START 表格里打勾。
```

## 5. 欠账清单（测试后补）

```text
[ ] F1/F2  transport 后端的 vte 渲染测试（红字 + resize + EOF）
[ ] F3     数据面吞吐/背压测试
[ ] F4     权限拒绝用例
[ ] F5-F7  插件 e2e（sshd / socat / telnetd）
[ ] F8-F10 回归 e2e + 截图基线
[ ] 全量：cargo test、2m soak ×N、15m soak、docs 对账
```
