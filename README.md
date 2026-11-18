# Tervo

用 Rust + Slint 写的原生终端模拟器：GPU 渲染（Skia / wgpu，无 GPU 自动回退软件光栅化），
目标是把 Tabby 的日常终端工作流做快、做轻——低内存、低输入延迟、可脚本化控制。

> **独立项目**：Tervo 与 Tabby / Terminus 的作者及项目无隶属、赞助或背书关系。
> “Tabby”是其权利人的商标，本项目仅在功能对比与兼容性说明中提及；不含 Tabby 的代码或美术资源。

## 主要功能

**终端核心**

- VT220 / xterm 兼容：宽字符与 CJK、emoji ZWJ 合并、组合字符、24 位色、备用屏、鼠标上报（1000/1002/1003/1006）
- 内联图片：Sixel、iTerm2 IIP（OSC 1337）、Kitty graphics（含源裁剪与 placement）
- 查找（大小写/正则）、双击选词/三击选行、复制粘贴（bracketed paste）、超链接与文件路径点击
- 滚动历史与滚动条、屏内缩放（`Ctrl+±`）、Shell Integration（OSC 133/7：命令边界、退出码、当前目录）
- 键盘：kitty keyboard protocol、多段快捷键、Caps/Num Lock 状态、IME 候选框跟随真实光标

**工作区**

- 标签页（重命名/撤销、最近使用栈、关闭范围快捷键、会话恢复）+ 分屏（拖拽重排、放大、广播输入的危险命令确认）
- 命名工作区：把当前分屏布局存为条目，之后一键恢复（`workspace.save` / `workspace.open`）
- 侧栏（Profile 常驻入口、状态区 pill）、命令面板（`Ctrl+Shift+P`）、快捷键设置页
- 多窗口：单进程多窗口、关闭前确认、退出恢复布局

**连接与插件**（可选，不装也能当纯本地终端用）

- Profile：本地 shell（多 Profile 同窗混用）、SSH、Serial、Telnet；启动失败/断线可见状态
- SSH：系统 `ssh` 路径 + **官方插件 POC**（`Backend::Plugin` transport seam）；known_hosts、端口转发、ProxyJump
- 插件 API v3：外部进程 + JSON-RPC；命令/快捷键/`when` 上下文、状态栏、声明式面板与设置、右键菜单装饰点
- Vault：加密凭据与 SSH 密码 askpass；MCP：Unix socket 上的实时控制与离线诊断

**可靠性**

- 会话快照（原子写 + `.bak` 回退）、异常退出检测、连续崩溃计数、safe mode（≥3 次异常退出时本次不加载插件）
- 内存门禁：2m/15m soak 基线，RSS/PSS、线程/FD 全程记录；release 打包带校验和

## 安装

### 从源码构建

```bash
git clone <repo> && cd tervo
export CARGO_HOME=$PWD/.cargo        # 沙箱/CI 里 ~/.cargo 只读时
cargo build --release -p tervo-app
./target/release/tervo --cols 120 --rows 32
```

运行环境：Linux（X11；Wayland 通过 XWayland），Rust 1.8x，Slint 1.18。
有 `/dev/dri` 时默认 GPU（Skia），否则自动回退 `winit-software`。

### 打包与安装

```bash
./scripts/package.sh                  # → dist/（二进制 + tar.gz + .deb + SHA256）
./scripts/install.sh                  # 安装到 $PREFIX（默认 /usr/local）
./scripts/install.sh --rollback       # 回滚到上一版
./scripts/test_packaging.sh           # 打包 + 干净 prefix 安装 + 升级/回滚/卸载验证
```

发布流程、可复现构建细节见 [`docs/RELEASING.md`](docs/RELEASING.md) 与
[`packaging/README.md`](packaging/README.md)。

## 使用

启动 `tervo`；配置与 Profile 在 `~/.config/tervo/`（`config.toml`、`profiles.toml`、
`workspaces.toml`），日志与状态在 `~/.local/state/tervo/`。

常用快捷键（全部可在 设置 → 快捷键 里改）：

| 操作 | 快捷键 |
|---|---|
| 命令面板 | `Ctrl+Shift+P` |
| 新建 / 关闭标签页 | `Ctrl+T` / `Ctrl+W` |
| 标签页 1–9 | `Ctrl+1` … `Ctrl+9` |
| 向右 / 向下分屏 | `Ctrl+Shift+E` / `Ctrl+Shift+D` |
| 复制 / 粘贴 | `Ctrl+Shift+C` / `Ctrl+Shift+V` |
| 查找 | `Ctrl+F` |
| 侧栏 | `Ctrl+Shift+B` |
| 放大 / 还原窗格 | `Ctrl+Shift+M` |
| 缩放 / 重置 | `Ctrl+=` `Ctrl+-` / `Ctrl+0` |
| 全屏 | `F11` |

其他入口：`tervo --restore` 恢复上次布局；`tervo diag env|snapshot|trace` 离线诊断；
MCP 控制见 [`docs/MCP.md`](docs/MCP.md)。

## 文档

详细文档已按主题整理到 **[`docs/README.md`](docs/README.md)**（wiki 式索引）：

- 开始使用：[架构](docs/ARCHITECTURE.md) · [产品原则与路标](docs/PRODUCT_PRINCIPLES.md) · [UI 规范](docs/UI_DESIGN.md)
- 使用指南：[插件安装](docs/PLUGIN_INSTALL.md) · [插件 API v3](docs/PLUGIN_V3.md) · [SSH/SFTP](docs/SSH_SFTP.md) · [Vault](docs/VAULT.md) · [MCP](docs/MCP.md) · [诊断](docs/DIAGNOSTICS.md) · [soak](docs/SOAK.md)
- 设计与内部：[传输插件](docs/PLUGIN_TRANSPORT.md) · [工作区壳](docs/WORKSPACE_SHELL.md) · [图片合成](docs/IMAGE_COMPOSITING.md) · [多窗口](docs/MULTI_WINDOW_ARCHITECTURE.md)
- 状态与报告：[功能矩阵](docs/FEATURE_MATRIX.md) · [闭环报告](docs/CLOSURE.md) · [范围审视](docs/SCOPE_REVIEW.md) · [性能](docs/PERFORMANCE.md) · [行动清单](docs/ACTION_ITEMS.md)
- 归档：[docs/archive/](docs/archive/)（历史报告与一次性分析）

## 开发

```bash
cargo test -p tervo-core                 # core 单测 + VT 契约
cargo test -p tervo-app --bin tervo      # app 单测
./scripts/selfcheck.sh                   # 构建 + 单测 + 启动预算（RUN_IME=1 加中文冒烟）
./scripts/soak.sh                        # 2m/15m 内存门禁（DURATION=15m）
python3 scripts/e2e_split_repaint.py      # 像素级分屏重绘 e2e
python3 scripts/check_docs_counts.py     # 文档数字对账（发版前必跑）
python3 scripts/check_doc_links.py       # 文档相对链接检查
```

开发规则见 [`AGENTS.md`](AGENTS.md)（大任务 18:00 后；改动必须有失败测试/证据；
长门禁 2m/15m/30m）。变更记录见 [`CHANGELOG.md`](CHANGELOG.md)。

## 许可证

GPL-3.0-only，见 [`LICENSE`](LICENSE)；第三方组件与商标声明见
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) 与 [`docs/LICENSING.md`](docs/LICENSING.md)。
