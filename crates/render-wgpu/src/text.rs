//! Instanced WGPU text rendering from [`xtervo_scene::TextBatch`].
//!
//! Pipeline shape:
//!
//! ```text
//! TextBatch cells
//!   -> per-cell background/underline solid instances
//!   -> per-ASCII-glyph instances (atlas UV + foreground color)
//!   -> one render pass, two instanced draw calls
//!   -> offscreen Rgba8Unorm target (no CPU terminal framebuffer)
//! ```
//!
//! Glyph rasterization happens only on an atlas miss. Reused glyphs cost no
//! upload. Instance buffers are grown only when the frame needs more instances;
//! they are not recreated every frame.
//!
//! Scope of this prototype: ASCII glyphs, foreground/background colors, bold /
//! italic faces when matching font files are installed, underline, and the
//! already-resolved `selected` colors. Non-ASCII characters deterministically
//! fall back to `?` and are counted in [`FrameStats::unknown_glyphs`]. Images,
//! cursor and z-order composition are intentionally out of scope.

use std::fmt;
use std::time::{Duration, Instant};

use xtervo_scene::{PhysicalSize, TextBatch};

use crate::atlas::{
    discover_font_path, AtlasConfig, AtlasError, AtlasStats, FontMetrics, FontStyle, GlyphAtlas,
    GlyphSlot,
};
use crate::{BackendError, FRAME_FORMAT, FRAME_USAGE};

/// Vertex data for a solid rectangle (cell background or underline).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SolidInstance {
    /// `[x, y, width, height]` in physical pixels.
    pub rect: [f32; 4],
    /// Straight-alpha color.
    pub color: [f32; 4],
}

/// Vertex data for one glyph quad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphInstance {
    /// `[x, y, width, height]` in physical pixels.
    pub rect: [f32; 4],
    /// Atlas rectangle as `[u0, v0, u1, v1]`.
    pub uv: [f32; 4],
    /// Straight-alpha foreground color.
    pub color: [f32; 4],
}

