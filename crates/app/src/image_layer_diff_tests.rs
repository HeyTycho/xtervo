// SPDX-License-Identifier: GPL-3.0-only
//! 图片层增量绘制与整屏重绘的差分测试。
//!
//! 随机生成图片 move / crop / cells / z 层 / above_text / 删除 / 新增序列，
//! 每一步只给少量文本 damage（甚至没有），然后把增量渲染结果和同一帧的
//! 整屏参考渲染逐像素比较。任何图片 placement/row/crop 计算的偏差都会表现
//! 为像素不一致，而不是依赖 dsh 的真实会话才能暴露。

use xtervo_core::{CellData, Frame, ImageCrop, ImagePlacementMeta, InlineImage, LineDamage, Theme};

use crate::render::Renderer;
use crate::test_support::require_test_fonts;

const COLS: usize = 24;
const ROWS: usize = 8;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo + 1) as u32) as i32
    }
}

fn pack(c: u32) -> u32 {
    let r = c >> 16 & 0xff;
    let g = c >> 8 & 0xff;
    let b = c & 0xff;
    r | g << 8 | b << 16 | 0xff00_0000
}

fn frame_with_text() -> Frame {
    let bg = Theme::default().bg();
    let mut frame = Frame::new(COLS, ROWS);
    for cell in &mut frame.cells {
        cell.bg = bg;
    }
    for row in 0..ROWS {
        for col in 0..COLS {
            let mut cell = CellData::default();
            cell.bg = bg;
            cell.fg = 0x00ff_ffff;
            cell.ch = if (row + col) % 3 == 0 {
                'Q' as u32
            } else {
                ' ' as u32
            };
            frame.cells[row * COLS + col] = cell;
        }
    }
    frame
}

fn source(w: u32, h: u32, a: u8, b: u8) -> std::sync::Arc<[u8]> {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            data.extend_from_slice(&[
                (x * 7 + a as u32) as u8,
                (y * 11 + b as u32) as u8,
                (x * y + b as u32) as u8,
                if (x + y) % 5 == 0 { 0 } else { 255 },
            ]);
        }
    }
    data.into()
}

#[derive(Clone, Debug)]
struct ImgSpec {
    id: u64,
    row: i32,
    col: u16,
    crop: Option<ImageCrop>,
    cells_w: u16,
    cells_h: u16,
    above: bool,
}

fn make_image(spec: &ImgSpec) -> (InlineImage, ImagePlacementMeta) {
    let (w, h) = (40u32, 32u32);
    let image = InlineImage {
        id: spec.id,
        source_id: spec.id + 1000,
        row_id: 1,
        row: spec.row,
        col: spec.col,
        width: w,
        height: h,
        data: source(w, h, spec.id as u8, (spec.id * 3) as u8),
        z_index: if spec.above { 1 } else { -1 },
        above_text: spec.above,
    };
    let meta = ImagePlacementMeta {
        placement_id: spec.id,
        image_id: spec.id,
        row_id: 1,
        source_rect: spec.crop,
        cells_w: spec.cells_w,
        cells_h: spec.cells_h,
        z_index: image.z_index,
        above_text: spec.above,
    };
    (image, meta)
}

fn sync_frame(frame: &mut Frame, specs: &[ImgSpec]) {
    frame.images.clear();
    frame.image_placements.clear();
    for spec in specs {
        let (image, meta) = make_image(spec);
        frame.images.push(image);
        frame.image_placements.push(meta);
    }
}

fn random_spec(rng: &mut Lcg, id: u64) -> ImgSpec {
    let crop = match rng.next() % 3 {
        0 => None,
        _ => {
            let x = rng.range(0, 20) as u32;
            let y = rng.range(0, 16) as u32;
            let w = rng.range(1, 40 - x as i32) as u32;
            let h = rng.range(1, 32 - y as i32) as u32;
            Some(ImageCrop { x, y, w, h })
        }
    };
    ImgSpec {
        id,
        row: rng.range(-3, ROWS as i32 + 1),
        col: rng.range(0, COLS as i32 - 2) as u16,
        crop,
        cells_w: rng.range(1, 10) as u16,
        cells_h: rng.range(1, 5) as u16,
        above: rng.next() % 4 == 0,
    }
}

