# Tervo

用 Rust + Slint 写的原生终端模拟器，目标是提供 Tabby（Electron）的常用工作流，
同时显著降低内存与输入延迟。

> **独立项目声明**：Tervo 是独立开源项目，与 Tabby / Terminus 的作者及项目**无任何隶属、
> 赞助或背书关系**。“Tabby”是其权利人的商标，本项目仅作功能对比与兼容性说明时提及。
> 本项目不含 Tabby 的代码或美术资源；图标为原创几何标记（生成脚本见 `scripts/gen_icon.py`）。

> 当前进度：Phase 0（可行性验证）已完成（[报告](docs/phase0-report.md)）；
> Phase 2 常用工作流已可用：标签页/分屏/右键四模式、多窗口、Tab 活动/OSC 通知、
> 可编辑快捷键、Profile 搜索/默认/复制/每 Profile 外观、会话恢复、Tab 自动收缩/搜索、
> Vault 加密凭据、Tabby 配置导入、外部进程插件 V1（命令/状态/主题包）。
> 功能覆盖与已知差距见 [`docs/FEATURE_MATRIX.md`](docs/FEATURE_MATRIX.md) 与
> [`docs/CLOSURE.md`](docs/CLOSURE.md)；W-02 分支验证：app **351** / core 单元 **108**
> + smoke **25** 单测，Xvfb :139 SSH/SFTP/Vault e2e PASS（19 张截图）。

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
| `docs/` | Phase 0 报告、UI/视觉规范（`UI_DESIGN.md`）、Tabby 功能对照与主题机制调研 |

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

# Profile 外观 + 会话恢复 e2e（Xvfb :136；需要先 cargo build 出 tervo/xwin/xclick/xkey）
./scripts/e2e_profile_session.sh

# Phase 0 全套指标
./scripts/bench.sh
./scripts/phase0.sh
```

命令行参数：`--cols --rows --scrollback --font-size --shell --latency N --fps SECS --exit-after-first`；`--version`/`-V` 打印版本、git commit 与构建时间。

## 打包与安装

`scripts/package.sh` 产出可复现的发布包（同 commit、同工具链、clean worktree
下重复执行字节一致）：

| 产物 | 说明 |
|---|---|
| `dist/tervo-stripped` | strip 后的发布二进制（`dist/tervo` 为兼容软链） |
| `dist/tervo-stripped.sha256` / `dist/SHA256SUMS` | SHA-256 校验和 |
| `dist/tervo-<version>-linux-<arch>.tar.gz` | 二进制 + `install.sh` + LICENSE/README/THIRD_PARTY_NOTICES/LICENSING + desktop/图标/BUILD-INFO |
| `dist/tervo_<version>-1_<arch>.deb` | 含 Depends/desktop/图标/文档，可直接 `dpkg-deb -x` 或 `apt install ./…deb` |

`target/release/tervo` 仍保留符号供 `perf`/flamegraph；`scripts/package.sh` 只
strip 输出目录里的副本，`target/release` 原样保留。

版本号、tag、CHANGELOG 与发布流程见 `docs/RELEASING.md`；变更记录见 `CHANGELOG.md`。

Agent 工作时间与开发规则见 `AGENTS.md`（大任务 18:00 后）。

```bash
./scripts/package.sh                            # → dist/（默认 40 MiB 门禁）
TERVO_DIST=/tmp/dist ./scripts/package.sh       # 自定义输出目录（相对仓库根）
./scripts/package.sh /tmp/dist                  # 同上，位置参数
SKIP_BUILD=1 ./scripts/package.sh               # 复用已有 release 二进制
SKIP_DEB=1 ./scripts/package.sh                 # 只出 tar.gz/二进制
MAX_MB=64 ./scripts/package.sh                  # 自定义体积门禁
ALLOW_DIRTY=1 ./scripts/package.sh              # 允许未提交修改（产物标 dirty）

