//! 帧/单元格快照：UI 侧主动拉取的最小数据面。
//!
//! 设计要点（Phase 0 验证项）：
//! - `Frame` 由 UI 侧持有并复用（`Box<Frame>` 往返），稳态零分配；
//! - 拉取时只拷贝 `Term::damage()` 报告的行段，不做整屏复制；
//! - 颜色在此层解析为 RGB，UI 不再依赖 alacritty 的类型。

use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::CursorShape as VteCursorShape;

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
    _pad: u16,
}

impl Default for CellData {
    fn default() -> Self {
        Self { ch: ' ' as u32, fg: 0, bg: 0, flags: 0, _pad: 0 }
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
            fg: palette.resolve(cell.fg),
            bg: palette.resolve(cell.bg),
            flags: cell.flags.bits(),
            _pad: 0,
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

/// 可复用的终端帧缓冲。
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub cols: usize,
    pub rows: usize,
    /// `rows * cols` 个单元格，行优先。
    pub cells: Vec<CellData>,
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
    /// 单调递增序号，便于 UI 判断是否漏帧。
    pub generation: u64,
    /// 本帧从 grid 拷贝的单元格数（性能计数器）。
    pub copied_cells: u32,
}

impl Frame {
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut f = Self { cols, rows, ..Default::default() };
        f.cells = vec![CellData::default(); cols * rows];
        f
    }

    /// 尺寸变化时重建缓冲（仅此路径分配）。
    pub fn ensure_size(&mut self, cols: usize, rows: usize) -> bool {
        if self.cols == cols && self.rows == rows && self.cells.len() == cols * rows {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.cells = vec![CellData::default(); cols * rows];
        self.damage.clear();
        self.full = true;
        true
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
}
