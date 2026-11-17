// SPDX-License-Identifier: GPL-3.0-only
//! 应用层的分屏树与布局计算。
//!
//! 刻意不依赖 Slint / core：纯逻辑、可单测。
//! 布局结果是一张扁平列表（每个 pane 一个矩形），Slint 侧直接用 `for` 渲染。

pub type PaneId = u64;

/// 分屏方向：`Row` = 左右并排，`Col` = 上下排列。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Row,
    Col,
}

/// 最小分屏比例，避免某一侧被拖没。
pub const MIN_RATIO: f32 = 0.1;
/// 分屏之间的视觉间隔（逻辑像素）。
pub const SPLIT_GAP: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(PaneId),
    Split { dir: Dir, ratio: f32, a: Box<Node>, b: Box<Node> },
}

impl Node {
    #[allow(dead_code)]
    pub fn leaf(id: PaneId) -> Self {
        Node::Leaf(id)
    }

    pub fn split(&self, target: PaneId, dir: Dir, new_id: PaneId, ratio: f32) -> Node {
        match self {
            Node::Leaf(id) if *id == target => Node::Split {
                dir,
                ratio: ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO),
                a: Box::new(Node::Leaf(*id)),
                b: Box::new(Node::Leaf(new_id)),
            },
            Node::Leaf(_) => self.clone(),
            Node::Split { dir: d, ratio: r, a, b } => Node::Split {
                dir: *d,
                ratio: *r,
                a: Box::new(a.split(target, dir, new_id, ratio)),
                b: Box::new(b.split(target, dir, new_id, ratio)),
            },
        }
    }

    /// 删除一个叶子；返回 None 表示整棵树被删空。
    pub fn remove(&self, target: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(id) => {
                if *id == target {
                    None
                } else {
                    Some(self.clone())
                }
            },
            Node::Split { dir, ratio, a, b } => match (a.remove(target), b.remove(target)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    dir: *dir,
                    ratio: *ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                // 一侧被删空，另一侧顶上来（关闭 pane 后兄弟节点填满空间）
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(leaf) => *leaf == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// 交换两个叶子的 pane id，分屏树结构与所有 ratio 原样保留。
    ///
    /// 拖拽重排只换“哪个 pane 占哪个格子”，不重建 Split：因此手动拖过的
    /// 分隔条比例不会因为交换而跳回 50%。
    pub fn swapped(&self, a: PaneId, b: PaneId) -> Node {
        match self {
            Node::Leaf(leaf) if *leaf == a => Node::Leaf(b),
            Node::Leaf(leaf) if *leaf == b => Node::Leaf(a),
            Node::Leaf(leaf) => Node::Leaf(*leaf),
            Node::Split { dir, ratio, a: left, b: right } => Node::Split {
                dir: *dir,
                ratio: *ratio,
                a: Box::new(left.swapped(a, b)),
                b: Box::new(right.swapped(a, b)),
            },
        }
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.collect(out);
                b.collect(out);
            },
        }
    }

    /// 计算每个 pane 的矩形（含 gap）。
    pub fn layout(&self, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, rect)),
            Node::Split { dir, ratio, a, b } => {
                let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                let (ra, rb) = match dir {
                    Dir::Row => {
                        let usable = (rect.w - SPLIT_GAP).max(0.0);
                        let wa = usable * ratio;
                        (
                            Rect::new(rect.x, rect.y, wa, rect.h),
                            Rect::new(rect.x + wa + SPLIT_GAP, rect.y, usable - wa, rect.h),
                        )
                    },
                    Dir::Col => {
                        let usable = (rect.h - SPLIT_GAP).max(0.0);
                        let ha = usable * ratio;
                        (
                            Rect::new(rect.x, rect.y, rect.w, ha),
                            Rect::new(rect.x, rect.y + ha + SPLIT_GAP, rect.w, usable - ha),
                        )
                    },
                };
                a.layout(ra, out);
                b.layout(rb, out);
            },
        }
    }

    /// 收集所有分隔条（用于 UI 渲染和拖拽命中）。
    pub fn dividers(&self, rect: Rect, out: &mut Vec<Divider>) {
        let mut path = Vec::new();
        self.collect_dividers(rect, &mut path, out);
    }

    fn collect_dividers(&self, rect: Rect, path: &mut Vec<bool>, out: &mut Vec<Divider>) {
        let Node::Split { dir, ratio, a, b } = self else { return };
        let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        let (ra, rb, divider) = match dir {
            Dir::Row => {
                let usable = (rect.w - SPLIT_GAP).max(0.0);
                let wa = usable * ratio;
                let ra = Rect::new(rect.x, rect.y, wa, rect.h);
                let rb = Rect::new(rect.x + wa + SPLIT_GAP, rect.y, usable - wa, rect.h);
                let d = Rect::new(rect.x + wa, rect.y, SPLIT_GAP, rect.h);
                (ra, rb, d)
            },
            Dir::Col => {
                let usable = (rect.h - SPLIT_GAP).max(0.0);
                let ha = usable * ratio;
                let ra = Rect::new(rect.x, rect.y, rect.w, ha);
                let rb = Rect::new(rect.x, rect.y + ha + SPLIT_GAP, rect.w, usable - ha);
                let d = Rect::new(rect.x, rect.y + ha, rect.w, SPLIT_GAP);
                (ra, rb, d)
            },
        };
        out.push(Divider { path: path.clone(), rect: divider, parent: rect, dir: *dir });
        path.push(false);
        a.collect_dividers(ra, path, out);
        path.pop();
        path.push(true);
        b.collect_dividers(rb, path, out);
        path.pop();
    }

    /// 按路径设置分屏比例；返回是否命中。
    pub fn set_ratio(&mut self, path: &[bool], ratio: f32) -> bool {
        let Node::Split { a, b, ratio: r, .. } = self else { return false };
        match path.split_first() {
            None => {
                *r = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                true
            },
            Some((false, rest)) => a.set_ratio(rest, ratio),
            Some((true, rest)) => b.set_ratio(rest, ratio),
        }
    }

}

