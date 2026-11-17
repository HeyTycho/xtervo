// SPDX-License-Identifier: GPL-3.0-only
//! 命名工作区（Workspace）：把当前 Tab 的分屏布局存成可复用条目（ACT-047）。
//!
//! 数据模型直接复用 `session::Tree`（已带 serde、split 比例、每个叶子的 cwd 与
//! Profile 关联），因此“保存布局”不需要另造一套树结构；恢复端把 Tree 交给
//! 现有的 tab 恢复路径即可。
//!
//! 落盘位置与 `profiles.toml` 同级（`$XDG_CONFIG_HOME/tervo/workspaces.toml`，
//! `TERVO_WORKSPACES` 可覆盖），独立文件避免为新增字段升 `profiles.toml` 版本。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::Tree;

/// 当前数据版本；未知版本当作损坏处理（返回空 store，不覆盖文件）。
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    /// 名字唯一（不区分大小写比较），用于命令行/菜单选择。
    pub name: String,
    /// 分屏布局：叶子带 cwd + profile_id。
    pub tree: Tree,
}

impl Default for Workspace {
    fn default() -> Self {
        Self { name: String::new(), tree: Tree::Leaf { cwd: None, profile_id: None } }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceStore {
    pub version: u32,
    pub workspaces: Vec<Workspace>,
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self { version: VERSION, workspaces: Vec::new() }
    }
}

impl WorkspaceStore {
    /// `TERVO_WORKSPACES` 覆盖，否则 `$XDG_CONFIG_HOME/tervo/workspaces.toml`。
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("TERVO_WORKSPACES") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("tervo").join("workspaces.toml")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// 读文件：不存在/解析失败/版本不认识都返回空 store（best-effort，不崩）。
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str::<Self>(&text) {
            Ok(store) if store.version == VERSION => store,
            Ok(store) => {
                eprintln!(
                    "[workspace] 未知版本 {}（期望 {VERSION}），忽略 {}",
                    store.version,
                    path.display()
                );
                Self::default()
            },
            Err(e) => {
                eprintln!("[workspace] 解析失败 {}: {e}", path.display());
                Self::default()
            },
        }
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    /// 原子写：临时文件 + rename；父目录自动创建。
    pub fn save_to(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let ordered = Self { version: VERSION, workspaces: self.workspaces.clone() };
        let Ok(text) = toml::to_string_pretty(&ordered) else {
            eprintln!("[workspace] 序列化失败");
            return;
        };
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text.as_bytes()).is_err() {
            eprintln!("[workspace] 写临时文件失败 {}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            eprintln!("[workspace] 提交失败 {}: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// 追加或覆盖同名条目（名字按 trim 后不区分大小写比较）。
    pub fn upsert(&mut self, workspace: Workspace) -> Result<(), String> {
        let name = workspace.name.trim().to_string();
        if name.is_empty() {
            return Err("工作区名字不能为空".into());
        }
        let mut workspace = workspace;
        workspace.name = name.clone();
        match self
            .workspaces
            .iter_mut()
            .find(|w| w.name.eq_ignore_ascii_case(&name))
        {
            Some(slot) => *slot = workspace,
            None => self.workspaces.push(workspace),
        }
        Ok(())
    }

    /// 按名字查（不区分大小写）。
    pub fn get(&self, name: &str) -> Option<&Workspace> {
        let name = name.trim();
        self.workspaces.iter().find(|w| w.name.eq_ignore_ascii_case(name))
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let name = name.trim();
        let before = self.workspaces.len();
        self.workspaces.retain(|w| !w.name.eq_ignore_ascii_case(name));
        self.workspaces.len() != before
    }
}

/// 清理工作区名：空白与路径分隔符换成空格，再 trim。
///
/// 命名弹窗用它预填 Tab 标题（SESS-003），`unique_name` 也复用它。
pub fn sanitize_name(base: &str) -> String {
    let cleaned: String = base
        .trim()
        .chars()
        .map(|c| if c.is_whitespace() || c == '/' || c == '\\' { ' ' } else { c })
        .collect();
    cleaned.trim().to_string()
}

/// 由 Tab 标题派生唯一工作区名：去掉空白/路径分隔符，重名时追加 `-2`、`-3`…
///
/// 抽成纯函数是为了单测；调用方只需把现有名字列表传进来。
pub fn unique_name(base: &str, existing: &[String]) -> String {
    let cleaned = sanitize_name(base);
    let base = if cleaned.is_empty() { "workspace" } else { cleaned.as_str() };
    if !existing.iter().any(|n| n.eq_ignore_ascii_case(base)) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|e| e.eq_ignore_ascii_case(&candidate)) {
            return candidate;
        }
        n += 1;
        if n > 999 {
            return format!("{base}-{}", now_seed());
        }
    }
}

fn now_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SplitDir;

    fn leaf(cwd: &str) -> Tree {
        Tree::Leaf { cwd: Some(cwd.to_string()), profile_id: None }
    }

    #[test]
    fn upsert_get_remove_roundtrip_and_name_rules() {
        let mut store = WorkspaceStore::default();
        assert!(store.upsert(Workspace { name: "  ".into(), tree: leaf("/") }).is_err());

        store
            .upsert(Workspace { name: "dev".into(), tree: leaf("/tmp/a") })
            .unwrap();
        // 同名（大小写不同）覆盖而不是追加
        store
            .upsert(Workspace { name: "DEV".into(), tree: leaf("/tmp/b") })
            .unwrap();
        assert_eq!(store.workspaces.len(), 1);
        assert_eq!(store.get("dev").unwrap().tree, leaf("/tmp/b"));

        assert!(store.get("nope").is_none());
        assert!(store.remove("Dev"));
        assert!(store.workspaces.is_empty());
        assert!(!store.remove("dev"));
    }

    #[test]
    fn unique_name_sanitizes_and_dedupes_case_insensitively() {
        assert_eq!(unique_name("my layout", &[]), "my layout");
        assert_eq!(unique_name("  ", &[]), "workspace");
        assert_eq!(unique_name("a/b", &[]), "a b");
        let existing = vec!["dev".to_string(), "dev-2".to_string()];
        // 保留用户原始大小写，只按不区分大小写去重
        assert_eq!(unique_name("DEV", &existing), "DEV-3");
    }

    #[test]
    fn split_tree_survives_disk_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.toml");
        let tree = Tree::Split {
            dir: SplitDir::Row,
            ratio: 0.25,
            a: Box::new(leaf("/tmp/left")),
            b: Box::new(Tree::Split {
                dir: SplitDir::Col,
                ratio: 0.5,
                a: Box::new(leaf("/tmp/right-top")),
                b: Box::new(leaf("/tmp/right-bottom")),
            }),
        };
        let mut store = WorkspaceStore::default();
        store.upsert(Workspace { name: "3-pane".into(), tree: tree.clone() }).unwrap();
        store.save_to(&path);

        let back = WorkspaceStore::load_from(&path);
        assert_eq!(back, store);
        assert_eq!(back.get("3-pane").unwrap().tree, tree);
        // 叶子数量与顺序保持（前序 = 左, 右上, 右下）
        assert_eq!(back.workspaces[0].tree.leaf_count(), 3);
    }

    #[test]
    fn broken_or_unknown_version_files_never_block_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.toml");
        assert!(WorkspaceStore::load_from(&path).workspaces.is_empty());

        std::fs::write(&path, b"not = [valid").unwrap();
        assert!(WorkspaceStore::load_from(&path).workspaces.is_empty());

        std::fs::write(&path, b"version = 99\n").unwrap();
        assert!(WorkspaceStore::load_from(&path).workspaces.is_empty());
    }
}
