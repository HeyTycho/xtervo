// SPDX-License-Identifier: GPL-3.0-only
//! 帧/单元格快照：UI 侧主动拉取的最小数据面。
//!
//! 设计要点（Phase 0 验证项）：
//! - `Frame` 由 UI 侧持有并复用（`Box<Frame>` 往返），稳态零分配；
//! - 拉取时只拷贝 `Term::damage()` 报告的行段，不做整屏复制；
//! - 颜色在此层解析为 RGB，UI 不再依赖 alacritty 的类型。

use std::collections::HashMap;
use std::sync::Arc;

use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::CursorShape as VteCursorShape;

use crate::link::{scan, LinkKind};
use crate::palette::Palette;

/// 单元格标志位，数值与 `alacritty_terminal::term::cell::Flags` 的 bits 一一对应。
pub mod flags {
    pub const INVERSE: u16 = 0b0000_0000_0000_0001;
    pub const BOLD: u16 = 0b0000_0000_0000_0010;
    pub const ITALIC: u16 = 0b0000_0000_0000_0100;
    pub const UNDERLINE: u16 = 0b0000_0000_0000_1000;
    pub const WRAPLINE: u16 = 0b0000_0000_0001_0000;
    pub const WIDE_CHAR: u16 = 0b0000_0000_0010_0000;
    pub const WIDE_CHAR_SPACER: u16 = 0b0000_0000_0100_0000;
    pub const DIM: u16 = 0b0000_0000_1000_0000;
    pub const HIDDEN: u16 = 0b0000_0001_0000_0000;
    pub const STRIKEOUT: u16 = 0b0000_0010_0000_0000;
    pub const DOUBLE_UNDERLINE: u16 = 0b0000_1000_0000_0000;
    pub const UNDERLINE_MASK: u16 =
        UNDERLINE | DOUBLE_UNDERLINE | 0b0001_0000_0000_0000 | 0b0010_0000_0000_0000 | 0b0100_0000_0000_0000;
}

/// 单个单元格的渲染数据。16 字节，可直接 memcpy。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellData {
    pub ch: u32,
    /// 0x00RRGGBB
    pub fg: u32,
    /// 0x00RRGGBB
    pub bg: u32,
    pub flags: u16,
    /// OSC 8 超链接在 [`Frame::hyperlinks`] 里的下标；0 = 无链接。
    /// 复用原来的 padding，`CellData` 仍是 16 字节。
    pub link: u16,
}

impl Default for CellData {
    fn default() -> Self {
        Self { ch: ' ' as u32, fg: 0, bg: 0, flags: 0, link: 0 }
    }
}

impl CellData {
    #[inline]
    pub fn char(&self) -> char {
        char::from_u32(self.ch).unwrap_or(' ')
    }

    #[inline]
    pub fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }

    #[inline]
    pub fn from_cell(cell: &Cell, palette: &Palette) -> Self {
        Self {
            ch: cell.c as u32,
            fg: palette.resolve_fg(cell.fg, cell.flags.contains(Flags::BOLD)),
            bg: palette.resolve(cell.bg),
            flags: cell.flags.bits(),
            link: 0,
        }
    }
}

/// 一次拉取中脏行的列区间（行号为视口内 0-based 行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineDamage {
    pub line: u16,
    pub left: u16,
    pub right: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

impl CursorShape {
    pub fn from_vte(shape: VteCursorShape) -> Self {
        match shape {
            VteCursorShape::Block => Self::Block,
            VteCursorShape::Underline => Self::Underline,
            VteCursorShape::Beam => Self::Beam,
            VteCursorShape::HollowBlock => Self::HollowBlock,
            VteCursorShape::Hidden => Self::Hidden,
        }
    }

    /// GNOME Terminal `cursor-shape` 配置值 → 形状。
    /// 接受 `block` / `underline` / `ibeam`（也兼容 `beam`/`bar`、大小写与
    /// `-`/`_`/空格分隔）；未知值回退 block，避免手写配置让光标消失。
    pub fn from_name(name: &str) -> Self {
        let key = name.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        match key.as_str() {
            "underline" | "under-line" => Self::Underline,
            "ibeam" | "i-beam" | "beam" | "bar" => Self::Beam,
            "hollow-block" | "hollow" => Self::HollowBlock,
            "hidden" | "none" => Self::Hidden,
            // block + 空 + 未知：GNOME 默认 block。
            _ => Self::Block,
        }
    }

