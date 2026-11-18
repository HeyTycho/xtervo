//! Adapter from the terminal snapshot ([`tervo_core::Frame`]) to the immutable draw scene.
//!
//! This module is the only place in `tervo-scene` that knows about the terminal core. It is
//! enabled by the default `frame` feature; disable default features to build the pure
//! graphics/scene contract without a terminal dependency.
//!
//! The conversion follows the legacy CPU renderer's *semantics* (selection is an inverse swap,
//! search uses the documented highlight colors, inverse cells are resolved before selection)
//! but never copies the full framebuffer and never copies RGBA image data: images are
//! represented only by [`tervo_graphics::SourceId`], crop/destination geometry and placement
//! metadata.
//!
//! # Layer order
//!
//! * background at [`BACKGROUND_LAYER_Z`] when the caller supplies one,
//! * below-text images at their policy-aware z (always `<` [`TEXT_LAYER_Z`]),
//! * one [`TextBatch`] at [`TEXT_LAYER_Z`] (selection/search colors already resolved),
//! * above-text images at their policy-aware z (always `>` [`TEXT_LAYER_Z`]),
//! * cursor at [`CURSOR_LAYER_Z`].
//!
//! # Skipped frames
//!
//! [`Frame::skipped`] means the actor did not produce a snapshot; the frame content is a blank
//! placeholder. Callers must keep presenting the previous scene and not convert a skipped
//! frame. The builder intentionally does not guess or synthesize pixels for that case.

use std::collections::BTreeMap;

use tervo_core::{
    flags, CellData, CursorShape as FrameCursorShape, Frame, InlineImage, Span as FrameSpan,
};
use tervo_graphics::{CompositePolicy, PlacementId, Rect, RowId, SourceId};

use crate::{
    BackgroundInstance, CursorInstance, CursorShape, CursorState, DamageRect, DamageRegion,
    DrawItem, ImageInstance, Scene, SceneLayer, SelectionSpan, TextAttributes, TextBatch, TextCell,
    Viewport, BACKGROUND_LAYER_Z, CURSOR_LAYER_Z, TEXT_LAYER_Z,
};

/// Colors used to resolve search hits into cell colors.
///
/// These defaults are the legacy CPU renderer's search highlight colors. Keeping the resolution
/// in the scene builder means the CPU reference and the WGPU backend consume the same resolved
/// cell colors instead of each owning a copy of the policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionStyle {
    pub search_foreground: u32,
    pub search_background: u32,
}

impl Default for SelectionStyle {
    fn default() -> Self {
        Self {
            search_foreground: 0x0011_111b,
            search_background: 0x00f9_e2af,
        }
    }
}

/// Builder that converts one [`Frame`] into one immutable [`Scene`].
///
/// The builder borrows the frame only for the duration of [`Self::build`]; the resulting scene
/// owns no terminal or pixel data. Image entries reference pixels only by `source_id`.
///
/// ```ignore
/// let scene = SceneBuilder::from_frame(&frame, viewport).build();
/// renderer.render(&scene, &scene.damage)
/// ```
pub struct FrameSceneBuilder<'a> {
    frame: &'a Frame,
    viewport: Viewport,
    revision: Option<u64>,
    damage: Option<DamageRegion>,
    background: Option<BackgroundInstance>,
    style: SelectionStyle,
    /// 图片 source ID 的命名空间（GPU 多 pane 时用 pane id）。
    source_namespace: u64,
}

impl<'a> FrameSceneBuilder<'a> {
    /// Creates a builder for one frame and viewport.
    pub fn new(frame: &'a Frame, viewport: Viewport) -> Self {
        Self {
            frame,
            viewport,
            revision: None,
            damage: None,
            background: None,
            style: SelectionStyle::default(),
            source_namespace: 0,
        }
    }

    /// Overrides the scene revision. Defaults to [`Frame::generation`].
    /// 给图片 source ID 加命名空间；默认 0 保持单 pane/测试行为不变。
    pub fn source_namespace(mut self, namespace: u64) -> Self {
        self.source_namespace = namespace;
        self
    }

    /// Overrides the scene revision. Defaults to [`Frame::generation`].
    pub fn revision(mut self, revision: u64) -> Self {
        self.revision = Some(revision);
        self
    }

    /// Overrides the mapped damage region.
    ///
    /// A stateless conversion cannot know about highlight changes between frames. A caller that
    /// tracks the previous selection/search/cursor must merge that damage itself (or request a
    /// full invalidation); this hook is the place to pass the merged region.
    pub fn damage(mut self, damage: DamageRegion) -> Self {
        self.damage = Some(damage);
        self
    }

