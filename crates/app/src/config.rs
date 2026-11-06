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

use crate::commands::{Chord, HotkeyOverride};

/// 当前配置结构版本。改动字段语义/结构时递增，并在 [`Config::migrate`] 里补迁移步骤。
pub const SCHEMA_VERSION: u32 = 1;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 配置结构版本；缺失（旧文件）= 0。
    #[serde(default)]
    pub version: u32,
    /// 主题 id（"auto" | "yaru-dark" | "catppuccin-mocha" | "custom:<stem>" ...）。
    /// 默认 `"auto"`：深色用 Yaru Dark，浅色用 Yaru Light；也可显式指定任意主题。
    pub theme: String,
    /// `theme = "auto"` 时深色侧使用的主题 id（省略则用内置 Yaru Dark）。
    pub auto_dark: Option<String>,
    /// `theme = "auto"` 时浅色侧使用的主题 id（省略时优先用 auto_dark 的 pair）。
    pub auto_light: Option<String>,
    pub font_size: f32,
    /// 历史缓冲行数（对新 pane 生效）。
    pub scrollback: usize,
    pub shell: Option<String>,
    /// 新 Tab（`+` / Ctrl+T / 命令 `tab.new`）默认使用的 Profile ID。
    /// 0 = 未设置，使用系统默认 shell；ID 已失效时同样回退（不自动改写用户配置）。
    pub default_profile_id: u64,
    pub cols: u16,
    pub rows: u16,
    /// 关闭窗口时若有多个会话，先弹确认（默认开）。
    pub confirm_close: bool,
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
    /// Tab 非活动时收到 OSC 9/777/BEL 是否调用 notify-send（默认开）。
    pub desktop_notifications: bool,
    /// 活动 pane 收到 BEL 时是否闪一下边框（视觉铃，默认开）。
    pub visual_bell: bool,
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
    // ---- Quake mode ----
    /// 启用后 F12（可配置）全局显示/隐藏下拉终端。
    pub quake_enabled: bool,
    pub quake_hotkey: String,
    /// 屏宽/屏高比例（顶部居中）。
    pub quake_width: f32,
    pub quake_height: f32,
    /// 关闭 Tab/Pane 时对会话的处理："end" 结束进程 | "background" 保留后台可重连。
    pub close_action: String,
    /// 用户自定义快捷键：`[[hotkeys]]` 表，命令 ID + 规范小写 chord。
    /// 空 = 全部使用命令表默认绑定；旧配置没有该字段时按空处理。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hotkeys: Vec<HotkeyOverride>,
    /// 本版本不认识的键，原样保留（前向兼容）。
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            theme: "auto".to_string(),
            auto_dark: None,
            auto_light: None,
            font_size: 15.0,
            scrollback: 2000,
            shell: None,
            default_profile_id: 0,
            cols: 120,
            rows: 32,
            confirm_close: true,
            copy_on_select: true,
            confirm_multiline_paste: true,
            osc52: "allow".to_string(),
            right_click_paste: false,
            right_click: "menu".to_string(),
            paste_on_middle_click: true,
            tabbar_position: "top".to_string(),
            desktop_notifications: true,
            visual_bell: true,
            background_image: String::new(),
            background_opacity: 1.0,
            background_dim: DEFAULT_BACKGROUND_DIM,
            quake_enabled: false,
            quake_hotkey: "F12".to_string(),
            quake_width: 0.7,
            quake_height: 0.5,
            close_action: "end".to_string(),
            hotkeys: Vec::new(),
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

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// 读配置；旧版本先备份再迁移写回。解析失败返回默认值且**不动原文件**。
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let mut cfg: Config = match toml::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[config] {} 解析失败，本次用默认值（原文件未改动）: {e}", path.display());
                return Self::default();
            },
        };
        // 手写配置也统一成规范小写 chord；非法项原样保留（分发时忽略）。
        for over in &mut cfg.hotkeys {
            if let Ok(chord) = Chord::parse(&over.chord) {
                over.chord = chord.canonical();
            }
        }

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
        let tmp = path.with_extension("toml.tmp");
        if let Err(e) = std::fs::write(&tmp, text) {
            eprintln!("[config] 写入 {} 失败: {e}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            eprintln!("[config] 替换 {} 失败: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
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
    fn missing_fields_use_defaults() {
        let cfg: Config = toml::from_str("theme = \"latte\"\n").unwrap();
        assert_eq!(cfg.theme, "latte");
        assert_eq!(cfg.font_size, 15.0);
        assert_eq!(cfg.scrollback, 2000);
        assert_eq!(cfg.default_profile_id, 0, "旧配置缺 default_profile_id 时回退系统 shell");
        // 新增的桌面通知/视觉铃开关对旧配置也要默认开启
        assert!(cfg.desktop_notifications, "旧配置缺 desktop_notifications 时默认开");
        assert!(cfg.visual_bell, "旧配置缺 visual_bell 时默认为视觉铃");
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
        assert_eq!(c.close_action, "end", "默认关闭即结束会话");
        assert_eq!(c.osc52, "allow", "OSC 52 默认放行");
        assert!(c.osc52_allowed());
        assert_eq!(c.version, SCHEMA_VERSION);
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
}
