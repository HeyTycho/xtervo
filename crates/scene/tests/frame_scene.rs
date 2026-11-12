//! Frame -> Scene semantic contract tests.
//!
//! These tests use a real `tervo_core::Frame` value and assert the scene preserves the frame's
//! cells, attributes, selection/search, cursor, damage and image metadata. They never start a
//! GUI, Xvfb, Slint or a GPU device.

#![cfg(feature = "frame")]

use std::sync::Arc;

use tervo_core::{
    flags, CellData, CursorShape as FrameCursorShape, CursorState, Frame, ImageCrop,
    ImagePlacementMeta, InlineImage, LineDamage, Span,
};
use tervo_graphics::{CompositePolicy, PlacementId, Rect, RowId, SourceId};
use tervo_scene::{
    CursorShape, DamageRect, DrawItem, ImageInstance, Scene, SceneBuilder, SelectionSpan,
    SelectionStyle, TextBatch, TextCell, Viewport, CURSOR_LAYER_Z, TEXT_LAYER_Z,
};

fn viewport(cols: usize, rows: usize) -> Viewport {
    Viewport {
        cell_w: 10.0,
        cell_h: 10.0,
        cols,
        rows,
    }
}

fn put(frame: &mut Frame, row: usize, col: usize, ch: char, fg: u32, bg: u32, flags: u16) {
    frame.cells[row * frame.cols + col] = CellData {
        ch: ch as u32,
        fg,
        bg,
        flags,
        link: 0,
    };
}

fn batch(scene: &Scene) -> &TextBatch {
    scene.text_batch().expect("text item always exists")
}

fn cell(scene: &Scene, row: u16, col: u16) -> &TextCell {
    batch(scene)
        .cells
        .iter()
        .find(|cell| cell.row == row && cell.col == col)
        .unwrap_or_else(|| panic!("missing scene cell ({row},{col})"))
}

#[allow(clippy::too_many_arguments)]
fn image(
    id: u64,
    source_id: u64,
    row_id: u64,
    row: i32,
    col: u16,
    width: u32,
    height: u32,
    z_index: i32,
    above_text: bool,
) -> InlineImage {
    InlineImage {
        id,
        source_id,
        row_id,
        row,
        col,
        width,
        height,
        data: Arc::from(vec![0u8; (width as usize) * (height as usize) * 4]),
        z_index,
        above_text,
    }
}

fn meta(
    placement_id: u64,
    row_id: u64,
    source_rect: Option<ImageCrop>,
    cells_w: u16,
    cells_h: u16,
    z_index: i32,
    above_text: bool,
) -> ImagePlacementMeta {
    ImagePlacementMeta {
        placement_id,
        image_id: placement_id,
        row_id,
        source_rect,
        cells_w,
        cells_h,
        z_index,
        above_text,
    }
}

fn image_at(scene: &Scene, id: u64) -> &ImageInstance {
    scene
        .images()
        .find(|image| image.placement_id == PlacementId(id))
        .unwrap_or_else(|| panic!("missing image placement {id}"))
}

#[test]
fn ascii_cells_keep_row_col_colors_and_attributes() {
    let mut frame = Frame::new(4, 2);
    put(&mut frame, 0, 0, 'A', 0x0011_2233, 0x0044_5566, 0);
    put(
        &mut frame,
        1,
        2,
        'b',
        0x00aa_bbcc,
        0x00dd_eeff,
        flags::BOLD
            | flags::ITALIC
            | flags::UNDERLINE
            | flags::STRIKEOUT
            | flags::DIM
            | flags::HIDDEN,
    );
    frame.generation = 41;
    frame.full = false;

    let vp = viewport(4, 2);
    let scene = SceneBuilder::from_frame(&frame, vp).build();

    assert_eq!(scene.viewport, vp);
    assert_eq!(scene.revision, 41);
    let cells = &batch(&scene).cells;
    assert_eq!(cells.len(), 4 * 2);
    for (index, cell) in cells.iter().enumerate() {
        assert_eq!(cell.row, (index / 4) as u16);
        assert_eq!(cell.col, (index % 4) as u16);
    }

    let a = cell(&scene, 0, 0);
    assert_eq!(a.text, "A");
    assert_eq!(a.foreground, 0x0011_2233);
    assert_eq!(a.background, 0x0044_5566);
    assert_eq!(a.attributes, Default::default());

    let b = cell(&scene, 1, 2);
    assert_eq!(b.text, "b");
    assert_eq!(b.foreground, 0x00aa_bbcc);
    assert_eq!(b.background, 0x00dd_eeff);
    assert!(b.attributes.bold);
    assert!(b.attributes.italic);
    assert!(b.attributes.underline);
    assert!(b.attributes.strikethrough);
    assert!(b.attributes.dim);
    assert!(b.attributes.hidden);
    assert!(!b.attributes.wide);
    assert!(!b.attributes.wide_spacer);
    assert!(!b.attributes.selected);
    assert!(!b.attributes.searched);

    // Blank cells are real cells, not omissions: row/column identity must stay exact.
    let blank = cell(&scene, 1, 0);
    assert_eq!(blank.text, " ");
    assert_eq!(blank.row, 1);
    assert_eq!(blank.col, 0);
}