#[test]
fn partial_image_sequence_matches_full_redraw() {
    let fonts = require_test_fonts();
    let theme = Theme::default();
    for seed in 0..96u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xdead_beef);
        let mut renderer =
            Renderer::new(fonts.clone(), COLS, ROWS, theme.bg(), theme.cursor(), 1.0);
        let mut reference =
            Renderer::new(fonts.clone(), COLS, ROWS, theme.bg(), theme.cursor(), 1.0);
        let mut frame = frame_with_text();
        let mut specs: Vec<ImgSpec> = (0..1 + (rng.next() % 2))
            .map(|i| random_spec(&mut rng, i as u64 + 1))
            .collect();
        sync_frame(&mut frame, &specs);
        frame.full = true;
        renderer.draw_without_cursor(&frame);

        for step in 0..24 {
            let prev_specs = specs.clone();
            match rng.next() % 5 {
                0 | 1 => {
                    let idx = (rng.next() as usize) % specs.len().max(1);
                    if let Some(spec) = specs.get_mut(idx) {
                        spec.row = rng.range(-4, ROWS as i32 + 2);
                    }
                }
                2 => {
                    let idx = (rng.next() as usize) % specs.len().max(1);
                    if let Some(spec) = specs.get_mut(idx) {
                        spec.col = rng.range(0, COLS as i32 - 1) as u16;
                        spec.crop = random_spec(&mut rng, spec.id).crop;
                        spec.cells_w = rng.range(1, 12) as u16;
                        spec.cells_h = rng.range(1, 6) as u16;
                    }
                }
                3 if specs.len() < 3 => {
                    let id = 10 + step as u64;
                    specs.push(random_spec(&mut rng, id));
                }
                _ => {
                    if specs.len() > 1 {
                        let idx = (rng.next() as usize) % specs.len();
                        specs.remove(idx);
                    } else if let Some(spec) = specs.first_mut() {
                        spec.above = !spec.above;
                    }
                }
            }
            sync_frame(&mut frame, &specs);
            frame.full = false;
            frame.damage.clear();
            if rng.next() % 3 == 0 {
                let line = rng.range(0, ROWS as i32 - 1) as u16;
                frame.damage.push(LineDamage {
                    line,
                    left: 0,
                    right: COLS as u16 - 1,
                });
            }
            renderer.draw_without_cursor(&frame);

            let mut full_frame = frame.clone();
            full_frame.full = true;
            full_frame.damage.clear();
            reference.draw_without_cursor(&full_frame);
            let mut bad_rows = Vec::new();
            let mut min_x = usize::MAX;
            let mut max_x = 0;
            let cell_h = renderer.cell_h;
            let cell_w = renderer.cell_w;
            for (i, (a, b)) in renderer.pixels().iter().zip(reference.pixels()).enumerate() {
                if a != b {
                    let row = i / renderer.width / cell_h;
                    let x = i % renderer.width;
                    if bad_rows.last() != Some(&row) {
                        bad_rows.push(row);
                    }
                    min_x = min_x.min(x / cell_w);
                    max_x = max_x.max(x / cell_w);
                }
            }
            let summarize = |list: &[ImgSpec]| {
                list.iter()
                    .map(|s| (s.id, s.row, s.col, s.crop, s.cells_w, s.cells_h, s.above))
                    .collect::<Vec<_>>()
            };
            assert!(
                renderer.pixels() == reference.pixels(),
                "seed={seed} step={step} bad_rows={bad_rows:?} cols={min_x}..={max_x}\nprev={:?}\ncur={:?}",
                summarize(&prev_specs),
                summarize(&specs),
            );
        }
    }
}

