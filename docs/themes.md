# 主题：内置、自定义与直接下载现成配色

## 1. 内置主题

设置面板（⚙）里可选 12 套，终端 16 色 + 标签栏/菜单/设置面板/搜索框同步换色：

| id | 名称 | 深浅 |
|---|---|---|
| `yaru-dark` / `yaru-light` | Yaru（Ubuntu/GNOME 系统风） | 深 / 浅 |
| `tokyo-night` | Tokyo Night | 深 |
| `catppuccin-mocha` / `-macchiato` / `-frappe` / `-latte` | Catppuccin 4 变体 | 深×3 / 浅 |
| `dracula` | Dracula | 深 |
| `nord` | Nord | 深 |
| `gruvbox-dark` | Gruvbox | 深 |
| `one-dark` | One Dark | 深 |
| `solarized-light` | Solarized | 浅 |

默认配置是 `theme = "auto"`：跟随系统深浅色，深色用 **Yaru Dark**，浅色用 **Yaru Light**；
也可以把 `auto_dark` / `auto_light` 指向任意主题（内置或自定义）。

```toml
# ~/.config/xtervo/config.toml
theme = "auto"
auto_dark = "custom:my-dark"    # 可选；省略 = Yaru Dark
auto_light = "custom:my-light"  # 可选；省略时优先用 auto_dark 的 pair，再退回 Yaru Light
```

## 2. 自定义主题目录

把主题文件放进 `~/.config/xtervo/themes/`（`XTERVO_THEMES_DIR` 可覆盖目录），重启应用生效。
**一个文件 = 一个主题**；id 由文件名生成：`custom:<slug>`，slug = 小写、空格/符号折成 `-`。
例如 `Dracula.itermcolors` → `theme = "custom:dracula"`。

支持直接使用这些现成格式（不需要转换、不需要手写配置）：

| 后缀 | 来源 / 格式 | 说明 |
|---|---|---|
| `.itermcolors` | iTerm2 配色（XML plist） | 最通用，iTerm2-Color-Schemes 有 200+ 套 |
| `.json` | Windows Terminal `schemes` | 单个 scheme 对象或 `schemes` 数组（取第一个） |
| `.conf` / `.config` / `.kitty` | kitty 配色 | `foreground` / `background` / `cursor` / `color0..15` |
| `.Xresources` / `.txt` / 无后缀 | Xresources | `*.foreground` / `*.background` / `*.color0..15`，Tabby 社区配色同格式 |
| `.toml` | Alacritty TOML | 含 `[colors.primary]` / `[colors.normal]` / `[colors.bright]` |
| `.yaml` / `.yml` | 简单配色 YAML | `name` / `foreground` / `background` / `cursor` / `colors:` 16 项列表（Catppuccin、Rosé Pine 的 Tabby 片段） |
| `.toml` | xtervo 完整主题 | 可精确控制 11 个界面颜色 + 声明 `pair` 深浅配对 |

导入格式只带终端 16 色 + 前景/背景/光标；**界面 chrome 颜色由应用自动推导**
（背景分层、边框、accent 取蓝色/青色、危险色取红色），不会出现“终端换了、标签栏没换”。

目录里的 README、截图、隐藏文件会被忽略；解析失败的文件只打一行 `[theme] <路径>: <原因>`，
不影响其它主题和启动。id 冲突时保留排序靠前的文件。

## 3. 直接下载现成主题

### iTerm2-Color-Schemes（推荐，格式最全）

<https://github.com/mbadolato/iTerm2-Color-Schemes>

```bash
mkdir -p ~/.config/xtervo/themes

# 单个 iTerm2 主题
curl -L -o ~/.config/xtervo/themes/Dracula.itermcolors \
  https://raw.githubusercontent.com/mbadolato/iTerm2-Color-Schemes/master/schemes/Dracula.itermcolors

# 批量：把整个 kitty 目录拿下来（每个 .conf 一套）
git clone --depth 1 https://github.com/mbadolato/iTerm2-Color-Schemes /tmp/iterm2-themes
cp /tmp/iterm2-themes/kitty/*.conf ~/.config/xtervo/themes/
# 也可以把 windowsterminal/*.json、xresources/*、alacritty/*.toml 复制进去
```

### Catppuccin / Rosé Pine（Tabby 配色片段）

这些仓库提供的是 Tabby 的 YAML 片段（`name` + `foreground` + `background` + `cursor` + `colors`），
直接把单个 `.yaml` / `.yml` 放进主题目录即可（不要把整个 Tabby `config.yaml` 放进来）：

```bash
curl -L -o ~/.config/xtervo/themes/catppuccin-mocha.yaml \
  https://raw.githubusercontent.com/catppuccin/tabby/main/themes/catppuccin-mocha.yaml
```

- <https://github.com/catppuccin/tabby>
- <https://github.com/rose-pine/tabby>

### Gogh / 其它主题站

Gogh 的安装脚本不是主题文件；从它的仓库里拿单个 `.itermcolors`、`.conf` 或 `.Xresources`
放进主题目录即可。<https://github.com/Gogh-Co/Gogh>

## 4. xtervo 完整主题格式（可选，控制界面颜色）

只有需要精确控制标签栏/菜单颜色、或声明深浅 `pair` 时才需要手写。完整示例：

```toml
schema = 1
name = "My Dark"          # 设置面板显示名
variant = "dark"          # dark | light
pair = "my-light"         # 可选：同目录 my-light.toml，必须 variant 相反且互相指向

foreground = "#cdd6f4"
background = "#1e1e2e"
cursor = "#f5e0dc"
# 可选：bright_foreground / dim_foreground
ansi = [
  "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
  "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
]

[ui]
bg = "#1e1e2e"
surface = "#181825"
raise = "#313244"
hover = "#45475a"
border = "#585b70"
text = "#cdd6f4"
subtext = "#a6adc8"
muted = "#6c7086"
accent = "#89b4fa"
accent-text = "#11111b"
danger = "#f38ba8"
```

校验：`schema` 必须为 1；`ansi` 恰好 16 项；颜色必须是 `#RRGGBB`；`[ui]` 11 个键缺一不可；
未知字段会拒绝（防止拼写错误静默失效）。`pair` 不合法只清空配对，主题仍可用。

## 5. 限制与排查

- **修改主题文件后需要重启应用**（当前还没有热重载）。
- Windows Terminal 一个 JSON 里有多套 `schemes` 时只取第一套；要全用就拆成多个文件。
- YAML 只支持简单配色片段；完整的 Tabby `config.yaml` 请先取出 `colorScheme` 部分。
- 应用启动时若主题文件有错，会在 stderr 打 `[theme] ...`；主题目录默认在
  `~/.config/xtervo/themes/`，也可用 `XTERVO_THEMES_DIR` 指向别处。