#[test]
fn wide_emoji_and_combining_cells_survive_without_shaping() {
    let mut frame = Frame::new(6, 2);
    put(
        &mut frame,
        0,
        1,
        '中',
        0x00ff_ffff,
        0x0000_0000,
        flags::WIDE_CHAR,
    );
    put(
        &mut frame,
        0,
        2,
        ' ',
        0x00ff_ffff,
        0x0000_0000,
        flags::WIDE_CHAR_SPACER,
    );
    put(
        &mut frame,
        0,
        4,
        '🙂',
        0x00ff_ffff,
        0x0000_0000,
        flags::WIDE_CHAR,
    );
    put(
        &mut frame,
        0,
        5,
        ' ',
        0x00ff_ffff,
        0x0000_0000,
        flags::WIDE_CHAR_SPACER,
    );
    let base = CellData {
        ch: 'e' as u32,
        fg: 0x0012_3456,
        bg: 0x0065_4321,
        ..CellData::default()
    };
    frame.put_zerowidth(6, base, None, &['\u{0301}']);

    let scene = SceneBuilder::from_frame(&frame, viewport(6, 2)).build();

    let cjk = cell(&scene, 0, 1);
    assert_eq!(cjk.text, "中");
    assert!(cjk.attributes.wide);
    assert!(!cjk.attributes.wide_spacer);

    let spacer = cell(&scene, 0, 2);
    assert!(spacer.attributes.wide_spacer);
    assert!(!spacer.attributes.wide);

    let emoji = cell(&scene, 0, 4);
    assert_eq!(emoji.text, "🙂");
    assert!(emoji.attributes.wide);

    // No shaping: the base cell plus the combining mark are one text run, exactly as the
    // terminal grid stores them. The mark does not create an extra scene cell.
    let accented = cell(&scene, 1, 0);
    assert_eq!(accented.text, "e\u{0301}");
    assert_eq!(accented.col, 0);
    assert_eq!(batch(&scene).cells.len(), 12);
}

#[test]
fn selection_and_search_resolve_into_cell_colors_and_scene_spans() {
    let mut frame = Frame::new(4, 2);
    put(&mut frame, 0, 1, 'x', 0x00ff_ffff, 0x0000_0000, 0);
    put(
        &mut frame,
        0,
        2,
        'y',
        0x00ff_0000,
        0x0000_ff00,
        flags::INVERSE,
    );
    put(&mut frame, 1, 0, 'a', 0x0011_1111, 0x0022_2222, 0);
    put(&mut frame, 1, 1, 'b', 0x0033_3333, 0x0044_4444, 0);
    put(&mut frame, 1, 2, 'c', 0x0055_5555, 0x0066_6666, 0);
    frame.selection = Some(Span {
        start_line: 0,
        start_col: 1,
        end_line: 0,
        end_col: 2,
    });
    frame.search = Some(Span {
        start_line: 1,
        start_col: 0,
        end_line: 1,
        end_col: 1,
    });

    let scene = SceneBuilder::from_frame(&frame, viewport(4, 2)).build();

    assert_eq!(
        scene.selection,
        Some(SelectionSpan {
            start_row: 0,
            start_col: 1,
            end_row: 0,
            end_col: 2,
        })
    );
    assert_eq!(
        scene.search,
        Some(SelectionSpan {
            start_row: 1,
            start_col: 0,
            end_row: 1,
            end_col: 1,
        })
    );

    // Selection swaps the already inverse-resolved colors (legacy CPU renderer semantics).
    let selected = cell(&scene, 0, 1);
    assert!(selected.attributes.selected);
    assert_eq!(selected.foreground, 0x0000_0000);
    assert_eq!(selected.background, 0x00ff_ffff);

    // inverse then selection = original colors, but both semantic flags are retained.
    let inverse_selected = cell(&scene, 0, 2);
    assert!(inverse_selected.attributes.inverse);
    assert!(inverse_selected.attributes.selected);
    assert_eq!(inverse_selected.foreground, 0x00ff_0000);
    assert_eq!(inverse_selected.background, 0x0000_ff00);

    let searched = cell(&scene, 1, 0);
    assert!(searched.attributes.searched);
    assert_eq!(searched.foreground, 0x0011_111b);
    assert_eq!(searched.background, 0x00f9_e2af);

    let not_searched = cell(&scene, 1, 2);
    assert!(!not_searched.attributes.searched);
    assert_eq!(not_searched.foreground, 0x0055_5555);
    assert_eq!(not_searched.background, 0x0066_6666);

    // Selection/search are not extra overlay items.
    let text_items = scene
        .layers
        .iter()
        .flat_map(|layer| layer.items.iter())
        .filter(|item| matches!(item, DrawItem::Text(_)))
        .count();
    assert_eq!(text_items, 1);
}

