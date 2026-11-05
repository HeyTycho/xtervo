//! 会话快照：把 tabs / 分屏树 / 每个 pane 的工作目录存盘，重启后原样拉起。
//!
//! 只存**布局与 cwd**，不存屏幕内容：历史由 shell / TUI 自己恢复，
//! 这也让快照体积极小（一个窗口几百字节）。
//!
//! 纯数据 + 纯函数，不碰 Slint，便于单测；落盘用「临时文件 + rename」保证原子性，
//! 半截文件永远不会被当成有效快照读进来（版本号也会兜底）。

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace::{Dir, Node, PaneId};

/// 快照格式版本；结构变化就递增，旧快照被安全忽略而不是解析出错。
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
    pub tree: Tree,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Tree {
    Leaf {
        cwd: Option<String>,
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
    pub fn from_node(node: &Node, cwds: &HashMap<PaneId, Option<String>>) -> Tree {
        match node {
            Node::Leaf(id) => Tree::Leaf { cwd: cwds.get(id).cloned().flatten() },
            Node::Split { dir, ratio, a, b } => Tree::Split {
                dir: (*dir).into(),
                ratio: *ratio,
                a: Box::new(Tree::from_node(a, cwds)),
                b: Box::new(Tree::from_node(b, cwds)),
            },
        }
    }

    /// 叶子 cwd，前序排列（与 [`Node::panes`] 一致，恢复时用来对位）。
    pub fn leaf_cwds(&self) -> Vec<Option<String>> {
        let mut out = Vec::new();
        self.collect_cwds(&mut out);
        out
    }

    fn collect_cwds(&self, out: &mut Vec<Option<String>>) {
        match self {
            Tree::Leaf { cwd } => out.push(cwd.clone()),
            Tree::Split { a, b, .. } => {
                a.collect_cwds(out);
                b.collect_cwds(out);
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

/// 快照文件路径：`RUSTTABBY_SESSION` 优先（测试/多实例用），否则 XDG 状态目录。
pub fn snapshot_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RUSTTABBY_SESSION") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("rusttabby").join("session.json")
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
    let _ = std::fs::remove_file(snapshot_path());
}

pub fn save_to(path: &Path, snap: &Snapshot) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    // 先写临时文件再 rename：中途崩溃不会留下半截 JSON。
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

pub fn load_from(path: &Path) -> Option<Snapshot> {
    let bytes = std::fs::read(path).ok()?;
    let snap: Snapshot = serde_json::from_slice(&bytes).ok()?;
    (snap.version == VERSION).then_some(snap)
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
                    tree: Tree::from_node(
                        &Node::leaf(1).split(1, Dir::Col, 2, 0.5),
                        &cwds(&[(1, Some("/home/user"))]),
                    ),
                },
                TabSnapshot { title: "Terminal 2".into(), focused: 0, tree: Tree::Leaf { cwd: None } },
            ],
        };
        let json = serde_json::to_vec(&snap).unwrap();
        let back: Snapshot = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, snap);

        let dir = std::env::temp_dir().join(format!("rt-session-{}", std::process::id()));
        let path = dir.join("session.json");
        save_to(&path, &snap).unwrap();
        assert_eq!(load_from(&path), Some(snap.clone()));

        // 版本不符 → 当作没有快照，绝不拿旧结构去拼布局
        let mut old = snap.clone();
        old.version = VERSION - 1;
        save_to(&path, &old).unwrap();
        assert!(load_from(&path).is_none());

        // 半截文件也不能 panic
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load_from(&path).is_none());
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