PREFIX=/tmp/prefix ./scripts/install.sh         # 装到自定义 prefix（验证用）
./scripts/install.sh --no-strip                 # 调试时安装带符号的副本
$PREFIX/lib/tervo/install.sh --rollback --prefix $PREFIX
$PREFIX/lib/tervo/install.sh --uninstall --prefix $PREFIX
```

安装/升级只替换程序文件，配置与 Profile 不动；升级自动保留
`$PREFIX/lib/tervo/previous/tervo`，`--rollback` 可来回切换；
`--uninstall` 保留 `~/.config/tervo`。版本信息：

```bash
./dist/tervo-stripped --version
# Tervo 0.3.1
# commit:  433056b（以 dist/tervo-stripped --version 为准）
# built:   2026-11-10
# target:  x86_64-unknown-linux-gnu
```

验证产物与安装/升级/回滚/卸载：

```bash
./scripts/test_packaging.sh                     # 打包 + 干净 prefix 安装 + Xvfb smoke + 升级/回滚/卸载
ls -l dist/ && file dist/tervo-stripped && ./dist/tervo-stripped --help
```

可复现细节、安装路径、迁移备份、`.deb` 字段、测试覆盖与**未完成项**
（自动更新/崩溃报告/AppImage/签名确实没有实现）见
[`packaging/README.md`](packaging/README.md)。

## 已实现

- 无边框窗口 + 自绘标签栏（新建 / 切换 / 关闭 / OSC 标题）。
- Tab 级 Pin：右键 Tab → Pin Tab / Unpin Tab（action 6，旧菜单 ID 不变）；pinned Tab 固定在最左
  pinned 区、显示小图钉角标，Pin 追加到最后一个 pinned 后，Unpin 回到第一个 unpinned 前；
  pinned/unpinned 不因普通排序跨组；`session.json` additive 保存 `pinned`，重启/会话恢复保留。
- Tab 拖拽：按住移动超过 6px 进入拖拽态，半透明 ghost 跟随鼠标；目标槽位显示 2px accent
  插入线并让后续 Tab 平滑让位；pinned 跨组时插入线不亮、松手不移动；Esc/窗口失焦取消并复位。
- 右键菜单分屏（左右 / 上下）、关闭 pane、焦点切换、窗口缩放重排。
- 每个 pane 独立 shell；关闭时自动回收 PTY 与线程。
- 无边框窗口自带 `─` 最小化、`□/❐` 最大化/还原、`✕` 关闭；拖标签栏移动窗口，双击标题栏最大化。
- 快捷键：`Ctrl+T` 新建标签、`Ctrl+Shift+N` 新建窗口、`Ctrl+W` 关闭标签。
- 终端链接：`Ctrl+左键点击` 打开 `http` / `https` / `file` 链接（OSC 8 优先）；右键链接所在格出现
  `Open Link` / `Copy Link`，`xdg-open` 失败会在底部浮层提示并写日志。
- 多窗口（MVP）：`Ctrl+Shift+N` / 标签栏右键 → New Window / 命令面板 `New Window`
  会拉起一个独立的 Tervo 进程，各自拥有 Tab/PTY；窗口互不影响，Tab 暂不能跨窗口移动。
- 可见滚动条：右侧 overlay，支持 `scrollbar_policy = "always" | "auto" | "never"`；
  显示当前回滚位置，支持点击/拖动跳转（core 暴露非阻塞
  `PaneHandle::scroll_position()` / `scroll_to()` / `scroll_to_bottom()`）。
- 视觉铃 + 可听铃：活动 pane 收到 BEL 时闪 2px accent 边框（`visual_bell = true`）；
  可听铃默认关闭，开启后 BEL 调 X11 Bell（`audible_bell = true`），两者独立；
  非活动 Tab 仍走未读点/桌面通知，不遮挡终端内容。
- 滚动策略：`scroll_on_output = false`（上滚时不抢视图，默认）、
  `scroll_on_keystroke = true`（按键回到底部）；`scrollback_unlimited = true`
  使用 100,000 行安全上限（预分配 ≈192 MB/新 pane，设置页有提示）。
- Backspace / Delete 编码可配：`backspace_binding = "ascii-delete" | "control-h"`、
  `delete_binding = "delete-sequence" | "ascii-del" | "control-h"`，默认 GNOME 值；
  只影响写入 PTY 的字节，设置即时生效。
- 文件拖放（X11）：从文件管理器拖文件/目录到 pane，插入 shell 转义后的路径
  （bracketed paste + 末尾空格）；Slint `DropArea` 已接入，winit 1.18 后端不投递外部拖放，
  由 winit `HoveredFile`/`DroppedFile` 原生桥接补齐（详见 `docs/terminal-ui-basics.md`）。
- 设置页（⚙）：默认是主窗口内的 **Settings Tab**——占满内容区、不创建 PTY、不进 workspace，
  标签栏有可关闭的齿轮 Tab；`Esc`、标签 `×`、点任意终端 Tab 都能退出。
  左侧按 PERSONALIZE / WORKSPACE / SYSTEM 分组的 8 个分类（Appearance / Terminal / Window /
  Keys / Profiles / Plugins / Vault / Advanced），每类一个低对比度线性图标，选中态用主题 accent。
  主题用下拉框，光标/滚动条/按键编码用分段选择器，开关/滑杆/步进器替代平铺 chip；顶部搜索的
  结果会跳到对应分类并滚动/高亮具体设置卡片。兼容性回退：启动时设 `TERVO_SETTINGS_LEGACY=1`
  可回到居中旧 modal（同一套控件，仅换呈现）。
- 界面语言（i18n）：Settings → Appearance 顶部新增 **Language / 语言** 分段按钮
  （English / 中文），点击后整个 Settings 页和终端右键/TabBar 菜单立即切换，无需重启并写回
  `language = "en" | "zh"`。默认 `en`，未知值读入回退 `en`；旧 config 缺省兼容；
  外部修改 config.toml 时 `language` 也走热更新。搜索索引中英文都可命中，结果行跟随当前语言。
  除语言选项自身必须同时显示 English/中文外，`en` 页无中文；`zh` 页无整句英文
  （SFTP/Sixel/Kitty/JSON/URL 等专有名词保留）。
  12 套内置主题 + `Auto`（跟随系统深/浅色，默认 Yaru Dark/Light）、字号、回滚行数、
  滚动条策略、可听/视觉铃、滚动策略、Backspace/Delete 编码；
  也支持把 iTerm2 / Windows Terminal / Alacritty / kitty / Xresources 主题直接放进 `themes/` 目录。
- 外部进程插件 V1：`~/.config/tervo/plugins/<id>/plugin.toml`（`TERVO_PLUGINS` 可覆盖）。
  `runtime = "process"` 常驻 JSON Lines 插件（2s 握手、10s 命令超时、崩溃重启上限 2、
  坏 JSON 禁用）；命令进 Palette/快捷键，结果回底部浮层；Status Provider 每 2s 轮询并在
  底部显示一行状态，可点击执行命令；`[contributes] theme` 可带声明式 TOML 主题；
  `[permissions]` 仅声明/展示（无沙箱）。旧 oneshot 与旧扁平清单保持兼容。
  清单/协议/安全边界见 [`docs/PLUGIN_API.md`](docs/PLUGIN_API.md)。
- Profile 外观：Settings → Profiles → Edit/New 可为每个 Local/SSH/Serial/Telnet Profile
  设置 `font_size` / `theme` / `cursor_shape` / `cursor_blink` / `scrollback`；空 = 继承
  全局配置。打开该 Profile 的 Tab 时应用，优先级 **Profile 显式值 > 全局**：之后全局
  主题/字号/光标变化仍作用于未覆盖的 pane，Profile 覆盖值不会被后台改掉；主题只影响
  该 pane 的终端内容，不改应用 UI。`profiles.toml` v2 是 additive 字段，旧文件不用迁移；
  v1 仍自动补齐 `kind`。
- SSH/SFTP（W-02）：SSH Profile 支持 ProxyJump（`-J`）、本地/远程/动态转发
  （`-L/-R/-D`）、agent forwarding（`-A`）与 keyboard-interactive 兜底；连接前在后台
  做 known_hosts 预检，首次/密钥变更弹窗显示 SHA256 指纹并要求显式 Trust/Update（绝不
  静默信任），ProxyJump/预检失败回退终端确认。SSH 退出显示退出码/信号与 Reconnect；
  SFTP 面板支持上传/下载队列、进度、取消、失败 Retry、chmod、10s 连接超时/keepalive。
  Vault 的 Argon2id 解锁/初始化/改主密码在后台线程执行，设置页保持可重绘。
- 会话恢复：标签栏右键 → **Restart Session** 手动恢复；Settings → Behavior 的
  **Restore session on startup**（默认关）开启后，正常退出写快照，下次启动恢复
  Tab 顺序/标题/分屏树/ratio/焦点/窗口网格与每个 pane 的 cwd。local Profile 关联
  会保留；SSH/Serial/Telnet 出于安全不自动建连，回退普通 shell。快照损坏或恢复
  失败时回退新 Tab；不恢复 scrollback/屏幕内容/运行中进程。
- Tabby 功能对照见 [`docs/tabby-parity.md`](docs/tabby-parity.md)；
  主题下载/自定义见 [`docs/themes.md`](docs/themes.md)，Tabby 主题机制调研见 [`docs/tabby-themes.md`](docs/tabby-themes.md)；
  度量/诊断/改进/门禁闭环见 [`docs/self-iteration.md`](docs/self-iteration.md)；
  插件目标/运行时/能力类型/市场调研见 [`docs/PLUGIN_SYSTEM.md`](docs/PLUGIN_SYSTEM.md)；
  P0/P1/周末任务状态见 [`docs/TASK_BOARD.md`](docs/TASK_BOARD.md)；
  最终人工复测见 [`docs/RETEST_CHECKLIST.md`](docs/RETEST_CHECKLIST.md)。

## 日常使用

```bash
./scripts/install.sh          # 装到 ~/.local（strip 后安装；--no-strip 保留符号）
tervo                 # 直接启动
```

| 操作 | 方式 |
|---|---|
| 新建 / 关闭标签 | `Ctrl+T` / `Ctrl+W`，或标签栏 `+` / `×` |
| Pin / Unpin Tab | 右键 Tab → Pin Tab / Unpin Tab；pinned 固定在左侧，普通拖动不跨组 |
| 拖动排序 | 按住 Tab 移动：ghost + 2px 插入线 + 让位动画；松手落位，Esc/失焦取消 |
| 新建窗口 | `Ctrl+Shift+N`，或标签栏右键 → New Window（独立进程，关闭互不影响） |
| 分屏 | 右键 pane → Split Right / Split Down |
| 关闭 pane | 右键 → Close Pane；关掉最后一个 pane 退出应用 |
| 复制 | 鼠标拖选（松手自动复制，同时写系统剪贴板与 X11 PRIMARY）或 `Ctrl+Shift+C`，或右键 → Copy |
| 打开链接 | `Ctrl+左键点击` http/https/file 链接（OSC 8 优先）；右键 → Open Link / Copy Link |
| 粘贴 | `Ctrl+Shift+V` 或右键 → Paste（多行会按终端 bracketed-paste 规则包起来）；中键优先粘 X11 PRIMARY，为空/不可读时回退普通剪贴板 |
| 搜索 | `Ctrl+F`，输入即高亮下一个；`Enter` 下一个，`Shift+Enter` 上一个，`Esc` 关闭；搜索条内可切 `Match case` / `Match regex`（或 `Alt+C` / `Alt+R`），非法正则在条内报错 |
| 只读终端 | 右键 → `Read Only` 开关；开启后 pane 右上角显示 `READ ONLY`，键盘/粘贴/鼠标上报不写 PTY，鼠标选择/滚动仍可用；关闭即恢复 |
| 复位终端 | 右键 → `Reset` / `Reset and Clear`（命令面板同名）；会清屏并复位终端状态，不关闭 pane/会话 |
| 保存内容 | 右键 → `Save Contents…`；输入明确路径（支持 `~`，默认 `$HOME/tervo-save-<epoch>.txt`），保存回滚+视口全部文本，失败在弹窗内报错 |
| 中文输入 | fcitx5 / ibus（X11 XIM）：拼音候选窗跟随光标；组合中的回车/退格只作用于输入法，不会写入 shell |
| 回滚历史 | 鼠标滚轮；或点击/拖动右侧滚动条（仅历史超出视口时显示） |
| 拖放路径 | 从文件管理器拖文件/目录到 pane：插入 shell 转义路径 + 末尾空格（X11） |
| 窗口置顶 | 右键标签栏空白处 → Always on Top |
| 调整分屏 | 拖动 pane 之间的分隔线（左右/上下比例都可调） |
| 缩放窗口 | 拖动窗口四边/四角（无边框窗口自绘命中区） |
| 设置 | 标签栏 ⚙ 打开 Settings Tab：分组分类（Appearance/Terminal/Window/Keys/Profiles/Plugins/Vault/Advanced/Help）、主题下拉、搜索跳转；Esc / Tab × / 点终端 Tab 关闭。环境变量 `TERVO_SETTINGS_LEGACY=1` 回退居中 modal |
| 帮助 / 诊断 | Settings → Help（SYSTEM 组最后）：快捷键表、config/log 路径（打开目录）、复制诊断信息、版本/commit/构建时间/target、已知限制与 `docs/` 路径；全部双语并纳入 Settings 搜索 |
| 移动窗口 | 拖标签栏；双击标签栏最大化/还原 |

配置：`~/.config/tervo/config.toml`（`TERVO_CONFIG` 可覆盖）。

Profile 与启动恢复相关配置：`~/.config/tervo/profiles.toml`
（`TERVO_PROFILES` 可覆盖）。`restore_session_on_startup = false` 是默认值；
开启后只在正常退出写 `session.json`（`TERVO_SESSION` 可覆盖路径）。

```toml
# config.toml（Settings → Behavior 同款开关）
restore_session_on_startup = false

