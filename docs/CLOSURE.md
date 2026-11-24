# 功能矩阵闭环报告

> **状态快照：2026-11-16 · v0.5.0（`6102bb9`）。** 本页数字由
> `python3 scripts/matrix_stats.py` 从 [`FEATURE_MATRIX.md`](FEATURE_MATRIX.md) 现算，
> 不再手工维护；未复核的计划项见 [`ACTION_ITEMS.md`](ACTION_ITEMS.md)。
> 发版后的“当前主线事实”以 [`MAINLINE_STATUS.md`](MAINLINE_STATUS.md) 为准，
> 版本说明以 [`CHANGELOG.md`](../CHANGELOG.md) 为准。

> 数据来源：`docs/FEATURE_MATRIX.md`（**244 条主条目 + 8 条附录**，附录不计入完成度）。
> 统计脚本：`python3 scripts/matrix_stats.py`（同时检查 `implemented` 行是否带证据）。
> 最后更新：2026-11-16。

> 历史快照（勿引用）：2026-11-05 第 30 轮为 228 条 / 23 implemented / 加权 28.6%；
> 交付摘要见 [`FINAL_REPORT.md`](archive/FINAL_REPORT.md)。

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
| 主条目 | **244** |
| implemented | **111（45.5%）** |
| partial | **72（29.5%）** |
| needs_verification | **13（5.3%）** |
| not_started | **41（16.8%）** |
| unsupported | **7（2.9%）** |
| 加权分 | **61.6%**（2026-11-05 第 30 轮为 28.6%） |
| 证据检查 | implemented 57 条全部带测试/真机证据，脚本检查“证据过短”= 0 |

自 2026-11-05 那版快照后，v0.5.0 把这几组能力并进了矩阵计数：单进程多窗口 +
session v2、崩溃恢复（autosave/fsync/`.bak`/`session.running` marker）、Plugin API v3
（含 WASM output_filter、本地安装/回滚/签名）、MCP live control + 默认 `mcp.sock`、
GPU-native Scene（Sixel/IIP/Kitty 进 GPU）。逐条状态仍以矩阵行和其证据列做准。

## 3. 分节状态

| 分节 | 条目 | impl | partial | nv | not_started | unsupported |
|---|---:|---:|---:|---:|---:|---:|
| B1. 主窗口与自绘标题栏 | 9 | 5 | 4 | 0 | 0 | 0 |
| B2. TabBar 操作 | 15 | 7 | 8 | 0 | 0 | 0 |
| B3. Split Pane / 分屏操作 | 11 | 7 | 2 | 1 | 1 | 0 |
| B4. Sidebar、Toolbar 与连接入口 | 8 | 0 | 2 | 0 | 6 | 0 |
| B5. Connection Manager / Profile 管理 UI | 13 | 5 | 4 | 1 | 3 | 0 |
| B6. Terminal View 交互 | 19 | 9 | 9 | 0 | 1 | 0 |
| B7. Settings 设置页 | 15 | 0 | 12 | 0 | 3 | 0 |
| B8. SSH / SFTP / Port Forward UI | 9 | 1 | 7 | 0 | 1 | 0 |
| B9. Serial / Telnet UI | 8 | 0 | 2 | 3 | 2 | 1 |
| B10. Quake Mode 与窗口操作 | 5 | 3 | 2 | 0 | 0 | 0 |
| B11. 插件管理 UI | 7 | 0 | 7 | 0 | 0 | 0 |
| C1. Terminal Emulator 核心 | 18 | 5 | 12 | 0 | 1 | 0 |
| C2. Local Shell / PTY | 7 | 2 | 3 | 0 | 1 | 1 |
| C3. SSH2 Client | 16 | 2 | 2 | 1 | 10 | 1 |
| C4. SFTP 与文件传输 | 7 | 0 | 7 | 0 | 0 | 0 |
| C5. Zmodem | 4 | 0 | 0 | 0 | 0 | 4 |
| C6. Serial Terminal | 10 | 0 | 0 | 5 | 5 | 0 |
| C7. Telnet | 4 | 0 | 0 | 1 | 3 | 0 |
| C8. Profiles / Workspace / Session 持久化 | 9 | 2 | 4 | 0 | 3 | 0 |
| C9. Hotkeys / Commands / Keyboard protocol | 12 | 7 | 3 | 0 | 2 | 0 |
| C10. Themes / Color Schemes / Appearance | 7 | 0 | 6 | 1 | 0 | 0 |
| C11. Progress / Notification / Smart Tabs | 6 | 2 | 3 | 0 | 1 | 0 |
| C12. Vault / Secrets / Security | 6 | 0 | 5 | 0 | 1 | 0 |
| C13. Plugin System 与旧插件兼容 | 13 | 0 | 6 | 1 | 6 | 0 |
| C14. Logging / Diagnostics / Update / Import-Export | 6 | 0 | 5 | 0 | 1 | 0 |
| **合计** | **244** | **57** | **115** | **14** | **51** | **7** |

## 4. 已经能端到端使用（v0.5.0 主线，证据指向）

> 权威清单见 [`MAINLINE_STATUS.md`](MAINLINE_STATUS.md)；这里只列能被外部复现的部分。

- **多窗口 / 会话**：单实例 primary + 0600 Unix control socket 转发；进程内
  `WindowRegistry`/`WindowRuntime`；session v2 恢复全部窗口的 geometry/maximized/
  active_tab/tabs，kill -9 后可恢复（`scripts/e2e_multi_window_session.sh`、
  `scripts/mw_session_e2e.py`）。限制：不恢复 scrollback、屏幕内容和正在运行的进程。