/// 一条可拖拽的分隔条。
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    /// 从根到该 Split 的分支路径：false = a 侧，true = b 侧。
    pub path: Vec<bool>,
    /// 视觉 gap 的矩形（1px 线画在中间）。
    pub rect: Rect,
    /// 该 Split 的父矩形，用于把鼠标位置换算成比例。
    pub parent: Rect,
    pub dir: Dir,
}


/// UI-PANE-007：最大化单个 pane 的**视图**布局（纯函数，便于单测）。
///
/// 只在 `pane` 确实属于当前布局时返回 `Some`：pane 独占整个内容区、分隔条全部
/// 隐藏。分屏树与比例不变，因此不改 Session/快照；传错 pane id 返回 `None`，
/// 调用方保持原布局。
pub fn zoom_layout(
    pane: PaneId,
    content: Rect,
    rects: &[(PaneId, Rect)],
    dividers: &[Divider],
) -> Option<(Vec<(PaneId, Rect)>, Vec<Divider>)> {
    if !rects.iter().any(|(id, _)| *id == pane) {
        return None;
    }
    Some((vec![(pane, content)], Vec::new()))
}

/// 一个标签页。
#[derive(Clone, Debug)]
pub struct Tab {
    pub title: String,
    pub root: Option<Node>,
    pub focused: PaneId,
    /// 每个 pane 最近一次用于布局的网格尺寸（列, 行），用于检测是否需要 resize。
    pub grid: std::collections::HashMap<PaneId, (usize, usize)>,
    /// Pin 到标签栏左侧固定区。会话快照 additive 保存，旧快照视为未 Pin。
    pub pinned: bool,
    /// 重命名前的标题（仅内存，不进会话快照）：供「撤销重命名」恢复。
    pub prev_title: Option<String>,
}

