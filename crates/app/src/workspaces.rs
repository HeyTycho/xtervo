// SPDX-License-Identifier: GPL-3.0-only
//! 命名工作区（Workspace）：把当前 Tab 的分屏布局存成可复用条目（ACT-047）。
//!
//! 数据模型直接复用 `session::Tree`（已带 serde、split 比例、每个叶子的 cwd 与
//! Profile 关联），因此“保存布局”不需要另造一套树结构；恢复端把 Tree 交给
//! 现有的 tab 恢复路径即可。
//!
//! 落盘位置与 `profiles.toml` 同级（`$XDG_CONFIG_HOME/xtervo/workspaces.toml`，
//! `XTERVO_WORKSPACES` 可覆盖），独立文件避免为新增字段升 `profiles.toml` 版本。

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
    /// 创建时间（Unix 秒，UTC）；旧版 workspaces.toml 没有该字段时按 0 读入。
    /// 覆盖同名保存时保留原创建时间。
    pub created_at: u64,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            name: String::new(),
            tree: Tree::Leaf { cwd: None, profile_id: None },
            created_at: 0,
        }
    }
}

impl Workspace {
    /// 布局里的 pane 数（工作区列表展示用）。
    pub fn pane_count(&self) -> usize {
        self.tree.leaf_count()
    }

