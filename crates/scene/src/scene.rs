//! Backend-independent terminal draw scene.

use tervo_graphics::{CompositePolicy, PlacementId, Rect, SourceId};

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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub viewport: Viewport,
    pub revision: u64,
    pub damage: DamageRegion,
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
/// `destination` is the unclipped placement rectangle, `clip` is its visible intersection with
/// the viewport, and `source_rect` is the source-pixel crop. The renderer must use `clip` as a
/// scissor and must not derive row identity from any of these numbers.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageInstance {
    pub placement_id: PlacementId,
    pub source_id: SourceId,
    pub source_rect: Rect,
    pub destination: Rect,
    pub clip: Rect,
    /// Protocol z-index as supplied by the placement. Composition order comes from the layer's
    /// policy-aware z, not from this raw value.
    pub z_index: i32,
    pub policy: CompositePolicy,
}

/// Cursor rectangle in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CursorInstance {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
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