#[test]
fn negative_row_image_is_cleared_when_it_scrolls_out() {
    let fonts = require_test_fonts();
    let theme = Theme::default();
    let mut renderer = Renderer::new(fonts.clone(), COLS, ROWS, theme.bg(), theme.cursor(), 1.0);
    let mut reference = Renderer::new(fonts, COLS, ROWS, theme.bg(), theme.cursor(), 1.0);
    let mut frame = frame_with_text();
    let mut spec = random_spec(&mut Lcg(7), 1);
    spec.row = -3;
    spec.col = 4;
    spec.crop = Some(ImageCrop {
        x: 4,
        y: 6,
        w: 20,
        h: 20,
    });
    spec.cells_w = 4;
    spec.cells_h = 3;
    sync_frame(&mut frame, &[spec]);
    frame.full = true;
    renderer.draw_without_cursor(&frame);

    // 图片完全滚出视口（row 远小于 0）：必须擦掉之前负行可见的像素。
    let mut spec = random_spec(&mut Lcg(8), 1);
    spec.row = -40;
    sync_frame(&mut frame, &[spec]);
    frame.full = false;
    frame.damage.clear();
    renderer.draw_without_cursor(&frame);

    let mut full_frame = frame.clone();
    full_frame.full = true;
    full_frame.damage.clear();
    reference.draw_without_cursor(&full_frame);
    assert_eq!(
        renderer.pixels(),
        reference.pixels(),
        "图片滚出视口后必须擦除旧像素"
    );
}

/// 顶部被视口裁掉一半的图片：可见切片必须画在真正的目标行，而不是再向下
/// 平移 -top。旧实现把中间色带画到 row1、底部色带画到 row2，并且这些行
/// 不在 row_span 内，滚出视口后不会被清理。
#[test]
fn clipped_top_image_lands_on_its_real_rows() {
    let fonts = require_test_fonts();
    let theme = Theme::default();
    let mut renderer = Renderer::new(fonts, 6, 6, theme.bg(), theme.cursor(), 1.0);
    let cell_w = renderer.cell_w as u32;
    let cell_h = renderer.cell_h as u32;
    let (w, h) = (cell_w * 6, cell_h * 3);

    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let color: [u8; 4] = if y < cell_h {
            [255, 0, 0, 255]
        } else if y < 2 * cell_h {
            [0, 255, 0, 255]
        } else {
            [0, 0, 255, 255]
        };
        for _ in 0..w {
            data.extend_from_slice(&color);
        }
    }

    let mut frame = Frame::new(6, 6);
    for cell in &mut frame.cells {
        cell.ch = ' ' as u32;
        cell.fg = 0x00ff_ffff;
        cell.bg = theme.bg();
    }
    frame.full = true;
    frame.images.push(InlineImage {
        id: 1,
        source_id: 1,
        row_id: 1,
        row: -1,
        col: 0,
        width: w,
        height: h,
        data: data.into(),
        z_index: -1,
        above_text: false,
    });
    frame.image_placements.push(ImagePlacementMeta {
        placement_id: 1,
        image_id: 1,
        row_id: 1,
        source_rect: None,
        cells_w: 6,
        cells_h: 3,
        z_index: -1,
        above_text: false,
    });
    renderer.draw_without_cursor(&frame);

    let x = renderer.width / 2;
    let pixel = |renderer: &Renderer, row: usize, y_off: usize| {
        let y = row * renderer.cell_h + y_off;
        renderer.pixels()[y * renderer.width + x]
    };
    assert_eq!(
        pixel(&renderer, 0, 8),
        pack(0x0000_ff00),
        "row0 应显示中间绿色带"
    );
    assert_eq!(
        pixel(&renderer, 1, 8),
        pack(0x0000_00ff),
        "row1 应显示底部蓝色带"
    );
    assert_eq!(
        pixel(&renderer, 2, 8),
        pack(theme.bg()),
        "row2 不应有图片像素"
    );

    // 图片完全滚出视口：之前画在 row0/row1 的像素必须擦掉。
    frame.images[0].row = -5;
    frame.full = false;
    frame.damage.clear();
    renderer.draw_without_cursor(&frame);
    for row in 0..6 {
        assert_eq!(
            pixel(&renderer, row, 8),
            pack(theme.bg()),
            "图片滚出后 row{row} 必须清空"
        );
    }
}
