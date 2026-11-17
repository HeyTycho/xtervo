// SPDX-License-Identifier: GPL-3.0-only
//! 会话快照：把窗口几何 / tabs / 分屏树 / 每个 pane 的工作目录存盘，重启后原样拉起。
//!
//! 当前落盘格式是 **v2 多窗口**（[`SnapshotV2`]）。v1 单窗口文件（[`Snapshot`]）
//! 仍可读：加载时包装成单窗口 v2，供 WindowRegistry 后续接入。
//!
//! 只存**布局与 cwd**，不存屏幕内容 / scrollback / 运行中的进程 / 凭据：历史由
//! shell / TUI 自己恢复，这也让快照体积极小（一个窗口几百字节）。
//!
//! 纯数据 + 纯函数，不碰 Slint，便于单测；落盘用「临时文件 + fsync + rename」
//! 保证原子性，半截文件永远不会被当成有效快照读进来（版本号也会兜底），
//! 并保留上一代 `.bak` 供损坏/未知版本时回退。

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace::{Dir, Node, PaneId};

/// 当前落盘格式版本：v2 多窗口。
///
/// 结构变化就递增；未知版本按“损坏快照”处理（回退 `.bak` 或返回 `None`），
/// 不会拿旧/新结构硬拼布局。
pub const VERSION: u32 = 2;

/// 旧版（v1）单窗口格式版本，只用于读取兼容。
///
/// v1 没有像素窗口几何；迁移时单窗口的 `geometry` 为
/// [`WindowGeometry::UNKNOWN`]。v1 的 `cols/rows` 是字符网格，v2 没有对应
/// 字段，因此旧 [`load`] 适配层返回 0（恢复端回退默认窗口尺寸）。
pub const LEGACY_VERSION: u32 = 1;

/// v1 单窗口迁移到 v2 时使用的窗口 id。
///
/// 与 WindowRegistry 的首个窗口一致（`WindowId` 从 1 开始单调递增）。
pub const LEGACY_WINDOW_ID: u64 = 1;

/// 旧版（v1）单窗口快照。
///
/// 磁盘上的 v1 文件仍可读：加载时由 [`SnapshotV2`] 包装成单窗口。新代码
/// 应直接使用 [`SnapshotV2`]；本类型只作为现有调用方（未接入 WindowRegistry
/// 的 main）的兼容 DTO。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// v1 格式版本。仅内存兼容字段；写盘时由 [`SnapshotV2`] 统一写
    /// [`VERSION`]，恢复时旧适配层填 [`LEGACY_VERSION`]。
    pub version: u32,
    /// v1 的字符网格（v2 用像素几何，不再保存该字段）。
    pub cols: u16,
    pub rows: u16,
    pub active: usize,
    pub tabs: Vec<TabSnapshot>,
}

/// 窗口像素几何。
///
/// `w == 0 || h == 0` 表示未知：v1 迁移还没有像素坐标，恢复端应回退平台
/// 默认尺寸，等窗口打开后的下一次 autosave 再写入真实几何。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl WindowGeometry {
    /// 未知几何（v1 迁移使用）。
    pub const UNKNOWN: Self = Self {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    /// 是否有可用的像素尺寸。
    #[allow(dead_code)] // WindowRegistry 接入前只有测试使用
    pub fn is_known(self) -> bool {
        self.w > 0 && self.h > 0
    }
}

/// 单窗口快照。只含布局：不存进程、scrollback、凭据。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    /// WindowRegistry 分配，进程内单调递增且不复用。
    pub id: u64,
    pub geometry: WindowGeometry,
    pub maximized: bool,
    /// 活跃 Tab 在 `tabs` 中的下标；恢复端必须 clamp 到有效范围。
    pub active_tab: usize,
    pub tabs: Vec<TabSnapshot>,
}

/// 一次恢复最多创建多少个窗口。
///
/// 快照是用户可编辑的纯文本文件；限制窗口数避免损坏/恶意文件让启动阶段
/// 无限创建原生窗口。超出的窗口在恢复时跳过并记日志，不影响前 16 个。
pub const MAX_RESTORE_WINDOWS: usize = 16;

impl WindowSnapshot {
    /// 是否有可恢复的 Tab。空窗口（例如刚创建、还没建 Tab 就崩溃）不参与
    /// 布局恢复：恢复一个空窗口不如让用户从主窗口继续。
    pub fn is_restorable(&self) -> bool {
        !self.tabs.is_empty()
    }

    /// 活跃 Tab 下标 clamp 到有效范围；空窗口返回 0。
    pub fn clamped_active_tab(&self) -> usize {
        if self.tabs.is_empty() {
            0
        } else {
            self.active_tab.min(self.tabs.len() - 1)
        }
    }

    /// 恢复用副本：clamp active_tab，布局字段原样保留。
    pub fn normalized_for_restore(&self) -> Self {
        let mut window = self.clone();
        window.active_tab = window.clamped_active_tab();
        window
    }
}

/// v2 多窗口快照。
///
/// 窗口在写盘和读入时都按 [`WindowSnapshot::id`] 升序稳定排序。重复的窗口
/// id 视为损坏快照：保存拒绝，读取返回 `None`（有 `.bak` 时回退上一代）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotV2 {
    pub version: u32,
    pub windows: Vec<WindowSnapshot>,
}

impl Default for SnapshotV2 {
    fn default() -> Self {
        Self {
            version: VERSION,
            windows: Vec::new(),
        }
    }
}

impl SnapshotV2 {
    /// 构造 v2 快照，并把窗口按 id 稳定排序（窗口顺序与 HashMap 遍历顺序解耦）。
    pub fn new(mut windows: Vec<WindowSnapshot>) -> Self {
        windows.sort_by_key(|window| window.id);
        Self {
            version: VERSION,
            windows,
        }
    }

    /// 稳定按 [`WindowSnapshot::id`] 升序排列。
    pub fn sort_windows(&mut self) {
        self.windows.sort_by_key(|window| window.id);
    }

