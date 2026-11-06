// SPDX-License-Identifier: GPL-3.0-only
//! 终端网格 → RGBA 像素缓冲的软件光栅化。
//!
//! Phase 0 目的：把「Slint + Skia 能否以 60 FPS 承载整屏终端帧」这件事量出来。
//! 现在使用**双缓冲持久帧**：两块 `SharedPixelBuffer` 隔帧交替写入，`Image`
//! 只做 refcount clone，不再每帧整屏 memcpy。每次切换 buffer 整屏重绘（2560x1413
//! 实测约 1ms），比整屏 image 拷贝（约 5ms）便宜，也避免新 buffer 缺上一帧内容。

use std::rc::Rc;

use tervo_core::{flags, CellData, CursorShape, Frame, Span};

use crate::glyph::{FontSet, GlyphCache, Style, ATLAS_W};

/// 0x00RRGGBB -> 内存序 RGBA(u32, LE) = r | g<<8 | b<<16 | a<<24
#[inline]
fn pack(c: u32) -> u32 {
    let r = c >> 16 & 0xff;
    let g = c >> 8 & 0xff;
    let b = c & 0xff;
    r | g << 8 | b << 16 | 0xff00_0000
}

/// 背景不透明度（0.0–1.0）→ 默认背景 alpha（0..=255）。
///
/// 纯函数，便于不启动 GUI 验证。0 = 完全透出背景图，1 = 主题纯色。
/// 非有限值按不透明处理。
pub fn background_alpha(opacity: f32) -> u32 {
    let opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
    (opacity * 255.0).round().clamp(0.0, 255.0) as u32
}

/// 终端默认背景像素：**直通（非预乘）RGBA**，主题 RGB 保持不变、只改 alpha。
///
/// `image()` 用 `Image::from_rgba8` 交给 Slint，由 Slint 统一预乘；这里若先预乘
/// 会被重复乘一次（alpha=0.5 时主题底色只剩 25%，整体发黑）。
/// 文字/光标/彩色 cell 背景在绘制时 alpha 恒为 255，因此始终不透明。
#[inline]
fn default_background(theme_bg: u32, opacity: f32) -> u32 {
    (pack(theme_bg) & 0x00ff_ffff) | background_alpha(opacity) << 24
}

/// 只比较 RGB：默认背景带 opacity alpha，而 cell.bg 是不透明的主题色，
/// 不能因为 alpha 不同就把整屏 cell 背景重新填成不透明。
#[inline]
fn same_rgb(a: u32, b: u32) -> bool {
    a & 0x00ff_ffff == b & 0x00ff_ffff
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

/// 双缓冲帧：交替写入两块 `SharedPixelBuffer`，把“每帧整屏 memcpy 进 Slint
/// Image”降为一次 refcount clone。
///
/// 为什么是两块：Slint 的 `Image` 持有它自己的 `SharedPixelBuffer` 引用，UI 还在
/// 显示上一帧时不能原地改。隔帧使用另一块，上一块的 `Image` 已被下一次属性赋值
/// 释放，`make_mut_bytes` 就能原地写（`make_mut_slice` 在仍共享时会 detach，
/// 这里用 `last_detached` 记录并在 profiling 日志里暴露，绝不假设）。
///
/// 每次切换缓冲都整屏重绘：整屏 draw 在 2560x1413 实测约 1ms，而整屏 image
/// 拷贝约 4.8ms；即使终端只脏一行，整屏重绘仍比全帧拷贝便宜。这样也避免
/// “新缓冲缺少上一帧内容 → 脏行叠加出现花屏”的问题。
struct FrameBuffers {
    buffers: [slint::SharedPixelBuffer<slint::Rgba8Pixel>; 2],
    active: usize,
    /// 最近一次 `make_mut_bytes` 是否发生了 detach（仍被共享）。
    detached: bool,
}

impl FrameBuffers {
    /// 只分配、不填充：每次 `draw()` 都会整屏重绘两块缓冲之一，首帧/换尺寸后
    /// 的空白像素会被完整覆盖。这样 resize 时省掉 2×整屏 `fill` 的写带宽。
    fn new(width: usize, height: usize) -> Self {
        let buffers = [
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width as u32, height as u32),
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width as u32, height as u32),
        ];
        Self { buffers, active: 0, detached: false }
    }

    fn fill_all(&mut self, color: u32) {
        for buffer in &mut self.buffers {
            let pixels: &mut [u32] = bytemuck::cast_slice_mut(buffer.make_mut_bytes());
            pixels.fill(color);
        }
    }

    fn pixels(&self) -> &[u32] {
        bytemuck::cast_slice(self.buffers[self.active].as_bytes())
    }

    fn pixels_mut(&mut self) -> &mut [u32] {
        let before = self.buffers[self.active].as_bytes().as_ptr();
        let bytes = self.buffers[self.active].make_mut_bytes();
        // 一帧里 fill_row/fill_rect/blit 会多次调用；只要有一次 detach 就要
        // 保留这个事实，供 profile 判断双缓冲是否真的省掉了整屏拷贝。
        self.detached |= bytes.as_ptr() != before;
        bytemuck::cast_slice_mut(bytes)
    }

    fn rotate(&mut self) -> bool {
        self.active = (self.active + 1) % 2;
        true
    }

    fn image(&self) -> slint::Image {
        slint::Image::from_rgba8(self.buffers[self.active].clone())
    }
}