impl Tab {
    pub fn single(id: PaneId, title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            root: Some(Node::Leaf(id)),
            focused: id,
            grid: std::collections::HashMap::new(),
            pinned: false,
            prev_title: None,
        }
    }

    pub fn split(&mut self, target: PaneId, dir: Dir, new_id: PaneId) {
        if let Some(root) = &self.root {
            self.root = Some(root.split(target, dir, new_id, 0.5));
        }
    }

    /// 删除 pane；返回是否把整个 tab 删空了。
    pub fn remove(&mut self, target: PaneId) -> bool {
        let Some(root) = &self.root else { return true };
        self.root = root.remove(target);
        self.grid.remove(&target);
        if self.root.is_none() {
            return true;
        }
        if self.focused == target {
            if let Some(root) = &self.root {
                if let Some(first) = root.panes().first() {
                    self.focused = *first;
                }
            }
        }
        false
    }

    pub fn panes(&self) -> Vec<PaneId> {
        self.root.as_ref().map(|r| r.panes()).unwrap_or_default()
    }

    /// 交换同一 Tab 内两个 pane 的位置；返回是否真的发生交换。
    ///
    /// 只改叶子 id：pane 的 PTY/运行时/网格尺寸都跟着 PaneId 走，不需要重启。
    pub fn swap_panes(&mut self, a: PaneId, b: PaneId) -> bool {
        if a == b {
            return false;
        }
        let Some(root) = &self.root else { return false };
        if !(root.contains(a) && root.contains(b)) {
            return false;
        }
        self.root = Some(root.swapped(a, b));
        true
    }
}

#[derive(Clone, Debug, Default)]
pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_id: PaneId,
}

impl Workspace {
    pub fn new() -> Self {
        Self { tabs: Vec::new(), active: 0, next_id: 1 }
    }

    pub fn alloc_id(&mut self) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    #[allow(dead_code)]
    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// pinned Tab 的数量。布局约定：pinned 永远构成 `tabs` 的前缀。
    pub fn pinned_count(&self) -> usize {
        self.tabs.iter().filter(|t| t.pinned).count()
    }

    /// 把第 `from` 个 Tab 移到 `to` 位置；越界、相同或跨 pinned 组返回 false。
    pub fn move_tab(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return false;
        }
        // 普通排序只能在同组内移动：pinned 留在左侧固定区，unpinned 不能插进去。
        let pinned_count = self.pinned_count();
        let (lo, hi) = if self.tabs[from].pinned {
            (0, pinned_count)
        } else {
            (pinned_count, self.tabs.len())
        };
        if to < lo || to >= hi {
            return false;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.track_active_after_move(from, to);
        true
    }

    /// 根据 remove+insert 的位置修正 active 下标（调用前已修改 tabs）。
    fn track_active_after_move(&mut self, from: usize, to: usize) {
        if self.active == from {
            self.active = to;
        } else if from < self.active && self.active <= to {
            self.active -= 1;
        } else if to <= self.active && self.active < from {
            self.active += 1;
        }
    }

    /// Pin/Unpin 第 `index` 个 Tab；返回移动后的新下标（状态未变返回 None）。
    ///
    /// 两种方向都移到「pinned 前缀的下一个槽位」：Pin 时即最后一个 pinned
    /// 之后；Unpin 时即第一个 unpinned 之前。
    pub fn set_tab_pinned(&mut self, index: usize, pinned: bool) -> Option<usize> {
        if index >= self.tabs.len() || self.tabs[index].pinned == pinned {
            return None;
        }
        let mut tab = self.tabs.remove(index);
        tab.pinned = pinned;
        let to = self
            .tabs
            .iter()
            .position(|t| !t.pinned)
            .unwrap_or(self.tabs.len());
        self.tabs.insert(to, tab);
        self.track_active_after_move(index, to);
        Some(to)
    }

    /// 按组插入一个 Tab（恢复会话/重开最近关闭用）；返回插入下标。
    /// pinned 插到 pinned 区末尾，unpinned 追加到末尾。
    pub fn insert_tab(&mut self, tab: Tab) -> usize {
        let index = if tab.pinned {
            self.tabs
                .iter()
                .position(|t| !t.pinned)
                .unwrap_or(self.tabs.len())
        } else {
            self.tabs.len()
        };
        self.tabs.insert(index, tab);
        index
    }

