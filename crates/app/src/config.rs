// SPDX-License-Identifier: GPL-3.0-only
//! 用户配置：`~/.config/tervo/config.toml`（可用 `TERVO_CONFIG` 覆盖路径）。
//!
//! 两条硬约束（需求 C8）：
//! - **版本化**：文件里带 `version`；旧文件（没有该字段）读入时视为 v0，
//!   迁移前先备份成 `config.toml.v<旧版本>.bak`，再原子写回新版本。
//! - **不丢未知字段**：其它工具/未来版本写进来的键通过 `extra` 原样保留，
//!   避免一次读写就把用户配置洗掉。
//!
//! 解析失败时绝不覆盖原文件：宁可本次用默认值，也要把用户的坏文件留在原处给人看。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tervo_core::CursorShape;

use crate::commands::{self, HotkeyOverride};
use crate::keys::{BackspaceBinding, DeleteBinding, KeyBindings};

/// 当前配置结构版本。改动字段语义/结构时递增，并在 [`Config::migrate`] 里补迁移步骤。
pub const SCHEMA_VERSION: u32 = 1;

/// `scrollback_unlimited = true` 时实际使用的安全上限。
///
/// alacritty 的历史缓冲是**预分配**的：100,000 行 × 120 列 × 16 B ≈ 192 MB
/// （每多一个 pane 就再占一份）。真正无限会在长时间输出后 OOM，所以这里
/// 给一个明确的上限，并在设置页展示风险。
pub const UNLIMITED_SCROLLBACK_LINES: usize = 100_000;

/// 右侧 overlay 滚动条策略（对应 GNOME Terminal 的 scrollbar-policy）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollbarPolicy {
    /// 始终显示；没有历史时 thumb 占满轨道（表示无可滚动内容）。
    Always,
    /// 只有存在 scrollback 时才显示（默认）。
    #[default]
    Auto,
    /// 永不显示。
    Never,
}

impl ScrollbarPolicy {
    /// 解析配置字符串；大小写/空白不敏感，未知值回退默认 `auto`。
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "always" => Self::Always,
            "never" => Self::Never,
            _ => Self::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Auto => "auto",
            Self::Never => "never",
        }
    }
}

/// 终端背景不透明度下限：0 = 完全透出背景图，默认不透明时保持主题底色。
pub const MIN_BACKGROUND_OPACITY: f32 = 0.0;
/// 终端背景不透明度上限。
pub const MAX_BACKGROUND_OPACITY: f32 = 1.0;
/// 背景图暗化遮罩下限（0 = 不遮）。
pub const MIN_BACKGROUND_DIM: f32 = 0.0;
/// 背景图暗化遮罩上限：再深就只剩一片黑，失去背景图意义。
pub const MAX_BACKGROUND_DIM: f32 = 0.8;
/// 默认遮罩 60%：白块被压到 ~102 灰，浅色终端文字仍能保持可读对比度。
pub const DEFAULT_BACKGROUND_DIM: f32 = 0.6;

/// 光标闪烁周期（毫秒），与 VTE/GNOME 的 ~530ms 对齐。
pub const CURSOR_BLINK_MS: u64 = 530;

/// 解析 `#RGB` / `#RRGGBB` / `RRGGBB`（大小写不敏感）为 0x00RRGGBB。
/// 空串、长度不是 3/6、非法字符都返回 None。
pub fn parse_hex_color(input: &str) -> Option<u32> {
    let raw = input.trim().trim_start_matches('#');
    let expanded = match raw.len() {
        3 => raw
            .chars()
            .map(|c| {
                let mut two = String::with_capacity(2);
                two.push(c);
                two.push(c);
                two
            })
            .collect::<String>(),
        6 => raw.to_string(),
        _ => return None,
    };
    if !expanded.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(&expanded, 16).ok()
}

/// 把颜色输入规范成 `#rrggbb`；空 = 关闭（None），非法 = None。
pub fn normalize_hex_color(input: &str) -> Option<String> {
    parse_hex_color(input).map(|rgb| format!("#{rgb:06x}"))
}

/// 光标形状的规范值：block | underline | ibeam。未知/空回退 block。
pub fn normalize_cursor_shape(input: &str) -> String {
    CursorShape::from_name(input).config_name().to_string()
}

/// 光标闪烁的规范值：system | on | off。未知/空回退 system。
pub fn normalize_cursor_blink(input: &str) -> String {
    match input.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => "on".to_string(),
        "off" | "false" | "no" | "0" => "off".to_string(),
        _ => "system".to_string(),
    }
}

/// 键盘协议的规范值：auto（默认）| off。未知/空回退 auto。
pub fn normalize_keyboard_protocol(input: &str) -> String {
    match input.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "disabled" | "no" | "0" => "off".to_string(),
        _ => "auto".to_string(),
    }
}

/// `keyboard_protocol` 是否允许响应/编码增强。
pub fn keyboard_protocol_enabled(value: &str) -> bool {
    normalize_keyboard_protocol(value) != "off"
}

/// 界面语言：`en`（默认）| `zh`。
pub const LANGUAGE_EN: &str = "en";
pub const LANGUAGE_ZH: &str = "zh";

/// 语言值归一：`zh` / `zh-CN` / `zh_CN` / `cn` / `chinese` → `zh`；
/// 其它（含空、拼错、未知）一律回退默认 `en`。大小写/空白不敏感。
pub fn normalize_language(input: &str) -> String {
    let lower = input.trim().to_ascii_lowercase();
    if lower == "zh"
        || lower.starts_with("zh-")
        || lower.starts_with("zh_")
        || matches!(lower.as_str(), "cn" | "chinese" | "中文")
    {
        LANGUAGE_ZH.to_string()
    } else {
        LANGUAGE_EN.to_string()
    }
}

/// 侧栏宽度夹取：200–360 逻辑像素；NaN/非有限值回 240，结果取整。
pub fn clamp_sidebar_width(width: f32) -> f32 {
    if !width.is_finite() {
        return 240.0;
    }
    width.clamp(200.0, 360.0).round()
}

/// 夹取到 0.0–1.0 并四舍五入到两位小数；NaN/∞ 回退到 1.0。
pub fn clamp_opacity(v: f32) -> f32 {
    if !v.is_finite() {
        return 1.0;
    }
    let v = v.clamp(MIN_BACKGROUND_OPACITY, MAX_BACKGROUND_OPACITY);
    (v * 100.0).round() / 100.0
}

/// 夹取到 0.0–0.8 并四舍五入到两位小数；NaN/∞ 回退到默认遮罩。
pub fn clamp_dim(v: f32) -> f32 {
    if !v.is_finite() {
        return DEFAULT_BACKGROUND_DIM;
    }
    let v = v.clamp(MIN_BACKGROUND_DIM, MAX_BACKGROUND_DIM);
    (v * 100.0).round() / 100.0
}

fn serialize_opacity<S>(value: &f32, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    // 用 f64 输出，避免 f32 二进制尾数被 TOML 序列化成 0.330000013...。
    let rounded = (clamp_opacity(*value) as f64 * 100.0).round() / 100.0;
    serializer.serialize_f64(rounded)
}

/// 手写 TOML 可能把 `background_opacity` 写成整数（如 `= 1`），两种都接受。
#[derive(Deserialize)]
#[serde(untagged)]
enum OpacityValue {
    Float(f64),
    Int(i64),
}

fn deserialize_opacity<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(clamp_opacity(deserialize_f32(deserializer)?))
}

fn serialize_dim<S>(value: &f32, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let rounded = (clamp_dim(*value) as f64 * 100.0).round() / 100.0;
    serializer.serialize_f64(rounded)
}

fn deserialize_dim<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(clamp_dim(deserialize_f32(deserializer)?))
}

fn deserialize_f32<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    let value = OpacityValue::deserialize(deserializer)?;
    Ok(match value {
        OpacityValue::Float(v) => v as f32,
        OpacityValue::Int(v) => v as f32,
    })
}

/// 展开 `~` / `~/...`；`~user` 不查 passwd，保持原样。
pub fn expand_tilde_with_home(input: &str, home: &Path) -> PathBuf {
    let input = input.trim();
    if input == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(input)
}

/// 用 `$HOME` 展开 `~`；HOME 缺失时按当前目录处理。
pub fn expand_tilde(input: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    expand_tilde_with_home(input, &home)
}

/// 校验设置里的背景图路径：空 → `None`（关闭）；否则展开 `~` 后必须是
/// **存在的普通文件**。目录、缺失文件、权限问题都返回可展示的错误文本。
pub fn resolve_background_image(input: &str) -> Result<Option<PathBuf>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let path = expand_tilde(input);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(Some(path)),
        Ok(_) => Err(format!("不是普通文件：{}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("文件不存在：{}", path.display()))
        },
        Err(e) => Err(format!("无法访问 {}：{e}", path.display())),
    }
}

