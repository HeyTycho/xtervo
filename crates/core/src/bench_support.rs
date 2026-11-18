// SPDX-License-Identifier: GPL-3.0-only
//! 无 PTY 的终端仿真夹具：用于解析吞吐基准与单元测试。
//!
//! 生产路径不经过这里——真实 pane 永远走 actor 线程。它只是把
//! `Term + Processor + fill_frame` 单独拿出来，便于隔离测量解析与取帧成本。

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;

use crate::frame::Frame;
use crate::palette::Palette;
use crate::pane::{fill_frame, GridSize};

pub struct TermSim {
    term: Term<VoidListener>,
    processor: Processor,
    palette: Palette,
    frame: Frame,
    size: GridSize,
    /// 零宽字符不产生 damage；检测到就要求下一帧整屏重取（与 PaneActor 一致）。
    force_full: bool,
    refresh_zerowidth: bool,
}

impl TermSim {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let size = GridSize::new(cols, rows);
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        let term = Term::new(config, &size, VoidListener);
        Self {
            term,
            processor: Processor::new(),
            palette: Palette::default(),
            frame: Frame::new(cols, rows),
            size,
            force_full: false,
            refresh_zerowidth: false,
        }
    }

    #[inline]
    pub fn advance(&mut self, bytes: &[u8]) {
        if crate::pane::has_zero_width_chars(bytes) {
            self.force_full = true;
            self.refresh_zerowidth = true;
        }
        self.processor.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.size = GridSize::new(cols, rows);
        self.term.resize(self.size);
    }

    /// 拉取一帧（会消费并重置 damage）。
    pub fn pull(&mut self) -> &Frame {
        let force_full = self.force_full;
        let refresh_zerowidth = self.refresh_zerowidth;
        self.force_full = false;
        self.refresh_zerowidth = false;
        fill_frame(
            &mut self.term,
            &mut self.frame,
            &self.palette,
            force_full,
            None,
            refresh_zerowidth,
        );
        &self.frame
    }

    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    pub fn cols(&self) -> usize {
        self.size.cols
    }

    /// 回滚历史行数（TERM-002 fuzz 的有界性断言用）。
    ///
    /// `Grid` 没有公开 `history_size`；把显示偏移临时推到顶部再读回，
    /// 不改内容，随后复位到底部。只在测试/诊断路径调用。
    pub fn history_lines(&mut self) -> usize {
        self.term.scroll_display(Scroll::Top);
        let lines = self.term.grid().display_offset();
        self.term.scroll_display(Scroll::Bottom);
        lines
    }

    pub fn rows(&self) -> usize {
        self.size.rows
    }

    /// 视口文本（行尾空格去除），用于断言。
    pub fn viewport_text(&mut self) -> String {
        let f = self.pull();
        let mut out = String::with_capacity(f.rows * (f.cols + 1));
        for row in 0..f.rows {
            let line: String = f.row(row).iter().map(|c| c.char()).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    pub fn contains(&mut self, needle: &str) -> bool {
        self.viewport_text().contains(needle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_lands_in_grid() {
        let mut sim = TermSim::new(20, 5, 100);
        sim.advance(b"hello\r\nworld");
        assert!(sim.contains("hello"));
        assert!(sim.contains("world"));
    }

    #[test]
    fn zero_width_detection_matches_combining_and_zwj() {
        use crate::pane::has_zero_width_chars as has_zw;
        assert!(!has_zw(b"plain ascii\r\n"));
        assert!(!has_zw("中文全角宽字符".as_bytes()), "普通 CJK 不是零宽，不该触发整帧刷新");
        assert!(has_zw("e\u{301}".as_bytes()), "组合音标应触发刷新");
        assert!(has_zw("👨\u{200D}👩".as_bytes()), "ZWJ 应触发刷新");
        assert!(has_zw("❤\u{FE0F}".as_bytes()), "变体选择符应触发刷新");
    }

    #[test]
    fn ascii_does_not_force_full_refresh() {
        let mut sim = TermSim::new(20, 5, 100);
        sim.advance(b"line 1\r\n");
        assert!(!sim.force_full, "纯 ASCII 不应触发零宽整帧刷新");
        assert!(!sim.pull().has_zerowidth(), "纯 ASCII 帧不应带零宽表");
        sim.advance("e\u{301}".as_bytes());
        assert!(sim.force_full, "组合音标必须触发整帧刷新");
        let f = sim.pull();
        assert!(f.has_zerowidth(), "组合音标应进入零宽表");
        assert_eq!(f.zerowidth_at(1, 0), &['\u{301}']);
    }

    #[test]
    fn damage_is_incremental() {
        let mut sim = TermSim::new(40, 10, 100);
        sim.advance(b"first line\r\n");
        let f = sim.pull();
        assert!(f.full, "首帧必须是全量");
        assert_eq!(f.copied_cells, 40 * 10);

        sim.advance(b"x");
        let f = sim.pull();
        assert!(!f.full, "后续帧应按 damage 局部更新");
        assert!(
            f.copied_cells < (40 * 10) as u32,
            "只应拷贝脏行，实际拷贝 {}",
            f.copied_cells
        );
    }

    #[test]
    fn sgr_colors_apply() {
        let mut sim = TermSim::new(20, 3, 10);
        sim.advance(b"\x1b[31mred\x1b[0m");
        let f = sim.pull();
        let cell = f.cell(0, 0);
        assert_eq!(cell.char(), 'r');
        assert_ne!(cell.fg, f.cell(0, 10).fg, "红色与默认前景应不同");
    }

    #[test]
    fn tab_stop_cell_is_blank_not_control_char() {
        // alacritty 把 HT 的起始格记成 '\t'；Frame 必须输出空格，否则渲染层拿
        // 不到该字形（TERM-001：HT 只移动光标，不打印控制字符）。
        let mut sim = TermSim::new(20, 2, 10);
        sim.advance(b"a\tb");
        let f = sim.pull();
        assert_eq!(f.cell(0, 0).char(), 'a');
        assert_eq!(f.cell(0, 1).char(), ' ', "制表位起始格必须是空白");
        assert_eq!(f.cell(0, 7).char(), ' ');
        assert_eq!(f.cell(0, 8).char(), 'b');
        assert_eq!(f.cursor.col, 9);
    }

    #[test]
    fn wide_chars_keep_spacer() {
        let mut sim = TermSim::new(10, 2, 10);
        sim.advance("中文".as_bytes());
        let f = sim.pull();
        assert_eq!(f.cell(0, 0).char(), '中');
        assert!(f.cell(0, 1).has(crate::frame::flags::WIDE_CHAR_SPACER));
    }

    #[test]
    fn combining_marks_stay_in_one_cell() {
        let mut sim = TermSim::new(8, 2, 10);
        sim.advance("e\u{301}x".as_bytes());
        let f = sim.pull();
        // 组合音标零宽，挂在前一个格子，不额外占格；渲染层负责叠在基字形上。
        assert_eq!(f.cell(0, 0).char(), 'e');
        assert_eq!(f.zerowidth_at(0, 0), &['\u{301}']);
        assert_eq!(f.cell(0, 1).char(), 'x');
        assert!(f.zerowidth_at(0, 1).is_empty());
        assert_eq!(f.cursor.col, 2, "e + 组合音标 + x 只应前进 2 格");
    }

    #[test]
    fn combining_mark_arriving_after_pull_is_refreshed() {
        let mut sim = TermSim::new(8, 2, 10);
        sim.advance(b"e");
        let _ = sim.pull();
        // 组合音标单独一批数据到达时，alacritty 不会给该格标 damage；帧刷新必须
        // 仍能把它带到渲染层，否则重音要等到下一次光标移动/整屏重绘才出现。
        sim.advance("\u{301}".as_bytes());
        let f = sim.pull();
        assert_eq!(f.cell(0, 0).char(), 'e');
        assert_eq!(f.zerowidth_at(0, 0), &['\u{301}'], "后到的组合音标丢失");
    }

    #[test]
    fn emoji_and_variation_selectors_use_grid_width() {
        let mut sim = TermSim::new(12, 2, 10);
        sim.advance("😀❤\u{FE0F}!".as_bytes());
        let f = sim.pull();
        use crate::frame::flags;
        // emoji 按 unicode-width 占 2 格（wide + spacer）。
        assert_eq!(f.cell(0, 0).char(), '😀');
        assert!(f.cell(0, 0).has(flags::WIDE_CHAR));
        assert!(f.cell(0, 1).has(flags::WIDE_CHAR_SPACER));
        // VS16 零宽，不额外占格；U+2764 本身是窄字符，仍只占 1 格。
        assert_eq!(f.cell(0, 2).char(), '❤');
        assert_eq!(f.zerowidth_at(0, 2), &['\u{FE0F}']);
        assert!(!f.cell(0, 2).has(flags::WIDE_CHAR));
        assert_eq!(f.cell(0, 3).char(), '!');
        assert_eq!(f.cursor.col, 4);
    }

    #[test]
    fn zwj_sequence_does_not_add_cells_for_joiners() {
        let mut sim = TermSim::new(12, 2, 10);
        sim.advance("👨\u{200D}👩\u{200D}👧X".as_bytes());
        let f = sim.pull();
        use crate::frame::flags;
        // 👨‍👩‍👧 是一个 grapheme cluster：ZWJ 之后的组件并入基准格，整个簇只占
        // 2 格（旧行为：网格按码位计宽，每个 emoji 各占 2 格 → 6 格）。
        assert_eq!(f.cell(0, 0).char(), '👨');
        assert_eq!(f.zerowidth_at(0, 0), &['\u{200D}', '👩', '\u{200D}', '👧']);
        assert!(f.cell(0, 0).has(flags::WIDE_CHAR), "簇仍按宽字符占 2 格");
        assert!(f.cell(0, 1).has(flags::WIDE_CHAR_SPACER));
        assert_eq!(f.cell(0, 2).char(), 'X', "簇之后的下一个字符紧接在第 2 格");
        assert_eq!(f.zerowidth_at(0, 2), &[]);
        assert_eq!(f.cursor.col, 3, "2 格簇 + X；不再按 3×2 格推进");
    }

    #[test]
    fn zwj_cluster_wraps_as_two_cells() {
        // 4 列终端放 2 格簇 + 2 个窄字符，不应因为 3 个 emoji 各占 2 格而折行。
        let mut sim = TermSim::new(4, 2, 10);
        sim.advance("👨\u{200D}👩\u{200D}👧XY".as_bytes());
        let f = sim.pull();
        assert_eq!(f.cell(0, 0).char(), '👨');
        assert_eq!(f.cell(0, 2).char(), 'X');
        assert_eq!(f.cell(0, 3).char(), 'Y');
        assert_eq!(f.cursor.line, 0, "2 格簇没有把后续字符挤到下一行");
        // 第 4 列写满后 alacritty 只置 input_needs_wrap，不越界推进列号。
        assert_eq!(f.cursor.col, 3);
    }

    #[test]
    fn wide_char_at_last_column_wraps_without_half_cell() {
        let mut sim = TermSim::new(4, 2, 10);
        sim.advance("abc中".as_bytes());
        let f = sim.pull();
        use crate::frame::flags;
        // 行尾放不下宽字符时，alacritty 先写 LEADING_WIDE_CHAR_SPACER 再换行；
        // 不允许把宽字符拆成两个半格。
        assert!(f.cell(0, 3).has(0b0000_0100_0000_0000), "行尾应先留 leading spacer");
        assert_eq!(f.cell(1, 0).char(), '中');
        assert!(f.cell(1, 0).has(flags::WIDE_CHAR));
        assert!(f.cell(1, 1).has(flags::WIDE_CHAR_SPACER));
        assert_eq!(f.cursor.line, 1);
        assert_eq!(f.cursor.col, 2);
    }
}
