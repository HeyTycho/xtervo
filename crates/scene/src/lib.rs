//! Backend-independent terminal draw scene.
//!
//! No wgpu, Skia or Slint types are allowed here.

use tervo_graphics::{CompositePolicy, PlacementId, Rect, SourceId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Viewport {
    pub cell_w: f32,
    pub cell_h: f32,
    pub cols: usize,
    pub rows: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DamageRegion {
    pub rects: Vec<DamageRect>,
    pub full: bool,
}

impl DamageRegion {
    pub fn full() -> Self {
        Self { rects: Vec::new(), full: true }
    }

    pub fn push(&mut self, rect: DamageRect) {
        if rect.w != 0 && rect.h != 0 {
            self.rects.push(rect);
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub viewport: Viewport,
    pub revision: u64,
    pub damage: DamageRegion,
    pub layers: Vec<SceneLayer>,
}

#[derive(Clone, Debug, Default)]
pub struct SceneLayer {
    pub z_index: i32,
    pub items: Vec<DrawItem>,
}

#[derive(Clone, Debug)]
pub enum DrawItem {
    Background(BackgroundInstance),
    Text(TextBatch),
    Image(ImageInstance),
    Cursor(CursorInstance),
}

#[derive(Clone, Debug, Default)]
pub struct BackgroundInstance {
    pub color: u32,
}

#[derive(Clone, Debug, Default)]
pub struct TextBatch {
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub struct ImageInstance {
    pub placement_id: PlacementId,
    pub source_id: SourceId,
    pub source_rect: Rect,
    pub destination: Rect,
    pub clip: Rect,
    pub z_index: i32,
    pub policy: CompositePolicy,
}

#[derive(Clone, Debug, Default)]
pub struct CursorInstance {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}
