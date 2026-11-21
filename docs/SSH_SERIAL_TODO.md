# SSH / Serial 对齐清单（2026-11-21 对账版）

> 基线：`main` @ `f92c698`。  
> 本表以**代码 + 测试 + 可复核证据**为准，旧版 `e9685ed` 的勾选不再可信（旧内容见 git 历史）。  
> 状态：✅ done（主路径有代码且有直接测试/证据）；◐ partial（可用但有明确缺口或证据不足）；✗ missing（未实现/明确 unsupported）。

## 0. 结论

| 范围 | 总数 | ✅ | ◐ | ✗ |
|---|---:|---:|---:|---:|
| 核心 Serial S01–S12 | 12 | 4 | 8 | 0 |
| 核心 SSH H01–H20 | 20 | 12 | 8 | 0 |
| **核心合计** | **32** | **16** | **16** | **0** |
| UI-SSH-001..009 | 9 | 4 | 5 | 0 |
| SSH-001..016 | 16 | 8 | 7 | 1 |
| SFTP-001..007 | 7 | 1 | 6 | 0 |
| SERIAL-001..010 | 10 | 4 | 5 | 1 |
| 横切 T01–T05 | 5 | 0 | 4 | 1 |
| **全表合计** | **79** | **33** | **43** | **3** |

**一句话：没有做完。核心 S/H 32 项实际是 16 完成 / 16 部分，全表 79 项只有 33 项可判完成。**

审计限制：

- Serial 审计环境没有可用 `cargo`，测试结论来自源码 `#[test]` 记录，未复跑；真实串口硬件不存在。
- SSH 实测 `cargo test ... ssh`：62 passed / 1 failed；失败是用户 `~/.ssh/config` 的
  `StrictHostKeyChecking accept-new` 导致测试不隔离，不是产品崩溃。
- SFTP 审计未跑完编译；结论来自源码/测试名静态核对。
- 真实外部 OpenSSH sshd、真实串口硬件、Windows 场景均无证据。

---

## 1. Serial S01–S12

| ID | 状态 | 证据 | 仍未完成 / 缺口 |
|---|---|---|---|
| S01 端口选择/Refresh/热刷新 | ◐ | `serial_native.rs:273`；`main.rs:7499`；`main.slint:8311` | 无 fake 端口列表测试、无点击选择 e2e；无真实硬件 |
| S02 baud/数据位/parity/stop/flow | ◐ | `profile.rs:753`；`serial_native.rs:422`；`main.slint:8263` | 无 5/6/7 数据位、7E2、2 stop、flow 的 termios/行为测试；PTY 只用 115200/8N1 |
| S03 打开/关闭/读写 | ✅ | `serial_native.rs:417,493,543`；`native_serial_roundtrips_through_a_pty` 等 | 无真实硬件；`into_halves` pane 路径无 e2e |
| S04 运行时 DTR/RTS | ◐ | `serial_native.rs:351`；`main.rs:18583`；`main.slint:11246` | 只断言期望值，不断言物理电平；控制写失败被 `let _` 吞掉；socat 回退无控制面板 |
| S05 CTS/DSR/DCD | ◐ | `serial_native.rs:388`；`main.rs:7462`；`main.slint:11257` | 无测试；`unwrap_or(false)` 把读取失败混成低电平；无硬件；`CONNECTION_BACKENDS.md` 仍写未暴露 |
| S06 Send Break | ◐ | `serial_native.rs:371`；`main.rs:18603` | 只调用不断言；失败状态不自动清除；无硬件/回环 |
| S07 拔插/自动重连 | ◐ | `main.rs:7094,7203`；仅 eligible 纯函数测试 | **`auto_reconnect` 无 UI 开关**；重连计数在 `open_profile_tab_manual` 被清空，退避/上限失效；重开失败不再调度；旧失败 Tab 残留；无拔插 e2e/硬件 |
| S08 输入模式 | ◐ | `serial_native.rs:118`；`input_modes_are_raw_except_hex` 等 | Readline/Byte 实际等同 raw Text，无本地行编辑/逐字节语义；上游定义未核 |
| S09 输出 hexdump | ✅ | `serial_native.rs:251`；`newline_and_hexdump_helpers` | 只有短输入单测；socat 回退不经过 formatter |
| S10 换行转换 | ✅ | `serial_native.rs:76,646`；单测覆盖 | 仅出站；无 e2e；socat 回退不生效 |
| S11 高频缓冲 | ◐ | `pane.rs:72,717`；`serial_native.rs:19` | 无 Serial 高频/长跑测试；soak 无 Serial workload；无丢字节指标 |
| S12 时间戳 | ✅ | `serial_native.rs:150`；时间戳单测 | 无 transport 级测试；socat 回退无时间戳 |