    /// Adds a background clear item at [`BACKGROUND_LAYER_Z`].
    pub fn background(mut self, background: BackgroundInstance) -> Self {
        self.background = Some(background);
        self
    }

    /// Overrides the search highlight colors.
    pub fn selection_style(mut self, style: SelectionStyle) -> Self {
        self.style = style;
        self
    }

    /// Builds the immutable scene.
    ///
    /// Every frame cell becomes one [`TextCell`], including wide-char spacers and blank cells,
    /// so row/column identity is preserved exactly. A spacer cell carries
    /// [`TextAttributes::wide_spacer`] and must not be shaped by the renderer.
    pub fn build(self) -> Scene {
        let Self {
            frame,
            viewport,
            revision,
            damage,
            background,
            style,
            source_namespace,
        } = self;

        let mut layers: BTreeMap<i32, Vec<DrawItem>> = BTreeMap::new();
        if let Some(background) = background {
            layers
                .entry(BACKGROUND_LAYER_Z)
                .or_default()
                .push(DrawItem::Background(background));
        }

        // Selection/search live in TextBatch cells, so the text layer always exists.
        layers
            .entry(TEXT_LAYER_Z)
            .or_default()
            .push(DrawItem::Text(text_batch(frame, style)));

        // Frame.images already has protocol paint order. Pushing in that order keeps ties
        // deterministic; BTreeMap fixes the cross-layer order.
        for image in &frame.images {
            if let Some((layer_z, instance)) =
                image_instance(frame, viewport, image, source_namespace)
            {
                layers
                    .entry(layer_z)
                    .or_default()
                    .push(DrawItem::Image(instance));
            }
        }

        let cursor = cursor_state(frame);
        if let Some(cursor) = cursor {
            if cursor.visible {
                let instance = cursor_instance(frame, viewport, cursor);
                layers
                    .entry(CURSOR_LAYER_Z)
                    .or_default()
                    .push(DrawItem::Cursor(instance));
            }
        }

        Scene {
            viewport,
            revision: revision.unwrap_or(frame.generation),
            damage: damage.unwrap_or_else(|| frame_damage(frame, viewport)),
            selection: frame.selection.map(selection_span),
            search: frame.search.map(selection_span),
            cursor,
            layers: layers
                .into_iter()
                .map(|(z_index, items)| SceneLayer { z_index, items })
                .collect(),
        }
    }
}

/// Converts a frame to a scene with default style, damage and revision.
///
/// This is the direct-function equivalent of `SceneBuilder::from_frame(...).build()`.
pub fn scene_from_frame(frame: &Frame, viewport: Viewport) -> Scene {
    FrameSceneBuilder::new(frame, viewport).build()
}

fn text_batch(frame: &Frame, style: SelectionStyle) -> TextBatch {
    let mut batch = TextBatch::new(frame.generation);
    if frame.cols == 0 || frame.rows == 0 {
        return batch;
    }
    // TextCell stores u16 coordinates. A terminal grid larger than that cannot be addressed by
    // a renderer anyway; truncating is explicit and keeps the mapping panic-free.
    let rows = frame.rows.min(u16::MAX as usize);
    let cols = frame.cols.min(u16::MAX as usize);
    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = frame.cells.get(row * frame.cols + col) else {
                continue;
            };
            batch.push(text_cell(frame, style, row as u16, col as u16, cell));
        }
    }
    batch
}

fn text_cell(
    frame: &Frame,
    style: SelectionStyle,
    row: u16,
    col: u16,
    cell: &CellData,
) -> TextCell {
    let mut foreground = cell.fg & 0x00ff_ffff;
    let mut background = cell.bg & 0x00ff_ffff;

    // Resolve semantic attributes into the colors the renderer consumes. The flags remain on
    // the cell for provenance and for features such as hidden/dim/wide that colors cannot carry.
    if cell.has(flags::INVERSE) {
        std::mem::swap(&mut foreground, &mut background);
    }
    let selected = frame.selection.is_some_and(|span| span.contains(row, col));
    if selected {
        // Selection is a cell attribute, not an overlay: swap the already inverse-resolved
        // colors, matching the legacy CPU renderer's fill order.
        std::mem::swap(&mut foreground, &mut background);
    }
    let searched = frame.search.is_some_and(|span| span.contains(row, col));
    if searched {
        foreground = style.search_foreground & 0x00ff_ffff;
        background = style.search_background & 0x00ff_ffff;
    }

    let mut text = String::new();
    text.push(cell.char());
    if !cell.has(flags::WIDE_CHAR_SPACER) {
        // Combining marks belong to the preceding cell in the terminal grid. Appending them is
        // not shaping; it only carries the real text run across the boundary.
        for mark in frame.zerowidth_at(row as usize, col as usize) {
            text.push(*mark);
        }
    }

    TextCell {
        row,
        col,
        text,
        foreground,
        background,
        attributes: TextAttributes {
            bold: cell.has(flags::BOLD),
            italic: cell.has(flags::ITALIC),
            underline: cell.has(flags::UNDERLINE_MASK),
            double_underline: cell.has(flags::DOUBLE_UNDERLINE),
            strikethrough: cell.has(flags::STRIKEOUT),
            dim: cell.has(flags::DIM),
            hidden: cell.has(flags::HIDDEN),
            inverse: cell.has(flags::INVERSE),
            wide: cell.has(flags::WIDE_CHAR),
            wide_spacer: cell.has(flags::WIDE_CHAR_SPACER),
            selected,
            searched,
        },
    }
}