impl SolidInstance {
    const BYTES: u64 = 32;
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: Self::BYTES,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
    };

    fn append_bytes(self, out: &mut Vec<u8>) {
        for value in self.rect.into_iter().chain(self.color) {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

impl GlyphInstance {
    const BYTES: u64 = 48;
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: Self::BYTES,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![
            0 => Float32x4,
            1 => Float32x4,
            2 => Float32x4,
        ],
    };

    fn append_bytes(self, out: &mut Vec<u8>) {
        for value in self.rect.into_iter().chain(self.uv).chain(self.color) {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// Cell grid and font size for one [`xtervo_scene::TextBatch`].
///
/// Positions are rounded to whole pixels before rasterization. That makes a
/// glyph quad cover exact coverage texels and lets the CUDA-free CPU reference
/// compositor compare byte-for-byte instead of fighting fractional geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextLayout {
    /// Left edge of the cell grid in physical pixels.
    pub origin_x: f32,
    /// Top edge of the cell grid in physical pixels.
    pub origin_y: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    /// Glyph rasterization size in pixels.
    pub font_size_px: u16,
    /// Distance from a cell's top edge to the text baseline.
    pub baseline: f32,
    /// Theme/default cell background (`0x00RRGGBB`).
    ///
    /// Cells whose resolved background matches this are left to the clear
    /// color, exactly like the CPU renderer's `same_rgb(bg, theme_bg)` rule.
    /// `None` paints every cell background.
    pub default_background: Option<u32>,
}

impl TextLayout {
    /// Builds a layout from fontdue-derived [`FontMetrics`].
    pub fn from_metrics(
        metrics: FontMetrics,
        origin_x: f32,
        origin_y: f32,
        font_size_px: u16,
    ) -> Self {
        Self {
            origin_x,
            origin_y,
            cell_width: metrics.cell_width,
            cell_height: metrics.cell_height,
            font_size_px,
            baseline: metrics.baseline,
            default_background: None,
        }
    }

    /// Leaves cells with this background color to the target clear color.
    pub fn with_default_background(mut self, color: Option<u32>) -> Self {
        self.default_background = color;
        self
    }

    /// Same layout translated by `(dx, dy)`.
    pub fn translated(self, dx: f32, dy: f32) -> Self {
        Self {
            origin_x: self.origin_x + dx,
            origin_y: self.origin_y + dy,
            ..self
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.font_size_px == 0 {
            return Err("font_size_px must be non-zero");
        }
        for (name, value) in [
            ("origin_x", self.origin_x),
            ("origin_y", self.origin_y),
            ("cell_width", self.cell_width),
            ("cell_height", self.cell_height),
            ("baseline", self.baseline),
        ] {
            if !value.is_finite() {
                let reason = match name {
                    "origin_x" => "origin_x must be finite",
                    "origin_y" => "origin_y must be finite",
                    "cell_width" => "cell_width must be finite",
                    "cell_height" => "cell_height must be finite",
                    _ => "baseline must be finite",
                };
                return Err(reason);
            }
        }
        if self.cell_width <= 0.0 {
            return Err("cell_width must be positive");
        }
        if self.cell_height <= 0.0 {
            return Err("cell_height must be positive");
        }
        Ok(())
    }
}

/// One text batch plus the layout it should be drawn with.
#[derive(Clone, Copy, Debug)]
pub struct TextSection<'a> {
    pub batch: &'a TextBatch,
    pub layout: TextLayout,
}

impl<'a> TextSection<'a> {
    pub fn new(batch: &'a TextBatch, layout: TextLayout) -> Self {
        Self { batch, layout }
    }
}

/// Per-frame counters. Byte counts are deltas for this frame unless the field
/// says `*_total`.
#[derive(Clone, Debug)]
pub struct FrameStats {
    pub frame: u64,
    pub width: u32,
    pub height: u32,
    pub cells: usize,
    pub solid_quads: usize,
    pub glyph_quads: usize,
    pub render_passes: u32,
    /// Actual draw calls: solid quads (if any) + glyph quads (if any).
    pub draw_calls: u32,
    pub atlas_hits: u64,
    pub atlas_misses: u64,
    pub atlas_rasterized: u64,
    /// `queue.write_texture` calls for new glyphs this frame.
    pub atlas_uploads: u64,
    /// Glyph bytes uploaded this frame; never a full-screen framebuffer.
    pub atlas_uploaded_bytes: u64,
    /// Instance bytes uploaded this frame (proportional to cells, not pixels).
    pub vertex_uploaded_bytes: u64,
    /// Atlas texture memory.
    pub atlas_gpu_bytes: u64,
    /// Offscreen target texture memory (not uploaded from CPU).
    pub target_gpu_bytes: u64,
    pub atlas_cached_glyphs: usize,
    /// Cumulative instance-buffer growth events.
    pub buffer_reallocations: u64,
    /// Non-ASCII characters with no font coverage, replaced by an explicit
    /// `?` marker (never silently dropped or mis-shaped).
    pub unknown_glyphs: u64,
    /// Glyphs rendered from a CJK/symbol/emoji fallback font.
    pub fallback_glyphs: u64,
    /// Characters no loaded font covers (subset of `unknown_glyphs`).
    pub missing_glyphs: u64,
    /// Bold/italic glyphs rendered with the regular face because the variant
    /// font file is absent.
    pub style_fallbacks: u64,
    /// Non-whitespace characters whose fontdue bitmap was empty.
    pub missing_outlines: u64,
    /// Control characters skipped by the cell layout.
    pub skipped_control: u64,
    /// Wide cells (2 grid columns) and spacer cells that were not drawn twice.
    pub wide_cells: u64,
    pub wide_spacer_cells: u64,
    /// SGR hidden cells: background painted, glyph omitted.
    pub hidden_cells: u64,
    /// Faint/dim cells rendered with the bold face.
    pub dim_cells: u64,
    /// Selection/search spans resolved into cell colors by the Scene builder.
    pub selected_cells: u64,
    pub searched_cells: u64,
    /// Hollow fallback boxes drawn when even the `?` marker is unavailable.
    pub placeholder_quads: u64,
    /// Source Scene provenance (only set by [`GpuTextRenderer::render_scene`]).
    pub scene_revision: u64,
    /// Images/cursor the Scene contained but the text slice did not composite.
    /// The app keeps its existing Slint image/cursor overlays for those.
    pub scene_images_skipped: u32,
    pub scene_cursor_skipped: bool,
    /// The clear color came from the Scene's background layer.
    pub background_from_scene: bool,
    /// CPU time spent building instances, uploading and submitting the frame.
    /// This excludes GPU execution (no timestamp queries in this prototype).
    pub cpu_time: Duration,
}

/// Failures that prevent a text frame from being submitted.
#[derive(Debug)]
pub enum TextRenderError {
    Atlas(AtlasError),
    InvalidLayout {
        section: usize,
        reason: &'static str,
    },
    /// A staged draw referenced a [`PreparedText`] that a newer
    /// [`GpuTextRenderer::prepare`] call has already superseded. The GPU
    /// instance buffers no longer hold that frame, so drawing it would produce
    /// a corrupt frame.
    StalePreparedFrame,
}

impl fmt::Display for TextRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Atlas(error) => write!(f, "glyph atlas: {error}"),
            Self::InvalidLayout { section, reason } => {
                write!(f, "text section {section} has an invalid layout: {reason}")
            }
            Self::StalePreparedFrame => write!(
                f,
                "staged text frame was superseded by a newer prepare call"
            ),
        }
    }
}

impl std::error::Error for TextRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Atlas(error) => Some(error),
            Self::InvalidLayout { .. } | Self::StalePreparedFrame => None,
        }
    }
}

impl From<AtlasError> for TextRenderError {
    fn from(error: AtlasError) -> Self {
        Self::Atlas(error)
    }
}

/// Offscreen `Rgba8Unorm` render target for the text pipeline.
///
/// The texture stays on the GPU. It is Slint-importable
/// (`TEXTURE_BINDING | RENDER_ATTACHMENT`) and `COPY_SRC` is present only so
/// tests/examples can read it back for pixel comparison. Production frames do
/// not download it.
pub struct TextTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: PhysicalSize,
}

impl TextTarget {
    pub fn new(device: &wgpu::Device, size: PhysicalSize) -> Result<Self, BackendError> {
        crate::validate_size(size, device.limits().max_texture_dimension_2d)?;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("xtervo-wgpu-text-target"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FRAME_FORMAT,
            usage: FRAME_USAGE,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(Self {
            texture,
            view,
            size,
        })
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn size(&self) -> PhysicalSize {
        self.size
    }

    /// GPU memory of the RGBA target, in bytes.
    pub fn gpu_bytes(&self) -> u64 {
        u64::from(self.size.width) * u64::from(self.size.height) * 4
    }
}

impl fmt::Debug for TextTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextTarget")
            .field("size", &self.size)
            .field("format", &self.texture.format())
            .finish_non_exhaustive()
    }
}

/// Instances built for one frame. Returned by
/// [`GpuTextRenderer::render_with_geometry`] so validation code can rasterize
/// the exact same geometry on the CPU.
#[derive(Clone, Debug, Default)]
pub struct TextGeometry {
    pub solids: Vec<SolidInstance>,
    pub glyphs: Vec<GlyphInstance>,
    pub cells: usize,
    pub unknown_glyphs: u64,
    pub fallback_glyphs: u64,
    pub missing_glyphs: u64,
    pub style_fallbacks: u64,
    pub missing_outlines: u64,
    pub skipped_control: u64,
    pub wide_cells: u64,
    pub wide_spacer_cells: u64,
    pub hidden_cells: u64,
    pub dim_cells: u64,
    pub selected_cells: u64,
    pub searched_cells: u64,
    pub placeholder_quads: u64,
}


