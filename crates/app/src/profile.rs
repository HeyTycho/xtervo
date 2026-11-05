//! 连接 Profile：名字 + 启动方式（当前只做 Local，SSH/Serial/Telnet 后续扩展）。
//!
//! 与 `config.rs` 同款策略：版本化、原子写、解析失败不覆盖用户文件。
//! 存到 `~/.config/rusttabby/profiles.toml`（`RUSTTABBY_PROFILES` 可覆盖，便于测试/多实例）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 当前 Profile 结构版本（v2 引入 kind 与连接字段）。
pub const VERSION: u32 = 2;

/// 一个本地 Profile：决定新 Tab 用什么 shell / 参数 / 起始目录 / 环境变量。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// 稳定 ID（文件内唯一；删除后不复用）。
    pub id: u64,
    pub name: String,
    /// `local` | `ssh` | `serial` | `telnet`（v1 没有该字段，迁移时补 local）。
    pub kind: String,
    // ---- local ----
    /// 空 = 用系统默认 shell。
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    // ---- ssh / telnet ----
    pub host: String,
    pub port: u16,
    pub user: String,
    /// `agent` | `key` | `password`（密码走 Vault，暂未实现）。
    pub auth: String,
    pub key_path: String,
    /// 传给 ssh 的追加参数（如 -o UserKnownHostsFile=... / -o StrictHostKeyChecking=yes）。
    pub extra_args: Vec<String>,
    /// 本地端口转发，每项形如 `8080:localhost:80` → `ssh -L 8080:localhost:80`。
    pub local_forwards: Vec<String>,
    // ---- serial ----
    pub serial_port: String,
    pub baud: u32,
}

/// 支持的 Profile 类型（字符串值即配置里的 `kind`）。
pub const KINDS: [&str; 4] = ["local", "ssh", "serial", "telnet"];

pub fn default_kind() -> String {
    "local".to_string()
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            kind: default_kind(),
            shell: String::new(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: "agent".to_string(),
            key_path: String::new(),
            extra_args: Vec::new(),
            local_forwards: Vec::new(),
            serial_port: String::new(),
            baud: 115200,
        }
    }
}

impl Profile {
    /// SSH 启动命令：用系统 `ssh` 客户端跑在 PTY 里（原生 russh 引擎后续再做）。
    ///
    /// 参数顺序：`[-i key] -p port [-l user] host`，与 OpenSSH 用法一致。
    pub fn ssh_command(&self) -> (String, Vec<String>) {
        let mut args = Vec::new();
        if self.auth == "key" && !self.key_path.trim().is_empty() {
            args.push("-i".to_string());
            args.push(self.key_path.trim().to_string());
        }
        args.push("-p".to_string());
        args.push(self.port.max(1).to_string());
        if !self.user.trim().is_empty() {
            args.push("-l".to_string());
            args.push(self.user.trim().to_string());
        }
        for f in &self.local_forwards {
            args.push("-L".to_string());
            args.push(f.clone());
        }
        args.extend(self.extra_args.iter().filter(|a| !a.trim().is_empty()).cloned());
        args.push(self.host.trim().to_string());
        ("ssh".to_string(), args)
    }

    /// Telnet 启动命令（系统 telnet 客户端）。
    pub fn telnet_command(&self) -> (String, Vec<String>) {
        ("telnet".to_string(), vec![self.host.trim().to_string(), self.port.max(1).to_string()])
    }

    /// Serial 启动命令（socat 把 PTY 和串口设备桥起来；没有 socat 时启动会失败并提示）。
    pub fn serial_command(&self) -> (String, Vec<String>) {
        let dev = self.serial_port.trim();
        let baud = self.baud.max(1);
        (
            "socat".to_string(),
            vec!["-,raw,echo=0".to_string(), format!("{dev},raw,echo=0,b{baud}")],
        )
    }

