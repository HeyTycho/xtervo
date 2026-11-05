// SPDX-License-Identifier: GPL-3.0-only
//! 插件清单（manifest）加载：**只做发现、校验、展示与启用状态，不加载/执行任何代码**。
//!
//! 目录：`~/.config/tervo/plugins/*.toml`（`TERVO_PLUGINS` 可覆盖，测试/多实例用）。
//! 真正的插件运行时（JS/沙箱/权限/API）尚未实现，本模块只保证：
//! - 坏文件不会让程序崩溃，也不会覆盖用户文件；
//! - 校验 ID/名称/版本，重复 ID 只保留第一个并告警；
//! - 提供稳定的列表数据给设置页展示。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一份插件清单。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginManifest {
    /// 稳定 ID（唯一，建议反向域名）。
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    /// 未来扩展字段（入口、权限等）原样保留，避免旧版本写回时丢配置。
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Default for PluginManifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            version: String::new(),
            description: String::new(),
            enabled: true,
            extra: toml::Table::new(),
        }
    }
}

impl PluginManifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("插件 id 不能为空".into());
        }
        if self.name.trim().is_empty() {
            return Err(format!("插件 {} 缺少 name", self.id));
        }
        if self.version.trim().is_empty() {
            return Err(format!("插件 {} 缺少 version", self.id));
        }
        Ok(())
    }

    /// 设置页展示用的一行。
    pub fn display_line(&self) -> String {
        let state = if self.enabled { "enabled" } else { "disabled" };
        if self.description.trim().is_empty() {
            format!("{} v{} — {state}", self.name, self.version)
        } else {
            format!("{} v{} — {state} — {}", self.name, self.version, self.description)
        }
    }
}

/// 发现结果。
#[derive(Clone, Debug, Default)]
pub struct PluginStore {
    pub plugins: Vec<PluginManifest>,
    /// 加载过程中被跳过的文件（路径 + 原因），设置页可显示数量。
    pub skipped: Vec<(String, String)>,
}

impl PluginStore {
    pub fn dir() -> PathBuf {
        if let Ok(p) = std::env::var("TERVO_PLUGINS") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("tervo").join("plugins")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::dir())
    }

    pub fn load_from(dir: &Path) -> Self {
        let mut store = PluginStore::default();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return store; // 目录不存在 = 没有插件
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "toml").unwrap_or(false))
            .collect();
        paths.sort();
        for path in paths {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    store.skipped.push((path.display().to_string(), format!("读取失败: {e}")));
                    continue;
                },
            };
            let manifest: PluginManifest = match toml::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    store.skipped.push((path.display().to_string(), format!("解析失败: {e}")));
                    continue;
                },
            };
            if let Err(e) = manifest.validate() {
                store.skipped.push((path.display().to_string(), e));
                continue;
            }
            if store.plugins.iter().any(|p| p.id == manifest.id) {
                store.skipped.push((
                    path.display().to_string(),
                    format!("重复 id {}", manifest.id),
                ));
                continue;
            }
            store.plugins.push(manifest);
        }
        store.plugins.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        store
    }

    pub fn enabled_count(&self) -> usize {
        self.plugins.iter().filter(|p| p.enabled).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-plug-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_valid_manifests_and_skips_broken() {
        let dir = tmpdir("load");
        std::fs::write(
            dir.join("a.toml"),
            r#"
id = "org.example.a"
name = "Alpha"
version = "1.0.0"
description = "first"
enabled = true
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b.toml"),
            r#"
id = "org.example.b"
name = "Beta"
version = "0.2.1"
enabled = false
"#,
        )
        .unwrap();
        std::fs::write(dir.join("broken.toml"), "id = [oops\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "not a manifest").unwrap();
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.plugins.len(), 2);
        assert_eq!(store.skipped.len(), 1);
        assert_eq!(store.enabled_count(), 1);
        assert_eq!(store.plugins[0].name, "Alpha");
        assert!(store.plugins[0].display_line().contains("1.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ids_and_invalid_fields_are_rejected() {
        let dir = tmpdir("dup");
        std::fs::write(dir.join("a.toml"), "id = \"same\"\nname = \"A\"\nversion = \"1\"\n").unwrap();
        std::fs::write(dir.join("b.toml"), "id = \"same\"\nname = \"B\"\nversion = \"1\"\n").unwrap();
        std::fs::write(dir.join("c.toml"), "id = \"c\"\nname = \"\"\nversion = \"1\"\n").unwrap();
        std::fs::write(dir.join("d.toml"), "id = \"d\"\nname = \"D\"\n").unwrap();
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.plugins.len(), 1);
        assert_eq!(store.plugins[0].name, "A");
        assert_eq!(store.skipped.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_empty_not_error() {
        let store = PluginStore::load_from(Path::new("/nonexistent/rt-plugins"));
        assert!(store.plugins.is_empty());
        assert!(store.skipped.is_empty());
    }

    #[test]
    fn unknown_fields_are_preserved() {
        let dir = tmpdir("extra");
        std::fs::write(
            dir.join("x.toml"),
            "id = \"x\"\nname = \"X\"\nversion = \"1\"\nentry = \"index.js\"\n",
        )
        .unwrap();
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.plugins.len(), 1);
        assert_eq!(
            store.plugins[0].extra.get("entry").and_then(|v| v.as_str()),
            Some("index.js")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