/// A text frame that has been built and uploaded, but not yet drawn.
///
/// This is the staging primitive behind [`GpuTextRenderer`]'s normal
/// single-pass render and the layered image/text composition path:
///
/// ```text
/// prepare -> draw_backgrounds (clear + cell solids)
///         -> [image renderer: below-text images]
///         -> draw_glyphs (load)
///         -> [image renderer: above-text images]
///         -> finish
/// ```
///
/// The GPU instance buffers belong to the renderer, not to this value. A newer
/// [`GpuTextRenderer::prepare`] invalidates it; the draw methods return
/// [`TextRenderError::StalePreparedFrame`] instead of drawing the wrong frame.
#[derive(Clone, Debug)]
pub struct PreparedText {
    geometry: TextGeometry,
    serial: u64,
    width: u32,
    height: u32,
    started: Instant,
    atlas_before: AtlasStats,
    atlas_after: AtlasStats,
    reallocations_before: u64,
    vertex_uploaded_bytes: u64,
}

impl PreparedText {
    /// The exact instances uploaded for this frame, for CPU reference tests.
    pub fn geometry(&self) -> &TextGeometry {
        &self.geometry
    }

    /// Physical size of the frame this geometry was built for.
    pub fn size(&self) -> PhysicalSize {
        PhysicalSize {
            width: self.width,
            height: self.height,
        }
    }

    pub fn solid_count(&self) -> usize {
        self.geometry.solids.len()
    }

    pub fn glyph_count(&self) -> usize {
        self.geometry.glyphs.len()
    }
}

fn rgba(color: u32) -> [f32; 4] {
    [
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
        1.0,
    ]
}

/// Characters that take no grid advance of their own (combining marks,
/// variation selectors, ZWJ/format controls). They are centered on the base
/// glyph instead of advancing a cell.
fn is_zero_width(ch: char) -> bool {
    use unicode_width::UnicodeWidthChar;
    ch.width() == Some(0)
}

/// Result of resolving one character through the atlas fallback chain.
struct EnsuredGlyph {
    slot: Option<GlyphSlot>,
    missing: bool,
}

/// Looks up `ch`, counts style/glyph fallbacks, and substitutes a centered `?`
/// for characters no font covers. Never silently drops or re-shapes a cell.
fn ensure_glyph(
    atlas: &mut GlyphAtlas,
    ch: char,
    px: u16,
    style: FontStyle,
    frame: &mut TextGeometry,
) -> Result<EnsuredGlyph, AtlasError> {
    let lookup = atlas.ensure(ch, px, style)?;
    if lookup.style_fallback {
        frame.style_fallbacks += 1;
    }
    if lookup.glyph_fallback {
        frame.fallback_glyphs += 1;
    }
    if lookup.missing {
        frame.missing_glyphs += 1;
        frame.unknown_glyphs += 1;
        // Explicit, deterministic fallback marker. The caller centers it in
        // the cell span, so a missing wide CJK/emoji does not shift the rest
        // of the row.
        let marker = atlas.ensure('?', px, style)?;
        return Ok(EnsuredGlyph {
            slot: marker.slot,
            missing: true,
        });
    }
    Ok(EnsuredGlyph {
        slot: lookup.slot,
        missing: false,
    })
}

/// Emits one glyph quad clipped to its owning cell rectangle.
///
/// A glyph bitmap can overhang the cell grid: italic faces, fallback faces whose
/// advance differs from the monospace cell, and centered combining marks. The
/// damage path clears/scissors per cell rect, so any ink outside the cell would
/// survive a partial repaint as stale strokes. Clipping the quad (and cropping
/// its UV rectangle by the same fractions) mirrors the CPU renderer's
/// `blit_bitmap` cell clip and keeps incremental output identical to a full
/// redraw. Wide cells pass their full two-column span, so CJK glyphs are not
/// cut. Returns the *unclipped* horizontal ink center for mark alignment.
#[allow(clippy::too_many_arguments)]
fn push_glyph_quad(
    atlas: &GlyphAtlas,
    slot: GlyphSlot,
    x: f32,
    baseline_y: f32,
    cell_x: f32,
    cell_width: f32,
    cell_y: f32,
    cell_height: f32,
    color: [f32; 4],
    frame: &mut TextGeometry,
) -> Option<f32> {
    if slot.is_empty() {
        return None;
    }
    let left = (x + slot.xmin as f32).round();
    let top = baseline_y.round() - (slot.ymin + slot.h as i32) as f32;
    let center = left + slot.w as f32 / 2.0;
    let (right, bottom) = (left + slot.w as f32, top + slot.h as f32);

    // Round the cell edges so clipped quads stay pixel-aligned with the damage
    // rects, which the scene builder derives from integer cell rectangles.
    let clipped_left = left.max(cell_x.round());
    let clipped_top = top.max(cell_y.round());
    let clipped_right = right.min((cell_x + cell_width).round());
    let clipped_bottom = bottom.min((cell_y + cell_height).round());
    if clipped_right <= clipped_left || clipped_bottom <= clipped_top {
        // Fully outside the cell: nothing can be painted. Keep the ink center
        // so a combining mark can still be centered on the base glyph.
        return Some(center);
    }

    let uv = slot.uv(atlas.width(), atlas.height());
    let u_scale = (uv[2] - uv[0]) / (right - left);
    let v_scale = (uv[3] - uv[1]) / (bottom - top);
    let uv = [
        uv[0] + (clipped_left - left) * u_scale,
        uv[1] + (clipped_top - top) * v_scale,
        uv[0] + (clipped_right - left) * u_scale,
        uv[1] + (clipped_bottom - top) * v_scale,
    ];
    frame.glyphs.push(GlyphInstance {
        rect: [
            clipped_left,
            clipped_top,
            clipped_right - clipped_left,
            clipped_bottom - clipped_top,
        ],
        uv,
        color,
    });
    Some(center)
}

