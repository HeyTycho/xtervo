# SSH / Serial 对齐清单（2026-11-20）

状态：`[x]` 完成，`[~]` 部分完成，`[ ]` 未完成。
顺序即执行顺序；每项都要求代码 + 测试/证据，再进入下一项。

## Serial

- [~] S01 端口选择：设置页显示枚举列表 + Refresh；缺下拉选择/记住设备。
- [x] S02 串口参数：baud、5–8 数据位、parity、1/2 停止位、none/software/hardware 流控。
- [x] S03 打开/关闭/读写：native `serialport`，PTY 双向与 timeout 重试测试。
- [ ] S04 运行时 DTR/RTS：连接中切换，而非只在打开时应用。
- [ ] S05 控制线状态：CTS / DSR / DCD 实时显示。
- [ ] S06 Send Break：按驱动能力发送 break。
- [~] S07 拔插/重连：表单 1.5s 刷新；缺设备消失检测与自动重连。
- [ ] S08 输入模式：Text / Readline / Byte / Hex。
- [ ] S09 输出模式：Text / Hexdump。
- [ ] S10 换行转换：CR / LF / CRLF / None。
- [~] S11 高频缓冲：现有 reader/writer 背压；缺高频长跑断言。
- [ ] S12 时间戳 / Zmodem：先核实上游能力，再决定是否实现。

## SSH

- [x] H01 建连/认证/PTY/shell：native russh；密码、未加密私钥、agent、Vault 口令。
- [x] H02 Profile/认证 UI：Host/Port/User/Auth/Key/Password/Vault。
- [~] H03 known_hosts：纯 Rust 校验/首次确认；缺条目查看/删除管理 UI。
- [ ] H04 keepalive：`ServerAliveInterval` / `ServerAliveCountMax` 映射到 russh。
- [~] H05 IdentityFile/默认密钥：ssh_config 有 IdentityFile 时回退系统 ssh；缺 native 多密钥尝试与 `~/.ssh/id_*` 发现。
- [~] H06 passphrase：表单+Vault；缺口令错误/缺失时的连接内重试 UI。
- [x] H07 ProxyJump：native 多跳；缺链路可视化/跳板独立凭据。
- [x] H08 转发：native L/R/D + SOCKS5；缺 port 0、转发管理面板/状态。
- [~] H09 agent forwarding：native 请求 + 成功门禁；缺拒绝场景回归与状态 UI。
- [~] H10 SFTP 导航：现有系统 sftp 面板；缺上级/刷新/属性列。
- [~] H11 SFTP 操作：上传/下载/目录/改名/删除/chmod；缺覆盖确认、断点续传、递归删除、特殊字符转义。
- [~] H12 传输进度：进度/取消/失败/Retry；缺总大小/ETA/自动重试。
- [ ] H13 Login Scripts：命令/顺序/延时/等待提示符/错误处理。
- [ ] H14 X11 Forwarding 与状态显示。
- [ ] H15 ProxyCommand：安全策略与实现；当前回退系统 ssh。
- [~] H16 ssh_config：native 解析 Host/HostName/Port/User/ProxyJump/known_hosts；缺 Match、IdentityFile、导入 UI。
- [~] H17 Connection Manager/历史：仅 Profiles；缺常驻入口、连接中/失败/取消三态、最近连接。
- [ ] H18 Persistent Port Forwards：规则随连接恢复。
- [ ] H19 算法配置入口：Ciphers/MAC/KEX；默认遵循库安全值。
- [~] H20 SSH 证书/`@cert-authority` 信任链：russh/ssh-key 原语可用，但 Tervo 的 known_hosts/预检/校验未接入；native 明确拒绝证书、回退系统 ssh。调研与安全实现清单见 [`SSH_CERTIFICATES.md`](SSH_CERTIFICATES.md)。

## 横切

- [ ] T01 每项补失败测试或复现证据。
- [ ] T02 对应 e2e（无真实环境时用 in-process server/PTY）。
- [ ] T03 阶段末 app/core 全测 + 2m/15m `SOAK_STRICT`。
- [ ] T04 阶段末 Codex 复审，P1/P2 清零或记录接受项。
- [ ] T05 最终只安装一次 release，并输出 hash/验证命令。

## 执行顺序

1. S04 → S05 → S06（串口运行时控制）
2. S01 → S07（端口选择/拔插重连）
3. H04 → H05 → H06（keepalive/密钥/口令）
4. H08 port 0 → H09 状态/回归
5. S08/S09/S10 → H10–H12
6. H13 → H14 → H15/H16 → H17–H20
7. T01–T05

## 完整 Tabby 条目对照

### UI / SSH