    /// 修正外部快照可能带来的 pinned/unpinned 交错顺序（稳定排序），active 跟着 Tab 走。
    pub fn normalize_pinned(&mut self) {
        let len = self.tabs.len();
        if len < 2 || self.tabs.windows(2).all(|w| w[0].pinned || !w[1].pinned) {
            return;
        }
        let active = self.active.min(len - 1);
        let active_pinned = self.tabs[active].pinned;
        let pinned_before = self.tabs[..active].iter().filter(|t| t.pinned).count();
        let unpinned_before = self.tabs[..active].iter().filter(|t| !t.pinned).count();
        let pinned_total = self.pinned_count();
        // sort_by_key 是稳定排序：同组内保持快照相对顺序。
        self.tabs.sort_by_key(|t| !t.pinned);
        self.active = if active_pinned {
            pinned_before
        } else {
            pinned_total + unpinned_before
        }
        .min(len - 1);
    }

    /// 只保留第 `index` 个 Tab；返回被丢弃 Tab 的 pane id（调用方负责关 PTY）。
    pub fn keep_only(&mut self, index: usize) -> Vec<PaneId> {
        if index >= self.tabs.len() {
            return Vec::new();
        }
        let mut dropped = Vec::new();
        let mut kept = None;
        for (i, tab) in std::mem::take(&mut self.tabs).into_iter().enumerate() {
            if i == index {
                kept = Some(tab);
            } else {
                dropped.extend(tab.panes());
            }
        }
        if let Some(tab) = kept {
            self.tabs.push(tab);
        }
        self.active = 0;
        dropped
    }