pub struct Renderer {
    pub cols: usize,
    pub rows: usize,
    pub cell_w: usize,
    pub cell_h: usize,
    pub width: usize,
    pub height: usize,
    buffer: FrameBuffers,
    fonts: Rc<FontSet>,
    glyphs: GlyphCache,
    default_bg: u32,
    /// 主题原始背景（不透明），用于判断 cell 是否使用默认背景。
    theme_bg: u32,
    /// 当前终端背景不透明度（0.0–1.0），只影响默认背景像素 alpha。
    opacity: f32,
    cursor_color: u32,
    force_full: bool,
    last_cursor: (u16, u16),
    last_selection: Option<Span>,
    last_search: Option<Span>,
    /// 统计
    pub last_draw_cells: u32,
    pub last_draw_rows: u32,
    pub full_redraws: u64,
    /// 最近一次 `draw`/`image` 的耗时（毫秒，仅用于 TERVO_RENDER_PROFILE 日志）。
    pub last_draw_us: f64,
    pub last_image_us: f64,
    /// 最近一次写帧缓冲时是否因为仍被 Slint 共享而 detach（profile 用）。
    pub last_buffer_detached: bool,
}

impl Renderer {
    pub fn new(
        fonts: Rc<FontSet>,
        cols: usize,
        rows: usize,
        theme_bg: u32,
        cursor_color: u32,
        opacity: f32,
    ) -> Self {
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let width = cols * cell_w;
        let height = rows * cell_h;
        let opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
        let default_bg = default_background(theme_bg, opacity);
        Self {
            cols,
            rows,
            cell_w,
            cell_h,
            width,
            height,
            buffer: FrameBuffers::new(width, height),
            fonts,
            glyphs: GlyphCache::default(),
            default_bg,
            theme_bg: pack(theme_bg),
            opacity,
            cursor_color: pack(cursor_color),
            force_full: true,
            last_cursor: (0, 0),
            last_selection: None,
            last_search: None,
            last_draw_cells: 0,
            last_draw_rows: 0,
            full_redraws: 0,
            last_draw_us: 0.0,
            last_image_us: 0.0,
            last_buffer_detached: false,
        }
    }

    /// 切换主题：更新默认背景与光标色，下一帧整屏重绘。
    pub fn set_theme(&mut self, theme_bg: u32, cursor_color: u32) {
        self.theme_bg = pack(theme_bg);
        self.default_bg = default_background(theme_bg, self.opacity);
        self.cursor_color = pack(cursor_color);
        self.buffer.fill_all(self.default_bg);
        self.force_full = true;
        self.last_selection = None;
        self.last_search = None;
    }

