// SPDX-License-Identifier: GPL-3.0-only
//! 会话快照：把 tabs / 分屏树 / 每个 pane 的工作目录存盘，重启后原样拉起。
//!
//! 只存**布局与 cwd**，不存屏幕内容：历史由 shell / TUI 自己恢复，
//! 这也让快照体积极小（一个窗口几百字节）。
//!
//! 纯数据 + 纯函数，不碰 Slint，便于单测；落盘用「临时文件 + rename」保证原子性，
//! 半截文件永远不会被当成有效快照读进来（版本号也会兜底）。

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace::{Dir, Node, PaneId};

/// 快照格式版本；结构变化就递增，旧快照被安全忽略而不是解析出错。
///
/// `Tree::Leaf::profile_id` 是 **additive** 字段（带 serde default）：
/// v1 快照没有它也能读入（视为“无 Profile 关联”），所以无需 bump。
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// 窗口网格（重启后按它开同样大的窗口）。
    pub cols: u16,
    pub rows: u16,
    pub active: usize,
    pub tabs: Vec<TabSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub title: String,
    /// 聚焦 pane 在叶子**前序**中的下标（与 [`Node::panes`] 同序）。
    pub focused: usize,
    /// Tab 是否 Pin 在标签栏左侧固定区。additive：旧快照没有该键时视为 false，
    /// false 也不写出，保持旧快照体积/格式。
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    pub tree: Tree,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Tree {
    Leaf {
        cwd: Option<String>,
        /// 该 pane 打开时使用的 Profile ID（可选；旧快照/手动新 Tab 为 None）。
        /// 只存关联，不存凭据/连接状态；恢复失败时调用方回退普通 shell。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<u64>,
    },
    Split {
        dir: SplitDir,
        ratio: f32,
        a: Box<Tree>,
        b: Box<Tree>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDir {
    Row,
    Col,
}

impl From<Dir> for SplitDir {
    fn from(dir: Dir) -> Self {
        match dir {
            Dir::Row => Self::Row,
            Dir::Col => Self::Col,
        }
    }
}

impl From<SplitDir> for Dir {
    fn from(dir: SplitDir) -> Self {
        match dir {
            SplitDir::Row => Dir::Row,
            SplitDir::Col => Dir::Col,
        }
    }
}

impl Tree {
    /// 应用层布局树 → 快照树（顺带记录每个叶子的 cwd）。
    ///
    /// 兼容旧调用：不记录 Profile 关联。需要恢复连接类型时用
    /// [`Tree::from_node_with_profiles`]。
    #[allow(dead_code)]
    pub fn from_node(node: &Node, cwds: &HashMap<PaneId, Option<String>>) -> Tree {
        Self::from_node_with_profiles(node, cwds, &HashMap::new())
    }

    /// 同时记录每个叶子的 Profile ID（`None` = 普通 shell / 无关联）。
    pub fn from_node_with_profiles(
        node: &Node,
        cwds: &HashMap<PaneId, Option<String>>,
        profiles: &HashMap<PaneId, Option<u64>>,
    ) -> Tree {
        match node {
            Node::Leaf(id) => Tree::Leaf {
                cwd: cwds.get(id).cloned().flatten(),
                profile_id: profiles.get(id).copied().flatten(),
            },
            Node::Split { dir, ratio, a, b } => Tree::Split {
                dir: (*dir).into(),
                ratio: *ratio,
                a: Box::new(Tree::from_node_with_profiles(a, cwds, profiles)),
                b: Box::new(Tree::from_node_with_profiles(b, cwds, profiles)),
            },
        }
    }

    /// 叶子 cwd，前序排列（与 [`Node::panes`] 一致，恢复时用来对位）。
    pub fn leaf_cwds(&self) -> Vec<Option<String>> {
        let mut out = Vec::new();
        self.collect_cwds(&mut out);
        out
    }

    /// 叶子 Profile ID，前序排列（与 [`Tree::leaf_cwds`] 一一对应）。
    pub fn leaf_profiles(&self) -> Vec<Option<u64>> {
        let mut out = Vec::new();
        self.collect_profiles(&mut out);
        out
    }

    fn collect_cwds(&self, out: &mut Vec<Option<String>>) {
        match self {
            Tree::Leaf { cwd, .. } => out.push(cwd.clone()),
            Tree::Split { a, b, .. } => {
                a.collect_cwds(out);
                b.collect_cwds(out);
            },
        }
    }

    fn collect_profiles(&self, out: &mut Vec<Option<u64>>) {
        match self {
            Tree::Leaf { profile_id, .. } => out.push(*profile_id),
            Tree::Split { a, b, .. } => {
                a.collect_profiles(out);
                b.collect_profiles(out);
            },
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            Tree::Leaf { .. } => 1,
            Tree::Split { a, b, .. } => a.leaf_count() + b.leaf_count(),
        }
    }

    /// 把叶子按前序绑定到给定 id；数量不符返回 None（拒绝恢复半张树）。
    pub fn assign(&self, ids: &[PaneId]) -> Option<Node> {
        if ids.len() != self.leaf_count() {
            return None;
        }
        let mut cursor = 0;
        self.build(ids, &mut cursor)
    }

    fn build(&self, ids: &[PaneId], cursor: &mut usize) -> Option<Node> {
        match self {
            Tree::Leaf { .. } => {
                let id = *ids.get(*cursor)?;
                *cursor += 1;
                Some(Node::Leaf(id))
            },
            Tree::Split { dir, ratio, a, b } => Some(Node::Split {
                dir: (*dir).into(),
                ratio: *ratio,
                a: Box::new(a.build(ids, cursor)?),
                b: Box::new(b.build(ids, cursor)?),
            }),
        }
    }
}

/// 快照文件路径：`TERVO_SESSION` 优先（测试/多实例用），否则 XDG 状态目录。
pub fn snapshot_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TERVO_SESSION") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("tervo").join("session.json")
}

pub fn save(snap: &Snapshot) -> io::Result<PathBuf> {
    let path = snapshot_path();
    save_to(&path, snap)?;
    Ok(path)
}

pub fn load() -> Option<Snapshot> {
    load_from(&snapshot_path())
}

/// 消费掉快照：恢复成功后调用，避免下次手动 `--restore` 又拉一遍旧布局。
pub fn clear() {
    let path = snapshot_path();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(backup_path(&path));
    let _ = std::fs::remove_file(path.with_extension("json.tmp"));
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

pub fn save_to(path: &Path, snap: &Snapshot) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    // 先写临时文件 + fsync，再 rename；中途崩溃不会留下半截 JSON，
    // 也不会把还在页缓存里的内容当成已落盘状态。
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    // Keep one previous generation. Copy is intentionally best-effort: a failed
    // backup must not prevent a fresh snapshot from being committed.
    if path.is_file() {
        let _ = std::fs::copy(path, backup_path(path));
    }
    std::fs::rename(&tmp, path)?;
    sync_parent(path);
    Ok(())
}

fn parse_snapshot(path: &Path) -> Option<Snapshot> {
    let bytes = std::fs::read(path).ok()?;
    let snap: Snapshot = serde_json::from_slice(&bytes).ok()?;
    (snap.version == VERSION).then_some(snap)
}

pub fn load_from(path: &Path) -> Option<Snapshot> {
    parse_snapshot(path).or_else(|| {
        // A torn/corrupt primary must not make the user lose the session when a
        // valid previous generation exists.
        let backup = backup_path(path);
        let recovered = parse_snapshot(&backup);
        if recovered.is_some() {
            eprintln!("[session] 主快照损坏/版本不符，已回退 {}", backup.display());
        }
        recovered
    })
}

fn running_path(path: &Path) -> PathBuf {
    path.with_extension("running")
}

/// Write an unclean-shutdown marker before the UI starts. It is removed only by
/// a normal exit; a crash/kill leaves it behind for the next launch.
pub fn mark_running() -> io::Result<()> {
    mark_running_at(&snapshot_path())
}

fn mark_running_at(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let marker = running_path(path);
    let mut file = std::fs::File::create(&marker)?;
    writeln!(file, "pid={} started={}", std::process::id(), now_secs())?;
    file.sync_all()?;
    Ok(())
}

pub fn mark_clean() {
    let _ = std::fs::remove_file(running_path(&snapshot_path()));
}

/// True only when a previous process left the running marker behind.
pub fn was_unclean() -> bool {
    was_unclean_at(&snapshot_path())
}

fn was_unclean_at(path: &Path) -> bool {
    running_path(path).is_file()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 进程的当前工作目录（`/proc/<pid>/cwd`）。进程已退出时返回 None。
pub fn cwd_of(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwds(pairs: &[(PaneId, Option<&str>)]) -> HashMap<PaneId, Option<String>> {
        pairs.iter().map(|(id, c)| (*id, c.map(str::to_string))).collect()
    }

    #[test]
    fn tree_roundtrip_preserves_shape_ratio_and_cwd() {
        // 左右分屏，右侧再上下分
        let node = Node::leaf(1).split(1, Dir::Row, 2, 0.4).split(2, Dir::Col, 3, 0.6);
        let snap = Tree::from_node(&node, &cwds(&[(1, Some("/tmp")), (2, None), (3, Some("/"))]));

        assert_eq!(snap.leaf_count(), 3);
        assert_eq!(
            snap.leaf_cwds(),
            vec![Some("/tmp".to_string()), None, Some("/".to_string())]
        );

        // 恢复成新 id，形状必须一致
        let restored = snap.assign(&[10, 11, 12]).expect("数量一致");
        assert_eq!(restored.panes(), vec![10, 11, 12]);

        let mut rects = Vec::new();
        restored.layout(crate::workspace::Rect::new(0.0, 0.0, 1004.0, 100.0), &mut rects);
        assert_eq!(rects.len(), 3);
        // 右侧从 x = 1004*0.4 + gap 开始
        assert!((rects[1].1.x - (0.4 * 1000.0 + 4.0)).abs() < 0.01, "{:?}", rects[1].1);
        // 右侧再按 0.6 上下分
        assert!((rects[2].1.y - (0.6 * 96.0 + 4.0)).abs() < 0.01, "{:?}", rects[2].1);
    }

    #[test]
    fn profile_ids_roundtrip_with_same_leaf_order_as_cwds() {
        let node = Node::leaf(1).split(1, Dir::Row, 2, 0.4).split(2, Dir::Col, 3, 0.6);
        let profiles: HashMap<PaneId, Option<u64>> =
            [(1, Some(7)), (2, None), (3, Some(42))].into_iter().collect();
        let tree = Tree::from_node_with_profiles(
            &node,
            &cwds(&[(1, Some("/tmp")), (2, None), (3, Some("/"))]),
            &profiles,
        );
        assert_eq!(tree.leaf_profiles(), vec![Some(7), None, Some(42)]);
        assert_eq!(tree.leaf_cwds().len(), tree.leaf_profiles().len());

        // assign 只重建布局，profile 关联由调用方按前序对位；形状/比例不变。
        let restored = tree.assign(&[10, 11, 12]).unwrap();
        assert_eq!(restored.panes(), vec![10, 11, 12]);
        assert_eq!(tree.leaf_profiles()[1], None);
    }

    #[test]
    fn old_snapshot_without_profile_id_is_still_readable() {
        // v1（无 profile_id）快照：serde default -> None，不能因为新增字段被拒。
        let json = r#"{
            "version": 1,
            "cols": 100,
            "rows": 30,
            "active": 0,
            "tabs": [{
                "title": "Terminal 1",
                "focused": 0,
                "tree": { "kind": "leaf", "cwd": "/tmp" }
            }]
        }"#;
        let snap: Snapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snap.tabs[0].tree.leaf_profiles(), vec![None]);
        assert_eq!(snap.tabs[0].tree.leaf_cwds(), vec![Some("/tmp".to_string())]);
        // 旧快照没有 pinned：additive default -> false。
        assert!(!snap.tabs[0].pinned);
        // None/false 不写出键：保持快照体积/旧格式兼容。
        let text = serde_json::to_string(&snap).unwrap();
        assert!(!text.contains("profile_id"), "{text}");
        assert!(!text.contains("pinned"), "{text}");
    }

    #[test]
    fn profile_id_is_skipped_when_none_but_kept_when_some() {
        let tree = Tree::Leaf { cwd: Some("/tmp".into()), profile_id: Some(9) };
        let text = serde_json::to_string(&tree).unwrap();
        assert!(text.contains("\"profile_id\":9"), "{text}");
        let back: Tree = serde_json::from_str(&text).unwrap();
        assert_eq!(back, tree);
        let empty = Tree::Leaf { cwd: None, profile_id: None };
        let text = serde_json::to_string(&empty).unwrap();
        assert!(!text.contains("profile_id"), "{text}");
    }

    #[test]
    fn assign_rejects_mismatched_ids() {
        let tree = Tree::from_node(&Node::leaf(1).split(1, Dir::Row, 2, 0.5), &cwds(&[]));
        assert!(tree.assign(&[1]).is_none());
        assert!(tree.assign(&[1, 2, 3]).is_none());
        assert!(tree.assign(&[1, 2]).is_some());
    }

    #[test]
    fn snapshot_json_roundtrip_and_version_gate() {
        let snap = Snapshot {
            version: VERSION,
            cols: 120,
            rows: 32,
            active: 0,
            tabs: vec![
                TabSnapshot {
                    title: "Terminal 1".into(),
                    focused: 1,
                    pinned: false,
                    tree: Tree::from_node(
                        &Node::leaf(1).split(1, Dir::Col, 2, 0.5),
                        &cwds(&[(1, Some("/home/user"))]),
                    ),
                },
                TabSnapshot {
                    title: "Terminal 2".into(),
                    focused: 0,
                    pinned: true,
                    tree: Tree::Leaf { cwd: None, profile_id: None },
                },
            ],
        };
        let json = serde_json::to_vec(&snap).unwrap();
        let text = String::from_utf8(json.clone()).unwrap();
        assert!(text.contains("\"pinned\":true"), "{text}");
        let back: Snapshot = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, snap);
        assert!(back.tabs[1].pinned);

        let dir = std::env::temp_dir().join(format!("rt-session-{}", std::process::id()));
        let path = dir.join("session.json");
        save_to(&path, &snap).unwrap();
        assert_eq!(load_from(&path), Some(snap.clone()));

        // 版本不符且没有 backup → 当作没有快照，绝不拿旧结构去拼布局
        let mut old = snap.clone();
        old.version = VERSION - 1;
        let old_only = dir.join("old-only.json");
        save_to(&old_only, &old).unwrap();
        assert!(load_from(&old_only).is_none());

        // 版本不符的 primary + 有效 backup：回退上一代，而不是丢 session
        save_to(&path, &old).unwrap();
        assert!(load_from(&path).is_some(), "应回退上一代快照");

        // 半截 primary + 有效 backup：回退上一代，而不是丢 session
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load_from(&path).is_some(), "应回退上一代快照");
        // 没有 backup 的坏文件仍然安全返回 None
        let orphan = dir.join("orphan.json");
        std::fs::write(&orphan, b"{not json").unwrap();
        assert!(load_from(&orphan).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_or_empty_snapshot_never_blocks_new_tab_fallback() {
        let dir = std::env::temp_dir().join(format!("rt-session-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");

        // 不存在 / 半截 JSON / 未来版本：都返回 None，启动方据此新建普通 Tab。
        assert!(load_from(&path).is_none());
        std::fs::write(&path, b"{oops").unwrap();
        assert!(load_from(&path).is_none());
        let future = Snapshot {
            version: VERSION + 1,
            cols: 80,
            rows: 24,
            active: 0,
            tabs: Vec::new(),
        };
        save_to(&path, &future).unwrap();
        assert!(load_from(&path).is_none());

        // 合法但空 tabs：load 成功，startup 仍需检查空列表并回退新 Tab。
        let empty = Snapshot {
            version: VERSION,
            cols: 80,
            rows: 24,
            active: 0,
            tabs: Vec::new(),
        };
        save_to(&path, &empty).unwrap();
        let loaded = load_from(&path).unwrap();
        assert!(loaded.tabs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn running_marker_reports_unclean_and_clean_exit() {
        let dir = std::env::temp_dir().join(format!("rt-session-running-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("session.json");
        assert!(!was_unclean_at(&path));
        mark_running_at(&path).unwrap();
        assert!(was_unclean_at(&path));
        // A second mark is harmless and updates the pid line.
        mark_running_at(&path).unwrap();
        assert!(was_unclean_at(&path));
        std::fs::remove_file(running_path(&path)).unwrap();
        assert!(!was_unclean_at(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_keeps_previous_generation_for_corruption_recovery() {
        let dir = std::env::temp_dir().join(format!("rt-session-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("session.json");
        let first = Snapshot { version: VERSION, cols: 80, rows: 24, active: 0, tabs: Vec::new() };
        let second = Snapshot { version: VERSION, cols: 120, rows: 40, active: 0, tabs: Vec::new() };
        save_to(&path, &first).unwrap();
        save_to(&path, &second).unwrap();
        assert!(backup_path(&path).is_file());
        std::fs::write(&path, b"{corrupt").unwrap();
        let recovered = load_from(&path).expect("previous generation");
        assert_eq!(recovered.cols, 80);
        assert_eq!(recovered.rows, 24);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cwd_of_self_matches_current_dir() {
        let pid = std::process::id();
        let cwd = cwd_of(pid).expect("/proc/self/cwd");
        let expected = std::env::current_dir().unwrap();
        assert_eq!(PathBuf::from(cwd), expected);
    }

    #[test]
    fn cwd_of_dead_pid_is_none() {
        // 1 号进程的 cwd 可能不可读，但绝不会 panic；这里只验证不 panic 的语义。
        let _ = cwd_of(u32::MAX);
    }
}
