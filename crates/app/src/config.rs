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

use serde::{Deserialize, Serialize};

use crate::commands::{Chord, HotkeyOverride};

/// 当前配置结构版本。改动字段语义/结构时递增，并在 [`Config::migrate`] 里补迁移步骤。
pub const SCHEMA_VERSION: u32 = 1;

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
    pub cols: u16,
    pub rows: u16,
    /// 关闭窗口时若有多个会话，先弹确认（默认开）。
    pub confirm_close: bool,
    /// 拖选后自动复制（默认开）。
    pub copy_on_select: bool,
    /// 多行粘贴前确认（默认开）。
    pub confirm_multiline_paste: bool,
    /// 右键直接粘贴（旧字段，保留用于迁移；新配置用 `right_click`）。
    pub right_click_paste: bool,
    /// Tabby 式右键模式：menu | paste | clipboard | off。
    pub right_click: String,
    /// 中键粘贴（Tabby 默认开）。
    pub paste_on_middle_click: bool,
    /// TabBar 位置："top" | "bottom"。
    pub tabbar_position: String,
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
            cols: 120,
            rows: 32,
            confirm_close: true,
            copy_on_select: true,
            confirm_multiline_paste: true,
            right_click_paste: false,
            right_click: "menu".to_string(),
            paste_on_middle_click: true,
            tabbar_position: "top".to_string(),
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
        let cfg = Config { theme: "latte".into(), font_size: 17.0, ..Config::default() };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.theme, "latte");
        assert!((back.font_size - 17.0).abs() < f32::EPSILON);
        assert_eq!(back.scrollback, cfg.scrollback);
        assert_eq!(back.version, SCHEMA_VERSION);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let cfg: Config = toml::from_str("theme = \"latte\"\n").unwrap();
        assert_eq!(cfg.theme, "latte");
        assert_eq!(cfg.font_size, 15.0);
        assert_eq!(cfg.scrollback, 2000);
        // 没有 version 的旧文件按 v0 读入
        assert_eq!(cfg.version, 0);
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
        assert_eq!(c.close_action, "end", "默认关闭即结束会话");
        assert_eq!(c.version, SCHEMA_VERSION);
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
