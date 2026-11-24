// SPDX-License-Identifier: GPL-3.0-only
//! 主题注册表：内置主题 + `~/.config/xtervo/themes/` 目录。
//!
//! 支持直接丢现成主题文件进来（不用手写 xtervo 格式）：
//! - `*.toml`        xtervo 完整主题（可带 `[ui]` + `pair`）或 Alacritty TOML
//! - `*.itermcolors` iTerm2 配色（XML plist）
//! - `*.json`        Windows Terminal `schemes` 数组 / 单个 scheme 对象
//! - `*.conf`        kitty 配色
//! - `*.Xresources` / `*.txt` / 无扩展名   Xresources（Tabby 社区配色同格式）
//! - `*.yaml` / `*.yml`  简单配色 YAML（Catppuccin / Rosé Pine 的 Tabby 片段）
//!
//! 导入格式只有 16 色 + fg/bg/cursor，界面 chrome 由 `UiColors::derive` 自动推导；
//! 自定义 xtervo 格式可以精确控制 11 个 UI 颜色并声明深浅 `pair`。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use xtervo_core::{Palette, PaletteSpec, Theme, UiColors};
use serde::Deserialize;

pub const CUSTOM_PREFIX: &str = "custom:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Dark,
    Light,
}

impl Variant {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Variant::Dark),
            "light" => Some(Variant::Light),
            _ => None,
        }
    }
}

/// 一套可直接下发的主题。
#[derive(Clone, Debug)]
pub struct ThemeData {
    /// 稳定 id：内置主题如 `yaru-dark`，自定义主题 `custom:<slug>`。
    pub id: String,
    /// 设置面板显示名。
    pub label: String,
    pub variant: Variant,
    pub spec: PaletteSpec,
    pub ui: UiColors,
    /// 自定义主题的深/浅配对（完整 id，如 `custom:my-light`）。
    pub pair: Option<String>,
}

impl ThemeData {
    pub fn palette(&self) -> Palette {
        Palette::from_spec(&self.spec)
    }
}

/// 内置主题 → ThemeData。
pub fn builtin(theme: Theme) -> ThemeData {
    ThemeData {
        id: theme.id().to_string(),
        label: theme.label().to_string(),
        variant: if theme.is_dark() { Variant::Dark } else { Variant::Light },
        spec: theme.spec(),
        ui: theme.ui(),
        pair: None,
    }
}

pub struct ThemeRegistry {
    builtins: Vec<ThemeData>,
    customs: Vec<ThemeData>,
    /// 加载/校验中的问题，供启动日志与设置页诊断。
    pub errors: Vec<String>,
}

impl ThemeRegistry {
    pub fn load(dir: &Path) -> Self {
        let builtins: Vec<ThemeData> = Theme::ALL.iter().copied().map(builtin).collect();
        let mut errors = Vec::new();
        let mut customs = Vec::new();

        match std::fs::read_dir(dir) {
            Ok(entries) => {
                let mut paths: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && is_theme_file(p))
                    .collect();
                paths.sort();
                for path in paths {
                    match load_one(&path) {
                        Ok(theme) => customs.push(theme),
                        Err(e) => errors.push(format!("{}: {e}", path.display())),
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => errors.push(format!("{}: 无法读取主题目录: {e}", dir.display())),
        }

        // 同名文件 slug 后可能撞 id：保留第一个。
        let mut seen = HashSet::new();
        customs.retain(|t| {
            if seen.insert(t.id.clone()) {
                true
            } else {
                errors.push(format!("{}: 主题 id 重复，已跳过", t.id));
                false
            }
        });

        validate_pairs(&mut customs, &mut errors);
        Self { builtins, customs, errors }
    }

    #[allow(dead_code)]
    pub fn from_parts(builtins: Vec<ThemeData>, customs: Vec<ThemeData>) -> Self {
        Self { builtins, customs, errors: Vec::new() }
    }

    /// 加入一个插件声明的主题（id 已加 `plugin-` 前缀）。重复 id 只记录错误，
    /// 不覆盖已有主题；主题坏掉不影响插件本身。
    pub fn add_custom(&mut self, theme: ThemeData) {
        if self.get(&theme.id).is_some() {
            self.errors.push(format!("{}: 主题 id 重复，已跳过", theme.id));
            return;
        }
        self.customs.push(theme);
        validate_pairs(&mut self.customs, &mut self.errors);
    }

    pub fn get(&self, id: &str) -> Option<&ThemeData> {
        self.builtins.iter().chain(self.customs.iter()).find(|t| t.id == id)
    }

    pub fn all(&self) -> impl Iterator<Item = &ThemeData> {
        self.builtins.iter().chain(self.customs.iter())
    }

    #[allow(dead_code)]
    pub fn custom_count(&self) -> usize {
        self.customs.len()
    }

    /// auto 未显式配置时的深色默认：Yaru Dark（贴合 Ubuntu/GNOME 系统主题）。
    pub fn dark_default(&self) -> &ThemeData {
        self.get("yaru-dark")
            .or_else(|| self.get("tokyo-night"))
            .unwrap_or(&self.builtins[0])
    }

    /// auto 未显式配置时的浅色默认：Yaru Light。
    pub fn light_default(&self) -> &ThemeData {
        self.get("yaru-light").unwrap_or_else(|| {
            self.builtins
                .iter()
                .find(|t| t.variant == Variant::Light)
                .unwrap_or(&self.builtins[0])
        })
    }

    /// 主题目录：`XTERVO_THEMES_DIR` > 配置文件同级的 `themes/`。
    pub fn default_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("XTERVO_THEMES_DIR") {
            return PathBuf::from(dir);
        }
        crate::config::Config::path()
            .parent()
            .map(|p| p.join("themes"))
            .unwrap_or_else(|| PathBuf::from("themes"))
    }
}

/// auto 模式下选一侧：
/// 1) 显式配置且 variant 匹配；
/// 2) 另一侧显式配置的主题的 `pair`；
/// 3) 内置默认（Yaru Dark / Yaru Light）。
pub fn resolve_auto(
    reg: &ThemeRegistry,
    want: Variant,
    auto_dark: Option<&str>,
    auto_light: Option<&str>,
) -> ThemeData {
    let (explicit, other) = match want {
        Variant::Dark => (auto_dark, auto_light),
        Variant::Light => (auto_light, auto_dark),
    };
    if let Some(data) = explicit.and_then(|id| reg.get(id)) {
        if data.variant == want {
            return data.clone();
        }
    }
    if let Some(pair_id) = other.and_then(|id| reg.get(id)).and_then(|d| d.pair.as_deref()) {
        if let Some(data) = reg.get(pair_id) {
            if data.variant == want {
                return data.clone();
            }
        }
    }
    match want {
        Variant::Dark => reg.dark_default().clone(),
        Variant::Light => reg.light_default().clone(),
    }
}

// ---------------------------------------------------------------- 文件分发

/// 目录里可能混着 README/截图：只处理认识的后缀，其余静默跳过。
fn is_theme_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.starts_with('.') {
        return false;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "" | "toml" | "itermcolors" | "json" | "yaml" | "yml" | "conf" | "config" | "kitty"
            | "xresources" | "txt" | "colors" | "theme"
    )
}

