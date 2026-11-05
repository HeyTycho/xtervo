# Rust Tabby

用 Rust + Slint 重写的原生终端模拟器，目标替代 Tabby（Electron），并显著降低内存与输入延迟。

> 当前进度：Phase 0（可行性验证）已完成（[报告](docs/phase0-report.md)）；
> Phase 1 功能线进行中：标签页 / 分屏 / 右键菜单已可用（[UI Shell 文档](docs/ui-shell.md)）。

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
| `scripts/` | 基准、启动/RSS、延迟/FPS、真机 GPU 验证 |
| `docs/` | Phase 0 报告、UI Shell、Tabby 功能对照与主题机制调研 |

## 快速开始

```bash
# 沙箱/CI 内 ~/.cargo 只读时
export CARGO_HOME=$PWD/.cargo

cargo test -p rusttabby-core
cargo build --release -p rusttabby-app

# 交互式：有 GPU 走 Skia；无 /dev/dri 会自动回退 winit-software
./target/release/rusttabby-app --cols 120 --rows 32

# 一键自检（构建 + 单测 + VT 契约 + 启动预算；RUN_IME=1 加中文冒烟）
./scripts/selfcheck.sh

# Phase 0 全套指标
./scripts/bench.sh
./scripts/phase0.sh
```

命令行参数：`--cols --rows --scrollback --font-size --shell --latency N --fps SECS --exit-after-first`。

## 已实现

- 无边框窗口 + 自绘标签栏（新建 / 切换 / 关闭 / OSC 标题）。
- 右键菜单分屏（左右 / 上下）、关闭 pane、焦点切换、窗口缩放重排。
- 每个 pane 独立 shell；关闭时自动回收 PTY 与线程。
- 无边框窗口自带 `─` 最小化、`□/❐` 最大化/还原、`✕` 关闭；拖标签栏移动窗口，双击标题栏最大化。
- 快捷键：`Ctrl+T` 新建标签、`Ctrl+W` 关闭标签。
- 设置面板（⚙）：12 套内置主题 + `Auto`（跟随系统深/浅色，默认 Yaru Dark/Light）、字号、回滚行数；
  也支持把 iTerm2 / Windows Terminal / Alacritty / kitty / Xresources 主题直接放进 `themes/` 目录。
- Tabby 功能对照见 [`docs/tabby-parity.md`](docs/tabby-parity.md)；
  主题下载/自定义见 [`docs/themes.md`](docs/themes.md)，Tabby 主题机制调研见 [`docs/tabby-themes.md`](docs/tabby-themes.md)；
  度量/诊断/改进/门禁闭环见 [`docs/self-iteration.md`](docs/self-iteration.md)。

## 日常使用

```bash
./scripts/install.sh          # 装到 ~/.local（可执行文件 + 桌面项 + 图标）
rusttabby-app                 # 直接启动
```

| 操作 | 方式 |
|---|---|
| 新建 / 关闭标签 | `Ctrl+T` / `Ctrl+W`，或标签栏 `+` / `×` |
| 分屏 | 右键 pane → Split Right / Split Down |
| 关闭 pane | 右键 → Close Pane；关掉最后一个 pane 退出应用 |
| 复制 | 鼠标拖选（松手自动复制）或 `Ctrl+Shift+C`，或右键 → Copy |
| 粘贴 | `Ctrl+Shift+V` 或右键 → Paste（多行会按终端 bracketed-paste 规则包起来） |
| 搜索 | `Ctrl+F`，输入即高亮下一个；`Enter` 下一个，`Shift+Enter` 上一个，`Esc` 关闭 |
| 中文输入 | fcitx5 / ibus（X11 XIM）：拼音候选窗跟随光标；组合中的回车/退格只作用于输入法，不会写入 shell |
| 回滚历史 | 鼠标滚轮 |
| 窗口置顶 | 右键标签栏空白处 → Always on Top |
| 调整分屏 | 拖动 pane 之间的分隔线（左右/上下比例都可调） |
| 缩放窗口 | 拖动窗口四边/四角（无边框窗口自绘命中区） |
| 设置 | 标签栏 ⚙：主题 / 字号 / 回滚行数 |
| 移动窗口 | 拖标签栏；双击标签栏最大化/还原 |

配置：`~/.config/rusttabby/config.toml`（`RUSTTABBY_CONFIG` 可覆盖）。

默认 `theme = "auto"`：跟随系统浅色/深色（深色 → Yaru Dark，浅色 → Yaru Light），也可以显式选任意主题。
把现成配色文件丢进 `~/.config/rusttabby/themes/` 即可在设置面板里选择，支持 iTerm2 `.itermcolors`、
Windows Terminal `.json`、Alacritty `.toml`、kitty `.conf`、Xresources，以及 Catppuccin/Rosé Pine 的 YAML 片段；
详见 [`docs/themes.md`](docs/themes.md)。

## 图标与桌面项

```bash
# 安装桌面项与图标（hicolor 目录）
install -Dm644 packaging/rusttabby.desktop ~/.local/share/applications/rusttabby.desktop
install -Dm644 assets/icon.png ~/.local/share/icons/hicolor/256x256/apps/rusttabby.png
update-desktop-database ~/.local/share/applications 2>/dev/null || true
```

- 应用内：标签栏左侧品牌图标、设置面板标题图标（`assets/icon-small.png`）。
- 窗口/任务栏图标：`_NET_WM_ICON` 由 Rust 侧直接写入（`assets/icon-64.rgba`），
  因为 Slint 内置 `Window.icon` 在本环境下会写成空属性。

## 已知限制

- 渲染目前是 CPU 光栅化 + 整帧上传；GPU/Skia 路径的实测数据待真机补齐。
- tab 不能拖拽排序；分屏已有分隔条拖拽与窗口拖边缩放。
- 分屏比例、窗口位置/大小都在应用层维护，Wayland 下窗口 `set_position` 可能被合成器忽略。
- 字体固定 JetBrains Mono（回退 DejaVu）；中文自动回退 WenQuanYi Micro Hei / Droid / Noto CJK，
  暂不支持自选字体、连字，emoji 仍留空。
- IME 走 winit 的 XIM（fcitx5 / ibus）；Wayland、GTK/Qt 的 IM module、macOS 输入法尚未验证。
- 主题跟随系统依赖 GNOME `gsettings`；非 GNOME 环境会回退 `gtk-theme` / `COLORFGBG` 做一次性检测。
- 自定义主题仍在启动时扫描，改主题文件后需要重启（热重载尚未实现）。