    /// 配置里使用的稳定名称（`CursorShape` 是运行时值，Hidden 不用于配置）。
    pub fn config_name(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Underline => "underline",
            Self::Beam => "ibeam",
            Self::HollowBlock => "hollow-block",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorState {
    pub line: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

/// 视口内的行/列区间（用于选区与搜索高亮）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start_line: u16,
    pub start_col: u16,
    pub end_line: u16,
    pub end_col: u16,
}

impl Span {
    #[inline]
    pub fn contains(&self, line: u16, col: u16) -> bool {
        if line < self.start_line || line > self.end_line {
            return false;
        }
        if self.start_line == self.end_line {
            return col >= self.start_col && col <= self.end_col;
        }
        if line == self.start_line {
            return col >= self.start_col;
        }
        if line == self.end_line {
            return col <= self.end_col;
        }
        true
    }
}

/// 视口内的一张内联图片（Sixel 等）。
///
/// 图片像素不进入 `cells`，因此不参与文本、选区、搜索与链接；UI 把它们
/// 合成在 cell 背景之上、文字之下。`data` 是直通 RGBA8（未预乘），
/// `row`/`col` 是当前视口内的锚点（`row` 可以为负，表示顶部在视口上方）。
#[derive(Clone)]
pub struct InlineImage {
    /// actor 内单调递增的 placement ID；UI 用它关联点击命中。
    pub id: u64,
    /// 本次 placement 的源像素 ID；同一 placement 源数据变化时它会变。
    pub source_id: u64,
    /// 稳定的逻辑行身份（alacritty `RowId`）。滚动/换屏/历史淘汰都不会
    /// 改变它；逻辑行消失（reset / 清屏 / alt-screen 切换）时 placement
    /// 会被显式丢弃。`row` 只是它在当前帧视口中的投影。
    pub row_id: u64,
    /// 视口行（0 = 顶行；负值表示滚动到视口上方）。
    pub row: i32,
    /// 图片左上角所在列（cell）。
    pub col: u16,
    /// 像素宽高（Sixel 的像素尺寸，不随字号缩放）。
    pub width: u32,
    pub height: u32,
    /// 直通 RGBA8，长度 = width * height * 4。
    pub data: Arc<[u8]>,
    /// 协议 z-index；Sixel/IIP 默认 -1（文字之下）。
    pub z_index: i32,
    /// 是否应绘制在 glyphs 之后（Kitty 非负 z / 上层策略）。
    ///
    /// 当前 CPU/Slint 渲染器仍统一把图片画在文字之下；Scene/Renderer
    /// 接入后必须按该字段分层，不能把 Kitty AboveText 静默降级。
    pub above_text: bool,
}

impl std::fmt::Debug for InlineImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineImage")
            .field("id", &self.id)
            .field("source_id", &self.source_id)
            .field("row_id", &self.row_id)
            .field("row", &self.row)
            .field("col", &self.col)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.data.len())
            .field("z_index", &self.z_index)
            .field("above_text", &self.above_text)
            .finish()
    }
}

/// 源图像里被 placement 显示的矩形（像素坐标）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageCrop {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// placement 的协议元数据。
///
/// 旧的扁平 [`InlineImage`] 只有像素与锚点；z-index、源裁剪、目标 cell 矩形
/// 放在这里，老 UI 可以继续忽略，新 Scene/Renderer 可以直接消费。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePlacementMeta {
    /// 与 [`InlineImage::id`] 相同的 placement ID。
    pub placement_id: u64,
    /// 协议图片 ID（Sixel/IIP 用本地图片 ID）。
    pub image_id: u64,
    /// 稳定逻辑行身份。
    pub row_id: u64,
    /// 源裁剪；`None` 表示整张图。
    pub source_rect: Option<ImageCrop>,
    /// 目标 cell 宽/高（由协议 c/r 或像素尺寸推导）。
    pub cells_w: u16,
    pub cells_h: u16,
    /// Kitty 协议 z-index。
    pub z_index: i32,
    /// 是否应绘制在文字之上（Kitty 正 z / C=1 语义由协议层给出）。
    pub above_text: bool,
}