    /// 兼容旧单窗口调用方：仅当恰好一个窗口时转换为 v1 DTO。
    ///
    /// 多窗口返回 `None`，避免旧启动路径只恢复第一个窗口后调用 [`clear`]，
    /// 把其余窗口从磁盘上删掉。v1 的 `cols/rows` 在 v2 没有对应字段，故为 0。
    #[allow(dead_code)] // 启动路径已改读 v2；兼容旧调用方/测试保留
    pub fn to_legacy_single_window(&self) -> Option<Snapshot> {
        if self.windows.len() != 1 {
            return None;
        }
        let window = &self.windows[0];
        Some(Snapshot {
            version: LEGACY_VERSION,
            cols: 0,
            rows: 0,
            active: window.active_tab,
            tabs: window.tabs.clone(),
        })
    }

    /// 恢复计划：按窗口 id 排序、跳过空窗口、clamp active_tab、限制窗口数。
    ///
    /// 部分窗口损坏（空 tabs / 下标越界）只跳过该窗口或修正下标，不返回错误：
    /// 启动路径必须总能拿到一个可用布局。真正的 pane 创建失败由 UI 层逐 Tab
    /// 跳过并回退新 Tab。
    pub fn restorable_windows(&self) -> Vec<WindowSnapshot> {
        let mut windows: Vec<WindowSnapshot> = self
            .windows
            .iter()
            .filter(|window| window.is_restorable())
            .map(WindowSnapshot::normalized_for_restore)
            .collect();
        windows.sort_by_key(|window| window.id);
        if windows.len() > MAX_RESTORE_WINDOWS {
            eprintln!(
                "[session] 快照含 {} 个窗口，超过恢复上限 {MAX_RESTORE_WINDOWS}，忽略其余窗口",
                windows.len()
            );
            windows.truncate(MAX_RESTORE_WINDOWS);
        }
        windows
    }
}

impl Snapshot {
    /// 旧单窗口 DTO → v2。
    ///
    /// v1 没有像素几何，迁移窗口的 `geometry` 为 [`WindowGeometry::UNKNOWN`]；
    /// 恢复端应使用平台默认尺寸，随后由 autosave 写入真实几何。
    pub fn to_v2(&self) -> SnapshotV2 {
        SnapshotV2::new(vec![WindowSnapshot {
            id: LEGACY_WINDOW_ID,
            geometry: WindowGeometry::UNKNOWN,
            maximized: false,
            active_tab: self.active,
            tabs: self.tabs.clone(),
        }])
    }
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
            }
        }
    }

    fn collect_profiles(&self, out: &mut Vec<Option<u64>>) {
        match self {
            Tree::Leaf { profile_id, .. } => out.push(*profile_id),
            Tree::Split { a, b, .. } => {
                a.collect_profiles(out);
                b.collect_profiles(out);
            }
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
            }
            Tree::Split { dir, ratio, a, b } => Some(Node::Split {
                dir: (*dir).into(),
                ratio: *ratio,
                a: Box::new(a.build(ids, cursor)?),
                b: Box::new(b.build(ids, cursor)?),
            }),
        }
    }
}

