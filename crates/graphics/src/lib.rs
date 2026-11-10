//! Backend-independent graphics resources and placements.
//!
//! This crate must never depend on wgpu, Slint or a terminal parser.

use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(pub u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlacementId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RowId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    pub fn intersection(&self, other: Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        Rect::new(x, y, (right - x).max(0.0), (bottom - y).max(0.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Sixel,
    Iterm2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositePolicy {
    /// Ordinary terminal images: cell background, image, glyphs.
    BelowText,
    /// Kitty `C=1` / positive z-index placement: image above glyphs.
    AboveText,
    /// Protocol declares the image owns its cells (placeholder/ImageCell).
    CellOwned,
}

#[derive(Clone, Debug)]
pub struct ImageResource {
    pub id: ImageId,
    pub source_id: SourceId,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    pub generation: u64,
}

#[derive(Clone, Debug)]
pub struct ImagePlacement {
    pub id: PlacementId,
    pub image_id: ImageId,
    pub source_id: SourceId,
    pub row_id: RowId,
    pub col: u16,
    pub source_rect: Option<Rect>,
    pub cells_w: u16,
    pub cells_h: u16,
    pub pixel_offset: (i32, i32),
    pub z_index: i32,
    pub protocol: ImageProtocol,
    pub policy: CompositePolicy,
}

#[derive(Default)]
pub struct GraphicsManager {
    resources: HashMap<ImageId, Arc<ImageResource>>,
    placements: HashMap<PlacementId, ImagePlacement>,
}

impl GraphicsManager {
    pub fn insert_resource(&mut self, resource: ImageResource) -> SourceId {
        let source = resource.source_id;
        self.resources.insert(resource.id, Arc::new(resource));
        source
    }

    pub fn resource(&self, id: ImageId) -> Option<&Arc<ImageResource>> {
        self.resources.get(&id)
    }

    pub fn remove_resource(&mut self, id: ImageId) -> Option<Arc<ImageResource>> {
        self.resources.remove(&id)
    }

    pub fn insert_placement(&mut self, placement: ImagePlacement) {
        self.placements.insert(placement.id, placement);
    }

    pub fn remove_placement(&mut self, id: PlacementId) -> Option<ImagePlacement> {
        self.placements.remove(&id)
    }

    pub fn placements(&self) -> impl Iterator<Item = &ImagePlacement> {
        self.placements.values()
    }

    /// Drop placements whose row identity is no longer present in the screen.
    /// The caller supplies the authoritative set of live row ids.
    pub fn retain_rows(&mut self, live: impl Fn(RowId) -> bool) {
        self.placements.retain(|_, placement| live(placement.row_id));
    }

    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_is_dropped_when_row_is_gone() {
        let mut manager = GraphicsManager::default();
        let resource = ImageResource {
            id: ImageId(1),
            source_id: SourceId(10),
            width: 1,
            height: 1,
            rgba: Arc::from(vec![255, 0, 0, 255]),
            generation: 1,
        };
        manager.insert_resource(resource);
        manager.insert_placement(ImagePlacement {
            id: PlacementId(2),
            image_id: ImageId(1),
            source_id: SourceId(10),
            row_id: RowId(7),
            col: 0,
            source_rect: None,
            cells_w: 1,
            cells_h: 1,
            pixel_offset: (0, 0),
            z_index: 0,
            protocol: ImageProtocol::Kitty,
            policy: CompositePolicy::AboveText,
        });
        manager.retain_rows(|row| row == RowId(8));
        assert_eq!(manager.placement_count(), 0);
    }
}
