# Tervo

用 Rust + Slint 写的原生终端模拟器，目标是提供 Tabby（Electron）的常用工作流，
同时显著降低内存与输入延迟。

> **独立项目声明**：Tervo 是独立开源项目，与 Tabby / Terminus 的作者及项目**无任何隶属、
> 赞助或背书关系**。“Tabby”是其权利人的商标，本项目仅作功能对比与兼容性说明时提及。
> 本项目不含 Tabby 的代码或美术资源；图标为原创几何标记（生成脚本见 `scripts/gen_icon.py`）。

> 当前进度：Phase 0（可行性验证）已完成（[报告](docs/phase0-report.md)）；
> Phase 2 常用工作流已可用：标签页/分屏/右键四模式、多窗口、Tab 活动/OSC 通知、
> 可编辑快捷键、Profile 搜索/默认/复制、Tab 自动收缩/搜索、Vault 加密凭据、Tabby 配置导入、外部进程插件 API。
> 功能覆盖与已知差距见 [`docs/FEATURE_MATRIX.md`](docs/FEATURE_MATRIX.md) 与
> [`docs/CLOSURE.md`](docs/CLOSURE.md)；最近一次 `./scripts/selfcheck.sh` 通过（app 209 / core 80）。

## 架构（已冻结）

```
┌──────────── app（Slint / Skia）────────────┐
│  MainWindow ── Image(帧位图) ── 键盘映射    │
│       ▲                  │                 │
│       │ 拉帧(有界)        │ 输入(有界)       │
└───────┼──────────────────┼─────────────────┘
        │                  ▼
┌───────┴────────── core ────────────────────┐
│  pane actor 线程: Term + vte::Processor     │
│   ├─ damage → 脏行快照（Frame 复用，零分配）│
│   ├─ Wakeup 容量 1 通道（洪峰合并）          │
│   └─ 语义事件有界通道（title/bell/exit）     │
│  pty-reader 线程 ──► 有界队列 ──► actor      │
│  pty-writer 线程 ◄── 有界队列 ◄── 键盘/PtyWrite│
└────────────────────────────────────────────┘
```

- 每个 pane 一个 `alacritty_terminal::Term`，**不使用**其内置 `EventLoop` 作主循环。
- UI 主动拉取帧，终端核心不推送整帧。
- 所有跨线程通道有界；洪峰时合并唤醒，不阻塞 PTY。
- 分屏 / 标签 / 会话管理全部在应用层，不依赖 alacritty 的 mux。

## 目录

| 路径 | 说明 |
|---|---|
| `crates/core` | VT actor、PTY、帧快照、颜色表、bench 夹具 |
| `crates/app` | Slint UI、字形 atlas、软件光栅化、X11 延迟探针 |
| `scripts/` | 基准、启动/RSS、延迟/FPS、真机 GPU 验证、发布打包 |
| `packaging/` | `.desktop` 入口与打包/安装说明 |
| `docs/` | Phase 0 报告、UI Shell、Tabby 功能对照与主题机制调研 |

## 快速开始

```bash
# 沙箱/CI 内 ~/.cargo 只读时
export CARGO_HOME=$PWD/.cargo

cargo test -p tervo-core
cargo build --release -p tervo-app

# 交互式：有 GPU 走 Skia；无 /dev/dri 会自动回退 winit-software
./target/release/tervo --cols 120 --rows 32

# 发布打包：构建后只 strip dist/ 里的副本（开发二进制保留符号）
./scripts/package.sh                  # → dist/tervo
TERVO_DIST=/tmp/dist ./scripts/package.sh

# 一键自检（构建 + 单测 + VT 契约 + 启动预算；RUN_IME=1 加中文冒烟）
./scripts/selfcheck.sh

# Phase 0 全套指标
./scripts/bench.sh
./scripts/phase0.sh
```

命令行参数：`--cols --rows --scrollback --font-size --shell --latency N --fps SECS --exit-after-first`。

## 打包与安装

两种二进制，用途不同：

| 路径 | 用途 | 体积（本机实测） |
|---|---|---|
| `target/release/tervo` | 开发 / 性能分析：`debug = 1`，保留符号给 `perf` / flamegraph | 168.4 MB |
| `dist/tervo`（`scripts/package.sh`） | 发布产物：复制后 `strip --strip-all` | 30.5 MB（-81.9%） |

`scripts/package.sh` 只 strip 输出目录里的副本，`target/release` 原样保留；
`scripts/install.sh` 同样先把安装副本放进临时目录 strip 再安装，默认 `~/.local`，
不会改动开发二进制。

```bash
./scripts/package.sh                            # → dist/tervo（默认 40 MB 门禁）
TERVO_DIST=/tmp/dist ./scripts/package.sh    # 自定义输出目录（相对仓库根）
./scripts/package.sh /tmp/dist                   # 同上，位置参数
SKIP_BUILD=1 ./scripts/package.sh                # 复用已有 release 二进制
MAX_MB=30 ./scripts/package.sh                   # 自定义体积门禁

PREFIX=/tmp/prefix ./scripts/install.sh          # 装到自定义 prefix（验证用）
./scripts/install.sh --no-strip                  # 调试时安装带符号的副本
```