/// Explicit hollow box for a character no font (not even `?`) covers.
fn push_placeholder(
    x0: f32,
    y0: f32,
    width: f32,
    height: f32,
    color: [f32; 4],
    frame: &mut TextGeometry,
) {
    frame.placeholder_quads += 1;
    let thickness = 1.0;
    let inner_w = (width - 2.0).max(1.0);
    let inner_h = (height - 2.0).max(1.0);
    for rect in [
        [x0, y0, width, thickness],
        [x0, y0 + height - thickness, width, thickness],
        [x0, y0, thickness, height],
        [x0 + width - thickness, y0, thickness, height],
    ] {
        frame.solids.push(SolidInstance {
            rect,
            color,
        });
    }
    let _ = (inner_w, inner_h);
}

fn build_frame(
    atlas: &mut GlyphAtlas,
    sections: &[TextSection<'_>],
) -> Result<TextGeometry, TextRenderError> {
    let mut frame = TextGeometry::default();

    for (section_index, section) in sections.iter().enumerate() {
        section
            .layout
            .validate()
            .map_err(|reason| TextRenderError::InvalidLayout {
                section: section_index,
                reason,
            })?;
        let layout = section.layout;
        for cell in &section.batch.cells {
            frame.cells += 1;
            let attributes = cell.attributes;

            // The right half of a wide cell owns no background/glyph; the wide
            // cell itself paints a 2-cell background span.
            if attributes.wide_spacer {
                frame.wide_spacer_cells += 1;
                continue;
            }

            let span = if attributes.wide { 2 } else { 1 };
            if attributes.wide {
                frame.wide_cells += 1;
            }
            let x0 = (layout.origin_x + f32::from(cell.col) * layout.cell_width).round();
            let y0 = (layout.origin_y + f32::from(cell.row) * layout.cell_height).round();
            let span_width = layout.cell_width * span as f32;

            // Background: one span-wide quad per logical cell. Cells already
            // painted by the target clear color are skipped so an opaque quad
            // never destroys terminal background opacity.
            let same_as_clear = layout
                .default_background
                .is_some_and(|default| default & 0x00ff_ffff == cell.background & 0x00ff_ffff);
            if !same_as_clear {
                frame.solids.push(SolidInstance {
                    rect: [x0, y0, span_width, layout.cell_height],
                    color: rgba(cell.background),
                });
            }

            if attributes.hidden {
                // SGR 8: background remains visible, glyph is hidden.
                frame.hidden_cells += 1;
                continue;
            }
            if attributes.dim {
                frame.dim_cells += 1;
            }
            if attributes.selected {
                frame.selected_cells += 1;
            }
            if attributes.searched {
                frame.searched_cells += 1;
            }

            let style = FontStyle::from_attributes(attributes);
            let foreground = rgba(cell.foreground);
            let baseline_y = y0 + layout.baseline.round();
            let mut advance_index = 0i32;
            let mut base_center: Option<f32> = None;
            let mut joined = false;

            for ch in cell.text.chars() {
                if ch.is_control() {
                    frame.skipped_control += 1;
                    continue;
                }
                // ZWJ 簇的后半段（`👨‍👩‍👧` 里的 👩/👧）：它们和基准字符合成同一个
                // grapheme cluster，网格里只有 2 格。没有 shaping 时逐个画会把宽字
                // 形推进 2 格、冲出本格；只画簇的第一个字形，和 CPU 渲染器一致。
                if ch == '\u{200D}' {
                    joined = true;
                    continue;
                }
                if joined {
                    joined = false;
                    continue;
                }
                if is_zero_width(ch) {
                    // Combining mark / VS / ZWJ: no advance, center on the last
                    // base glyph's ink like the CPU renderer.
                    if let Some(center) = base_center {
                        let ensured = ensure_glyph(
                            atlas,
                            ch,
                            layout.font_size_px,
                            style,
                            &mut frame,
                        )?;
                        if let Some(slot) = ensured.slot {
                            let mark_x = center - slot.w as f32 / 2.0;
                            push_glyph_quad(
                                atlas,
                                slot,
                                mark_x,
                                baseline_y,
                                x0,
                                span_width,
                                y0,
                                layout.cell_height,
                                foreground,
                                &mut frame,
                            );
                        }
                    }
                    continue;
                }
                if ch.is_whitespace() {
                    advance_index += 1;
                    base_center = None;
                    continue;
                }

                use unicode_width::UnicodeWidthChar;
                let cells = ch.width().unwrap_or(1).clamp(1, span);
                let char_width = layout.cell_width * cells as f32;
                let x = x0 + advance_index as f32 * layout.cell_width;
                advance_index += cells as i32;

                let ensured =
                    ensure_glyph(atlas, ch, layout.font_size_px, style, &mut frame)?;
                match ensured.slot {
                    Some(slot) if !slot.is_empty() => {
                        // Center per-cell glyphs inside their span, matching
                        // the CPU renderer's `(total - advance) / 2` padding.
                        let pad = ((char_width - slot.advance as f32) / 2.0).max(0.0);
                        base_center = push_glyph_quad(
                            atlas,
                            slot,
                            x + pad,
                            baseline_y,
                            // Clip to the columns this character itself
                            // advances over. A synthetic cell can hold more
                            // than one base char ("a\tb"); each one still owns
                            // exactly its own cell span, while the normal
                            // one-char cell maps to `x0..x0+cell_width`.
                            x,
                            char_width,
                            y0,
                            layout.cell_height,
                            foreground,
                            &mut frame,
                        );
                    }
                    _ => {
                        if ensured.missing {
                            push_placeholder(
                                x0,
                                y0,
                                span_width,
                                layout.cell_height,
                                foreground,
                                &mut frame,
                            );
                        } else if !ch.is_whitespace() {
                            frame.missing_outlines += 1;
                        }
                        base_center = None;
                    }
                }
            }

            // Underline/strikeout are cell geometry, not glyph shapes. Their
            // positions match the CPU renderer (bottom-2 / middle).
            let cell_height = layout.cell_height.round();
            if attributes.underline || attributes.double_underline {
                let underline_y = y0 + cell_height - 2.0;
                frame.solids.push(SolidInstance {
                    rect: [x0, underline_y, span_width, 1.0],
                    color: foreground,
                });
                if attributes.double_underline {
                    frame.solids.push(SolidInstance {
                        rect: [x0, underline_y - 2.0, span_width, 1.0],
                        color: foreground,
                    });
                }
            }
            if attributes.strikethrough {
                frame.solids.push(SolidInstance {
                    rect: [x0, y0 + (cell_height / 2.0).floor(), span_width, 1.0],
                    color: foreground,
                });
            }
        }
    }
    Ok(frame)
}

fn ensure_vertex_buffer(
    device: &wgpu::Device,
    buffer: &mut wgpu::Buffer,
    capacity: &mut u64,
    needed: u64,
    label: &str,
) -> bool {
    if needed == 0 || needed <= *capacity {
        return false;
    }
    let new_capacity = needed.max(256).next_power_of_two();
    *buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: new_capacity,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    *capacity = new_capacity;
    true
}

/// Offscreen text renderer that owns the atlas, pipelines and reusable
/// instance buffers.
pub struct GpuTextRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    atlas: GlyphAtlas,
    solid_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    screen_buffer: wgpu::Buffer,
    solid_buffer: wgpu::Buffer,
    solid_capacity: u64,
    glyph_buffer: wgpu::Buffer,
    glyph_capacity: u64,
    solid_bytes: Vec<u8>,
    glyph_bytes: Vec<u8>,
    frame_index: u64,
    buffer_reallocations: u64,
    /// Serial of the most recent [`Self::prepare`]; staged draws must match it.
    prepared_serial: u64,
}