#[test]
fn custom_search_colors_come_from_the_builder() {
    let mut frame = Frame::new(2, 1);
    put(&mut frame, 0, 0, 's', 0x00ff_ffff, 0x0000_0000, 0);
    frame.search = Some(Span {
        start_line: 0,
        start_col: 0,
        end_line: 0,
        end_col: 0,
    });

    let scene = SceneBuilder::from_frame(&frame, viewport(2, 1))
        .selection_style(SelectionStyle {
            search_foreground: 0x0000_00ff,
            search_background: 0x0000_ff00,
        })
        .build();

    let searched = cell(&scene, 0, 0);
    assert_eq!(searched.foreground, 0x0000_00ff);
    assert_eq!(searched.background, 0x0000_ff00);
}

#[test]
fn scroll_projection_keeps_viewport_rows_and_frame_damage() {
    let mut frame = Frame::new(5, 3);
    frame.row_ids = vec![10, 11, 12];
    frame.display_offset = 4;
    put(&mut frame, 0, 0, 'a', 0x00ff_ffff, 0x0000_0000, 0);
    put(&mut frame, 1, 0, 'b', 0x00ff_ffff, 0x0000_0000, 0);
    put(&mut frame, 2, 0, 'c', 0x00ff_ffff, 0x0000_0000, 0);
    frame.damage = vec![LineDamage {
        line: 2,
        left: 1,
        right: 3,
    }];
    frame.images.push(image(7, 70, 11, 1, 0, 20, 10, -1, false));
    frame
        .image_placements
        .push(meta(7, 11, None, 2, 1, -1, false));

    let scene = SceneBuilder::from_frame(&frame, viewport(5, 3)).build();

    // Text rows are the current viewport projection, not absolute/scroll rows.
    assert_eq!(cell(&scene, 0, 0).text, "a");
    assert_eq!(cell(&scene, 1, 0).text, "b");
    assert_eq!(cell(&scene, 2, 0).text, "c");

    // Image keeps both the projected row and the stable row identity.
    let image = image_at(&scene, 7);
    assert_eq!(image.row, 1);
    assert_eq!(image.row_id, RowId(11));

    // LineDamage.right is inclusive: columns 1..=3 at 10px cells = x 10, width 30.
    assert_eq!(scene.damage.rects, vec![DamageRect::new(10, 20, 30, 10)]);
    assert!(!scene.damage.is_full());
}

#[test]
fn full_frame_invalidates_the_whole_scene() {
    let mut frame = Frame::new(2, 2);
    frame.full = true;
    frame.damage = vec![LineDamage {
        line: 0,
        left: 0,
        right: 0,
    }];

    let scene = SceneBuilder::from_frame(&frame, viewport(2, 2)).build();
    assert!(scene.damage.is_full());
    assert!(scene.damage.rects.is_empty());
}

