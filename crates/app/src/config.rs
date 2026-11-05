//! 用户配置：`~/.config/rusttabby/config.toml`（可用 `RUSTTABBY_CONFIG` 覆盖路径）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "auto".to_string(),
            auto_dark: None,
            auto_light: None,
            font_size: 15.0,
            scrollback: 2000,
            shell: None,
            cols: 120,
            rows: 32,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("RUSTTABBY_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("rusttabby").join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    eprintln!("[config] 写入 {} 失败: {e}", path.display());
                }
            },
            Err(e) => eprintln!("[config] 序列化失败: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let cfg = Config { theme: "latte".into(), font_size: 17.0, ..Config::default() };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.theme, "latte");
        assert!((back.font_size - 17.0).abs() < f32::EPSILON);
        assert_eq!(back.scrollback, cfg.scrollback);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let cfg: Config = toml::from_str("theme = \"latte\"\n").unwrap();
        assert_eq!(cfg.theme, "latte");
        assert_eq!(cfg.font_size, 15.0);
        assert_eq!(cfg.scrollback, 2000);
    }
}
