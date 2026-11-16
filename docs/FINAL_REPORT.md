# 最终交付报告（Tervo）

> **2026-11-16 注：本报告是 2026-11-05 的历史快照，数字与“未做”清单已过时**
> （Vault、Quake、插件运行时、多窗口、MCP 在 0.4/0.5 已落地）。当前状态请看
> [`MAINLINE_STATUS.md`](MAINLINE_STATUS.md)、[`CLOSURE.md`](CLOSURE.md)、
> [`CHANGELOG.md`](../CHANGELOG.md)。

> 本报告对应 30 轮 goal 的收尾验收（2026-11-05，最后一轮）。
> 结论先说：**这是一个能日常跑起来的 Slint/X11 终端工作区，但不是官方 Tabby 的完整替代品。**
> 功能矩阵仍有 105 条 `not_started`；剩余大头（插件运行时、内建 SSH/SFTP 协议栈、
> Vault、Quake、连接管理器 UI）都相当于再开一个项目。详见 [`CLOSURE.md`](CLOSURE.md)。

## 1. 如何构建 / 运行 / 验收

```bash
export CARGO_HOME=$PWD/.cargo
cargo build --release                       # 开发二进制（带符号）
./target/release/tervo              # 运行
./scripts/selfcheck.sh                      # 构建 + 三套测试 + 启动/RSS 门禁
SKIP_BUILD=1 ./scripts/package.sh /tmp/dist # strip + ≤40MB 门禁 + --help 冒烟（产物 dist/tervo）
PREFIX=/tmp/prefix ./scripts/install.sh     # bin + .desktop + 图标
python3 scripts/matrix_stats.py             # 功能矩阵统计
```

## 2. 最终验收数据（本轮实测）

| 项目 | 结果 |
|---|---|
| `selfcheck.sh` | **PASS**：ready 52.6 ms / first_damage 85.7 ms（口径修正后）/ 空闲 RSS 52.5 MB |
| 测试 | app **90** / core **66** / VT conformance **11**（共 167） |
| 打包 | 193 MB（带符号）→ **32.1 MB stripped**（−82.5%），门禁 ≤40 MB ✓ |
| 安装 | `/tmp/final-prefix/bin/tervo` + `.desktop` + 图标，`--help` 正常 |
| 4 pane | RSS **44 MB**、空闲 CPU **0.2%**（`docs/PERFORMANCE.md`） |
| 功能矩阵 | 228 条：implemented 23 / partial 76 / needs_verification 17 / not_started 105 / unsupported 7，加权 **28.6%** |

## 3. 已经可用且验证过的能力（按 D 顺序）

1. **UI 壳**：自绘标题栏、无边框拖拽/缩放、最小化/最大化/全屏、TabBar 上/下位置并持久化。
2. **工作区**：Tab 新建/关闭/重命名/拖拽排序/右键菜单/Close Others、关闭确认、
   后台会话保留与重连（pid 不变）、左右/上下分屏、比例拖动、焦点边框、关闭折叠。
3. **终端**：alacritty VT 后端、alt screen、256 色/truecolor、回滚、查找、清屏/全选、
   正/反向选择复制、Copy-on-select 开关、多行粘贴确认、鼠标上报、bracketed paste 包裹。
4. **键盘/命令**：Ctrl+T/W/F、Ctrl+Shift+C/V/P/E/D/W、Ctrl+Tab/Shift+Tab、Ctrl+1–9、
   F11、F2；Command Palette 21 条命令；Hotkeys 只读页 + 冲突数。
5. **设置**：6 分类（Appearance/Terminal/Window/Config/Hotkeys/Plugins）+ 搜索跳转；
   开关即时写配置（`copy_on_select`/`confirm_close`/`confirm_multiline_paste`/`tabbar_position`）。
6. **Profiles**：local/ssh/serial/telnet 四类，新建/编辑/删除、按类型校验、v1→v2 迁移。
7. **连接**：SSH（系统 ssh，key 认证，paramiko 服务端到端 `PARAMIKO-SSH-OK`）、
   Telnet（`TELNET-OK`）、Serial（socat + PTY 对 `SERIAL-OK`）、`-L` 端口转发（`ECHO:ping`）。
8. **SFTP**：面板列目录/下载/上传端到端（`.logs/sftp-panel.png`）。
9. **插件**：`plugins/*.toml` 清单发现/校验/列表（明确不执行代码）。
10. **打包/性能**：strip 产物、安装演练、启动/RSS/4-pane 基线（`docs/PERFORMANCE.md`）。

证据索引：`.logs/`（截图与 selfcheck 历史）、`crates/*/tests/`、各文档“Test”列。

## 4. 明确未闭环（不要误读为可用）

- **内建 SSH/SFTP 协议栈**：当前是系统 `ssh`/`sftp` 跑在 PTY 里，没有 russh/内建实现。
- **密码/Vault**：`auth=password` 只存字段，不存密码；无 Keyring/Secret Service。
- **插件运行时**：无 JS/沙箱/权限/API/旧插件兼容，只有清单。
- **Zmodem**：`unsupported`（环境无 lrzsz，未实现内建协议）。
- **Quake Mode、Sidebar/Toolbar、连接管理器 UI、设置导入导出、通知/进度**：未做。
- **性能深度**：fps、P95、50 MiB/s 吞吐、长稳未验收。
- **上游核对**：网络不可用，大量 `needs_verification` 的上游行为未核实。

## 5. 环境/自动化经验（复用价值）

- 键盘自动化必须 `XMODIFIERS=@im=none`，否则中文输入法会拦截 XTest 按键。
- 右键菜单行位置 = `click_y + 18 + 30*行号`，菜单 x 被夹到 `min(x, 宽-160)`。
- Slint 浮层要声明在内容之后才有正确 z 序；`TextInput.text` 清空需双向绑定。
- 截图验收用 `xwd` + `scripts/xwd2png.py`（无需 ImageMagick）。
- 测试 SSH/SFTP/转发用 `scripts/ssh_test_server.py`（paramiko，含 shell/SFTP/direct-tcpip）。

## 6. 如果继续做

优先级见 `docs/CLOSURE.md` 第 7 节：设置导入导出 → SFTP 深度 → 转发管理面板 →
Jump Host → 串口参数/重连 → 内建 SSH → Vault → 插件运行时 → Quake → 性能深度验收。

## 7. 目标状态

30 轮内完成了 UI 壳、工作区、终端交互、Profiles/连接、SFTP/转发、设置/快捷键、
插件清单、打包与性能基线；**功能矩阵尚未闭环**（见第 4 节与 `CLOSURE.md`）。
因此不宣称“完整替代官方 Tabby”，goal 保持 active，等待后续轮次继续。