#[test]
fn top_cropped_image_keeps_negative_row_crop_and_clip() {
    let mut frame = Frame::new(10, 5);
    frame
        .images
        .push(image(7, 70, 700, -1, 2, 40, 20, -2, false));
    frame.image_placements.push(meta(
        7,
        700,
        Some(ImageCrop {
            x: 8,
            y: 4,
            w: 24,
            h: 12,
        }),
        4,
        2,
        -2,
        false,
    ));

    let strong_before = Arc::strong_count(&frame.images[0].data);
    let scene = SceneBuilder::from_frame(&frame, viewport(10, 5)).build();
    assert_eq!(
        Arc::strong_count(&frame.images[0].data),
        strong_before,
        "scene must reference image pixels only by source_id"
    );

    let image = image_at(&scene, 7);
    assert_eq!(image.source_id, SourceId(70));
    assert_eq!(image.source_rect, Rect::new(8.0, 4.0, 24.0, 12.0));
    assert_eq!(image.source_size, (40, 20));
    assert_eq!(image.cells_w, 4);
    assert_eq!(image.cells_h, 2);
    assert_eq!(image.row, -1);
    assert_eq!(image.col, 2);
    assert_eq!(image.row_id, RowId(700));
    assert_eq!(image.z_index, -2);
    assert!(!image.above_text);
    assert_eq!(image.policy, CompositePolicy::BelowText);

    // The placement rectangle starts above the viewport; only the clip is clamped.
    assert_eq!(image.destination, Rect::new(20.0, -10.0, 40.0, 20.0));
    assert_eq!(image.clip, Rect::new(20.0, 0.0, 40.0, 10.0));

    // The image layer is below text and keeps its policy-aware z.
    assert!(scene.layer(-2).is_some());
    assert!(scene.layer(TEXT_LAYER_Z).is_some());
    let below = scene
        .layers
        .iter()
        .position(|layer| layer.z_index == -2)
        .expect("below-text image layer");
    let text = scene
        .layers
        .iter()
        .position(|layer| layer.z_index == TEXT_LAYER_Z)
        .expect("text layer");
    assert!(below < text);
}

#[test]
fn above_text_images_stay_above_glyphs_and_keep_stable_order() {
    let mut frame = Frame::new(12, 5);
    // Deliberately interleave above/below entries in frame order.
    for (id, z, above) in [
        (1, -5, false),
        (2, 0, false),
        (3, -1, false),
        (4, 0, true),
        (5, 5, true),
        (6, 1, true),
    ] {
        frame
            .images
            .push(image(id, 100 + id, 1000 + id, 0, 0, 20, 20, z, above));
        frame.image_placements.push(meta(
            id,
            1000 + id,
            Some(ImageCrop {
                x: 0,
                y: 0,
                w: 20,
                h: 20,
            }),
            2,
            2,
            z,
            above,
        ));
    }

    let scene = SceneBuilder::from_frame(&frame, viewport(12, 5)).build();

    let layer_zs: Vec<_> = scene.layers.iter().map(|layer| layer.z_index).collect();
    assert_eq!(layer_zs, vec![-5, -1, TEXT_LAYER_Z, 1, 5]);

    let image_ids: Vec<_> = scene.images().map(|image| image.placement_id).collect();
    assert_eq!(
        image_ids,
        vec![
            PlacementId(1),
            PlacementId(2),
            PlacementId(3),
            PlacementId(4),
            PlacementId(6),
            PlacementId(5),
        ]
    );

    for id in [1, 2, 3] {
        let image = image_at(&scene, id);
        assert!(!image.above_text);
        assert_eq!(image.policy, CompositePolicy::BelowText);
        assert!(image.z_index <= 0);
    }
    for id in [4, 5, 6] {
        let image = image_at(&scene, id);
        assert!(image.above_text);
        assert_eq!(image.policy, CompositePolicy::AboveText);
    }

    let text = scene
        .layers
        .iter()
        .position(|layer| layer.z_index == TEXT_LAYER_Z)
        .expect("text layer");
    for (index, layer) in scene.layers.iter().enumerate() {
        if layer
            .items
            .iter()
            .any(|item| matches!(item, DrawItem::Image(_)))
        {
            if layer.z_index < TEXT_LAYER_Z {
                assert!(index < text);
            } else {
                assert!(index > text);
            }
        }
    }
}