    /// 人类可读的创建时间；0/未知返回 “—”，避免显示 1970。
    pub fn created_label(&self) -> String {
        format_created_at(self.created_at)
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
    /// `XTERVO_WORKSPACES` 覆盖，否则 `$XDG_CONFIG_HOME/xtervo/workspaces.toml`。
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("XTERVO_WORKSPACES") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("xtervo").join("workspaces.toml")
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

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::path())
    }

    /// 原子写：临时文件 + rename；父目录自动创建。失败必须上抛给 UI，
    /// 不能只打日志后让调用方显示“已保存”。
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
        }
        let ordered = Self { version: VERSION, workspaces: self.workspaces.clone() };
        let text = toml::to_string_pretty(&ordered)
            .map_err(|e| format!("序列化失败: {e}"))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("提交 {} 失败: {e}", path.display()));
        }
        Ok(())
    }

    /// 追加或覆盖同名条目（名字按 trim 后不区分大小写比较）。
    ///
    /// 覆盖时保留原 `created_at`（“创建时间”不该因为重新保存而变成修改时间）；
    /// 新条目没有显式时间时用当前时间。
    pub fn upsert(&mut self, workspace: Workspace) -> Result<(), String> {
        let name = workspace.name.trim().to_string();
        if name.is_empty() {
            return Err("工作区名字不能为空".into());
        }
        if name.chars().any(|c| c.is_control()) {
            return Err("工作区名字不能包含控制字符".into());
        }
        let mut workspace = workspace;
        workspace.name = name.clone();
        match self
            .workspaces
            .iter_mut()
            .find(|w| w.name.eq_ignore_ascii_case(&name))
        {
            Some(slot) => {
                if slot.created_at > 0 {
                    workspace.created_at = slot.created_at;
                }
                *slot = workspace;
            },
            None => {
                if workspace.created_at == 0 {
                    workspace.created_at = now_secs();
                }
                self.workspaces.push(workspace);
            },
        }
        Ok(())
    }

    /// 重命名工作区（保留布局与创建时间）。
    ///
    /// 安全规则：空名/控制字符拒绝；与**其它**条目重名（不区分大小写）拒绝；
    /// 只改大小写允许。成功返回新的规范名字。
    pub fn rename(&mut self, old: &str, new: &str) -> Result<String, String> {
        let old = old.trim();
        let new = new.trim();
        if new.is_empty() {
            return Err("工作区名字不能为空".into());
        }
        if new.chars().any(|c| c.is_control()) {
            return Err("工作区名字不能包含控制字符".into());
        }
        let Some(index) = self
            .workspaces
            .iter()
            .position(|w| w.name.eq_ignore_ascii_case(old))
        else {
            return Err(format!("工作区「{old}」不存在"));
        };
        if self
            .workspaces
            .iter()
            .enumerate()
            .any(|(i, w)| i != index && w.name.eq_ignore_ascii_case(new))
        {
            return Err(format!("已存在名为「{new}」的工作区"));
        }
        self.workspaces[index].name = new.to_string();
        Ok(new.to_string())
    }

    /// 导出单个工作区为 JSON（含 name/tree 比例/cwd/profile_id/created_at）。
    /// 解析回来后仍是 `Workspace`，可直接喂给 `workspace.open` 路径。
    pub fn export_json(&self, name: &str) -> Result<String, String> {
        let workspace = self
            .get(name)
            .ok_or_else(|| format!("工作区「{}」不存在", name.trim()))?;
        serde_json::to_string_pretty(workspace)
            .map_err(|e| format!("序列化工作区失败：{e}"))
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
    now_secs()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Unix 秒 → `YYYY-MM-DD HH:MM UTC`。0 = 未知（旧文件没有 created_at）。
///
/// 不引入 chrono/time 依赖：只需要公历日期，用 Howard Hinnant 的
/// `civil_from_days` 反算（1970-01-01 起的天数）。
pub fn format_created_at(secs: u64) -> String {
    if secs == 0 {
        return "—".to_string();
    }
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// 1970-01-01 起的天数 → (年, 月, 日)；算法见
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
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
        assert!(store.upsert(Workspace { name: "  ".into(), tree: leaf("/"), ..Workspace::default() }).is_err());

        store
            .upsert(Workspace { name: "dev".into(), tree: leaf("/tmp/a"), ..Workspace::default() })
            .unwrap();
        // 同名（大小写不同）覆盖而不是追加
        store
            .upsert(Workspace { name: "DEV".into(), tree: leaf("/tmp/b"), ..Workspace::default() })
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
        store.upsert(Workspace { name: "3-pane".into(), tree: tree.clone(), created_at: 1_700_000_000 }).unwrap();
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

    #[test]
    fn created_at_is_backfilled_and_preserved_on_overwrite() {
        let mut store = WorkspaceStore::default();
        store
            .upsert(Workspace { name: "dev".into(), tree: leaf("/tmp"), ..Workspace::default() })
            .unwrap();
        let created = store.get("dev").unwrap().created_at;
        assert!(created > 0, "新工作区应自动填创建时间");

        // 重新保存（改名大小写也算覆盖）不能把创建时间改成修改时间。
        store
            .upsert(Workspace {
                name: "DEV".into(),
                tree: leaf("/var"),
                created_at: 7,
            })
            .unwrap();
        assert_eq!(store.get("dev").unwrap().created_at, created);
        assert_eq!(store.get("dev").unwrap().tree, leaf("/var"));
    }

    #[test]
    fn rename_rejects_duplicates_and_keeps_layout() {
        let mut store = WorkspaceStore::default();
        store
            .upsert(Workspace {
                name: "a".into(),
                tree: leaf("/a"),
                created_at: 42,
            })
            .unwrap();
        store
            .upsert(Workspace { name: "b".into(), tree: leaf("/b"), ..Workspace::default() })
            .unwrap();

        // 空名/控制字符/重名/旧名不存在都拒绝，且不改动 store。
        assert!(store.rename("a", "  ").is_err());
        assert!(store.rename("a", "bad\nname").is_err());
        assert!(store.rename("a", "B").is_err());
        assert!(store.rename("missing", "c").is_err());
        assert_eq!(store.get("a").unwrap().tree, leaf("/a"));

        // 正常改名保留布局/创建时间；只改大小写允许。
        assert_eq!(store.rename("a", "alpha").unwrap(), "alpha");
        assert!(store.get("a").is_none());
        assert_eq!(store.get("alpha").unwrap().created_at, 42);
        assert_eq!(store.get("alpha").unwrap().tree, leaf("/a"));
        assert_eq!(store.rename("alpha", "ALPHA").unwrap(), "ALPHA");
        assert_eq!(store.workspaces.len(), 2);
    }

    #[test]
    fn export_json_roundtrips_and_counts_panes() {
        let mut store = WorkspaceStore::default();
        let tree = Tree::Split {
            dir: SplitDir::Row,
            ratio: 0.4,
            a: Box::new(leaf("/left")),
            b: Box::new(leaf("/right")),
        };
        store
            .upsert(Workspace {
                name: "two-up".into(),
                tree: tree.clone(),
                created_at: 1_700_000_000,
            })
            .unwrap();
        assert_eq!(store.get("two-up").unwrap().pane_count(), 2);

        let json = store.export_json("TWO-UP").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "two-up");
        assert_eq!(value["created_at"], 1_700_000_000u64);
        assert_eq!(value["tree"]["kind"], "split");
        assert_eq!(value["tree"]["ratio"], 0.4);
        // 导出的 JSON 能反序列化回 Workspace，布局不丢。
        let roundtrip: Workspace = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, *store.get("two-up").unwrap());
        assert!(store.export_json("missing").is_err());
    }

    #[test]
    fn created_time_formats_known_epochs() {
        assert_eq!(format_created_at(0), "—");
        assert_eq!(format_created_at(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(format_created_at(1_735_689_599), "2024-12-31 23:59 UTC");
        assert_eq!(format_created_at(1_735_689_600), "2025-01-01 00:00 UTC");
    }

    #[test]
    fn legacy_file_without_created_at_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.toml");
        std::fs::write(
            &path,
            "version = 1\n[[workspaces]]\nname = \"legacy\"\n[workspaces.tree]\nkind = \"leaf\"\ncwd = \"/tmp\"\n",
        )
        .unwrap();
        let store = WorkspaceStore::load_from(&path);
        let ws = store.get("legacy").expect("旧文件应可读");
        assert_eq!(ws.created_at, 0);
        assert_eq!(ws.created_label(), "—");
        assert_eq!(ws.pane_count(), 1);
    }
}
