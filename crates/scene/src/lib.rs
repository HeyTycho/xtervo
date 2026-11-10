//! Backend-independent terminal draw scene.
//!
//! No wgpu, Skia or Slint types are allowed here. A [`Scene`] is an immutable description built
//! from [`tervo_graphics::GraphicsManager`] plus a row resolver; renderers consume it and own
//! all GPU resources.
//!
//! Selection is represented as cell attributes in [`TextBatch`], never as a top overlay.

#![forbid(unsafe_code)]

mod builder;
mod damage;
mod scene;
mod text;

pub use builder::SceneBuilder;
pub use damage::{DamageRect, DamageRegion};
pub use scene::{
    BackgroundInstance, CursorInstance, DrawItem, ImageInstance, PhysicalSize, Scene, SceneLayer,
    Viewport, BACKGROUND_LAYER_Z, CURSOR_LAYER_Z, TEXT_LAYER_Z,
};
pub use text::{TextAttributes, TextBatch, TextCell};
