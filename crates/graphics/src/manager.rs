//! Reference-aware store of image resources and row-anchored placements.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use crate::{GraphicsError, ImageId, ImagePlacement, ImageResource, PlacementId, RowId, SourceId};

/// Owns image resources and their placements for one terminal screen.
///
/// # Reference-aware deletion
///
/// A resource is immutable and may be shared by many placements. Removing a placement detaches
/// only that placement; when its image no longer has any placements, the resource is released in
/// the same operation. This bounds CPU image memory by the currently visible ownership graph.
///
/// Explicit [`Self::remove_resource`] force-deletes the resource and every placement that points
/// at it, which is the mapping for the protocol `DeleteImage` command. Callers that want to
/// inspect first can use [`Self::placements_for_image`].
///
/// A newly inserted resource that has never been placed is retained explicitly until it is
/// placed, force-deleted, or [`Self::delete_all`] is called. That covers the normal
/// transmit-then-place command sequence without introducing a permanent image cache.
///
/// # Source and generation invariants
///
/// * A live source id always names the same bytes and generation.
/// * Replacing an image id with different bytes requires a new source id and a strictly newer
///   generation.
/// * Placements bound to the old source are dropped atomically when a resource is replaced.
/// * A placement's `source_id` must match its resource's `source_id`.
///
/// Only live ids are indexed. The manager never stores deleted ids or generation history, so a
/// long session cannot grow an unbounded cache here.
#[derive(Debug, Default)]
pub struct GraphicsManager {
    resources: HashMap<ImageId, Arc<ImageResource>>,
    source_images: HashMap<SourceId, ImageId>,
    placements: HashMap<PlacementId, ImagePlacement>,
    placements_by_image: HashMap<ImageId, BTreeSet<PlacementId>>,
}

impl GraphicsManager {
    /// Creates an empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a resource, returning its immutable source id.
    ///
    /// Re-inserting the exact same image/source/generation is idempotent. Replacing an image
    /// with a newer generation drops placements that referenced the previous source; their row
    /// anchors no longer describe the current pixels.
    pub fn insert_resource(&mut self, resource: ImageResource) -> Result<SourceId, GraphicsError> {
        resource.validate()?;

        let image_id = resource.id;
        let source_id = resource.source_id;
        let generation = resource.generation;

        if let Some(existing) = self.resources.get(&image_id) {
            if existing.source_id == source_id {
                if existing.generation != generation {
                    return if generation < existing.generation {
                        Err(GraphicsError::StaleGeneration {
                            image_id,
                            current: existing.generation,
                            incoming: generation,
                        })
                    } else {
                        Err(GraphicsError::SourceConflict {
                            image_id,
                            source_id,
                            reason: "generation changed without a new source id",
                        })
                    };
                }
                if existing.width != resource.width
                    || existing.height != resource.height
                    || existing.rgba.as_ref() != resource.rgba.as_ref()
                {
                    return Err(GraphicsError::SourceConflict {
                        image_id,
                        source_id,
                        reason: "a source id must always identify the same pixels",
                    });
                }
                return Ok(source_id);
            }

            if generation <= existing.generation {
                return Err(GraphicsError::StaleGeneration {
                    image_id,
                    current: existing.generation,
                    incoming: generation,
                });
            }
            if let Some(owner) = self.source_images.get(&source_id) {
                if *owner != image_id {
                    return Err(GraphicsError::SourceInUse {
                        source_id,
                        image_id: *owner,
                    });
                }
            }

            // The old source is no longer valid for this image. Drop stale placements before
            // exposing the replacement resource, never after.
            self.discard_placements_for_image(image_id);
            let old = self
                .resources
                .remove(&image_id)
                .expect("resource was observed above");
            self.source_images.remove(&old.source_id);
        } else if let Some(owner) = self.source_images.get(&source_id) {
            return Err(GraphicsError::SourceInUse {
                source_id,
                image_id: *owner,
            });
        }

        self.source_images.insert(source_id, image_id);
        self.resources.insert(image_id, Arc::new(resource));
        Ok(source_id)
    }