    pub fn tab_of(&self, pane: PaneId) -> Option<usize> {
        self.tabs.iter().position(|t| t.root.as_ref().is_some_and(|r| r.contains(pane)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(node: &Node, w: f32, h: f32) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        node.layout(Rect::new(0.0, 0.0, w, h), &mut out);
        out
    }

    #[test]
    fn single_leaf_fills_rect() {
        let node = Node::leaf(1);
        let r = rects(&node, 800.0, 600.0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, Rect::new(0.0, 0.0, 800.0, 600.0));
    }

    #[test]
    fn split_row_divides_width_with_gap() {
        let node = Node::leaf(1).split(1, Dir::Row, 2, 0.5);
        let r = rects(&node, 800.0, 600.0);
        assert_eq!(r.len(), 2);
        assert!((r[0].1.w - (800.0 - SPLIT_GAP) / 2.0).abs() < 0.01);
        assert_eq!(r[1].1.x, r[0].1.x + r[0].1.w + SPLIT_GAP);
        assert_eq!(r[0].1.h, 600.0);
    }

    #[test]
    fn split_col_divides_height() {
        let node = Node::leaf(1).split(1, Dir::Col, 2, 0.5);
        let r = rects(&node, 800.0, 600.0);
        assert!((r[0].1.h - (600.0 - SPLIT_GAP) / 2.0).abs() < 0.01);
        assert_eq!(r[1].1.y, r[0].1.y + r[0].1.h + SPLIT_GAP);
    }

    #[test]
    fn remove_collapses_to_sibling() {
        let node = Node::leaf(1).split(1, Dir::Row, 2, 0.5);
        let after = node.remove(2).expect("还剩一个");
        assert_eq!(after.panes(), vec![1]);
        assert_eq!(rects(&after, 800.0, 600.0)[0].1, Rect::new(0.0, 0.0, 800.0, 600.0));
    }

    #[test]
    fn remove_last_leaf_returns_none() {
        assert!(Node::leaf(7).remove(7).is_none());
    }

    #[test]
    fn nested_split_layout_is_contained() {
        let node = Node::leaf(1).split(1, Dir::Row, 2, 0.5).split(2, Dir::Col, 3, 0.5);
        let r = rects(&node, 1000.0, 500.0);
        assert_eq!(r.len(), 3);
        for (_, rect) in &r {
            assert!(rect.x >= 0.0 && rect.y >= 0.0);
            assert!(rect.x + rect.w <= 1000.0 + 0.01);
            assert!(rect.y + rect.h <= 500.0 + 0.01);
        }
        assert_eq!(r[0].0, 1);
        assert_eq!(r[1].0, 2);
        assert_eq!(r[2].0, 3);
    }

    #[test]
    fn ratio_is_clamped() {
        let node = Node::leaf(1).split(1, Dir::Row, 2, 0.999);
        let r = rects(&node, 1000.0, 100.0);
        assert!(r[1].1.w >= (1000.0 - SPLIT_GAP) * MIN_RATIO - 0.01);
    }

    #[test]
    fn swap_panes_keeps_split_ratios_and_geometry() {
        // 先左右 30/70，再把右侧上下 40/60；交换左 pane(1) 与右下 pane(3)。
        let mut node = Node::leaf(1).split(1, Dir::Row, 2, 0.3);
        node = node.split(2, Dir::Col, 3, 0.4);
        let before = rects(&node, 1000.0, 500.0);
        let before_dividers = {
            let mut out = Vec::new();
            node.dividers(Rect::new(0.0, 0.0, 1000.0, 500.0), &mut out);
            out
        };
        let swapped = node.swapped(1, 3);
        assert_eq!(swapped.panes(), vec![3, 2, 1]);
        // 几何/比例完全不变：交换只换 id，不重排 Split。
        let after = rects(&swapped, 1000.0, 500.0);
        for ((id, r), (old_id, old_r)) in after.iter().zip(before.iter()) {
            assert_eq!(*r, *old_r, "pane {id} 不应改变格子几何（原 {old_id}）");
        }
        let after_dividers = {
            let mut out = Vec::new();
            swapped.dividers(Rect::new(0.0, 0.0, 1000.0, 500.0), &mut out);
            out
        };
        assert_eq!(before_dividers, after_dividers, "分隔条比例必须原样保留");

        // 同一对 id 再交换一次回到原布局。
        let back = swapped.swapped(1, 3);
        let mut back_dividers = Vec::new();
        back.dividers(Rect::new(0.0, 0.0, 1000.0, 500.0), &mut back_dividers);
        assert_eq!(back_dividers, before_dividers);
    }

    #[test]
    fn swap_panes_rejects_missing_or_same_pane() {
        let mut tab = Tab::single(1, "t");
        tab.split(1, Dir::Row, 2);
        assert!(!tab.swap_panes(1, 1), "自己和自己不算交换");
        assert!(!tab.swap_panes(1, 99), "不在树里的 pane 不交换");
        assert!(tab.swap_panes(1, 2));
        assert_eq!(tab.panes(), vec![2, 1]);
        assert_eq!(tab.focused, 1, "焦点跟着 pane 身份走，不换成格子");
    }

    #[test]
    fn tab_focus_moves_after_removal() {
        let mut tab = Tab::single(1, "t");
        tab.split(1, Dir::Row, 2);
        tab.focused = 2;
        assert!(!tab.remove(2));
        assert_eq!(tab.focused, 1);
        assert!(tab.remove(1));
    }

    #[test]
    fn dividers_match_split_gap() {
        let node = Node::leaf(1).split(1, Dir::Row, 2, 0.5);
        let mut out = Vec::new();
        node.dividers(Rect::new(0.0, 0.0, 800.0, 600.0), &mut out);
        assert_eq!(out.len(), 1);
        let d = &out[0];
        assert!(d.path.is_empty());
        assert_eq!(d.dir, Dir::Row);
        assert!((d.rect.x - (800.0 - SPLIT_GAP) / 2.0).abs() < 0.01);
        assert_eq!(d.rect.w, SPLIT_GAP);
        assert_eq!(d.rect.h, 600.0);
    }

    #[test]
    fn nested_dividers_have_paths() {
        // 先左右分，再把右侧上下分
        let node = Node::leaf(1).split(1, Dir::Row, 2, 0.5).split(2, Dir::Col, 3, 0.5);
        let mut out = Vec::new();
        node.dividers(Rect::new(0.0, 0.0, 1000.0, 500.0), &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, Vec::<bool>::new());
        assert_eq!(out[0].dir, Dir::Row);
        assert_eq!(out[1].path, vec![true]);
        assert_eq!(out[1].dir, Dir::Col);
    }

    #[test]
    fn set_ratio_updates_layout_and_clamps() {
        let mut node = Node::leaf(1).split(1, Dir::Row, 2, 0.5);
        assert!(node.set_ratio(&[], 0.25));
        let r = rects(&node, 1000.0, 100.0);
        assert!((r[0].1.w - (1000.0 - SPLIT_GAP) * 0.25).abs() < 0.01);

        assert!(node.set_ratio(&[], 0.99));
        let r = rects(&node, 1000.0, 100.0);
        assert!(r[1].1.w >= (1000.0 - SPLIT_GAP) * MIN_RATIO - 0.01);

        // 路径不存在
        assert!(!Node::leaf(1).set_ratio(&[], 0.3));
        assert!(!node.set_ratio(&[true], 0.3));
    }

    #[test]
    fn move_tab_reorders_and_tracks_active() {
        let mut ws = Workspace::new();
        let ids: Vec<PaneId> = (0..4).map(|_| ws.alloc_id()).collect();
        for (i, id) in ids.iter().enumerate() {
            ws.tabs.push(Tab::single(*id, format!("t{i}")));
        }
        ws.active = 2;

        // 0 → 3：其它 Tab 前移，active 跟着前移
        assert!(ws.move_tab(0, 3));
        assert_eq!(ws.tabs.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), vec!["t1", "t2", "t3", "t0"]);
        assert_eq!(ws.active, 1);

        // 3 → 0：后面的往后挪，active 回到 2
        assert!(ws.move_tab(3, 0));
        assert_eq!(ws.active, 2);

        // 拖自己/越界不动
        assert!(!ws.move_tab(1, 1));
        assert!(!ws.move_tab(0, 9));
        assert_eq!(ws.tabs.len(), 4);
    }

    #[test]
    fn keep_only_drops_other_tabs_and_reports_panes() {
        let mut ws = Workspace::new();
        let a = ws.alloc_id();
        let b = ws.alloc_id();
        let c = ws.alloc_id();
        ws.tabs.push(Tab::single(a, "a"));
        ws.tabs.push(Tab::single(b, "b"));
        ws.tabs.push(Tab::single(c, "c"));
        ws.active = 2;

        let dropped = ws.keep_only(1);
        assert_eq!(dropped, vec![a, c]);
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[0].panes(), vec![b]);
        assert_eq!(ws.active, 0);
        // 越界不 panic、也不改状态
        assert!(ws.keep_only(5).is_empty());
        assert_eq!(ws.tabs.len(), 1);
    }