/// 加载插件目录内的声明式主题文件（TOML/Alacritty 等既有格式）。
///
/// 主题 id 统一加 `custom:plugin-<插件名>-` 前缀，避免与 `~/.config/xtervo/themes`
/// 里的用户主题撞 id；显示名追加来源，方便在设置页辨认。
pub fn load_plugin_theme(path: &Path, plugin: &str) -> Result<ThemeData, String> {
    let mut theme = load_one(path)?;
    let plugin_slug = slugify(plugin).ok_or_else(|| format!("插件名 {plugin:?} 无法生成主题前缀"))?;
    let stem = theme
        .id
        .strip_prefix(CUSTOM_PREFIX)
        .unwrap_or(theme.id.as_str())
        .to_string();
    theme.id = format!("{CUSTOM_PREFIX}plugin-{plugin_slug}-{stem}");
    theme.label = format!("{} (plugin: {})", theme.label, plugin);
    Ok(theme)
}

fn load_one(path: &Path) -> Result<ThemeData, String> {
    let raw_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "文件名不是合法 UTF-8".to_string())?;
    let stem = slugify(raw_stem).ok_or_else(|| format!("文件名 {raw_stem:?} 无法生成主题 id"))?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "toml" => load_toml(&text, &stem, raw_stem),
        "itermcolors" => load_itermcolors(&text, &stem, raw_stem),
        "json" => load_windows_terminal(&text, &stem, raw_stem),
        "yaml" | "yml" => load_simple_yaml(&text, &stem, raw_stem),
        "conf" | "config" | "kitty" => load_kitty(&text, &stem, raw_stem),
        "" | "xresources" | "txt" | "colors" | "theme" => load_xresources(&text, &stem, raw_stem),
        other => Err(format!("不支持的主题格式 .{other}")),
    }
}

fn imported(stem: &str, label: Option<String>, spec: PaletteSpec) -> ThemeData {
    let variant = if is_dark_bg(spec.bg) { Variant::Dark } else { Variant::Light };
    ThemeData {
        id: format!("{CUSTOM_PREFIX}{stem}"),
        label: label.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| stem.to_string()),
        variant,
        ui: UiColors::derive(&spec),
        spec,
        pair: None,
    }
}

pub(crate) fn is_dark_bg(bg: u32) -> bool {
    let ch = |s: u32| ((bg >> s) & 0xff) as f32 / 255.0;
    0.299 * ch(16) + 0.587 * ch(8) + 0.114 * ch(0) < 0.5
}