/// 会话恢复能做什么、不能做什么（SESS-006）。
///
/// “恢复会话”恢复的是**布局 + cwd + 本地 Profile 的启动配置**；重启后每个 pane
/// 都是新的 shell 进程，不恢复运行中的进程/历史，也不自动连 SSH/Serial/Telnet。
/// UI 用它生成明确标注，避免把“布局恢复”误解成“进程续跑”。
pub struct RestoreBoundaries {
    /// 会恢复的内容（面向用户的一句话）。
    pub restored: [&'static str; 3],
    /// 不会恢复的内容；进程/历史/远端连接各一条。
    pub not_restored: [&'static str; 3],
}

/// 中英文恢复边界说明；纯静态文案，方便单测锁口径。
pub fn restore_boundaries(zh: bool) -> RestoreBoundaries {
    if zh {
        RestoreBoundaries {
            restored: [
                "分屏布局与比例",
                "每个 pane 的工作目录",
                "本地 Profile 的启动配置（shell/参数/env/外观）",
            ],
            not_restored: [
                "运行中的进程：重启后是新的 shell",
                "滚动历史与屏幕内容",
                "SSH/Serial/Telnet 连接：不自动建连",
            ],
        }
    } else {
        RestoreBoundaries {
            restored: [
                "Split layout and ratios",
                "Working directory of each pane",
                "Local profile launch settings (shell/args/env/appearance)",
            ],
            not_restored: [
                "Running processes: a restart starts a new shell",
                "Scrollback and screen contents",
                "SSH/Serial/Telnet connections: never auto-connect",
            ],
        }
    }
}

/// 单行完整说明（设置页/日志共用）：`布局+cwd+Profile；不恢复 …`。
pub fn restore_boundaries_text(zh: bool) -> String {
    let b = restore_boundaries(zh);
    if zh {
        format!(
            "恢复：{}。不恢复：{}。",
            b.restored.join("、"),
            b.not_restored.join("；")
        )
    } else {
        format!(
            "Restored: {}. Not restored: {}.",
            b.restored.join(", "),
            b.not_restored.join("; ")
        )
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

/// 保存旧单窗口 DTO：兼容包装，实际写 v2（单窗口）。
///
/// 这样未接入 WindowRegistry 的调用方不用改签名，也不会再写出 v1。
pub fn save(snap: &Snapshot) -> io::Result<PathBuf> {
    let path = snapshot_path();
    save_to(&path, snap)?;
    Ok(path)
}

/// 读取 v1/v2 快照并适配成旧单窗口 DTO。
///
/// 多窗口 v2 返回 `None`（见 [`SnapshotV2::to_legacy_single_window`]），避免
/// 旧启动路径 `clear()` 掉其它窗口；文件保持不动。
pub fn load() -> Option<Snapshot> {
    load_from(&snapshot_path())
}

/// 只读 v1 文件的字符网格，用于迁移启动尺寸。
///
/// v2/未知版本/坏文件返回 `None`；不会像 [`load`] 那样对多窗口 v2 打
/// “旧 API 不恢复”日志（启动探测阶段没有必要）。
pub fn legacy_grid() -> Option<(u16, u16)> {
    let bytes = std::fs::read(snapshot_path()).ok()?;
    let probe: VersionProbe = serde_json::from_slice(&bytes).ok()?;
    if probe.version != LEGACY_VERSION {
        return None;
    }
    let legacy: Snapshot = serde_json::from_slice(&bytes).ok()?;
    Some((legacy.cols, legacy.rows))
}

/// 消费掉快照：恢复成功后调用，避免下次手动 `--restore` 又拉一遍旧布局。
pub fn clear() {
    let path = snapshot_path();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(backup_path(&path));
    let _ = std::fs::remove_file(tmp_path(&path));
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

fn tmp_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

fn unique_window_ids(windows: &[WindowSnapshot]) -> bool {
    let mut ids: Vec<u64> = windows.iter().map(|window| window.id).collect();
    ids.sort_unstable();
    ids.windows(2).all(|pair| pair[0] != pair[1])
}

/// 保存 v2 多窗口快照（主路径）。
#[allow(dead_code)] // WindowRegistry 接入后由 main 使用；当前由单测覆盖
pub fn save_v2(snap: &SnapshotV2) -> io::Result<PathBuf> {
    let path = snapshot_path();
    save_v2_to(&path, snap)?;
    Ok(path)
}

/// 读取 v2 多窗口快照；v1 文件会自动迁移成单窗口 v2。
#[allow(dead_code)] // WindowRegistry 接入后由 main 使用；当前由单测覆盖
pub fn load_v2() -> Option<SnapshotV2> {
    load_v2_from(&snapshot_path())
}

/// 旧 API 的路径版包装：单窗口 DTO 转成 v2 后原子落盘。
pub fn save_to(path: &Path, snap: &Snapshot) -> io::Result<()> {
    save_v2_to(path, &snap.to_v2())
}

/// 原子写 v2：临时文件 + fsync，rename 前把当前文件复制为上一代 `.bak`，
/// 最后 fsync 父目录。保存前校验版本与窗口 id 唯一性，拒绝写坏快照。
pub fn save_v2_to(path: &Path, snap: &SnapshotV2) -> io::Result<()> {
    if snap.version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unsupported session snapshot version {} (expected {VERSION})",
                snap.version
            ),
        ));
    }
    if !unique_window_ids(&snap.windows) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "duplicate window id in session snapshot",
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 窗口顺序稳定 sorted by WindowId，不受 HashMap 遍历顺序影响。
    let mut ordered = snap.clone();
    ordered.sort_windows();
    let json = serde_json::to_vec_pretty(&ordered).map_err(io::Error::other)?;

    let tmp = tmp_path(path);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    // 保留上一代。备份是 best-effort：失败不能阻止新快照提交；成功时
    // fsync 备份，避免 rename 后崩溃留下只存在于页缓存的上一代。
    if path.is_file() {
        let backup = backup_path(path);
        if std::fs::copy(path, &backup).is_ok() {
            if let Ok(file) = std::fs::File::open(&backup) {
                let _ = file.sync_all();
            }
        }
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    sync_parent(path);
    Ok(())
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

fn parse_snapshot(path: &Path) -> Option<SnapshotV2> {
    let bytes = std::fs::read(path).ok()?;
    parse_snapshot_bytes(&bytes)
}

fn parse_snapshot_bytes(bytes: &[u8]) -> Option<SnapshotV2> {
    let probe: VersionProbe = serde_json::from_slice(bytes).ok()?;
    match probe.version {
        // 旧单窗口文件：结构校验交给 Snapshot，再包装成单窗口 v2。
        LEGACY_VERSION => {
            let legacy: Snapshot = serde_json::from_slice(bytes).ok()?;
            eprintln!("[session] v1 单窗口快照已迁移为 v2 内存模型");
            Some(legacy.to_v2())
        }
        VERSION => {
            let mut snap: SnapshotV2 = serde_json::from_slice(bytes).ok()?;
            if snap.version != VERSION || !unique_window_ids(&snap.windows) {
                return None;
            }
            snap.sort_windows();
            Some(snap)
        }
        // 未知版本（旧版不认识的新格式 / 手改坏版本）一律当损坏。
        _ => None,
    }
}

/// 读取 v1（迁移为单窗口 v2）或 v2；主文件损坏/版本不符时回退上一代 `.bak`。
pub fn load_v2_from(path: &Path) -> Option<SnapshotV2> {
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

/// 旧 API 的路径版包装：v1/v2 → 单窗口 DTO；多窗口 v2 返回 `None`。
pub fn load_from(path: &Path) -> Option<Snapshot> {
    legacy_from_v2(load_v2_from(path)?)
}

fn legacy_from_v2(snap: SnapshotV2) -> Option<Snapshot> {
    let legacy = snap.to_legacy_single_window();
    if legacy.is_none() && !snap.windows.is_empty() {
        eprintln!(
            "[session] v2 快照含 {} 个窗口；旧单窗口 API 不恢复（保留文件）",
            snap.windows.len()
        );
    }
    legacy
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

/// 崩溃连击计数的落盘位置（与 running marker 同目录）。
fn crash_streak_path(path: &Path) -> PathBuf {
    path.with_extension("crashes")
}

/// 连续异常退出次数：启动时若检测到上次 unclean 就 +1，正常退出清零。
///
/// 这是 safe mode 的前置数据：连续 N 次崩溃应当降级启动（禁插件/GPU/WASM），
/// 但降级策略本身还没实现，这里只负责把事实记准、并在 UI 里提示。
pub fn bump_crash_streak() -> u8 {
    bump_crash_streak_at(&snapshot_path())
}

fn bump_crash_streak_at(path: &Path) -> u8 {
    let file = crash_streak_path(path);
    // 文件格式：`<次数> <unix 秒>`；只有第一个字段是语义值，旧格式（纯数字）兼容。
    let (previous, bumped_at) = std::fs::read_to_string(&file)
        .ok()
        .map(|text| {
            let mut parts = text.split_whitespace();
            let count = parts.next().and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
            let at = parts.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            (count, at)
        })
        .unwrap_or((0, 0));
    let now = now_secs();
    // 后端回退会 exec 自身（同 pid、静态量失效），10 秒内视为同一次启动，不再累加。
    if bumped_at != 0 && now.saturating_sub(bumped_at) <= 10 {
        return previous;
    }
    let next = previous.saturating_add(1);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut out) = std::fs::File::create(&file) {
        let _ = writeln!(out, "{next} {now}");
        let _ = out.sync_all();
    }
    next
}

/// 读当前连击数（0 = 没有记录）。
pub fn crash_streak() -> u8 {
    crash_streak_at(&snapshot_path())
}

fn crash_streak_at(path: &Path) -> u8 {
    // 文件可能是 `<次数> <unix 秒>`（新）或纯数字（旧），都只取第一个字段。
    std::fs::read_to_string(crash_streak_path(path))
        .ok()
        .and_then(|text| {
            text.split_whitespace()
                .next()
                .and_then(|first| first.parse::<u8>().ok())
        })
        .unwrap_or(0)
}

/// 正常退出清除连击。
pub fn reset_crash_streak() {
    let _ = std::fs::remove_file(crash_streak_path(&snapshot_path()));
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 进程的当前工作目录（`/proc/<pid>/cwd`）。进程已退出时返回 None。
pub fn cwd_of(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwds(pairs: &[(PaneId, Option<&str>)]) -> HashMap<PaneId, Option<String>> {
        pairs
            .iter()
            .map(|(id, c)| (*id, c.map(str::to_string)))
            .collect()
    }

    fn leaf(cwd: Option<&str>, profile_id: Option<u64>) -> Tree {
        Tree::Leaf {
            cwd: cwd.map(str::to_string),
            profile_id,
        }
    }

    fn tab(title: &str, focused: usize, pinned: bool, tree: Tree) -> TabSnapshot {
        TabSnapshot {
            title: title.to_string(),
            focused,
            pinned,
            tree,
        }
    }

    fn geometry(x: i32, y: i32, w: u32, h: u32) -> WindowGeometry {
        WindowGeometry { x, y, w, h }
    }

    fn window(
        id: u64,
        geometry: WindowGeometry,
        maximized: bool,
        active_tab: usize,
        tabs: Vec<TabSnapshot>,
    ) -> WindowSnapshot {
        WindowSnapshot {
            id,
            geometry,
            maximized,
            active_tab,
            tabs,
        }
    }

    fn v2(windows: Vec<WindowSnapshot>) -> SnapshotV2 {
        SnapshotV2::new(windows)
    }

    fn ids(snap: &SnapshotV2) -> Vec<u64> {
        snap.windows.iter().map(|window| window.id).collect()
    }

    fn sorted_keys(map: &serde_json::Map<String, serde_json::Value>) -> Vec<&str> {
        let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn tree_roundtrip_preserves_shape_ratio_and_cwd() {
        // 左右分屏，右侧再上下分
        let node = Node::leaf(1)
            .split(1, Dir::Row, 2, 0.4)
            .split(2, Dir::Col, 3, 0.6);
        let snap = Tree::from_node(
            &node,
            &cwds(&[(1, Some("/tmp")), (2, None), (3, Some("/"))]),
        );

        assert_eq!(snap.leaf_count(), 3);
        assert_eq!(
            snap.leaf_cwds(),
            vec![Some("/tmp".to_string()), None, Some("/".to_string())]
        );

        // 恢复成新 id，形状必须一致
        let restored = snap.assign(&[10, 11, 12]).expect("数量一致");
        assert_eq!(restored.panes(), vec![10, 11, 12]);

        let mut rects = Vec::new();
        restored.layout(
            crate::workspace::Rect::new(0.0, 0.0, 1004.0, 100.0),
            &mut rects,
        );
        assert_eq!(rects.len(), 3);
        // 右侧从 x = 1004*0.4 + gap 开始
        assert!(
            (rects[1].1.x - (0.4 * 1000.0 + 4.0)).abs() < 0.01,
            "{:?}",
            rects[1].1
        );
        // 右侧再按 0.6 上下分
        assert!(
            (rects[2].1.y - (0.6 * 96.0 + 4.0)).abs() < 0.01,
            "{:?}",
            rects[2].1
        );
    }

    #[test]
    fn profile_ids_roundtrip_with_same_leaf_order_as_cwds() {
        let node = Node::leaf(1)
            .split(1, Dir::Row, 2, 0.4)
            .split(2, Dir::Col, 3, 0.6);
        let profiles: HashMap<PaneId, Option<u64>> = [(1, Some(7)), (2, None), (3, Some(42))]
            .into_iter()
            .collect();
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
        assert_eq!(
            snap.tabs[0].tree.leaf_cwds(),
            vec![Some("/tmp".to_string())]
        );
        // 旧快照没有 pinned：additive default -> false。
        assert!(!snap.tabs[0].pinned);
        // None/false 不写出键：保持快照体积/旧格式兼容。
        let text = serde_json::to_string(&snap).unwrap();
        assert!(!text.contains("profile_id"), "{text}");
        assert!(!text.contains("pinned"), "{text}");
    }

    #[test]
    fn profile_id_is_skipped_when_none_but_kept_when_some() {
        let tree = Tree::Leaf {
            cwd: Some("/tmp".into()),
            profile_id: Some(9),
        };
        let text = serde_json::to_string(&tree).unwrap();
        assert!(text.contains("\"profile_id\":9"), "{text}");
        let back: Tree = serde_json::from_str(&text).unwrap();
        assert_eq!(back, tree);
        let empty = Tree::Leaf {
            cwd: None,
            profile_id: None,
        };
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
    fn v1_loads_as_single_window_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let json = r#"{
            "version": 1,
            "cols": 132,
            "rows": 43,
            "active": 1,
            "tabs": [
                {
                    "title": "Terminal 1",
                    "focused": 0,
                    "tree": { "kind": "leaf", "cwd": "/tmp" }
                },
                {
                    "title": "Terminal 2",
                    "focused": 1,
                    "pinned": true,
                    "tree": {
                        "kind": "split",
                        "dir": "row",
                        "ratio": 0.5,
                        "a": { "kind": "leaf", "cwd": "/" },
                        "b": { "kind": "leaf", "cwd": "/home", "profile_id": 7 }
                    }
                }
            ]
        }"#;
        std::fs::write(&path, json).unwrap();

        let snap = load_v2_from(&path).expect("v1 file must load as v2");
        assert_eq!(snap.version, VERSION);
        assert_eq!(snap.windows.len(), 1);
        let window = &snap.windows[0];
        assert_eq!(window.id, LEGACY_WINDOW_ID);
        assert_eq!(window.geometry, WindowGeometry::UNKNOWN);
        assert_eq!(window.geometry, WindowGeometry::default());
        assert!(!window.maximized);
        assert_eq!(window.active_tab, 1);
        assert_eq!(window.tabs.len(), 2);
        assert_eq!(window.tabs[1].title, "Terminal 2");
        assert!(window.tabs[1].pinned);
        assert_eq!(window.tabs[1].focused, 1);
        assert_eq!(
            window.tabs[1].tree.leaf_cwds(),
            vec![Some("/".to_string()), Some("/home".to_string())]
        );
        assert_eq!(window.tabs[1].tree.leaf_profiles(), vec![None, Some(7)]);

        // 旧 API 也读得懂 v1，但字符网格没有 v2 对应字段。
        let legacy = load_from(&path).expect("legacy adapter");
        assert_eq!(legacy.version, LEGACY_VERSION);
        assert_eq!(
            (legacy.cols, legacy.rows),
            (0, 0),
            "v1 grid is not representable in v2"
        );
        assert_eq!(legacy.active, 1);
        assert_eq!(legacy.tabs, window.tabs);
    }

    #[test]
    fn v2_roundtrip_preserves_windows_geometry_maximized_and_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let first = window(
            1,
            geometry(-1920, 40, 1280, 720),
            true,
            1,
            vec![
                tab("A", 0, false, leaf(Some("/tmp"), None)),
                tab("B", 0, true, leaf(None, Some(9))),
            ],
        );
        let second = window(
            2,
            geometry(100, 120, 800, 600),
            false,
            0,
            vec![tab("C", 0, false, leaf(Some("/"), None))],
        );
        // 输入乱序，落盘/读回都按 id 稳定排序。
        let snap = v2(vec![second.clone(), first.clone()]);
        save_v2_to(&path, &snap).unwrap();
        assert!(!tmp_path(&path).exists(), "tmp file must be renamed away");
        assert!(
            !backup_path(&path).exists(),
            "first save has no previous generation"
        );

        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"].as_u64(), Some(VERSION as u64));
        assert_eq!(raw["windows"].as_array().unwrap().len(), 2);
        assert_eq!(raw["windows"][0]["id"].as_u64(), Some(1));
        assert_eq!(raw["windows"][1]["id"].as_u64(), Some(2));
        assert_eq!(raw["windows"][0]["geometry"]["x"].as_i64(), Some(-1920));
        assert_eq!(raw["windows"][0]["geometry"]["y"].as_i64(), Some(40));
        assert_eq!(raw["windows"][0]["geometry"]["w"].as_u64(), Some(1280));
        assert_eq!(raw["windows"][0]["geometry"]["h"].as_u64(), Some(720));
        assert_eq!(raw["windows"][0]["maximized"].as_bool(), Some(true));
        assert_eq!(raw["windows"][0]["active_tab"].as_u64(), Some(1));
        assert_eq!(raw["windows"][0]["tabs"].as_array().unwrap().len(), 2);
        assert_eq!(raw["windows"][1]["tabs"].as_array().unwrap().len(), 1);

        let back = load_v2_from(&path).expect("v2 roundtrip");
        assert_eq!(back, v2(vec![first, second]));
        assert_eq!(back.windows[0].geometry, geometry(-1920, 40, 1280, 720));
        assert!(back.windows[0].maximized);
        assert_eq!(back.windows[0].tabs.len(), 2);
        assert_eq!(back.windows[1].tabs.len(), 1);

        // 第二次保存生成上一代 .bak。
        let newer = v2(vec![window(
            3,
            WindowGeometry::UNKNOWN,
            false,
            0,
            Vec::new(),
        )]);
        save_v2_to(&path, &newer).unwrap();
        assert!(backup_path(&path).is_file());
        assert!(!tmp_path(&path).exists());
        assert_eq!(load_v2_from(&path), Some(newer));
    }

    #[test]
    fn window_order_is_normalized_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let unsorted = SnapshotV2 {
            version: VERSION,
            windows: vec![
                window(30, WindowGeometry::UNKNOWN, false, 0, Vec::new()),
                window(10, WindowGeometry::UNKNOWN, false, 0, Vec::new()),
                window(20, WindowGeometry::UNKNOWN, false, 0, Vec::new()),
            ],
        };
        save_v2_to(&path, &unsorted).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let raw_ids: Vec<u64> = raw["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|window| window["id"].as_u64().unwrap())
            .collect();
        assert_eq!(raw_ids, vec![10, 20, 30], "disk order must be stable");

        let back = load_v2_from(&path).unwrap();
        assert_eq!(ids(&back), vec![10, 20, 30]);
        assert_eq!(ids(&SnapshotV2::new(unsorted.windows)), vec![10, 20, 30]);
    }

    #[test]
    fn restorable_windows_skip_empty_and_clamp_active_tab() {
        let snap = v2(vec![
            window(
                3,
                geometry(-10, 20, 900, 700),
                true,
                9, // 越界：必须 clamp 到最后一个 tab
                vec![
                    tab("a", 1, false, leaf(Some("/tmp"), None)),
                    tab("b", 0, false, leaf(None, None)),
                ],
            ),
            // 刚创建还没建 Tab 的空窗口：不恢复，避免启动多个空壳窗口。
            window(1, geometry(0, 0, 800, 600), false, 0, Vec::new()),
            window(
                2,
                geometry(30, 40, 1024, 768),
                false,
                0,
                vec![tab("c", 0, false, leaf(None, None))],
            ),
        ]);

        let plan = snap.restorable_windows();
        assert_eq!(ids(&SnapshotV2::new(plan.clone())), vec![2, 3]);
        assert_eq!(plan[0].active_tab, 0);
        assert_eq!(plan[1].active_tab, 1, "active_tab must be clamped");
        assert_eq!(plan[1].geometry, geometry(-10, 20, 900, 700));
        assert!(plan[1].maximized);
        assert_eq!(plan[1].tabs.len(), 2);
        // 原快照不被恢复计划修改。
        assert_eq!(
            snap.windows
                .iter()
                .find(|window| window.id == 3)
                .map(|window| window.active_tab),
            Some(9)
        );
    }

    #[test]
    fn restorable_windows_are_capped_for_corrupt_files() {
        let windows = (1..=MAX_RESTORE_WINDOWS as u64 + 3)
            .map(|id| {
                window(
                    id,
                    WindowGeometry::UNKNOWN,
                    false,
                    0,
                    vec![tab("t", 0, false, leaf(None, None))],
                )
            })
            .collect();
        let plan = v2(windows).restorable_windows();
        assert_eq!(plan.len(), MAX_RESTORE_WINDOWS);
        assert_eq!(plan[0].id, 1);
        assert_eq!(
            plan.last().map(|window| window.id),
            Some(MAX_RESTORE_WINDOWS as u64)
        );
    }

    #[test]
    fn v1_file_roundtrips_through_v2_disk_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let json = r#"{
            "version": 1,
            "cols": 100,
            "rows": 30,
            "active": 1,
            "tabs": [
                {
                    "title": "one",
                    "focused": 0,
                    "tree": { "kind": "leaf", "cwd": "/tmp" }
                },
                {
                    "title": "two",
                    "focused": 1,
                    "tree": {
                        "kind": "split",
                        "dir": "col",
                        "ratio": 0.3,
                        "a": { "kind": "leaf", "cwd": "/" },
                        "b": { "kind": "leaf", "cwd": "/home" }
                    }
                }
            ]
        }"#;
        std::fs::write(&path, json).unwrap();

        let migrated = load_v2_from(&path).expect("v1 migration");
        assert_eq!(migrated.version, VERSION);
        assert_eq!(migrated.windows.len(), 1);
        assert_eq!(migrated.windows[0].geometry, WindowGeometry::UNKNOWN);
        assert_eq!(migrated.windows[0].active_tab, 1);
        assert_eq!(migrated.windows[0].tabs.len(), 2);

        // v1 → v2 落盘 → 再读：内容不变，版本变成 2。
        save_v2_to(&path, &migrated).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"].as_u64(), Some(VERSION as u64));
        assert!(raw.get("windows").is_some());
        assert!(raw.get("cols").is_none(), "v2 must not write v1 grid");
        let back = load_v2_from(&path).unwrap();
        assert_eq!(back, migrated);
        assert_eq!(
            back.restorable_windows()[0].geometry,
            WindowGeometry::UNKNOWN,
            "v1 has no pixel geometry; restore must use platform defaults"
        );
    }

    #[test]
    fn empty_windows_is_a_valid_v2_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let empty = SnapshotV2 {
            version: VERSION,
            windows: Vec::new(),
        };
        save_v2_to(&path, &empty).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["windows"].as_array().unwrap().len(), 0);
        assert_eq!(load_v2_from(&path), Some(empty));

        // 旧单窗口 API 没有窗口可恢复；文件不能被删。
        assert!(load_from(&path).is_none());
        assert!(path.is_file());
    }