    /// 按类型校验必填项（本地 Profile 几乎不校验，SSH/Serial 必须有目标）。
    pub fn validate(&self) -> Result<(), String> {
        if !KINDS.contains(&self.kind.as_str()) {
            return Err(format!("未知 Profile 类型: {}", self.kind));
        }
        match self.kind.as_str() {
            "ssh" => {
                if self.host.trim().is_empty() {
                    return Err("SSH 主机不能为空".into());
                }
                if self.port == 0 {
                    return Err("SSH 端口必须在 1-65535".into());
                }
                if self.user.trim().is_empty() {
                    return Err("SSH 用户名不能为空".into());
                }
                if !["agent", "key", "password"].contains(&self.auth.as_str()) {
                    return Err(format!("未知认证方式: {}", self.auth));
                }
                if self.auth == "key" && self.key_path.trim().is_empty() {
                    return Err("选择 key 认证时必须填私钥路径".into());
                }
                for f in &self.local_forwards {
                    let parts: Vec<&str> = f.split(':').collect();
                    if parts.len() < 3 || parts.iter().any(|x| x.trim().is_empty()) {
                        return Err(format!("端口转发格式应为 本地端口:主机:端口，实际: {f}"));
                    }
                    if parts[0].trim().parse::<u16>().map(|p| p == 0).unwrap_or(true) {
                        return Err(format!("端口转发的本地端口无效: {f}"));
                    }
                }
            },
            "serial" => {
                if self.serial_port.trim().is_empty() {
                    return Err("串口设备不能为空".into());
                }
                if self.baud == 0 {
                    return Err("波特率必须大于 0".into());
                }
            },
            "telnet" => {
                if self.host.trim().is_empty() {
                    return Err("Telnet 主机不能为空".into());
                }
                if self.port == 0 {
                    return Err("Telnet 端口必须在 1-65535".into());
                }
            },
            _ => {},
        }
        Ok(())
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
        if store.version < VERSION {
            // v1 没有 kind 字段：serde default 已补成 local，这里补版本并写回
            for p in &mut store.profiles {
                if !KINDS.contains(&p.kind.as_str()) {
                    p.kind = default_kind();
                }
            }
            store.version = VERSION;
            store.fix_ids();
            store.save_to(path);
            eprintln!("[profile] 已迁移到 v{VERSION}");
            return store;
        }
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
        profile.validate()?;
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        profile.id = id;
        profile.name = name;
        self.profiles.push(profile);
        Ok(id)
    }