    /// Looks up a resource by protocol image id.
    pub fn resource(&self, id: ImageId) -> Option<&Arc<ImageResource>> {
        self.resources.get(&id)
    }

    /// Looks up a resource by immutable pixel-data id.
    pub fn resource_by_source(&self, source_id: SourceId) -> Option<&Arc<ImageResource>> {
        self.source_images
            .get(&source_id)
            .and_then(|image_id| self.resources.get(image_id))
    }

    /// Current generation for an image id.
    pub fn resource_generation(&self, id: ImageId) -> Option<u64> {
        self.resources.get(&id).map(|resource| resource.generation)
    }

    /// Iterates live resources. Order is unspecified.
    pub fn resources(&self) -> impl Iterator<Item = &Arc<ImageResource>> {
        self.resources.values()
    }

    /// Number of live resources.
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Whether a source id is currently live.
    pub fn source_in_use(&self, source_id: SourceId) -> bool {
        self.source_images.contains_key(&source_id)
    }

    /// Inserts one placement after validating its resource and source-rectangle invariants.
    ///
    /// Duplicate placement ids are rejected instead of silently replacing an existing anchor.
    pub fn insert_placement(&mut self, placement: ImagePlacement) -> Result<(), GraphicsError> {
        if self.placements.contains_key(&placement.id) {
            return Err(GraphicsError::DuplicatePlacement(placement.id));
        }

        let resource = self
            .resources
            .get(&placement.image_id)
            .ok_or(GraphicsError::UnknownImage(placement.image_id))?;

        if resource.source_id != placement.source_id {
            return Err(GraphicsError::SourceMismatch {
                placement_id: placement.id,
                image_id: placement.image_id,
                expected: resource.source_id,
                actual: placement.source_id,
            });
        }

        if placement.cells_w == 0 || placement.cells_h == 0 {
            return Err(GraphicsError::InvalidPlacement(
                "cells_w and cells_h must be non-zero",
            ));
        }

        if let Some(source_rect) = placement.source_rect {
            if !source_rect.is_finite() || source_rect.is_empty() {
                return Err(GraphicsError::InvalidPlacement(
                    "source_rect must be finite and non-empty",
                ));
            }
            if source_rect.x < 0.0
                || source_rect.y < 0.0
                || source_rect.right() > resource.width as f32
                || source_rect.bottom() > resource.height as f32
            {
                return Err(GraphicsError::SourceRectOutOfBounds {
                    placement_id: placement.id,
                    image_id: placement.image_id,
                });
            }
        }

        self.placements_by_image
            .entry(placement.image_id)
            .or_default()
            .insert(placement.id);
        self.placements.insert(placement.id, placement);
        Ok(())
    }

    /// Looks up one placement by id.
    pub fn placement(&self, id: PlacementId) -> Option<&ImagePlacement> {
        self.placements.get(&id)
    }

    /// Iterates every live placement. Order is unspecified; use [`Self::placements_in_z_order`]
    /// for deterministic composition order.
    pub fn placements(&self) -> impl Iterator<Item = &ImagePlacement> {
        self.placements.values()
    }

