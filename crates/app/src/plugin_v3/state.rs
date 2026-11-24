// SPDX-License-Identifier: GPL-3.0-only
//! v3 插件启用状态与权限确认的持久化。
//!
//! 状态文件独立于插件目录：插件可以整体升级/替换，但“用户是否已确认这组权限”
//! 不能由插件自己写入。任何权限集合变化都会让 `permissions_approved` 失效，
//! 下次启用必须重新确认。
//!
//! 文件只保存 id / enabled / 权限指纹，不保存任何凭据、路径或环境变量。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, PersistedPlugin>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedPlugin {
    pub enabled: bool,
    pub permissions_approved: bool,
    pub permissions: Vec<String>,
}

impl PersistedState {
    pub fn entry(&self, id: &str) -> PersistedPlugin {
        self.plugins.get(id).cloned().unwrap_or_default()
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.plugins.get(id).is_some_and(|entry| entry.enabled)
    }

    /// 权限是否与上次确认的集合完全一致。
    pub fn approved_for(&self, id: &str, permissions: &[String]) -> bool {
        self.plugins
            .get(id)
            .is_some_and(|entry| entry.permissions_approved && entry.permissions == permissions)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        self.plugins.entry(id.to_string()).or_default().enabled = enabled;
    }

    pub fn approve(&mut self, id: &str, permissions: Vec<String>) {
        let entry = self.plugins.entry(id.to_string()).or_default();
        entry.permissions_approved = true;
        entry.permissions = permissions;
    }

    pub fn revoke_approval(&mut self, id: &str) {
        if let Some(entry) = self.plugins.get_mut(id) {
            entry.permissions_approved = false;
        }
    }
}

/// 读取状态文件；缺失 = 空状态，损坏 = 记录错误后空状态（不删除文件）。
pub fn load_state(path: &Path) -> (PersistedState, Option<String>) {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (PersistedState::default(), None)
        }
        Err(e) => {
            return (
                PersistedState::default(),
                Some(format!("读取状态文件失败: {e}")),
            )
        }
    };
    if metadata.len() > MAX_STATE_BYTES {
        return (
            PersistedState::default(),
            Some(format!("状态文件过大（{} 字节）", metadata.len())),
        );
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            return (
                PersistedState::default(),
                Some(format!("读取状态文件失败: {e}")),
            )
        }
    };
    match toml::from_str::<PersistedState>(&text) {
        Ok(mut state) => {
            if state.version == 0 {
                state.version = STATE_VERSION;
            }
            if state.version != STATE_VERSION {
                return (
                    PersistedState::default(),
                    Some(format!("不支持的状态文件 version {}", state.version)),
                );
            }
            (state, None)
        }
        Err(e) => (
            PersistedState::default(),
            Some(format!("解析状态文件失败: {e}")),
        ),
    }
}

/// 原子写入：同目录临时文件 + rename；失败返回错误，不 panic。
pub fn save_state(path: &Path, state: &PersistedState) -> Result<(), String> {
    let mut state = state.clone();
    state.version = STATE_VERSION;
    let text = toml::to_string_pretty(&state).map_err(|e| format!("序列化状态失败: {e}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建状态目录失败: {e}"))?;
    }
    let tmp = PathBuf::from(format!("{}.tmp.{}", path.display(), std::process::id()));
    fs::write(&tmp, text).map_err(|e| format!("写入状态临时文件失败: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("替换状态文件失败: {e}")
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xtervo-plugin-v3-state-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_roundtrip_and_permission_fingerprint() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("state.toml");
        let mut state = PersistedState::default();
        state.set_enabled("dev.echo", true);
        state.approve(
            "dev.echo",
            vec!["clipboard".into(), "terminal_context".into()],
        );
        save_state(&path, &state).unwrap();
        let (loaded, error) = load_state(&path);
        assert!(error.is_none(), "{error:?}");
        assert!(loaded.is_enabled("dev.echo"));
        assert!(loaded.approved_for(
            "dev.echo",
            &["clipboard".to_string(), "terminal_context".to_string()]
        ));
        assert!(!loaded.approved_for("dev.echo", &["clipboard".to_string()]));
        assert!(!loaded.approved_for(
            "dev.echo",
            &[
                "clipboard".to_string(),
                "terminal_context".to_string(),
                "network".to_string()
            ]
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_state_is_empty_and_corrupt_state_is_reported() {
        let dir = tmpdir("corrupt");
        let path = dir.join("state.toml");
        let (state, error) = load_state(&path);
        assert!(error.is_none());
        assert!(state.plugins.is_empty());
        fs::write(&path, "not toml [[[").unwrap();
        let (state, error) = load_state(&path);
        assert!(state.plugins.is_empty());
        assert!(error.unwrap().contains("解析状态文件失败"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabling_keeps_approval_but_enabled_flag_is_authoritative() {
        let mut state = PersistedState::default();
        state.approve("dev.echo", vec!["clipboard".into()]);
        state.set_enabled("dev.echo", true);
        assert!(state.approved_for("dev.echo", &["clipboard".to_string()]));
        state.set_enabled("dev.echo", false);
        assert!(!state.is_enabled("dev.echo"));
        assert!(state.approved_for("dev.echo", &["clipboard".to_string()]));
        let _ = tmpdir("unused");
    }
}
