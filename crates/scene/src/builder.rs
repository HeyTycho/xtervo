//! Scene construction from graphics ownership data.

use std::collections::BTreeMap;

use xtervo_graphics::{GraphicsManager, ImagePlacement, Rect, RowId};

use crate::{
    BackgroundInstance, CursorInstance, DamageRegion, DrawItem, ImageInstance, Scene, SceneLayer,
    TextBatch, Viewport, BACKGROUND_LAYER_Z, CURSOR_LAYER_Z, TEXT_LAYER_Z,
};

/// Builds an image layer set from [`GraphicsManager`] state.
///
/// This is intentionally the only ownership knowledge the scene needs: row identity comes from
/// the supplied resolver, pixel identity comes from the manager, and no terminal parser or GPU
/// type appears in the result.
///
/// A placement whose row is absent from the resolver is omitted from the scene. The resolver is
/// authoritative; the builder never guesses a row from a stale index. Call
/// [`GraphicsManager::retain_rows`] with the same predicate to prune the manager itself.
///
/// The text layer at [`TEXT_LAYER_Z`] is always emitted so selection (a [`TextBatch`] concern)
/// has a stable home. Background and cursor layers are optional and only added when requested.
pub struct SceneBuilder<'a, R> {
    graphics: &'a GraphicsManager,
    viewport: Viewport,
    resolve_row: R,
    revision: u64,
    damage: DamageRegion,
    text: TextBatch,
    background: Option<BackgroundInstance>,
    cursor: Option<CursorInstance>,
}

