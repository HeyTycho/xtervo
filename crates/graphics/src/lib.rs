//! Backend-independent graphics resources and placements.
//!
//! The crate owns the CPU-side image model shared by every graphics protocol. It deliberately
//! has no GPU, UI or terminal-parser dependency:
//!
//! * [`ImageResource`] is immutable pixel data identified by [`SourceId`].
//! * [`ImagePlacement`] anchors that data to a stable [`RowId`].
//! * [`GraphicsManager`] owns both and enforces the source/generation invariants.
//!
//! # Lifetime model
//!
//! A resource may be referenced by any number of placements. Deleting a placement is
//! reference-aware: the resource is released once the last placement referencing it is
//! removed. A resource that has never been placed is owned explicitly and stays alive until
//! [`GraphicsManager::remove_resource`] or [`GraphicsManager::delete_all`]. Force-deleting a
//! resource also deletes its placements, so a placement can never outlive the resource it
//! points at.
//!
//! Only live ids are indexed. There is intentionally no "seen ids" or generation history
//! cache: memory is bounded by the current resource and placement counts.

#![forbid(unsafe_code)]

mod error;
mod id;
mod manager;
mod rect;
mod resource;

pub use error::GraphicsError;
pub use id::{ImageId, PlacementId, RowId, SourceId};
pub use manager::GraphicsManager;
pub use rect::Rect;
pub use resource::{
    CompositePolicy, ImagePlacement, ImageProtocol, ImageResource, ABOVE_TEXT_LAYER_MIN_Z,
    BELOW_TEXT_LAYER_MAX_Z, TEXT_LAYER_Z,
};