    /// 更新已有 Profile（保留 ID；名字不能与其他 Profile 冲突）。
    pub fn update(&mut self, id: u64, mut updated: Profile) -> Result<(), String> {
        let name = updated.name.trim().to_string();
        if name.is_empty() {
            return Err("名字不能为空".into());
        }
        if self.profiles.iter().any(|p| p.id != id && p.name == name) {
            return Err(format!("已存在同名 Profile: {name}"));
        }
        updated.id = id;
        updated.name = name;
        updated.validate()?;
        let slot = self.profiles.iter_mut().find(|p| p.id == id).ok_or("Profile 不存在")?;
        *slot = updated;
        Ok(())
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
    fn ssh_command_builds_openssh_args() {
        let p = Profile {
            kind: "ssh".into(),
            host: "example.com".into(),
            port: 2222,
            user: "yt".into(),
            auth: "key".into(),
            key_path: "/home/yt/.ssh/id_ed25519".into(),
            ..Profile::default()
        };
        let (prog, args) = p.ssh_command();
        assert_eq!(prog, "ssh");
        assert_eq!(args, vec!["-i", "/home/yt/.ssh/id_ed25519", "-p", "2222", "-l", "yt", "example.com"]);

        let agent = Profile { kind: "ssh".into(), host: "h".into(), port: 22, user: "u".into(), auth: "agent".into(), ..Profile::default() };
        assert_eq!(agent.ssh_command().1, vec!["-p", "22", "-l", "u", "h"]);

        let with_extra = Profile {
            kind: "ssh".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth: "agent".into(),
            extra_args: vec!["-o".into(), "UserKnownHostsFile=/tmp/kh".into()],
            ..Profile::default()
        };
        assert_eq!(
            with_extra.ssh_command().1,
            vec!["-p", "22", "-l", "u", "-o", "UserKnownHostsFile=/tmp/kh", "h"]
        );
    }

    #[test]
    fn update_keeps_id_and_rejects_duplicate_names() {
        let mut s = ProfileStore::default();
        let a = s.add(sample("a")).unwrap();
        let b = s.add(sample("b")).unwrap();
        let mut edited = sample("a2");
        edited.shell = "/bin/zsh".into();
        s.update(a, edited).unwrap();
        assert_eq!(s.get(a).unwrap().name, "a2");
        assert_eq!(s.get(a).unwrap().shell, "/bin/zsh");
        // 改成别人的名字要拒绝
        assert!(s.update(a, sample("b")).is_err());
        // 空名拒绝
        assert!(s.update(a, sample("  ")).is_err());
        assert_eq!(s.profiles.len(), 2);
        assert_eq!(s.get(b).unwrap().name, "b");
    }

    #[test]
    fn ssh_forwards_become_dash_l_args() {
        let p = Profile {
            kind: "ssh".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            local_forwards: vec!["8080:localhost:80".into(), "5432:db.internal:5432".into()],
            ..Profile::default()
        };
        assert_eq!(
            p.ssh_command().1,
            vec!["-p", "22", "-l", "u", "-L", "8080:localhost:80", "-L", "5432:db.internal:5432", "h"]
        );
        let bad = Profile { local_forwards: vec!["8080".into()], ..p.clone() };
        assert!(bad.validate().unwrap_err().contains("端口转发"));
        let bad_port = Profile { local_forwards: vec!["0:h:80".into()], ..p };
        assert!(bad_port.validate().is_err());
    }

    #[test]
    fn telnet_and_serial_commands() {
        let t = Profile {
            kind: "telnet".into(),
            host: "10.0.0.1".into(),
            port: 23,
            ..Profile::default()
        };
        assert_eq!(t.telnet_command(), ("telnet".to_string(), vec!["10.0.0.1".to_string(), "23".to_string()]));

        let s = Profile {
            kind: "serial".into(),
            serial_port: "/dev/ttyUSB0".into(),
            baud: 115200,
            ..Profile::default()
        };
        assert_eq!(
            s.serial_command(),
            ("socat".to_string(), vec!["-,raw,echo=0".to_string(), "/dev/ttyUSB0,raw,echo=0,b115200".to_string()])
        );
    }

    #[test]
    fn validate_by_kind() {
        let mut ssh = Profile { name: "s".into(), kind: "ssh".into(), host: String::new(), user: String::new(), ..Profile::default() };
        assert!(ssh.validate().unwrap_err().contains("主机"));
        ssh.host = "example.com".into();
        ssh.user = "yt".into();
        ssh.port = 22;
        assert!(ssh.validate().is_ok());
        ssh.port = 0;
        assert!(ssh.validate().is_err());
        ssh.port = 2222;
        ssh.auth = "key".into();
        assert!(ssh.validate().unwrap_err().contains("私钥"));
        ssh.key_path = "/home/yt/.ssh/id_ed25519".into();
        assert!(ssh.validate().is_ok());

        let mut serial = Profile { name: "c".into(), kind: "serial".into(), ..Profile::default() };
        assert!(serial.validate().is_err());
        serial.serial_port = "/dev/ttyUSB0".into();
        assert!(serial.validate().is_ok());

        let mut telnet = Profile { name: "t".into(), kind: "telnet".into(), ..Profile::default() };
        assert!(telnet.validate().is_err());
        telnet.host = "10.0.0.1".into();
        assert!(telnet.validate().is_ok());

        let mut bogus = Profile { name: "x".into(), ..Profile::default() };
        bogus.kind = "carrier-pigeon".into();
        assert!(bogus.validate().unwrap_err().contains("未知"));
    }

    #[test]
    fn v1_file_is_migrated_and_written_back() {
        let dir = tmpdir("v1");
        let path = dir.join("profiles.toml");
        // v1：没有 kind 字段
        std::fs::write(
            &path,
            r#"
version = 1
next_id = 2

[[profiles]]
id = 1
name = "old"
shell = "/bin/sh"
args = []
cwd = "/tmp"
env = []
"#,
        )
        .unwrap();
        let store = ProfileStore::load_from(&path);
        assert_eq!(store.version, VERSION);
        assert_eq!(store.profiles[0].kind, "local");
        // 迁移已写回文件
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("version = 2"), "{text}");
        assert!(text.contains("kind = \"local\""), "{text}");
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
