# 今晚开工单（功能优先，测试后置）

用户决定：**先把功能堆叠完成，测试/门禁后补**。本文件是 18:00（或即刻开工）后的执行顺序。

## 0. 执行顺序（2026-11-17 用户确认：核心功能做完再做其它）

```text
阶段 1  核心终端冲刺     69.2% → ≥80%   ← 现在只做这个（docs/CORE_TO_80.md 的 17 条）
阶段 2  连接插件         transport seam → SSH → Serial → Telnet（F1–F7）
阶段 3  壳与收尾         Connection Manager / Toolbar / safe mode / 截图 / 工作区补完
```

**硬门禁：核心完成度 < 80% 之前，不启动阶段 2/3 的任何代码。**
唯一例外：阶段 1 里已经排进去的 UI 类小件（Tab 重命名撤销、最近 Tab 栈等）不涉及连接层。

排班与逐条做法见 **`docs/CORE_TO_80.md`**：今晚 Wave 1（快赢）→ Wave 2（观感）→ Wave 3（结构），
目标 75%，加分 77%；`emoji grapheme / pane 拖拽 / 录制 API / 插件命令上下文` 不在今晚。

## 0.1 规则调整（今晚有效）

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
| F11 | **截图功能**（见 §6） | 新 `crates/app/src/screenshot.rs` | 45m |
| F12 | **工作区补完**（见 §7，用户确认有需求） | `workspaces.rs` + 设置页 + 启动 | 2–3h |

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
- `~/.config/xtervo/` 下**没有 profiles.toml**（还没有保存过任何 Profile）；
- `~/.ssh/config` 里只有一个 Host：`ssh.example.test`；
- `SSH_AUTH_SOCK=<agent socket>`（agent 可用），`~/.ssh` 有 ed25519/rsa 公钥；
- known_hosts 已配置。

F5 联调就用 `ssh.example.test`（或你临时指定的主机）建 Profile：
F9 步骤前不要动 UI，先用 `ssh` 命令行确认 `BatchMode=yes` 能通，再让插件走同一条路径。

## 3. 中断保护

- 每个 commit 前必须 `cargo check -p xtervo-app -p xtervo-core` 通过；
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

## 6. F11 截图功能（已确认列入计划，待做）

用户需求：给 xtervo 加“截图”能力。已核对依赖，**不需要新增 crate**：

```text
png     0.18   （编码 PNG）
arboard 3      （写系统剪贴板，可选）
x11rb   0.14   （features: xtest, damage, shape, randr —— 可 get_image 抓窗口）
```

### 两种范围

| 方案 | 内容 | 平台 | 工作量 |
|---|---|---|---|
| A 整窗截图 | 含标签栏/侧栏/状态区，所见即所得 | X11 可直接做；Wayland 需 portal（重） | 45–60m |
| B 终端内容 | 只截当前 pane 的文字/图片区域 | 全平台（复用 renderer buffer） | 20m |

**建议**：先做 A，Wayland 下自动退化为 B 并提示“Wayland 暂不支持整窗截图”。

### A 的实现要点

```text
1. 取窗口：slint 的 with_winit_window → raw window handle（X11: XID + 连接）
2. 坐标：x11rb translate_coordinates 到 root，处理多屏（本例 x=0 与 x=2560）
3. 抓像素：get_image(ZPixmap, x, y, w, h, 0xffffffff)；注意
   - depth 24/32 与 LSBFirst（踩过：byte_order 读反整屏发蓝）
   - BGRX → RGBA 重排
4. 编码：png crate 写 $XDG_PICTURES_DIR/xtervo-YYYYmmdd-HHMMSS.png
   （无 XDG_PICTURES_DIR → ~/Pictures → 退到 XDG_STATE_HOME/screenshots）
5. 可选：arboard 写入剪贴板（配置项 screenshot_copy，默认 false）
6. 反馈：状态区 pill 显示保存路径；失败时明确原因（未映射/不可见/Wayland）
```

### 命令与快捷键

```text
命令     view.screenshot   （Category::View，命令面板可搜）
快捷键   默认不绑；候选 alt+shift+s（需确认不和终端应用冲突）
```

### 测试（后补，含在欠账里）

```text
[ ] 单测：BGRX→RGBA 重排、路径命名/去重、PNG 编解码往返
[ ] e2e（Xvfb）：抓窗口 → 文件存在 → 尺寸等于窗口 → 抽样像素与截图对比
[ ] 多屏：窗口在第二屏（x=2560）时坐标正确
[ ] Wayland：退化为内容截图且提示明确
```

### 风险

- **遮挡/未映射**：X11 抓的是窗口 drawable，被挡住时可能拿到陈旧内容；截图前先 `map`/raise 或提示。
- **HiDPI**：窗口物理像素 vs Slint 逻辑像素要乘 scale_factor，否则截出来一半。
- **安全**：截图会包含屏幕内容，写入前不弹确认，但路径固定在 Pictures 且日志只记路径不记内容。

## 7. F12 工作区补完（用户确认按 A 方案做）

现状（v1，已实现）：`workspaces.rs` 数据层（命名布局复用 `session::Tree`、独立
`workspaces.toml`、原子写、容错、4 条单测）；命令 `workspace.save`（自动取 Tab 标题命名）、
`workspace.open`（恢复成新 Tab）、命令面板里的 `Workspace: <name>` 选择器；
e2e：预置 two-up → 面板选中 → 1 tab/1 pane 变 2 tabs/3 panes。

不足：没有命名弹窗、没有管理 UI、只存单个 Tab、不能启动自动打开。

### 拆解

| 步骤 | 内容 | 估时 |
|---|---|---|
| F12.1 | 命名弹窗：保存时预填 Tab 标题、可编辑、重名提示（复用现有表单弹窗样式） | 40m |
| F12.2 | 设置页「工作区」列表：名字 / pane 数 / 创建时间 / 打开 / 重命名 / 删除 | 60m |
| F12.3 | 启动恢复：config `workspace_on_start`（空 = 不恢复），启动时打开对应工作区 | 30m |
| F12.4 | （可选）整窗多 Tab 快照：`WorkspaceStore` 升 version 2，存 Tab 列表而非单树 | 60m |

### 验收

```text
[ ] 保存时可输入名字；重名给出提示并允许覆盖/改名
[ ] 设置页列出工作区，能打开/重命名/删除，操作后 workspaces.toml 立即更新
[ ] config.workspace_on_start 设置后，重启自动打开且失败时回退普通启动并报错
[ ] 删除当前正在使用的工作区不崩溃（只删记录，不动已开的 Tab）
[ ] e2e：保存 → 设置页可见 → 重命名 → 删除 → 启动自动打开（Xvfb + XTEST）
```

### 为什么值得做

- 分屏布局每次重开是真实痛点，v1 已经证明了恢复路径可靠（复用 session 恢复）；
- 它和"连接插件"天然互补：一个工作区 = 一组 SSH/本地 pane 的布局，连上连接管理后价值更高；
- 成本低（数据层已在，缺的是 UI 入口与两个字段）。

### 优先级建议

如果按"终端本体优先"（连接插件不算核心），F12 可以插到 F5 之前做；
如果按"连接差异化优先"，保持 F1–F7 在前、F12 放夜里后段。
