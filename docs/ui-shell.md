# UI Shell：标签页 / 分屏 / 右键菜单

> 状态：核心交互已跑通（Phase 1 功能线第一轮）。性能优化（帧时钟、脏带上传）暂停。

## 1. 布局模型

```
Workspace
 ├─ tabs: Vec<Tab>
 │    ├─ title         ← OSC 0/2 标题（无则显示 shell 名）
 │    ├─ root: Node     ← 分屏树
 │    └─ focused       ← 该 tab 内键盘归属
 └─ active: usize

Node = Leaf(PaneId)
     | Split { dir: Row|Col, ratio: 0.5, a, b }
```

- 布局在 Rust 侧计算：`Node::layout(rect)` 遍历树产出**扁平矩形列表**（`PaneId + Rect`），
  Slint 用 `for p in root.panes` 渲染，避免递归组件。
- 每个 `PaneRuntime` 持有独立的 `PaneHandle`（自带 actor 线程 + PTY）、`Renderer`、帧缓冲和 `Slint Image`。
- 关闭 pane / tab 时直接 `drop(PaneHandle)`：子进程被 kill、actor 收到 Shutdown、PTY 读写线程自然退出。

## 2. 窗口外壳

- `no-frame: true`：去掉原生标题栏（截图验证：窗口顶部直接是标签栏）。
- 标签栏左侧是 tab 列表；右侧固定三个窗口按钮：`─` 最小化、`□/❐` 最大化/还原、`✕` 关闭。
- 最大化状态用 Slint 内置的 `Window.maximized` 属性（与窗口状态双向同步），按钮图标随之切换。
- 标签栏空白区域覆盖 `WindowMoveArea`：按住拖动移动窗口；其内部再套一个 `TouchArea`，
  双击标题栏 = 最大化/还原（`WindowMoveArea` 用 `input_event_filter_before_children` 先判定拖动阈值，
  所以「拖动移动」与「双击」可以共存）。
- 最大化后窗口尺寸变化会经 `resized` 回调重排所有 pane 并触发 `Term::resize`（实测 900x602 → 2560x1413 正常）。
- 窗口四边/四角有自绘 `ResizeHandle` 命中区，按下后按 root 指针位移直接 `set_size`/`set_position`；
  不依赖 WM 的 `_NET_WM_MOVERESIZE`（Mutter 对 XTest 合成事件不支持）。

## 3. 交互清单

| 操作 | 行为 |
|---|---|
| 点击 `+` / `Ctrl+Shift+T` | 新建标签页（独立 shell） |
| 点击标签 | 切换标签页 |
| 点击标签上的 `×` / `Ctrl+Shift+W` | 关闭标签页；关掉最后一个标签 → 退出应用 |
| 点击 `─` | 最小化窗口 |
| 点击 `□` / `❐` | 最大化 / 还原窗口 |
| 双击标签栏空白处 | 最大化 / 还原窗口 |
| 左键点 pane | 聚焦该 pane（不再画整圈外框） |
| 右键 pane | 弹出菜单：Split Right / Split Down / Close Pane |
| Split Right | 当前 pane 左右对半分，新 pane 获得焦点 |
| Split Down | 当前 pane 上下对半分 |
| Close Pane | 关闭当前 pane，兄弟节点自动填满；关掉最后一个 → 退出应用 |
| 拖动 pane 之间的分隔线 | 调整左右/上下分屏比例（夹在 10%–90%），实时重排并 `Term::resize` |
| 拖动窗口四边/四角 | 缩放窗口（左上/上/右上/右/右下/下/左下/左 八个方向） |
| 窗口缩放 | `resized` 回调重算布局，必要时对每个 pane 发 `Term::resize` |

## 4. 设置

