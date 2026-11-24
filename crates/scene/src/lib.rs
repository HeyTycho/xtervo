//! Backend-independent terminal draw scene.
//!
//! No wgpu, Skia or Slint types are allowed here. A [`Scene`] is an immutable description built
//! from [`xtervo_graphics::GraphicsManager`] plus a row resolver, or directly from a
//! [`xtervo_core::Frame`] through [`SceneBuilder::from_frame`]. Renderers consume it and own
//! all GPU resources.
//!
//! Selection is represented as cell attributes in [`TextBatch`], never as a top overlay. The
//! [`Scene`] also retains the source selection/search spans and cursor state for provenance.
//!
//! # Features
//!
//! `frame` (default) enables the [`xtervo_core::Frame`] adapter. Disable default features to
//! build the pure graphics/scene contract without a terminal-core dependency:
//!
//! ```text
//! cargo check -p xtervo-scene --no-default-features
//! ```

#![forbid(unsafe_code)]

mod builder;
mod damage;
#[cfg(feature = "frame")]
mod frame;
mod scene;
mod text;

pub use builder::SceneBuilder;
pub use damage::{DamageRect, DamageRegion};
#[cfg(feature = "frame")]
pub use frame::{scene_from_frame, FrameSceneBuilder, SelectionStyle};
pub use scene::{
    BackgroundInstance, CursorInstance, CursorShape, CursorState, DrawItem, ImageInstance,
    PhysicalSize, Scene, SceneLayer, SelectionSpan, Viewport, BACKGROUND_LAYER_Z, CURSOR_LAYER_Z,
    TEXT_LAYER_Z,
};
pub use text::{TextAttributes, TextBatch, TextCell};