验证产物：

```bash
ls -l dist/ && file dist/tervo && ./dist/tervo --help
```

细节、环境变量与兼容性风险见 [`packaging/README.md`](packaging/README.md)。

## 已实现

- 无边框窗口 + 自绘标签栏（新建 / 切换 / 关闭 / OSC 标题）。
- 右键菜单分屏（左右 / 上下）、关闭 pane、焦点切换、窗口缩放重排。
- 每个 pane 独立 shell；关闭时自动回收 PTY 与线程。
- 无边框窗口自带 `─` 最小化、`□/❐` 最大化/还原、`✕` 关闭；拖标签栏移动窗口，双击标题栏最大化。
- 快捷键：`Ctrl+T` 新建标签、`Ctrl+Shift+N` 新建窗口、`Ctrl+W` 关闭标签。
- 终端链接：`Ctrl+左键点击` 打开 `http` / `https` / `file` 链接（OSC 8 优先）；右键链接所在格出现
  `Open Link` / `Copy Link`，`xdg-open` 失败会在底部浮层提示并写日志。
- 多窗口（MVP）：`Ctrl+Shift+N` / 标签栏右键 → New Window / 命令面板 `New Window`
  会拉起一个独立的 Tervo 进程，各自拥有 Tab/PTY；窗口互不影响，Tab 暂不能跨窗口移动。
- 设置面板（⚙）：12 套内置主题 + `Auto`（跟随系统深/浅色，默认 Yaru Dark/Light）、字号、回滚行数；
  也支持把 iTerm2 / Windows Terminal / Alacritty / kitty / Xresources 主题直接放进 `themes/` 目录。
- Tabby 功能对照见 [`docs/tabby-parity.md`](docs/tabby-parity.md)；
  主题下载/自定义见 [`docs/themes.md`](docs/themes.md)，Tabby 主题机制调研见 [`docs/tabby-themes.md`](docs/tabby-themes.md)；
  度量/诊断/改进/门禁闭环见 [`docs/self-iteration.md`](docs/self-iteration.md)。

## 日常使用

```bash
./scripts/install.sh          # 装到 ~/.local（strip 后安装；--no-strip 保留符号）
tervo                 # 直接启动
```

| 操作 | 方式 |
|---|---|
| 新建 / 关闭标签 | `Ctrl+T` / `Ctrl+W`，或标签栏 `+` / `×` |
| 新建窗口 | `Ctrl+Shift+N`，或标签栏右键 → New Window（独立进程，关闭互不影响） |
| 分屏 | 右键 pane → Split Right / Split Down |
| 关闭 pane | 右键 → Close Pane；关掉最后一个 pane 退出应用 |
| 复制 | 鼠标拖选（松手自动复制，同时写系统剪贴板与 X11 PRIMARY）或 `Ctrl+Shift+C`，或右键 → Copy |
| 复制 | 鼠标拖选（松手自动复制，同时写系统剪贴板与 X11 PRIMARY）或 `Ctrl+Shift+C`，或右键 → Copy |
| 打开链接 | `Ctrl+左键点击` http/https/file 链接（OSC 8 优先）；右键 → Open Link / Copy Link |
| 粘贴 | `Ctrl+Shift+V` 或右键 → Paste（多行会按终端 bracketed-paste 规则包起来）；中键优先粘 X11 PRIMARY，为空/不可读时回退普通剪贴板 |
| 搜索 | `Ctrl+F`，输入即高亮下一个；`Enter` 下一个，`Shift+Enter` 上一个，`Esc` 关闭 |
| 中文输入 | fcitx5 / ibus（X11 XIM）：拼音候选窗跟随光标；组合中的回车/退格只作用于输入法，不会写入 shell |
| 回滚历史 | 鼠标滚轮 |
| 窗口置顶 | 右键标签栏空白处 → Always on Top |
| 调整分屏 | 拖动 pane 之间的分隔线（左右/上下比例都可调） |
| 缩放窗口 | 拖动窗口四边/四角（无边框窗口自绘命中区） |
| 设置 | 标签栏 ⚙：主题 / 字号 / 回滚行数 |
| 移动窗口 | 拖标签栏；双击标签栏最大化/还原 |

配置：`~/.config/tervo/config.toml`（`TERVO_CONFIG` 可覆盖）。

剪贴板：拖选/Ctrl+Shift+C 会同时写系统剪贴板与 X11 PRIMARY（中键粘贴用）。
远端程序可通过 **OSC 52** 读写本机剪贴板，默认放行；不需要时在配置里关闭：

```toml
osc52 = "deny"   # 默认 "allow"；deny 同时拒绝写入和查询
```

> 安全提示：`allow` 意味着任何能向终端输出转义序列的程序（包括 SSH 远端、
> `cat` 一个不可信文件、tmux 里的远端进程）都能覆盖或读取你的系统剪贴板/
> PRIMARY。只处理可信内容，或设为 `deny`。