    #[test]
    fn tabs_count_is_preserved_per_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let snap = v2(vec![
            window(1, WindowGeometry::UNKNOWN, false, 0, Vec::new()),
            window(
                2,
                geometry(1, 2, 3, 4),
                false,
                2,
                vec![
                    tab("a", 0, false, leaf(None, None)),
                    tab("b", 0, false, leaf(None, None)),
                    tab("c", 0, false, leaf(None, None)),
                ],
            ),
            window(
                3,
                geometry(5, 6, 7, 8),
                true,
                3,
                vec![
                    tab("d", 0, false, leaf(None, None)),
                    tab("e", 0, false, leaf(None, None)),
                    tab("f", 0, false, leaf(None, None)),
                    tab("g", 0, false, leaf(None, None)),
                ],
            ),
        ]);
        save_v2_to(&path, &snap).unwrap();
        let back = load_v2_from(&path).unwrap();
        assert_eq!(
            back.windows
                .iter()
                .map(|window| window.tabs.len())
                .collect::<Vec<_>>(),
            vec![0, 3, 4]
        );
        assert_eq!(back.windows[1].active_tab, 2);
        assert_eq!(back.windows[2].active_tab, 3);
    }

    #[test]
    fn bad_json_unknown_version_and_duplicate_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        assert!(load_v2_from(&path).is_none(), "missing file");
        std::fs::write(&path, b"{oops").unwrap();
        assert!(load_v2_from(&path).is_none(), "torn JSON");
        std::fs::write(&path, br#"{"version":3,"windows":[]}"#).unwrap();
        assert!(load_v2_from(&path).is_none(), "future version");
        std::fs::write(&path, br#"{"version":2}"#).unwrap();
        assert!(load_v2_from(&path).is_none(), "missing windows");
        std::fs::write(
            &path,
            br#"{"version":2,"windows":[{"id":1,"geometry":{"x":0,"y":0,"w":1,"h":1},"maximized":false,"active_tab":0,"tabs":[]},{"id":1,"geometry":{"x":0,"y":0,"w":1,"h":1},"maximized":false,"active_tab":0,"tabs":[]}]}"#,
        )
        .unwrap();
        assert!(load_v2_from(&path).is_none(), "duplicate window ids");
        std::fs::write(
            &path,
            br#"{"version":2,"windows":[{"id":1,"geometry":{"x":0,"y":0},"maximized":false,"active_tab":0,"tabs":[]}]}"#,
        )
        .unwrap();
        assert!(load_v2_from(&path).is_none(), "incomplete geometry");
    }

    #[test]
    fn backup_fallback_restores_previous_v2_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let first = v2(vec![window(
            1,
            geometry(10, 20, 800, 600),
            false,
            0,
            vec![tab("first", 0, false, leaf(Some("/tmp"), None))],
        )]);
        let second = v2(vec![window(
            2,
            geometry(30, 40, 1024, 768),
            true,
            0,
            Vec::new(),
        )]);
        save_v2_to(&path, &first).unwrap();
        save_v2_to(&path, &second).unwrap();
        assert!(backup_path(&path).is_file());

        std::fs::write(&path, b"{corrupt").unwrap();
        assert_eq!(load_v2_from(&path), Some(first.clone()));
        assert_eq!(
            load_from(&path).map(|snap| snap.tabs),
            Some(first.windows[0].tabs.clone())
        );

        // 未知版本同样回退上一代。
        std::fs::write(&path, br#"{"version":9,"windows":[]}"#).unwrap();
        assert_eq!(load_v2_from(&path), Some(first.clone()));

        // 没有 backup 的坏主文件安全返回 None。
        std::fs::remove_file(backup_path(&path)).unwrap();
        std::fs::write(&path, b"{corrupt").unwrap();
        assert!(load_v2_from(&path).is_none());
    }

    #[test]
    fn backup_fallback_can_migrate_a_v1_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let v1 = r#"{"version":1,"cols":80,"rows":24,"active":0,"tabs":[{"title":"old","focused":0,"tree":{"kind":"leaf","cwd":"/tmp"}}]}"#;
        std::fs::write(&path, v1).unwrap();
        let fresh = v2(vec![window(
            1,
            geometry(0, 0, 1200, 800),
            false,
            0,
            Vec::new(),
        )]);
        save_v2_to(&path, &fresh).unwrap(); // v1 成为上一代
        std::fs::write(&path, b"{torn").unwrap();

        let recovered = load_v2_from(&path).expect("v1 backup must be migrated");
        assert_eq!(recovered.windows.len(), 1);
        assert_eq!(recovered.windows[0].geometry, WindowGeometry::UNKNOWN);
        assert_eq!(recovered.windows[0].tabs.len(), 1);
        assert_eq!(recovered.windows[0].tabs[0].title, "old");
    }

    #[test]
    fn legacy_api_writes_v2_and_keeps_tabs_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let legacy = Snapshot {
            version: LEGACY_VERSION,
            cols: 120,
            rows: 32,
            active: 1,
            tabs: vec![
                tab("Terminal 1", 0, false, leaf(Some("/tmp"), None)),
                tab("Terminal 2", 1, true, leaf(None, Some(7))),
            ],
        };
        save_to(&path, &legacy).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"].as_u64(), Some(VERSION as u64));
        assert_eq!(raw["windows"].as_array().unwrap().len(), 1);
        assert_eq!(raw["windows"][0]["tabs"].as_array().unwrap().len(), 2);

        let back = load_from(&path).expect("legacy load");
        assert_eq!(back.version, LEGACY_VERSION);
        assert_eq!(back.active, 1);
        assert_eq!(back.tabs, legacy.tabs);
        assert_eq!((back.cols, back.rows), (0, 0), "v1 grid has no v2 field");
    }

    #[test]
    fn legacy_load_refuses_multi_window_v2_and_preserves_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let snap = v2(vec![
            window(
                1,
                WindowGeometry::UNKNOWN,
                false,
                0,
                vec![tab("one", 0, false, leaf(None, None))],
            ),
            window(
                2,
                WindowGeometry::UNKNOWN,
                false,
                0,
                vec![tab("two", 0, false, leaf(None, None))],
            ),
        ]);
        save_v2_to(&path, &snap).unwrap();

        // 旧启动路径若只恢复 window[0] 再 clear()，会删掉第二个窗口；必须拒绝。
        assert!(load_from(&path).is_none());
        assert!(
            path.is_file(),
            "legacy load must not delete multi-window data"
        );
        assert_eq!(load_v2_from(&path), Some(snap));
    }

    #[test]
    fn v2_json_contains_only_layout_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let snap = v2(vec![window(
            1,
            geometry(1, 2, 3, 4),
            false,
            0,
            vec![tab("T", 0, true, leaf(Some("/tmp"), Some(5)))],
        )]);
        save_v2_to(&path, &snap).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(
            sorted_keys(value.as_object().unwrap()),
            vec!["version", "windows"]
        );
        let window = value["windows"][0].as_object().unwrap();
        assert_eq!(
            sorted_keys(window),
            vec!["active_tab", "geometry", "id", "maximized", "tabs"]
        );
        assert_eq!(
            sorted_keys(window["geometry"].as_object().unwrap()),
            vec!["h", "w", "x", "y"]
        );
        let tab = window["tabs"][0].as_object().unwrap();
        assert_eq!(sorted_keys(tab), vec!["focused", "pinned", "title", "tree"]);
        assert_eq!(
            sorted_keys(tab["tree"].as_object().unwrap()),
            vec!["cwd", "kind", "profile_id"]
        );

        // 布局之外的东西（进程/scrollback/凭据）不得进入快照。
        let lower = text.to_ascii_lowercase();
        for forbidden in [
            "scrollback",
            "pid",
            "process",
            "password",
            "credential",
            "token",
        ] {
            assert!(
                !lower.contains(forbidden),
                "must not persist {forbidden}: {text}"
            );
        }
    }

    #[test]
    fn save_v2_rejects_wrong_version_and_duplicate_window_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let wrong = SnapshotV2 {
            version: LEGACY_VERSION,
            windows: Vec::new(),
        };
        let error = save_v2_to(&path, &wrong).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!path.exists(), "invalid snapshot must not create a file");

        let duplicate = SnapshotV2 {
            version: VERSION,
            windows: vec![
                window(7, WindowGeometry::UNKNOWN, false, 0, Vec::new()),
                window(7, WindowGeometry::UNKNOWN, false, 0, Vec::new()),
            ],
        };
        assert!(save_v2_to(&path, &duplicate).is_err());
        assert!(!path.exists());
    }

    /// 写盘失败（权限拒绝；ENOSPC 走同一条 tmp 写入错误路径）时：
    /// 旧快照原样保留、不留下半截 tmp、读回仍是上一代。
    #[test]
    fn write_failure_keeps_previous_snapshot_and_leaves_no_tmp() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // root 下 0o500 目录依然可写，跳过而不是假失败。
            if unsafe { libc::geteuid() } == 0 {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.json");
            let good = SnapshotV2 {
                version: VERSION,
                windows: vec![window(1, WindowGeometry::UNKNOWN, false, 0, Vec::new())],
            };
            save_v2_to(&path, &good).unwrap();
            let before = std::fs::read(&path).unwrap();

            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))
                .unwrap();
            let newer = SnapshotV2 {
                version: VERSION,
                windows: vec![window(2, WindowGeometry::UNKNOWN, false, 0, Vec::new())],
            };
            let error = save_v2_to(&path, &newer).unwrap_err();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(std::fs::read(&path).unwrap(), before, "旧快照必须原样保留");
            assert_eq!(load_v2_from(&path), Some(good));
            assert!(!tmp_path(&path).exists(), "失败后不能留下 tmp 文件");
        }
    }

    /// 主快照被截断 → 回退上一代 `.bak`；`.bak` 也坏 → None（不崩）。
    #[test]
    fn truncated_primary_recovers_from_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let first = SnapshotV2 {
            version: VERSION,
            windows: vec![window(1, WindowGeometry::UNKNOWN, false, 0, Vec::new())],
        };
        let second = SnapshotV2 {
            version: VERSION,
            windows: vec![window(2, WindowGeometry::UNKNOWN, false, 0, Vec::new())],
        };
        save_v2_to(&path, &first).unwrap();
        save_v2_to(&path, &second).unwrap();
        assert!(backup_path(&path).is_file(), "第二次保存应生成上一代 .bak");
        assert_eq!(load_v2_from(&path), Some(second));

        // 半截 JSON：解析失败，回退 .bak
        std::fs::write(&path, b"{\"version\":2,\"windows\":[").unwrap();
        assert_eq!(load_v2_from(&path), Some(first.clone()));

        // 主文件与备份都坏：返回 None，启动方回退新 Tab
        std::fs::write(backup_path(&path), b"garbage").unwrap();
        assert!(load_v2_from(&path).is_none());
    }

    #[test]
    fn bad_or_empty_snapshot_never_blocks_new_tab_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");

        // 不存在 / 半截 JSON / 未知版本：都返回 None，启动方据此新建普通 Tab。
        assert!(load_from(&path).is_none());
        std::fs::write(&path, b"{oops").unwrap();
        assert!(load_from(&path).is_none());
        let future = SnapshotV2 {
            version: VERSION + 1,
            windows: Vec::new(),
        };
        std::fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();
        assert!(load_v2_from(&path).is_none());
        assert!(load_from(&path).is_none());

        // 合法但空 windows：v2 可读，旧单窗口 API 没有窗口可恢复。
        let empty = SnapshotV2 {
            version: VERSION,
            windows: Vec::new(),
        };
        save_v2_to(&path, &empty).unwrap();
        assert_eq!(load_v2_from(&path), Some(empty));
        assert!(load_from(&path).is_none());

        // 旧 API 空 tabs 快照：仍然能读回，恢复端检查 tabs 后回退新 Tab。
        let empty_tabs = Snapshot {
            version: LEGACY_VERSION,
            cols: 80,
            rows: 24,
            active: 0,
            tabs: Vec::new(),
        };
        save_to(&path, &empty_tabs).unwrap();
        let loaded = load_from(&path).unwrap();
        assert!(loaded.tabs.is_empty());
    }

    #[test]
    fn crash_streak_bumps_on_unclean_and_resets_on_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        assert_eq!(crash_streak_at(&path), 0);

        // 第一次异常退出：0 -> 1
        assert_eq!(bump_crash_streak_at(&path), 1);
        // 同一次启动内的重复调用（后端回退会 exec 自身）不再累加
        assert_eq!(bump_crash_streak_at(&path), 1);

        // 模拟“下一次启动”：把时间戳改成很久以前，才会继续累加
        std::fs::write(crash_streak_path(&path), b"1 1").unwrap();
        assert_eq!(bump_crash_streak_at(&path), 2);
        assert_eq!(crash_streak_at(&path), 2);

        // 饱和：手改成 255 也不会溢出（旧格式无时间戳，允许立即 +1）
        std::fs::write(crash_streak_path(&path), b"255").unwrap();
        assert_eq!(bump_crash_streak_at(&path), 255);

        // 正常退出清零
        let _ = std::fs::remove_file(crash_streak_path(&path));
        assert_eq!(crash_streak_at(&path), 0);

        // 坏内容按 0 处理
        std::fs::write(crash_streak_path(&path), b"nonsense").unwrap();
        assert_eq!(crash_streak_at(&path), 0);
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

    #[test]
    fn restore_boundaries_distinguish_layout_from_processes_and_connections() {
        for zh in [true, false] {
            let b = restore_boundaries(zh);
            assert_eq!(b.restored.len(), 3);
            assert_eq!(b.not_restored.len(), 3);
            let text = restore_boundaries_text(zh);
            // 关键口径必须出现在用户可见文案里：新 shell / 历史 / 远端不自动连。
            if zh {
                assert!(text.contains("新的 shell"), "{text}");
                assert!(text.contains("滚动历史"), "{text}");
                assert!(text.contains("不自动建连"), "{text}");
                assert!(text.contains("分屏布局"), "{text}");
            } else {
                assert!(text.contains("new shell"), "{text}");
                assert!(text.contains("Scrollback"), "{text}");
                assert!(text.contains("never auto-connect"), "{text}");
                assert!(text.contains("Split layout"), "{text}");
            }
        }
        // 文案本身不得暗示“恢复进程”。
        assert!(!restore_boundaries_text(true).contains("恢复进程"));
    }
}
