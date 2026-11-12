//! Backend-independent terminal draw scene.

use tervo_graphics::{CompositePolicy, PlacementId, Rect, RowId, SourceId};

use crate::{DamageRegion, TextBatch};

/// Bottom composition layer. A background item covers everything below it.
pub const BACKGROUND_LAYER_Z: i32 = i32::MIN;
/// Text and cell selection layer. Selection is part of [`TextBatch`], not a separate overlay.
pub const TEXT_LAYER_Z: i32 = tervo_graphics::TEXT_LAYER_Z;
/// Top composition layer. The cursor is always emitted after images.
pub const CURSOR_LAYER_Z: i32 = i32::MAX;

/// Physical output size in device pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

/// Logical terminal viewport geometry.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    pub cell_w: f32,
    pub cell_h: f32,
    pub cols: usize,
    pub rows: usize,
}

impl Viewport {
    /// The drawable viewport rectangle in logical pixels.
    pub fn pixel_bounds(&self) -> Rect {
        Rect::new(
            0.0,
            0.0,
            self.cell_w.max(0.0) * self.cols as f32,
            self.cell_h.max(0.0) * self.rows as f32,
        )
    }

    /// The pixel rectangle for one visible cell.
    pub fn cell_rect(&self, row: usize, col: usize) -> Rect {
        Rect::new(
            col as f32 * self.cell_w.max(0.0),
            row as f32 * self.cell_h.max(0.0),
            self.cell_w.max(0.0),
            self.cell_h.max(0.0),
        )
    }
}

/// One immutable frame description consumed by renderers.
///
/// The span/cursor fields are provenance for the text layer: selection and search colors are
/// already resolved into [`TextBatch`] cells, and a visible cursor is also emitted as a
/// [`DrawItem::Cursor`] at [`CURSOR_LAYER_Z`]. Keeping the source spans lets a presenter redraw
/// highlight changes without re-deriving them from cell colors.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub viewport: Viewport,
    pub revision: u64,
    pub damage: DamageRegion,
    /// Inclusive selection span (viewport cell coordinates), if the frame has a selection.
    pub selection: Option<SelectionSpan>,
    /// Inclusive search-hit span (viewport cell coordinates), if the frame has one.
    pub search: Option<SelectionSpan>,
    /// Cursor state from the source frame. [`Self::cursor`] is present even when the cursor is
    /// hidden or out of bounds; only a visible, in-bounds cursor has a draw item.
    pub cursor: Option<CursorState>,
    pub layers: Vec<SceneLayer>,
}

impl Scene {
    /// Returns the layer at an exact composition z, if present.
    pub fn layer(&self, z_index: i32) -> Option<&SceneLayer> {
        self.layers.iter().find(|layer| layer.z_index == z_index)
    }

    /// Iterates image instances in composition order.
    pub fn images(&self) -> impl Iterator<Item = &ImageInstance> {
        self.layers
            .iter()
            .flat_map(|layer| layer.items.iter())
            .filter_map(|item| match item {
                DrawItem::Image(image) => Some(image),
                _ => None,
            })
    }

    /// The text batch in composition order, if the scene has one.
    ///
    /// The frame adapter always emits exactly one batch at [`TEXT_LAYER_Z`]. Renderers that
    /// consume the WGPU text path can pass this directly to their text pipeline.
    pub fn text_batch(&self) -> Option<&TextBatch> {
        self.layers
            .iter()
            .flat_map(|layer| layer.items.iter())
            .find_map(|item| match item {
                DrawItem::Text(batch) => Some(batch),
                _ => None,
            })
    }

    /// Number of image instances in the scene.
    pub fn image_count(&self) -> usize {
        self.images().count()
    }

    /// Sorts layers by z while preserving item order inside each layer.
    ///
    /// Scene builders already produce sorted layers; this is available to callers that assemble
    /// a scene manually. The stable sort is intentional: equal z-indices use item order.
    pub fn sort_layers(&mut self) {
        self.layers.sort_by_key(|layer| layer.z_index);
    }
}