    /// 调整终端默认背景 alpha；文字/光标/cell 彩色背景保持不透明。
    pub fn set_opacity(&mut self, opacity: f32) {
        let opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
        self.opacity = opacity;
        self.default_bg = default_background(self.theme_bg, opacity);
        self.buffer.fill_all(self.default_bg);
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

    /// 当前可显示帧的像素（测试/诊断用；返回刚绘制的那块缓冲）。
    #[allow(dead_code)]
    pub fn pixels(&self) -> &[u32] {
        self.buffer.pixels()
    }

    pub fn resize_grid(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.width = cols * self.cell_w;
        self.height = rows * self.cell_h;
        self.buffer = FrameBuffers::new(self.width, self.height);
        self.force_full = true;
        self.last_selection = None;
        self.last_search = None;
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let w = w.min(self.width - x);
        let width = self.width;
        let pixels = self.buffer.pixels_mut();
        for row in y..(y + h).min(self.height) {
            let start = row * width + x;
            pixels[start..start + w].fill(color);
        }
    }

    fn fill_row(&mut self, row: usize) {
        let width = self.width;
        let bg = self.default_bg;
        let start = row * self.cell_h * width;
        let end = start + self.cell_h * width;
        let pixels = self.buffer.pixels_mut();
        pixels[start..end].fill(bg);
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
        let width = self.width;
        let height = self.height;
        let pixels = self.buffer.pixels_mut();
        for row in 0..h {
            let dy = gy + row as i32;
            if dy < 0 || dy >= height as i32 {
                continue;
            }
            let drow = dy as usize * width;
            let arow = (g.y as usize + row) * ATLAS_W + g.x as usize;
            for col in 0..w {
                let dx = gx + col as i32;
                if dx < 0 || dx >= width as i32 || atlas[arow + col] == 0 {
                    continue;
                }
                let a = atlas[arow + col] as u32;
                let idx = drow + dx as usize;
                pixels[idx] = blend(color, pixels[idx], a);
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
            if !same_rgb(bg, self.theme_bg) {
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
        let started = std::time::Instant::now();
        if frame.cols != self.cols || frame.rows != self.rows {
            self.resize_grid(frame.cols, frame.rows);
        }
        // 切换到另一块缓冲；上一块仍可能被 Slint 的 Image 持有，不能原地写。
        // 新缓冲缺少上一帧内容，因此本帧必须整屏重绘（详见 FrameBuffers 注释）。
        let buffer_switched = self.buffer.rotate();
        self.buffer.detached = false;
        self.last_draw_cells = 0;
        self.last_draw_rows = 0;

        // 双缓冲每次切换缓冲都必须整屏重建（见 FrameBuffers）。
        let full = frame.full || self.force_full || buffer_switched;
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
        self.last_draw_us = started.elapsed().as_secs_f64() * 1000.0;
        self.last_buffer_detached = self.buffer.detached;
    }

    /// 把当前双缓冲帧交给 Slint：refcount clone，正常路径无整屏 memcpy。
    ///
    /// 纹理上传仍发生在 Skia 渲染阶段（Slint 1.18 没有安全的持久 GPU texture
    /// API）；`last_image_us` 只量这里交给 Slint 的成本，供 profile 区分
    /// “终端栅格化 / 帧缓冲交接 / Skia 合成”。
    pub fn image(&mut self) -> slint::Image {
        let started = std::time::Instant::now();
        // 只做 refcount clone：像素已经在双缓冲里，Slint 的 Image 与下一帧写入
        // 使用不同的 buffer。正常路径没有整屏 memcpy/清零。
        let image = self.buffer.image();
        self.last_image_us = started.elapsed().as_secs_f64() * 1000.0;
        image
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use tervo_core::Theme;

    fn ink(renderer: &Renderer, x0: usize, x1: usize, y0: usize, y1: usize) -> usize {
        let bg = renderer.default_bg;
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1.min(renderer.width) {
                if renderer.pixels()[y * renderer.width + x] != bg {
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
    fn background_alpha_clamps_and_rounds() {
        assert_eq!(background_alpha(1.0), 255);
        assert_eq!(background_alpha(0.5), 128);
        assert_eq!(background_alpha(0.7), 179);
        assert_eq!(background_alpha(0.2), 51);
        // 0 是合法值：完全透出背景图；越界值夹到边界。
        assert_eq!(background_alpha(0.0), 0);
        assert_eq!(background_alpha(-1.0), 0);
        assert_eq!(background_alpha(1.9), 255);
        assert_eq!(background_alpha(f32::NAN), 255);
    }

    #[test]
    fn default_background_keeps_theme_rgb_and_sets_alpha() {
        let theme_bg = 0x0012_3456;
        let px = default_background(theme_bg, 0.5);
        // 像素是内存序 RGBA（LE），低 24 位与 pack 的 RGB 布局一致。
        // 保持直通 alpha：RGB 不预乘，由 Slint 在合成前统一预乘一次。
        assert_eq!(px & 0x00ff_ffff, pack(theme_bg) & 0x00ff_ffff, "默认背景 RGB 保持主题色");
        assert_eq!(px >> 24, 128);
        assert_eq!(default_background(theme_bg, 0.0) >> 24, 0);
        assert_eq!(default_background(theme_bg, 0.0) & 0x00ff_ffff, pack(theme_bg) & 0x00ff_ffff);
        assert_eq!(default_background(theme_bg, 1.0) >> 24, 255);
        assert_eq!(default_background(0x00ff_ffff, 1.0) & 0x00ff_ffff, 0x00ff_ffff);
    }

    #[test]
    fn drawn_frame_applies_opacity_only_to_default_background() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let theme_bg = Theme::default().bg();
        let mut renderer =
            Renderer::new(Rc::new(fonts), 4, 2, theme_bg, Theme::default().cursor(), 0.5);
        let mut frame = blank_frame(4, 2);
        frame.full = true;
        // row 1 / col 0：非默认 cell 背景应保持不透明。
        frame.cells[4].bg = 0x0011_2233;
        renderer.draw(&frame);

        // 取不在光标 (0,0) 上的默认背景像素。
        let bg_idx = renderer.cell_h * renderer.width + renderer.cell_w * 3;
        assert_eq!(renderer.pixels()[bg_idx] >> 24, 128, "默认背景应带 50% alpha");
        assert_eq!(
            renderer.pixels()[bg_idx] & 0x00ff_ffff,
            pack(theme_bg) & 0x00ff_ffff,
            "默认背景 RGB 不应预乘（否则会被 Slint 再乘一次）"
        );
        // row 1 / col 0 的像素下标（像素缓冲按像素宽度索引，不是按 cell）。
        let idx = renderer.cell_h * renderer.width;
        assert_eq!(renderer.pixels()[idx] >> 24, 255, "彩色 cell 背景不能跟着透明");
        assert_eq!(
            renderer.pixels()[idx] & 0x00ff_ffff,
            pack(0x0011_2233) & 0x00ff_ffff
        );

        renderer.set_opacity(0.2);
        assert_eq!(renderer.default_bg >> 24, 51, "即时调整透明度后默认背景 alpha 更新");
    }

    #[test]
    fn text_stays_opaque_when_default_background_is_transparent() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let theme_bg = Theme::default().bg();
        let mut renderer =
            Renderer::new(Rc::new(fonts), 4, 2, theme_bg, Theme::default().cursor(), 0.0);
        let mut frame = blank_frame(4, 2);
        frame.full = true;
        let mut cell = CellData::default();
        cell.ch = 'A' as u32;
        cell.fg = 0x00ff_ffff;
        cell.bg = theme_bg;
        frame.cells[4 + 1] = cell;
        renderer.draw(&frame);

        // 默认背景 alpha=0，字形像素必须 alpha=255：文字/光标不能跟着背景一起“半透明”。
        let mut opaque = 0;
        let mut transparent = 0;
        for y in renderer.cell_h..renderer.cell_h * 2 {
            for x in renderer.cell_w..renderer.cell_w * 2 {
                let a = renderer.pixels()[y * renderer.width + x] >> 24;
                assert!(a == 0 || a == 255, "文字帧出现半透明像素 alpha={a}");
                if a == 255 {
                    opaque += 1;
                } else {
                    transparent += 1;
                }
            }
        }
        assert!(opaque > 0, "应有不透明的字形像素");
        assert!(transparent > 0, "应有透明的默认背景像素");
        // 不透明字形像素不应被预先压暗：取字形中最亮的白色像素。
        assert!(
            renderer.pixels().iter().any(|p| *p >> 24 == 255 && p & 0x00ff_ffff == 0x00ff_ffff),
            "白色字形应保持原色"
        );
    }

    #[test]
    fn wide_cjk_is_drawn_across_two_cells() {
        let Some(fonts) = FontSet::load(15.0) else {
            eprintln!("跳过：没有可用字体");
            return;
        };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            8,
            1,
            Theme::default().bg(),
            Theme::default().cursor(),
            1.0,
        );
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
    fn dirty_frame_across_buffer_switch_keeps_previous_cells() {
        // 双缓冲每次 draw 会切到另一块 buffer 并整屏重建；这个回归测试保证
        // 第二帧的脏行更新后，上一帧的字形仍在，不会出现花屏/空白。
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            4,
            1,
            Theme::default().bg(),
            Theme::default().cursor(),
            1.0,
        );
        let mut frame = blank_frame(4, 1);
        frame.full = true;
        frame.cells[0].ch = 'A' as u32;
        frame.cells[0].fg = 0x00ff_ffff;
        renderer.draw(&frame);
        assert!(ink(&renderer, 0, cell_w, 0, renderer.cell_h) > 10, "首帧 A 未画出");

        // 第二帧只声明 row 0 脏，并新增 B；A 必须从上一帧保留下来。
        frame.full = false;
        frame.damage = vec![tervo_core::LineDamage { line: 0, left: 0, right: 3 }];
        frame.cells[2].ch = 'B' as u32;
        frame.cells[2].fg = 0x00ff_ffff;
        renderer.draw(&frame);
        assert!(ink(&renderer, 0, cell_w, 0, renderer.cell_h) > 10, "切换 buffer 后 A 丢失");
        assert!(
            ink(&renderer, 2 * cell_w, 3 * cell_w, 0, renderer.cell_h) > 10,
            "切换 buffer 后 B 未画出"
        );
    }

    #[test]
    fn ascii_does_not_bleed_outside_cell() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            8,
            1,
            Theme::default().bg(),
            Theme::default().cursor(),
            1.0,
        );
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