fn selection_span(span: FrameSpan) -> SelectionSpan {
    SelectionSpan {
        start_row: span.start_line,
        start_col: span.start_col,
        end_row: span.end_line,
        end_col: span.end_col,
    }
}

fn cursor_state(frame: &Frame) -> Option<CursorState> {
    if frame.rows == 0 || frame.cols == 0 {
        return None;
    }
    let row = frame.cursor.line;
    let col = frame.cursor.col;
    if row as usize >= frame.rows || col as usize >= frame.cols {
        return None;
    }
    let shape = map_cursor_shape(frame.cursor.shape);
    Some(CursorState {
        row,
        col,
        shape,
        visible: frame.cursor.visible && shape != CursorShape::Hidden,
    })
}

fn map_cursor_shape(shape: FrameCursorShape) -> CursorShape {
    match shape {
        FrameCursorShape::Block => CursorShape::Block,
        FrameCursorShape::Underline => CursorShape::Underline,
        FrameCursorShape::Beam => CursorShape::Beam,
        FrameCursorShape::HollowBlock => CursorShape::HollowBlock,
        FrameCursorShape::Hidden => CursorShape::Hidden,
    }
}

fn cursor_instance(frame: &Frame, viewport: Viewport, cursor: CursorState) -> CursorInstance {
    let cell_w = viewport.cell_w.max(0.0);
    let cell_h = viewport.cell_h.max(0.0);
    let span = frame
        .cells
        .get(cursor.row as usize * frame.cols + cursor.col as usize)
        .map_or(1, |cell| {
            if cell.has(flags::WIDE_CHAR) || cell.has(flags::WIDE_CHAR_SPACER) {
                2
            } else {
                1
            }
        });
    CursorInstance {
        row: cursor.row,
        col: cursor.col,
        x: cursor.col as f32 * cell_w,
        y: cursor.row as f32 * cell_h,
        w: span as f32 * cell_w,
        h: cell_h,
        shape: cursor.shape,
    }
}

fn frame_damage(frame: &Frame, viewport: Viewport) -> DamageRegion {
    if frame.full {
        return DamageRegion::full();
    }
    let mut damage = DamageRegion::empty();
    let bounds = viewport.pixel_bounds();
    if bounds.is_empty() {
        return damage;
    }
    let cell_w = viewport.cell_w.max(0.0);
    let cell_h = viewport.cell_h.max(0.0);
    for line in &frame.damage {
        // LineDamage.right is inclusive, unlike Rect/DamageRect half-open edges.
        let x0 = (f32::from(line.left) * cell_w).floor().clamp(0.0, bounds.w);
        let x1 = ((f32::from(line.right) + 1.0) * cell_w)
            .ceil()
            .clamp(0.0, bounds.w);
        let y0 = (f32::from(line.line) * cell_h).floor().clamp(0.0, bounds.h);
        let y1 = ((f32::from(line.line) + 1.0) * cell_h)
            .ceil()
            .clamp(0.0, bounds.h);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        damage.push(DamageRect::new(
            x0 as u32,
            y0 as u32,
            (x1 - x0) as u32,
            (y1 - y0) as u32,
        ));
    }
    damage
}

