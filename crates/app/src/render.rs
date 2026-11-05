//! 终端网格 → RGBA 像素缓冲的软件光栅化。
//!
//! Phase 0 目的：把「Slint + Skia 能否以 60 FPS 承载整屏终端帧」这件事量出来。
//! 因此这里刻意做成最朴素、可解释的实现：脏行重绘 + 字形 atlas 混合 + 一次
//! `SharedPixelBuffer` 拷贝进 `Image`。

use std::rc::Rc;

use rusttabby_core::{flags, CellData, CursorShape, Frame, Span};

use crate::glyph::{FontSet, GlyphCache, Style, ATLAS_W};

/// 0x00RRGGBB -> 内存序 RGBA(u32, LE) = r | g<<8 | b<<16 | a<<24
#[inline]
fn pack(c: u32) -> u32 {
    let r = c >> 16 & 0xff;
    let g = c >> 8 & 0xff;
    let b = c & 0xff;
    r | g << 8 | b << 16 | 0xff00_0000
}

/// 轻微 gamma 校正：暗底上的细笔画更实一些（a^(1/1.35)）。
fn gamma_alpha(a: u32) -> u32 {
    use std::sync::OnceLock;
    static LUT: OnceLock<[u8; 256]> = OnceLock::new();
    let lut = LUT.get_or_init(|| {
        let mut lut = [0u8; 256];
        for (i, slot) in lut.iter_mut().enumerate() {
            let x = i as f32 / 255.0;
            *slot = (x.powf(1.0 / 1.35) * 255.0).min(255.0) as u8;
        }
        lut
    });
    lut[(a & 0xff) as usize] as u32
}

#[inline]
fn blend(fg: u32, bg: u32, a: u32) -> u32 {
    let a = gamma_alpha(a);
    if a == 255 {
        return fg;
    }
    let inv = 255 - a;
    let ch = |shift: u32| {
        let f = (fg >> shift) & 0xff;
        let b = (bg >> shift) & 0xff;
        ((f * a + b * inv) / 255) & 0xff
    };
    ch(0) | ch(8) << 8 | ch(16) << 16 | 0xff00_0000
}

pub struct Renderer {
    pub cols: usize,
    pub rows: usize,
    pub cell_w: usize,
    pub cell_h: usize,
    pub width: usize,
    pub height: usize,
    pixels: Vec<u32>,
    fonts: Rc<FontSet>,
    glyphs: GlyphCache,
    default_bg: u32,
    cursor_color: u32,
    force_full: bool,
    last_cursor: (u16, u16),
    last_selection: Option<Span>,
    last_search: Option<Span>,
    /// 统计
    pub last_draw_cells: u32,
    pub last_draw_rows: u32,
    pub full_redraws: u64,
}

impl Renderer {
    pub fn new(
        fonts: Rc<FontSet>,
        cols: usize,
        rows: usize,
        theme_bg: u32,
        cursor_color: u32,
    ) -> Self {
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let width = cols * cell_w;
        let height = rows * cell_h;
        let default_bg = pack(theme_bg);
        Self {
            cols,
            rows,
            cell_w,
            cell_h,
            width,
            height,
            pixels: vec![default_bg; width * height],
            fonts,
            glyphs: GlyphCache::default(),
            default_bg,
            cursor_color: pack(cursor_color),
            force_full: true,
            last_cursor: (0, 0),
            last_selection: None,
            last_search: None,
            last_draw_cells: 0,
            last_draw_rows: 0,
            full_redraws: 0,
        }
    }

    /// 切换主题：更新默认背景与光标色，下一帧整屏重绘。
    pub fn set_theme(&mut self, theme_bg: u32, cursor_color: u32) {
        self.default_bg = pack(theme_bg);
        self.cursor_color = pack(cursor_color);
        self.pixels.fill(self.default_bg);
        self.force_full = true;
        self.last_selection = None;
        self.last_search = None;
    }

