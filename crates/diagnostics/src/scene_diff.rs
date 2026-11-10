// SPDX-License-Identifier: GPL-3.0-only
//! Debug scene diff.
//!
//! Compares two lightweight [`SceneSummary`] values: text rows/cells, image
//! placements (added/removed/moved/resized/re-sourced) and damage regions plus
//! viewport coverage. It deliberately does not depend on wgpu, Slint or the
//! renderer; a caller converts its frame/scene into a summary.
//!
//! Output is bounded: row details are capped at [`MAX_TEXT_DETAIL_ROWS`] so a
//! dumped diff cannot grow with the scrollback or with a full-screen change.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Maximum changed-row previews retained in a text diff.
pub const MAX_TEXT_DETAIL_ROWS: usize = 32;
/// Maximum chars per row preview in text output.
const PREVIEW_CHARS: usize = 96;
const PPM: u64 = 1_000_000;

/// One damage rectangle in cell coordinates. Only positive `w`/`h` count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl DamageRect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn area(self) -> u64 {
        if self.is_empty() {
            return 0;
        }
        (self.w as u64).saturating_mul(self.h as u64)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamageSummary {
    #[serde(default)]
    pub full: bool,
    #[serde(default)]
    pub rects: Vec<DamageRect>,
}

impl DamageSummary {
    /// Union area of the damage rects in cells. `full` covers the viewport.
    pub fn area(&self, viewport_cells: u64) -> u64 {
        if self.full {
            return viewport_cells;
        }
        let _ = viewport_cells;
        union_area(&self.rects)
    }

    /// Coverage in parts per million, clamped to 100%.
    pub fn coverage_ppm(&self, viewport_cells: u64) -> u64 {
        if viewport_cells == 0 {
            return 0;
        }
        self.area(viewport_cells)
            .min(viewport_cells)
            .saturating_mul(PPM)
            / viewport_cells
    }

    pub fn region_count(&self) -> u64 {
        if self.full {
            1
        } else {
            self.rects.iter().filter(|rect| !rect.is_empty()).count() as u64
        }
    }
}

/// Lightweight image placement summary (cell coordinates).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSummary {
    pub placement_id: u64,
    pub source_id: u64,
    pub row: i32,
    pub col: u16,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub z_index: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneSummary {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub cols: usize,
    #[serde(default)]
    pub rows: usize,
    /// Row text. Missing rows are treated as empty strings.
    #[serde(default)]
    pub text: Vec<String>,
    #[serde(default)]
    pub images: Vec<ImageSummary>,
    #[serde(default)]
    pub damage: DamageSummary,
}

impl SceneSummary {
    pub fn viewport_area(&self) -> u64 {
        (self.cols as u64).saturating_mul(self.rows as u64)
    }

    pub fn damage_area(&self) -> u64 {
        self.damage.area(self.viewport_area())
    }