- 标签栏右上角 ⚙ 打开设置面板（半透明遮罩 + 居中卡片，点遮罩关闭）。
- 三个可调项，全部即时写回 `~/.config/tervo/config.toml`（`TERVO_CONFIG` 可覆盖路径）：
  - **主题**：12 套内置主题（默认 Auto）；**Auto（跟随系统）**深色映射 Yaru Dark、浅色映射 Yaru Light，
    选择后后台监听 `gsettings monitor org.gnome.desktop.interface color-scheme`，系统切换深浅色时自动重绘；
    也可用配置里的 `auto_dark` / `auto_light` 指定任意主题。改为具体主题即停止跟随。
    `~/.config/tervo/themes/` 里的 iTerm2 / Windows Terminal / Alacritty / kitty / Xresources / YAML
    主题会追加到列表（界面 chrome 颜色由终端配色自动推导）。两者都会向所有 pane 的 actor 发 `SetTheme`，
    强制整屏重取并重绘。
  - **字号**：重建字体集与每个 pane 的渲染器，按新单元尺寸重排网格（`Term::resize`）。
  - **回滚行数**：保存到配置，对新开 pane 生效（历史缓冲是构造时预分配的）。
- 启动时读取配置：主题 / 字号 / 回滚 / shell / 初始窗口列数行数。

## 5. 品牌图标

- 资源：`assets/icon.png`（512，打包用）、`icon-small.png`（64，界面内）、`icon-64.rgba`（裸 RGBA，写 X11 图标用）。
- 界面：标签栏最左侧 18px 品牌位；设置面板标题左侧 22px。
- 窗口/任务栏：Slint 的 `Window.icon` 在本环境写出的 `_NET_WM_ICON` 为空，因此应用启动后由
  `xconn::set_window_icon()` 直接写 `_NET_WM_ICON`（ARGB CARDINAL，64×64）。
  定位窗口用 `_NET_WM_PID == getpid()`，避免多实例时写错窗口。
- 桌面项：`packaging/tervo.desktop`（`Icon=tervo`，安装说明见 README）。

## 6. 已知限制（下一轮）

1. 分屏分隔线可拖动；焦点 pane 不再画整圈外框，改用 1px 分隔线区分。
2. tab 不能拖拽排序、不能重命名。
3. 窗口四边/四角可拖动缩放（应用层 set_size/set_position）；最小化后需从任务栏/概览恢复。
4. 关掉 pane 后焦点转移到第一个 pane，而非相邻 pane。
5. 字体固定 JetBrains Mono；中文靠 WenQuanYi Micro Hei / Droid / Noto CJK 回退（启动多 ~150 ms），
   emoji 无彩色字形，自选字体/连字未做。
6. 每个 pane 一个 1MB 字形 atlas（未共享）。
7. 设置面板仍是主题 / 字号 / 回滚三项；UI chrome 配色现已随主题同步换色。
8. Auto 依赖 GNOME `gsettings`：非 GNOME 环境回退 `gtk-theme` / `COLORFGBG`，都取不到时按深色处理；
   `gsettings monitor` 不可用时只在启动/切换时检测一次，不会实时跟随。
9. IME 目前只在 X11 + XIM（fcitx5 / ibus）验证过；Wayland 与 macOS 需另行接入。

## 7. 验证记录

- 截图：`.logs/nf.png`（无边框 + 3 pane）、`.logs/m1.png`（右键菜单）、`.logs/m2.png`（Split Down 后 4 pane）、`.logs/t4.png`（新建 tab）。
- 分屏：`.logs/split_before.png`（仅 1px 分隔线、无外框）、`.logs/split_after.png`（拖动分隔条后比例变化）。
- 主题：`.logs/yaru_main.png`（默认 Auto → Yaru Dark）、`.logs/yaru_settings.png`（12 套主题网格）、
  `.logs/imported_settings4.png`（直接放入的 Dracula iTerm2/WT/kitty/Xresources 主题）。
- 中文：`.logs/cjk2.png`（你好世界）、`.logs/bold.png`（粗体 + ANSI 色）、`scripts/check_ime.sh`（XIM 上屏冒烟）。
- 自动化：`cargo test -p tervo`（36 个，含 CJK 宽字符/回退、主题检测、6 种导入格式）、
  `cargo test -p tervo-core`（15 单测 + 9 集成 + 10 VT 语义契约）；`./scripts/selfcheck.sh` 一键复跑并记录基线。
- RSS（120×32，软件后端）：1 pane 49.1 MB / 3 pane 44.5 MB（多次测量在 42–49 MB 波动），子进程数 1 / 3 符合预期。
- 启动：软件后端 + CJK 回退字体，READY 约 200 ms（字体加载约 160 ms）。