- **崩溃恢复**：autosave + 写盘 fsync + 上一代 `.json.bak` + `session.running` marker
  （`crates/app/src/session.rs`，CHANGELOG 0.5.0「会话崩溃恢复」）。
- **分屏 / Tab**：左右上下拆分、比例拖动、焦点边框、close pane 折叠；Tab 新建/关闭/
  重命名/拖拽/右键菜单/Close Others（workspace 单测 + `[ws] split|close` 日志）。
  已知回归：新 pane 首帧不重绘，见 §5 第 1 条。
- **终端**：VT conformance、alt screen、256 色/truecolor、回滚、选择/复制、查找
  （大小写/正则）、清屏/全选、鼠标上报；Sixel / iTerm2 IIP / Kitty 图片进 GPU Scene。
- **命令 / 设置**：Command Palette、热键录制 + 冲突检测、设置分类与搜索/跳转。
- **插件**：Plugin API v3（子进程 + JSON-RPC、权限确认、supervisor）、WASM
  `terminal.output_filter` 热路径、本地安装/更新/回滚/签名；v1/v2 明确 unsupported。
- **MCP / 诊断**：`xtervo diag serve`（stdio）与默认 `$XDG_RUNTIME_DIR/xtervo/mcp.sock`
  （token + read/input/tabs/destructive 分组）；`diag doctor`、metrics、observability、
  Settings → Help → Feedback 脱敏支持包。
- **连接**：Profiles（local/ssh/serial/telnet）、SFTP（含 mkdir/rename/rm）、
  `-L` 转发、Telnet、Serial（系统工具桥接，非内建协议栈）。

## 5. 未闭环项与原因（2026-11-16 复核）

1. **分屏首帧不重绘（观察项；0.5.0 隔离环境不可复现）**：原证据来自 0.4.0-rc.2，
   0.5.0 在 Xvfb 下单/双窗口 split 都能正常绘制（`target/repro_split*.py`）。以下为原始记录：
   `split` 之后新 pane 的 Term 已有内容
   （`live_output` 能读到 `➜  ~`）、也有 damage，但帧不再重建，界面空白到下一次外力
   重绘。证据：`live_snapshot` 显示 `generation=6` 而 `damage.w=0`，日志只有
   `frame rev=2 / glyphs=0` 一帧。修之前必须先补失败测试。
2. **恢复的深水区**：只恢复“布局/cwd/Profile”；没有 safe mode / 崩溃循环熔断，
   也没有 ENOSPC、权限拒绝、日志截断、配置损坏这些破坏面的恢复矩阵。
3. **B4 连接入口（6 条 not_started）**：Sidebar / Toolbar / Connection Manager 未做。
4. **C3 SSH2（10 条 not_started）**：仍是“系统 ssh 跑在 PTY 里”，无内建 russh、
   无 Jump Host / X11 / agent 状态 / known_hosts 管理。
5. **C13 旧插件兼容（6 条 not_started）**：v3 之外无 v1/v2 兼容层，也没有 OS 级沙箱。
6. **C6/C7 Serial / Telnet（5+3 条 not_started）**：系统工具桥接，无内建状态机。
7. **C5 Zmodem（unsupported）**：明确不做，不留假入口。
8. **B7 / C8 剩余**：Config Sync、导入导出边界、Workspace 切换细节。
9. **性能深度**：P95 帧延迟、吞吐、24h soak、Wayland 原生、emoji shaping、atlas 驱逐。
10. **MCP 治理**：per-harness scope 与审计留存仍是最小实现。

## 6. 为什么不是 100%

- **上游核对受限**：`docs/TABBY_UPSTREAM_AUDIT.md` 里仍有 `needs_verification` 条目需要
  读官方源码/文档；本机能否联网取决于会话环境，没有网络时不能凭空“实现一致”。
- **体量差异**：官方 Tabby 是 Electron + 多年插件生态；本仓库是 Slint 原生实现，**不打算追平**。
  连接管理器 UI、内建 SSH/SFTP 协议栈仍在做，但按 xtervo 自己的使用价值排期，不为“补齐 Tabby”而做；
  旧插件兼容不在目标内。
- **本报告定义的“闭环”**是：**每条需求都有可辩护的状态与证据，且没有假按钮/假实现**，
  而不是宣称功能与上游逐条等价。

## 7. 如果继续做，优先级建议（2026-11-16）

1. **分屏首帧不重绘**（回归，见 §5 第 1 条）：先写失败测试，再修帧拉取路径。
2. **恢复矩阵 + safe mode**：ENOSPC/权限/截断/配置损坏的注入测试；崩溃循环熔断与
   安全模式启动（`docs/DEV_SCHEDULE.md` 里“周日 09:30”那两项）。
3. **内建 SSH（russh）试点**，替换系统 ssh；带 Jump Host / known_hosts / agent 状态（C3，大）。
4. **B4 连接入口**：Sidebar / Toolbar / Connection Manager（6 条 not_started）。
5. **C13 旧插件兼容 + OS 沙箱**（很大）。
6. **串口参数 + 自动重连**、Telnet 选项协商（C6/C7，中）。
7. **性能深度验收**：P95 / 吞吐 / 24h soak / Wayland / emoji shaping（D2，中）。
8. **MCP 治理**：per-harness scope、审计留存（C14/MCP，中）。