---

## 2. SSH H01–H20

| ID | 状态 | 证据 | 仍未完成 / 缺口 |
|---|---|---|---|
| H01 建连/认证/PTY/shell | ✅ | `ssh_native.rs:306,616,1306`；3 个 native 认证测试 | agent 认证 `try_agent` 无专门测试；PTY resize 无断言；无真实 sshd |
| H02 Profile/认证 UI | ✅ | `profile.rs:553`；`main.slint:8199`；e2e 表单截图 | 无 General/Ports/Advanced 分组；无表单级自动化 |
| H03 known_hosts | ✅ | `known_hosts.rs:544,603`；Settings 20 例测试 | 删除仍依赖 `ssh-keygen -R`；无 CA 条目管理 |
| H04 keepalive | ✅ | `ssh_config.rs:350,132`；`ssh_native.rs:642` | 无 UI；无“静默 server 超时断开”运行测试 |
| H05 IdentityFile/默认密钥 | ✅ | `ssh_native.rs:1219`；config 测试 | 默认密钥回退无专门测试；不支持 DSA/sk key |
| H06 passphrase/Vault | ✅ | `ssh_native.rs:111,1347`；`main.rs:6482`；测试 | ssh_config 的加密 IdentityFile/默认密钥不会弹口令（只认 Profile key_path） |
| H07 ProxyJump | ◐ | `ssh_native.rs:653`；`native_ssh_proxy_jump_reaches_target` | 无链路可视化；跳板复用目标凭据，无独立 Vault |
| H08 L/R/D + port 0 | ◐ | `ssh_forward.rs:413,466`；local/remote 集成测试 | **`profile::validate_remote_forward` 拒绝端口 0，UI 无法保存 `-R 0:...`**；D 无集成测试；port 0 只有显示单测 |
| H09 agent forwarding | ✅ | `ssh_native.rs:785,566`；拒绝/成功测试；状态 UI | 文档仍写 `[~]` 已过期；`Unavailable` 无集成测试；`ssh_config ForwardAgent yes` 整条回退 |
| H10 SFTP 导航 | ✅ | `sftp.rs:338,369`；`main.rs:8834`；测试 | 无 GUI 自动断言；系统 sftp 子进程，非 native channel |
| H11 SFTP 操作 | ◐ | 覆盖确认/递归删除/特殊字符均已有测试 | **断点续传完全缺失** |
| H12 传输进度 | ◐ | `sftp.rs:1122,1781`；Retry 手动 | 无总大小；ETA 解析后未显示；无自动重试；真实 OpenSSH 非 tty 下进度可能不可用 |
| H13 Login Scripts | ✅ | `login_script.rs:58`；`ssh_native.rs:1107`；顺序测试 | 无交互式确认脚本；命令内分号需转义 |
| H14 X11 Forwarding | ✅ | `profile.rs:577`；native 明确回退系统 ssh | 无真实 X11 e2e；无 Display/xauth 配置；无状态提示 |
| H15 ProxyCommand | ✅ | `profile.rs:614`；native 回退 | 无端到端 |
| H16 ssh_config | ◐ | Host/HostName/User/ProxyJump/known_hosts/IdentityFile/keepalive 已解析 | **`Port` 解析后目标端口仍固定用 `profile.port`**；`ForwardAgent yes` 回退未文档化；Match unsupported |
| H17 历史/最近连接 | ◐ | `history.rs`；`main.rs:5829,16765`；5 个测试 | 无主机别名管理；无历史清理 UI；无独立最近连接面板 |
| H18 Persistent Port Forwards | ◐ | Profile 规则持久化；重连后按规则重建 | 无重连恢复测试；无独立常驻 forward daemon；文档 `[x]` 过高 |
| H19 算法配置 | ✅ | `profile.rs:604`；UI；native 回退 | 无真实算法协商 e2e |
| H20 SSH 证书 | ◐ | 证书明确拒绝，回退系统 ssh；`SSH_CERTIFICATES.md` | 信任链未实现；无 CA-only/证书握手拒绝测试 |