impl fmt::Debug for GpuTextRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuTextRenderer")
            .field("atlas", &self.atlas)
            .field("frame_index", &self.frame_index)
            .field("buffer_reallocations", &self.buffer_reallocations)
            .finish_non_exhaustive()
    }
}

impl GpuTextRenderer {
    /// Loads the discovered system font and creates a default 1024x1024 atlas.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Result<Self, AtlasError> {
        let path = discover_font_path()?;
        Self::with_font_path(device, queue, path, AtlasConfig::default())
    }

    /// Creates a renderer with an explicit font file and atlas configuration.
    pub fn with_font_path(
        device: wgpu::Device,
        queue: wgpu::Queue,
        font_path: impl AsRef<std::path::Path>,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let atlas = GlyphAtlas::from_path(&device, &queue, font_path, config)?;
        Self::with_atlas(device, queue, atlas)
    }

    /// Creates a renderer from explicit regular/bold/italic/bold-italic files.
    ///
    /// The app integration uses this so the GPU atlas rasterizes the exact
    /// same font files the CPU renderer uses (including a Medium regular face
    /// whose bold sibling is not named `Medium-Bold`).
    pub fn with_font_paths(
        device: wgpu::Device,
        queue: wgpu::Queue,
        regular: impl AsRef<std::path::Path>,
        bold: Option<&std::path::Path>,
        italic: Option<&std::path::Path>,
        bold_italic: Option<&std::path::Path>,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let atlas = GlyphAtlas::from_paths(
            &device, &queue, regular, bold, italic, bold_italic, config,
        )?;
        Self::with_atlas(device, queue, atlas)
    }

    /// Creates a renderer from in-memory font bytes (deterministic tests).
    pub fn with_font_bytes(
        device: wgpu::Device,
        queue: wgpu::Queue,
        bytes: Vec<u8>,
        source: impl AsRef<std::path::Path>,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let atlas = GlyphAtlas::from_font_bytes(&device, &queue, bytes, source, config)?;
        Self::with_atlas(device, queue, atlas)
    }

    fn with_atlas(
        device: wgpu::Device,
        queue: wgpu::Queue,
        atlas: GlyphAtlas,
    ) -> Result<Self, AtlasError> {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("xtervo-wgpu-text-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("text.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("xtervo-wgpu-text-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let screen_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xtervo-wgpu-text-screen"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("xtervo-wgpu-text-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("xtervo-wgpu-text-bind-group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: screen_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(atlas.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("xtervo-wgpu-text-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let color_target = wgpu::ColorTargetState {
            format: FRAME_FORMAT,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        };
        let targets = [Some(color_target.clone())];

        let solid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("xtervo-wgpu-text-solid-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_solid"),
                compilation_options: Default::default(),
                buffers: &[Some(SolidInstance::LAYOUT)],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_solid"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });
        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("xtervo-wgpu-text-glyph-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_glyph"),
                compilation_options: Default::default(),
                buffers: &[Some(GlyphInstance::LAYOUT)],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_glyph"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });

        let solid_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xtervo-wgpu-text-solid-instances"),
            size: 256,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let glyph_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xtervo-wgpu-text-glyph-instances"),
            size: 256,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            device,
            queue,
            atlas,
            solid_pipeline,
            glyph_pipeline,
            bind_group,
            screen_buffer,
            solid_buffer,
            solid_capacity: 256,
            glyph_buffer,
            glyph_capacity: 256,
            solid_bytes: Vec::new(),
            glyph_bytes: Vec::new(),
            frame_index: 0,
            buffer_reallocations: 0,
            prepared_serial: 0,
        })
    }

    /// Atlas accessor for metrics and the validation-only CPU reference.
    pub fn atlas(&self) -> &GlyphAtlas {
        &self.atlas
    }

    /// Monospace metrics for a font size.
    pub fn metrics(&self, font_size_px: u16) -> FontMetrics {
        self.atlas.metrics(font_size_px)
    }

    /// Font file backing the regular face.
    pub fn font_path(&self) -> &std::path::Path {
        self.atlas.font_path()
    }

    /// Configured fallback font paths (parsed on demand, not all loaded).
    pub fn fallback_font_paths(&self) -> Vec<std::path::PathBuf> {
        self.atlas.fallback_font_paths()
    }

    /// Renders `sections` into `target` in one render pass.
    ///
    /// The target is cleared to `clear` (RGBA8) regardless of whether any
    /// instances exist. Build failures (including atlas overflow) return before
    /// a command encoder is created, so the target keeps its previous contents.
    pub fn render(
        &mut self,
        target: &TextTarget,
        clear: [u8; 4],
        sections: &[TextSection<'_>],
    ) -> Result<FrameStats, TextRenderError> {
        self.render_with_geometry(target, clear, sections)
            .map(|(stats, _)| stats)
    }

    /// Same as [`Self::render`], but also returns the instance geometry for
    /// pixel-comparison tests/examples.
    pub fn render_with_geometry(
        &mut self,
        target: &TextTarget,
        clear: [u8; 4],
        sections: &[TextSection<'_>],
    ) -> Result<(FrameStats, TextGeometry), TextRenderError> {
        let prepared = self.prepare(target, sections)?;
        let draw_calls = self.draw_prepared_all(target, clear, &prepared)?;
        Ok(self.finish(prepared, draw_calls, 1))
    }

    /// Builds and uploads one text frame without submitting any GPU work.
    ///
    /// The returned [`PreparedText`] can be drawn in layers with
    /// [`Self::draw_backgrounds`] and [`Self::draw_glyphs`], leaving a gap for
    /// the image renderer, then finalized with [`Self::finish`]. A newer
    /// `prepare` call invalidates previously returned values; staged draws
    /// then fail with [`TextRenderError::StalePreparedFrame`].
    pub fn prepare(
        &mut self,
        target: &TextTarget,
        sections: &[TextSection<'_>],
    ) -> Result<PreparedText, TextRenderError> {
        let started = Instant::now();
        let atlas_before = self.atlas.stats();
        let reallocations_before = self.buffer_reallocations;

        let frame = build_frame(&mut self.atlas, sections)?;
        let atlas_after = self.atlas.stats();

        let width = target.size().width;
        let height = target.size().height;
        let screen = [width as f32, height as f32, 0.0_f32, 0.0_f32];
        let mut screen_bytes = Vec::with_capacity(16);
        for value in screen {
            screen_bytes.extend_from_slice(&value.to_le_bytes());
        }
        self.queue
            .write_buffer(&self.screen_buffer, 0, &screen_bytes);

        let solid_needed = frame.solids.len() as u64 * SolidInstance::BYTES;
        let glyph_needed = frame.glyphs.len() as u64 * GlyphInstance::BYTES;
        if ensure_vertex_buffer(
            &self.device,
            &mut self.solid_buffer,
            &mut self.solid_capacity,
            solid_needed,
            "xtervo-wgpu-text-solid-instances",
        ) {
            self.buffer_reallocations += 1;
        }
        if ensure_vertex_buffer(
            &self.device,
            &mut self.glyph_buffer,
            &mut self.glyph_capacity,
            glyph_needed,
            "xtervo-wgpu-text-glyph-instances",
        ) {
            self.buffer_reallocations += 1;
        }

        self.solid_bytes.clear();
        for instance in &frame.solids {
            instance.append_bytes(&mut self.solid_bytes);
        }
        self.glyph_bytes.clear();
        for instance in &frame.glyphs {
            instance.append_bytes(&mut self.glyph_bytes);
        }
        if !self.solid_bytes.is_empty() {
            self.queue
                .write_buffer(&self.solid_buffer, 0, &self.solid_bytes);
        }
        if !self.glyph_bytes.is_empty() {
            self.queue
                .write_buffer(&self.glyph_buffer, 0, &self.glyph_bytes);
        }

        self.prepared_serial = self.prepared_serial.wrapping_add(1);
        Ok(PreparedText {
            geometry: frame,
            serial: self.prepared_serial,
            width,
            height,
            started,
            atlas_before,
            atlas_after,
            reallocations_before,
            vertex_uploaded_bytes: (self.solid_bytes.len() + self.glyph_bytes.len()) as u64,
        })
    }

    /// Clears `target` and draws the prepared text backgrounds (cell solids and
    /// underlines). Returns the number of draw calls submitted.
    ///
    /// This is the first stage of the layered composition path; glyphs are
    /// drawn later by [`Self::draw_glyphs`], which lets below-text images
    /// composite between backgrounds and glyphs instead of being hidden under
    /// opaque cell backgrounds.
    pub fn draw_backgrounds(
        &mut self,
        target: &TextTarget,
        clear: [u8; 4],
        prepared: &PreparedText,
    ) -> Result<u32, TextRenderError> {
        self.validate_prepared(target, prepared)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xtervo-wgpu-text-backgrounds"),
            });
        let draw_calls = self.encode_pass(
            &mut encoder,
            target,
            "xtervo-wgpu-text-backgrounds-pass",
            wgpu::LoadOp::Clear(wgpu::Color {
                r: f64::from(clear[0]) / 255.0,
                g: f64::from(clear[1]) / 255.0,
                b: f64::from(clear[2]) / 255.0,
                a: f64::from(clear[3]) / 255.0,
            }),
            &prepared.geometry.solids,
            &[],
            None,
        );
        self.queue.submit(Some(encoder.finish()));
        Ok(draw_calls)
    }

    /// Draws the prepared glyphs over the target's current contents.
    ///
    /// Returns `0` without submitting when the frame has no glyph quads.
    pub fn draw_glyphs(
        &mut self,
        target: &TextTarget,
        prepared: &PreparedText,
    ) -> Result<u32, TextRenderError> {
        self.validate_prepared(target, prepared)?;
        if prepared.geometry.glyphs.is_empty() {
            return Ok(0);
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xtervo-wgpu-text-glyphs"),
            });
        let draw_calls = self.encode_pass(
            &mut encoder,
            target,
            "xtervo-wgpu-text-glyphs-pass",
            wgpu::LoadOp::Load,
            &[],
            &prepared.geometry.glyphs,
            None,
        );
        self.queue.submit(Some(encoder.finish()));
        Ok(draw_calls)
    }

    /// Clears only `rect` and draws the prepared backgrounds clipped to it.
    ///
    /// The target keeps its previous contents outside the rect (`LoadOp::Load`).
    /// The clear color is painted by one solid quad because WGPU's render-pass
    /// clear ignores scissor. Used by the damage-scissor path; images are not
    /// allowed in this partial path (the caller falls back to a full frame).
    pub fn draw_backgrounds_damaged(
        &mut self,
        target: &TextTarget,
        clear: [u8; 4],
        prepared: &PreparedText,
        rect: [u32; 4],
    ) -> Result<u32, TextRenderError> {
        self.validate_prepared(target, prepared)?;
        let [x, y, width, height] = rect;
        if width == 0 || height == 0 {
            return Ok(0);
        }
        let size = target.size();
        let x = x.min(size.width);
        let y = y.min(size.height);
        let width = width.min(size.width.saturating_sub(x));
        let height = height.min(size.height.saturating_sub(y));
        if width == 0 || height == 0 {
            return Ok(0);
        }
        let clear_instance = SolidInstance {
            rect: [x as f32, y as f32, width as f32, height as f32],
            color: [
                f32::from(clear[0]) / 255.0,
                f32::from(clear[1]) / 255.0,
                f32::from(clear[2]) / 255.0,
                f32::from(clear[3]) / 255.0,
            ],
        };
        let mut clear_bytes = Vec::with_capacity(SolidInstance::BYTES as usize);
        clear_instance.append_bytes(&mut clear_bytes);
        let clear_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xtervo-wgpu-text-damage-clear"),
            size: SolidInstance::BYTES,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&clear_buffer, 0, &clear_bytes);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xtervo-wgpu-text-damage-backgrounds"),
            });
        let mut draw_calls = 0u32;
        {
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: target.view(),
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("xtervo-wgpu-text-damage-backgrounds-pass"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_scissor_rect(x, y, width, height);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_pipeline(&self.solid_pipeline);
            pass.set_vertex_buffer(0, clear_buffer.slice(..));
            pass.draw(0..6, 0..1);
            draw_calls += 1;
            if !prepared.geometry.solids.is_empty() {
                pass.set_vertex_buffer(0, self.solid_buffer.slice(..));
                pass.draw(0..6, 0..prepared.geometry.solids.len() as u32);
                draw_calls += 1;
            }
        }
        self.queue.submit(Some(encoder.finish()));
        Ok(draw_calls)
    }

    /// Draws prepared glyphs clipped to `rect`, keeping the rest of the target.
    pub fn draw_glyphs_damaged(
        &mut self,
        target: &TextTarget,
        prepared: &PreparedText,
        rect: [u32; 4],
    ) -> Result<u32, TextRenderError> {
        self.validate_prepared(target, prepared)?;
        if prepared.geometry.glyphs.is_empty() {
            return Ok(0);
        }
        let size = target.size();
        let [x, y, width, height] = rect;
        let x = x.min(size.width);
        let y = y.min(size.height);
        let width = width.min(size.width.saturating_sub(x));
        let height = height.min(size.height.saturating_sub(y));
        if width == 0 || height == 0 {
            return Ok(0);
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xtervo-wgpu-text-damage-glyphs"),
            });
        let draw_calls = self.encode_pass(
            &mut encoder,
            target,
            "xtervo-wgpu-text-damage-glyphs-pass",
            wgpu::LoadOp::Load,
            &[],
            &prepared.geometry.glyphs,
            Some([x, y, width, height]),
        );
        self.queue.submit(Some(encoder.finish()));
        Ok(draw_calls)
    }

    /// Finalizes a prepared frame and returns its stats plus instance geometry.
    ///
    /// `draw_calls`/`render_passes` are supplied by the caller because the
    /// staged path may interleave non-text passes (images).
    pub fn finish(
        &mut self,
        prepared: PreparedText,
        draw_calls: u32,
        render_passes: u32,
    ) -> (FrameStats, TextGeometry) {
        self.frame_index = self.frame_index.wrapping_add(1);
        let delta = |before: u64, after: u64| after.saturating_sub(before);
        let geometry = prepared.geometry;
        let stats = FrameStats {
            frame: self.frame_index,
            width: prepared.width,
            height: prepared.height,
            cells: geometry.cells,
            solid_quads: geometry.solids.len(),
            glyph_quads: geometry.glyphs.len(),
            render_passes,
            draw_calls,
            atlas_hits: delta(prepared.atlas_before.hits, prepared.atlas_after.hits),
            atlas_misses: delta(prepared.atlas_before.misses, prepared.atlas_after.misses),
            atlas_rasterized: delta(
                prepared.atlas_before.rasterized,
                prepared.atlas_after.rasterized,
            ),
            atlas_uploads: delta(prepared.atlas_before.uploads, prepared.atlas_after.uploads),
            atlas_uploaded_bytes: delta(
                prepared.atlas_before.uploaded_bytes,
                prepared.atlas_after.uploaded_bytes,
            ),
            vertex_uploaded_bytes: prepared.vertex_uploaded_bytes,
            atlas_gpu_bytes: self.atlas.gpu_bytes(),
            target_gpu_bytes: u64::from(prepared.width) * u64::from(prepared.height) * 4,
            atlas_cached_glyphs: self.atlas.cached_glyph_count(),
            buffer_reallocations: delta(prepared.reallocations_before, self.buffer_reallocations),
            unknown_glyphs: geometry.unknown_glyphs,
            fallback_glyphs: geometry.fallback_glyphs,
            missing_glyphs: geometry.missing_glyphs,
            style_fallbacks: geometry.style_fallbacks,
            missing_outlines: geometry.missing_outlines,
            skipped_control: geometry.skipped_control,
            wide_cells: geometry.wide_cells,
            wide_spacer_cells: geometry.wide_spacer_cells,
            hidden_cells: geometry.hidden_cells,
            dim_cells: geometry.dim_cells,
            selected_cells: geometry.selected_cells,
            searched_cells: geometry.searched_cells,
            placeholder_quads: geometry.placeholder_quads,
            scene_revision: 0,
            scene_images_skipped: 0,
            scene_cursor_skipped: false,
            background_from_scene: false,
            cpu_time: prepared.started.elapsed(),
        };
        (stats, geometry)
    }

    fn validate_prepared(
        &self,
        target: &TextTarget,
        prepared: &PreparedText,
    ) -> Result<(), TextRenderError> {
        if prepared.serial != self.prepared_serial || prepared.size() != target.size() {
            return Err(TextRenderError::StalePreparedFrame);
        }
        Ok(())
    }

    fn draw_prepared_all(
        &mut self,
        target: &TextTarget,
        clear: [u8; 4],
        prepared: &PreparedText,
    ) -> Result<u32, TextRenderError> {
        self.validate_prepared(target, prepared)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xtervo-wgpu-text-encoder"),
            });
        let draw_calls = self.encode_pass(
            &mut encoder,
            target,
            "xtervo-wgpu-text-pass",
            wgpu::LoadOp::Clear(wgpu::Color {
                r: f64::from(clear[0]) / 255.0,
                g: f64::from(clear[1]) / 255.0,
                b: f64::from(clear[2]) / 255.0,
                a: f64::from(clear[3]) / 255.0,
            }),
            &prepared.geometry.solids,
            &prepared.geometry.glyphs,
            None,
        );
        self.queue.submit(Some(encoder.finish()));
        Ok(draw_calls)
    }

    fn encode_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &TextTarget,
        label: &'static str,
        load: wgpu::LoadOp<wgpu::Color>,
        solids: &[SolidInstance],
        glyphs: &[GlyphInstance],
        scissor: Option<[u32; 4]>,
    ) -> u32 {
        let mut draw_calls = 0u32;
        {
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: target.view(),
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(label),
                color_attachments: &color_attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some([x, y, width, height]) = scissor {
                pass.set_scissor_rect(x, y, width, height);
            }
            pass.set_bind_group(0, &self.bind_group, &[]);
            if !solids.is_empty() {
                pass.set_pipeline(&self.solid_pipeline);
                pass.set_vertex_buffer(0, self.solid_buffer.slice(..));
                pass.draw(0..6, 0..solids.len() as u32);
                draw_calls += 1;
            }
            if !glyphs.is_empty() {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_vertex_buffer(0, self.glyph_buffer.slice(..));
                pass.draw(0..6, 0..glyphs.len() as u32);
                draw_calls += 1;
            }
        }
        draw_calls
    }

    /// Cumulative atlas counters. Useful for soak/perf checks.
    pub fn atlas_stats(&self) -> AtlasStats {
        self.atlas.stats()
    }
}