    /// Placements for one image, ordered by placement id.
    pub fn placements_for_image(
        &self,
        image_id: ImageId,
    ) -> impl Iterator<Item = &ImagePlacement> + '_ {
        self.placements_by_image
            .get(&image_id)
            .into_iter()
            .flat_map(|ids| ids.iter())
            .filter_map(|id| self.placements.get(id))
    }

    /// Number of live placements.
    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    /// Number of live placements for one image.
    pub fn placement_count_for_image(&self, image_id: ImageId) -> usize {
        self.placements_by_image
            .get(&image_id)
            .map_or(0, BTreeSet::len)
    }

    /// All placements in deterministic composition order.
    ///
    /// Ordering is `(policy-aware layer z, placement id)`. The id tiebreak is required because
    /// protocol z-indices are not unique and equal-z images must not flicker between frames.
    pub fn placements_in_z_order(&self) -> Vec<&ImagePlacement> {
        let mut placements: Vec<_> = self.placements.values().collect();
        placements.sort_by(|a, b| {
            a.composition_z()
                .cmp(&b.composition_z())
                .then_with(|| a.id.cmp(&b.id))
        });
        placements
    }

    /// Removes one placement.
    ///
    /// If this was the last placement referencing its image, the image resource is released as
    /// well. Returns the removed placement.
    pub fn remove_placement(&mut self, id: PlacementId) -> Option<ImagePlacement> {
        let placement = self.placements.remove(&id)?;
        let image_id = placement.image_id;
        if let Some(ids) = self.placements_by_image.get_mut(&image_id) {
            ids.remove(&id);
            if ids.is_empty() {
                self.placements_by_image.remove(&image_id);
                self.reap_resource(image_id);
            }
        }
        Some(placement)
    }

    /// Force-deletes a resource and all placements that reference it.
    ///
    /// This is the mapping for the protocol `DeleteImage` command. Returns the removed resource
    /// so an owning renderer can invalidate its texture immediately.
    pub fn remove_resource(&mut self, id: ImageId) -> Option<Arc<ImageResource>> {
        self.discard_placements_for_image(id);
        let resource = self.resources.remove(&id)?;
        self.source_images.remove(&resource.source_id);
        Some(resource)
    }

    /// Drops placements whose stable row no longer exists.
    ///
    /// `live` is the authoritative row set from the screen. A missing `RowId` is never guessed
    /// from a row index or scroll offset. Returns the number of dropped placements. Resources
    /// that lose their last placement are released immediately.
    pub fn retain_rows(&mut self, mut live: impl FnMut(RowId) -> bool) -> usize {
        let stale: Vec<_> = self
            .placements
            .values()
            .filter(|placement| !live(placement.row_id))
            .map(|placement| placement.id)
            .collect();
        let removed = stale.len();
        for id in stale {
            self.remove_placement(id);
        }
        removed
    }

    /// Removes every resource and placement.
    pub fn delete_all(&mut self) {
        self.resources.clear();
        self.source_images.clear();
        self.placements.clear();
        self.placements_by_image.clear();
    }

    /// Alias for [`Self::delete_all`], matching the protocol reset vocabulary.
    pub fn clear(&mut self) {
        self.delete_all();
    }

    /// Whether the manager holds no resources or placements.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.placements.is_empty()
    }

    fn discard_placements_for_image(&mut self, image_id: ImageId) -> usize {
        let Some(ids) = self.placements_by_image.remove(&image_id) else {
            return 0;
        };
        let mut removed = 0;
        for id in ids {
            if self.placements.remove(&id).is_some() {
                removed += 1;
            }
        }
        removed
    }

    fn reap_resource(&mut self, image_id: ImageId) -> Option<Arc<ImageResource>> {
        if self
            .placements_by_image
            .get(&image_id)
            .is_some_and(|ids| !ids.is_empty())
        {
            return None;
        }
        let resource = self.resources.remove(&image_id)?;
        self.source_images.remove(&resource.source_id);
        Some(resource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompositePolicy, ImageProtocol, Rect};

    fn resource(id: u32, source: u64, generation: u64) -> ImageResource {
        ImageResource {
            id: ImageId(id),
            source_id: SourceId(source),
            width: 2,
            height: 1,
            rgba: Arc::from(vec![255, 0, 0, 255, 0, 255, 0, 255]),
            generation,
        }
    }

    fn placement(id: u64, row: u64) -> ImagePlacement {
        ImagePlacement {
            id: PlacementId(id),
            image_id: ImageId(1),
            source_id: SourceId(10),
            row_id: RowId(row),
            col: 0,
            source_rect: None,
            cells_w: 1,
            cells_h: 1,
            pixel_offset: (0, 0),
            z_index: 0,
            protocol: ImageProtocol::Kitty,
            policy: CompositePolicy::BelowText,
        }
    }

    fn manager_with_resource() -> GraphicsManager {
        let mut manager = GraphicsManager::new();
        manager
            .insert_resource(resource(1, 10, 1))
            .expect("test resource is valid");
        manager
    }

    #[test]
    fn same_source_supports_multiple_placements() {
        let mut manager = manager_with_resource();
        manager
            .insert_placement(placement(1, 7))
            .expect("first placement");
        manager
            .insert_placement(placement(2, 8))
            .expect("second placement");

        assert_eq!(manager.resource_count(), 1);
        assert_eq!(manager.placement_count(), 2);
        let ids: Vec<_> = manager
            .placements_for_image(ImageId(1))
            .map(|placement| placement.id)
            .collect();
        assert_eq!(ids, vec![PlacementId(1), PlacementId(2)]);
        assert_eq!(
            manager
                .resource_by_source(SourceId(10))
                .map(|resource| resource.id),
            Some(ImageId(1))
        );
    }

    #[test]
    fn shared_resource_survives_until_last_placement_is_removed() {
        let mut manager = manager_with_resource();
        manager
            .insert_placement(placement(1, 7))
            .expect("first placement");
        manager
            .insert_placement(placement(2, 7))
            .expect("second placement");

        assert!(manager.remove_placement(PlacementId(1)).is_some());
        assert_eq!(manager.placement_count(), 1);
        assert_eq!(manager.resource_count(), 1);
        assert!(manager.resource(ImageId(1)).is_some());

        assert!(manager.remove_placement(PlacementId(2)).is_some());
        assert_eq!(manager.placement_count(), 0);
        assert_eq!(manager.resource_count(), 0);
        assert!(manager.resource(ImageId(1)).is_none());
        assert!(!manager.source_in_use(SourceId(10)));
    }

    #[test]
    fn retain_rows_drops_missing_rows_and_keeps_shared_resource_until_last_ref() {
        let mut manager = manager_with_resource();
        manager
            .insert_placement(placement(1, 7))
            .expect("stale placement");
        manager
            .insert_placement(placement(2, 8))
            .expect("live placement");

        assert_eq!(manager.retain_rows(|row| row == RowId(8)), 1);
        assert!(manager.placement(PlacementId(1)).is_none());
        assert!(manager.placement(PlacementId(2)).is_some());
        assert_eq!(manager.resource_count(), 1);

        assert!(manager.remove_placement(PlacementId(2)).is_some());
        assert_eq!(manager.resource_count(), 0);
    }

    #[test]
    fn placement_and_image_lookups_are_available() {
        let mut manager = manager_with_resource();
        manager
            .insert_placement(placement(7, 3))
            .expect("valid placement");

        assert_eq!(
            manager.placement(PlacementId(7)).map(|p| p.row_id),
            Some(RowId(3))
        );
        assert_eq!(manager.placement_count_for_image(ImageId(1)), 1);
        assert_eq!(
            manager.resource_generation(ImageId(1)),
            Some(1),
            "generation is exposed for renderer invalidation checks"
        );
    }

    #[test]
    fn z_order_is_policy_aware_and_stable_for_equal_layers() {
        let mut manager = manager_with_resource();

        let mut above_five = placement(20, 0);
        above_five.z_index = 5;
        above_five.policy = CompositePolicy::AboveText;

        let mut below_minus_four = placement(10, 0);
        below_minus_four.z_index = -4;
        below_minus_four.policy = CompositePolicy::BelowText;

        let mut below_three = placement(11, 0);
        below_three.z_index = 3;
        below_three.policy = CompositePolicy::BelowText;

        let mut above_zero = placement(12, 0);
        above_zero.z_index = 0;
        above_zero.policy = CompositePolicy::AboveText;

        let mut above_one = placement(13, 0);
        above_one.z_index = 1;
        above_one.policy = CompositePolicy::AboveText;

        for placement in [
            above_five,
            below_minus_four,
            below_three,
            above_zero,
            above_one,
        ] {
            manager
                .insert_placement(placement)
                .expect("valid placement");
        }

        let ids: Vec<_> = manager
            .placements_in_z_order()
            .into_iter()
            .map(|placement| placement.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                PlacementId(10),
                PlacementId(11),
                PlacementId(12),
                PlacementId(13),
                PlacementId(20)
            ]
        );

        assert_eq!(CompositePolicy::BelowText.composition_z(0), -1);
        assert_eq!(CompositePolicy::AboveText.composition_z(-3), 1);
        assert_eq!(CompositePolicy::CellOwned.composition_z(-3), 1);
    }

    #[test]
    fn placement_with_mismatched_source_is_rejected() {
        let mut manager = manager_with_resource();
        let mut bad = placement(1, 7);
        bad.source_id = SourceId(99);

        assert_eq!(
            manager.insert_placement(bad),
            Err(GraphicsError::SourceMismatch {
                placement_id: PlacementId(1),
                image_id: ImageId(1),
                expected: SourceId(10),
                actual: SourceId(99),
            })
        );
        assert_eq!(manager.placement_count(), 0);
    }

    #[test]
    fn stale_generation_is_rejected_without_mutating_the_manager() {
        let mut manager = GraphicsManager::new();
        manager
            .insert_resource(resource(1, 10, 2))
            .expect("current generation");

        assert_eq!(
            manager.insert_resource(resource(1, 11, 1)),
            Err(GraphicsError::StaleGeneration {
                image_id: ImageId(1),
                current: 2,
                incoming: 1,
            })
        );
        assert_eq!(
            manager.resource(ImageId(1)).map(|r| r.source_id),
            Some(SourceId(10))
        );

        manager
            .insert_resource(resource(1, 11, 3))
            .expect("newer generation replaces the image");
        assert_eq!(
            manager.resource(ImageId(1)).map(|r| r.source_id),
            Some(SourceId(11))
        );
        assert!(!manager.source_in_use(SourceId(10)));
    }

    #[test]
    fn replacing_a_source_drops_placements_bound_to_the_old_pixels() {
        let mut manager = manager_with_resource();
        manager
            .insert_placement(placement(1, 7))
            .expect("old placement");

        manager
            .insert_resource(resource(1, 11, 2))
            .expect("new source");
        assert_eq!(manager.placement_count(), 0);
        assert_eq!(
            manager.resource(ImageId(1)).map(|r| r.source_id),
            Some(SourceId(11))
        );
    }

    #[test]
    fn invalid_pixel_buffer_is_rejected() {
        let mut manager = GraphicsManager::new();
        let mut bad = resource(1, 10, 1);
        bad.rgba = Arc::from(vec![0, 0, 0]);

        assert!(matches!(
            manager.insert_resource(bad),
            Err(GraphicsError::InvalidResource(_))
        ));
        assert_eq!(manager.resource_count(), 0);
    }

    #[test]
    fn out_of_bounds_source_rect_and_empty_cells_are_rejected() {
        let mut manager = manager_with_resource();

        let mut out_of_bounds = placement(1, 0);
        out_of_bounds.source_rect = Some(Rect::new(1.0, 0.0, 2.0, 1.0));
        assert!(matches!(
            manager.insert_placement(out_of_bounds),
            Err(GraphicsError::SourceRectOutOfBounds { .. })
        ));

        let mut empty_cells = placement(2, 0);
        empty_cells.cells_w = 0;
        assert!(matches!(
            manager.insert_placement(empty_cells),
            Err(GraphicsError::InvalidPlacement(_))
        ));
    }

    #[test]
    fn force_delete_removes_all_shared_placements() {
        let mut manager = manager_with_resource();
        manager
            .insert_placement(placement(1, 1))
            .expect("first placement");
        manager
            .insert_placement(placement(2, 2))
            .expect("second placement");

        let removed = manager
            .remove_resource(ImageId(1))
            .expect("resource is present");
        assert_eq!(removed.source_id, SourceId(10));
        assert_eq!(manager.placement_count(), 0);
        assert_eq!(manager.resource_count(), 0);
    }
}