/// Inclusive span of viewport cells, used for selection and search provenance.
///
/// Coordinates are half-open only in the sense that they address cells: both endpoints are
/// included, matching the terminal selection contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionSpan {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
}

impl SelectionSpan {
    /// Whether a viewport cell lies inside the span.
    pub fn contains(&self, row: u16, col: u16) -> bool {
        if row < self.start_row || row > self.end_row {
            return false;
        }
        if self.start_row == self.end_row {
            return col >= self.start_col && col <= self.end_col;
        }
        if row == self.start_row {
            return col >= self.start_col;
        }
        if row == self.end_row {
            return col <= self.end_col;
        }
        true
    }
}

/// Cursor shape requested by the terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

/// Cursor state as reported by the source frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorState {
    /// Viewport row.
    pub row: u16,
    /// Grid column.
    pub col: u16,
    pub shape: CursorShape,
    /// False when the terminal hides the cursor or applies a hidden cursor shape.
    pub visible: bool,
}

/// A composition layer. Equal z values are one layer and preserve item insertion order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneLayer {
    pub z_index: i32,
    pub items: Vec<DrawItem>,
}

/// A drawable item. Selection is intentionally absent: it belongs to [`TextBatch`] cells.
#[derive(Clone, Debug, PartialEq)]
pub enum DrawItem {
    Background(BackgroundInstance),
    Text(TextBatch),
    Image(ImageInstance),
    Cursor(CursorInstance),
}

/// Terminal background clear color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackgroundInstance {
    pub color: u32,
}

/// A clipped, positioned image prepared for the renderer.
///
/// Image **pixels are intentionally absent**: [`Self::source_id`] identifies immutable RGBA data
/// that the renderer already owns (or uploads through a separate resource path). The scene only
/// carries metadata and geometry, so building it never copies pixel buffers.
///
/// `destination` is the unclipped placement rectangle, `clip` is its visible intersection with
/// the viewport, and `source_rect` is the source-pixel crop. The renderer must use `clip` as a
/// scissor and must not derive row identity from any of these numbers: [`Self::row`] is only the
/// viewport projection, while [`Self::row_id`] is the stable terminal row identity.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageInstance {
    pub placement_id: PlacementId,
    pub source_id: SourceId,
    /// Crop inside the source image, in source pixels.
    pub source_rect: Rect,
    /// Full source pixel dimensions, used to normalize [`Self::source_rect`] into texture UVs.
    pub source_size: (u32, u32),
    pub destination: Rect,
    pub clip: Rect,
    /// Explicit target cell rectangle from the protocol metadata (Kitty `c`/`r`).
    pub cells_w: u16,
    pub cells_h: u16,
    /// Viewport row (`0` = top row). Negative when the image starts above the viewport.
    pub row: i32,
    /// Grid column of the image's left edge.
    pub col: u16,
    /// Stable terminal row identity; never derived from [`Self::row`].
    pub row_id: RowId,
    /// Protocol z-index as supplied by the placement. Composition order comes from the layer's
    /// policy-aware z, not from this raw value.
    pub z_index: i32,
    /// Whether the image composites after glyphs. Kept from the protocol frame explicitly.
    pub above_text: bool,
    pub policy: CompositePolicy,
}

/// Cursor rectangle in logical pixels.
///
/// The cursor is emitted as a normal scene item at [`CURSOR_LAYER_Z`]. `shape` tells the
/// renderer whether to fill the whole cell (block), draw a beam/underline, or leave the
/// hollow-block outline; hidden cursors are not emitted at all.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CursorInstance {
    pub row: u16,
    pub col: u16,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub shape: CursorShape,
}

#[cfg(test)]
mod tests {
    use super::Viewport;

    #[test]
    fn viewport_geometry_is_clamped_to_drawable_pixels() {
        let viewport = Viewport {
            cell_w: 8.0,
            cell_h: 16.0,
            cols: 10,
            rows: 5,
        };
        assert_eq!(viewport.pixel_bounds().w, 80.0);
        assert_eq!(viewport.cell_rect(2, 3).x, 24.0);
        assert_eq!(viewport.cell_rect(2, 3).y, 32.0);
    }
}
