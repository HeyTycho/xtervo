// SPDX-License-Identifier: GPL-3.0-only
//! Resource accounting interface.
//!
//! `ResourceTracker` is the seam between resource owners (graphics manager,
//! glyph atlas, GPU backend) and diagnostics. The default implementation is
//! atomic and can be fed incrementally; a GPU backend that already knows its
//! own totals can instead implement `ResourceTracker` directly.
//!
//! This is accounting only. Nothing here reads or waits for the GPU, and no
//! resource is created or destroyed by this module.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Count plus byte estimate for one resource class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceCount {
    pub count: u64,
    pub bytes: u64,
}

impl ResourceCount {
    pub const fn new(count: u64, bytes: u64) -> Self {
        Self { count, bytes }
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0 && self.bytes == 0
    }
}

/// Point-in-time resource accounting for one session/backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    /// Decoded CPU image resources (`ImageResource`).
    pub image_resources: ResourceCount,
    /// Visible image placements. Usually no separate pixel bytes; kept for
    /// symmetry because the interface is counts+bytes everywhere.
    pub image_placements: ResourceCount,
    /// Glyph cache / atlas entries and approximate bytes.
    pub glyph_atlas: ResourceCount,
    /// GPU textures owned by the renderer and approximate bytes.
    pub gpu_textures: ResourceCount,
}

impl ResourceSnapshot {
    pub fn total_bytes(&self) -> u64 {
        self.image_resources
            .bytes
            .saturating_add(self.image_placements.bytes)
            .saturating_add(self.glyph_atlas.bytes)
            .saturating_add(self.gpu_textures.bytes)
    }

    /// Additive `tervo.diag.v1` resource object.
    pub fn to_json(&self) -> Value {
        json!({
            "image_resources": {"count": self.image_resources.count, "bytes": self.image_resources.bytes},
            "image_placements": {"count": self.image_placements.count, "bytes": self.image_placements.bytes},
            "glyph_atlas": {"count": self.glyph_atlas.count, "bytes": self.glyph_atlas.bytes},
            "gpu_textures": {"count": self.gpu_textures.count, "bytes": self.gpu_textures.bytes},
            "total_bytes_estimate": self.total_bytes(),
        })
    }
}

/// Read-only resource snapshot provider.
///
/// Implement this directly when a backend already maintains authoritative
/// totals; use [`AtomicResourceTracker`] when feeding values incrementally.
pub trait ResourceTracker: Send + Sync {
    fn snapshot(&self) -> ResourceSnapshot;

    /// Default JSON view; implementations rarely need to override this.
    fn snapshot_json(&self) -> Value {
        self.snapshot().to_json()
    }
}

#[derive(Debug, Default)]
struct AtomicCount {
    count: AtomicU64,
    bytes: AtomicU64,
}

impl AtomicCount {
    fn set(&self, count: u64, bytes: u64) {
        self.count.store(count, Ordering::Relaxed);
        self.bytes.store(bytes, Ordering::Relaxed);
    }

    fn get(&self) -> ResourceCount {
        ResourceCount {
            count: self.count.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

/// Default resource tracker: plain relaxed atomics, no locks, no allocation
/// on update. CPU/GPU backends can call the setters whenever their accounting
/// changes; snapshots are read-only and cheap.
#[derive(Debug, Default)]
pub struct AtomicResourceTracker {
    image_resources: AtomicCount,
    image_placements: AtomicCount,
    glyph_atlas: AtomicCount,
    gpu_textures: AtomicCount,
}

impl AtomicResourceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_image_resources(&self, count: u64, bytes: u64) {
        self.image_resources.set(count, bytes);
    }

    pub fn set_image_placements(&self, count: u64, bytes: u64) {
        self.image_placements.set(count, bytes);
    }

    pub fn set_glyph_atlas(&self, count: u64, bytes: u64) {
        self.glyph_atlas.set(count, bytes);
    }

    pub fn set_gpu_textures(&self, count: u64, bytes: u64) {
        self.gpu_textures.set(count, bytes);
    }
}

impl ResourceTracker for AtomicResourceTracker {
    fn snapshot(&self) -> ResourceSnapshot {
        ResourceSnapshot {
            image_resources: self.image_resources.get(),
            image_placements: self.image_placements.get(),
            glyph_atlas: self.glyph_atlas.get(),
            gpu_textures: self.gpu_textures.get(),
        }
    }
}

/// `width * height * 4` for straight RGBA8, saturating instead of overflowing.
pub const fn rgba_bytes(width: u32, height: u32) -> u64 {
    (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tracker_snapshots_zeroes() {
        let tracker = AtomicResourceTracker::new();
        let snapshot = tracker.snapshot();
        assert!(snapshot.image_resources.is_empty());
        assert!(snapshot.image_placements.is_empty());
        assert!(snapshot.glyph_atlas.is_empty());
        assert!(snapshot.gpu_textures.is_empty());
        assert_eq!(snapshot.total_bytes(), 0);
    }

    #[test]
    fn tracker_reports_all_resource_classes() {
        let tracker = AtomicResourceTracker::new();
        tracker.set_image_resources(2, 1024);
        tracker.set_image_placements(5, 0);
        tracker.set_glyph_atlas(100, 2048);
        tracker.set_gpu_textures(3, 4096);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.image_resources, ResourceCount::new(2, 1024));
        assert_eq!(snapshot.image_placements, ResourceCount::new(5, 0));
        assert_eq!(snapshot.glyph_atlas, ResourceCount::new(100, 2048));
        assert_eq!(snapshot.gpu_textures, ResourceCount::new(3, 4096));
        assert_eq!(snapshot.total_bytes(), 7_168);

        let value = tracker.snapshot_json();
        assert_eq!(value["gpu_textures"]["count"], 3);
        assert_eq!(value["image_resources"]["bytes"], 1024);
        assert_eq!(value["total_bytes_estimate"], 7_168);
    }

    #[test]
    fn rgba_bytes_saturates() {
        assert_eq!(rgba_bytes(2, 3), 24);
        assert_eq!(rgba_bytes(u32::MAX, u32::MAX), u64::MAX);
    }

    #[test]
    fn trait_object_is_usable_by_backends() {
        let tracker = AtomicResourceTracker::new();
        tracker.set_gpu_textures(1, 7);
        let dynamic: &dyn ResourceTracker = &tracker;
        assert_eq!(dynamic.snapshot().gpu_textures.bytes, 7);
    }
}