impl<'a, R> SceneBuilder<'a, R>
where
    R: Fn(RowId) -> Option<usize>,
{
    /// Creates a builder for a viewport and row resolver.
    ///
    /// The resolver maps a stable [`RowId`] to a viewport row index (`0..viewport.rows`).
    pub fn new(graphics: &'a GraphicsManager, viewport: Viewport, resolve_row: R) -> Self {
        Self {
            graphics,
            viewport,
            resolve_row,
            revision: 0,
            damage: DamageRegion::default(),
            text: TextBatch::default(),
            background: None,
            cursor: None,
        }
    }

    /// Sets the scene revision.
    pub fn revision(mut self, revision: u64) -> Self {
        self.revision = revision;
        self
    }

    /// Sets the damage to include in the scene.
    pub fn damage(mut self, damage: DamageRegion) -> Self {
        self.damage = damage;
        self
    }

    /// Sets the text batch (including resolved cell selection colors).
    pub fn text(mut self, text: TextBatch) -> Self {
        self.text = text;
        self
    }

    /// Adds a background clear item.
    pub fn background(mut self, background: BackgroundInstance) -> Self {
        self.background = Some(background);
        self
    }

    /// Adds a cursor item.
    pub fn cursor(mut self, cursor: CursorInstance) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// Builds the scene. The builder can be reused because it does not consume manager state.
    pub fn build(&self) -> Scene {
        let viewport_rect = self.viewport.pixel_bounds();
        let mut layers: BTreeMap<i32, Vec<DrawItem>> = BTreeMap::new();

        if let Some(background) = self.background {
            layers
                .entry(BACKGROUND_LAYER_Z)
                .or_default()
                .push(DrawItem::Background(background));
        }

        // Selection lives in TextBatch cells, so the text layer always exists.
        layers
            .entry(TEXT_LAYER_Z)
            .or_default()
            .push(DrawItem::Text(self.text.clone()));

        // placements_in_z_order is deterministic and policy-aware. Stable placement-id ties
        // guarantee that equal z-indices do not flicker between frames.
        for placement in self.graphics.placements_in_z_order() {
            if let Some((layer_z, image)) = self.image_instance(placement, viewport_rect) {
                layers
                    .entry(layer_z)
                    .or_default()
                    .push(DrawItem::Image(image));
            }
        }

        let cursor_state = self.cursor.map(|cursor| crate::CursorState {
            row: cursor.row,
            col: cursor.col,
            shape: cursor.shape,
            visible: true,
        });
        if let Some(cursor) = self.cursor {
            layers
                .entry(CURSOR_LAYER_Z)
                .or_default()
                .push(DrawItem::Cursor(cursor));
        }

        Scene {
            viewport: self.viewport,
            revision: self.revision,
            damage: self.damage.clone(),
            selection: None,
            search: None,
            cursor: cursor_state,
            layers: layers
                .into_iter()
                .map(|(z_index, items)| SceneLayer { z_index, items })
                .collect(),
        }
    }

    fn image_instance(
        &self,
        placement: &ImagePlacement,
        viewport_rect: Rect,
    ) -> Option<(i32, ImageInstance)> {
        let row = (self.resolve_row)(placement.row_id)?;
        if row >= self.viewport.rows {
            return None;
        }

        let resource = self.graphics.resource(placement.image_id)?;
        if resource.source_id != placement.source_id {
            return None;
        }

        let source_bounds = resource.source_bounds();
        let source_rect = placement
            .source_rect
            .map_or(source_bounds, |rect| rect.intersection(source_bounds));
        if source_rect.is_empty() {
            return None;
        }

        let destination = Rect::new(
            placement.col as f32 * self.viewport.cell_w.max(0.0) + placement.pixel_offset.0 as f32,
            row as f32 * self.viewport.cell_h.max(0.0) + placement.pixel_offset.1 as f32,
            placement.cells_w as f32 * self.viewport.cell_w.max(0.0),
            placement.cells_h as f32 * self.viewport.cell_h.max(0.0),
        );
        if destination.is_empty() {
            return None;
        }

        let clip = destination.intersection(viewport_rect);
        if clip.is_empty() {
            return None;
        }

        let layer_z = placement.composition_z();
        Some((
            layer_z,
            ImageInstance {
                placement_id: placement.id,
                source_id: placement.source_id,
                source_rect,
                source_size: (resource.width, resource.height),
                destination,
                clip,
                cells_w: placement.cells_w,
                cells_h: placement.cells_h,
                row: row.min(i32::MAX as usize) as i32,
                col: placement.col,
                row_id: placement.row_id,
                z_index: placement.z_index,
                above_text: placement.is_above_text(),
                policy: placement.policy,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use xtervo_graphics::{
        CompositePolicy, GraphicsManager, ImageId, ImagePlacement, ImageProtocol, PlacementId,
        Rect, RowId, SourceId,
    };

    use super::SceneBuilder;
    use crate::{
        CursorInstance, DrawItem, TextAttributes, TextBatch, TextCell, Viewport,
        BACKGROUND_LAYER_Z, CURSOR_LAYER_Z, TEXT_LAYER_Z,
    };

    fn viewport() -> Viewport {
        Viewport {
            cell_w: 10.0,
            cell_h: 10.0,
            cols: 10,
            rows: 5,
        }
    }

    fn manager() -> GraphicsManager {
        let mut manager = GraphicsManager::new();
        manager
            .insert_resource(xtervo_graphics::ImageResource {
                id: ImageId(1),
                source_id: SourceId(10),
                width: 32,
                height: 16,
                rgba: Arc::from(vec![0; 32 * 16 * 4]),
                generation: 1,
            })
            .expect("valid image");
        manager
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

    #[test]
    fn builder_clips_destination_to_viewport_and_preserves_source_rect() {
        let mut manager = manager();
        let mut placement = placement(1, 7);
        placement.col = 9;
        placement.cells_w = 3;
        placement.source_rect = Some(Rect::new(4.0, 4.0, 20.0, 10.0));
        manager
            .insert_placement(placement)
            .expect("valid placement");

        let scene =
            SceneBuilder::new(&manager, viewport(), |row| (row == RowId(7)).then_some(0)).build();

        let image = scene.images().next().expect("visible image");
        assert_eq!(image.placement_id, PlacementId(1));
        assert_eq!(image.source_id, SourceId(10));
        assert_eq!(image.destination, Rect::new(90.0, 0.0, 30.0, 10.0));
        assert_eq!(image.clip, Rect::new(90.0, 0.0, 10.0, 10.0));
        assert_eq!(image.source_rect, Rect::new(4.0, 4.0, 20.0, 10.0));
        assert_eq!(image.policy, CompositePolicy::BelowText);
        assert_eq!(image.z_index, 0);
    }

    #[test]
    fn builder_uses_full_resource_rect_and_clips_negative_pixel_offset() {
        let mut manager = manager();
        let mut placement = placement(1, 4);
        placement.pixel_offset = (-4, -6);
        manager
            .insert_placement(placement)
            .expect("valid placement");

        let scene = SceneBuilder::new(&manager, viewport(), |_| Some(0)).build();
        let image = scene.images().next().expect("visible image");
        assert_eq!(image.source_rect, Rect::new(0.0, 0.0, 32.0, 16.0));
        assert_eq!(image.destination, Rect::new(-4.0, -6.0, 10.0, 10.0));
        assert_eq!(image.clip, Rect::new(0.0, 0.0, 6.0, 4.0));
    }

    #[test]
    fn missing_row_drops_the_placement_from_the_scene() {
        let mut manager = manager();
        manager
            .insert_placement(placement(1, 7))
            .expect("valid placement");

        let scene = SceneBuilder::new(&manager, viewport(), |_| None).build();
        assert_eq!(scene.image_count(), 0);

        assert_eq!(manager.retain_rows(|_| false), 1);
        assert_eq!(manager.placement_count(), 0);
        assert_eq!(manager.resource_count(), 0);
    }

    #[test]
    fn builder_orders_layers_by_policy_aware_z_and_keeps_text_layer() {
        let mut manager = manager();

        let mut below_zero = placement(1, 0);
        below_zero.z_index = 0;
        below_zero.policy = CompositePolicy::BelowText;

        let mut above_zero = placement(2, 0);
        above_zero.z_index = 0;
        above_zero.policy = CompositePolicy::AboveText;

        let mut above_five = placement(3, 0);
        above_five.z_index = 5;
        above_five.policy = CompositePolicy::AboveText;

        let mut below_minus_three = placement(4, 0);
        below_minus_three.z_index = -3;
        below_minus_three.policy = CompositePolicy::BelowText;

        let mut above_five_tie = placement(5, 0);
        above_five_tie.z_index = 5;
        above_five_tie.policy = CompositePolicy::AboveText;

        for placement in [
            below_zero,
            above_zero,
            above_five,
            below_minus_three,
            above_five_tie,
        ] {
            manager
                .insert_placement(placement)
                .expect("valid placement");
        }

        let scene = SceneBuilder::new(&manager, viewport(), |_| Some(0)).build();
        let layer_zs: Vec<_> = scene.layers.iter().map(|layer| layer.z_index).collect();
        assert_eq!(layer_zs, vec![-3, -1, TEXT_LAYER_Z, 1, 5]);

        let image_ids: Vec<_> = scene.images().map(|image| image.placement_id).collect();
        assert_eq!(
            image_ids,
            vec![
                PlacementId(4),
                PlacementId(1),
                PlacementId(2),
                PlacementId(3),
                PlacementId(5)
            ]
        );
        assert!(scene.layer(TEXT_LAYER_Z).is_some());
    }

    #[test]
    fn selection_is_text_batch_data_not_an_overlay_item() {
        let manager = manager();
        let mut text = TextBatch::new(11);
        text.push(TextCell {
            row: 0,
            col: 2,
            text: "x".to_string(),
            foreground: 0x00ff_ffff,
            background: 0x0000_8800,
            attributes: TextAttributes {
                selected: true,
                ..TextAttributes::default()
            },
        });

        let scene = SceneBuilder::new(&manager, viewport(), |_| Some(0))
            .text(text)
            .build();

        let text_item = scene
            .layers
            .iter()
            .flat_map(|layer| layer.items.iter())
            .find_map(|item| match item {
                DrawItem::Text(batch) => Some(batch),
                _ => None,
            })
            .expect("text layer is always present");
        assert!(text_item.cells[0].attributes.selected);
        assert_eq!(text_item.cells[0].background, 0x0000_8800);
    }

    #[test]
    fn background_text_and_cursor_have_stable_outermost_layers() {
        let manager = manager();
        let scene = SceneBuilder::new(&manager, viewport(), |_| Some(0))
            .background(crate::BackgroundInstance { color: 0x0012_3456 })
            .cursor(CursorInstance {
                row: 0,
                col: 0,
                x: 1.0,
                y: 2.0,
                w: 8.0,
                h: 16.0,
                shape: crate::CursorShape::Block,
            })
            .build();

        assert_eq!(
            scene.layers.first().map(|layer| layer.z_index),
            Some(BACKGROUND_LAYER_Z)
        );
        assert_eq!(
            scene.layers.last().map(|layer| layer.z_index),
            Some(CURSOR_LAYER_Z)
        );
    }

    #[test]
    fn off_viewport_placement_is_omitted() {
        let mut manager = manager();
        let mut placement = placement(1, 7);
        placement.col = 50;
        manager
            .insert_placement(placement)
            .expect("valid placement");

        let scene = SceneBuilder::new(&manager, viewport(), |_| Some(0)).build();
        assert_eq!(scene.image_count(), 0);
    }
}