// ---------------------------------------------------------------- xtervo TOML

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    #[serde(default = "default_schema")]
    schema: u32,
    name: String,
    variant: String,
    pair: Option<String>,
    foreground: String,
    background: String,
    cursor: String,
    bright_foreground: Option<String>,
    dim_foreground: Option<String>,
    ansi: Vec<String>,
    ui: RawUi,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUi {
    bg: String,
    surface: String,
    raise: String,
    hover: String,
    border: String,
    text: String,
    subtext: String,
    muted: String,
    accent: String,
    #[serde(rename = "accent-text")]
    accent_text: String,
    danger: String,
}

fn default_schema() -> u32 {
    1
}

fn load_toml(text: &str, stem: &str, raw_stem: &str) -> Result<ThemeData, String> {
    if let Ok(raw) = toml::from_str::<RawTheme>(text) {
        return parse_raw(stem, raw);
    }
    // 退回 Alacritty TOML
    let alt: AlacrittyFile = toml::from_str(text)
        .map_err(|e| format!("既不是 xtervo 主题也不是 Alacritty 配色: {e}"))?;
    Ok(imported(stem, Some(raw_stem.to_string()), alt.into_spec()?))
}

fn parse_raw(stem: &str, raw: RawTheme) -> Result<ThemeData, String> {
    if raw.schema != 1 {
        return Err(format!("schema 必须为 1（当前 {}）", raw.schema));
    }
    let name = raw.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err("name 不能为空且不超过 64 字符".into());
    }
    let variant = Variant::parse(&raw.variant)
        .ok_or_else(|| format!("variant 必须是 dark/light（当前 {:?}）", raw.variant))?;
    if raw.ansi.len() != 16 {
        return Err(format!("ansi 必须恰好 16 项（当前 {}）", raw.ansi.len()));
    }
    let mut ansi16 = [0u32; 16];
    for (i, s) in raw.ansi.iter().enumerate() {
        ansi16[i] = parse_hex(s).map_err(|e| format!("ansi[{i}]: {e}"))?;
    }
    let fg = parse_hex(&raw.foreground).map_err(|e| format!("foreground: {e}"))?;
    let bg = parse_hex(&raw.background).map_err(|e| format!("background: {e}"))?;
    let cursor = parse_hex(&raw.cursor).map_err(|e| format!("cursor: {e}"))?;
    let bright_fg = raw
        .bright_foreground
        .as_deref()
        .map(parse_hex)
        .transpose()
        .map_err(|e| format!("bright_foreground: {e}"))?
        .unwrap_or(fg);
    let dim_fg = raw
        .dim_foreground
        .as_deref()
        .map(parse_hex)
        .transpose()
        .map_err(|e| format!("dim_foreground: {e}"))?
        .unwrap_or(fg);

    let ui = UiColors {
        bg: parse_hex(&raw.ui.bg).map_err(|e| format!("ui.bg: {e}"))?,
        surface: parse_hex(&raw.ui.surface).map_err(|e| format!("ui.surface: {e}"))?,
        raise: parse_hex(&raw.ui.raise).map_err(|e| format!("ui.raise: {e}"))?,
        hover: parse_hex(&raw.ui.hover).map_err(|e| format!("ui.hover: {e}"))?,
        border: parse_hex(&raw.ui.border).map_err(|e| format!("ui.border: {e}"))?,
        text: parse_hex(&raw.ui.text).map_err(|e| format!("ui.text: {e}"))?,
        subtext: parse_hex(&raw.ui.subtext).map_err(|e| format!("ui.subtext: {e}"))?,
        muted: parse_hex(&raw.ui.muted).map_err(|e| format!("ui.muted: {e}"))?,
        accent: parse_hex(&raw.ui.accent).map_err(|e| format!("ui.accent: {e}"))?,
        accent_text: parse_hex(&raw.ui.accent_text)
            .map_err(|e| format!("ui.accent-text: {e}"))?,
        danger: parse_hex(&raw.ui.danger).map_err(|e| format!("ui.danger: {e}"))?,
    };

    let pair = raw.pair.map(|p| format!("{CUSTOM_PREFIX}{}", slugify(&p).unwrap_or_default()));

    Ok(ThemeData {
        id: format!("{CUSTOM_PREFIX}{stem}"),
        label: name.to_string(),
        variant,
        spec: PaletteSpec { fg, bg, cursor, bright_fg, dim_fg, ansi16 },
        ui,
        pair,
    })
}

// ---------------------------------------------------------------- Alacritty TOML

#[derive(Deserialize)]
struct AlacrittyFile {
    colors: AlacrittyColors,
}

#[derive(Deserialize)]
struct AlacrittyColors {
    primary: AlacrittyPrimary,
    cursor: Option<AlacrittyCursor>,
    normal: AlacrittyNamed,
    bright: AlacrittyNamed,
}

#[derive(Deserialize)]
struct AlacrittyPrimary {
    background: String,
    foreground: String,
}

#[derive(Deserialize)]
struct AlacrittyCursor {
    cursor: String,
}

#[derive(Deserialize)]
struct AlacrittyNamed {
    black: String,
    red: String,
    green: String,
    yellow: String,
    blue: String,
    magenta: String,
    cyan: String,
    white: String,
}