/// `[[terminal_menu]]`：pane 右键菜单的一项（表驱动，按出现顺序排列）。
///
/// 语义：`terminal_menu` 非空时**整张菜单按这里的顺序生成**（替换内置默认表）——
/// 删一项 = 不写它；排序 = 调整顺序；加一项 = 写一个新的 `id`（内置命令、
/// 插件命令 `<plugin-id>.<command>` 或配置里自定义的 command ID）。
/// 空数组/缺省 = 使用内置默认菜单（行为与改造前逐项一致）。
///
/// 内置特殊 ID：
/// - `link.open` / `link.copy`：右键命中链接时才显示（与默认菜单一致）；
/// - `terminal.toggle-broadcast` / `terminal.toggle-read-only`：标签带 ON/OFF；
/// - `pane.*` / `terminal.*`：既有命令表 ID；
/// - `separator = true`（或 `id = "-"`/`"separator"`）：分隔线。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalMenuItem {
    /// 命令 ID；分隔线项可留空（`separator = true`）。
    #[serde(default)]
    pub id: String,
    /// 覆盖显示标签；空 = 取内置本地化标签或命令标题。
    #[serde(default)]
    pub label: String,
    /// 覆盖快捷键提示；空 = 取命令当前生效绑定（含 `[[hotkeys]]` 覆盖）。
    #[serde(default)]
    pub shortcut: String,
    /// 破坏性项标红；`None` = 沿用内置默认（如 Close Pane 为 true）。
    #[serde(default)]
    pub danger: Option<bool>,
    /// true = 分隔线（忽略 `id`）。
    #[serde(default)]
    pub separator: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 配置结构版本；缺失（旧文件）= 0。
    #[serde(default)]
    pub version: u32,
    /// 界面语言：`en`（默认）| `zh`。未知值读入时回退 `en`；热更新包含它。
    pub language: String,
    /// 主题 id（"auto" | "yaru-dark" | "catppuccin-mocha" | "custom:<stem>" ...）。
    /// 默认 `"auto"`：深色用 Yaru Dark，浅色用 Yaru Light；也可显式指定任意主题。
    pub theme: String,
    /// `theme = "auto"` 时深色侧使用的主题 id（省略则用内置 Yaru Dark）。
    pub auto_dark: Option<String>,
    /// `theme = "auto"` 时浅色侧使用的主题 id（省略时优先用 auto_dark 的 pair）。
    pub auto_light: Option<String>,
    pub font_size: f32,
    /// 主字体族：字体文件路径（`/path/x.ttf`）或族名（按文件名匹配，如
    /// `"JetBrains Mono"`）。空 = 内置 JetBrains Mono → DejaVu Sans Mono 链。
    pub font_family: String,
    /// 字重（100–900）：挑主字体里最接近这个字重的 face；≥600 时普通文本也用
    /// 粗体 face。非法值归一成 400。
    pub font_weight: u16,
    /// 行距倍数（1.0 = 字体自然行高）：调大后 cell 变高、PTY 行数变少。
    pub line_height: f32,
    /// 额外字间距（像素，可为负）：直接加在 cell 宽度上。
    pub letter_spacing: f32,
    /// 渲染缩放（1.0 = 按 `font_size` 光栅化）：只改字形像素大小，不改逻辑字号；
    /// cell 尺寸按缩放后的 advance 重算。
    pub render_scale: f32,
    /// 历史缓冲行数（对新 pane 生效）。
    pub scrollback: usize,
    pub shell: Option<String>,
    /// 新 Tab（`+` / Ctrl+T / 命令 `tab.new`）默认使用的 Profile ID。
    /// 0 = 未设置，使用系统默认 shell；ID 已失效时同样回退（不自动改写用户配置）。
    pub default_profile_id: u64,
    /// 按**名字**指定默认 Profile（手写 config.toml 用，避免去查内部 ID）。
    /// 与 `default_profile_id` 二选一：ID 命中时优先；名字匹配不区分大小写。
    /// 空 = 未设置；名字不存在时回退系统 shell。
    pub default_profile: String,
    pub cols: u16,
    pub rows: u16,
    /// 关闭窗口时若有多个会话，先弹确认（默认开）。
    pub confirm_close: bool,
    /// 启动时自动按上次退出快照恢复 Tab/分屏/cwd（默认 false = 只保留手动
    /// Restart Session）。只在正常退出时写快照；崩溃不保证恢复。
    pub restore_session_on_startup: bool,
    /// 断线自动重连的最大次数（SESS-007；默认 3，0 = 关闭）。仍要求 Profile
    /// 打开 `auto_reconnect`；策略适用于所有连接类型，不只 SSH。
    pub reconnect_max_attempts: u32,
    /// 每次自动重连前的退避（毫秒；默认 2000/5000/10000）。列表短于次数时
    /// 重复最后一项；条目夹取到 100ms–10min。
    #[serde(default = "default_reconnect_backoff_ms")]
    pub reconnect_backoff_ms: Vec<u64>,
    /// 拖选后自动复制（默认开）。
    pub copy_on_select: bool,
    /// 多行粘贴前确认（默认开）。
    pub confirm_multiline_paste: bool,
    /// OSC 52 剪贴板："allow"（默认）| "deny"。
    ///
    /// 允许时远端程序可通过 OSC 52 写本机剪贴板/PRIMARY，也可查询剪贴板并经
    /// PTY 回写；不需要时设 `"deny"` 关闭两个方向。未知值按默认 `allow` 处理。
    pub osc52: String,
    /// 右键直接粘贴（旧字段，保留用于迁移；新配置用 `right_click`）。
    pub right_click_paste: bool,
    /// Tabby 式右键模式：menu | paste | clipboard | off。
    pub right_click: String,
    /// 中键粘贴（Tabby 默认开）。
    pub paste_on_middle_click: bool,
    /// TabBar 位置："top" | "bottom"。
    pub tabbar_position: String,
    /// 本地 shell OSC 标题前缀（默认空）。只影响本地 pane；远端标题留给连接插件。
    pub title_prefix: String,
    /// 本地 shell OSC 标题后缀（默认空）。
    pub title_suffix: String,
    /// 本地 shell OSC 标题过滤器：`;` 分隔的字面替换规则，`from=>to`；
    /// 没有 `=>` 的规则表示删除该子串。先过滤，再拼前后缀。
    pub title_filter: String,
    /// Tab 非活动时收到 OSC 9/777/BEL 是否调用 notify-send（默认开）。
    pub desktop_notifications: bool,
    /// 活动 pane 收到 BEL 时是否闪一下边框（视觉铃，默认开）。
    pub visual_bell: bool,
    /// BEL 时是否调用系统响铃（audible bell，默认关，避免突然响铃）。
    ///
    /// 与 `visual_bell` 独立：可以只响、只闪、都开或都关。
    pub audible_bell: bool,
    /// 用户上滚查看历史时，新输出是否强制回到底部（默认 false = 不抢视图）。
    pub scroll_on_output: bool,
    /// 按键后是否回到底部（默认 true）。
    pub scroll_on_keystroke: bool,
    /// Sixel 内联图片（默认开）。关闭后不解析/渲染图片，DA1 不再宣告参数 4。
    pub sixel: bool,
    /// Sixel 图片是否跟随滚动历史移动（默认 true）；false 时固定在创建时的屏幕行。
    pub sixel_scrolling: bool,
    /// iTerm2 IIP（OSC 1337 File）内联图片（默认开）。关闭后不解析/解码 IIP，
    /// 已显示的 IIP 图片立即清除；不影响 Sixel/DA1。
    pub iip: bool,
    /// 设置页可开关的渲染 HUD（FPS/renderer/GPU 计数）；默认关。
    /// `TERVO_RENDER_HUD=1` 仍可强制开启，便于无配置复现。
    pub render_hud: bool,
    /// 反馈 issue 的新建页 URL；空 = 只生成本地 bundle，不打开网页。
    /// 也可以用 `TERVO_FEEDBACK_ISSUE_URL` 覆盖。不会自动上传或创建 MR。
    pub feedback_issue_url: String,
    /// 本地安装插件时要求 Ed25519 签名（设置页 Plugins 开关）。
    /// `TERVO_PLUGIN_REQUIRE_SIGNATURE=1` 同样会强制；默认 false（允许未签名，
    /// 但仍会拒绝坏签名/带签名却无公钥的包）。见 docs/PLUGIN_INSTALL.md。
    pub plugin_require_signature: bool,
    // ---- MCP（内嵌功能，不是插件；additive 字段不 bump schema）----
    /// MCP 总开关（默认 true）。关闭后不创建 live socket、不暴露 live 工具；
    /// 离线 `tervo diag` 工具不受影响。
    pub mcp_enabled: bool,
    /// read 分组：capabilities/snapshot/list/output（默认 true = 默认只读）。
    pub mcp_read: bool,
    /// input 分组：send_text/send_key/run（默认 false）。
    pub mcp_input: bool,
    /// tabs 分组：new_tab/split/focus/scroll（默认 false）。
    pub mcp_tabs: bool,
    /// destructive 分组：close_pane/close_tab/kill（默认 false）。
    /// 这是附加开关：close 还要求 tabs，kill 还要求 input。
    pub mcp_destructive: bool,
    /// 忽略 `scrollback` 数值，使用 [`UNLIMITED_SCROLLBACK_LINES`] 安全上限。
    /// 只对之后新建的 pane 生效（历史缓冲大小不能在会话中安全扩容）。
    pub scrollback_unlimited: bool,
    /// 右侧 overlay 滚动条策略："always" | "auto" | "never"（默认 auto）。
    pub scrollbar_policy: String,
    /// Backspace 编码："ascii-delete"（0x7f，默认）| "control-h"（0x08）。
    pub backspace_binding: String,
    /// Delete 编码："delete-sequence"（ESC [3~，默认）| "ascii-del"（0x7f）|
    /// "control-h"（0x08）。
    pub delete_binding: String,
    /// 键盘协议："auto"（默认，响应 kitty/modifyOtherKeys query 并增强编码）|
    /// "off"（不响应、不增强，字节与旧版完全一致）。只对新 pane 生效。
    pub keyboard_protocol: String,
    /// 窗口/终端背景图路径。空 = 关闭；`~` 在应用时展开。
    /// 只有“存在的普通文件”才会生效（校验见 [`resolve_background_image`]）。
    pub background_image: String,
    /// 终端默认背景的不透明度（0.0–1.0，默认 1.0）。0 = 完全透出背景图，
    /// 1 = 主题纯色。写入时四舍五入到两位小数。
    #[serde(deserialize_with = "deserialize_opacity", serialize_with = "serialize_opacity")]
    pub background_opacity: f32,
    /// 背景图之上的暗化遮罩（0.0–0.8，默认 0.6）。只影响背景图，
    /// 让浅色终端文字在纯白/亮色图片区域仍有可读对比度。
    #[serde(deserialize_with = "deserialize_dim", serialize_with = "serialize_dim")]
    pub background_dim: f32,
    // ---- 光标 / 粗体（GNOME Terminal 基础外观）----
    /// 光标形状：`block`（默认）| `underline` | `ibeam`。
    /// 兼容 `beam`/`bar` 等别名；非法值读入时回退 block。
    pub cursor_shape: String,
    /// 光标闪烁：`system`（默认，当前等同 on）| `on` | `off`。
    pub cursor_blink: String,
    /// 光标颜色 `#RRGGBB`；空 = 使用主题光标色。
    pub cursor_color: String,
    /// 光标下字符颜色 `#RRGGBB`；空 = 使用主题背景色（块状光标反色）。
    pub cursor_text_color: String,
    /// 粗体使用亮色（GNOME `bold-is-bright`，默认 true）。
    pub bold_is_bright: bool,
    /// 粗体自定义颜色 `#RRGGBB`；空 = 不启用。
    pub bold_color: String,
    /// true = 粗体沿用普通前景色并忽略 `bold_color`（GNOME
    /// `bold-color-same-as-fg`，默认 true）。
    pub bold_color_same_as_fg: bool,
    // ---- Quake mode ----
    /// 启用后 F12（可配置）全局显示/隐藏下拉终端。
    pub quake_enabled: bool,
    pub quake_hotkey: String,
    /// 屏宽/屏高比例（顶部居中）。
    pub quake_width: f32,
    pub quake_height: f32,
    /// 关闭 Tab/Pane 时对会话的处理："end" 结束进程 | "background" 保留后台可重连。
    pub close_action: String,
    // ---- 工作区壳（Sidebar / Toolbar / 状态区）----
    /// 侧栏是否展开。false（默认）= 布局与 0.5.0 完全一致，不占一像素。
    pub sidebar_visible: bool,
    /// 侧栏宽度（逻辑像素，界面按 200–360 夹取）。
    pub sidebar_width: f32,
    /// 侧栏分区：0 = Profiles，1 = Sessions，2 = Plugins（界面按 0–2 夹取）。
    pub sidebar_section: u8,
    /// 用户自定义快捷键：`[[hotkeys]]` 表，命令 ID + 规范小写 chord。
    /// 空 = 全部使用命令表默认绑定；旧配置没有该字段时按空处理。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hotkeys: Vec<HotkeyOverride>,
    /// pane 右键菜单表（UI-TERM-013）：非空时按此顺序替换内置菜单。
    /// 空 = 内置默认菜单（默认行为不变）；插件声明的菜单项始终追加在末尾。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal_menu: Vec<TerminalMenuItem>,
    /// 本版本不认识的键，原样保留（前向兼容）。
    #[serde(flatten)]
    pub extra: toml::Table,
}