/// Reads an offscreen target back to tightly packed RGBA8.
///
/// Validation only: this is the one place in the prototype that copies a full
/// frame from GPU to CPU, and it is never called by [`GpuTextRenderer::render`]
/// or by the Slint presenter.
pub fn read_target_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    target: &TextTarget,
) -> Result<Vec<u8>, String> {
    const COPY_ALIGNMENT: u32 = 256;
    let size = target.size();
    let unpadded = size.width * 4;
    let padded = unpadded.div_ceil(COPY_ALIGNMENT) * COPY_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("xtervo-wgpu-text-readback"),
        size: u64::from(padded) * u64::from(size.height),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("xtervo-wgpu-text-readback-encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: target.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(size.height),
            },
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|error| format!("device.poll failed during readback: {error}"))?;
    receiver
        .recv()
        .map_err(|error| format!("readback map callback dropped: {error}"))?
        .map_err(|error| format!("readback map failed: {error}"))?;

    let mapped = slice
        .get_mapped_range()
        .map_err(|error| format!("readback get_mapped_range failed: {error}"))?;
    let mut rgba = Vec::with_capacity((unpadded * size.height) as usize);
    for row in 0..size.height as usize {
        let start = row * padded as usize;
        rgba.extend_from_slice(&mapped[start..start + unpadded as usize]);
    }
    drop(mapped);
    buffer.unmap();
    Ok(rgba)
}