    #[test]
    fn workspace_finds_tab_of_pane() {
        let mut ws = Workspace::new();
        let a = ws.alloc_id();
        let b = ws.alloc_id();
        ws.tabs.push(Tab::single(a, "a"));
        ws.tabs.push(Tab::single(b, "b"));
        assert_eq!(ws.tab_of(a), Some(0));
        assert_eq!(ws.tab_of(b), Some(1));
        assert_eq!(ws.tab_of(999), None);
    }

    fn titles(ws: &Workspace) -> Vec<&str> {
        ws.tabs.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn pin_moves_to_last_pinned_and_unpin_to_first_unpinned() {
        let mut ws = Workspace::new();
        for (i, title) in ["a", "b", "c", "d"].iter().enumerate() {
            ws.tabs.push(Tab::single(i as PaneId + 1, *title));
        }

        // 没有 pinned 时 Pin b：进入 pinned 区（最左），后面的 Tab 前移。
        assert_eq!(ws.set_tab_pinned(1, true), Some(0));
        assert_eq!(titles(&ws), vec!["b", "a", "c", "d"]);
        assert_eq!(ws.pinned_count(), 1);

        // Pin c：放到最后一个 pinned（b）之后。
        assert_eq!(ws.set_tab_pinned(2, true), Some(1));
        assert_eq!(titles(&ws), vec!["b", "c", "a", "d"]);
        assert_eq!(ws.pinned_count(), 2);

        // Unpin b：回到第一个 unpinned 之前（a 前）。
        assert_eq!(ws.set_tab_pinned(0, false), Some(1));
        assert_eq!(titles(&ws), vec!["c", "b", "a", "d"]);
        assert_eq!(ws.pinned_count(), 1);

        // 已经是目标状态：不动。
        assert_eq!(ws.set_tab_pinned(0, true), None);
        assert_eq!(ws.set_tab_pinned(99, true), None);
    }

    #[test]
    fn pin_keeps_active_tab_following_its_position() {
        let mut ws = Workspace::new();
        for (i, title) in ["a", "b", "c"].iter().enumerate() {
            ws.tabs.push(Tab::single(i as PaneId + 1, *title));
        }
        ws.active = 2; // c

        // Pin b（active 在它右边）：c 的下标保持不变。
        ws.set_tab_pinned(1, true).unwrap();
        assert_eq!(titles(&ws), vec!["b", "a", "c"]);
        assert_eq!(ws.active, 2);
        assert_eq!(ws.tabs[ws.active].title, "c");

        // Pin 当前 active 的 c：active 跟着 Tab 走。
        ws.set_tab_pinned(2, true).unwrap();
        assert_eq!(titles(&ws), vec!["b", "c", "a"]);
        assert_eq!(ws.active, 1);
        assert_eq!(ws.tabs[ws.active].title, "c");

        // Unpin 当前 active：仍指向同一个 Tab。
        ws.set_tab_pinned(1, false).unwrap();
        assert_eq!(titles(&ws), vec!["b", "c", "a"]);
        assert_eq!(ws.active, 1);
        assert_eq!(ws.tabs[ws.active].title, "c");
    }

    #[test]
    fn normal_reorder_never_crosses_pinned_group() {
        let mut ws = Workspace::new();
        let mut a = Tab::single(1, "a");
        let b = Tab::single(2, "b");
        let mut c = Tab::single(3, "c");
        let d = Tab::single(4, "d");
        a.pinned = true;
        c.pinned = true;
        ws.tabs = vec![a, c, b, d];
        ws.active = 2;

        // pinned 只能落在 0..pinned_count，不能拖到 unpinned 区。
        assert!(!ws.move_tab(0, 2));
        assert!(!ws.move_tab(0, 3));
        assert!(ws.move_tab(0, 1));
        assert_eq!(titles(&ws), vec!["c", "a", "b", "d"]);
        assert_eq!(ws.active, 2);

        // unpinned 不能插进 pinned 区。
        assert!(!ws.move_tab(3, 1));
        assert!(!ws.move_tab(2, 0));
        assert!(ws.move_tab(3, 2));
        assert_eq!(titles(&ws), vec!["c", "a", "d", "b"]);
    }

    #[test]
    fn insert_tab_and_normalize_keep_pinned_prefix() {
        let mut ws = Workspace::new();
        ws.tabs.push(Tab::single(1, "a"));
        ws.tabs.push(Tab::single(2, "b"));

        let mut p = Tab::single(3, "p");
        p.pinned = true;
        let at = ws.insert_tab(p);
        assert_eq!(at, 0);
        assert_eq!(titles(&ws), vec!["p", "a", "b"]);
        assert_eq!(ws.pinned_count(), 1);

        // 模拟坏快照：pinned 出现在 unpinned 后面，normalize 后回到前缀。
        let mut u = Tab::single(4, "u");
        u.pinned = false;
        let mut p2 = Tab::single(5, "p2");
        p2.pinned = true;
        ws.tabs = vec![ws.tabs[0].clone(), u, p2];
        ws.active = 1; // u
        ws.normalize_pinned();
        assert_eq!(titles(&ws), vec!["p", "p2", "u"]);
        assert_eq!(ws.active, 2);
        assert_eq!(ws.tabs[ws.active].title, "u");
    }

    #[test]
    fn zoom_layout_collapses_view_without_touching_split_tree() {
        let content = Rect::new(0.0, 0.0, 800.0, 600.0);
        let rects = vec![
            (1, Rect::new(0.0, 0.0, 400.0, 600.0)),
            (2, Rect::new(400.0, 0.0, 400.0, 600.0)),
        ];
        let dividers = vec![Divider {
            path: vec![],
            rect: Rect::new(399.0, 0.0, 2.0, 600.0),
            parent: content,
            dir: Dir::Row,
        }];
        let (zoomed, bars) = zoom_layout(2, content, &rects, &dividers).expect("pane 存在");
        assert_eq!(zoomed, vec![(2, content)], "最大化 pane 独占内容区");
        assert!(bars.is_empty(), "最大化时不显示分隔条");
        assert_eq!(rects.len(), 2, "原布局不被修改，还原靠它");
        assert!(zoom_layout(99, content, &rects, &dividers).is_none(), "陌生 pane 不改布局");
    }
}
