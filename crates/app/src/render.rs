// SPDX-License-Identifier: GPL-3.0-only
//! 终端网格 → RGBA 像素缓冲的软件光栅化。
//!
//! Phase 0 目的：把「Slint + Skia 能否以 60 FPS 承载整屏终端帧」这件事量出来。
//! 现在使用**双缓冲持久帧**：两块 `SharedPixelBuffer` 隔帧交替写入，`Image`
//! 只做 refcount clone，不再每帧整屏 memcpy。每次切换 buffer 整屏重绘（2560x1413
//! 实测约 1ms），比整屏 image 拷贝（约 5ms）便宜，也避免新 buffer 缺上一帧内容。

use std::rc::Rc;

use tervo_core::{flags, CellData, CursorShape, Frame, Span};

use crate::glyph::{
    is_zero_width_control, placeholder_kind, ColorGlyph, FontSet, Glyph, GlyphCache, PlaceholderKind,
    Style, ATLAS_W,
};

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

/// 彩色 emoji 的直通 alpha 混合。
///
/// 位图 alpha 不是字体 coverage，不能套 `gamma_alpha`（会把彩色图整体压暗/发灰）；
/// 前景色直接取像素 RGB，不再乘终端 fg。结果保持不透明。
#[inline]
fn blend_color(fg: u32, bg: u32) -> u32 {
    let a = fg >> 24 & 0xff;
    if a == 0 {
        return bg;
    }
    if a == 255 {
        return fg;
    }
    let inv = 255 - a;
    let ch = |shift: u32| {
        let f = (fg >> shift) & 0xff;
        let b = (bg >> shift) & 0xff;
        ((f * a + b * inv + 127) / 255) & 0xff
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

/// 光标在 pane 内的像素几何（overlay 与测试共用）。
#[derive(Clone, Copy, Debug, PartialEq)]
struct CursorRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    shape: CursorShape,
}

/// 光标 overlay：光标格大小的小图 + 在 pane 内的像素坐标。
///
/// 终端帧本身不画光标；Slint 把这张图叠在 `pane.image` 之上。闪烁只需切换
/// `cursor-visible`，不需要重绘/重新上传整屏终端帧。
#[derive(Clone, Debug)]
pub struct CursorOverlay {
    pub image: slint::Image,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
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
    /// 主题光标色（0x00RRGGBB packed）；用户覆盖见 `cursor_color_override`。
    cursor_color: u32,
    /// 用户配置的光标形状（覆盖 VTE 的 DECSCUSR；frame 显式 Hidden 仍隐藏）。
    cursor_shape: CursorShape,
    /// 用户配置的光标颜色；None = 主题光标色。
    cursor_color_override: Option<u32>,
    /// 用户配置的光标下文字颜色；None = 主题背景色。
    cursor_text_color_override: Option<u32>,
    /// 帧内光标闪烁相位（`draw` 用；app 的光标 overlay 由 UI timer 控制）。
    cursor_blink_on: bool,
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
            cursor_shape: CursorShape::Block,
            cursor_color_override: None,
            cursor_text_color_override: None,
            cursor_blink_on: true,
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

    /// 下发光标外观（形状/颜色/文字颜色）。返回是否发生变化；颜色为
    /// 0x00RRGGBB，None = 跟随主题。
    pub fn set_cursor_style(
        &mut self,
        shape: CursorShape,
        color: Option<u32>,
        text_color: Option<u32>,
    ) -> bool {
        let color = color.map(pack);
        let text_color = text_color.map(pack);
        let changed = self.cursor_shape != shape
            || self.cursor_color_override != color
            || self.cursor_text_color_override != text_color;
        self.cursor_shape = shape;
        self.cursor_color_override = color;
        self.cursor_text_color_override = text_color;
        changed
    }

    /// 帧内光标闪烁相位。app 使用独立 overlay 时不需要调用；保留给
    /// `draw`/单测验证“闪烁 off 不画光标”。
    #[allow(dead_code)]
    pub fn set_cursor_blink_phase(&mut self, on: bool) {
        self.cursor_blink_on = on;
    }

    /// 实际绘制用的光标形状：frame 显式 Hidden（或不可见）时返回 None。
    ///
    /// 应用通过 DECSCUSR 请求的 underline/beam/hollow 优先（vim 等 TUI 会切
    /// 插入模式光标）；frame 的 Block 无法区分“默认”和“显式 block”，此时用
    /// 用户配置的 `cursor_shape`。
    fn effective_cursor_shape(&self, frame: &Frame) -> Option<CursorShape> {
        if !frame.cursor.visible {
            return None;
        }
        match frame.cursor.shape {
            CursorShape::Hidden => None,
            CursorShape::Block => Some(self.cursor_shape),
            shape => Some(shape),
        }
    }

    #[inline]
    fn cursor_fg(&self) -> u32 {
        self.cursor_color_override.unwrap_or(self.cursor_color)
    }

    #[inline]
    fn cursor_text(&self) -> u32 {
        // 块状光标下的字符用不透明主题背景反色，不能复用可能带 opacity alpha 的
        // default_bg，否则文字会跟着背景一起半透明。
        self.cursor_text_color_override.unwrap_or(self.theme_bg)
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
        let width = self.width;
        let height = self.height;
        Self::fill_rect_buf(self.buffer.pixels_mut(), width, height, x, y, w, h, color);
    }

    /// 在任意 RGBA 像素缓冲上填矩形；`fill_rect` 与光标 overlay 共用。
    fn fill_rect_buf(
        pixels: &mut [u32],
        width: usize,
        height: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        color: u32,
    ) {
        if w == 0 || h == 0 || x >= width || y >= height {
            return;
        }
        let w = w.min(width - x);
        for row in y..(y + h).min(height) {
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

    /// 把一个已定位的字形位图混合进帧缓冲；横向严格裁剪到 `[clip_x0, clip_x1)`。
    ///
    /// 裁剪是“渲染不撑破网格”的最后一道保险：即使字体步进异常、或网格把 emoji
    /// 标成 1 格，字形也只会出现在自己的单元格里。
    fn blit_bitmap(
        &mut self,
        gx: i32,
        gy: i32,
        g: Glyph,
        color: u32,
        clip_x0: i32,
        clip_x1: i32,
    ) {
        let width = self.width;
        let height = self.height;
        let atlas = self.glyphs.atlas();
        let pixels = self.buffer.pixels_mut();
        Self::blit_bitmap_buf(pixels, width, height, atlas, gx, gy, g, color, clip_x0, clip_x1);
    }

    /// 把字形位图混合进任意缓冲；`blit_bitmap` 与光标 overlay 共用。
    #[allow(clippy::too_many_arguments)]
    fn blit_bitmap_buf(
        pixels: &mut [u32],
        width: usize,
        height: usize,
        atlas: &[u8],
        gx: i32,
        gy: i32,
        g: Glyph,
        color: u32,
        clip_x0: i32,
        clip_x1: i32,
    ) {
        if gx >= width as i32 || gy >= height as i32 {
            return;
        }
        let (w, h) = (g.w as usize, g.h as usize);
        for row in 0..h {
            let dy = gy + row as i32;
            if dy < 0 || dy >= height as i32 {
                continue;
            }
            let drow = dy as usize * width;
            let arow = (g.y as usize + row) * ATLAS_W + g.x as usize;
            for col in 0..w {
                let dx = gx + col as i32;
                if dx < clip_x0 || dx >= clip_x1 || dx < 0 || dx >= width as i32 {
                    continue;
                }
                let a = atlas[arow + col] as u32;
                if a == 0 {
                    continue;
                }
                let idx = drow + dx as usize;
                pixels[idx] = blend(color, pixels[idx], a);
            }
        }
    }

    /// 把彩色 emoji 位图混合进帧缓冲；横向裁剪到该字符占的 `span` 格，纵向裁剪
    /// 到本行，保证异常尺寸/损坏字体也不会画穿网格。
    fn blit_color(&mut self, x0: usize, y0: usize, g: &ColorGlyph, span: usize) {
        let (width, height) = (self.width, self.height);
        Self::blit_color_buf(
            self.buffer.pixels_mut(),
            width,
            height,
            x0,
            y0,
            g,
            span,
            self.cell_w,
            self.cell_h,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn blit_color_buf(
        pixels: &mut [u32],
        width: usize,
        height: usize,
        x0: usize,
        y0: usize,
        g: &ColorGlyph,
        span: usize,
        cell_w: usize,
        cell_h: usize,
    ) {
        let clip_x0 = x0 as i32;
        let clip_x1 = x0.saturating_add(span.max(1) * cell_w) as i32;
        let clip_y0 = y0 as i32;
        let clip_y1 = y0.saturating_add(cell_h) as i32;
        let gx = x0 as i32 + g.x_off as i32;
        let gy = y0 as i32 + g.y_off as i32;
        let (gw, gh) = (g.w as usize, g.h as usize);
        let src = g.pixels();
        if src.len() < gw * gh {
            return;
        }
        for row in 0..gh {
            let dy = gy + row as i32;
            if dy < clip_y0 || dy >= clip_y1 || dy < 0 || dy >= height as i32 {
                continue;
            }
            let drow = dy as usize * width;
            for col in 0..gw {
                let dx = gx + col as i32;
                if dx < clip_x0 || dx >= clip_x1 || dx < 0 || dx >= width as i32 {
                    continue;
                }
                let px = src[row * gw + col];
                if px >> 24 == 0 {
                    continue;
                }
                let idx = drow + dx as usize;
                pixels[idx] = blend_color(px, pixels[idx]);
            }
        }
    }

    /// `span` = 占用的 cell 数（CJK 宽字符为 2）；字形在 span 内居中。
    ///
    /// 返回字形墨迹的水平中心，供组合音标对齐；完全画不出来（缺字/只有位图
    /// 无轮廓的字体）时返回 `None`，调用方决定是否画占位框。
    fn blit(
        &mut self,
        x: i32,
        baseline: i32,
        ch: char,
        style: Style,
        color: u32,
        span: usize,
    ) -> Option<i32> {
        let g = self.glyphs.get(&self.fonts, ch, style, span)?;
        let total = (span * self.cell_w) as i32;
        let pad = ((total - g.advance as i32) / 2).max(0);
        let gx = x + pad + g.left as i32;
        let gy = baseline + g.top as i32;
        self.blit_bitmap(gx, gy, g, color, x, x + total);
        Some(gx + g.w as i32 / 2)
    }

    /// 组合音标/ZWJ/变体选择符：零宽，叠在基字形上，不占新格。
    ///
    /// fontdue 没有 GPOS mark attachment，不能直接按字体锚点定位；这里把标记
    /// 位图的水平中心对齐基字形的墨迹中心。ZWJ/VS 等控制符没有可见字形，跳过。
    fn blit_marks(
        &mut self,
        x0: i32,
        baseline: i32,
        center_x: i32,
        marks: &[char],
        style: Style,
        color: u32,
        span: usize,
    ) {
        let total = (span * self.cell_w) as i32;
        for &mark in marks {
            if is_zero_width_control(mark) {
                continue;
            }
            let Some(g) = self.glyphs.get(&self.fonts, mark, style, span) else { continue };
            let gx = center_x - g.w as i32 / 2;
            let gy = baseline + g.top as i32;
            // 组合标记也裁剪在基字形占的 span 内，避免重音压到相邻格。
            self.blit_bitmap(gx, gy, g, color, x0, x0 + total);
        }
    }

    /// 光标 overlay 专用：把字形画进一块小型临时缓冲（坐标以该缓冲为原点）。
    #[allow(clippy::too_many_arguments)]
    fn blit_into(
        glyphs: &mut GlyphCache,
        fonts: &FontSet,
        cell_w: usize,
        pixels: &mut [u32],
        width: usize,
        height: usize,
        x: i32,
        baseline: i32,
        ch: char,
        style: Style,
        color: u32,
        span: usize,
    ) -> Option<i32> {
        let g = glyphs.get(fonts, ch, style, span)?;
        let total = (span * cell_w) as i32;
        let pad = ((total - g.advance as i32) / 2).max(0);
        let gx = x + pad + g.left as i32;
        let gy = baseline + g.top as i32;
        let atlas = glyphs.atlas();
        Self::blit_bitmap_buf(pixels, width, height, atlas, gx, gy, g, color, x, x + total);
        Some(gx + g.w as i32 / 2)
    }

    /// 光标 overlay 专用：把彩色 emoji 画进小型临时缓冲。返回水平中心，供零宽标记对齐。
    #[allow(clippy::too_many_arguments)]
    fn blit_color_into(
        glyphs: &mut GlyphCache,
        fonts: &FontSet,
        cell_w: usize,
        cell_h: usize,
        pixels: &mut [u32],
        width: usize,
        height: usize,
        x0: usize,
        y0: usize,
        ch: char,
        style: Style,
        span: usize,
    ) -> Option<i32> {
        let g = glyphs.get_color(fonts, ch, style, span)?;
        Self::blit_color_buf(pixels, width, height, x0, y0, &g, span, cell_w, cell_h);
        Some(x0 as i32 + g.x_off as i32 + g.w as i32 / 2)
    }

    /// 光标 overlay 专用：组合音标叠加，坐标以 overlay 缓冲为原点。
    #[allow(clippy::too_many_arguments)]
    fn blit_marks_into(
        glyphs: &mut GlyphCache,
        fonts: &FontSet,
        cell_w: usize,
        pixels: &mut [u32],
        width: usize,
        height: usize,
        x0: i32,
        baseline: i32,
        center_x: i32,
        marks: &[char],
        style: Style,
        color: u32,
        span: usize,
    ) {
        let total = (span * cell_w) as i32;
        for &mark in marks {
            if is_zero_width_control(mark) {
                continue;
            }
            let Some(g) = glyphs.get(fonts, mark, style, span) else { continue };
            let gx = center_x - g.w as i32 / 2;
            let gy = baseline + g.top as i32;
            let atlas = glyphs.atlas();
            Self::blit_bitmap_buf(pixels, width, height, atlas, gx, gy, g, color, x0, x0 + total);
        }
    }

    /// 在给定像素缓冲里画光标（block/underline/beam/hollow）。
    ///
    /// `x0`/`y0` 是光标格的左上角；帧内绘制时 `width`/`height` 是整屏尺寸，
    /// overlay 绘制时是 cell 尺寸、`x0=y0=0`。坐标与裁剪逻辑完全共用。
    #[allow(clippy::too_many_arguments)]
    fn paint_cursor(
        glyphs: &mut GlyphCache,
        fonts: &FontSet,
        cell_w: usize,
        cell_h: usize,
        pixels: &mut [u32],
        width: usize,
        height: usize,
        x0: usize,
        y0: usize,
        span: usize,
        shape: CursorShape,
        cell: Option<CellData>,
        marks: &[char],
        color: u32,
        text_color: u32,
    ) {
        let cell_px = span * cell_w;
        let baseline = y0 as i32 + fonts.baseline;
        match shape {
            CursorShape::Block => {
                Self::fill_rect_buf(
                    pixels, width, height, x0, y0, cell_px, cell_h, color,
                );
                if let Some(cell) = cell {
                    let ch = cell.char();
                    if ch != ' ' {
                        let style = Self::style_of(&cell);
                        // 彩色 emoji 优先（与普通渲染一致）；没有彩色字体时再走单色轮廓。
                        let color_center = if placeholder_kind(ch) == Some(PlaceholderKind::Emoji) {
                            Self::blit_color_into(
                                glyphs, fonts, cell_w, cell_h, pixels, width, height, x0, y0, ch,
                                style, span,
                            )
                        } else {
                            None
                        };
                        match color_center.or_else(|| {
                            Self::blit_into(
                                glyphs,
                                fonts,
                                cell_w,
                                pixels,
                                width,
                                height,
                                x0 as i32,
                                baseline,
                                ch,
                                style,
                                text_color,
                                span,
                            )
                        }) {
                            Some(center) if !marks.is_empty() => {
                                Self::blit_marks_into(
                                    glyphs,
                                    fonts,
                                    cell_w,
                                    pixels,
                                    width,
                                    height,
                                    x0 as i32,
                                    baseline,
                                    center,
                                    marks,
                                    style,
                                    text_color,
                                    span,
                                );
                            },
                            Some(_) => {},
                            None => {
                                // 缺字：和普通渲染一样画占位框，保证 block 光标
                                // 不会把“画不出的字符”变成纯色块。
                                if let Some(kind) = placeholder_kind(ch) {
                                    Self::draw_placeholder_buf(
                                        pixels,
                                        width,
                                        height,
                                        cell_h,
                                        x0,
                                        y0,
                                        cell_px,
                                        text_color,
                                        kind,
                                    );
                                }
                            },
                        }
                    }
                }
            },
            CursorShape::Underline => {
                let uy = y0 + cell_h.saturating_sub(2);
                Self::fill_rect_buf(pixels, width, height, x0, uy, cell_px, 2, color);
            },
            CursorShape::Beam => {
                Self::fill_rect_buf(pixels, width, height, x0, y0, 2, cell_h, color);
            },
            CursorShape::HollowBlock => {
                Self::fill_rect_buf(pixels, width, height, x0, y0, cell_px, 1, color);
                Self::fill_rect_buf(
                    pixels,
                    width,
                    height,
                    x0,
                    y0 + cell_h.saturating_sub(1),
                    cell_px,
                    1,
                    color,
                );
                Self::fill_rect_buf(pixels, width, height, x0, y0, 1, cell_h, color);
                Self::fill_rect_buf(
                    pixels,
                    width,
                    height,
                    x0 + cell_px.saturating_sub(1),
                    y0,
                    1,
                    cell_h,
                    color,
                );
            },
            CursorShape::Hidden => {},
        }
    }

    /// 字体链完全画不出来时的明确占位：1px 空框（严格在 span 内）。
    ///
    /// 这不是字体的 .notdef tofu，而是“已识别但当前后端画不了”的主动标记：
    /// emoji 画中心点，Nerd Font/私用区图标画竖条，便于和真实字形区分。
    fn draw_placeholder(
        &mut self,
        x0: usize,
        y0: usize,
        w: usize,
        color: u32,
        kind: PlaceholderKind,
    ) {
        let width = self.width;
        let height = self.height;
        Self::draw_placeholder_buf(
            self.buffer.pixels_mut(),
            width,
            height,
            self.cell_h,
            x0,
            y0,
            w,
            color,
            kind,
        );
    }

    /// 在任意缓冲上画缺字占位框（光标 overlay 也复用）。
    fn draw_placeholder_buf(
        pixels: &mut [u32],
        width: usize,
        height: usize,
        h: usize,
        x0: usize,
        y0: usize,
        w: usize,
        color: u32,
        kind: PlaceholderKind,
    ) {
        if w < 4 || h < 4 {
            Self::fill_rect_buf(pixels, width, height, x0, y0 + h / 2, w, 1, color);
            return;
        }
        // 上下边 + 左右边，缩进 1px 留出格间距。
        Self::fill_rect_buf(pixels, width, height, x0 + 1, y0 + 2, w - 2, 1, color);
        Self::fill_rect_buf(pixels, width, height, x0 + 1, y0 + h - 3, w - 2, 1, color);
        Self::fill_rect_buf(pixels, width, height, x0 + 1, y0 + 2, 1, h - 4, color);
        Self::fill_rect_buf(pixels, width, height, x0 + w - 2, y0 + 2, 1, h - 4, color);
        match kind {
            PlaceholderKind::Emoji => {
                Self::fill_rect_buf(
                    pixels,
                    width,
                    height,
                    x0 + w / 2 - 1,
                    y0 + h / 2 - 1,
                    2,
                    2,
                    color,
                );
            },
            PlaceholderKind::Icon => {
                Self::fill_rect_buf(
                    pixels,
                    width,
                    height,
                    x0 + w / 2,
                    y0 + h / 2 - 3,
                    1,
                    6,
                    color,
                );
                Self::fill_rect_buf(
                    pixels,
                    width,
                    height,
                    x0 + w / 2,
                    y0 + h / 2 - 3,
                    3,
                    1,
                    color,
                );
            },
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
            let style = Self::style_of(cell);
            let marks = if frame.has_zerowidth() {
                frame.zerowidth_at(row, col)
            } else {
                &[][..]
            };
            if ch != ' ' {
                // 彩色 emoji 优先；彩色字体缺失/不支持时回落到单色轮廓，再回落到占位框。
                // 只对 emoji 码位查彩色缓存，ASCII/CJK 不增加字体探测。
                let color = if placeholder_kind(ch) == Some(PlaceholderKind::Emoji) {
                    self.glyphs.get_color(&self.fonts, ch, style, span)
                } else {
                    None
                };
                if let Some(g) = color {
                    self.blit_color(x0, y0, &g, span);
                    if !marks.is_empty() {
                        let center = x0 as i32 + g.x_off as i32 + g.w as i32 / 2;
                        self.blit_marks(x0 as i32, baseline, center, marks, style, fg, span);
                    }
                } else {
                    match self.blit(x0 as i32, baseline, ch, style, fg, span) {
                        Some(center) => {
                            if !marks.is_empty() {
                                self.blit_marks(
                                    x0 as i32, baseline, center, marks, style, fg, span,
                                );
                            }
                        },
                        // 回退链完全画不出来：emoji/PUA 图标画明确占位框，其它缺字留空。
                        None => {
                            if let Some(kind) = placeholder_kind(ch) {
                                self.draw_placeholder(x0, y0, cell_px, fg, kind);
                            }
                        },
                    }
                }
            } else if !marks.is_empty() {
                // 空格上挂了组合音标（行首等边角情况）：居中叠在空格上。
                let center = x0 as i32 + cell_px as i32 / 2;
                self.blit_marks(x0 as i32, baseline, center, marks, style, fg, span);
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

    /// 光标格几何：`(row, col, span)`。不可见/Hidden/越界返回 None。
    fn cursor_geometry(&self, frame: &Frame) -> Option<(usize, usize, usize)> {
        self.effective_cursor_shape(frame)?;
        let (row, col) = (frame.cursor.line as usize, frame.cursor.col as usize);
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let cell = frame.cells.get(row * frame.cols + col);
        let span = if cell.is_some_and(|c| {
            c.has(flags::WIDE_CHAR) || c.has(flags::WIDE_CHAR_SPACER)
        }) {
            2
        } else {
            1
        };
        Some((row, col, span))
    }

    /// 把光标画进终端帧缓冲（`draw` 的兼容路径）。app 使用 `draw_without_cursor`
    /// + [`Renderer::cursor_overlay`]，让闪烁只切换 Slint overlay 的可见性。
    fn draw_cursor(&mut self, frame: &Frame) {
        if !self.cursor_blink_on {
            return;
        }
        let Some(shape) = self.effective_cursor_shape(frame) else { return };
        let Some((row, col, span)) = self.cursor_geometry(frame) else { return };
        let (x0, y0) = (col * self.cell_w, row * self.cell_h);
        let cell = frame.cells.get(row * frame.cols + col).copied();
        let marks = frame.zerowidth_at(row, col);
        let (color, text_color) = (self.cursor_fg(), self.cursor_text());
        let (width, height) = (self.width, self.height);
        let pixels = self.buffer.pixels_mut();
        Self::paint_cursor(
            &mut self.glyphs,
            &self.fonts,
            self.cell_w,
            self.cell_h,
            pixels,
            width,
            height,
            x0,
            y0,
            span,
            shape,
            cell,
            marks,
            color,
            text_color,
        );
    }

    /// 生成光标 overlay 的像素缓冲与几何；测试直接检查像素，`cursor_overlay`
    /// 再包成 Slint Image。
    fn cursor_buffer(
        &mut self,
        frame: &Frame,
    ) -> Option<(slint::SharedPixelBuffer<slint::Rgba8Pixel>, CursorRect)> {
        let shape = self.effective_cursor_shape(frame)?;
        let (row, col, span) = self.cursor_geometry(frame)?;
        let width = span * self.cell_w;
        let height = self.cell_h;
        let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(
            width as u32,
            height as u32,
        );
        {
            let pixels: &mut [u32] = bytemuck::cast_slice_mut(buffer.make_mut_bytes());
            pixels.fill(0); // 透明：Slint 直接按 alpha 叠在终端帧上
            let cell = frame.cells.get(row * frame.cols + col).copied();
            let marks = frame.zerowidth_at(row, col);
            let (color, text_color) = (self.cursor_fg(), self.cursor_text());
            Self::paint_cursor(
                &mut self.glyphs,
                &self.fonts,
                self.cell_w,
                self.cell_h,
                pixels,
                width,
                height,
                0,
                0,
                span,
                shape,
                cell,
                marks,
                color,
                text_color,
            );
        }
        Some((
            buffer,
            CursorRect {
                x: (col * self.cell_w) as f32,
                y: (row * self.cell_h) as f32,
                width: width as f32,
                height: height as f32,
                shape,
            },
        ))
    }

    /// 生成光标 overlay：只含光标所在格的小图（block 为整格，underline/beam
    /// 其余像素透明）。闪烁由 UI 计时器切换 `visible`，不重绘终端帧。
    pub fn cursor_overlay(&mut self, frame: &Frame) -> Option<CursorOverlay> {
        let (buffer, rect) = self.cursor_buffer(frame)?;
        Some(CursorOverlay {
            image: slint::Image::from_rgba8(buffer),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        })
    }

    /// 按 damage 增量重绘（带帧内光标）。app 走 `draw_without_cursor` + overlay；
    /// 这个入口保留给无 overlay 的调用方/测试。
    #[allow(dead_code)]
    pub fn draw(&mut self, frame: &Frame) {
        self.draw_impl(frame, true);
    }

    /// 只画终端内容、不画光标；app 用独立的 Slint overlay 画光标，这样光标
    /// 闪烁/移动不需要重新栅格化整屏，GPU 后端也只合成一个小图层。
    pub fn draw_without_cursor(&mut self, frame: &Frame) {
        self.draw_impl(frame, false);
    }

    fn draw_impl(&mut self, frame: &Frame, draw_cursor: bool) {
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
        if draw_cursor {
            self.draw_cursor(frame);
        }
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

    /// 彩色像素数：排除背景与灰阶文字，只统计 RGB 通道明显不同的像素。
    fn color_ink(renderer: &Renderer, x0: usize, x1: usize, y0: usize, y1: usize) -> usize {
        let bg = renderer.default_bg;
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1.min(renderer.width) {
                let p = renderer.pixels()[y * renderer.width + x];
                if p == bg {
                    continue;
                }
                let (r, g, b) = (p & 0xff, (p >> 8) & 0xff, (p >> 16) & 0xff);
                if r.abs_diff(g) > 8 || g.abs_diff(b) > 8 || r.abs_diff(b) > 8 {
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
    fn combining_mark_is_overlaid_inside_base_cell() {
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
        let mut cell = CellData::default();
        cell.ch = 'x' as u32;
        cell.fg = 0x00ffffff;
        cell.bg = Theme::default().bg();

        let mut frame = blank_frame(8, 1);
        frame.full = true;
        frame.put_zerowidth(0, cell, None, &['\u{301}']);
        renderer.draw(&frame);
        let with_mark = ink(&renderer, 0, cell_w, 0, cell_h);
        assert!(with_mark > 0, "基字形没画出来");
        assert_eq!(
            ink(&renderer, cell_w, 8 * cell_w, 0, cell_h),
            0,
            "组合音标溢出到了下一格"
        );

        // 对照：没有组合音标时墨水量应更少，证明重音真的叠上去了。
        let mut plain = blank_frame(8, 1);
        plain.full = true;
        plain.cells[0] = cell;
        renderer.draw(&plain);
        let without = ink(&renderer, 0, cell_w, 0, cell_h);
        assert!(without > 0);
        assert!(
            with_mark > without,
            "组合音标没有叠加: with={with_mark} without={without}"
        );
    }

    #[test]
    fn missing_private_use_icon_draws_placeholder_inside_cell() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            4,
            1,
            Theme::default().bg(),
            Theme::default().cursor(),
            1.0,
        );
        // 从常见 Nerd Font 图标码位里挑一个当前字体链确实画不出来的。
        let mut missing = None;
        for ch in ['\u{F121}', '\u{E709}', '\u{E796}', '\u{F000}'] {
            if renderer.glyphs.get(&renderer.fonts, ch, Style::Regular, 1).is_none() {
                missing = Some(ch);
                break;
            }
        }
        let Some(ch) = missing else {
            eprintln!("跳过：系统已安装覆盖这些图标的 Nerd Font");
            return;
        };
        let mut cell = CellData::default();
        cell.ch = ch as u32;
        cell.fg = 0x00ffffff;
        cell.bg = Theme::default().bg();
        let mut frame = blank_frame(4, 1);
        frame.full = true;
        frame.put(0, cell, None);
        renderer.draw(&frame);

        assert!(
            ink(&renderer, 0, cell_w, 0, cell_h) > 0,
            "缺失的 Nerd 图标应画明确占位框，而不是空白"
        );
        assert_eq!(
            ink(&renderer, cell_w, 4 * cell_w, 0, cell_h),
            0,
            "占位框溢出到相邻格"
        );
    }

    #[test]
    fn zwj_family_keeps_every_component_inside_grid() {
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
        for (col, ch) in [(0usize, '👨'), (2, '👩'), (4, '👧')] {
            let mut wide = CellData::default();
            wide.ch = ch as u32;
            wide.fg = 0x00ffffff;
            wide.bg = Theme::default().bg();
            wide.flags = flags::WIDE_CHAR;
            // ZWJ 零宽，挂在每个 emoji 的格子上，不额外占格。
            frame.put_zerowidth(col, wide, None, &['\u{200D}']);
            let mut spacer = CellData::default();
            spacer.flags = flags::WIDE_CHAR_SPACER;
            frame.put(col + 1, spacer, None);
        }
        renderer.draw(&frame);

        // 3 个 emoji 网格 = 6 格；无论回退到单色字体还是占位框，都必须有可见内容，
        // 且不能越过第 6 格。
        assert!(ink(&renderer, 0, 6 * cell_w, 0, cell_h) > 0, "ZWJ 家族没有可见字形");
        assert_eq!(ink(&renderer, 6 * cell_w, 8 * cell_w, 0, cell_h), 0, "ZWJ 渲染撑破网格");
    }

    #[test]
    fn emoji_forced_narrow_does_not_bleed_into_neighbor() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            4,
            1,
            Theme::default().bg(),
            Theme::default().cursor(),
            1.0,
        );
        // 人为把 emoji 放进 1 格（正常网格不会这样，但异常/损坏的帧不能画穿网格）：
        // 要么缩到 cell_w，要么画占位框，都不能溢到第 2 格。
        let mut cell = CellData::default();
        cell.ch = '😀' as u32;
        cell.fg = 0x00ffffff;
        cell.bg = Theme::default().bg();
        let mut frame = blank_frame(4, 1);
        frame.full = true;
        frame.put(0, cell, None);
        renderer.draw(&frame);
        assert!(ink(&renderer, 0, cell_w, 0, cell_h) > 0);
        assert_eq!(ink(&renderer, cell_w, 4 * cell_w, 0, cell_h), 0, "1 格 emoji 撑破网格");
    }

    #[test]
    fn color_emoji_draws_rgb_inside_wide_cell() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            4,
            1,
            Theme::default().bg(),
            Theme::default().cursor(),
            1.0,
        );
        if renderer.glyphs.get_color(&renderer.fonts, '😀', Style::Regular, 2).is_none() {
            eprintln!("跳过：系统没有彩色 emoji 位图");
            return;
        }
        let mut wide = CellData::default();
        wide.ch = '😀' as u32;
        wide.fg = 0x00ffffff;
        wide.bg = Theme::default().bg();
        wide.flags = flags::WIDE_CHAR;
        let mut spacer = CellData::default();
        spacer.flags = flags::WIDE_CHAR_SPACER;
        let mut frame = blank_frame(4, 1);
        frame.full = true;
        frame.put(0, wide, None);
        frame.put(1, spacer, None);
        renderer.draw(&frame);

        assert!(color_ink(&renderer, 0, 2 * cell_w, 0, cell_h) > 10, "😀 没有画出彩色像素");
        assert_eq!(
            ink(&renderer, 2 * cell_w, 4 * cell_w, 0, cell_h),
            0,
            "彩色 emoji 溢出到相邻格"
        );
    }

    #[test]
    fn vs16_heart_color_stays_in_one_cell() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            4,
            1,
            Theme::default().bg(),
            Theme::default().cursor(),
            1.0,
        );
        if renderer.glyphs.get_color(&renderer.fonts, '❤', Style::Regular, 1).is_none() {
            eprintln!("跳过：系统没有 ❤ 彩色位图");
            return;
        }
        // ❤ 本身是窄字符，VS16 零宽挂在前一格：网格仍是 1 格。
        let mut cell = CellData::default();
        cell.ch = '❤' as u32;
        cell.fg = 0x00ffffff;
        cell.bg = Theme::default().bg();
        let mut frame = blank_frame(4, 1);
        frame.full = true;
        frame.put_zerowidth(0, cell, None, &['\u{FE0F}']);
        renderer.draw(&frame);

        assert!(color_ink(&renderer, 0, cell_w, 0, cell_h) > 0, "❤+VS16 没有彩色像素");
        assert_eq!(ink(&renderer, cell_w, 4 * cell_w, 0, cell_h), 0, "❤+VS16 溢出单格");
    }

    #[test]
    fn zwj_family_color_components_stay_inside_grid() {
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
        // 彩色字体可能只覆盖部分组件；至少一个能画就验证网格，否则跳过。
        let supported = ['👨', '👩', '👧']
            .iter()
            .any(|ch| renderer.glyphs.get_color(&renderer.fonts, *ch, Style::Regular, 2).is_some());
        if !supported {
            eprintln!("跳过：系统没有 ZWJ 家族组件的彩色位图");
            return;
        }
        let mut frame = blank_frame(8, 1);
        frame.full = true;
        for (col, ch) in [(0usize, '👨'), (2, '👩'), (4, '👧')] {
            let mut wide = CellData::default();
            wide.ch = ch as u32;
            wide.fg = 0x00ffffff;
            wide.bg = Theme::default().bg();
            wide.flags = flags::WIDE_CHAR;
            // ZWJ 零宽，挂在每个 emoji 上；彩色渲染逐个组件顺序覆盖，不额外占格。
            frame.put_zerowidth(col, wide, None, &['\u{200D}']);
            let mut spacer = CellData::default();
            spacer.flags = flags::WIDE_CHAR_SPACER;
            frame.put(col + 1, spacer, None);
        }
        renderer.draw(&frame);

        assert!(
            color_ink(&renderer, 0, 6 * cell_w, 0, cell_h) > 0,
            "ZWJ 家族没有彩色组件"
        );
        assert_eq!(ink(&renderer, 6 * cell_w, 8 * cell_w, 0, cell_h), 0, "ZWJ 彩色渲染撑破网格");
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

    /// 建一个光标可见的帧，光标放在 (line,col)，该格保持空格。
    fn cursor_frame(cols: usize, rows: usize, line: u16, col: u16) -> Frame {
        let mut frame = blank_frame(cols, rows);
        frame.full = true;
        frame.cursor = tervo_core::CursorState {
            line,
            col,
            shape: CursorShape::Block,
            visible: true,
        };
        frame
    }

    fn pixel(renderer: &Renderer, x: usize, y: usize) -> u32 {
        renderer.pixels()[y * renderer.width + x]
    }

    #[test]
    fn cursor_shapes_paint_expected_regions() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let cursor = 0x00ff_0000;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            4,
            2,
            Theme::default().bg(),
            cursor,
            1.0,
        );
        let frame = cursor_frame(4, 2, 1, 1);
        let (x, y) = (cell_w, cell_h);
        let red = pack(cursor);

        // 默认 block：整格填充。
        renderer.draw(&frame);
        assert_eq!(pixel(&renderer, x, y), red, "block 左上未填");
        assert_eq!(pixel(&renderer, x + cell_w - 1, y + cell_h - 1), red, "block 右下未填");

        // underline：只画底部 2px。
        renderer.set_cursor_style(CursorShape::Underline, None, None);
        renderer.draw(&frame);
        assert_eq!(pixel(&renderer, x, y + cell_h - 1), red, "underline 底线未画");
        assert_eq!(pixel(&renderer, x, y + cell_h - 2), red, "underline 第二行未画");
        assert_ne!(pixel(&renderer, x, y), red, "underline 不应填满整格");

        // ibeam：只画左侧 2px。
        renderer.set_cursor_style(CursorShape::Beam, None, None);
        renderer.draw(&frame);
        assert_eq!(pixel(&renderer, x, y), red, "ibeam 左列未画");
        assert_eq!(pixel(&renderer, x + 1, y + cell_h - 1), red, "ibeam 第二列未画");
        assert_ne!(pixel(&renderer, x + 2, y), red, "ibeam 不应填满整格");
    }

    #[test]
    fn cursor_color_and_text_color_overrides_apply() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let mut renderer = Renderer::new(
            Rc::new(fonts),
            4,
            1,
            Theme::default().bg(),
            0x00ff_0000,
            1.0,
        );
        let mut frame = cursor_frame(4, 1, 0, 0);
        let mut cell = CellData::default();
        cell.ch = 'A' as u32;
        cell.fg = 0x00ff_ffff;
        cell.bg = Theme::default().bg();
        frame.cells[0] = cell;
        let cursor = 0x0000_ff00;
        let text = 0x0000_00ff;
        renderer.set_cursor_style(CursorShape::Block, Some(cursor), Some(text));
        renderer.draw(&frame);
        let count = |color: u32| {
            (0..cell_w * cell_h)
                .filter(|i| pixel(&renderer, i % cell_w, i / cell_w) == color)
                .count()
        };
        assert!(count(pack(cursor)) > 20, "自定义光标色未生效");
        assert!(count(pack(text)) > 0, "光标下文字颜色未生效");
        assert_eq!(count(pack(0x00ff_0000)), 0, "主题光标色不应残留");
    }

    #[test]
    fn app_requested_cursor_shape_wins_over_config_block_default() {
        // vim 等 TUI 用 DECSCUSR 切 beam/underline；frame 的形状应优先于配置。
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_h = fonts.cell_h;
        let cursor = 0x00ff_0000;
        let mut renderer =
            Renderer::new(Rc::new(fonts), 4, 1, Theme::default().bg(), cursor, 1.0);
        renderer.set_cursor_style(CursorShape::Beam, None, None);
        let mut frame = cursor_frame(4, 1, 0, 0);
        frame.cursor.shape = CursorShape::Underline;
        renderer.draw(&frame);
        let red = pack(cursor);
        assert_eq!(pixel(&renderer, 0, cell_h - 1), red, "frame 的 underline 未生效");
        assert_ne!(pixel(&renderer, 0, 0), red, "不应按配置画 beam");
    }

    #[test]
    fn cursor_blink_off_does_not_draw_cursor_in_frame() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cursor = 0x00ff_0000;
        let mut renderer =
            Renderer::new(Rc::new(fonts), 4, 1, Theme::default().bg(), cursor, 1.0);
        let frame = cursor_frame(4, 1, 0, 1);
        renderer.set_cursor_blink_phase(false);
        renderer.draw(&frame);
        assert!(
            !renderer.pixels().iter().any(|p| *p == pack(cursor)),
            "闪烁 off 时帧缓冲里仍有光标像素"
        );
        renderer.set_cursor_blink_phase(true);
        renderer.draw(&frame);
        assert!(
            renderer.pixels().iter().any(|p| *p == pack(cursor)),
            "闪烁 on 时未画光标"
        );
    }

    #[test]
    fn draw_without_cursor_keeps_frame_clean_but_overlay_works() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let cursor = 0x00ff_0000;
        let mut renderer =
            Renderer::new(Rc::new(fonts), 4, 2, Theme::default().bg(), cursor, 1.0);
        let frame = cursor_frame(4, 2, 1, 2);
        renderer.draw_without_cursor(&frame);
        assert!(
            !renderer.pixels().iter().any(|p| *p == pack(cursor)),
            "终端帧不应包含光标"
        );
        let (buffer, rect) = renderer.cursor_buffer(&frame).expect("可见光标应有 overlay");
        assert_eq!((rect.x, rect.y), ((2 * cell_w) as f32, cell_h as f32));
        assert_eq!((rect.width, rect.height), (cell_w as f32, cell_h as f32));
        let pixels: &[u32] = bytemuck::cast_slice(buffer.as_bytes());
        let opaque = pixels.iter().filter(|p| *p >> 24 == 255).count();
        assert_eq!(opaque, pixels.len(), "block overlay 应整格不透明");
    }

    #[test]
    fn cursor_overlay_underline_and_beam_are_transparent_outside_line() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let cell_w = fonts.cell_w;
        let cell_h = fonts.cell_h;
        let cursor = 0x00ff_0000;
        let mut renderer =
            Renderer::new(Rc::new(fonts), 4, 1, Theme::default().bg(), cursor, 1.0);
        let frame = cursor_frame(4, 1, 0, 0);
        let red = pack(cursor);
        let opaque = |buffer: &slint::SharedPixelBuffer<slint::Rgba8Pixel>| {
            let pixels: &[u32] = bytemuck::cast_slice(buffer.as_bytes());
            let rows: Vec<usize> = (0..cell_h)
                .filter(|y| (0..cell_w).any(|x| pixels[y * cell_w + x] == red))
                .collect();
            (pixels.iter().filter(|p| *p >> 24 != 0).count(), rows)
        };
        renderer.set_cursor_style(CursorShape::Underline, None, None);
        let (underline, _) = renderer.cursor_buffer(&frame).unwrap();
        let (n, rows) = opaque(&underline);
        assert_eq!(n, cell_w * 2, "underline overlay 只应有两行不透明");
        assert_eq!(rows, vec![cell_h - 2, cell_h - 1], "underline 位置不对");

        renderer.set_cursor_style(CursorShape::Beam, None, None);
        let (beam, _) = renderer.cursor_buffer(&frame).unwrap();
        let (n, _) = opaque(&beam);
        assert_eq!(n, cell_h * 2, "beam overlay 只应有两列不透明");
    }
}