fn default_reconnect_backoff_ms() -> Vec<u64> {
    crate::connection::DEFAULT_BACKOFF_MS.to_vec()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            language: LANGUAGE_EN.to_string(),
            theme: "auto".to_string(),
            auto_dark: None,
            auto_light: None,
            font_size: 15.0,
            font_family: String::new(),
            font_weight: 400,
            line_height: 1.0,
            letter_spacing: 0.0,
            render_scale: 1.0,
            scrollback: 2000,
            shell: None,
            default_profile_id: 0,
            default_profile: String::new(),
            cols: 120,
            rows: 32,
            confirm_close: true,
            restore_session_on_startup: false,
            reconnect_max_attempts: crate::connection::DEFAULT_MAX_ATTEMPTS,
            reconnect_backoff_ms: default_reconnect_backoff_ms(),
            copy_on_select: true,
            confirm_multiline_paste: true,
            osc52: "allow".to_string(),
            right_click_paste: false,
            right_click: "menu".to_string(),
            paste_on_middle_click: true,
            tabbar_position: "top".to_string(),
            title_prefix: String::new(),
            title_suffix: String::new(),
            title_filter: String::new(),
            desktop_notifications: true,
            visual_bell: true,
            audible_bell: false,
            scroll_on_output: false,
            scroll_on_keystroke: true,
            sixel: true,
            sixel_scrolling: true,
            iip: true,
            render_hud: false,
            feedback_issue_url: String::new(),
            plugin_require_signature: false,
            mcp_enabled: true,
            mcp_read: true,
            mcp_input: false,
            mcp_tabs: false,
            mcp_destructive: false,
            scrollback_unlimited: false,
            scrollbar_policy: "auto".to_string(),
            backspace_binding: "ascii-delete".to_string(),
            delete_binding: "delete-sequence".to_string(),
            keyboard_protocol: "auto".to_string(),
            background_image: String::new(),
            background_opacity: 1.0,
            background_dim: DEFAULT_BACKGROUND_DIM,
            cursor_shape: "block".to_string(),
            cursor_blink: "system".to_string(),
            cursor_color: String::new(),
            cursor_text_color: String::new(),
            bold_is_bright: true,
            bold_color: String::new(),
            bold_color_same_as_fg: true,
            quake_enabled: false,
            quake_hotkey: "F12".to_string(),
            quake_width: 0.7,
            quake_height: 0.5,
            close_action: "end".to_string(),
            sidebar_visible: false,
            sidebar_width: 240.0,
            sidebar_section: 0,
            hotkeys: Vec::new(),
            terminal_menu: Vec::new(),
            extra: toml::Table::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("TERVO_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("tervo").join("config.toml")
    }

    /// 生效语言：`zh` 或 `en`（未知值回退 en）。UI 热更新/启动都读取它。
    pub fn lang(&self) -> &'static str {
        if normalize_language(&self.language) == LANGUAGE_ZH {
            LANGUAGE_ZH
        } else {
            LANGUAGE_EN
        }
    }

    /// 生效的右键模式：旧配置 `right_click_paste=true` 且未显式选模式时按 paste 处理。
    pub fn effective_right_click(&self) -> &str {
        if self.right_click == "menu" && self.right_click_paste {
            "paste"
        } else {
            &self.right_click
        }
    }

    /// OSC 52 是否放行。默认 allow；只有显式 `deny`（不区分大小写）才拒绝，
    /// 未知值仍按默认 allow，避免写错一个词就让功能整体失效。
    pub fn osc52_allowed(&self) -> bool {
        !self.osc52.trim().eq_ignore_ascii_case("deny")
    }

    /// SESS-007：生效的自动重连策略（次数/退避都在 config 里；读入时夹取）。
    pub fn reconnect_policy(&self) -> crate::connection::ReconnectPolicy {
        crate::connection::ReconnectPolicy::from_config(
            self.reconnect_max_attempts,
            &self.reconnect_backoff_ms,
        )
    }

    /// 生效的光标形状。配置非法时回退 block（GNOME 默认）。
    pub fn cursor_shape(&self) -> CursorShape {
        CursorShape::from_name(&self.cursor_shape)
    }

    /// 光标是否闪烁。`off` 关闭；`on` / `system` 开启（GNOME 的系统默认是开）。
    pub fn cursor_blinking(&self) -> bool {
        !self.cursor_blink.trim().eq_ignore_ascii_case("off")
    }

    /// 光标颜色覆盖；空/非法 = None（主题色）。
    pub fn cursor_color_rgb(&self) -> Option<u32> {
        parse_hex_color(&self.cursor_color)
    }

    /// 光标下字符颜色覆盖；空/非法 = None（主题背景色）。
    pub fn cursor_text_color_rgb(&self) -> Option<u32> {
        parse_hex_color(&self.cursor_text_color)
    }

    /// 粗体自定义颜色；空/非法 = None。
    pub fn bold_color_rgb(&self) -> Option<u32> {
        parse_hex_color(&self.bold_color)
    }

    /// 把外观相关字段归一成规范值。新增字段都是 additive，不 bump
    /// schema；旧配置（v0/v1）读入时靠这里 + serde default 补齐。
    fn normalize_appearance(&mut self) {
        // 语言是 additive 字段：旧配置缺省 en，手写大小写/别名归一。
        self.language = normalize_language(&self.language);
        self.cursor_shape = normalize_cursor_shape(&self.cursor_shape);
        self.cursor_blink = normalize_cursor_blink(&self.cursor_blink);
        self.keyboard_protocol = normalize_keyboard_protocol(&self.keyboard_protocol);
        normalize_color_field(&mut self.cursor_color, "cursor_color");
        normalize_color_field(&mut self.cursor_text_color, "cursor_text_color");
        normalize_color_field(&mut self.bold_color, "bold_color");
        // 字体外观：手写配置可能给出 0/负数/NaN，统一夹到安全范围（与
        // `glyph::FontOptions::normalized` 同一组范围，避免 UI 显示和实际生效不一致）。
        self.font_family = self.font_family.trim().to_string();
        if !(100..=900).contains(&self.font_weight) {
            self.font_weight = 400;
        }
        if !self.line_height.is_finite() {
            self.line_height = 1.0;
        }
        self.line_height = self.line_height.clamp(0.8, 2.5);
        if !self.letter_spacing.is_finite() {
            self.letter_spacing = 0.0;
        }
        self.letter_spacing = self.letter_spacing.clamp(-4.0, 16.0);
        if !self.render_scale.is_finite() {
            self.render_scale = 1.0;
        }
        self.render_scale = self.render_scale.clamp(0.5, 3.0);
    }

    /// 实际下发给新 pane 的 scrollback 行数：开启无限滚动时使用安全上限。
    pub fn effective_scrollback(&self) -> usize {
        if self.scrollback_unlimited {
            UNLIMITED_SCROLLBACK_LINES
        } else {
            self.scrollback
        }
    }

    /// 解析后的滚动条策略；未知值按 `auto`。
    /// 侧栏宽度：夹到 200–360，NaN 回默认 240。
    pub fn sidebar_width(&self) -> f32 {
        clamp_sidebar_width(self.sidebar_width)
    }

    /// 侧栏分区：夹到 0–2（未知值回 Profiles）。
    pub fn sidebar_section(&self) -> u8 {
        self.sidebar_section.min(2)
    }

    pub fn scrollbar_policy(&self) -> ScrollbarPolicy {
        ScrollbarPolicy::from_config(&self.scrollbar_policy)
    }

    /// 当前 Backspace/Delete 编码配置（每次按键读取，改设置立即生效）。
    pub fn key_bindings(&self) -> KeyBindings {
        KeyBindings {
            backspace: BackspaceBinding::from_config(&self.backspace_binding),
            delete: DeleteBinding::from_config(&self.delete_binding),
        }
    }

    /// 键盘协议是否启用（每次创建 pane 时读取）。
    pub fn keyboard_protocol_enabled(&self) -> bool {
        keyboard_protocol_enabled(&self.keyboard_protocol)
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// 只解析、归一化，不写盘。热更新监听使用这个入口，避免因为文件里
    /// `version` 较旧就在后台触发迁移写回（那会被监听器误判成外部修改）。
    pub(crate) fn parse_text(text: &str) -> Result<Self, toml::de::Error> {
        let mut cfg: Config = toml::from_str(text)?;
        // 手写配置也统一成规范小写 chord（支持两段 `ctrl+k ctrl+t`）；
        // 非法项原样保留（分发时忽略，启动期由 repair_hotkey_overrides 清理）。
        for over in &mut cfg.hotkeys {
            if let Ok(shortcut) = commands::Shortcut::parse(&over.chord) {
                over.chord = shortcut.canonical();
            }
        }
        // 外观字段是 additive 的：旧配置缺字段走 serde default，手写别名/大小写
        // 在这里归一；v1 不 bump schema（避免每次启动重写用户配置）。
        cfg.normalize_appearance();
        Ok(cfg)
    }

    /// 读配置；旧版本先备份再迁移写回。解析失败返回默认值且**不动原文件**。
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let mut cfg: Config = match Self::parse_text(&text) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[config] {} 解析失败，本次用默认值（原文件未改动）: {e}", path.display());
                return Self::default();
            },
        };

        if cfg.version < SCHEMA_VERSION {
            let from = cfg.version;
            // 迁移前备份：出错还能人工回退
            let backup = backup_path(path, from);
            match std::fs::copy(path, &backup) {
                Ok(_) => eprintln!("[config] 迁移 v{from} → v{SCHEMA_VERSION} 前已备份 {}", backup.display()),
                Err(e) => eprintln!("[config] 备份失败（继续迁移）: {e}"),
            }
            cfg.migrate(from);
            cfg.save_to(path);
            eprintln!("[config] 已迁移到 v{SCHEMA_VERSION}");
        }
        cfg
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    /// 原子写：先写临时文件再 rename，避免半截配置。
    pub fn save_to(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let text = match toml::to_string_pretty(self) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[config] 序列化失败: {e}");
                return;
            },
        };
        // serde/toml 只保证数值精度，不保留末尾 0；这里把两个小数配置固定成两位小数。
        let text = format_float_line(
            text,
            "background_opacity",
            clamp_opacity(self.background_opacity),
        );
        let text = format_float_line(text, "background_dim", clamp_dim(self.background_dim));
        // 字体外观的小数也固定两位，避免 1 与 1.0 反复改写文件。
        let text = format_float_line(text, "line_height", self.line_height);
        let text = format_float_line(text, "letter_spacing", self.letter_spacing);
        let text = format_float_line(text, "render_scale", self.render_scale);
        let tmp = path.with_extension("toml.tmp");
        if let Err(e) = std::fs::write(&tmp, &text) {
            eprintln!("[config] 写入 {} 失败: {e}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            eprintln!("[config] 替换 {} 失败: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        // 告诉轮询线程“这是自己写的”，避免设置页保存后又被 watcher 套用一次。
        crate::config_watch::note_own_write(path, text.as_bytes());
    }

    /// 逐步迁移到当前版本。新增版本时在这里追加分支，保证可以跨多个版本一次升上来。
    fn migrate(&mut self, from: u32) {
        let mut v = from;
        // v0 → v1：仅打版本号（字段语义没有变化，历史文件本来就能读）。
        if v == 0 {
            v = 1;
        }
        self.version = SCHEMA_VERSION.max(v);
    }
}