    #[allow(dead_code)]
    pub fn font_path(&self) -> &str {
        &self.fonts.font_path
    }

    #[allow(dead_code)]
    pub fn glyph_count(&self) -> usize {
        self.glyphs.cached_count()
    }

    #[allow(dead_code)]
    pub fn metrics(&self) -> (usize, usize) {
        (self.cell_w, self.cell_h)
    }

    pub fn resize_grid(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.width = cols * self.cell_w;
        self.height = rows * self.cell_h;
        self.pixels = vec![self.default_bg; self.width * self.height];
        self.force_full = true;
        self.last_selection = None;
        self.last_search = None;
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let w = w.min(self.width - x);
        for row in y..(y + h).min(self.height) {
            let start = row * self.width + x;
            self.pixels[start..start + w].fill(color);
        }
    }

    fn fill_row(&mut self, row: usize) {
        let start = row * self.cell_h * self.width;
        let end = start + self.cell_h * self.width;
        self.pixels[start..end].fill(self.default_bg);
    }

    /// `span` = 占用的 cell 数（CJK 宽字符为 2）；字形在 span 内居中。
    fn blit(&mut self, x: i32, baseline: i32, ch: char, style: Style, color: u32, span: usize) {
        let Some(g) = self.glyphs.get(&self.fonts, ch, style) else { return };
        let total = (span * self.cell_w) as i32;
        let pad = ((total - g.advance as i32) / 2).max(0);
        let gx = x + pad + g.left as i32;
        let gy = baseline + g.top as i32;
        if gx >= self.width as i32 || gy >= self.height as i32 {
            return;
        }
        let (w, h) = (g.w as usize, g.h as usize);
        let atlas = self.glyphs.atlas();
        for row in 0..h {
            let dy = gy + row as i32;
            if dy < 0 || dy >= self.height as i32 {
                continue;
            }
            let drow = dy as usize * self.width;
            let arow = (g.y as usize + row) * ATLAS_W + g.x as usize;
            for col in 0..w {
                let dx = gx + col as i32;
                if dx < 0 || dx >= self.width as i32 || atlas[arow + col] == 0 {
                    continue;
                }
                let a = atlas[arow + col] as u32;
                let idx = drow + dx as usize;
                self.pixels[idx] = blend(color, self.pixels[idx], a);
            }
        }
    }

    fn style_of(cell: &CellData) -> Style {
        let bold = cell.has(flags::BOLD) || cell.has(flags::DIM);
        let italic = cell.has(flags::ITALIC);
        match (bold, italic) {
            (true, true) => Style::BoldItalic,
            (true, false) => Style::Bold,
            (false, true) => Style::Italic,
            (false, false) => Style::Regular,
        }
    }