impl AlacrittyFile {
    fn into_spec(self) -> Result<PaletteSpec, String> {
        let c = self.colors;
        let fg = parse_loose(&c.primary.foreground)?;
        let bg = parse_loose(&c.primary.background)?;
        let cursor = c.cursor.map(|x| parse_loose(&x.cursor)).transpose()?.unwrap_or(fg);
        let mut ansi16 = [0u32; 16];
        let normal = [
            c.normal.black, c.normal.red, c.normal.green, c.normal.yellow,
            c.normal.blue, c.normal.magenta, c.normal.cyan, c.normal.white,
        ];
        let bright = [
            c.bright.black, c.bright.red, c.bright.green, c.bright.yellow,
            c.bright.blue, c.bright.magenta, c.bright.cyan, c.bright.white,
        ];
        for (i, s) in normal.iter().enumerate() {
            ansi16[i] = parse_loose(s)?;
        }
        for (i, s) in bright.iter().enumerate() {
            ansi16[8 + i] = parse_loose(s)?;
        }
        Ok(PaletteSpec { fg, bg, cursor, bright_fg: fg, dim_fg: fg, ansi16 })
    }
}

// ---------------------------------------------------------------- iTerm2

fn load_itermcolors(text: &str, stem: &str, raw_stem: &str) -> Result<ThemeData, String> {
    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(text, opts)
        .map_err(|e| format!("XML 解析失败: {e}"))?;
    let dict = doc
        .descendants()
        .find(|n| n.has_tag_name("dict"))
        .ok_or_else(|| "itermcolors 里找不到 <dict>".to_string())?;

    let mut entries: HashMap<String, u32> = HashMap::new();
    for node in dict.children().filter(|n| n.is_element()) {
        if !node.has_tag_name("key") {
            continue;
        }
        let Some(color_dict) = node.next_sibling_element() else { continue };
        if !color_dict.has_tag_name("dict") {
            continue;
        }
        let mut rgb = [0f32; 3];
        for item in color_dict.children().filter(|n| n.is_element()) {
            if item.has_tag_name("key") {
                let Some(v) = item.next_sibling_element().and_then(|n| n.text()) else { continue };
                let Ok(v) = v.trim().parse::<f32>() else { continue };
                match item.text().unwrap_or("").trim().to_ascii_lowercase().as_str() {
                    "red component" => rgb[0] = v,
                    "green component" => rgb[1] = v,
                    "blue component" => rgb[2] = v,
                    _ => {},
                }
            }
        }
        let hex = ((rgb[0].clamp(0.0, 1.0) * 255.0).round() as u32) << 16
            | ((rgb[1].clamp(0.0, 1.0) * 255.0).round() as u32) << 8
            | (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u32;
        entries.insert(node.text().unwrap_or("").trim().to_ascii_lowercase(), hex);
    }

    let mut ansi16 = [0u32; 16];
    for (i, slot) in ansi16.iter_mut().enumerate() {
        *slot = *entries
            .get(&format!("ansi {i} color"))
            .ok_or_else(|| format!("缺少 Ansi {i} Color"))?;
    }
    let fg = *entries.get("foreground color").ok_or("缺少 Foreground Color")?;
    let bg = *entries.get("background color").ok_or("缺少 Background Color")?;
    let cursor = entries.get("cursor color").copied().unwrap_or(fg);
    Ok(imported(
        stem,
        Some(raw_stem.to_string()),
        PaletteSpec { fg, bg, cursor, bright_fg: fg, dim_fg: fg, ansi16 },
    ))
}

// ---------------------------------------------------------------- Windows Terminal

fn load_windows_terminal(text: &str, stem: &str, raw_stem: &str) -> Result<ThemeData, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("JSON 解析失败: {e}"))?;
    let scheme = match v.get("schemes").and_then(|x| x.as_array()) {
        Some(arr) => arr.first().ok_or_else(|| "schemes 数组为空".to_string())?,
        None => &v,
    };
    let get = |k: &str| scheme.get(k).and_then(|x| x.as_str());
    let need = |k: &str| get(k).ok_or_else(|| format!("缺少 {k}"));
    let keys = [
        "black", "red", "green", "yellow", "blue", "purple", "cyan", "white",
        "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue",
        "brightPurple", "brightCyan", "brightWhite",
    ];
    let mut ansi16 = [0u32; 16];
    for (i, k) in keys.iter().enumerate() {
        ansi16[i] = parse_loose(need(k)?)?;
    }
    let fg = parse_loose(need("foreground")?)?;
    let bg = parse_loose(need("background")?)?;
    let cursor = get("cursorColor").map(parse_loose).transpose()?.unwrap_or(fg);
    let label = get("name").map(str::to_string).unwrap_or_else(|| raw_stem.to_string());
    Ok(imported(
        stem,
        Some(label),
        PaletteSpec { fg, bg, cursor, bright_fg: fg, dim_fg: fg, ansi16 },
    ))
}

// ---------------------------------------------------------------- kitty