---

## 3. 完整 Tabby 对照

### 3.1 UI-SSH

| ID | 状态 | 证据 | 缺口 |
|---|---|---|---|
| UI-SSH-001 Profile 编辑 | ◐ | 字段齐全；e2e 表单截图 | 无 General/Ports/Advanced 分组；无逐字段实时校验 |
| UI-SSH-002 主机密钥确认/管理 | ✅ | Trust/Update 弹窗 + Settings 管理 + 20 例测试 | 文档“缺管理 UI”已过期；只差 e2e 复跑 |
| UI-SSH-003 SFTP 导航 | ✅ | Up/Refresh/元数据/路径转义 + 测试 | 无 GUI 自动断言；文档旧描述已过期 |
| UI-SSH-004 SFTP 文件操作 | ◐ | 上传/下载/mkdir/rename/delete/chmod/队列 | 仅缺断点续传；覆盖确认/递归删除已实现 |
| UI-SSH-005 传输进度 | ◐ | 进度/Cancel/Retry | 无总大小/ETA 展示/自动重试 |
| UI-SSH-006 转发管理 | ✅ | 面板 L/R/D 状态/启停/增删 | `-R 0` 无法保存（见 H08）；无 GUI 自动化 |
| UI-SSH-007 Jump Host | ◐ | 可配置/多跳 | 无链路可视化/跳板独立凭据 |
| UI-SSH-008 Login Scripts | ✅ | 多行编辑 + 远端顺序测试 | 无交互式脚本 |
| UI-SSH-009 Agent/X11 状态 | ◐ | Agent 状态 pill；X11 开关 | 无 X11 转发状态/失败提示 |

### 3.2 SSH 条目

| ID | 状态 | 证据 | 缺口 |
|---|---|---|---|
| SSH-001 建连/超时/重连 | ◐ | native、keepalive、连接状态 | 无 keepalive UI；无真实 sshd；无 SSH 自动重连专项 |
| SSH-002 Connection Manager | ◐ | 5 态模型 + Tab/pill | 无常驻 Connections 列表；无连接中 Cancel |
| SSH-003 基础连接 | ✅ | Profile + native | — |
| SSH-004 认证/Vault | ✅ | password/key/agent/Vault；口令测试 | agent 认证无专门测试 |
| SSH-005 known_hosts | ◐ | native 校验 + 管理 UI | 证书信任链；ssh_config 边界 |
| SSH-006 ssh_config | ◐ | 多数字段已解析 | Port 不生效；ForwardAgent 回退；Match unsupported |
| SSH-007 Jump Hosts | ✅ | native 多跳测试 | 链路可视化/独立凭据属 UI 缺口 |
| SSH-008 ProxyCommand | ✅ | 参数 + 回退 | 无 e2e |
| SSH-009 Agent forwarding | ✅ | 拒绝/成功测试 + 状态 UI | Unavailable 无集成测试；ForwardAgent 配置回退 |
| SSH-010 X11 Forwarding | ✅ | 参数 + 回退 | 无真实 X11 e2e/状态 UI |
| SSH-011 L/R/D 转发 | ◐ | L/R 集成；SOCKS5 | **`-R 0` 被校验拦住**；D 无集成测试 |
| SSH-012 Persistent Forwards | ◐ | Profile 持久化 + 重连重建 | 无常驻 daemon；无重连恢复测试 |
| SSH-013 Login Scripts | ✅ | 解析 + 远端顺序测试 | 无交互式脚本 |
| SSH-014 算法配置 | ✅ | UI + `-o` + native 回退 | 无真实协商 e2e |
| SSH-015 历史/最近连接 | ◐ | `history.toml` + ↺ 最近项 | 无别名管理/历史清理 UI |
| SSH-016 WinSCP | ✗ | 明确 unsupported | 平台不适用，保持明确不支持 |