/// 可复用的终端帧缓冲。
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub cols: usize,
    pub rows: usize,
    /// `rows * cols` 个单元格，行优先。
    pub cells: Vec<CellData>,
    /// 视口每一行的稳定 RowId（下标 = 视口行，长度恒为 `rows`）。
    ///
    /// 来自 vendored alacritty `Grid::row_id`，滚动时跟着逻辑行走；图片
    /// placement 只认它，不再用 `scroll_total`/绝对行号推算。整屏和脏行
    /// 路径都会刷新（O(rows) 次 u64 写入，不分配）。
    pub row_ids: Vec<u64>,
    /// 本次拉取中发生变化的行段；`full == true` 时为空（表示整屏）。
    pub damage: Vec<LineDamage>,
    pub full: bool,
    pub cursor: CursorState,
    /// 当前选区（视口坐标）。
    pub selection: Option<Span>,
    /// 搜索命中高亮（视口坐标）。
    pub search: Option<Span>,
    /// `TermMode` 原始位。
    pub mode: u32,
    /// 视口相对滚动历史顶端的偏移。
    pub display_offset: usize,
    /// 当前视口内可见的内联图片（Sixel/IIP/Kitty），按绘制顺序排列。
    pub images: Vec<InlineImage>,
    /// 与 [`Frame::images`] 一一对应的 placement 元数据（顺序相同）。
    /// 老 UI 忽略；新 Scene/Renderer 用来还原 source crop、z-index 与目标
    /// cell 矩形。
    pub image_placements: Vec<ImagePlacementMeta>,
    /// 单调递增序号，便于 UI 判断是否漏帧。
    pub generation: u64,
    /// true = 本次拉帧超时被跳过，帧是空占位；UI 不应重绘，等下一次整屏补帧。
    pub skipped: bool,
    /// 本帧从 grid 拷贝的单元格数（性能计数器）。
    pub copied_cells: u32,
    /// OSC 8 链接表：下标 0 是占位（无链接），1.. 为 URI。
    /// 链接很少见，做成「单元格存 u16 索引 + 帧内字符串表」，避免每格塞 String。
    pub hyperlinks: Vec<String>,
    /// URI → 表下标，跨帧复用（partial 更新不会重建整表）。
    link_index: HashMap<String, u16>,
    /// 单元格下标 → 零宽码位（组合音标、ZWJ、变体选择符）。
    ///
    /// 这些码位在 alacritty 里挂在**前一个格子**上，不占新格。组合字符很少见，
    /// 用独立 side table 而不是塞进 `CellData`，保持 16 字节单元格的 memcpy 成本；
    /// 没有组合字符的 ASCII/CJK 会话完全不会触碰它。
    zerowidth: HashMap<usize, Vec<char>>,
}