默认 `theme = "auto"`：跟随系统浅色/深色（深色 → Yaru Dark，浅色 → Yaru Light），也可以显式选任意主题。
把现成配色文件丢进 `~/.config/tervo/themes/` 即可在设置面板里选择，支持 iTerm2 `.itermcolors`、
Windows Terminal `.json`、Alacritty `.toml`、kitty `.conf`、Xresources，以及 Catppuccin/Rosé Pine 的 YAML 片段；
详见 [`docs/themes.md`](docs/themes.md)。

## 图标与桌面项

```bash
# 安装桌面项与图标（hicolor 目录）
install -Dm644 packaging/tervo.desktop ~/.local/share/applications/tervo.desktop
install -Dm644 assets/icon.png ~/.local/share/icons/hicolor/256x256/apps/tervo.png
update-desktop-database ~/.local/share/applications 2>/dev/null || true
```

- 应用内：标签栏左侧品牌图标、设置面板标题图标（`assets/icon-small.png`）。
- 窗口/任务栏图标：`_NET_WM_ICON` 由 Rust 侧直接写入（`assets/icon-64.rgba`），
  因为 Slint 内置 `Window.icon` 在本环境下会写成空属性。

## 已知限制

- 终端字符光栅仍是 CPU（fontdue），但已改为双缓冲持久帧：`Image` 只做 refcount
  clone，不再每帧整屏 `SharedPixelBuffer` 拷贝；Skia 仍会把帧上传为纹理再合成。
  默认有 `/dev/dri` 走 Skia/GL，无 DRM 或 1s 内确认不到 GPU API 会自动回退
  `winit-software`；真机 NVIDIA 的 GPU 帧率/延迟数据仍待用户复测。
- tab 不能拖拽排序；分屏已有分隔条拖拽与窗口拖边缩放。
- 分屏比例、窗口位置/大小都在应用层维护，Wayland 下窗口 `set_position` 可能被合成器忽略。
- 字体固定 JetBrains Mono（回退 DejaVu）；中文自动回退 WenQuanYi Micro Hei / Droid / Noto CJK，
  暂不支持自选字体、连字，emoji 仍留空。
- IME 走 winit 的 XIM（fcitx5 / ibus）；Wayland、GTK/Qt 的 IM module、macOS 输入法尚未验证。
- 主题跟随系统依赖 GNOME `gsettings`；非 GNOME 环境会回退 `gtk-theme` / `COLORFGBG` 做一次性检测。
- 自定义主题仍在启动时扫描，改主题文件后需要重启（热重载尚未实现）。

## 许可证

Tervo 以 **GPL-3.0-only** 发布，全文见 [`LICENSE`](LICENSE)。

选择 GPL-3.0 的原因：UI 依赖 **Slint 1.18**，其许可证为
`GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`；
本项目使用其中的 GPL-3.0 选项，因此整体按 GPL-3.0-only 分发。若你需要宽松/闭源许可，
需要向 Slint GmbH 购买商业或 royalty-free 许可，本项目无法转授。

其余依赖均为宽松许可（MIT / Apache-2.0 / Zlib / BSD），与 GPL-3.0 兼容；
完整依赖清单与随附许可证文本见 [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md)；
合规细节、依赖兼容性与分发检查清单见 [`docs/LICENSING.md`](docs/LICENSING.md)。

## 商标与第三方声明

- “Tabby”“Terminus”是其各自权利人的商标。本项目仅在做功能对照、兼容性说明时
  以**指名方式**提及，不暗示任何关联或背书。
- 项目名称、图标、界面均为原创；图标由 `scripts/gen_icon.py` 程序化生成。
- 内置配色方案（Tokyo Night、Dracula、Solarized、Yaru 等）为社区调色板，
  名称可能属于各自作者；仓库只包含颜色数值，不含对方代码或美术资源。
- 导入第三方主题文件时，请自行确认该主题的许可证；Tervo 不随附这些文件。

## 改名说明

项目曾用工作名 “Rust Tabby”，为避让 Tabby 的商标与品牌混淆，现更名为 **Tervo**
（仓库目录名可以任意）。配置目录、环境变量、二进制名均已随之改为
`~/.config/tervo`、`TERVO_*`、`tervo`；旧配置不会自动迁移，如需保留请手动复制。

## 日志与诊断

- 每次启动写日志：`~/.local/state/tervo/logs/tervo-<epoch>.log`（保留 8 个）；
  `TERVO_LOG=0` 关闭，`TERVO_LOG_DIR` 改目录。
- `tervo --diagnose` 打印环境、屏幕、活动窗口、其他 Tervo 实例窗口与最近日志。
- 右键排查：日志里有 `[trace] pane-down-right` 表示事件到达 Tervo；没有则多半是别的窗口
  （常见：旧的全屏 Tervo 实例）挡住了，详见 [`docs/LOGGING.md`](docs/LOGGING.md)。

## 开发