#[test]
fn cursor_maps_to_state_and_wide_cell_geometry() {
    let mut frame = Frame::new(5, 3);
    put(
        &mut frame,
        1,
        2,
        '中',
        0x00ff_ffff,
        0x0000_0000,
        flags::WIDE_CHAR,
    );
    put(
        &mut frame,
        1,
        3,
        ' ',
        0x00ff_ffff,
        0x0000_0000,
        flags::WIDE_CHAR_SPACER,
    );
    frame.cursor = CursorState {
        line: 1,
        col: 2,
        shape: FrameCursorShape::Beam,
        visible: true,
    };

    let scene = SceneBuilder::from_frame(&frame, viewport(5, 3)).build();

    let state = scene.cursor.expect("cursor state preserved");
    assert_eq!(state.row, 1);
    assert_eq!(state.col, 2);
    assert_eq!(state.shape, CursorShape::Beam);
    assert!(state.visible);

    let cursor_layer = scene.layer(CURSOR_LAYER_Z).expect("cursor draw item");
    assert_eq!(cursor_layer.items.len(), 1);
    let DrawItem::Cursor(cursor) = &cursor_layer.items[0] else {
        panic!("expected cursor item");
    };
    assert_eq!(cursor.row, 1);
    assert_eq!(cursor.col, 2);
    assert_eq!(cursor.shape, CursorShape::Beam);
    assert_eq!(
        (cursor.x, cursor.y, cursor.w, cursor.h),
        (20.0, 10.0, 20.0, 10.0)
    );

    // Hidden cursor: provenance remains, but no draw item is emitted.
    frame.cursor = CursorState {
        line: 0,
        col: 0,
        shape: FrameCursorShape::Hidden,
        visible: true,
    };
    let hidden = SceneBuilder::from_frame(&frame, viewport(5, 3)).build();
    let state = hidden.cursor.expect("cursor state preserved");
    assert_eq!(state.shape, CursorShape::Hidden);
    assert!(!state.visible);
    assert!(hidden.layer(CURSOR_LAYER_Z).is_none());
}

#[test]
fn sixel_style_image_without_crop_keeps_pane_fit_geometry() {
    let mut frame = Frame::new(10, 5);
    // No placement meta: the frame only knows pixels and the grid anchor.
    frame.images.push(image(1, 9, 900, 0, 0, 25, 15, -1, false));

    let scene = SceneBuilder::from_frame(&frame, viewport(10, 5)).build();
    let image = image_at(&scene, 1);

    assert_eq!(image.source_rect, Rect::new(0.0, 0.0, 25.0, 15.0));
    assert_eq!(image.cells_w, 3);
    assert_eq!(image.cells_h, 2);
    // Pane-fit contract: a 25x15 source in a 100x50 pane is not scaled up and not rounded to
    // whole cells.
    assert_eq!(image.destination, Rect::new(0.0, 0.0, 25.0, 15.0));
    assert_eq!(image.clip, Rect::new(0.0, 0.0, 25.0, 15.0));
}

#[test]
fn placement_metadata_is_paired_by_id_not_vector_index() {
    let mut frame = Frame::new(10, 5);
    frame
        .images
        .push(image(1, 11, 101, 0, 0, 30, 10, -1, false));
    frame
        .images
        .push(image(2, 22, 202, 1, 0, 30, 10, -1, false));
    // Reversed metadata order must still attach the right crop to the right image.
    frame.image_placements.push(meta(
        2,
        202,
        Some(ImageCrop {
            x: 5,
            y: 0,
            w: 10,
            h: 10,
        }),
        1,
        1,
        -1,
        false,
    ));
    frame.image_placements.push(meta(
        1,
        101,
        Some(ImageCrop {
            x: 1,
            y: 2,
            w: 7,
            h: 3,
        }),
        3,
        1,
        -1,
        false,
    ));

    let scene = SceneBuilder::from_frame(&frame, viewport(10, 5)).build();

    let first = image_at(&scene, 1);
    assert_eq!(first.source_id, SourceId(11));
    assert_eq!(first.row_id, RowId(101));
    assert_eq!(first.source_rect, Rect::new(1.0, 2.0, 7.0, 3.0));
    assert_eq!((first.cells_w, first.cells_h), (3, 1));

    let second = image_at(&scene, 2);
    assert_eq!(second.source_id, SourceId(22));
    assert_eq!(second.row_id, RowId(202));
    assert_eq!(second.source_rect, Rect::new(5.0, 0.0, 10.0, 10.0));
    assert_eq!((second.cells_w, second.cells_h), (1, 1));
}