impl Frame {
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut f = Self { cols, rows, ..Default::default() };
        f.cells = vec![CellData::default(); cols * rows];
        f.row_ids = vec![0; rows];
        f.reset_links();
        f
    }

    /// 尺寸变化时重建缓冲（仅此路径分配）。
    pub fn ensure_size(&mut self, cols: usize, rows: usize) -> bool {
        if self.cols == cols && self.rows == rows && self.cells.len() == cols * rows
            && self.row_ids.len() == rows
        {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.cells = vec![CellData::default(); cols * rows];
        self.row_ids = vec![0; rows];
        self.damage.clear();
        self.images.clear();
        self.image_placements.clear();
        self.full = true;
        self.reset_links();
        self.zerowidth.clear();
        true
    }

    /// 清空链接表并放回占位 0；整屏重取时调用。
    pub fn reset_links(&mut self) {
        self.hyperlinks.clear();
        self.hyperlinks.push(String::new());
        self.link_index.clear();
    }

    /// 清空零宽字符表；整屏重取前调用，避免已删除格子的组合字符残留。
    pub fn clear_zerowidth(&mut self) {
        self.zerowidth.clear();
    }

    /// 本帧是否有任何组合字符/ZWJ/变体选择符（渲染的逐格查找快速路径）。
    #[inline]
    pub fn has_zerowidth(&self) -> bool {
        !self.zerowidth.is_empty()
    }

    /// 取某格挂着的零宽码位（组合音标、ZWJ、变体选择符）；没有则返回空切片。
    #[inline]
    pub fn zerowidth_at(&self, row: usize, col: usize) -> &[char] {
        if self.zerowidth.is_empty() {
            return &[];
        }
        self.zerowidth
            .get(&(row * self.cols + col))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 登记一个 URI，返回表下标；0 表示放弃（表满）。
    pub fn intern(&mut self, uri: &str) -> u16 {
        if self.hyperlinks.is_empty() {
            self.hyperlinks.push(String::new());
        }
        if let Some(&idx) = self.link_index.get(uri) {
            return idx;
        }
        // 防止异常程序刷爆内存：表满后新链接一律降级为“无链接”
        if self.hyperlinks.len() >= 4096 {
            return 0;
        }
        let idx = self.hyperlinks.len() as u16;
        self.hyperlinks.push(uri.to_string());
        self.link_index.insert(uri.to_string(), idx);
        idx
    }

    /// 写一个单元格，同时登记它的超链接（如果有）。
    ///
    /// 这是整屏/脏行拷贝的最热路径：保持小函数并内联，零宽字符走单独的
    /// [`Frame::put_zerowidth`]，避免为罕见的组合字符给每格加分支和 hash 操作。
    #[inline]
    pub fn put(&mut self, index: usize, mut data: CellData, link: Option<&str>) {
        if let Some(uri) = link {
            data.link = self.intern(uri);
        }
        if let Some(slot) = self.cells.get_mut(index) {
            *slot = data;
        }
    }

    /// 写一个带零宽字符（组合音标/ZWJ/VS）的单元格。
    ///
    /// partial 更新时若该格原本有组合字符而本次没有，必须把旧记录删掉，否则会画
    /// 出幽灵重音；整屏路径先 `clear_zerowidth`，这里只做插入。
    pub fn put_zerowidth(
        &mut self,
        index: usize,
        data: CellData,
        link: Option<&str>,
        zerowidth: &[char],
    ) {
        self.put(index, data, link);
        if zerowidth.is_empty() {
            if !self.zerowidth.is_empty() {
                self.zerowidth.remove(&index);
            }
            return;
        }
        match self.zerowidth.get_mut(&index) {
            Some(existing) if existing.as_slice() == zerowidth => {},
            Some(existing) => {
                existing.clear();
                existing.extend_from_slice(zerowidth);
            },
            None => {
                self.zerowidth.insert(index, zerowidth.to_vec());
            },
        }
    }

    /// 取某格的超链接 URI。
    ///
    /// 宽字符占两列：右半边是 `WIDE_CHAR_SPACER`，某些应用不会把 OSC 8
    /// 复制到 spacer 上，这里回退到左半边，保证点右半边也能打开。
    #[inline]
    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<&str> {
        let index = row.checked_mul(self.cols)?.checked_add(col)?;
        let cell = self.cells.get(index)?;
        let mut idx = cell.link as usize;
        if idx == 0 && col > 0 && cell.has(flags::WIDE_CHAR_SPACER) {
            idx = self.cells.get(index - 1)?.link as usize;
        }
        (idx > 0).then(|| self.hyperlinks.get(idx)).flatten().map(|s| s.as_str())
    }

    /// 取某格上的行内链接（URL / IPv4[:port] / 本地路径）。
    ///
    /// 与 [`Frame::url_at`] 共用同一套显示列映射（宽字符/组合字符不会错位）；
    /// 不包含 OSC 8，调用方应先查 [`Frame::hyperlink_at`] 让显式超链接优先。
    /// 返回 `(类型, 文本)`，是否允许打开由 app 的白名单/落地策略决定。
    pub fn link_at(&self, row: usize, col: usize) -> Option<(LinkKind, String)> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let (text, char_of_col) = self.row_text_and_columns(row);
        let char_idx = *char_of_col.get(col)?;
        if char_idx == u32::MAX {
            return None;
        }
        scan(&text).into_iter().find_map(|link| {
            (link.start as u32 <= char_idx && char_idx <= link.end as u32)
                .then_some((link.kind, link.text))
        })
    }

    /// 取某格上的普通 URL（行内文本识别，不支持 OSC 8；调用方应先查
    /// [`Frame::hyperlink_at`]，让显式超链接优先）。
    ///
    /// 返回的是新分配的 URI 字符串，只在点击时调用。列号是**显示列**
    /// （与鼠标命中格一致），内部按单元格重建文本并记录“显示列 → 字符下标”：
    /// - 宽字符占一列文本、两列网格；右半 spacer 映射回同一字符；
    /// - 组合字符由 alacritty 存在单元格的 `zerowidth` 里，不占列，故不会错位；
    /// - 中文、全角标点等宽字符不会让 URL 列号偏移。
    pub fn url_at(&self, row: usize, col: usize) -> Option<String> {
        self.link_at(row, col)
            .and_then(|(kind, text)| (kind == LinkKind::Url).then_some(text))
    }

    /// 按显示列重建一行文本，并返回 `显示列 -> 文本字符下标`。
    /// spacer 不写入文本（宽字符只占一个字符），但会把它的列映射回左半边。
    fn row_text_and_columns(&self, row: usize) -> (String, Vec<u32>) {
        let mut text = String::with_capacity(self.cols);
        let mut char_of_col = vec![u32::MAX; self.cols];
        let mut char_idx = 0u32;
        for (col, cell) in self.row(row).iter().enumerate() {
            if cell.has(flags::WIDE_CHAR_SPACER) {
                if col > 0 {
                    char_of_col[col] = char_of_col[col - 1];
                }
                continue;
            }
            char_of_col[col] = char_idx;
            if cell.has(flags::WIDE_CHAR) && col + 1 < self.cols {
                char_of_col[col + 1] = char_idx;
            }
            text.push(cell.char());
            char_idx += 1;
        }
        (text, char_of_col)
    }

    #[inline]
    pub fn cell(&self, row: usize, col: usize) -> &CellData {
        &self.cells[row * self.cols + col]
    }

    #[inline]
    pub fn row(&self, row: usize) -> &[CellData] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }

    /// 是否整屏都过期（首次绘制、resize、滚动历史）。
    #[inline]
    pub fn needs_full_redraw(&self) -> bool {
        self.full || self.damage.len() > self.rows / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_bits_match_alacritty() {
        use alacritty_terminal::term::cell::Flags;
        assert_eq!(flags::INVERSE, Flags::INVERSE.bits());
        assert_eq!(flags::BOLD, Flags::BOLD.bits());
        assert_eq!(flags::ITALIC, Flags::ITALIC.bits());
        assert_eq!(flags::UNDERLINE, Flags::UNDERLINE.bits());
        assert_eq!(flags::WIDE_CHAR, Flags::WIDE_CHAR.bits());
        assert_eq!(flags::WIDE_CHAR_SPACER, Flags::WIDE_CHAR_SPACER.bits());
        assert_eq!(flags::HIDDEN, Flags::HIDDEN.bits());
        assert_eq!(flags::STRIKEOUT, Flags::STRIKEOUT.bits());
        assert_eq!(flags::DOUBLE_UNDERLINE, Flags::DOUBLE_UNDERLINE.bits());
    }

    #[test]
    fn cell_data_is_16_bytes() {
        assert_eq!(std::mem::size_of::<CellData>(), 16);
    }

    fn put_text(f: &mut Frame, row: usize, col: usize, text: &str) -> usize {
        let mut at = col;
        for ch in text.chars() {
            let mut cell = CellData::default();
            cell.ch = ch as u32;
            f.put(row * f.cols + at, cell, None);
            at += 1;
        }
        at
    }

    #[test]
    fn url_at_maps_display_columns_across_wide_chars() {
        let mut f = Frame::new(40, 2);
        // URL 前有中文和空格：中占 0/1 列、文占 2/3 列、空格 4 列、URL 从 5 列开始。
        put_text(&mut f, 0, 0, "中");
        f.cells[0].flags = flags::WIDE_CHAR;
        f.cells[1].ch = ' ' as u32;
        f.cells[1].flags = flags::WIDE_CHAR_SPACER;
        put_text(&mut f, 0, 2, "文");
        f.cells[2].flags = flags::WIDE_CHAR;
        f.cells[3].ch = ' ' as u32;
        f.cells[3].flags = flags::WIDE_CHAR_SPACER;
        let end = put_text(&mut f, 0, 4, " https://example.com/");
        put_text(&mut f, 0, end, "中");
        f.cells[end].flags = flags::WIDE_CHAR;
        f.cells[end + 1].ch = ' ' as u32;
        f.cells[end + 1].flags = flags::WIDE_CHAR_SPACER;
        put_text(&mut f, 0, end + 2, "文");
        f.cells[end + 2].flags = flags::WIDE_CHAR;
        f.cells[end + 3].ch = ' ' as u32;
        f.cells[end + 3].flags = flags::WIDE_CHAR_SPACER;

        assert_eq!(f.url_at(0, 5).as_deref(), Some("https://example.com/中文"));
        // 点击最后的中文（宽字符右半边 spacer）也要命中同一个 URL。
        let start = 5 + "https://example.com/".len();
        assert_eq!(f.url_at(0, start).as_deref(), Some("https://example.com/中文"));
        assert_eq!(f.url_at(0, start + 1).as_deref(), Some("https://example.com/中文"));
        assert_eq!(f.url_at(0, start + 2).as_deref(), Some("https://example.com/中文"));
        assert_eq!(f.url_at(0, start + 3).as_deref(), Some("https://example.com/中文"));
        // 宽字符左半边/空格不属于 URL；下一行也不命中。
        assert_eq!(f.url_at(0, 0), None);
        assert_eq!(f.url_at(0, 4), None);
        assert_eq!(f.url_at(1, 5), None);
        assert_eq!(f.url_at(2, 0), None, "越界行必须是 None");
        assert_eq!(f.url_at(0, 40), None, "越界列必须是 None");
    }

    #[test]
    fn url_at_does_not_shift_after_zero_width_combining_mark() {
        // 组合字符只占一个 CellData 的字符位、不新增网格列；URL 的显示列不能因此偏移。
        let mut f = Frame::new(32, 1);
        put_text(&mut f, 0, 0, "e");
        put_text(&mut f, 0, 1, "\u{0301}"); // 实际 alacritty 存在前一格 zerowidth，这里显式占格验证映射
        put_text(&mut f, 0, 2, " ");
        put_text(&mut f, 0, 3, "https://example.com/x");
        assert_eq!(f.url_at(0, 3).as_deref(), Some("https://example.com/x"));
        assert_eq!(f.url_at(0, 4).as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn link_at_returns_url_ipv4_and_path_for_app_side_filtering() {
        let mut f = Frame::new(64, 3);
        put_text(&mut f, 0, 0, "open /tmp/a.txt");
        put_text(&mut f, 1, 0, "http://127.0.0.1:8080/health");
        put_text(&mut f, 2, 0, "127.0.0.1:8080");

        assert_eq!(
            f.link_at(0, 7),
            Some((LinkKind::Path, "/tmp/a.txt".to_string()))
        );
        assert_eq!(f.url_at(0, 7), None, "url_at 仍只返回 URL");
        assert_eq!(
            f.link_at(1, 2),
            Some((LinkKind::Url, "http://127.0.0.1:8080/health".to_string()))
        );
        // 裸 IP:port 仍是可识别链接，但是否打开由 app 决定（不强制打开）。
        assert_eq!(
            f.link_at(2, 2),
            Some((LinkKind::Ipv4, "127.0.0.1:8080".to_string()))
        );
        assert_eq!(f.link_at(0, 0), None);
        assert_eq!(f.link_at(3, 0), None, "越界行必须是 None");
        assert_eq!(f.link_at(0, 64), None, "越界列必须是 None");
    }

    #[test]
    fn link_at_maps_path_columns_across_wide_and_combining_chars() {
        // 显示列：中=0/1、空格=2、e(+组合重音)=3、空格=4，路径从 5 开始。
        // 路径自身还含中文（路/径占两列），点右半边 spacer 也必须命中。
        let mut f = Frame::new(40, 1);
        put_text(&mut f, 0, 0, "中");
        f.cells[0].flags = flags::WIDE_CHAR;
        f.cells[1].ch = ' ' as u32;
        f.cells[1].flags = flags::WIDE_CHAR_SPACER;
        put_text(&mut f, 0, 2, " e ");
        let base = f.cell(0, 3).clone();
        f.put_zerowidth(3, base, None, &['\u{0301}']);
        let end = put_text(&mut f, 0, 5, "/tmp/");
        put_text(&mut f, 0, end, "路");
        f.cells[end].flags = flags::WIDE_CHAR;
        f.cells[end + 1].ch = ' ' as u32;
        f.cells[end + 1].flags = flags::WIDE_CHAR_SPACER;
        put_text(&mut f, 0, end + 2, "径");
        f.cells[end + 2].flags = flags::WIDE_CHAR;
        f.cells[end + 3].ch = ' ' as u32;
        f.cells[end + 3].flags = flags::WIDE_CHAR_SPACER;
        put_text(&mut f, 0, end + 4, "/a.txt");

        let want = Some((LinkKind::Path, "/tmp/路径/a.txt".to_string()));
        assert_eq!(f.link_at(0, 5), want, "路径起点");
        assert_eq!(f.link_at(0, 7), want, "ASCII 路径段");
        assert_eq!(f.link_at(0, end), want, "路径里的中文左半边");
        assert_eq!(f.link_at(0, end + 1), want, "路径里的中文右半边 spacer");
        assert_eq!(f.link_at(0, end + 2), want, "第二个中文字符");
        assert_eq!(f.url_at(0, end), None, "url_at 不返回路径");
        assert_eq!(f.link_at(0, 0), None, "中文前缀不是路径");
        assert_eq!(f.link_at(0, 2), None, "空格不是路径");
    }

    #[test]
    fn hyperlink_at_falls_back_to_wide_char_left_half() {
        let mut f = Frame::new(4, 1);
        f.hyperlinks.push("https://osc.example/中".to_string());
        let mut wide = CellData::default();
        wide.ch = '中' as u32;
        wide.flags = flags::WIDE_CHAR;
        wide.link = 1;
        f.put(0, wide, None);
        let mut spacer = CellData::default();
        spacer.ch = ' ' as u32;
        spacer.flags = flags::WIDE_CHAR_SPACER;
        // 故意不给 spacer 设置 link，验证点击右半边时回退到左半边。
        f.put(1, spacer, None);
        assert_eq!(f.hyperlink_at(0, 0), Some("https://osc.example/中"));
        assert_eq!(f.hyperlink_at(0, 1), Some("https://osc.example/中"));
    }

    #[test]
    fn zerowidth_marks_follow_their_base_cell() {
        let mut f = Frame::new(4, 1);
        let base = CellData { ch: 'e' as u32, ..CellData::default() };
        f.put_zerowidth(0, base, None, &['\u{301}']);
        assert_eq!(f.zerowidth_at(0, 0), &['\u{301}']);
        assert!(f.zerowidth_at(0, 1).is_empty());
        assert!(f.has_zerowidth());

        // partial 更新把带重音的格子覆盖成普通字符时，旧重音必须消失。
        f.put_zerowidth(0, base, None, &[]);
        assert!(!f.has_zerowidth());
        assert!(f.zerowidth_at(0, 0).is_empty());
    }

    #[test]
    fn zerowidth_table_is_cleared_on_resize() {
        let mut f = Frame::new(4, 1);
        f.put_zerowidth(0, CellData::default(), None, &['\u{200D}']);
        assert!(f.has_zerowidth());
        f.ensure_size(8, 2);
        assert!(!f.has_zerowidth(), "换尺寸后旧下标必须清空");
    }

    #[test]
    fn links_are_interned_and_deduped() {
        let mut f = Frame::new(4, 2);
        assert_eq!(f.hyperlinks.len(), 1, "占位 0 必须在");
        let a = f.intern("https://a.example");
        let b = f.intern("https://b.example");
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(f.intern("https://a.example"), a, "同 URI 复用下标");

        let mut cell = CellData::default();
        f.put(0, cell, Some("https://a.example"));
        cell.ch = 'x' as u32;
        f.put(1, cell, None);
        assert_eq!(f.hyperlink_at(0, 0), Some("https://a.example"));
        assert_eq!(f.hyperlink_at(0, 1), None, "没链接的格子必须是 None");
        assert_eq!(f.hyperlink_at(1, 0), None);
    }

    #[test]
    fn cursor_shape_config_names_roundtrip() {
        assert_eq!(CursorShape::from_name("block"), CursorShape::Block);
        assert_eq!(CursorShape::from_name("underline"), CursorShape::Underline);
        assert_eq!(CursorShape::from_name("ibeam"), CursorShape::Beam);
        assert_eq!(CursorShape::from_name("I-BEAM"), CursorShape::Beam);
        assert_eq!(CursorShape::from_name("beam"), CursorShape::Beam);
        assert_eq!(CursorShape::from_name(""), CursorShape::Block);
        assert_eq!(CursorShape::from_name("nope"), CursorShape::Block);
        for shape in [
            CursorShape::Block,
            CursorShape::Underline,
            CursorShape::Beam,
            CursorShape::HollowBlock,
            CursorShape::Hidden,
        ] {
            assert_eq!(CursorShape::from_name(shape.config_name()), shape);
        }
    }
}
