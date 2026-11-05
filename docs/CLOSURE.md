# 功能矩阵闭环报告

> 数据来源：`docs/FEATURE_MATRIX.md`（228 条主条目 + 6 条附录）。
> 统计脚本：`python3 scripts/matrix_stats.py`（同时检查 `implemented` 行是否带证据）。
> 最后更新：2026-11-05（goal 第 30 轮最终验收；交付摘要见 [`FINAL_REPORT.md`](FINAL_REPORT.md)）。

## 1. 口径

| 状态 | 含义 |
|---|---|
| `implemented` | 有自动化测试**或**可重复的真机步骤，且限制已写明 |
| `partial` | 主路径可用，仍有明确缺口（矩阵“Limitation”列） |
| `needs_verification` | 上游能力/字段未核实，暂不实现或只做保守实现 |
| `not_started` | 未实现 |
| `unsupported` | 明确不做（附原因，不留假入口） |

加权分 = `(implemented + partial×0.5 + needs_verification×0.25) / 条目数`，
只用于看趋势，不代表“完成度百分比”。

## 2. 总体快照

| 指标 | 数值 |
|---|---|
| 主条目 | **228** |
| implemented | **23（10.1%）** |
| partial | **76（33.3%）** |
| needs_verification | **17（7.5%）** |
| not_started | **105（46.1%）** |
| unsupported | **7（3.1%）** |
| 加权分 | **28.6%**（第 1 轮约 20.2%） |
| 最终验收 | selfcheck PASS（ready 46.7ms / first 47.4ms / RSS 52.5MB）；app 90 / core 66 / conformance 11 |

本轮审计修正了两条状态：`UI-TERM-006`（Copy-on-Select）补上开关与真机证据后升为
`implemented`；`UI-TERM-001`（点击聚焦/IME）因缺少 IME 端到端证据降为 `partial`。
所有 `implemented` 行均带测试/真机记录（脚本检查“证据过短”为 0）。

## 3. 分节状态

| 分节 | 条目 | impl | partial | nv | not_started | unsupported |
|---|---:|---:|---:|---:|---:|---:|
| B1. 主窗口与自绘标题栏 | 9 | 5 | 3 | 0 | 1 | 0 |
| B2. TabBar 操作 | 12 | 2 | 9 | 0 | 1 | 0 |
| B3. Split Pane / 分屏 | 11 | 6 | 1 | 1 | 3 | 0 |
| B4. Sidebar / Toolbar / 连接入口 | 8 | 0 | 2 | 0 | 6 | 0 |
| B5. Connection Manager / Profile UI | 13 | 0 | 2 | 3 | 8 | 0 |
| B6. Terminal View 交互 | 14 | 2 | 9 | 0 | 3 | 0 |
| B7. Settings 设置页 | 14 | 0 | 8 | 0 | 6 | 0 |
| B8. SSH/SFTP/Port Forward UI | 9 | 0 | 5 | 0 | 4 | 0 |
| B9. Serial / Telnet UI | 8 | 0 | 2 | 3 | 2 | 1 |
| B10. Quake Mode | 5 | 0 | 0 | 0 | 5 | 0 |
| B11. 插件管理 UI | 7 | 0 | 3 | 0 | 4 | 0 |
| C1. Terminal Emulator 核心 | 16 | 5 | 10 | 0 | 1 | 0 |
| C2. Local Shell / PTY | 7 | 2 | 3 | 0 | 1 | 1 |
| C3. SSH2 Client | 16 | 0 | 0 | 1 | 14 | 1 |
| C4. SFTP 与文件传输 | 7 | 0 | 0 | 0 | 7 | 0 |
| C5. Zmodem | 4 | 0 | 0 | 0 | 0 | 4 |
| C6. Serial Terminal | 10 | 0 | 0 | 5 | 5 | 0 |
| C7. Telnet | 4 | 0 | 0 | 1 | 3 | 0 |
| C8. Profiles/Session 持久化 | 9 | 0 | 4 | 0 | 5 | 0 |
| C9. Hotkeys / Commands | 7 | 1 | 4 | 0 | 2 | 0 |
| C10. Themes / Appearance | 7 | 0 | 5 | 1 | 1 | 0 |
| C11. Progress / Notification | 6 | 0 | 1 | 1 | 4 | 0 |
| C12. Vault / Secrets | 6 | 0 | 1 | 0 | 5 | 0 |
| C13. Plugin System | 13 | 0 | 0 | 1 | 12 | 0 |
| C14. Logging/Diag/Update/Import | 6 | 0 | 4 | 0 | 2 | 0 |

## 4. 已经能端到端使用（有证据）

- **窗口/壳**：自绘标题栏、最小化/最大化/全屏、TabBar 上/下位置并持久化、
  多会话关闭确认（`.logs/tabbar-bottom2.png`、`docs/ui-shell.md`）。
