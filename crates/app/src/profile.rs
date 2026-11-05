//! 连接 Profile：名字 + 启动方式（当前只做 Local，SSH/Serial/Telnet 后续扩展）。
//!
//! 与 `config.rs` 同款策略：版本化、原子写、解析失败不覆盖用户文件。
//! 存到 `~/.config/rusttabby/profiles.toml`（`RUSTTABBY_PROFILES` 可覆盖，便于测试/多实例）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 当前 Profile 结构版本。
pub const VERSION: u32 = 1;

/// 一个本地 Profile：决定新 Tab 用什么 shell / 参数 / 起始目录 / 环境变量。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// 稳定 ID（文件内唯一；删除后不复用）。
    pub id: u64,
    pub name: String,
    /// 空 = 用系统默认 shell。
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

impl Default for Profile {
    fn default() -> Self {
        Self { id: 0, name: String::new(), shell: String::new(), args: Vec::new(), cwd: None, env: Vec::new() }
    }
}

/// Profile 集合。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileStore {
    pub version: u32,
    pub profiles: Vec<Profile>,
    /// 下一个可用 ID（持久化，避免重启后复用已删除的 ID）。
    pub next_id: u64,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self { version: VERSION, profiles: Vec::new(), next_id: 1 }
    }
}

impl ProfileStore {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("RUSTTABBY_PROFILES") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("rusttabby").join("profiles.toml")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let mut store: ProfileStore = match toml::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[profile] {} 解析失败，本次用空列表（原文件未改）: {e}", path.display());
                return Self::default();
            },
        };
        if store.version > VERSION {
            eprintln!("[profile] 文件版本 v{} 比程序新（v{VERSION}），忽略", store.version);
            return Self::default();
        }
        // 旧版本迁移：目前只有 v1，先只保证 ID 连续
        store.fix_ids();
        store
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
            Ok(t) => t,
            Err(e) => {
                eprintln!("[profile] 序列化失败: {e}");
                return;
            },
        };
        let tmp = path.with_extension("toml.tmp");
        if let Err(e) = std::fs::write(&tmp, text) {
            eprintln!("[profile] 写入 {} 失败: {e}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            eprintln!("[profile] 替换 {} 失败: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// 新增 Profile（同名或空名拒绝），返回新 ID。
    pub fn add(&mut self, mut profile: Profile) -> Result<u64, String> {
        let name = profile.name.trim().to_string();
        if name.is_empty() {
            return Err("名字不能为空".into());
        }
        if self.profiles.iter().any(|p| p.name == name) {
            return Err(format!("已存在同名 Profile: {name}"));
        }
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        profile.id = id;
        profile.name = name;
        self.profiles.push(profile);
        Ok(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.profiles.len();
        self.profiles.retain(|p| p.id != id);
        self.profiles.len() != before
    }

    pub fn get(&self, id: u64) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// 把 ID 修正成唯一且 < next_id（手改文件后也不至于开新 Tab 全撞一个 ID）。
    fn fix_ids(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut max_id = 0;
        for p in &mut self.profiles {
            while p.id == 0 || !seen.insert(p.id) {
                p.id = max_id + 1;
                max_id = p.id;
            }
            max_id = max_id.max(p.id);
        }
        self.next_id = self.next_id.max(max_id + 1).max(1);
        self.version = VERSION;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-prof-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(name: &str) -> Profile {
        Profile { name: name.into(), shell: "/bin/bash".into(), ..Profile::default() }
    }

    #[test]
    fn add_enforces_unique_nonempty_names_and_ids() {
        let mut s = ProfileStore::default();
        let a = s.add(sample("work")).unwrap();
        let b = s.add(sample("home")).unwrap();
        assert_ne!(a, b);
        assert!(s.add(sample("work")).is_err(), "同名要拒绝");
        assert!(s.add(sample("   ")).is_err(), "空名要拒绝");
        assert_eq!(s.profiles.len(), 2);
    }

    #[test]
    fn roundtrip_and_atomic_save() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("profiles.toml");
        let mut s = ProfileStore::default();
        s.add(Profile {
            name: "dev".into(),
            shell: "/bin/zsh".into(),
            args: vec!["-l".into()],
            cwd: Some("/home/yt/work".into()),
            env: vec![("FOO".into(), "1".into())],
            ..Profile::default()
        })
        .unwrap();
        s.save_to(&path);
        assert!(!dir.join("profiles.toml.tmp").exists());

        let back = ProfileStore::load_from(&path);
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].shell, "/bin/zsh");
        assert_eq!(back.profiles[0].args, vec!["-l"]);
        assert_eq!(back.profiles[0].cwd.as_deref(), Some("/home/yt/work"));
        assert_eq!(back.profiles[0].env, vec![("FOO".to_string(), "1".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_file_keeps_original() {
        let dir = tmpdir("broken");
        let path = dir.join("profiles.toml");
        let broken = "profiles = [oops\n";
        std::fs::write(&path, broken).unwrap();
        let s = ProfileStore::load_from(&path);
        assert!(s.profiles.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ids_are_repaired_on_load() {
        let dir = tmpdir("ids");
        let path = dir.join("profiles.toml");
        std::fs::write(
            &path,
            r#"
version = 1
next_id = 1

[[profiles]]
id = 7
name = "a"
shell = "/bin/sh"

[[profiles]]
id = 7
name = "b"
shell = "/bin/sh"
"#,
        )
        .unwrap();
        let s = ProfileStore::load_from(&path);
        assert_eq!(s.profiles.len(), 2);
        let ids: std::collections::HashSet<u64> = s.profiles.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), 2, "重复 ID 应被修正");
        assert!(s.next_id > *ids.iter().max().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_works_and_new_ids_not_reused() {
        let mut s = ProfileStore::default();
        let a = s.add(sample("a")).unwrap();
        assert!(s.remove(a));
        assert!(!s.remove(a));
        let b = s.add(sample("b")).unwrap();
        assert_ne!(a, b, "删除后 ID 不该复用");
    }
}