/// 配置文件的“健康检查 + 迁移报告”（SESS-008）。
///
/// 只描述文件本身，不携带任何 secret：设置页用它显示当前版本、是否存在
/// 迁移前备份、以及能否一键回滚。`version = None` 表示文件不存在/读不了。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigInspection {
    pub path: PathBuf,
    pub version: Option<u32>,
    pub current_version: u32,
    /// 迁移前备份（`config.toml.v<from>.bak`，优先取版本最大的一个）。
    pub backup: Option<PathBuf>,
    /// 备份里记录的旧版本号（从文件名解析）。
    pub backup_version: Option<u32>,
    /// 解析失败原因（原文件保持不动）。
    pub parse_error: Option<String>,
}

impl ConfigInspection {
    /// 中英文单行报告；空 = 一切正常且没有可回滚备份。
    pub fn summary(&self, zh: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        match (&self.parse_error, self.version) {
            (Some(err), _) => parts.push(if zh {
                format!("配置文件解析失败（本次用默认值，原文件未改）：{err}")
            } else {
                format!("Config parse failed (defaults used, file untouched): {err}")
            }),
            (None, Some(v)) if v > self.current_version => parts.push(if zh {
                format!("配置版本 v{v} 高于程序支持的 v{}，本次忽略", self.current_version)
            } else {
                format!("Config v{v} is newer than supported v{}; ignored", self.current_version)
            }),
            (None, Some(v)) if v < self.current_version => parts.push(if zh {
                format!("配置仍为 v{v}（程序 v{}）；下次启动会先备份再迁移", self.current_version)
            } else {
                format!("Config v{v} (program v{}); next start backs up then migrates", self.current_version)
            }),
            _ => {},
        }
        if let (Some(backup), Some(from)) = (&self.backup, self.backup_version) {
            parts.push(if zh {
                format!("可回滚：迁移前备份 v{from} → {}", backup.display())
            } else {
                format!("Rollback available: pre-migration v{from} backup → {}", backup.display())
            });
        }
        parts.join(if zh { "；" } else { "; " })
    }
}

impl Config {
    /// 检查磁盘上的配置文件（不修改、不迁移）。设置页打开时调用。
    pub fn inspect_file(path: &Path) -> ConfigInspection {
        let mut out = ConfigInspection {
            path: path.to_path_buf(),
            version: None,
            current_version: SCHEMA_VERSION,
            backup: None,
            backup_version: None,
            parse_error: None,
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
            Err(e) => {
                out.parse_error = Some(e.to_string());
                return out;
            },
        };
        match Self::parse_text(&text) {
            Ok(cfg) => out.version = Some(cfg.version),
            Err(e) => out.parse_error = Some(e.to_string()),
        }
        // 迁移备份名与 config.rs 里的 backup_path 一致：`<name>.v<from>.bak`。
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
            let prefix = format!("{}.v", name.to_string_lossy());
            if let Ok(entries) = std::fs::read_dir(dir) {
                let mut best: Option<(u32, PathBuf)> = None;
                for entry in entries.flatten() {
                    let file = entry.file_name();
                    let file = file.to_string_lossy();
                    let Some(rest) = file.strip_prefix(&prefix) else { continue };
                    let Some(ver) = rest.strip_suffix(".bak") else { continue };
                    let Ok(ver) = ver.parse::<u32>() else { continue };
                    if best.as_ref().map(|(v, _)| ver > *v).unwrap_or(true) {
                        best = Some((ver, entry.path()));
                    }
                }
                if let Some((ver, p)) = best {
                    out.backup = Some(p);
                    out.backup_version = Some(ver);
                }
            }
        }
        out
    }

    /// 导出当前内存配置（含已知字段的规范化值）到指定路径。
    ///
    /// 与 `save_to` 不同：不标记 config-watch 自己写，避免把导出文件
    /// 误当成生效配置；也不做默认路径推断。
    pub fn export_to(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| format!("序列化失败: {e}"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
        }
        std::fs::write(path, text).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
    }

    /// 从导出文件读取并校验版本（不写入生效路径）。
    ///
    /// 版本比程序新时拒绝：避免用未来格式覆盖当前配置（字段会被 serde 丢弃）。
    pub fn import_from(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读不到 {}: {e}", path.display()))?;
        let cfg = Self::parse_text(&text).map_err(|e| format!("配置解析失败: {e}"))?;
        if cfg.version > SCHEMA_VERSION {
            return Err(format!(
                "配置版本 v{} 比程序支持的 v{SCHEMA_VERSION} 新，拒绝导入",
                cfg.version
            ));
        }
        Ok(cfg)
    }

    /// 把最新的迁移前备份恢复成 `path`（覆盖当前配置）；返回被使用的备份路径。
    ///
    /// 只做文件复制，不自动应用：主进程下次启动读取 `path` 时生效。
    pub fn restore_latest_backup(path: &Path) -> Result<PathBuf, String> {
        let inspection = Self::inspect_file(path);
        let Some(backup) = inspection.backup else {
            return Err("没有找到迁移前备份（config.toml.v*.bak）".into());
        };
        let bytes = std::fs::read(&backup)
            .map_err(|e| format!("读备份 {} 失败: {e}", backup.display()))?;
        // 先确认备份本身是可解析的配置，避免把坏文件盖上去了。
        let text = String::from_utf8_lossy(&bytes);
        Self::parse_text(&text)
            .map_err(|e| format!("备份 {} 不可解析，未覆盖: {e}", backup.display()))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
        }
        std::fs::write(path, bytes).map_err(|e| format!("恢复 {} 失败: {e}", path.display()))?;
        Ok(backup)
    }
}

/// 归一化颜色字段：空保持空（跟随主题），合法值写成 `#rrggbb`，非法值清空并
/// 在日志里提示一次——不让手写错误把光标/粗体颜色变成不可诊断的“没生效”。
fn normalize_color_field(field: &mut String, key: &str) {
    let raw = field.trim();
    if raw.is_empty() {
        field.clear();
        return;
    }
    match normalize_hex_color(raw) {
        Some(color) => *field = color,
        None => {
            eprintln!("[config] {key}={raw:?} 不是合法颜色，已回退为跟随主题");
            field.clear();
        },
    }
}

/// `config.toml` + v0 → `config.toml.v0.bak`。
fn backup_path(path: &Path, from: u32) -> PathBuf {
    let name = path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default();
    path.with_file_name(format!("{name}.v{from}.bak"))
}

