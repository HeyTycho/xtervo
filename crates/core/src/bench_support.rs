//! 无 PTY 的终端仿真夹具：用于解析吞吐基准与单元测试。
//!
//! 生产路径不经过这里——真实 pane 永远走 actor 线程。它只是把
//! `Term + Processor + fill_frame` 单独拿出来，便于隔离测量解析与取帧成本。

use alacritty_terminal::event::VoidListener;
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
        }
    }

    #[inline]
    pub fn advance(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.size = GridSize::new(cols, rows);
        self.term.resize(self.size);
    }

    /// 拉取一帧（会消费并重置 damage）。
    pub fn pull(&mut self) -> &Frame {
        fill_frame(&mut self.term, &mut self.frame, &self.palette, false, None);
        &self.frame
    }

    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    pub fn cols(&self) -> usize {
        self.size.cols
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
    fn wide_chars_keep_spacer() {
        let mut sim = TermSim::new(10, 2, 10);
        sim.advance("中文".as_bytes());
        let f = sim.pull();
        assert_eq!(f.cell(0, 0).char(), '中');
        assert!(f.cell(0, 1).has(crate::frame::flags::WIDE_CHAR_SPACER));
    }
}