    pub fn damage_coverage_ppm(&self) -> u64 {
        self.damage.coverage_ppm(self.viewport_area())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewportDiff {
    pub left_cols: usize,
    pub left_rows: usize,
    pub right_cols: usize,
    pub right_rows: usize,
}

impl ViewportDiff {
    pub fn changed(&self) -> bool {
        self.left_cols != self.right_cols || self.left_rows != self.right_rows
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRowDiff {
    pub row: usize,
    pub changed_cells: u64,
    pub before: String,
    pub after: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDiff {
    pub changed_rows: u64,
    pub changed_cells: u64,
    pub added_rows: u64,
    pub removed_rows: u64,
    pub details: Vec<TextRowDiff>,
    pub details_truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePosition {
    pub row: i32,
    pub col: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovedImage {
    pub placement_id: u64,
    pub before: ImagePosition,
    pub after: ImagePosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizedImage {
    pub placement_id: u64,
    pub before: ImageSize,
    pub after: ImageSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceChangedImage {
    pub placement_id: u64,
    pub before_source_id: u64,
    pub after_source_id: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageDiff {
    pub left_count: u64,
    pub right_count: u64,
    pub added: Vec<ImageSummary>,
    pub removed: Vec<ImageSummary>,
    pub moved: Vec<MovedImage>,
    pub resized: Vec<ResizedImage>,
    pub source_changed: Vec<SourceChangedImage>,
}

impl ImageDiff {
    pub fn changed(&self) -> bool {
        !self.added.is_empty()
            || !self.removed.is_empty()
            || !self.moved.is_empty()
            || !self.resized.is_empty()
            || !self.source_changed.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamageSide {
    pub full: bool,
    pub region_count: u64,
    pub area: u64,
    pub coverage_ppm: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamageDiff {
    pub left: DamageSide,
    pub right: DamageSide,
    pub coverage_delta_ppm: i64,
    pub regions_added: Vec<DamageRect>,
    pub regions_removed: Vec<DamageRect>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneDiff {
    pub left_revision: u64,
    pub right_revision: u64,
    pub viewport: ViewportDiff,
    pub text: TextDiff,
    pub images: ImageDiff,
    pub damage: DamageDiff,
}

impl SceneDiff {
    /// Compare two summaries. All retained details are bounded.
    pub fn between(left: &SceneSummary, right: &SceneSummary) -> Self {
        let viewport = ViewportDiff {
            left_cols: left.cols,
            left_rows: left.rows,
            right_cols: right.cols,
            right_rows: right.rows,
        };
        let text = diff_text(left, right);
        let images = diff_images(left, right);
        let damage = diff_damage(left, right);
        Self {
            left_revision: left.revision,
            right_revision: right.revision,
            viewport,
            text,
            images,
            damage,
        }
    }

    pub fn has_changes(&self) -> bool {
        self.viewport.changed()
            || self.text.changed_rows > 0
            || self.images.changed()
            || self.damage.coverage_delta_ppm != 0
            || self.damage.left.full != self.damage.right.full
            || !self.damage.regions_added.is_empty()
            || !self.damage.regions_removed.is_empty()
    }

    /// Additive `tervo.diag.v1` JSON form.
    pub fn to_json(&self) -> Value {
        json!({
            "schema": "tervo.diag.v1",
            "kind": "scene_diff",
            "changed": self.has_changes(),
            "left_revision": self.left_revision,
            "right_revision": self.right_revision,
            "viewport": {
                "left": {"cols": self.viewport.left_cols, "rows": self.viewport.left_rows},
                "right": {"cols": self.viewport.right_cols, "rows": self.viewport.right_rows},
                "changed": self.viewport.changed(),
            },
            "text": {
                "changed_rows": self.text.changed_rows,
                "changed_cells": self.text.changed_cells,
                "added_rows": self.text.added_rows,
                "removed_rows": self.text.removed_rows,
                "details_truncated": self.text.details_truncated,
                "details": self.text.details.iter().map(|row| json!({
                    "row": row.row,
                    "changed_cells": row.changed_cells,
                    "before": row.before,
                    "after": row.after,
                })).collect::<Vec<_>>(),
            },
            "images": {
                "left_count": self.images.left_count,
                "right_count": self.images.right_count,
                "added": self.images.added,
                "removed": self.images.removed,
                "moved": self.images.moved,
                "resized": self.images.resized,
                "source_changed": self.images.source_changed,
                "counts": {
                    "added": self.images.added.len(),
                    "removed": self.images.removed.len(),
                    "moved": self.images.moved.len(),
                    "resized": self.images.resized.len(),
                    "source_changed": self.images.source_changed.len(),
                },
            },
            "damage": {
                "left": self.damage.left,
                "right": self.damage.right,
                "coverage_delta_ppm": self.damage.coverage_delta_ppm,
                "regions_added": self.damage.regions_added,
                "regions_removed": self.damage.regions_removed,
                "counts": {
                    "added": self.damage.regions_added.len(),
                    "removed": self.damage.regions_removed.len(),
                },
            },
        })
    }

    /// Bounded human-readable diff.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "scene diff: rev {} -> {}\n",
            self.left_revision, self.right_revision
        ));
        out.push_str(&format!(
            "viewport: {}x{} -> {}x{}\n",
            self.viewport.left_cols,
            self.viewport.left_rows,
            self.viewport.right_cols,
            self.viewport.right_rows
        ));
        out.push_str(&format!(
            "text: {} changed rows, {} changed cells, +{} / -{} rows\n",
            self.text.changed_rows,
            self.text.changed_cells,
            self.text.added_rows,
            self.text.removed_rows
        ));
        for row in &self.text.details {
            out.push_str(&format!(
                "  row {} ({} cells): {:?} -> {:?}\n",
                row.row, row.changed_cells, row.before, row.after
            ));
        }
        if self.text.details_truncated {
            out.push_str(&format!(
                "  ... {} more changed rows\n",
                self.text
                    .changed_rows
                    .saturating_sub(self.text.details.len() as u64)
            ));
        }

        out.push_str(&format!(
            "images: +{} -{} moved {} resized {} source_changed {}\n",
            self.images.added.len(),
            self.images.removed.len(),
            self.images.moved.len(),
            self.images.resized.len(),
            self.images.source_changed.len()
        ));
        for image in &self.images.added {
            out.push_str(&format!(
                "  + placement {} source {} at row {} col {} {}x{}\n",
                image.placement_id,
                image.source_id,
                image.row,
                image.col,
                image.width,
                image.height
            ));
        }
        for image in &self.images.removed {
            out.push_str(&format!(
                "  - placement {} source {} at row {} col {} {}x{}\n",
                image.placement_id,
                image.source_id,
                image.row,
                image.col,
                image.width,
                image.height
            ));
        }
        for image in &self.images.moved {
            out.push_str(&format!(
                "  ~ placement {} moved row {} col {} -> row {} col {}\n",
                image.placement_id,
                image.before.row,
                image.before.col,
                image.after.row,
                image.after.col
            ));
        }
        for image in &self.images.resized {
            out.push_str(&format!(
                "  ~ placement {} resized {}x{} -> {}x{}\n",
                image.placement_id,
                image.before.width,
                image.before.height,
                image.after.width,
                image.after.height
            ));
        }
        for image in &self.images.source_changed {
            out.push_str(&format!(
                "  ~ placement {} source {} -> {}\n",
                image.placement_id, image.before_source_id, image.after_source_id
            ));
        }

        out.push_str(&format!(
            "damage: {} regions / {} cells / {} ppm -> {} regions / {} cells / {} ppm (delta {} ppm)\n",
            self.damage.left.region_count,
            self.damage.left.area,
            self.damage.left.coverage_ppm,
            self.damage.right.region_count,
            self.damage.right.area,
            self.damage.right.coverage_ppm,
            self.damage.coverage_delta_ppm
        ));
        for rect in &self.damage.regions_added {
            out.push_str(&format!(
                "  + damage rect x={} y={} w={} h={}\n",
                rect.x, rect.y, rect.w, rect.h
            ));
        }
        for rect in &self.damage.regions_removed {
            out.push_str(&format!(
                "  - damage rect x={} y={} w={} h={}\n",
                rect.x, rect.y, rect.w, rect.h
            ));
        }
        out
    }
}

fn diff_text(left: &SceneSummary, right: &SceneSummary) -> TextDiff {
    let row_count = left
        .rows
        .max(right.rows)
        .max(left.text.len())
        .max(right.text.len());
    let mut diff = TextDiff {
        added_rows: right.rows.saturating_sub(left.rows) as u64,
        removed_rows: left.rows.saturating_sub(right.rows) as u64,
        ..TextDiff::default()
    };
    for row in 0..row_count {
        let before = left.text.get(row).map(String::as_str).unwrap_or("");
        let after = right.text.get(row).map(String::as_str).unwrap_or("");
        if before == after {
            continue;
        }
        let changed = changed_cells(before, after);
        diff.changed_rows = diff.changed_rows.saturating_add(1);
        diff.changed_cells = diff.changed_cells.saturating_add(changed);
        if diff.details.len() < MAX_TEXT_DETAIL_ROWS {
            diff.details.push(TextRowDiff {
                row,
                changed_cells: changed,
                before: preview(before),
                after: preview(after),
            });
        } else {
            diff.details_truncated = true;
        }
    }
    diff
}

fn changed_cells(before: &str, after: &str) -> u64 {
    let mut before = before.chars();
    let mut after = after.chars();
    let mut changed = 0u64;
    loop {
        match (before.next(), after.next()) {
            (None, None) => break,
            (Some(a), Some(b)) if a == b => {}
            _ => changed = changed.saturating_add(1),
        }
    }
    changed
}

fn preview(row: &str) -> String {
    if row.chars().count() <= PREVIEW_CHARS {
        return row.to_string();
    }
    let mut out: String = row.chars().take(PREVIEW_CHARS.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn diff_images(left: &SceneSummary, right: &SceneSummary) -> ImageDiff {
    use std::collections::BTreeMap;

    let left_by_id: BTreeMap<u64, &ImageSummary> = left
        .images
        .iter()
        .map(|image| (image.placement_id, image))
        .collect();
    let right_by_id: BTreeMap<u64, &ImageSummary> = right
        .images
        .iter()
        .map(|image| (image.placement_id, image))
        .collect();

    let mut diff = ImageDiff {
        left_count: left_by_id.len() as u64,
        right_count: right_by_id.len() as u64,
        ..ImageDiff::default()
    };
    for (id, before) in &left_by_id {
        match right_by_id.get(id) {
            None => diff.removed.push(**before),
            Some(after) => {
                if before.row != after.row || before.col != after.col {
                    diff.moved.push(MovedImage {
                        placement_id: *id,
                        before: ImagePosition {
                            row: before.row,
                            col: before.col,
                        },
                        after: ImagePosition {
                            row: after.row,
                            col: after.col,
                        },
                    });
                }
                if before.width != after.width || before.height != after.height {
                    diff.resized.push(ResizedImage {
                        placement_id: *id,
                        before: ImageSize {
                            width: before.width,
                            height: before.height,
                        },
                        after: ImageSize {
                            width: after.width,
                            height: after.height,
                        },
                    });
                }
                if before.source_id != after.source_id {
                    diff.source_changed.push(SourceChangedImage {
                        placement_id: *id,
                        before_source_id: before.source_id,
                        after_source_id: after.source_id,
                    });
                }
            }
        }
    }
    for (id, after) in &right_by_id {
        if !left_by_id.contains_key(id) {
            diff.added.push(**after);
        }
    }
    diff
}

fn diff_damage(left: &SceneSummary, right: &SceneSummary) -> DamageDiff {
    let left_area = left.viewport_area();
    let right_area = right.viewport_area();
    let left_side = DamageSide {
        full: left.damage.full,
        region_count: left.damage.region_count(),
        area: left.damage.area(left_area),
        coverage_ppm: left.damage.coverage_ppm(left_area),
    };
    let right_side = DamageSide {
        full: right.damage.full,
        region_count: right.damage.region_count(),
        area: right.damage.area(right_area),
        coverage_ppm: right.damage.coverage_ppm(right_area),
    };

    let mut remaining: Vec<DamageRect> = right
        .damage
        .rects
        .iter()
        .copied()
        .filter(|rect| !rect.is_empty())
        .collect();
    let mut removed = Vec::new();
    for rect in left
        .damage
        .rects
        .iter()
        .copied()
        .filter(|rect| !rect.is_empty())
    {
        if let Some(index) = remaining.iter().position(|candidate| *candidate == rect) {
            remaining.remove(index);
        } else {
            removed.push(rect);
        }
    }

    DamageDiff {
        left: left_side,
        right: right_side,
        coverage_delta_ppm: right_side.coverage_ppm as i64 - left_side.coverage_ppm as i64,
        regions_added: remaining,
        regions_removed: removed,
    }
}

fn union_area(rects: &[DamageRect]) -> u64 {
    let rects: Vec<DamageRect> = rects
        .iter()
        .copied()
        .filter(|rect| !rect.is_empty())
        .collect();
    if rects.is_empty() {
        return 0;
    }
    let mut xs = Vec::with_capacity(rects.len().saturating_mul(2));
    for rect in &rects {
        xs.push(u64::from(rect.x));
        xs.push(u64::from(rect.x).saturating_add(u64::from(rect.w)));
    }
    xs.sort_unstable();
    xs.dedup();

    let mut area = 0u64;
    for pair in xs.windows(2) {
        let (x0, x1) = (pair[0], pair[1]);
        if x1 <= x0 {
            continue;
        }
        let mut ys: Vec<(u64, u64)> = rects
            .iter()
            .filter(|rect| {
                u64::from(rect.x) <= x0 && u64::from(rect.x).saturating_add(u64::from(rect.w)) >= x1
            })
            .map(|rect| {
                (
                    u64::from(rect.y),
                    u64::from(rect.y).saturating_add(u64::from(rect.h)),
                )
            })
            .collect();
        ys.sort_unstable();
        let mut covered = 0u64;
        let mut current: Option<(u64, u64)> = None;
        for (y0, y1) in ys {
            match current {
                None => current = Some((y0, y1)),
                Some((start, end)) if y0 <= end => current = Some((start, end.max(y1))),
                Some((start, end)) => {
                    covered = covered.saturating_add(end.saturating_sub(start));
                    current = Some((y0, y1));
                }
            }
        }
        if let Some((start, end)) = current {
            covered = covered.saturating_add(end.saturating_sub(start));
        }
        area = area.saturating_add(x1.saturating_sub(x0).saturating_mul(covered));
    }
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: u64, source: u64, row: i32, col: u16) -> ImageSummary {
        ImageSummary {
            placement_id: id,
            source_id: source,
            row,
            col,
            width: 4,
            height: 2,
            z_index: 0,
        }
    }

    #[test]
    fn text_diff_counts_changed_rows_and_cells() {
        let left = SceneSummary {
            revision: 1,
            cols: 4,
            rows: 2,
            text: vec!["abc".into(), "def".into()],
            ..SceneSummary::default()
        };
        let right = SceneSummary {
            revision: 2,
            cols: 4,
            rows: 3,
            text: vec!["abd".into(), "dex".into(), "new".into()],
            ..SceneSummary::default()
        };
        let diff = SceneDiff::between(&left, &right);
        assert_eq!(diff.text.changed_rows, 3);
        assert_eq!(diff.text.changed_cells, 1 + 1 + 3);
        assert_eq!(diff.text.added_rows, 1);
        assert_eq!(diff.text.removed_rows, 0);
        assert_eq!(diff.text.details.len(), 3);
        assert_eq!(diff.text.details[0].row, 0);
        assert_eq!(diff.text.details[0].changed_cells, 1);
    }

    #[test]
    fn image_diff_reports_add_remove_move_and_resize() {
        let mut left_image = image(1, 10, 0, 0);
        left_image.width = 4;
        let left = SceneSummary {
            images: vec![left_image, image(2, 20, 1, 1)],
            cols: 10,
            rows: 5,
            ..SceneSummary::default()
        };
        let mut moved = image(1, 10, 2, 3);
        moved.width = 8;
        let right = SceneSummary {
            images: vec![moved, image(3, 30, 0, 0)],
            cols: 10,
            rows: 5,
            ..SceneSummary::default()
        };
        let diff = SceneDiff::between(&left, &right);
        assert_eq!(diff.images.added.len(), 1);
        assert_eq!(diff.images.added[0].placement_id, 3);
        assert_eq!(diff.images.removed.len(), 1);
        assert_eq!(diff.images.removed[0].placement_id, 2);
        assert_eq!(diff.images.moved.len(), 1);
        assert_eq!(
            diff.images.moved[0].before,
            ImagePosition { row: 0, col: 0 }
        );
        assert_eq!(diff.images.moved[0].after, ImagePosition { row: 2, col: 3 });
        assert_eq!(diff.images.resized.len(), 1);
        assert_eq!(diff.images.resized[0].after.width, 8);
    }

    #[test]
    fn damage_coverage_uses_union_not_sum() {
        let damage = DamageSummary {
            full: false,
            rects: vec![DamageRect::new(0, 0, 2, 2), DamageRect::new(1, 1, 2, 2)],
        };
        // Two 2x2 rects overlapping in one cell => 7 cells.
        assert_eq!(damage.area(16), 7);
        assert_eq!(damage.region_count(), 2);
        assert_eq!(damage.coverage_ppm(16), 437_500);

        let full = DamageSummary {
            full: true,
            rects: Vec::new(),
        };
        assert_eq!(full.area(16), 16);
        assert_eq!(full.coverage_ppm(16), 1_000_000);
    }

    #[test]
    fn damage_diff_reports_region_and_coverage_delta() {
        let left = SceneSummary {
            cols: 10,
            rows: 2,
            damage: DamageSummary {
                full: false,
                rects: vec![DamageRect::new(0, 0, 2, 1)],
            },
            ..SceneSummary::default()
        };
        let right = SceneSummary {
            cols: 10,
            rows: 2,
            damage: DamageSummary {
                full: false,
                rects: vec![DamageRect::new(5, 0, 2, 1)],
            },
            ..SceneSummary::default()
        };
        let diff = SceneDiff::between(&left, &right);
        assert_eq!(diff.damage.left.region_count, 1);
        assert_eq!(diff.damage.left.area, 2);
        assert_eq!(diff.damage.regions_added, vec![DamageRect::new(5, 0, 2, 1)]);
        assert_eq!(
            diff.damage.regions_removed,
            vec![DamageRect::new(0, 0, 2, 1)]
        );
        assert_eq!(diff.damage.coverage_delta_ppm, 0);
    }

    #[test]
    fn diff_output_is_bounded_and_serializes_additively() {
        let left = SceneSummary {
            revision: 1,
            cols: 10,
            rows: 500,
            ..SceneSummary::default()
        };
        let right = SceneSummary {
            revision: 2,
            cols: 10,
            rows: 500,
            text: vec!["x".into(); 500],
            ..SceneSummary::default()
        };
        let diff = SceneDiff::between(&left, &right);
        assert_eq!(diff.text.changed_rows, 500);
        assert_eq!(diff.text.details.len(), MAX_TEXT_DETAIL_ROWS);
        assert!(diff.text.details_truncated);
        let json = diff.to_json();
        assert_eq!(json["schema"], "tervo.diag.v1");
        assert_eq!(json["kind"], "scene_diff");
        assert_eq!(
            json["text"]["details"].as_array().unwrap().len(),
            MAX_TEXT_DETAIL_ROWS
        );
        assert!(diff.to_text().chars().count() < 8_000);
    }
}