fn image_instance(
    frame: &Frame,
    viewport: Viewport,
    image: &InlineImage,
    source_namespace: u64,
) -> Option<(i32, ImageInstance)> {
    if image.width == 0 || image.height == 0 {
        return None;
    }
    // Frame documents a one-to-one, same-order relationship, but pairing by placement id is
    // robust against callers that rebuild one of the two vectors. There is intentionally no
    // index-based fallback that could silently attach the wrong crop.
    let meta = frame
        .image_placements
        .iter()
        .find(|meta| meta.placement_id == image.id);

    let source_bounds = Rect::new(0.0, 0.0, image.width as f32, image.height as f32);
    let source_rect = meta
        .and_then(|meta| meta.source_rect)
        .map_or(source_bounds, |crop| {
            Rect::new(crop.x as f32, crop.y as f32, crop.w as f32, crop.h as f32)
        })
        .intersection(source_bounds);
    if source_rect.is_empty() || !source_rect.is_finite() {
        return None;
    }

    let cell_w = viewport.cell_w.max(1.0);
    let cell_h = viewport.cell_h.max(1.0);
    let cells_w = meta
        .map(|meta| meta.cells_w)
        .filter(|cells| *cells > 0)
        .unwrap_or_else(|| pixels_to_cells(image.width, cell_w));
    let cells_h = meta
        .map(|meta| meta.cells_h)
        .filter(|cells| *cells > 0)
        .unwrap_or_else(|| pixels_to_cells(image.height, cell_h));

    let x = image.col as f32 * cell_w;
    let y = image.row as f32 * cell_h;
    let explicit_cell_rect =
        meta.is_some_and(|meta| meta.source_rect.is_some() && meta.cells_w > 0 && meta.cells_h > 0);
    let (display_w, display_h) = if explicit_cell_rect {
        (cells_w as f32 * cell_w, cells_h as f32 * cell_h)
    } else {
        // Mirror the legacy frame contract: Sixel/IIP placement without an explicit crop keeps
        // the pane-fit behavior; otherwise only shrink to fit the remaining viewport.
        let avail_w = (viewport.cell_w.max(0.0) * viewport.cols as f32 - x).max(cell_w);
        let avail_h = (viewport.cell_h.max(0.0) * viewport.rows as f32 - y.max(0.0)).max(cell_h);
        fit_inline_image_size(source_rect.w, source_rect.h, avail_w, avail_h)
    };
    let destination = Rect::new(x, y, display_w, display_h);
    if destination.is_empty() || !destination.is_finite() {
        return None;
    }
    let clip = destination.intersection(viewport.pixel_bounds());
    if clip.is_empty() {
        return None;
    }

    let above_text = meta.map_or(image.above_text, |meta| meta.above_text);
    let z_index = meta.map_or(image.z_index, |meta| meta.z_index);
    let row_id = meta.map_or(image.row_id, |meta| meta.row_id);
    // The policy selects the layer group and wins over the raw protocol z-index. This is the
    // same rule the graphics/scene builder uses, so images can never silently cross the text
    // layer just because a protocol emitted an unexpected z value.
    let policy = if above_text {
        CompositePolicy::AboveText
    } else {
        CompositePolicy::BelowText
    };
    let layer_z = policy.composition_z(z_index);

    Some((
        layer_z,
        ImageInstance {
            placement_id: PlacementId(image.id),
            source_id: SourceId::namespaced(source_namespace, image.source_id),
            source_rect,
            source_size: (image.width, image.height),
            destination,
            clip,
            cells_w,
            cells_h,
            row: image.row,
            col: image.col,
            row_id: RowId(row_id),
            z_index,
            above_text,
            policy,
        },
    ))
}

fn pixels_to_cells(pixels: u32, cell_pixels: f32) -> u16 {
    let cells = (pixels as f32 / cell_pixels.max(1.0)).ceil();
    cells.clamp(1.0, u16::MAX as f32) as u16
}

/// Mirrors the legacy CPU renderer's inline-image fit rule: never upscale, keep at least one
/// pixel, and leave the source size untouched when there is no usable available box.
fn fit_inline_image_size(src_w: f32, src_h: f32, avail_w: f32, avail_h: f32) -> (f32, f32) {
    if src_w <= 0.0 || src_h <= 0.0 || avail_w <= 1.0 || avail_h <= 1.0 {
        return (src_w, src_h);
    }
    let scale = (avail_w / src_w).min(avail_h / src_h).min(1.0);
    ((src_w * scale).max(1.0), (src_h * scale).max(1.0))
}

/// Convenience constructor on [`crate::SceneBuilder`].
///
/// The graphics-backed [`crate::SceneBuilder::new`] and this frame-backed entry point are
/// deliberately separate constructors; this one ignores the row resolver because a real
/// [`Frame`] already contains resolved viewport rows.
impl crate::SceneBuilder<'static, ()> {
    /// Starts a frame-backed scene build.
    pub fn from_frame(frame: &Frame, viewport: Viewport) -> FrameSceneBuilder<'_> {
        FrameSceneBuilder::new(frame, viewport)
    }
}