    fn draw_row(&mut self, frame: &Frame, row: usize) {
        if row >= self.rows || row >= frame.rows {
            return;
        }
        self.fill_row(row);
        let y0 = row * self.cell_h;
        let baseline = y0 as i32 + self.fonts.baseline;
        let (sel, search) = (frame.selection, frame.search);
        for col in 0..self.cols.min(frame.cols) {
            let cell = frame.cell(row, col);
            // 宽字符的右半格只是占位：背景与字形都由左半格负责，跳过早于填背景，
            // 否则会把已经画好的 CJK 字形右半边盖掉。
            if cell.has(flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let span = if cell.has(flags::WIDE_CHAR) { 2 } else { 1 };
            let cell_px = span * self.cell_w;
            let mut fg = pack(cell.fg);
            let mut bg = pack(cell.bg);
            if cell.has(flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let (r16, c16) = (row as u16, col as u16);
            if sel.is_some_and(|s| s.contains(r16, c16)) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if search.is_some_and(|s| s.contains(r16, c16)) {
                bg = pack(0xf9e2af);
                fg = pack(0x11111b);
            }
            let x0 = col * self.cell_w;
            if bg != self.default_bg {
                self.fill_rect(x0, y0, cell_px, self.cell_h, bg);
            }
            if cell.has(flags::HIDDEN) {
                continue;
            }
            let ch = cell.char();
            if ch != ' ' {
                self.blit(x0 as i32, baseline, ch, Self::style_of(cell), fg, span);
            }
            if cell.has(flags::UNDERLINE_MASK) {
                let uy = y0 + self.cell_h - 2;
                self.fill_rect(x0, uy, cell_px, 1, fg);
                if cell.has(flags::DOUBLE_UNDERLINE) {
                    self.fill_rect(x0, uy.saturating_sub(2), cell_px, 1, fg);
                }
            }
            if cell.has(flags::STRIKEOUT) {
                let sy = y0 + self.cell_h / 2;
                self.fill_rect(x0, sy, cell_px, 1, fg);
            }
        }
        self.last_draw_cells += self.cols as u32;
    }

    fn draw_cursor(&mut self, frame: &Frame) {
        if !frame.cursor.visible || frame.cursor.shape == CursorShape::Hidden {
            return;
        }
        let (row, col) = (frame.cursor.line as usize, frame.cursor.col as usize);
        if row >= self.rows || col >= self.cols {
            return;
        }
        let x0 = col * self.cell_w;
        let y0 = row * self.cell_h;
        let cell = frame.cells.get(row * frame.cols + col);
        let span = if cell.is_some_and(|c| {
            c.has(flags::WIDE_CHAR) || c.has(flags::WIDE_CHAR_SPACER)
        }) {
            2
        } else {
            1
        };
        let cell_px = span * self.cell_w;
        match frame.cursor.shape {
            CursorShape::Block => {
                self.fill_rect(x0, y0, cell_px, self.cell_h, self.cursor_color);
                if let Some(cell) = cell {
                    let ch = cell.char();
                    if ch != ' ' {
                        let baseline = y0 as i32 + self.fonts.baseline;
                        self.blit(x0 as i32, baseline, ch, Style::Regular, self.default_bg, span);
                    }
                }
            },
            CursorShape::Underline => {
                self.fill_rect(x0, y0 + self.cell_h - 2, cell_px, 2, self.cursor_color);
            },
            CursorShape::Beam => {
                self.fill_rect(x0, y0, 2, self.cell_h, self.cursor_color);
            },
            CursorShape::HollowBlock => {
                self.fill_rect(x0, y0, cell_px, 1, self.cursor_color);
                self.fill_rect(x0, y0 + self.cell_h - 1, cell_px, 1, self.cursor_color);
                self.fill_rect(x0, y0, 1, self.cell_h, self.cursor_color);
                self.fill_rect(x0 + cell_px - 1, y0, 1, self.cell_h, self.cursor_color);
            },
            CursorShape::Hidden => {},
        }
    }

    /// 按 damage 增量重绘。
    pub fn draw(&mut self, frame: &Frame) {
        if frame.cols != self.cols || frame.rows != self.rows {
            self.resize_grid(frame.cols, frame.rows);
        }
        self.last_draw_cells = 0;
        self.last_draw_rows = 0;

        let full = frame.full || self.force_full;
        if full {
            self.force_full = false;
            self.full_redraws += 1;
            for row in 0..self.rows {
                self.draw_row(frame, row);
                self.last_draw_rows += 1;
            }
        } else {
            // 行级脏标记：用 cursor 前后位置补齐
            let mut dirty = vec![false; self.rows];
            for d in &frame.damage {
                if (d.line as usize) < self.rows {
                    dirty[d.line as usize] = true;
                }
            }
            if frame.cursor.visible {
                let (old_row, _) = self.last_cursor;
                if (old_row as usize) < self.rows {
                    dirty[old_row as usize] = true;
                }
                if (frame.cursor.line as usize) < self.rows {
                    dirty[frame.cursor.line as usize] = true;
                }
            }
            // 选区/搜索高亮变化：补齐受影响的行
            let marks: [Option<Span>; 4] =
                [self.last_selection, frame.selection, self.last_search, frame.search];
            if self.last_selection != frame.selection || self.last_search != frame.search {
                for span in marks.into_iter().flatten() {
                    for line in span.start_line..=span.end_line {
                        if (line as usize) < self.rows {
                            dirty[line as usize] = true;
                        }
                    }
                }
            }
            for (row, d) in dirty.iter().enumerate() {
                if *d {
                    self.draw_row(frame, row);
                    self.last_draw_rows += 1;
                }
            }
        }
        self.draw_cursor(frame);
        self.last_cursor = (frame.cursor.line, frame.cursor.col);
        self.last_selection = frame.selection;
        self.last_search = frame.search;
    }

    /// 把像素缓冲交给 Slint（一次 4 字节/像素 的拷贝 + 纹理上传）。
    pub fn image(&self) -> slint::Image {
        let mut buffer =
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(self.width as u32, self.height as u32);
        let bytes: &[u8] = bytemuck::cast_slice(&self.pixels);
        buffer.make_mut_bytes().copy_from_slice(bytes);
        slint::Image::from_rgba8(buffer)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use rusttabby_core::Theme;

    fn ink(renderer: &Renderer, x0: usize, x1: usize, y0: usize, y1: usize) -> usize {
        let bg = renderer.default_bg;
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1.min(renderer.width) {
                if renderer.pixels[y * renderer.width + x] != bg {
                    n += 1;
                }
            }
        }
        n
    }

    fn blank_frame(cols: usize, rows: usize) -> Frame {
        let bg = Theme::default().bg();
        let mut f = Frame::new(cols, rows);
        for c in &mut f.cells {
            c.bg = bg;
        }
        f
    }

    #[test]
    fn wide_cjk_is_drawn_across_two_cells() {
        let Some(fonts) = FontSet::load(15.0) else {
            eprintln!("跳过：没有可用字体");
            return;
        };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(Rc::new(fonts), 8, 1, Theme::default().bg(), Theme::default().cursor());
        let mut frame = blank_frame(8, 1);
        frame.full = true;

        let mut wide = CellData::default();
        wide.ch = '你' as u32;
        wide.fg = 0x00ffffff;
        wide.flags = flags::WIDE_CHAR;
        frame.cells[0] = wide;
        let mut spacer = CellData::default();
        spacer.flags = flags::WIDE_CHAR_SPACER;
        frame.cells[1] = spacer;

        renderer.draw(&frame);

        let in_wide = ink(&renderer, 0, 2 * cell_w, 0, cell_h);
        assert!(in_wide > 40, "中文宽字符几乎没画出像素: {in_wide}");
        // 宽字符结束后不能继续溢出到第 3 格
        let after = ink(&renderer, 2 * cell_w, 8 * cell_w, 0, cell_h);
        assert_eq!(after, 0, "宽字符溢出到了后续单元格");

        // 左右两半都应该有笔画（说明 CJK 字形确实横跨两格）
        let left = ink(&renderer, 0, cell_w, 0, cell_h);
        let right = ink(&renderer, cell_w, 2 * cell_w, 0, cell_h);
        assert!(left > 0 && right > 0, "宽字符只画了一格: left={left} right={right}");
    }

    #[test]
    fn ascii_does_not_bleed_outside_cell() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(Rc::new(fonts), 8, 1, Theme::default().bg(), Theme::default().cursor());
        let mut frame = blank_frame(8, 1);
        frame.full = true;
        let mut c = CellData::default();
        c.ch = 'W' as u32;
        c.fg = 0x00ffffff;
        frame.cells[0] = c;
        renderer.draw(&frame);
        assert!(ink(&renderer, 0, cell_w, 0, cell_h) > 10);
        assert_eq!(ink(&renderer, cell_w, 8 * cell_w, 0, cell_h), 0);
    }
}