fn load_kitty(text: &str, stem: &str, raw_stem: &str) -> Result<ThemeData, String> {
    let mut fg = None;
    let mut bg = None;
    let mut cursor = None;
    let mut ansi16 = [None; 16];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };
        let Some(value) = it.next() else { continue };
        match key {
            "foreground" => fg = Some(parse_loose(value)?),
            "background" => bg = Some(parse_loose(value)?),
            "cursor" => cursor = Some(parse_loose(value)?),
            k if k.starts_with("color") => {
                if let Ok(i) = k[5..].parse::<usize>() {
                    if i < 16 {
                        ansi16[i] = Some(parse_loose(value)?);
                    }
                }
            },
            _ => {},
        }
    }
    let mut colors = [0u32; 16];
    for (i, slot) in colors.iter_mut().enumerate() {
        *slot = ansi16[i].ok_or_else(|| format!("缺少 color{i}"))?;
    }
    let fg = fg.ok_or("缺少 foreground")?;
    Ok(imported(
        stem,
        Some(raw_stem.to_string()),
        PaletteSpec {
            fg,
            bg: bg.ok_or("缺少 background")?,
            cursor: cursor.unwrap_or(fg),
            bright_fg: fg,
            dim_fg: fg,
            ansi16: colors,
        },
    ))
}

// ---------------------------------------------------------------- Xresources

fn load_xresources(text: &str, stem: &str, raw_stem: &str) -> Result<ThemeData, String> {
    let mut fg = None;
    let mut bg = None;
    let mut cursor = None;
    let mut ansi16 = [None; 16];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('!') || line.starts_with("#define") {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.split_whitespace().next().unwrap_or("");
        let Some(color) = parse_xcolor(value) else { continue };
        let suffix = key
            .rsplit(['.', '*'])
            .next()
            .unwrap_or(key)
            .trim()
            .trim_start_matches('*');
        match suffix {
            "foreground" => fg = Some(color),
            "background" => bg = Some(color),
            "cursorColor" | "cursor" => cursor = Some(color),
            s if s.starts_with("color") => {
                if let Ok(i) = s[5..].parse::<usize>() {
                    if i < 16 {
                        ansi16[i] = Some(color);
                    }
                }
            },
            _ => {},
        }
    }
    let mut colors = [0u32; 16];
    for (i, slot) in colors.iter_mut().enumerate() {
        *slot = ansi16[i].ok_or_else(|| format!("缺少 *.color{i}"))?;
    }
    let fg = fg.ok_or("缺少 *.foreground")?;
    Ok(imported(
        stem,
        Some(raw_stem.to_string()),
        PaletteSpec {
            fg,
            bg: bg.ok_or("缺少 *.background")?,
            cursor: cursor.unwrap_or(fg),
            bright_fg: fg,
            dim_fg: fg,
            ansi16: colors,
        },
    ))
}

fn parse_xcolor(s: &str) -> Option<u32> {
    if let Some(rest) = s.strip_prefix("rgb:") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 3 {
            let ch = |p: &str| {
                // rgb: 分量是 ASCII 十六进制；先按字符取前 4 位，绝不能用
                // &p[..min(4)] 切在 UTF-8 字符中间（畸形 Xresources 会 panic）。
                let digits: String = p.chars().take(4).collect();
                let v = if digits.chars().count() >= 2 {
                    u32::from_str_radix(&digits, 16).ok()?
                } else {
                    u32::from_str_radix(&digits, 16).ok()? * 17
                };
                Some((v.min(0xffff) >> 8) as u32)
            };
            return Some(ch(parts[0])? << 16 | ch(parts[1])? << 8 | ch(parts[2])?);
        }
        return None;
    }
    parse_loose(s).ok()
}

// ---------------------------------------------------------------- 简单 YAML

fn load_simple_yaml(text: &str, stem: &str, raw_stem: &str) -> Result<ThemeData, String> {
    let mut name = None;
    let mut fg = None;
    let mut bg = None;
    let mut cursor = None;
    let mut colors: Vec<String> = Vec::new();
    let mut in_colors = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("---") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ") {
            if in_colors {
                colors.push(unquote(rest));
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        let value = unquote(value.trim());
        match key {
            "colors" => {
                in_colors = true;
                if let Some(list) = value.trim().strip_prefix('[') {
                    for item in list.trim_end_matches(']').split(',') {
                        let item = unquote(item.trim());
                        if !item.is_empty() {
                            colors.push(item);
                        }
                    }
                }
            },
            "name" => name = Some(value),
            "foreground" => fg = Some(value),
            "background" => bg = Some(value),
            "cursor" => cursor = Some(value),
            _ => in_colors = false,
        }
    }

    if colors.len() != 16 {
        return Err(format!("colors 必须 16 项（当前 {}）", colors.len()));
    }
    let mut ansi16 = [0u32; 16];
    for (i, c) in colors.iter().enumerate() {
        ansi16[i] = parse_loose(c)?;
    }
    let fg = parse_loose(fg.as_deref().ok_or("缺少 foreground")?)?;
    let bg = parse_loose(bg.as_deref().ok_or("缺少 background")?)?;
    let cursor = match cursor.as_deref() {
        None | Some("null") | Some("~") | Some("") => None,
        Some(c) => Some(parse_loose(c)?),
    }
    .unwrap_or(fg);
    Ok(imported(
        stem,
        name.or_else(|| Some(raw_stem.to_string())),
        PaletteSpec { fg, bg, cursor, bright_fg: fg, dim_fg: fg, ansi16 },
    ))
}