# profiles.toml：外观字段均可选，省略 = 继承全局
[[profiles]]
id = 1
name = "work"
kind = "local"
shell = "/bin/zsh"
args = ["-l"]
cwd = "/home/me/src"
env = [["FOO", "1"]]
font_size = 18.0
theme = "catppuccin-mocha"
cursor_shape = "underline"
cursor_blink = "off"
scrollback = 10000
```

剪贴板：拖选/Ctrl+Shift+C 会同时写系统剪贴板与 X11 PRIMARY（中键粘贴用）。
远端程序可通过 **OSC 52** 读写本机剪贴板，默认放行；不需要时在配置里关闭：

```toml
osc52 = "deny"   # 默认 "allow"；deny 同时拒绝写入和查询
```

光标与粗体（GNOME Terminal 兼容；颜色留空 = 跟随主题）：

```toml
cursor_shape = "block"        # block | underline | ibeam
cursor_blink = "system"       # system（当前等同 on）| on | off
cursor_color = "#ff5f56"      # 空 = 主题光标色
cursor_text_color = "#101010" # 空 = 主题背景色（块状光标反色）
bold_is_bright = true         # 粗体 ANSI 0–7 用亮色
bold_color = "#ff8800"        # 空 = 不启用
bold_color_same_as_fg = false # false 时 bold_color 作用于默认前景色粗体
```

背景图与终端透明度（设置页 Appearance → Background；图片字段支持 `~` 和 Browse 选择器）：

```toml
background_image = "~/Pictures/wallpaper.png"  # 空 = 关闭；PNG/JPEG/SVG
background_opacity = 0.35                       # 0 = 完全透明，1 = 主题纯色
background_dim = 0.6                            # 只压暗背景图
```

- 没有背景图也可以调低 `background_opacity`：启动时使用透明 visual，终端帧 alpha 直接透出桌面；
  需要桌面合成器（GNOME/KDE 等），无合成器的裸 X11 可能显示为黑底。运行时从 100% 降到半透明
  需要重启 Tervo 才切换窗口 visual（Slint 1.18 Linux 后端限制）。
- 设置页的 **Browse…** 调 zenity/kdialog（`TERVO_FILE_DIALOG` 可覆盖）；选择失败/取消不改变当前图片。
- Background Image/Cwd/Shell/SSH key 等本地路径支持 `~`；远端 SSH 路径不本地展开。

> 安全提示：`allow` 意味着任何能向终端输出转义序列的程序（包括 SSH 远端、
> `cat` 一个不可信文件、tmux 里的远端进程）都能覆盖或读取你的系统剪贴板/
> PRIMARY。只处理可信内容，或设为 `deny`。

终端基础行为也可以直接写配置（设置页 Terminal 分类有同样控件）：

```toml
audible_bell = false              # BEL 响系统铃（默认关）
visual_bell = true                # BEL 闪边框（默认开，与 audible_bell 独立）
scroll_on_output = false          # 上滚时不抢视图（默认）
scroll_on_keystroke = true        # 按键回到底部（默认，GNOME 行为）
scrollback_unlimited = false      # true = 100,000 行安全上限（≈192 MB/新 pane）
scrollbar_policy = "auto"         # always | auto | never
backspace_binding = "ascii-delete" # ascii-delete(0x7f) | control-h(0x08)
delete_binding = "delete-sequence" # delete-sequence(ESC[3~) | ascii-del | control-h
```

Sixel 与 iTerm2 IIP 内联图片默认开启（设置页 Terminal → Images 可分别关闭）：

```toml
sixel = true             # 默认；false 时不解析/渲染 Sixel，DA1 不再宣告参数 4，已有图清除
sixel_scrolling = true   # true = 图片跟随滚动历史；false = 固定在创建时的屏幕行
iip = true               # 默认；false 时不解析/渲染 iTerm2 IIP（OSC 1337 File），已有 IIP 图清除
```

- 支持 DCS `ESC P ... q ... ST`（含 BEL 终止、C0 分包、跨 PTY chunk）、RGB/HLS 调色板、
  透明像素、重复（`!Pn`）与 raster attributes；非法/超限数据只丢图，不崩溃。
- 上限：单条 Sixel 原始数据 25 MiB、单图 16 MP / 单边 16384 px；图片作为独立
  Slint Image overlay 画在终端帧之上，不进入 cell 文本/选区，也不改变终端帧重绘频率。
- Primary DA1 在开启 Sixel 时回 `CSI ?62;4;9;22c`（含 Sixel 参数 4，供 dsh-tui 等自动选择），
  关闭时回 alacritty 原生的 `CSI ?6c`；不宣告 Kitty。iTerm2 IIP 走 OSC 1337，不需要 DA1
  宣告，因此 DA1 完全不受 IIP 开关影响。
- iTerm2 IIP 支持 `OSC 1337;File=...:BASE64`（BEL / ST 终止）、`inline=1`、`name`、
  `width`/`height`（cell 数、`px`、`auto`）与 `preserveAspectRatio`；严格 base64 解码，
  PNG 用 `png`、JPEG 用轻量纯 Rust `zune-jpeg`。上限：单个文件 20 MiB、单图 16 MP /
  单边 16384 px；非法/超限只丢图，不崩溃，图片字节不进入文本/选区。
- 两种图片共用同一套 overlay/缓存/滚动/resize 机制；IIP 与 Sixel 独立开关、独立清除，
  切换 Tab 或窗口 resize 后按光标 cell 重新定位。
- PTY `winsize.pixel_width/height` 与 XTWINOPS `CSI 16t/14t` 会返回真实 cell/window
  像素尺寸，dsh-tui 等据此编码图片，不再退回 8×16 默认值导致低清。
- 超过 pane 可用区域的图片会等比缩到终端内（不放大、不撑出 pane）；点击图片打开灯箱，
  支持缩小/放大/Fit/Close 和 Esc 关闭，灯箱始终使用原始解码图。灯箱 Fit 只缩不放，
  100% 是原图像素 1:1，>100% 使用 pixelated，避免平滑插值糊。
- 已知限制：TUI 的 ECH 擦除会清掉内联图片（不做逐像素局部擦除；清屏/备用屏切换同样清图）；IIP/Sixel 图片
  覆盖在文本之上、不占用光标行（与 iTerm2 协议一致，客户端应自行换行留位）；不做动画/
  文件下载（`inline=0` 或缺少 `inline=1` 的文件被忽略）；scrollback 淘汰最旧行后，锚点
  可能偏移；Sixel 没有部分透明；仅在 Xvfb/llvmpipe 验证。

详见 [`docs/terminal-behavior.md`](docs/terminal-behavior.md)。

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
- 文件拖放的原生桥接目前只覆盖 X11/winit：Slint 1.18.1 的 winit 后端不会把
  `HoveredFile`/`DroppedFile` 转成 `DataTransfer`，因此 DropArea 单独在 winit 下收不到
  外部拖放；Wayland 下暂不保证。视觉铃尚未跟随系统 reduced motion。
- 分屏比例、窗口位置/大小都在应用层维护，Wayland 下窗口 `set_position` 可能被合成器忽略。
- 字体固定 JetBrains Mono（回退 DejaVu）；中文自动回退 WenQuanYi Micro Hei / Droid / Noto CJK，
  彩色 emoji 默认开启（Noto Color Emoji CBDT/CBLC/sbix，`TERVO_COLOR_EMOJI=0` 可关），
  暂不支持自选字体、连字，COLR/SVG 彩色 emoji 仍会回落单色/占位。
- IME 走 winit 的 XIM（fcitx5 / ibus）；Wayland、GTK/Qt 的 IM module、macOS 输入法尚未验证。
- 主题跟随系统依赖 GNOME `gsettings`；非 GNOME 环境会回退 `gtk-theme` / `COLORFGBG` 做一次性检测。
- 自定义主题仍在启动时扫描，改主题文件后需要重启（热重载尚未实现）。
- 设置页目前不含字体族/行距设置；搜索跳转定位到设置卡片而不是单个控件，键盘焦点遍历（Tab/方向键）
  仍以鼠标点击/搜索为主。旧 modal 回退是同一套控件换呈现，并非保留 2026-11 之前的旧实现。
- 会话快照只恢复布局/标题/cwd/Profile 关联和 Tab pinned，不恢复滚动历史、屏幕内容或运行中进程；
  自动恢复只在正常退出时写快照，崩溃/被 SIGKILL 不保证，也没有崩溃恢复守护。
  `session.json` 全局只有一份，多窗口布局不能分别恢复；`--window` 子窗口不写快照。
- SSH/Serial/Telnet Profile 不会随会话恢复自动重连（需要用户主动打开，避免启动即连远端）；
  恢复为普通 shell 并保留 Tab 标题/布局/cwd。
- Tab pin 是“固定到左侧 pinned 区”，不是窗口置顶；pinned 组内顺序仍可拖动，但不能跨到
  unpinned 组；拖出窗口/跨窗口移动未实现。
- Profile 外观覆盖是 per-pane 的“pin”：Profile 显式设置后，全局字号/主题/光标改动不会
  刷新该 pane；清空 Profile 对应字段后新开 Tab 即回到继承。
- 打包/发布：自动更新、崩溃报告/上传、AppImage/Flatpak/Snap、发布签名与 APT 仓库
  本轮均未实现，也没有对应按钮；升级靠新 tar 的 `install.sh` 或新 `.deb`，回滚用
  `install.sh --rollback`。详见 [`packaging/README.md`](packaging/README.md)。

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