### 3.3 SFTP

| ID | 状态 | 证据 | 缺口 |
|---|---|---|---|
| SFTP-001 会话 | ◐ | 每次 batch 新建系统 `sftp -b` 子进程 | 非 native SSH Channel；无持久会话；password-only 不能注入 Vault 密码；无真实 sshd |
| SFTP-002 列表/导航/元数据 | ✅ | Up/Refresh/权限/size/mtime/路径转义 + 测试 | 无真实服务器；`ls -1` 回退无元数据（设计） |
| SFTP-003 上传/下载/目录/改名/删除 | ◐ | 覆盖确认/递归删除/特殊字符 + 测试 | **断点续传缺失** |
| SFTP-004 进度/取消/重试 | ◐ | 解析/取消/手动 Retry/队列 | 无总大小、ETA 未显示、无自动重试；真实 sftp 进度可能不可用 |
| SFTP-005 大文件流式 | ◐ | 系统 sftp 流式 | 无限速/续传/总大小；无应用内通道 |
| SFTP-006 独立面板 | ◐ | 480×610 overlay | 非独立 Tab/窗口/插件 API |
| SFTP-007 权限/覆盖/路径/断线 | ◐ | chmod/删除确认/特殊字符转义 | 无续传/断线恢复；非 UTF-8 文件名不可靠往返 |

### 3.4 SERIAL

| ID | 状态 | 证据 | 缺口 |
|---|---|---|---|
| SERIAL-001 枚举/开关 | ◐ | 枚举 + 选择器 + open/close | 无枚举/选择测试；无真机；文档仍写“缺端口选择器” |
| SERIAL-002 多 Profile | ✅ | 通用 ProfileStore + Serial 字段 | 无专门多 Serial 用例 |
| SERIAL-003 串口参数 | ◐ | Profile 校验 + serialport 映射 | 无 5/6/7/8/E/O/2stop/flow 行为测试；无真机 |
| SERIAL-004 输入模式 | ◐ | Text/Hex 有语义 | Readline/Byte 实为 Text；无本地行编辑/逐字节 |
| SERIAL-005 输出模式 | ✅ | Text/Hexdump + 单测 | 只有短输入；无 transport |
| SERIAL-006 换行 | ✅ | none/lf/cr/crlf + 单测 | 仅出站；socat 回退不生效 |
| SERIAL-007 拔插/重连 | ◐ | 断开状态 + 自动重连代码 | 无 UI 开关；重连计数被重置；失败不续调度；无 e2e |
| SERIAL-008 读写缓冲 | ◐ | 有界通道/背压 | 无高频长跑/丢字节指标 |
| SERIAL-009 Zmodem | ✗ | 无实现；多处文档说 unsupported | 条目自身仍写“先核实上游”，需统一为 unsupported 或排期实现 |
| SERIAL-010 时间戳 | ✅ | 状态机 + 单测 | 无 transport 级测试；socat 回退不生效 |

---

## 4. 横切 T01–T05

| ID | 状态 | 证据 | 缺口 |
|---|---|---|---|
| T01 每项失败测试/证据 | ◐ | SFTP 44 例、Serial 9 例、SSH 62 例等存在 | 没有逐项映射/失败记录；多项只有源码测试名，未复跑 |
| T02 对应 e2e | ◐ | `e2e_ssh_sftp_vault.sh`（假 ssh/sftp） | 未覆盖 Up/Refresh、元数据、覆盖确认、递归删除、特殊字符、ETA；无 Serial e2e；无真实 sshd |
| T03 全测 + 2m/15m soak | ◐ | 提交说明有 773 passed | 无保留测试日志；`.logs/soak` 不存在；15m 数字 `576.8→668.8` 与 `+3.90 MiB/min` 算不平（实际 6.13）；soak 无 Serial |
| T04 Codex 复审 | ◐ | review P1/P2 修复提交存在 | 无正式复审报告/P1/P2 清单/接受项记录 |
| T05 只安装一次 release + hash | ✗ | 文档只给 `31c4c2...` | 该 hash 在仓库/安装目录不存在；当前 release/安装已多次变化（`d90f29c`），无验证命令 |

