# Tabby 主题机制与 rusttabby 自定义主题方案

> 实现更新：rusttabby 现在可以直接加载 iTerm2 / Windows Terminal / Alacritty / kitty /
> Xresources / 简单 YAML 主题，使用说明见 [`themes.md`](themes.md)；本文第 3 节是最初的
> rusttabby TOML 方案设计（已落地为“完整主题格式”，并增加了多格式导入与自动 UI 配色）。

> 调研对象：Eugeny/tabby `master` 分支（2026-11 抓取）。标记约定：**[官方]** 官方 wiki / 源码 / typedoc；**[社区]** 第三方主题仓库或文章；**[推断]** 由实现推断，官方文档未明说。

## 1. Tabby 的主题分层

Tabby 把「界面外观」和「终端配色」拆成两套独立机制，另有插件与 customCSS 两个扩展口。配置文件是 YAML：Linux `~/.config/tabby/config.yaml`，macOS `~/Library/Application Support/tabby`，Windows `%APPDATA%/Tabby`（[官方 wiki](https://github.com/Eugeny/tabby/wiki/Config-file)；源码用 Electron `userData` + `TABBY_CONFIG_DIRECTORY` 覆盖目录，并写 `config.yaml.backup`，见 [app/lib/index.ts](https://github.com/Eugeny/tabby/blob/master/app/lib/index.ts)、[app/lib/config.ts](https://github.com/Eugeny/tabby/blob/master/app/lib/config.ts)）。核心键如下：

| 层 | 配置键 | 作用 |
|---|---|---|
| UI 主题 | `appearance.theme` | 整个应用 chrome 的 CSS；值为主题 `name`，内置默认 `Follow the color scheme`，插件主题填其名字（如 `Hype`） |
| 终端配色 | `terminal.colorScheme` / `terminal.lightColorScheme` | 深/浅两套 16 色 + fg/bg/cursor；**存完整对象，不是字符串 id** |
| 用户配色 | `terminal.customColorSchemes[]` | 用户保存/粘贴的完整配色对象数组，选择时复制进 `colorScheme` |
| 自定义 CSS | `appearance.css` | 注入渲染进程的原始 CSS；设置界面标签叫 “Custom CSS”，YAML 键名是 `css`（当前 master 没有 `customCSS` 键） |
| 主题插件 | npm 包 | UI 主题提供 `Theme` 服务；配色插件提供 `TerminalColorSchemeProvider` |

注意 `colorScheme` 不在 `appearance` 下，而在 `terminal` 下；`appearance` 下与主题相关的是 `theme`、`css`、`colorSchemeMode`。

```yaml
appearance:
  theme: Follow the color scheme      # UI 主题名；插件主题如 Hype
  css: |                              # ← 真正的键名是 css，不是 customCSS
    .tab-bar { background: #000; }
  colorSchemeMode: auto               # auto | dark | light（终端配色用）
terminal:
  colorScheme:                        # 完整对象，不是字符串 id
    name: Catppuccin Mocha
    foreground: '#cdd6f4'
    background: '#1e1e2e'
    cursor: '#f5e0dc'
    selection: null
    colors: ['#45475a', '#f38ba8', '#a6e3a1', '#f9e2af', '#89b4fa', '#f5c2e7', '#94e2d5', '#bac2de',
             '#585b70', '#f38ba8', '#a6e3a1', '#f9e2af', '#89b4fa', '#f5c2e7', '#94e2d5', '#a6adc8']
  lightColorScheme:                   # 同结构，auto 的浅色侧
    name: Catppuccin Latte
    # foreground/background/cursor/colors 同上
  customColorSchemes: []              # Catppuccin / Rosé Pine 的 YAML 粘到这里
```

Tabby 启动时读一次 `config.yaml`，源码中未见对它的文件监听；外部直接改文件通常需重启，设置 → Config file 的 “Save and apply” 会走 `save()+load()+emitChange()`（[源码](https://github.com/Eugeny/tabby/blob/master/tabby-core/src/services/config.service.ts)，外部监听部分为 **[推断]**）。

数据结构（[`api/theme.ts`](https://github.com/Eugeny/tabby/blob/master/tabby-core/src/api/theme.ts)，[官方 typedoc](https://docs.tabby.sh/interfaces/TerminalColorScheme.html)）：

```ts
interface TerminalColorScheme { name: string; foreground: string; background: string; cursor: string;
                                colors: string[] /* 16 项 */; selection?: string; selectionForeground?: string; cursorAccent?: string }
abstract class Theme { name: string; css: string; terminalBackground: string; followsColorScheme?: boolean }  // css 是整份全局样式表
```

**主题插件如何打包**（[Hype](https://github.com/Eugeny/tabby-theme-hype)、[Noctis](https://github.com/aaronhuggins/tabby-theme-noctis) 两个实例）：npm 包 `main: dist/index.js`，关键字 `tabby-plugin`（旧包用 `terminus-plugin`），peer 依赖 `tabby-core`/`tabby-terminal`，webpack 把 SCSS 编成字符串；模块 default export 一个 NgModule，按类型 multi-provide：

```ts
@Injectable() class HypeTheme extends Theme { name = 'Hype'; css = require('./theme.scss'); terminalBackground = '#010101' }
@NgModule({ providers: [{ provide: Theme, useClass: HypeTheme, multi: true }] }) export default class HypeThemeModule {}

class MySchemes extends TerminalColorSchemeProvider { async getSchemes () { return [/* 16 色对象 */] } }
@NgModule({ providers: [{ provide: TerminalColorSchemeProvider, useClass: MySchemes, multi: true }] }) export default class MySchemesModule {}
```

**社区主题的接入**：Catppuccin（[仓库](https://github.com/catppuccin/tabby)）和 Rosé Pine（[仓库](https://github.com/rose-pine/tabby)）**不是 npm 插件**，只提供 YAML 片段，用户粘进 `customColorSchemes[]` 再到设置里选；Catppuccin 四个 flavor 是四个独立方案，没有自动深浅配对。Tabby 内置的 `tabby-community-color-schemes` 则把约 200 个 Xresources 方案（`*.foreground/background/cursorColor/color0..15`）打包时内嵌、运行时解析成上述对象；Electron 端还会从 `~/.hyper_plugins` 导入 Hyper 主题（[源码](https://github.com/Eugeny/tabby/blob/master/tabby-electron/src/colorSchemes.ts)）。UI 主题（能出现在 Theme 下拉框里的那种）必须装 npm 插件，仅靠 customCSS 不会注册主题；入口在 **Settings → Window → Theme**，且只有 `themes.length > 1` 时才显示（[源码](https://github.com/Eugeny/tabby/blob/master/tabby-settings/src/components/windowSettingsTab.component.pug)）。当前 master 内置 UI 主题**只有** `Follow the color scheme` 一个（[core providers](https://github.com/Eugeny/tabby/blob/master/tabby-core/src/index.ts)）；若看到“内置 Standard/Compact/Paper”之类说法，以源码为准。

**customCSS 的能力边界**：它被塞进 `style#custom-css`（[入口](https://github.com/Eugeny/tabby/blob/master/app/index.pug)），只能作用于 Electron 渲染进程的 DOM；默认终端前端是 `xterm-webgl`（[默认值](https://github.com/Eugeny/tabby/blob/master/tabby-terminal/src/config.ts)），ANSI 文字画在 WebGL canvas 上，CSS 不能按单元格/ANSI 索引改色，也改不了原生菜单/对话框、原生窗口边框。主题 CSS 是整份替换（`<style id="theme">` 的 `textContent`），选择器依赖 Angular 组件 DOM 与 Bootstrap 类名；Hype 的 README 明确以 Bootstrap 4 为基线，升级 Tabby 后易碎（**[社区]** 经验，非官方兼容承诺）。可覆盖的 CSS 变量由 `ThemesService` 每次切换时写入 `documentElement` 内联样式，主要有 `--theme-bg/--theme-fg/--theme-bg-more/--theme-fg-more`、`--theme-primary`、`--bs-body-bg/--bs-body-color` 等（[源码](https://github.com/Eugeny/tabby/blob/master/tabby-core/src/services/themes.service.ts)）；用 customCSS 覆盖它们需 `!important` 才压得过内联值，属于可用但不稳定的技巧 **[推断]**。

## 2. 跟随系统深色：现状与可行做法

- **原生支持存在，且不需要插件**：`appearance.colorSchemeMode` 有 `auto`/`dark`/`light`；`auto` 调 `PlatformService.getTheme()`，Electron 实现读 `nativeTheme.shouldUseDarkColors`，并监听 `nativeTheme` 的 `updated` 事件触发重绘（[源码](https://github.com/Eugeny/tabby/blob/master/tabby-electron/src/services/platform.service.ts)）。设置里对应 “From system / Always dark / Always light”（[UI 源码](https://github.com/Eugeny/tabby/blob/master/tabby-terminal/src/components/colorSchemeSettingsTab.component.pug)）。
- **深浅两套配色是分开配置的**：`terminal.colorScheme` 与 `terminal.lightColorScheme` 各存一个完整对象，`auto` 时按系统在两者间切换。因此 Catppuccin 用户可把深色侧设为 Mocha、浅色侧设为 Latte，即可自动跟随；社区主题本身不提供配对元数据，需要手动各选一次。
- **UI chrome 是否跟随，取决于当前 UI 主题**：只有 `Theme.followsColorScheme === true` 时，`ThemesService` 才会从当前终端方案重新计算 `--theme-*`/`--bs-*` 变量；内置 `Follow the color scheme` 正是这样做的（[`theme.ts`](https://github.com/Eugeny/tabby/blob/master/tabby-core/src/theme.ts)）。插件 UI 主题（如 Hype）的 CSS 是固定写死的、不依赖 `--theme-*` 变量，所以切系统时通常只有终端配色变、标签栏/设置页不变；一个插件主题若主动设 `followsColorScheme = true` 并只用变量取色，也能跟随，但这不是默认行为。
- **[社区] 常见做法**：主题仓库按 flavor 拆成多个独立方案，用户分别在 “Dark mode / Light mode” 两个页签里选；没有“主题内声明浅色对应项”的官方字段。网上“Tabby 不能跟随系统”的说法已过时，通常指插件 UI 主题不跟随，或指旧版本。

## 3. 对本项目的建议：`~/.config/rusttabby/themes/*.toml`

沿用现有 `RUSTTABBY_CONFIG`/XDG 路径规则，主题目录默认取配置文件父目录下的 `themes/`，并允许 `RUSTTABBY_THEMES_DIR` 覆盖。一个文件 = 一个完整变体；可选 `pair` 指向同目录的另一个变体，形成 dark/light 对。主题是纯数据，不加载 JS/脚本。

### 3.1 完整示例（`themes/my-dark.toml`）

```toml
schema = 1
name = "My Dark"          # 设置面板显示名，可重复；id 由文件名决定
variant = "dark"          # dark | light
pair = "my-light"         # 可选：同目录 my-light.toml，必须 variant="light" 且反向指向本文件

foreground = "#cdd6f4"
background = "#1e1e2e"
cursor = "#f5e0dc"
ansi = [                  # 前 8 个 normal、后 8 个 bright，共 16 项
  "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
  "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
]                         # 顺序同 Tabby：black ... white, brightBlack ... brightWhite

[ui]                      # 11 个 chrome 颜色，缺一即拒绝加载
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

`themes/my-light.toml` 与上面同构，只把 `name`、`variant = "light"`、`pair = "my-dark"` 和三组颜色换掉。**两侧 `variant` 必须相反、`pair` 必须互相指向**，否则视为无 pair（主题仍可加载，只打 warning）。

### 3.2 命名与配置约定

- 文件名去 `.toml` 即 `<stem>`，只允许 `[A-Za-z0-9][A-Za-z0-9_-]{0,63}`；配置写 `theme = "custom:my-dark"`，不写扩展名、不允许路径分隔符。`custom:` 前缀是命名空间，避免与内置 id 冲突。
- 保留现有内置 id 与 `theme = "auto"` 的语义不变；`theme = "custom:<stem>"` 是固定选择，系统切换不影响它。
- 自定义主题参与 auto 时，用 `auto_dark`/`auto_light` 指定成员；只写一侧时由该主题的 `pair` 补齐另一侧：

```toml
theme = "auto"                    # 跟随系统
auto_dark = "custom:my-dark"      # 省略则内置 Tokyo Night
auto_light = "custom:my-light"    # 省略且 pair 有效时自动补为 my-light；否则内置 Latte
```

### 3.3 加载、校验、热重载与回退

**加载**：启动时先注册 10 个内置主题，再扫描主题目录；按 stem 排序解析，非法文件跳过并记录 `[theme] <path>:<reason>`。配置里的主题 id 解析顺序：内置 id → `auto` → `custom:<stem>`；找不到就回退内置 `tokyo-night`，且**不写回配置**（保留用户原值便于修复）。设置面板的主题列表 = Auto + 内置 + 所有通过校验的自定义项（label 用 `name`）；选中自定义项只改 `theme`，不动 `auto_*`。

**校验**（结构性错误拒绝该文件，语义警告不阻塞）：`schema` 必须为 1（缺省按 1，其余拒绝）；`name` 非空且 ≤64 字符；`variant` 必须是 `dark`/`light`；颜色只接受 `#RRGGBB`（大小写不敏感，拒绝 8 位 alpha）；`ansi` 必须恰好 16 项；`[ui]` 的 11 个键必须齐全；未知键仅 warning（便于向前兼容）。`pair` 必须存在、`variant` 相反且互相指向，否则忽略 pair。对比度沿用 `palette.rs` 测试里的亮度公式：`|lum(fg)-lum(bg)| < 40` 拒绝，`< 60` warning；`accent`/`accent-text`、`text`/`bg` 同样检查。

**构建与下发**：把校验结果转成 core 的 `ThemeColors`/`UiColors`（u32 RGB），复用现有 256 色立方与 dim 变体生成逻辑（把 `Palette::for_theme` 的入参抽成 `ThemeColors`）。注意现有 `Ctrl::SetTheme(Theme)` 依赖 `Theme: Copy`；建议改成 `SetTheme(Arc<ThemeData>)`（或 core 内维护 `ThemeId -> Arc` 注册表），一次性广播到所有 pane，避免每 pane 重复解析。

**热重载**：用 `notify`（Linux inotify）监听主题目录与配置文件，200ms debounce；编辑器常见的「临时文件 + rename」按 create/rename 处理。配置文件自身写入会触发事件，用内容哈希比较，相同则 no-op。活动主题文件变更为合法内容时立即重解析并下发；非法时**保留上一份有效主题**，只记录错误。活动文件被删除时保留内存中的 last-good 并提示，只有重启后找不到才回退内置。配置里 `theme`/`auto_*` 变化立即重新解析（无需重启），字号/回滚仍按现有路径处理。

**错误回退**：启动时活动主题非法 → `tokyo-night`；运行中非法 → 保留当前已解析主题，不闪回；`custom:` id 未知 → 同上；auto 的某一侧缺失/非法 → 该侧单独回退（深色 Tokyo Night、浅色 Latte）并 warning；pair 断裂 → 仅忽略 pair。所有错误写入 `last_theme_error` 供设置面板显示一行，不要 panic。

**auto 与 custom 的解析**：`theme = "auto"` 时优先用 `auto_dark`/`auto_light`，每侧按 `variant` 校验（深色槽只能放 `variant="dark"`）；都未配置则完全走现有内置映射。系统监听沿用 `main.rs` 的 `detect_system_dark()`/`watch_system_scheme()`；因为 auto 仍以 `theme = "auto"` 表示，现有 watcher 的启动条件不用改。解析结果结构建议为 `ResolvedTheme { data: Arc<ThemeData>, source: ThemeSource, variant }`（`ThemeSource = Builtin(id) | Custom(stem)`；`ThemeData` 含 ansi16/fg/bg/cursor 与 11 个 chrome 色），Slint 的 `Palette.colors`（chrome）与各 pane 的 `SetTheme`（终端调色板）都从这个对象取值，保证「界面 chrome / 终端 16 色 / 光标」始终同源。

**测试清单**：完整示例可解析；15/17 个 ANSI 拒绝；坏 hex、缺 `[ui]` 键、`schema=2` 拒绝；pair 非互指只 warning；auto 四象限（内置/自定义 × 深/浅）解析正确；非法重载保留 last-good；删除活动文件后的回退；`custom:` id 不存在时的启动回退。

## 参考链接

- 官方 wiki：[Config file](https://github.com/Eugeny/tabby/wiki/Config-file)、[Color scheme format](https://github.com/Eugeny/tabby/wiki/Color-scheme-format)
- 官方源码：[`configDefaults.yaml`](https://github.com/Eugeny/tabby/blob/master/tabby-core/src/configDefaults.yaml)、[`api/theme.ts`](https://github.com/Eugeny/tabby/blob/master/tabby-core/src/api/theme.ts)、[`themes.service.ts`](https://github.com/Eugeny/tabby/blob/master/tabby-core/src/services/themes.service.ts)、[`tabby-terminal/src/config.ts`](https://github.com/Eugeny/tabby/blob/master/tabby-terminal/src/config.ts)、[`colorSchemeProvider.ts`](https://github.com/Eugeny/tabby/blob/master/tabby-terminal/src/api/colorSchemeProvider.ts)、[`windowSettingsTab.component.pug`](https://github.com/Eugeny/tabby/blob/master/tabby-settings/src/components/windowSettingsTab.component.pug)、[`platform.service.ts`](https://github.com/Eugeny/tabby/blob/master/tabby-electron/src/services/platform.service.ts)
- 官方 API 参考：[TerminalColorScheme](https://docs.tabby.sh/interfaces/TerminalColorScheme.html)、[Theme](https://docs.tabby.sh/classes/Theme.html)
- 社区主题：[Catppuccin for Tabby](https://github.com/catppuccin/tabby)、[Rosé Pine for Tabby](https://github.com/rose-pine/tabby)、[Hype UI theme](https://github.com/Eugeny/tabby-theme-hype)、[Noctis color schemes](https://github.com/aaronhuggins/tabby-theme-noctis)