- **Tab**：新建/关闭/重命名/拖拽排序/右键菜单/Close Others、关闭后保留后台会话并重连
  （`[session] detach/reattach`，pid 不变；`.logs/bgpanel.png`）。
- **分屏**：左右/上下拆分、比例拖动、焦点边框、关闭折叠（workspace 单测 + 真机）。
- **终端**：VT conformance 11 项、alt screen、256 色/truecolor、回滚、selection
  正/反向复制、Copy on select 开关、多行粘贴确认、查找、清屏/全选、鼠标上报
  （`crates/core/tests/*`，app 89 例）。
- **命令**：Command Palette（21 条命令、点击执行）、Hotkeys 只读列表 + 冲突数。
- **设置**：6 个分类（Appearance/Terminal/Window/Config/Hotkeys/Plugins），
  开关即时写配置（`copy_on_select`/`confirm_close`/`confirm_multiline_paste`/
  `tabbar_position`）。
- **Profiles**：local/ssh/serial/telnet 四类、新建/编辑/删除、按类型校验、
  v1→v2 迁移（11 例测试）。
- **连接**：SSH（系统 ssh 客户端，key 认证，端到端 `PARAMIKO-SSH-OK`）、
  Telnet（`TELNET-OK`）、Serial（socat + PTY 对 `SERIAL-OK`）。
- **转发**：`-L` 本地转发端到端（`ECHO:ping`，`scripts/ssh_test_server.py`）。
- **SFTP**：面板列目录/下载/上传端到端（`.logs/sftp-panel.png`）。
- **插件**：`plugins/*.toml` 清单发现/校验/列表（不执行代码）。
- **打包**：32.2 MB stripped 产物、安装演练、启动/RSS/4-pane 基线
  （`docs/PERFORMANCE.md`）。

## 5. 未闭环项与原因（按 D 顺序）

1. **B4/B5 连接入口/连接管理器 UI**：Sidebar/Toolbar/Profile 下拉等需要专门设计，
   当前只有 TabBar 菜单里的 Profiles 面板。
2. **B7 Settings**：无设置搜索、无 Config Sync、无导入导出；Vault 未实现，
   因此 `auth=password` 只存字段不存密码。
3. **B8/B9 连接能力深度**：SFTP 无 mkdir/rename/rm、无进度/取消；转发无管理面板；
   无 Jump Host / Login Scripts / X11；Serial 无数据位/校验/流控/重连；
   Telnet 无选项协商设置。
4. **C3 SSH2 Client**：当前是“系统 ssh 跑在 PTY 里”的 v1，不是内建 russh 实现；
   密码认证、Agent 状态展示、known_hosts 管理、跳板机、X11 转发均未做。
5. **C5 Zmodem（unsupported）**：环境无 `lrzsz`/`sz`/`rz`，且未实现内建协议栈，
   明确不做而不是留假入口。
6. **C6/C7 Serial/Telnet 核心**：同样是系统工具桥接，无内建实现与状态机。
7. **C11 通知/进度**：无后台任务通知与进度模型。
8. **C12 Vault**：无 Secret Service/Keyring 集成，配置与日志也还没有 secret 可脱敏。
9. **C13 插件运行时**：只有清单层，无 JS/沙箱/权限/API/旧插件兼容。
10. **B10 Quake Mode**：未做（需要独立窗口/全局热键/显隐动画）。
11. **D2 性能深度**：fps、P95 帧延迟、50 MiB/s 吞吐、长时间稳定性未验收。

## 6. 为什么不是 100%

- **上游核对受限**：`docs/TABBY_UPSTREAM_AUDIT.md` 里大量 `needs_verification` 需要
  读官方源码/文档，环境无网络，不能凭空“实现一致”。
- **体量差异**：官方 Tabby 是 Electron + 多年插件生态；本仓库是 Slint 原生实现，
  剩余大头（插件运行时、内建 SSH/SFTP 协议栈、Vault、Quake、连接管理器 UI）
  都是“再开一个项目”的量级。
- **本报告定义的“闭环”**是：**每条需求都有可辩护的状态与证据，且没有假按钮/假实现**，
  而不是宣称功能与上游逐条等价。

## 7. 如果继续做，优先级建议

1. 设置搜索 + Config 导入导出（B7，中小）。
2. SFTP 面板补 mkdir/rename/rm + 进度/取消（B8，中）。
3. 转发管理面板（列出/新增/删除/状态）（B8，中）。
4. SSH Jump Host / Login Scripts（C3，中）。
5. 串口参数 + 自动重连（B9/C6，中）。
6. 内建 SSH（russh）试点，替换系统 ssh（C3，大）。
7. Vault（Secret Service）+ 密码认证（C12/C3，大）。
8. 插件 API/沙箱/旧插件兼容（C13，很大）。
9. Quake Mode（B10，中）。
10. 性能深度验收（fps/吞吐/P95）（D2，中）。