### “今晚 Gate”11 项

实际 **不是 11/11**：

- ✅ S01、S04、S05、S06、H04、H05、H06、H09（8 项）
- ◐ S07（自动重连不可靠）、H08（port 0 被校验拦住、证据不足）
- ◐ T01–T04（测试/e2e/soak/复审未闭环）

---

## 5. 真正未完成/最高优先级

1. **`-R 0:...` 无法保存**：`profile::validate_remote_forward` 拒绝 0，UI 新增转发走它；修复后补 in-process server 分配端口测试。
2. **Serial 自动重连不可靠**：计数被 `open_profile_tab_manual` 清空，退避/上限失效；Profile 表单无 `auto_reconnect` 开关；重连失败不再调度。
3. **SFTP 断点续传完全缺失**（H11/UI-SSH-004/SFTP-003/005/007）。
4. **SFTP 总大小/ETA/自动重试缺失**（H12/UI-SSH-005/SFTP-004）；ETA 已解析但未显示。
5. **独立 SFTP Tab/插件 API、大文件限速、断线恢复**未做。
6. **真实环境验证为 0**：无真实 OpenSSH sshd、无真实串口硬件；SFTP e2e 只覆盖假 sftp。
7. **Zmodem 状态未决**：条目仍写“先核实上游”，但其他文档已写 unsupported；需要统一为明确 unsupported 或排期。
8. **S05/S04/S06 错误处理**：`unwrap_or(false)` 把读取失败混成低电平；DTR/RTS/Break 写失败只吞掉，无硬件闭环。
9. **S08 Readline/Byte 语义**：目前等同 raw Text；要么明确接受并写入文档，要么实现。
10. **H16 ssh_config**：`Port` 不生效；`ForwardAgent yes` 整条回退未文档化。
11. **H06**：ssh_config 的加密 IdentityFile/默认密钥不会触发口令弹窗。

---

## 6. 文档与代码不一致（需后续修）

- `docs/SSH_SFTP.md §6/§7` 仍写 app 351 passed、SFTP 无覆盖确认/递归/特殊字符/权限列；均已过期。
- `docs/CONNECTION_BACKENDS.md`：写 CTS/DSR/DCD 未暴露、`IdentityFile` 回退、ssh-keygen 只在单测用；与代码不符（生产删除仍调 `ssh-keygen -R`）。
- `docs/FEATURE_MATRIX.md`：SERIAL-004..010、UI-PROF-012 仍写无实现，已过期。
- `docs/tabby-parity.md`、`docs/COMPATIBILITY_MATRIX.md`：仍写 Serial 未实现/Cargo 无 serialport。
- `docs/SSH_PLUGIN_POC.md`：仍写 Cargo.lock 无 russh、10 例测试（实际 russh 0.63.3、23 例）。
- `docs/VERIFICATION.md`：期望 666 passed，与 773 基线不一致。
- 本文旧版“Final evidence”的 hash/soak/e2e 不可复核，已删除；需要重跑并保留日志后再写。

---

## 7. 复核命令

```bash
# SSH（注意 ~/.ssh/config accept-new 会让一个预检测试按设计失败）
cargo test -p tervo-app --bin tervo ssh
cargo test -p tervo-app --bin tervo ssh_native
cargo test -p tervo-app --bin tervo known_hosts

# Serial（PTY；无真实硬件）
cargo test -p tervo-app --bin tervo serial_native

# SFTP
cargo test -p tervo-app --bin tervo sftp
scripts/e2e_ssh_sftp_vault.sh   # 假 ssh/sftp；需 Xvfb
```