- [~] UI-SSH-001 Profile 编辑/认证提示：字段齐全；缺 General/Ports/Advanced/Ciphers/Login Scripts 分组。
- [x] UI-SSH-002 主机密钥预检/首次确认/变更告警：应用内 Trust/Update 已通；缺 known_hosts 管理 UI。
- [~] UI-SSH-003 SFTP 面板导航：列表/路径/取消；缺上级/刷新/权限/mtime/大小列。
- [~] UI-SSH-004 SFTP 文件操作：上传/下载/建目录/改名/删除/chmod/队列；缺覆盖确认/递归删除/断点续传。
- [~] UI-SSH-005 传输进度：进度/Cancel/Retry；缺总大小/ETA/自动重试。
- [~] UI-SSH-006 Port Forward 管理：表单可配 L/R/D/Jump；缺查看/启停/监听状态。
- [~] UI-SSH-007 Jump Host：可配置/多跳；缺链路可视化、跳板独立凭据。
- [ ] UI-SSH-008 Login Scripts：命令/顺序/延时/等待/失败处理。
- [~] UI-SSH-009 Agent/X11 状态：agent 可配置；缺 X11、状态/失败提示。

### SSH 后端

- [~] SSH-001 建连/断开/超时/keepalive/重连：native 已通；缺 keepalive 配置项、真实外部 sshd e2e。
- [~] SSH-002 Connection Manager：Profiles 面板可用；缺常驻入口/连接中/失败/取消三态。
- [x] SSH-003 用户名/主机/端口/认证方式：Profile + native。
- [x] SSH-004 密码/私钥/加密私钥口令/Agent：native + Vault；加密私钥口令重试弹窗（保存 Vault/取消/本地校验）。
- [~] SSH-005 known_hosts：native 校验 + 应用内确认 + Settings 管理入口（查看/删除已应用条目）；证书信任链仍不支持。
- [~] SSH-006 ssh_config：native 解析 Host/HostName/Port/User/ProxyJump/known_hosts；Match/IdentityFile/ProxyCommand 回退。
- [x] SSH-007 Jump Hosts：native 多跳。
- [ ] SSH-008 ProxyCommand：native 未实现，回退系统 ssh。
- [x] SSH-009 Agent forwarding：native 请求 + 门禁 + UI 状态（on/denied/no local agent）+ 拒绝回归测试（测试 server 的 `agent_request` 已修）。
- [ ] SSH-010 X11 Forwarding：未实现。
- [x] SSH-011 L/R/D 转发：native 三种；缺 port 0 与管理 UI。
- [ ] SSH-012 Persistent Port Forwards：未实现。
- [ ] SSH-013 Login Scripts：未实现。
- [ ] SSH-014 Ciphers/算法配置：未实现。
- [ ] SSH-015 主机别名/历史/最近连接：未实现。
- [ ] SSH-016 WinSCP：平台不支持，保持 unsupported。

### SFTP

- [~] SFTP-001 会话：系统 sftp 子进程；非内建 SSH Channel。
- [~] SFTP-002 列表/导航/元数据：缺上级/刷新/权限/mtime/大小。
- [~] SFTP-003 上传/下载/目录/改名/删除：缺覆盖确认/递归/续传。
- [~] SFTP-004 进度/取消/重试：缺总大小/ETA/自动重试。
- [~] SFTP-005 大文件流式：系统 sftp 流式；无应用内通道/限速/续传。
- [~] SFTP-006 独立面板：面板可用；非独立 Tab/插件 API。
- [~] SFTP-007 权限/覆盖/路径编码/断线：chmod/删除确认；缺覆盖确认/特殊字符/续传。

### Serial

- [~] SERIAL-001 枚举/开关：native 打开/关闭；缺端口选择器。
- [x] SERIAL-002 多 Serial Profiles：已实现。
- [x] SERIAL-003 baud/data/parity/stop/flow：已实现。
- [ ] SERIAL-004 输入模式：Text/Readline/Byte/Hex。
- [ ] SERIAL-005 输出模式：Text/Hexdump。
- [ ] SERIAL-006 换行转换：CR/LF/CRLF/None。
- [ ] SERIAL-007 拔插/自动重连：只有刷新。
- [~] SERIAL-008 读写缓冲：后端背压；缺高频断言。
- [ ] SERIAL-009 Zmodem：先核实上游。
- [ ] SERIAL-010 时间戳：未实现。

## 今晚 Gate（必须完成）

- [ ] S01 端口选择器
- [ ] S04 运行时 DTR/RTS
- [ ] S05 CTS/DSR/DCD
- [ ] S06 Break
- [ ] S07 拔插检测 + 自动重连
- [ ] H04 keepalive
- [ ] H05 IdentityFile/默认密钥
- [ ] H06 passphrase 重试
- [ ] H08 远程转发 port 0
- [ ] H09 agent 拒绝回归 + 状态
- [ ] T01–T04 测试/e2e/2m+15m/Codex

Phase 2/3（SFTP 完整化、转发管理、Login Scripts、X11、ProxyCommand、证书、Connection Manager、历史、算法、Zmodem、硬件/真实 sshd）不在今晚 Gate 内，按清单继续。