/// TOML 序列化出来的某个 f32 配置行改成固定两位小数（如 `background_dim = 0.60`）。
fn format_float_line(text: String, key: &str, value: f32) -> String {
    let formatted = format!("{key} = {value:.2}");
    let mut out = String::with_capacity(text.len() + 1);
    for line in text.lines() {
        if line.starts_with(key) && line[key.len()..].starts_with(" = ") {
            out.push_str(&formatted);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-cfg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip() {
        let cfg = Config {
            theme: "latte".into(),
            font_size: 17.0,
            default_profile_id: 42,
            background_image: "~/wall.png".into(),
            background_opacity: 0.55,
            background_dim: 0.45,
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.theme, "latte");
        assert!((back.font_size - 17.0).abs() < f32::EPSILON);
        assert_eq!(back.scrollback, cfg.scrollback);
        assert_eq!(back.default_profile_id, 42);
        assert_eq!(back.background_image, "~/wall.png");
        assert!((back.background_opacity - 0.55).abs() < f32::EPSILON);
        assert!((back.background_dim - 0.45).abs() < f32::EPSILON);
        assert_eq!(back.version, SCHEMA_VERSION);
    }

    #[test]
    fn default_profile_id_defaults_to_zero_and_survives_roundtrip() {
        assert_eq!(Config::default().default_profile_id, 0, "默认 Profile 默认未设置");
        // 旧配置没有该字段：serde(default) 补 0，不能解析失败
        let old: Config = toml::from_str("theme = \"nord\"\n").unwrap();
        assert_eq!(old.default_profile_id, 0);
        // 写盘再读回保持一致
        let dir = tmpdir("default-profile");
        let path = dir.join("config.toml");
        let cfg = Config { default_profile_id: 7, ..Config::default() };
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("default_profile_id = 7"), "{text}");
        assert_eq!(Config::load_from(&path).default_profile_id, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_session_on_startup_defaults_off_and_roundtrips() {
        assert!(
            !Config::default().restore_session_on_startup,
            "默认保持旧行为：启动不自动恢复"
        );
        let dir = tmpdir("restore-session");
        let path = dir.join("config.toml");
        let cfg = Config { restore_session_on_startup: true, ..Config::default() };
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("restore_session_on_startup = true"), "{text}");
        assert!(Config::load_from(&path).restore_session_on_startup);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconnect_policy_defaults_roundtrip_and_clamps() {
        use crate::connection::ReconnectPolicy;
        let cfg = Config::default();
        assert_eq!(cfg.reconnect_max_attempts, 3);
        assert_eq!(cfg.reconnect_backoff_ms, vec![2000, 5000, 10000]);
        assert_eq!(cfg.reconnect_policy(), ReconnectPolicy::default());

        let dir = tmpdir("reconnect-policy");
        let path = dir.join("config.toml");
        let cfg = Config {
            reconnect_max_attempts: 5,
            reconnect_backoff_ms: vec![500, 1500],
            ..Config::default()
        };
        cfg.save_to(&path);
        let back = Config::load_from(&path);
        assert_eq!(back.reconnect_max_attempts, 5);
        assert_eq!(back.reconnect_backoff_ms, vec![500, 1500]);
        // 列表短于次数：重复最后一项；第 6 次停止。
        let policy = back.reconnect_policy();
        assert_eq!(policy.delay(2), Some(std::time::Duration::from_millis(1500)));
        assert_eq!(policy.delay(5), Some(std::time::Duration::from_millis(1500)));
        assert_eq!(policy.delay(6), None);

        // 旧配置没有这两个键：serde default 必须给出历史 2/5/10 行为。
        std::fs::write(&path, "language = \"en\"\n").unwrap();
        let old = Config::load_from(&path);
        assert_eq!(old.reconnect_max_attempts, 3);
        assert_eq!(old.reconnect_backoff_ms, vec![2000, 5000, 10000]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidebar_defaults_off_and_clamps() {
        let cfg = Config::default();
        assert!(!cfg.sidebar_visible, "侧栏默认必须关，布局与 0.5.0 一致");
        assert!((cfg.sidebar_width() - 240.0).abs() < f32::EPSILON);
        assert_eq!(cfg.sidebar_section(), 0);

        // 宽度夹取 200–360，NaN 回 240，结果取整
        for (raw, want) in [(100.0, 200.0), (999.0, 360.0), (240.4, 240.0),
                            (300.6, 301.0), (f32::NAN, 240.0), (f32::INFINITY, 240.0)] {
            let mut c = Config::default();
            c.sidebar_width = raw;
            assert!((c.sidebar_width() - want).abs() < f32::EPSILON,
                    "width {raw} -> {} want {want}", c.sidebar_width());
        }
        // 分区夹取 0–2
        let mut c = Config::default();
        c.sidebar_section = 9;
        assert_eq!(c.sidebar_section(), 2);
    }

    #[test]
    fn sidebar_fields_roundtrip_and_old_config_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.sidebar_visible = true;
        cfg.sidebar_width = 312.0;
        cfg.sidebar_section = 2;
        let text = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(&path, &text).unwrap();
        let back = Config::load_from(&path);
        assert!(back.sidebar_visible);
        assert!((back.sidebar_width - 312.0).abs() < f32::EPSILON);
        assert_eq!(back.sidebar_section, 2);

        // 旧配置没有这三个键 -> serde(default) 补齐，不报错、不改写文件
        let old = "version = 1\nfont_size = 15.0\n";
        std::fs::write(&path, old).unwrap();
        let old_cfg = Config::load_from(&path);
        assert!(!old_cfg.sidebar_visible);
        assert!((old_cfg.sidebar_width() - 240.0).abs() < f32::EPSILON);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
    }

    #[test]
    fn render_hud_defaults_off_and_roundtrips() {
        assert!(!Config::default().render_hud, "HUD 默认关闭");
        let dir = tmpdir("render-hud");
        let path = dir.join("config.toml");
        let cfg = Config { render_hud: true, ..Config::default() };
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("render_hud = true"), "{text}");
        let back = Config::load_from(&path);
        assert!(back.render_hud);
        let old: Config = toml::from_str("theme = \"nord\"\n").unwrap();
        assert!(!old.render_hud, "旧配置缺字段必须回退关闭");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let cfg: Config = toml::from_str("theme = \"latte\"\n").unwrap();
        assert_eq!(cfg.theme, "latte");
        assert_eq!(cfg.font_size, 15.0);
        assert_eq!(cfg.scrollback, 2000);
        assert_eq!(cfg.default_profile_id, 0, "旧配置缺 default_profile_id 时回退系统 shell");
        assert!(!cfg.restore_session_on_startup, "旧配置默认不自动恢复会话");
        // 新增的桌面通知/视觉铃开关对旧配置也要默认开启
        assert!(cfg.desktop_notifications, "旧配置缺 desktop_notifications 时默认开");
        assert!(cfg.visual_bell, "旧配置缺 visual_bell 时默认为视觉铃");
        // 终端行为新增项：响铃默认关、输出不抢滚动、按键回底、滚动条 auto、GNOME 默认编码
        assert!(!cfg.audible_bell, "旧配置缺 audible_bell 时默认不响铃");
        assert!(!cfg.scroll_on_output);
        assert!(cfg.scroll_on_keystroke);
        assert!(!cfg.scrollback_unlimited);
        assert_eq!(cfg.scrollbar_policy(), ScrollbarPolicy::Auto);
        assert_eq!(cfg.key_bindings(), KeyBindings::default());
        // 背景图/透明度对旧配置也必须兼容（默认关闭背景、不透明）
        assert_eq!(cfg.background_image, "");
        assert_eq!(cfg.background_opacity, 1.0);
        assert!((cfg.background_dim - DEFAULT_BACKGROUND_DIM).abs() < f32::EPSILON);
        // 没有 version 的旧文件按 v0 读入
        assert_eq!(cfg.version, 0);
    }

    #[test]
    fn background_opacity_clamps_and_rounds() {
        assert!((clamp_opacity(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((clamp_opacity(-0.3) - 0.0).abs() < f32::EPSILON);
        assert!((clamp_opacity(0.333) - 0.33).abs() < f32::EPSILON);
        assert!((clamp_opacity(0.999) - 1.0).abs() < f32::EPSILON);
        assert!((clamp_opacity(1.5) - 1.0).abs() < f32::EPSILON);
        assert!((clamp_opacity(f32::NAN) - 1.0).abs() < f32::EPSILON);
        assert!((Config::default().background_opacity - 1.0).abs() < f32::EPSILON);

        // 手写配置可超出范围 / 写成整数；读入时统一夹取、两位小数。
        let cfg: Config = toml::from_str("version = 1\nbackground_opacity = 0.456\n").unwrap();
        assert!((cfg.background_opacity - 0.46).abs() < 1e-6);
        let cfg: Config = toml::from_str("version = 1\nbackground_opacity = 1\n").unwrap();
        assert!((cfg.background_opacity - 1.0).abs() < f32::EPSILON);
        let cfg: Config = toml::from_str("version = 1\nbackground_opacity = 0.05\n").unwrap();
        assert!((cfg.background_opacity - 0.05).abs() < f32::EPSILON, "0% 也必须允许");
        let cfg: Config = toml::from_str("version = 1\nbackground_opacity = 2.0\n").unwrap();
        assert!((cfg.background_opacity - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn background_dim_clamps_and_rounds() {
        assert!((clamp_dim(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((clamp_dim(0.333) - 0.33).abs() < f32::EPSILON);
        assert!((clamp_dim(0.8) - 0.8).abs() < f32::EPSILON);
        assert!((clamp_dim(0.95) - 0.8).abs() < f32::EPSILON, "遮罩上限 80%");
        assert!((clamp_dim(-1.0) - 0.0).abs() < f32::EPSILON);
        assert!((clamp_dim(f32::NAN) - DEFAULT_BACKGROUND_DIM).abs() < f32::EPSILON);
        assert!((Config::default().background_dim - 0.6).abs() < f32::EPSILON);

        let cfg: Config = toml::from_str("version = 1\nbackground_dim = 0.456\n").unwrap();
        assert!((cfg.background_dim - 0.46).abs() < 1e-6);
        let cfg: Config = toml::from_str("version = 1\nbackground_dim = 1\n").unwrap();
        assert!((cfg.background_dim - 0.8).abs() < f32::EPSILON);
        let cfg: Config = toml::from_str("version = 1\nbackground_dim = 0\n").unwrap();
        assert!((cfg.background_dim - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn background_fields_roundtrip_and_persist_two_decimals() {
        let dir = tmpdir("bg-roundtrip");
        let path = dir.join("config.toml");
        let cfg = Config {
            background_image: "~/Pictures/wall.png".into(),
            background_opacity: 0.333,
            background_dim: 0.666,
            ..Config::default()
        };
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("background_image = \"~/Pictures/wall.png\""), "{text}");
        assert!(text.contains("background_opacity = 0.33"), "{text}");
        assert!(text.contains("background_dim = 0.67"), "{text}");
        let back = Config::load_from(&path);
        assert_eq!(back.background_image, "~/Pictures/wall.png");
        assert!((back.background_opacity - 0.33).abs() < 1e-6);
        assert!((back.background_dim - 0.67).abs() < 1e-6);

        // 末尾 0 也要保留：0.5 → "0.50"。
        let path2 = dir.join("config2.toml");
        Config { background_opacity: 0.5, background_dim: 0.5, ..Config::default() }
            .save_to(&path2);
        let text2 = std::fs::read_to_string(&path2).unwrap();
        assert!(text2.contains("background_opacity = 0.50"), "{text2}");
        assert!(text2.contains("background_dim = 0.50"), "{text2}");
        let back2 = Config::load_from(&path2);
        assert!((back2.background_opacity - 0.5).abs() < f32::EPSILON);
        assert!((back2.background_dim - 0.5).abs() < f32::EPSILON);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_path_requires_existing_regular_file() {
        let dir = tmpdir("bg-path");
        let file = dir.join("wall.png");
        std::fs::write(&file, b"not-really-a-png").unwrap();

        assert_eq!(resolve_background_image("").unwrap(), None);
        assert_eq!(resolve_background_image("   ").unwrap(), None);
        assert_eq!(resolve_background_image(file.to_str().unwrap()).unwrap(), Some(file.clone()));
        let err = resolve_background_image(dir.to_str().unwrap()).unwrap_err();
        assert!(err.contains("不是普通文件"), "{err}");
        let missing = dir.join("missing.png");
        let err = resolve_background_image(missing.to_str().unwrap()).unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tilde_expands_only_home_prefix() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_tilde_with_home("~/Pictures/wall.png", home),
            home.join("Pictures/wall.png")
        );
        assert_eq!(expand_tilde_with_home("~", home), home.to_path_buf());
        assert_eq!(
            expand_tilde_with_home("~other/wall.png", home),
            PathBuf::from("~other/wall.png")
        );
        assert_eq!(expand_tilde_with_home("/abs/wall.png", home), PathBuf::from("/abs/wall.png"));
        assert_eq!(expand_tilde_with_home("  ~/a.png  ", home), home.join("a.png"));
    }

    #[test]
    fn interactive_defaults() {
        let c = Config::default();
        assert!(c.confirm_close, "默认关闭多会话要确认");
        assert!(c.copy_on_select, "默认拖选自动复制");
        assert!(c.confirm_multiline_paste, "默认多行粘贴要确认");
        assert!(!c.right_click_paste, "兼容字段默认关闭");
        assert_eq!(c.right_click, "menu");
        assert_eq!(c.effective_right_click(), "menu");
        let mut legacy = Config::default();
        legacy.right_click_paste = true;
        assert_eq!(legacy.effective_right_click(), "paste", "旧 right_click_paste 迁移为 paste");
        legacy.right_click = "off".into();
        assert_eq!(legacy.effective_right_click(), "off", "显式模式优先");
        assert!(c.paste_on_middle_click);
        assert!(!c.quake_enabled, "Quake 默认关闭");
        assert_eq!(c.quake_hotkey, "F12");
        assert_eq!(c.tabbar_position, "top", "默认 TabBar 在上");
        assert!(c.desktop_notifications, "默认允许桌面通知");
        assert!(c.visual_bell, "默认开启视觉铃");
        assert!(!c.audible_bell, "audible bell 默认关闭，避免突然响铃");
        assert!(!c.scroll_on_output, "输出默认不抢用户上滚的视图");
        assert!(c.scroll_on_keystroke, "按键默认回到底部（GNOME 默认）");
        assert!(!c.scrollback_unlimited, "默认使用固定 scrollback");
        assert_eq!(c.effective_scrollback(), 2000);
        assert_eq!(c.scrollbar_policy, "auto");
        assert_eq!(c.scrollbar_policy(), ScrollbarPolicy::Auto);
        assert_eq!(c.backspace_binding, "ascii-delete", "GNOME 默认 Backspace = 0x7f");
        assert_eq!(c.delete_binding, "delete-sequence", "GNOME 默认 Delete = ESC [3~");
        assert_eq!(c.keyboard_protocol, "auto", "默认响应应用的键盘协议 query");
        assert!(c.keyboard_protocol_enabled());
        assert_eq!(c.key_bindings().backspace, BackspaceBinding::AsciiDelete);
        assert_eq!(c.key_bindings().delete, DeleteBinding::DeleteSequence);
        assert_eq!(c.close_action, "end", "默认关闭即结束会话");
        assert_eq!(c.osc52, "allow", "OSC 52 默认放行");
        assert!(c.osc52_allowed());
        assert_eq!(c.version, SCHEMA_VERSION);
    }

    #[test]
    fn keyboard_protocol_config_parsing_and_roundtrip() {
        assert_eq!(normalize_keyboard_protocol("auto"), "auto");
        assert_eq!(normalize_keyboard_protocol(" AUTO "), "auto");
        assert_eq!(normalize_keyboard_protocol("off"), "off");
        assert_eq!(normalize_keyboard_protocol(" false "), "off");
        assert_eq!(normalize_keyboard_protocol("bogus"), "auto");
        assert!(keyboard_protocol_enabled("auto"));
        assert!(!keyboard_protocol_enabled("OFF"));

        let cfg: Config = toml::from_str("version = 1\nkeyboard_protocol = \"off\"\n").unwrap();
        assert_eq!(cfg.keyboard_protocol, "off");
        assert!(!cfg.keyboard_protocol_enabled());

        // 旧配置没有该字段时走 default = auto。
        let old: Config = toml::from_str("version = 1\nfont_size = 13.0\n").unwrap();
        assert_eq!(old.keyboard_protocol, "auto");

        let dir = tmpdir("keyboard-protocol");
        let path = dir.join("config.toml");
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("keyboard_protocol = \"off\""), "{text}");
        let back = Config::load_from(&path);
        assert_eq!(back.keyboard_protocol, "off");
        assert!(!back.keyboard_protocol_enabled());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_behavior_config_parsing_and_roundtrip() {
        // 滚动条策略：三档 + 未知值回退 auto + 大小写/空白不敏感。
        assert_eq!(ScrollbarPolicy::from_config("always"), ScrollbarPolicy::Always);
        assert_eq!(ScrollbarPolicy::from_config(" NEVER "), ScrollbarPolicy::Never);
        assert_eq!(ScrollbarPolicy::from_config("auto"), ScrollbarPolicy::Auto);
        assert_eq!(ScrollbarPolicy::from_config(""), ScrollbarPolicy::Auto);
        assert_eq!(ScrollbarPolicy::from_config("bogus"), ScrollbarPolicy::Auto);
        assert_eq!(ScrollbarPolicy::Always.as_str(), "always");

        let cfg: Config = toml::from_str(
            "version = 1\n\
             audible_bell = true\n\
             scroll_on_output = true\n\
             scroll_on_keystroke = false\n\
             scrollback_unlimited = true\n\
             sixel = false\n\
             sixel_scrolling = false\n\
             iip = false\n\
             scrollbar_policy = \"never\"\n\
             backspace_binding = \"control-h\"\n\
             delete_binding = \"ascii-del\"\n",
        )
        .unwrap();
        assert!(cfg.audible_bell);
        assert!(cfg.scroll_on_output);
        assert!(!cfg.scroll_on_keystroke);
        assert!(cfg.scrollback_unlimited);
        assert!(!cfg.sixel);
        assert!(!cfg.sixel_scrolling);
        assert!(!cfg.iip);
        assert_eq!(cfg.effective_scrollback(), UNLIMITED_SCROLLBACK_LINES, "unlimited 必须有明确安全上限");
        assert_eq!(UNLIMITED_SCROLLBACK_LINES, 100_000);
        assert_eq!(cfg.scrollbar_policy(), ScrollbarPolicy::Never);
        assert_eq!(cfg.key_bindings().backspace, BackspaceBinding::ControlH);
        assert_eq!(cfg.key_bindings().delete, DeleteBinding::AsciiDel);

        let dir = tmpdir("terminal-behavior");
        let path = dir.join("config.toml");
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("audible_bell = true"), "{text}");
        assert!(text.contains("scroll_on_output = true"), "{text}");
        assert!(text.contains("sixel = false"), "{text}");
        assert!(text.contains("iip = false"), "{text}");
        assert!(text.contains("scrollbar_policy = \"never\""), "{text}");
        assert!(text.contains("backspace_binding = \"control-h\""), "{text}");
        let back = Config::load_from(&path);
        assert!(back.audible_bell);
        assert!(back.scroll_on_output);
        assert!(!back.sixel);
        assert!(!back.sixel_scrolling);
        assert!(!back.iip);
        assert!(!back.scroll_on_keystroke);
        assert_eq!(back.scrollbar_policy(), ScrollbarPolicy::Never);
        assert_eq!(back.key_bindings(), cfg.key_bindings());
        let _ = std::fs::remove_dir_all(&dir);

        // 旧配置没有 iip 字段：additive，默认开启。
        let old: Config = toml::from_str("version = 1\nfont_size = 13.0\n").unwrap();
        assert!(old.iip, "旧配置缺 iip 时必须默认开启");

        // 固定 scrollback 时忽略 unlimited 上限，保持用户配置的数值。
        let fixed = Config { scrollback: 1234, ..Config::default() };
        assert_eq!(fixed.effective_scrollback(), 1234);
        let unlimited = Config { scrollback: 10, scrollback_unlimited: true, ..Config::default() };
        assert_eq!(unlimited.effective_scrollback(), UNLIMITED_SCROLLBACK_LINES);
    }

    #[test]
    fn osc52_config_allow_deny_and_unknown() {
        let cfg: Config = toml::from_str("version = 1\nosc52 = \"deny\"\n").unwrap();
        assert!(!cfg.osc52_allowed());
        let cfg: Config = toml::from_str("version = 1\nosc52 = \"ALLOW\"\n").unwrap();
        assert!(cfg.osc52_allowed());
        // 拼错/未知值按默认 allow；空值也按默认。
        let cfg: Config = toml::from_str("version = 1\nosc52 = \"nope\"\n").unwrap();
        assert!(cfg.osc52_allowed());
        let cfg: Config = toml::from_str("version = 1\n").unwrap();
        assert_eq!(cfg.osc52, "allow", "旧配置缺 osc52 时默认 allow");

        let dir = tmpdir("osc52");
        let path = dir.join("config.toml");
        Config { osc52: "deny".into(), ..Config::default() }.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("osc52 = \"deny\""), "{text}");
        assert!(!Config::load_from(&path).osc52_allowed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_config_is_backed_up_and_migrated() {
        let dir = tmpdir("migrate");
        let path = dir.join("config.toml");
        std::fs::write(&path, "theme = \"nord\"\nfont_size = 13.0\n").unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.version, SCHEMA_VERSION);
        assert_eq!(cfg.theme, "nord");
        assert!((cfg.font_size - 13.0).abs() < f32::EPSILON);

        // 备份保留原始内容
        let backup = dir.join("config.toml.v0.bak");
        let text = std::fs::read_to_string(&backup).expect("应生成迁移前备份");
        assert!(text.contains("nord"));
        assert!(!text.contains("version"));

        // 迁移后的文件已是新版本，且没有临时文件残留
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("version = 1"), "{migrated}");
        // v0 没有外观字段：迁移写回时补上 GNOME 默认值。
        assert!(migrated.contains("cursor_shape = \"block\""), "{migrated}");
        assert!(migrated.contains("cursor_blink = \"system\""), "{migrated}");
        assert!(migrated.contains("bold_is_bright = true"), "{migrated}");
        assert!(!dir.join("config.toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_version_is_not_backed_up_again() {
        let dir = tmpdir("nobackup");
        let path = dir.join("config.toml");
        let cfg = Config { theme: "dracula".into(), ..Config::default() };
        cfg.save_to(&path);

        let again = Config::load_from(&path);
        assert_eq!(again.theme, "dracula");
        assert!(!dir.join("config.toml.v1.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_keys_survive_roundtrip() {
        let dir = tmpdir("extra");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "version = 1\ntheme = \"nord\"\nfuture_option = 42\n[future_table]\nx = \"y\"\n",
        )
        .unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.extra.get("future_option").and_then(|v| v.as_integer()), Some(42));

        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("future_option = 42"), "{text}");
        assert!(text.contains("future_table"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_toml_keeps_original_file() {
        let dir = tmpdir("broken");
        let path = dir.join("config.toml");
        let broken = "theme = [unclosed\n";
        std::fs::write(&path, broken).unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.theme, Config::default().theme);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken, "解析失败不能覆盖原文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_nested_dirs() {
        let dir = tmpdir("nested");
        let path = dir.join("a/b/config.toml");
        let cfg = Config::default();
        cfg.save_to(&path);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hotkeys_default_to_empty_and_old_config_loads() {
        assert!(Config::default().hotkeys.is_empty());
        // 旧配置没有 hotkeys 字段：serde(default) 兼容，不能解析失败。
        let dir = tmpdir("hotkeys-old");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\ntheme = \"nord\"\n").unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.theme, "nord");
        assert!(cfg.hotkeys.is_empty());

        // 手写的大写 chord 读入时归一成小写规范格式。
        std::fs::write(
            &path,
            "version = 1\n[[hotkeys]]\ncommand = \"tab.new\"\nchord = \"Ctrl + Shift + N\"\n",
        )
        .unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.hotkeys[0].chord, "ctrl+shift+n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_chord_hotkeys_are_canonicalized() {
        let dir = tmpdir("hotkeys-multi");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "version = 1\n[[hotkeys]]\ncommand = \"tab.new\"\nchord = \"Ctrl + K Ctrl + T\"\n",
        )
        .unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.hotkeys[0].chord, "ctrl+k ctrl+t");
        // 规范格式再解析仍是同一序列（分发/校验共用）。
        let back = commands::Shortcut::parse(&cfg.hotkeys[0].chord).unwrap();
        assert_eq!(back.chords.len(), 2);
        assert_eq!(back.canonical(), "ctrl+k ctrl+t");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hotkeys_roundtrip_and_are_written_as_tables() {
        let dir = tmpdir("hotkeys-roundtrip");
        let path = dir.join("config.toml");
        let cfg = Config {
            hotkeys: vec![
                HotkeyOverride { command: "tab.new".into(), chord: "ctrl+shift+n".into() },
                HotkeyOverride { command: "terminal.paste".into(), chord: "ctrl+shift+v".into() },
            ],
            ..Config::default()
        };
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[hotkeys]]"), "{text}");
        assert!(text.contains("command = \"tab.new\""), "{text}");
        assert!(text.contains("chord = \"ctrl+shift+n\""), "{text}");

        let back = Config::load_from(&path);
        assert_eq!(back.hotkeys, cfg.hotkeys);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hotkeys_and_unknown_keys_roundtrip_together() {
        let dir = tmpdir("hotkeys-extra");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "version = 1\ntheme = \"nord\"\nfuture_option = 42\n\n[[hotkeys]]\ncommand = \"tab.new\"\nchord = \"ctrl+shift+n\"\n\n[future_table]\nx = \"y\"\n",
        )
        .unwrap();

        let cfg = Config::load_from(&path);
        assert_eq!(cfg.hotkeys.len(), 1);
        assert_eq!(cfg.hotkeys[0].command, "tab.new");
        assert_eq!(cfg.extra.get("future_option").and_then(|v| v.as_integer()), Some(42));

        cfg.save_to(&path);
        let again = Config::load_from(&path);
        assert_eq!(again.hotkeys, cfg.hotkeys);
        assert_eq!(again.extra.get("future_option").and_then(|v| v.as_integer()), Some(42));
        assert_eq!(
            again.extra.get("future_table").and_then(|v| v.as_table()),
            cfg.extra.get("future_table").and_then(|v| v.as_table())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_hotkeys_are_omitted_from_toml() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("hotkeys"), "{text}");
    }

    #[test]
    fn terminal_menu_table_roundtrips_and_default_is_empty() {
        // 默认空表 = 内置菜单（默认行为不变），且不往用户 config 里写噪音。
        assert!(Config::default().terminal_menu.is_empty());
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("terminal_menu"), "{text}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"[[terminal_menu]]
id = "terminal.copy"
label = "Copy Text"
shortcut = "Ctrl+C"

[[terminal_menu]]
separator = true

[[terminal_menu]]
id = "terminal.recording-start"
danger = true
"#,
        )
        .unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.terminal_menu.len(), 3, "config 表按文件顺序读入");
        assert_eq!(cfg.terminal_menu[0].id, "terminal.copy");
        assert_eq!(cfg.terminal_menu[0].label, "Copy Text");
        assert_eq!(cfg.terminal_menu[0].shortcut, "Ctrl+C");
        assert!(cfg.terminal_menu[1].separator);
        assert_eq!(cfg.terminal_menu[2].id, "terminal.recording-start");
        assert_eq!(cfg.terminal_menu[2].danger, Some(true));

        cfg.save_to(&path);
        let again = Config::load_from(&path);
        assert_eq!(again.terminal_menu, cfg.terminal_menu);
    }

    #[test]
    fn appearance_defaults_match_gnome() {
        let c = Config::default();
        assert_eq!(c.cursor_shape, "block", "GNOME cursor-shape 默认 block");
        assert_eq!(c.cursor_blink, "system", "GNOME cursor-blink 默认 system");
        assert!(c.cursor_blinking(), "system 按开处理");
        assert_eq!(c.cursor_shape(), tervo_core::CursorShape::Block);
        assert!(c.cursor_color.is_empty(), "空 = 跟随主题");
        assert!(c.cursor_text_color.is_empty(), "空 = 跟随主题背景");
        assert!(c.bold_is_bright, "GNOME bold-is-bright 默认 true");
        assert!(c.bold_color.is_empty());
        assert!(c.bold_color_same_as_fg, "GNOME bold-color-same-as-fg 默认 true");
        assert_eq!(c.cursor_color_rgb(), None);
        assert_eq!(c.bold_color_rgb(), None);
    }

    #[test]
    fn old_config_gets_appearance_defaults() {
        // v1 之前的文件没有这些键：serde(default) + Config::default 必须补齐。
        let cfg: Config =
            toml::from_str("version = 1\ntheme = \"nord\"\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.theme, "nord");
        assert_eq!(cfg.cursor_shape, "block");
        assert_eq!(cfg.cursor_blink, "system");
        assert!(cfg.bold_is_bright);
        assert!(cfg.bold_color_same_as_fg);
        // 没有 version 的 v0 文件同样按默认补齐。
        let v0: Config = toml::from_str("theme = \"nord\"\n").unwrap();
        assert_eq!(v0.cursor_shape, "block");
        assert!(v0.bold_is_bright);
    }

    #[test]
    fn language_defaults_to_english_and_old_config_is_compatible() {
        assert_eq!(Config::default().language, "en");
        assert_eq!(Config::default().lang(), "en");
        // 旧配置没有 language 字段：serde(default) 补齐 en，不能解析失败。
        let cfg: Config = toml::from_str("version = 1\ntheme = \"nord\"\n").unwrap();
        assert_eq!(cfg.language, "en");
        assert_eq!(cfg.lang(), "en");
    }

    #[test]
    fn language_parses_zh_and_unknown_falls_back_to_en() {
        let cfg = Config::parse_text("version = 1\nlanguage = \"zh\"\n").unwrap();
        assert_eq!(cfg.lang(), "zh");
        // 大小写/区域写法都归一成规范 zh。
        assert_eq!(normalize_language(" ZH "), "zh");
        assert_eq!(normalize_language("zh-CN"), "zh");
        // 未知/拼错/空值一律回退 en（additive 字段不能因为写错而解析失败）。
        for raw in ["", "fr", "english!", "zho"] {
            let cfg =
                Config::parse_text(&format!("version = 1\nlanguage = \"{raw}\"\n")).unwrap();
            assert_eq!(cfg.language, "en", "raw={raw:?}");
            assert_eq!(cfg.lang(), "en");
        }
        assert_eq!(normalize_language("chinese"), "zh");
        assert_eq!(normalize_language("zh-TW"), "zh");
    }

    #[test]
    fn language_hot_reload_and_roundtrip() {
        let dir = tmpdir("language-roundtrip");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\nlanguage = \"zh\"\n").unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.lang(), "zh");
        // save_to 必须把 language 写回，watcher 才能观察到热更新。
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("language = \"zh\""), "{text}");
        let again = Config::parse_text(&text).unwrap();
        assert_eq!(again.lang(), "zh");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_color_parses_short_long_and_rejects_garbage() {
        assert_eq!(parse_hex_color("#ff0000"), Some(0x00ff_0000));
        assert_eq!(parse_hex_color("FF8000"), Some(0x00ff_8000));
        assert_eq!(parse_hex_color("#f80"), Some(0x00ff_8800));
        assert_eq!(parse_hex_color("  #00ff00  "), Some(0x0000_ff00));
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#gg0000"), None);
        assert_eq!(parse_hex_color("#ff00"), None);
        assert_eq!(normalize_hex_color("#F80"), Some("#ff8800".into()));
        assert_eq!(normalize_hex_color("nope"), None);
    }

    #[test]
    fn cursor_shape_and_blink_config_normalize() {
        let dir = tmpdir("cursor-shape-blink");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\ncursor_shape = \"I-Beam\"\ncursor_blink = \"OFF\"\n")
            .unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.cursor_shape, "ibeam");
        assert_eq!(cfg.cursor_shape(), tervo_core::CursorShape::Beam);
        assert!(!cfg.cursor_blinking());
        // on/system 都开；未知值按 system。
        let cfg: Config = toml::from_str("version = 1\ncursor_blink = \"on\"\n").unwrap();
        assert!(cfg.cursor_blinking());
        assert_eq!(normalize_cursor_blink("nope"), "system");
        assert_eq!(normalize_cursor_shape("underline"), "underline");
        assert_eq!(normalize_cursor_shape("BAR"), "ibeam");
        assert_eq!(normalize_cursor_shape("nope"), "block");
        assert_eq!(normalize_cursor_shape(""), "block");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appearance_colors_normalize_and_invalid_falls_back_to_theme() {
        let dir = tmpdir("appearance-colors");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "version = 1\ncursor_shape = \"underline\"\ncursor_color = \"#F80\"\ncursor_text_color = \"#00ff00\"\nbold_color = \"not-a-color\"\n",
        )
        .unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.cursor_color, "#ff8800");
        assert_eq!(cfg.cursor_text_color, "#00ff00");
        assert_eq!(cfg.cursor_color_rgb(), Some(0x00ff_8800));
        assert!(cfg.bold_color.is_empty(), "非法颜色回退主题/前景");
        assert_eq!(cfg.bold_color_rgb(), None);
        // load_from 只归一内存值，不因 additive 字段重写 v1 文件。
        assert!(!dir.join("config.toml.v1.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appearance_roundtrip() {
        let dir = tmpdir("appearance-roundtrip");
        let path = dir.join("config.toml");
        let cfg = Config {
            cursor_shape: "ibeam".into(),
            cursor_blink: "off".into(),
            cursor_color: "#123456".into(),
            cursor_text_color: "#abcdef".into(),
            bold_is_bright: false,
            bold_color: "#fedcba".into(),
            bold_color_same_as_fg: false,
            ..Config::default()
        };
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("cursor_shape = \"ibeam\""), "{text}");
        assert!(text.contains("cursor_blink = \"off\""), "{text}");
        assert!(text.contains("cursor_color = \"#123456\""), "{text}");
        assert!(text.contains("cursor_text_color = \"#abcdef\""), "{text}");
        assert!(text.contains("bold_is_bright = false"), "{text}");
        assert!(text.contains("bold_color = \"#fedcba\""), "{text}");
        assert!(text.contains("bold_color_same_as_fg = false"), "{text}");
        let back = Config::load_from(&path);
        assert_eq!(back.cursor_shape, "ibeam");
        assert_eq!(back.cursor_blink, "off");
        assert_eq!(back.cursor_color_rgb(), Some(0x0012_3456));
        assert_eq!(back.cursor_text_color_rgb(), Some(0x00ab_cdef));
        assert!(!back.bold_is_bright);
        assert_eq!(back.bold_color_rgb(), Some(0x00fe_dcba));
        assert!(!back.bold_color_same_as_fg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_defaults_are_enabled_read_only_and_old_config_compatible() {
        let cfg = Config::default();
        assert!(cfg.mcp_enabled, "MCP 默认开启");
        assert!(cfg.mcp_read, "MCP 默认只读（read 开）");
        assert!(!cfg.mcp_input, "input 默认关");
        assert!(!cfg.mcp_tabs, "tabs 默认关");
        assert!(!cfg.mcp_destructive, "destructive 默认关");

        // v1 旧文件没有 MCP 字段：容器级 serde(default) 必须补默认值。
        let old: Config = toml::from_str("version = 1\ntheme = \"nord\"\n").unwrap();
        assert!(old.mcp_enabled && old.mcp_read);
        assert!(!old.mcp_input && !old.mcp_tabs && !old.mcp_destructive);
        // v0 文件同样按默认补齐，且不改变既有迁移规则。
        let v0: Config = toml::from_str("theme = \"nord\"\n").unwrap();
        assert_eq!(v0.version, 0);
        assert!(v0.mcp_enabled && v0.mcp_read);
        assert!(!v0.mcp_input && !v0.mcp_tabs && !v0.mcp_destructive);
    }

    #[test]
    fn mcp_flags_roundtrip_as_flat_toml() {
        let dir = tmpdir("mcp-roundtrip");
        let path = dir.join("config.toml");
        let cfg = Config {
            mcp_enabled: false,
            mcp_read: false,
            mcp_input: true,
            mcp_tabs: true,
            mcp_destructive: true,
            ..Config::default()
        };
        cfg.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        for line in [
            "mcp_enabled = false",
            "mcp_read = false",
            "mcp_input = true",
            "mcp_tabs = true",
            "mcp_destructive = true",
        ] {
            assert!(text.contains(line), "missing {line}:\n{text}");
        }
        assert!(!text.contains("[mcp]"), "MCP 必须是 flat TOML:\n{text}");
        let back = Config::load_from(&path);
        assert!(!back.mcp_enabled && !back.mcp_read);
        assert!(back.mcp_input && back.mcp_tabs && back.mcp_destructive);
        // additive 字段不 bump schema，也不产生 v1 备份。
        assert_eq!(back.version, SCHEMA_VERSION);
        assert!(!dir.join("config.toml.v1.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_defaults_are_written_by_v0_migration() {
        let dir = tmpdir("mcp-migrate");
        let path = dir.join("config.toml");
        std::fs::write(&path, "theme = \"nord\"\n").unwrap();
        let cfg = Config::load_from(&path);
        assert!(cfg.mcp_enabled && cfg.mcp_read);
        assert!(!cfg.mcp_input && !cfg.mcp_tabs && !cfg.mcp_destructive);
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("mcp_enabled = true"), "{migrated}");
        assert!(migrated.contains("mcp_read = true"), "{migrated}");
        assert!(migrated.contains("mcp_input = false"), "{migrated}");
        assert!(dir.join("config.toml.v0.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inspection_reports_version_backup_and_parse_error() {
        let dir = tmpdir("inspect");
        let path = dir.join("config.toml");
        // 不存在：不算错误，版本未知。
        let missing = Config::inspect_file(&path);
        assert_eq!(missing.version, None);
        assert!(missing.backup.is_none());
        assert!(missing.summary(true).is_empty());

        std::fs::write(&path, "language = \"zh\"\nversion = 1\n").unwrap();
        let ok = Config::inspect_file(&path);
        assert_eq!(ok.version, Some(1));
        assert_eq!(ok.current_version, SCHEMA_VERSION);
        assert!(ok.parse_error.is_none());

        // 迁移备份按版本号取最大者。
        std::fs::write(dir.join("config.toml.v0.bak"), "version = 0\n").unwrap();
        std::fs::write(dir.join("config.toml.v3.bak"), "version = 3\n").unwrap();
        let with_backup = Config::inspect_file(&path);
        assert_eq!(with_backup.backup_version, Some(3));
        assert!(with_backup.summary(true).contains("可回滚"));
        assert!(with_backup.summary(false).contains("Rollback"));

        // 坏文件：报告解析错误且明确“原文件未改”。
        std::fs::write(&path, "oops = [").unwrap();
        let broken = Config::inspect_file(&path);
        assert!(broken.parse_error.is_some());
        assert!(broken.summary(true).contains("原文件未改"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_import_roundtrip_and_version_gate() {
        let dir = tmpdir("export");
        let export = dir.join("config-export.toml");
        let mut cfg = Config::default();
        cfg.language = "zh".into();
        cfg.font_size = 15.0;
        cfg.export_to(&export).unwrap();

        let imported = Config::import_from(&export).unwrap();
        assert_eq!(imported.language, "zh");
        assert_eq!(imported.font_size, 15.0);
        assert_eq!(imported.version, SCHEMA_VERSION);

        // 未来版本拒绝导入（避免字段被静默丢弃）。
        std::fs::write(&export, format!("version = {}\n", SCHEMA_VERSION + 1)).unwrap();
        let err = Config::import_from(&export).unwrap_err();
        assert!(err.contains("拒绝导入"), "{err}");
        // 坏 TOML 拒绝。
        std::fs::write(&export, "version = [").unwrap();
        assert!(Config::import_from(&export).is_err());
        // 不存在的文件给可读错误。
        assert!(Config::import_from(&dir.join("nope.toml")).is_err());
    }

    #[test]
    fn restore_latest_backup_overwrites_only_after_validation() {
        let dir = tmpdir("restore");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\nlanguage = \"en\"\n").unwrap();
        std::fs::write(dir.join("config.toml.v0.bak"), "version = 0\nlanguage = \"zh\"\n").unwrap();
        let used = Config::restore_latest_backup(&path).unwrap();
        assert_eq!(used.file_name().unwrap(), "config.toml.v0.bak");
        let restored = std::fs::read_to_string(&path).unwrap();
        assert!(restored.contains("zh"), "{restored}");

        // 坏备份不能覆盖当前配置。
        std::fs::write(&path, "version = 1\nlanguage = \"en\"\n").unwrap();
        std::fs::write(dir.join("config.toml.v0.bak"), "broken = [").unwrap();
        assert!(Config::restore_latest_backup(&path).is_err());
        assert!(std::fs::read_to_string(&path).unwrap().contains("en"));
        // 没有备份时明确报错。
        std::fs::remove_file(dir.join("config.toml.v0.bak")).unwrap();
        assert!(Config::restore_latest_backup(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
