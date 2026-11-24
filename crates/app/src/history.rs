// SPDX-License-Identifier: GPL-3.0-only
//! SSH 连接历史（SSH-015）：成功的 host/Profile/时间，用于最近连接。
//!
//! 只保存展示所需字段（Profile 名、host、user、port、时间），**绝不保存**
//! 密码、私钥口令、Vault 内容或完整命令行。文件默认
//! `~/.config/xtervo/history.toml`，`XTERVO_HISTORY` 可覆盖（测试/多实例）。
//!
//! 记录按“同一 Profile 或同一 host/user/port”去重，最新在前，最多
//! [`MAX_ENTRIES`] 条；读盘失败/文件损坏时返回空历史，不阻塞连接。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_ENTRIES: usize = 20;

/// 最近连接一项；字段刻意不含任何凭据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryEntry {
    /// 稳定 Profile ID（0 = 未知）。
    pub profile_id: u64,
    pub profile_name: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    /// Unix 毫秒时间戳。
    pub at_ms: u64,
}

impl Default for HistoryEntry {
    fn default() -> Self {
        Self {
            profile_id: 0,
            profile_name: String::new(),
            host: String::new(),
            user: String::new(),
            port: 0,
            at_ms: 0,
        }
    }
}

impl HistoryEntry {
    /// 同一目标：优先 Profile ID；没有 ID 时回退 host/user/port。
    fn same_target(&self, other: &Self) -> bool {
        if self.profile_id != 0 || other.profile_id != 0 {
            return self.profile_id != 0 && self.profile_id == other.profile_id;
        }
        self.host.eq_ignore_ascii_case(&other.host)
            && self.user == other.user
            && self.port == other.port
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionHistory {
    pub version: u32,
    pub entries: Vec<HistoryEntry>,
}

impl Default for ConnectionHistory {
    fn default() -> Self {
        Self { version: VERSION, entries: Vec::new() }
    }
}

impl ConnectionHistory {
    pub fn path() -> PathBuf {
        if let Ok(path) = std::env::var("XTERVO_HISTORY") {
            return PathBuf::from(path);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("xtervo").join("history.toml")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str::<ConnectionHistory>(&text) {
            Ok(mut history) => {
                if history.version > VERSION {
                    eprintln!(
                        "[history] 文件版本 v{} 比程序新（v{VERSION}），忽略",
                        history.version
                    );
                    return Self::default();
                }
                history.version = VERSION;
                history.entries.truncate(MAX_ENTRIES);
                history
            },
            Err(error) => {
                eprintln!("[history] {} 解析失败，本次用空历史（原文件未改）: {error}", path.display());
                Self::default()
            },
        }
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    pub fn save_to(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let text = match toml::to_string_pretty(self) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("[history] 序列化失败: {error}");
                return;
            },
        };
        let tmp = path.with_extension("toml.tmp");
        if let Err(error) = std::fs::write(&tmp, text) {
            eprintln!("[history] 写入 {} 失败: {error}", tmp.display());
            return;
        }
        if let Err(error) = std::fs::rename(&tmp, path) {
            eprintln!("[history] 替换 {} 失败: {error}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// 记录一次成功连接：同目标去重、最新在前、最多 [`MAX_ENTRIES`] 条。
    pub fn record(&mut self, entry: HistoryEntry) {
        if entry.host.trim().is_empty() && entry.profile_id == 0 {
            return;
        }
        self.entries.retain(|old| !old.same_target(&entry));
        self.entries.insert(0, entry);
        self.entries.truncate(MAX_ENTRIES);
        self.version = VERSION;
    }

    /// 把历史映射回当前 Profile 下标（最新的在前，同一 Profile 只出现一次）。
    pub fn recent_profile_indices(&self, profiles: &[crate::profile::Profile]) -> Vec<usize> {
        let mut out = Vec::new();
        for entry in &self.entries {
            let index = profiles
                .iter()
                .position(|p| p.id == entry.profile_id)
                .or_else(|| {
                    profiles
                        .iter()
                        .position(|p| p.kind == "ssh" && p.name == entry.profile_name)
                });
            if let Some(index) = index {
                if !out.contains(&index) {
                    out.push(index);
                }
            }
            if out.len() >= 5 {
                break;
            }
        }
        out
    }

    /// 最近连接的时间标签（与当前时刻比较，避免依赖 chrono）。
    pub fn relative_label(at_ms: u64, now_ms: u64) -> String {
        if at_ms == 0 || at_ms > now_ms {
            return String::new();
        }
        let secs = (now_ms - at_ms) / 1000;
        if secs < 60 {
            "刚刚".into()
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else if secs < 86_400 {
            format!("{}h", secs / 3600)
        } else {
            format!("{}d", secs / 86_400)
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    fn entry(id: u64, name: &str, host: &str, at: u64) -> HistoryEntry {
        HistoryEntry {
            profile_id: id,
            profile_name: name.into(),
            host: host.into(),
            user: "tester".into(),
            port: 22,
            at_ms: at,
        }
    }

    #[test]
    fn record_dedupes_by_profile_and_keeps_newest_first() {
        let mut history = ConnectionHistory::default();
        history.record(entry(7, "work", "example.com", 100));
        history.record(entry(8, "prod", "prod.example", 200));
        history.record(entry(7, "work-renamed", "example.com", 300));
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[0].profile_id, 7);
        assert_eq!(history.entries[0].profile_name, "work-renamed");
        assert_eq!(history.entries[1].profile_id, 8);
    }

    #[test]
    fn history_is_bounded() {
        let mut history = ConnectionHistory::default();
        for i in 0..(MAX_ENTRIES + 5) {
            history.record(entry(i as u64 + 1, "p", "h", i as u64));
        }
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert_eq!(history.entries[0].profile_id, (MAX_ENTRIES + 5) as u64);
    }

    #[test]
    fn roundtrip_and_no_credentials_in_file() {
        let dir = std::env::temp_dir().join(format!(
            "xtervo-history-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.toml");
        let mut history = ConnectionHistory::default();
        history.record(entry(1, "work", "example.com", 123));
        history.save_to(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        for absent in ["password", "passphrase", "secret", "token"] {
            assert!(!text.to_lowercase().contains(absent), "unexpected {absent}: {text}");
        }
        let back = ConnectionHistory::load_from(&path);
        assert_eq!(back.entries, history.entries);
        let corrupted = dir.join("bad.toml");
        std::fs::write(&corrupted, "not = [toml").unwrap();
        assert!(ConnectionHistory::load_from(&corrupted).entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_indices_map_profile_ids_and_names() {
        let profiles = vec![
            Profile { id: 1, name: "work".into(), kind: "ssh".into(), ..Profile::default() },
            Profile { id: 2, name: "prod".into(), kind: "ssh".into(), ..Profile::default() },
            Profile { id: 3, name: "local".into(), kind: "local".into(), ..Profile::default() },
        ];
        let mut history = ConnectionHistory::default();
        // 名字回退：Profile 改 ID 后仍能通过名字找到。
        history.record(HistoryEntry {
            profile_id: 99,
            profile_name: "prod".into(),
            ..entry(0, "", "h", 1)
        });
        history.record(entry(1, "work", "h", 2));
        assert_eq!(history.recent_profile_indices(&profiles), vec![0, 1]);
    }

    #[test]
    fn relative_labels_cover_ranges() {
        assert_eq!(ConnectionHistory::relative_label(1_000, 1_000 + 5_000), "刚刚");
        assert_eq!(ConnectionHistory::relative_label(1_000, 1_000 + 120_000), "2m");
        assert_eq!(ConnectionHistory::relative_label(1_000, 1_000 + 7_200_000), "2h");
        assert_eq!(ConnectionHistory::relative_label(1_000, 1_000 + 172_800_000), "2d");
        assert_eq!(ConnectionHistory::relative_label(2_000, 1_000), "");
    }
}