// ---------------------------------------------------------------- 公共工具

/// 文件名 → 主题 id：小写、非字母数字折叠成 `-`，截断 64 字符。
pub(crate) fn slugify(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut dash = false;
    for c in s.trim().chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                if out.chars().count() >= 64 {
                    break;
                }
                out.push(lc);
            }
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    (!out.is_empty()).then_some(out)
}

fn parse_hex(s: &str) -> Result<u32, String> {
    let s = s.trim();
    let hex = s
        .strip_prefix('#')
        .ok_or_else(|| format!("颜色必须形如 #RRGGBB（当前 {s:?}）"))?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("颜色必须形如 #RRGGBB（当前 {s:?}）"));
    }
    u32::from_str_radix(hex, 16).map_err(|e| format!("颜色解析失败 {s:?}: {e}"))
}

/// 导入格式常见的写法：`#rrggbb` / `0xrrggbb` / `rrggbb`。
fn parse_loose(s: &str) -> Result<u32, String> {
    let s = s
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches(';')
        .trim();
    let hex = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("0x"))
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("颜色必须形如 #RRGGBB（当前 {s:?}）"));
    }
    u32::from_str_radix(hex, 16).map_err(|e| format!("颜色解析失败 {s:?}: {e}"))
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

/// 校验 pair 互指且深浅相反；不合法只清空 pair，主题本身仍可用。
fn validate_pairs(customs: &mut [ThemeData], errors: &mut Vec<String>) {
    let index: HashMap<String, usize> =
        customs.iter().enumerate().map(|(i, t)| (t.id.clone(), i)).collect();
    let snapshot: Vec<(String, Option<String>, Variant)> = customs
        .iter()
        .map(|t| (t.id.clone(), t.pair.clone(), t.variant))
        .collect();

    for (id, pair, variant) in snapshot {
        let Some(pair_id) = pair else { continue };
        let idx = index.get(&id).copied();
        let pair_idx = index.get(&pair_id).copied();
        let ok = match (idx, pair_idx) {
            (Some(i), Some(p)) if i != p => {
                let other = &customs[p];
                other.variant != variant && other.pair.as_deref() == Some(id.as_str())
            },
            _ => false,
        };
        if !ok {
            errors.push(format!(
                "{id}: pair {pair_id:?} 不存在、不是相反深浅或未互相指向，已忽略 pair"
            ));
            if let Some(i) = idx {
                customs[i].pair = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
schema = 1
name = "My Dark"
variant = "dark"
pair = "my-light"
foreground = "#cdd6f4"
background = "#1e1e2e"
cursor = "#f5e0dc"
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
"##;

    fn parse(s: &str, stem: &str) -> Result<ThemeData, String> {
        let raw: RawTheme = toml::from_str(s).map_err(|e| e.to_string())?;
        parse_raw(stem, raw)
    }

    #[test]
    fn malformed_xresources_component_does_not_panic() {
        // é 占两个字节；旧实现 &p[..4] 会切在字符中间并 panic。
        assert_eq!(parse_xcolor("rgb:aaaé/00/00"), None);
        assert_eq!(parse_xcolor("rgb:ffff/0000/0000"), Some(0x00ff_0000));
    }

    #[test]
    fn parses_complete_theme() {
        let t = parse(SAMPLE, "my-dark").unwrap();
        assert_eq!(t.id, "custom:my-dark");
        assert_eq!(t.label, "My Dark");
        assert_eq!(t.variant, Variant::Dark);
        assert_eq!(t.pair.as_deref(), Some("custom:my-light"));
        assert_eq!(t.spec.bg, 0x001e1e2e);
        assert_eq!(t.spec.ansi16[1], 0x00f38ba8);
        assert_eq!(t.palette().bg, 0x001e1e2e);
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(parse(&SAMPLE.replace("schema = 1", "schema = 2"), "x").is_err());
        assert!(parse(&SAMPLE.replace("variant = \"dark\"", "variant = \"dim\""), "x").is_err());
        assert!(parse(&SAMPLE.replace("\"#45475a\", ", ""), "x").is_err());
        assert!(parse(&SAMPLE.replace("#cdd6f4", "#cdd6f"), "x").is_err());
        assert!(parse(&SAMPLE.replace("#cdd6f4", "#00cdd6f4"), "x").is_err());
        assert!(parse(&SAMPLE.replace("name = \"My Dark\"", "name = \"\""), "x").is_err());
        assert!(parse(&SAMPLE.replace("danger = \"#f38ba8\"", ""), "x").is_err());
        assert!(parse(&format!("{SAMPLE}\nextra = 1\n"), "x").is_err());
    }

    #[test]
    fn slug_rules() {
        assert_eq!(slugify("My Dark").as_deref(), Some("my-dark"));
        assert_eq!(slugify("Catppuccin_Mocha").as_deref(), Some("catppuccin-mocha"));
        assert_eq!(slugify("--bad--").as_deref(), Some("bad"));
        assert_eq!(slugify("..").as_deref(), None);
        assert!(slugify(&"x".repeat(100)).unwrap().chars().count() <= 64);
    }

    fn custom(stem: &str, variant: Variant, pair: Option<&str>) -> ThemeData {
        let mut t = parse(SAMPLE, stem).unwrap();
        t.variant = variant;
        t.pair = pair.map(|p| format!("custom:{p}"));
        t
    }

    fn registry_with(customs: Vec<ThemeData>) -> ThemeRegistry {
        ThemeRegistry::from_parts(Theme::ALL.iter().copied().map(builtin).collect(), customs)
    }

    #[test]
    fn pair_validation_and_auto_resolution() {
        let dark = custom("demo-dark", Variant::Dark, Some("demo-light"));
        let light = custom("demo-light", Variant::Light, Some("demo-dark"));
        let mut customs = vec![dark, light];
        let mut errors = Vec::new();
        validate_pairs(&mut customs, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        let reg = registry_with(customs);
        let t = resolve_auto(&reg, Variant::Dark, Some("custom:demo-dark"), Some("custom:demo-light"));
        assert_eq!(t.id, "custom:demo-dark");
        let t = resolve_auto(&reg, Variant::Light, Some("custom:demo-dark"), Some("custom:demo-light"));
        assert_eq!(t.id, "custom:demo-light");
        let t = resolve_auto(&reg, Variant::Light, Some("custom:demo-dark"), None);
        assert_eq!(t.id, "custom:demo-light");
        let t = resolve_auto(&reg, Variant::Dark, None, Some("custom:demo-light"));
        assert_eq!(t.id, "custom:demo-dark");
        assert_eq!(resolve_auto(&reg, Variant::Dark, None, None).id, "yaru-dark");
        assert_eq!(resolve_auto(&reg, Variant::Light, None, None).id, "yaru-light");
        assert_eq!(
            resolve_auto(&reg, Variant::Dark, Some("custom:demo-light"), None).id,
            "yaru-dark"
        );
    }

    #[test]
    fn broken_pair_is_dropped_but_theme_survives() {
        let a = custom("a-dark", Variant::Dark, Some("b-light"));
        let b = custom("b-light", Variant::Light, None);
        let mut customs = vec![a, b];
        let mut errors = Vec::new();
        validate_pairs(&mut customs, &mut errors);
        assert!(!errors.is_empty());
        assert!(customs[0].pair.is_none());
        assert!(customs[1].pair.is_none());
    }

    #[test]
    fn imports_xresources() {
        let mut text = String::from("*.foreground: #f8f8f2\n*.background: #282a36\n*.cursorColor: #f8f8f2\n");
        for i in 0..16 {
            text.push_str(&format!("*.color{i}: #{:02x}{:02x}{:02x}\n", i * 10, 20, 30));
        }
        let t = load_xresources(&text, "dracula", "Dracula").unwrap();
        assert_eq!(t.id, "custom:dracula");
        assert_eq!(t.spec.fg, 0x00f8f8f2);
        assert_eq!(t.spec.bg, 0x00282a36);
        assert_eq!(t.spec.ansi16[3], 0x001e141e);
        assert_eq!(t.variant, Variant::Dark);
    }

    #[test]
    fn imports_kitty() {
        let mut text = String::from("foreground #ffffff\nbackground #101010\ncursor #ff8800\n");
        for i in 0..16 {
            text.push_str(&format!("color{i} #{:02x}0000\n", i * 8));
        }
        let t = load_kitty(&text, "kitty-test", "kitty test").unwrap();
        assert_eq!(t.spec.bg, 0x00101010);
        assert_eq!(t.spec.cursor, 0x00ff8800);
        assert_eq!(t.spec.ansi16[15], 0x00780000);
    }

    #[test]
    fn imports_windows_terminal() {
        let keys = [
            "black", "red", "green", "yellow", "blue", "purple", "cyan", "white",
            "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue",
            "brightPurple", "brightCyan", "brightWhite",
        ];
        let mut obj = serde_json::Map::new();
        obj.insert("name".into(), "Campbell".into());
        obj.insert("foreground".into(), "#cccccc".into());
        obj.insert("background".into(), "#0c0c0c".into());
        obj.insert("cursorColor".into(), "#ffffff".into());
        for (i, k) in keys.iter().enumerate() {
            obj.insert((*k).into(), format!("#{:02x}1122", i * 8).into());
        }
        let json = serde_json::Value::Object(obj).to_string();
        let t = load_windows_terminal(&json, "campbell", "Campbell").unwrap();
        assert_eq!(t.label, "Campbell");
        assert_eq!(t.spec.bg, 0x000c0c0c);
        assert_eq!(t.spec.ansi16[4], 0x00201122);
    }

    #[test]
    fn imports_alacritty_toml() {
        let text = r##"
[colors.primary]
background = "#1e1e2e"
foreground = "#cdd6f4"
[colors.cursor]
cursor = "#f5e0dc"
[colors.normal]
black = "#45475a"
red = "#f38ba8"
green = "#a6e3a1"
yellow = "#f9e2af"
blue = "#89b4fa"
magenta = "#f5c2e7"
cyan = "#94e2d5"
white = "#bac2de"
[colors.bright]
black = "#585b70"
red = "#f38ba8"
green = "#a6e3a1"
yellow = "#f9e2af"
blue = "#89b4fa"
magenta = "#f5c2e7"
cyan = "#94e2d5"
white = "#a6adc8"
"##;
        let t = load_toml(text, "alacritty-mocha", "alacritty mocha").unwrap();
        assert_eq!(t.spec.bg, 0x001e1e2e);
        assert_eq!(t.spec.ansi16[1], 0x00f38ba8);
    }

    #[test]
    fn imports_simple_yaml() {
        let mut text = String::from(
            "name: Catppuccin Mocha\nforeground: '#cdd6f4'\nbackground: '#1e1e2e'\nselection: null\ncursor: '#f5e0dc'\ncursorAccent: null\ncolors:\n",
        );
        for i in 0..16 {
            text.push_str(&format!("  - '#{:02x}2233'\n", i * 8));
        }
        let t = load_simple_yaml(&text, "yaml-mocha", "yaml mocha").unwrap();
        assert_eq!(t.label, "Catppuccin Mocha");
        assert_eq!(t.spec.bg, 0x001e1e2e);
        assert_eq!(t.spec.ansi16[2], 0x00102233);
    }

    #[test]
    fn imports_itermcolors() {
        fn color_xml(name: &str, r: f32, g: f32, b: f32) -> String {
            format!(
                "<key>{name}</key><dict>\
                 <key>Red Component</key><real>{r}</real>\
                 <key>Green Component</key><real>{g}</real>\
                 <key>Blue Component</key><real>{b}</real>\
                 </dict>"
            )
        }
        let mut body = String::new();
        body.push_str(&color_xml("Foreground Color", 0.96, 0.96, 0.96));
        body.push_str(&color_xml("Background Color", 0.1, 0.1, 0.1));
        body.push_str(&color_xml("Cursor Color", 1.0, 0.5, 0.0));
        for i in 0..16 {
            body.push_str(&color_xml(
                &format!("ANSI {i} Color"),
                i as f32 / 16.0,
                0.5,
                0.25,
            ));
        }
        let xml = format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>{body}</dict></plist>");
        let t = load_itermcolors(&xml, "iterm-test", "iTerm Test").unwrap();
        assert!((t.spec.fg & 0xff) > 240);
        assert_eq!(t.spec.cursor, 0x00ff8000);
    }

    #[test]
    fn loads_directory_and_reports_errors() {
        let dir = std::env::temp_dir().join(format!(
            "xtervo-themes-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("My Dark.toml"), SAMPLE).unwrap();
        std::fs::write(dir.join("broken.toml"), "schema = 1\nname = \"x\"\n").unwrap();
        let mut xres = String::from("*.foreground: #ffffff\n*.background: #111111\n");
        for i in 0..16 {
            xres.push_str(&format!("*.color{i}: #{:02x}2233\n", i * 8));
        }
        std::fs::write(dir.join("Some Xresources Theme.txt"), xres).unwrap();
        let reg = ThemeRegistry::load(&dir);
        assert_eq!(reg.custom_count(), 2);
        assert!(reg.get("custom:my-dark").is_some());
        assert!(reg.get("custom:some-xresources-theme").is_some());
        assert!(reg.errors.iter().any(|e| e.contains("broken.toml")));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn plugin_theme_is_namespaced_and_duplicate_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "xtervo-theme-pack-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut xres = String::from("*.foreground: #eeeeee\n*.background: #101010\n");
        for i in 0..16 {
            xres.push_str(&format!("*.color{i}: #{:02x}2233\n", i * 8));
        }
        let path = dir.join("Ocean.txt");
        std::fs::write(&path, xres).unwrap();
        let theme = load_plugin_theme(&path, "demo.plugin").unwrap();
        assert_eq!(theme.id, "custom:plugin-demo-plugin-ocean");
        assert!(theme.label.contains("plugin: demo.plugin"), "{}", theme.label);

        let mut reg = ThemeRegistry::from_parts(
            Theme::ALL.iter().copied().map(builtin).collect(),
            Vec::new(),
        );
        reg.add_custom(theme.clone());
        assert!(reg.get("custom:plugin-demo-plugin-ocean").is_some());
        assert_eq!(reg.custom_count(), 1);
        reg.add_custom(theme);
        assert_eq!(reg.custom_count(), 1);
        assert!(reg.errors.iter().any(|e| e.contains("重复")), "{:?}", reg.errors);
        let _ = std::fs::remove_dir_all(&dir);
    }

}
