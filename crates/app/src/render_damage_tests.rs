// SPDX-License-Identifier: GPL-3.0-only
//! P1-06 渲染 damage 回归测试。
//!
//! 独立于 `render.rs` 的既有单测：这里只通过 `Renderer` 的公开 API 和像素缓冲
//! 做黑盒断言，覆盖「单行只重绘一行」「双缓冲轮换后旧行不丢」「full 触发矩阵」
//! 「增量结果与整屏重绘逐像素一致」四类风险。字形/光标内部细节仍由 render.rs
//! 的单测负责。

use std::rc::Rc;

use xtervo_core::{flags, CellData, CursorShape, CursorState, Frame, LineDamage, Span, Theme};

use crate::glyph::FontSet;
use crate::render::Renderer;
use crate::test_support::require_test_fonts;

const COLS: usize = 24;
const ROWS: usize = 6;

fn fonts() -> Rc<FontSet> {
    require_test_fonts()
}

fn theme() -> Theme {
    Theme::default()
}

/// 主题背景 0x00RRGGBB -> 像素缓冲里的内存序 RGBA（不透明）。
fn pack(c: u32) -> u32 {
    let r = c >> 16 & 0xff;
    let g = c >> 8 & 0xff;
    let b = c & 0xff;
    r | g << 8 | b << 16 | 0xff00_0000
}

fn blank(cols: usize, rows: usize) -> Frame {
    let bg = theme().bg();
    let mut frame = Frame::new(cols, rows);
    for cell in &mut frame.cells {
        cell.bg = bg;
    }
    frame
}

fn renderer(fonts: &Rc<FontSet>, cols: usize, rows: usize) -> Renderer {
    Renderer::new(
        fonts.clone(),
        cols,
        rows,
        theme().bg(),
        theme().cursor(),
        1.0,
    )
}

fn put(frame: &mut Frame, row: usize, col: usize, ch: char, fg: u32) {
    let mut cell = CellData::default();
    cell.ch = ch as u32;
    cell.fg = fg;
    cell.bg = theme().bg();
    frame.cells[row * frame.cols + col] = cell;
}

/// 单行 damage（整行宽度，和 core 的 LineDamage 一样是列区间）。
fn damage_line(frame: &mut Frame, row: usize) {
    frame.full = false;
    frame.damage.clear();
    frame.damage.push(LineDamage {
        line: row as u16,
        left: 0,
        right: frame.cols.saturating_sub(1) as u16,
    });
}

fn damage_all(frame: &mut Frame) {
    frame.full = false;
    frame.damage.clear();
    for line in 0..frame.rows {
        frame.damage.push(LineDamage {
            line: line as u16,
            left: 0,
            right: frame.cols.saturating_sub(1) as u16,
        });
    }
}

fn pixel(renderer: &Renderer, x: usize, y: usize) -> u32 {
    renderer.pixels()[y * renderer.width + x]
}

fn ink_in_row(renderer: &Renderer, row: usize) -> usize {
    let bg = pack(theme().bg());
    let y0 = row * renderer.cell_h;
    let y1 = y0 + renderer.cell_h;
    renderer.pixels()[y0 * renderer.width..y1 * renderer.width]
        .iter()
        .filter(|p| **p != bg)
        .count()
}

/// 参考实现：同一帧用全新 Renderer 整屏重绘，必须和增量结果逐像素一致。
fn assert_matches_fresh_redraw(
    incremental: &Renderer,
    fonts: &Rc<FontSet>,
    frame: &Frame,
    step: &str,
) {
    let mut fresh = renderer(fonts, frame.cols, frame.rows);
    let mut reference = frame.clone();
    reference.full = true;
    reference.damage.clear();
    fresh.draw_without_cursor(&reference);
    let (a, b) = (incremental.pixels(), fresh.pixels());
    if let Some(i) = a.iter().zip(b).position(|(x, y)| x != y) {
        let width = incremental.width;
        let (x, y) = (i % width, i / width);
        panic!(
            "增量重绘与整屏重绘不一致（{step}）：首个差异像素 x={x} y={y} \
             cell_row={}（cell_h={}）incremental={:#010x} fresh={:#010x}",
            y / incremental.cell_h.max(1),
            incremental.cell_h,
            a[i],
            b[i]
        );
    }
}

#[test]
fn single_row_damage_redraws_only_one_row_and_keeps_others() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);

    // 先画一行可识别的 A，再只脏别的行，验证轮换后 A 仍在。
    put(&mut frame, 1, 2, 'A', 0x00ff_ffff);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    assert!(renderer.last_draw_full, "首帧必须整屏");
    let full_redraws = renderer.full_redraws;

    put(&mut frame, 3, 5, 'Z', 0x00ff_ffff);
    damage_line(&mut frame, 3);
    renderer.draw_without_cursor(&frame);

    assert!(!renderer.last_draw_full, "单行 damage 不应整屏");
    assert_eq!(renderer.last_draw_rows, 1, "单行 damage 只应重绘 1 行");
    // 上一帧是整屏，新 buffer 缺全部行；本帧重绘的行跳过复制，其余行补过来。
    assert_eq!(
        renderer.last_copied_rows,
        (ROWS - 1) as u32,
        "整屏后的首帧要补齐未重绘行"
    );
    assert_eq!(
        renderer.full_redraws, full_redraws,
        "partial 不应增加 full_redraws"
    );
    assert!(
        ink_in_row(&renderer, 1) > 0,
        "上一帧第 1 行的 A 在换 buffer 后丢失"
    );
    assert!(ink_in_row(&renderer, 3) > 0, "本帧第 3 行的 Z 未画出");

    // 再换一次行：上一帧改动的第 3 行要在轮换时复制到新 buffer。
    put(&mut frame, 4, 5, 'M', 0x00ff_ffff);
    damage_line(&mut frame, 4);
    renderer.draw_without_cursor(&frame);
    assert_eq!(renderer.last_draw_rows, 1);
    assert_eq!(renderer.last_copied_rows, 1, "应复制上一帧唯一脏行");
    assert!(ink_in_row(&renderer, 1) > 0, "连续轮换后第 1 行丢失");
    assert!(ink_in_row(&renderer, 3) > 0, "连续轮换后第 3 行丢失");
    assert!(ink_in_row(&renderer, 4) > 0, "本帧第 4 行未画出");
}

#[test]
fn typing_burst_never_grows_full_redraws() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let before = renderer.full_redraws;

    for i in 0..12 {
        let row = i % ROWS;
        put(&mut frame, row, i % COLS, 'k', 0x00cdd6f4);
        damage_line(&mut frame, row);
        renderer.draw_without_cursor(&frame);
        assert!(!renderer.last_draw_full, "第 {i} 次按键误判为整屏");
        assert_eq!(renderer.last_draw_rows, 1, "第 {i} 次按键重绘行数不为 1");
    }
    assert_eq!(
        renderer.full_redraws, before,
        "连续按键不应累积 full_redraws"
    );
}

#[test]
fn full_frame_draws_every_row() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    assert!(renderer.last_draw_full);
    assert_eq!(renderer.last_draw_rows, ROWS as u32);
    assert_eq!(renderer.last_copied_rows, 0);

    frame.full = true;
    renderer.draw_without_cursor(&frame);
    assert_eq!(renderer.full_redraws, 2);
}

#[test]
fn all_rows_damage_takes_full_path_even_without_frame_full() {
    // 普通屏幕滚动时 alacritty 给所有行 damage、frame.full=false；
    // 渲染器应识别为整屏，而不是逐行 blend 后再复制半屏。
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let before = renderer.full_redraws;

    damage_all(&mut frame);
    renderer.draw_without_cursor(&frame);
    assert!(renderer.last_draw_full);
    assert_eq!(renderer.last_draw_rows, ROWS as u32);
    assert_eq!(renderer.full_redraws, before + 1);
}

#[test]
fn theme_change_forces_full_and_uses_new_background() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let before = renderer.full_redraws;

    let new_bg = 0x0012_3456;
    // app 在主题切换后会从 core 拉到新调色板的 cell 背景。
    for cell in &mut frame.cells {
        cell.bg = new_bg;
    }
    renderer.set_theme(new_bg, 0x00ff_0000);
    damage_line(&mut frame, 2);
    renderer.draw_without_cursor(&frame);

    assert!(renderer.last_draw_full, "set_theme 后首帧必须整屏");
    assert_eq!(renderer.last_draw_rows, ROWS as u32);
    assert_eq!(renderer.full_redraws, before + 1);
    let empty = pixel(&renderer, renderer.width - 1, renderer.height - 1);
    assert_eq!(
        empty & 0x00ff_ffff,
        pack(new_bg) & 0x00ff_ffff,
        "主题切换后背景未更新"
    );
    assert_eq!(empty >> 24, 255);
}

#[test]
fn resize_forces_full_and_allocates_new_frame() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let before = renderer.full_redraws;

    renderer.resize_grid(10, 3);
    let mut resized = blank(10, 3);
    damage_line(&mut resized, 1);
    renderer.draw_without_cursor(&resized);
    assert!(renderer.last_draw_full, "resize 后首帧必须整屏");
    assert_eq!(renderer.last_draw_rows, 3);
    assert_eq!(renderer.full_redraws, before + 1);
    assert_eq!(renderer.pixels().len(), renderer.width * renderer.height);
}

#[test]
fn selection_change_redraws_only_old_and_new_spans() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let full_redraws = renderer.full_redraws;

    // 选区出现：第 1..=2 行。
    frame.full = false;
    frame.damage.clear();
    frame.selection = Some(Span {
        start_line: 1,
        start_col: 0,
        end_line: 2,
        end_col: 10,
    });
    renderer.draw_without_cursor(&frame);
    assert!(!renderer.last_draw_full);
    assert_eq!(renderer.last_draw_rows, 2);
    assert_eq!(renderer.full_redraws, full_redraws);

    // 选区收缩到第 4 行：旧 1..=2 行要恢复、新 4 行要高亮。
    frame.selection = Some(Span {
        start_line: 4,
        start_col: 0,
        end_line: 4,
        end_col: 10,
    });
    renderer.draw_without_cursor(&frame);
    assert_eq!(renderer.last_draw_rows, 3, "旧选区 2 行 + 新选区 1 行");
    assert!(!renderer.last_draw_full);

    // 选区清除：只恢复第 4 行。
    frame.selection = None;
    renderer.draw_without_cursor(&frame);
    assert_eq!(renderer.last_draw_rows, 1);
}

#[test]
fn search_change_redraws_highlighted_rows() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let full_redraws = renderer.full_redraws;

    frame.full = false;
    frame.damage.clear();
    frame.search = Some(Span {
        start_line: 3,
        start_col: 0,
        end_line: 3,
        end_col: 5,
    });
    renderer.draw_without_cursor(&frame);
    assert!(!renderer.last_draw_full);
    assert_eq!(renderer.last_draw_rows, 1);
    assert_eq!(renderer.full_redraws, full_redraws);
    assert_eq!(
        pixel(&renderer, 0, 3 * renderer.cell_h) & 0x00ff_ffff,
        pack(0x00f9_e2af) & 0x00ff_ffff,
        "搜索高亮没有画到帧缓冲"
    );

    // 清除搜索：只重绘旧命中行并恢复原背景。
    frame.search = None;
    renderer.draw_without_cursor(&frame);
    assert_eq!(renderer.last_draw_rows, 1);
    assert_eq!(
        pixel(&renderer, 0, 3 * renderer.cell_h) & 0x00ff_ffff,
        pack(theme().bg()) & 0x00ff_ffff
    );
}

#[test]
fn cursor_overlay_move_does_not_touch_frame_pixels() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.cursor = CursorState {
        line: 1,
        col: 1,
        shape: CursorShape::Block,
        visible: true,
    };
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let before: Vec<u32> = renderer.pixels().to_vec();
    let full_redraws = renderer.full_redraws;

    // app 路径：光标由独立 overlay 画，移动光标不应重绘/重传终端帧。
    frame.full = false;
    frame.damage.clear();
    frame.cursor.line = 4;
    frame.cursor.col = 7;
    renderer.draw_without_cursor(&frame);

    assert!(!renderer.last_draw_full);
    assert_eq!(
        renderer.last_draw_rows, 0,
        "光标 overlay 移动不应重绘终端行"
    );
    assert_eq!(renderer.full_redraws, full_redraws);
    assert_eq!(
        renderer.pixels(),
        before.as_slice(),
        "光标 overlay 改动污染了终端帧"
    );
}

#[test]
fn frame_cursor_path_erases_old_cursor_row_on_move() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    let cursor_color = theme().cursor();
    frame.cursor = CursorState {
        line: 1,
        col: 2,
        shape: CursorShape::Block,
        visible: true,
    };
    frame.full = true;
    renderer.draw(&frame);
    let cursor_px = pack(cursor_color);
    assert!(
        renderer.pixels().iter().any(|p| *p == cursor_px),
        "draw() 首帧没画光标"
    );

    // 光标从第 1 行移到第 3 行：旧行必须擦除（重绘 cell），新行画光标。
    frame.full = false;
    frame.damage.clear();
    frame.cursor.line = 3;
    renderer.draw(&frame);
    assert!(!renderer.last_draw_full);
    assert_eq!(renderer.last_draw_rows, 2, "旧光标行 + 新光标行");
    assert!(
        !renderer.pixels()
            [1 * renderer.cell_h * renderer.width..2 * renderer.cell_h * renderer.width]
            .iter()
            .any(|p| *p == cursor_px),
        "旧光标行残留光标像素"
    );
    assert!(
        renderer.pixels()
            [3 * renderer.cell_h * renderer.width..4 * renderer.cell_h * renderer.width]
            .iter()
            .any(|p| *p == cursor_px),
        "新光标行没有画光标"
    );
}

#[test]
fn incremental_sequence_matches_fresh_full_redraw_pixel_for_pixel() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    // 第 0 行放一些基线内容，后面逐步制造 damage。
    for col in 0..8 {
        put(&mut frame, 0, col, 'a', 0x00cdd6f4);
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "首帧");

    // 1) 单行文本变化。
    put(&mut frame, 2, 3, 'Q', 0x00f38ba8);
    damage_line(&mut frame, 2);
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "单行文本");

    // 2) 宽字符 + spacer。
    let mut wide = CellData::default();
    wide.ch = '你' as u32;
    wide.fg = 0x00a6e3a1;
    wide.bg = theme().bg();
    wide.flags = flags::WIDE_CHAR;
    frame.cells[3 * COLS + 4] = wide;
    let mut spacer = CellData::default();
    spacer.bg = theme().bg();
    spacer.flags = flags::WIDE_CHAR_SPACER;
    frame.cells[3 * COLS + 5] = spacer;
    damage_line(&mut frame, 3);
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "宽字符");

    // 3) 选区出现（无 damage，只靠 selection 变化补齐）。
    frame.full = false;
    frame.damage.clear();
    frame.selection = Some(Span {
        start_line: 1,
        start_col: 0,
        end_line: 2,
        end_col: 9,
    });
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "选区出现");

    // 4) 带颜色背景的行变化。
    frame.cells[0].bg = 0x0031_2443;
    damage_line(&mut frame, 0);
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "彩色背景");

    // 5) 搜索高亮出现，再和选区一起变化。
    frame.search = Some(Span {
        start_line: 2,
        start_col: 2,
        end_line: 2,
        end_col: 8,
    });
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "搜索高亮");
    frame.selection = Some(Span {
        start_line: 0,
        start_col: 0,
        end_line: 5,
        end_col: 3,
    });
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "选区扩大");

    // 6) 下划线/删除线路径。
    frame.cells[4 * COLS + 6].flags = flags::UNDERLINE | flags::STRIKEOUT;
    damage_line(&mut frame, 4);
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "下划线/删除线");

    // 7) 清空选区/搜索，旧高亮行必须恢复。
    frame.selection = None;
    frame.search = None;
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "清除选区/搜索");

    // 8) 光标 overlay 移动不产生帧变化。
    frame.cursor.line = 5;
    frame.cursor.col = 20;
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "光标移动");

    // 9) 一帧整屏后立即回到单行 damage：下一帧要复制上一帧的全部行，
    //    不能把全屏内容留在旧 buffer 里。
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "整屏重绘");
    put(&mut frame, 1, 1, 'R', 0x00f9e2af);
    damage_line(&mut frame, 1);
    renderer.draw_without_cursor(&frame);
    assert_eq!(renderer.last_copied_rows, (ROWS - 1) as u32);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "整屏后单行");
}

#[test]
fn image_partially_above_viewport_does_not_write_outside_its_rows() {
    use std::sync::Arc;
    use xtervo_core::InlineImage;

    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);

    // 顶部滚出视口：row=-1，高度 3 行。旧 dest_y0 公式会把它整体下移
    // 22px，写到 row_span 之外的行；该行没有 damage，增量渲染就留下残影。
    let cell_h = fonts.cell_h as u32;
    let cell_w = fonts.cell_w as u32;
    frame.images.push(InlineImage {
        id: 9,
        source_id: 9,
        row_id: 9,
        row: -1,
        col: 4,
        width: cell_w,
        height: 3 * cell_h,
        data: Arc::from(vec![0u8, 255, 0, 255].repeat((cell_w * 3 * cell_h) as usize)),
        z_index: -1,
        above_text: false,
    });
    frame.full = false;
    frame.damage.clear();
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "image 顶部滚出视口");
}

#[test]
fn inline_image_scroll_removal_matches_fresh_full_redraw() {
    use std::sync::Arc;
    use xtervo_core::{ImageCrop, ImagePlacementMeta, InlineImage};

    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    for row in 0..ROWS {
        put(&mut frame, row, 1, 'A', 0x00ff_ffff);
    }
    let mut image = InlineImage {
        id: 7,
        source_id: 7,
        row_id: 7,
        row: 1,
        col: 2,
        width: 3,
        height: 3,
        data: Arc::from(vec![255u8, 0, 0, 255].repeat(9)),
        z_index: -1,
        above_text: false,
    };
    frame.images.push(image.clone());
    frame.image_placements.push(ImagePlacementMeta {
        placement_id: 7,
        image_id: 7,
        row_id: 7,
        source_rect: Some(ImageCrop {
            x: 0,
            y: 1,
            w: 2,
            h: 1,
        }),
        cells_w: 2,
        cells_h: 1,
        z_index: -1,
        above_text: false,
    });
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "image 首帧");

    // 只把图片从第 1 行移动到第 4 行；文本 cells 不变，damage 列表为空。
    image.row = 4;
    frame.images[0] = image.clone();
    frame.full = false;
    frame.damage.clear();
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "image 滚动");

    // 删除图片；旧位置行必须被清掉。
    frame.images.clear();
    frame.full = false;
    frame.damage.clear();
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "image 删除");
}

#[test]
fn tall_glyphs_do_not_leave_stale_pixels_in_neighbor_rows() {
    // 字形位图只做水平裁剪；这里用一批可能向上/下伸出的字符验证：只重绘本行
    // 也不会在相邻行留下上一帧的墨迹（与整屏参考逐像素一致）。
    let fonts = fonts();
    let mut renderer = renderer(&fonts, 8, 5);
    let mut frame = blank(8, 5);
    for col in 0..8 {
        put(&mut frame, 1, col, 'm', 0x00cdd6f4);
        put(&mut frame, 3, col, 'w', 0x00a6e3a1);
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);

    let candidates = [
        'g', 'j', 'y', 'Q', '(', '|', 'Ä', 'Ö', '你', '好', '│', '─', '▀', '█', '▌', '😀', '❤',
        '\u{F121}', '\u{2502}', '\u{FF5C}',
    ];
    for (i, ch) in candidates.iter().enumerate() {
        // 每轮把字符放到第 2 行不同列；第 1/3 行有内容，若旧字形渗漏到邻行，
        // 参考整屏会擦掉而增量不会，逐像素比较会失败。
        let col = i % 8;
        put(&mut frame, 2, col, *ch, 0x00f9e2af);
        damage_line(&mut frame, 2);
        renderer.draw_without_cursor(&frame);
        assert_matches_fresh_redraw(&renderer, &fonts, &frame, &format!("字形 {ch:?} @col{col}"));
    }
}

#[test]
fn partial_redraw_does_not_detach_shared_buffers() {
    // 模拟 app：上一帧的 Image 持有 front buffer，直到本帧 draw 之后才被替换。
    // 只要轮换/复制逻辑正确，写另一块 buffer 不应触发整屏 CoW 拷贝。
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let mut front = renderer.image();

    for i in 0..20 {
        for col in 0..COLS {
            put(
                &mut frame,
                i % ROWS,
                col,
                (b'a' + (i % 26) as u8) as char,
                0x00cdd6f4,
            );
        }
        damage_line(&mut frame, i % ROWS);
        renderer.draw_without_cursor(&frame);
        assert_eq!(renderer.last_draw_rows, 1);
        front = renderer.image();
        assert!(
            !renderer.last_buffer_detached,
            "第 {i} 帧写入目标 buffer 时仍被共享，触发了整屏 detach"
        );
    }
    drop(front);
}

/// 模拟「渲染端落后 k 帧」：始终持有最近 `hold` 张 Image，然后连续做单行 damage。
/// 返回逐帧的 detach 标记。
fn run_with_frame_hold(
    fonts: &Rc<FontSet>,
    renderer: &mut Renderer,
    frame: &mut Frame,
    hold: usize,
    steps: usize,
) -> Vec<bool> {
    let mut held: Vec<slint::Image> = (0..hold).map(|_| renderer.image()).collect();
    let mut detaches = Vec::with_capacity(steps);
    for i in 0..steps {
        let row = i % ROWS;
        for col in 0..COLS {
            put(frame, row, col, (b'a' + (i % 26) as u8) as char, 0x00cdd6f4);
        }
        damage_line(frame, row);
        renderer.draw_without_cursor(frame);
        assert_eq!(renderer.last_draw_rows, 1, "第 {i} 帧应只重绘一行");
        assert_matches_fresh_redraw(renderer, fonts, frame, &format!("持有 {hold} 帧 seq={i}"));
        detaches.push(renderer.last_buffer_detached);
        // 池稳定后每帧提交的是「最近 池大小-1 帧的 damage 行」，不是整屏：
        // 池 3 时同步 2 行 + 栅格化 1 行，仍远小于一帧 6 行。
        if renderer.buffer_pool_len >= 3 {
            let budget = (renderer.buffer_pool_len - 1) as u32;
            assert!(
                renderer.last_copied_rows <= budget,
                "第 {i} 帧同步 {} 行，超过池深度预算 {budget}",
                renderer.last_copied_rows
            );
            assert!(
                renderer.last_copied_rows + renderer.last_draw_rows < ROWS as u32,
                "第 {i} 帧提交的不是 damage 行（复制 {} + 重绘 {} >= {ROWS} 行）",
                renderer.last_copied_rows,
                renderer.last_draw_rows
            );
        }
        held.push(renderer.image());
        if held.len() > hold {
            held.remove(0);
        }
    }
    detaches
}

/// 尾部稳态不允许再有整块 CoW（爬坡阶段的 detach 见各个测试）。
fn assert_steady_state(fonts: &Rc<FontSet>, renderer: &Renderer, detaches: &[bool], label: &str) {
    let tail = &detaches[detaches.len().saturating_sub(8)..];
    let bad = tail.iter().filter(|d| **d).count();
    assert_eq!(
        bad, 0,
        "{label}：稳态仍有 {bad} 帧整块 CoW（池={}）",
        renderer.buffer_pool_len
    );
    let _ = fonts;
}

#[test]
fn two_frame_hold_grows_pool_instead_of_cow_every_rotation() {
    // 模拟「Slint/合成器落后 2 帧」：上一帧和上上帧的 Image 都还没释放。
    // 2 块缓冲轮换回上上帧那块时必然被共享，`make_mut_bytes` 只能整块 CoW——
    // 旧实现 24 帧里 23 帧整块 CoW。这里要求：观测到之后扩池，之后每帧退化为
    // 「1 行同步 + 1 行栅格化」，且像素与整屏参考逐像素一致。
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    for row in 0..ROWS {
        put(&mut frame, row, 1, 'A', 0x00ff_ffff);
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);

    let detaches = run_with_frame_hold(&fonts, &mut renderer, &mut frame, 2, 24);
    let total = detaches.iter().filter(|d| **d).count();
    assert_steady_state(&fonts, &renderer, &detaches, "持有 2 帧");
    assert_eq!(renderer.buffer_pool_len, 3, "持有 2 帧需要 3 块缓冲");
    assert_eq!(renderer.buffer_growths, 1);
    // 爬坡只应付一次整块 CoW（Slint 不暴露 refcount，无法写前判定）；
    // 之后 23 帧全是「同步 damage 行 + 栅格化 damage 行」。
    assert_eq!(total, 1, "发现持有 >1 帧只应付出一次整块 CoW，之后按行提交");
    assert_eq!(renderer.buffer_detaches, 1);
}

#[test]
fn three_frame_hold_grows_pool_to_four() {
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    for row in 0..ROWS {
        put(&mut frame, row, 2, 'B', 0x00a6e3a1);
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);

    let detaches = run_with_frame_hold(&fonts, &mut renderer, &mut frame, 3, 24);
    assert_steady_state(&fonts, &renderer, &detaches, "持有 3 帧");
    assert_eq!(
        renderer.buffer_pool_len, 4,
        "池上限 4：持有 3 帧应扩到 4 块"
    );
    // 每次扩一块只会在「发现那一刻」付一次 CoW：2→3、3→4 共两次。
    assert_eq!(detaches.iter().filter(|d| **d).count(), 2);
}

#[test]
fn hold_deeper_than_pool_cap_stays_pixel_correct() {
    // 超过池上限（4 块 → 只能容忍 3 帧）时允许退回整块 CoW，但**不能花屏**：
    // 逐像素仍必须等于整屏参考，且 detach 要计数暴露而不是静默。
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    for row in 0..ROWS {
        put(&mut frame, row, 3, 'C', 0x00f38ba8);
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);

    let detaches = run_with_frame_hold(&fonts, &mut renderer, &mut frame, 6, 20);
    let total = detaches.iter().filter(|d| **d).count();
    assert_eq!(renderer.buffer_pool_len, 4, "池不应超过上限");
    assert!(total > 0, "超过池深度必须仍有 detach 计数");
    assert_eq!(renderer.buffer_detaches as usize, total);
}

#[test]
fn theme_change_after_growth_still_matches_fresh_redraw() {
    // 扩池后整屏重绘（主题/滚动历史）要重置历史：其它块整体过期，轮换时
    // 必须整块同步，否则会留下上一主题的旧行。
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    for row in 0..ROWS {
        put(&mut frame, row, 4, 'D', 0x00f9e2af);
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let _ = run_with_frame_hold(&fonts, &mut renderer, &mut frame, 2, 6);
    assert_eq!(renderer.buffer_pool_len, 3);

    let theme = Theme::default();
    renderer.set_theme(theme.bg(), theme.cursor());
    renderer.draw_without_cursor(&frame);
    assert!(renderer.last_draw_full);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "扩池后主题切换");

    // 整屏之后立刻单行 damage：其它块缺「全部行」，必须补齐。
    put(&mut frame, 5, 0, 'E', 0x00cdd6f4);
    damage_line(&mut frame, 5);
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "扩池后整屏再单行");
}

#[test]
fn resize_keeps_learned_pool_depth() {
    // 学到「渲染端持有 2 帧」之后 resize：新池直接按深度+1 分配，不该再重新
    // 爬坡（否则每次拖窗口都会在 resize 后付一次整块 CoW）。
    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    for row in 0..ROWS {
        put(&mut frame, row, 5, 'F', 0x00f9e2af);
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let _ = run_with_frame_hold(&fonts, &mut renderer, &mut frame, 2, 8);
    assert_eq!(renderer.buffer_pool_len, 3);

    renderer.resize_grid(COLS, ROWS);
    assert_eq!(renderer.buffer_pool_len, 3, "resize 后应沿用学到的池深度");
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    assert!(!renderer.last_buffer_detached);

    // resize 之后仍然持有 2 帧：稳态不能再整块 CoW。
    let mut held: Vec<slint::Image> = vec![renderer.image()];
    for i in 0..10 {
        let row = i % ROWS;
        put(
            &mut frame,
            row,
            i % COLS,
            (b'a' + (i % 26) as u8) as char,
            0x00cdd6f4,
        );
        damage_line(&mut frame, row);
        renderer.draw_without_cursor(&frame);
        assert!(
            !renderer.last_buffer_detached,
            "resize 后第 {i} 帧又整块 CoW"
        );
        assert_matches_fresh_redraw(&renderer, &fonts, &frame, &format!("resize 后 seq={i}"));
        held.push(renderer.image());
        if held.len() > 2 {
            held.remove(0);
        }
    }
}

// ---------------------------------------------------------------------------
// 内存门禁：帧缓冲池 + damage 提交的 2m/15m soak。
//
// 环境里没有 Xvfb/合成器（`scripts/soak.sh` 起不来 :143），所以这里对**本次
// 改动的那一层**做无头 soak：持续单行 damage + 渲染端落后 2 帧 + 周期整屏/
// resize，采样 RSS/VmHWM/线程/FD，并周期性做逐像素差分。用法：
//
//   XTERVO_RENDER_SOAK_SECS=120 \
//     cargo test -p xtervo-app --bin xtervo --offline soak_damage_pool -- --ignored --nocapture
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn proc_status_kb(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn proc_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
#[ignore = "内存门禁：设置 XTERVO_RENDER_SOAK_SECS 后手动跑（--ignored --nocapture）"]
fn soak_damage_pool_memory_is_bounded() {
    let secs: u64 = match std::env::var("XTERVO_RENDER_SOAK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(s) if s > 0 => s,
        _ => {
            eprintln!("[soak] XTERVO_RENDER_SOAK_SECS 未设置，跳过");
            return;
        }
    };
    let fonts = fonts();
    const SOAK_COLS: usize = 110;
    const SOAK_ROWS: usize = 34;
    let mut renderer = renderer(&fonts, SOAK_COLS, SOAK_ROWS);
    let mut frame = blank(SOAK_COLS, SOAK_ROWS);
    // 预置一段带内容的画面（与真实回滚区一样继续往行里写）。
    for row in 0..SOAK_ROWS {
        for col in 0..SOAK_COLS {
            put(
                &mut frame,
                row,
                col,
                (b'a' + (col % 26) as u8) as char,
                0x00cdd6f4,
            );
        }
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);

    let start = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(secs);
    // 模拟渲染端落后 2 帧：始终多持有一张 Image（池因此扩到 3）。
    let mut held: Vec<slint::Image> = vec![renderer.image(), renderer.image()];
    let mut frame_no: u64 = 0;
    let mut next_sample = 0.0f64;
    let mut rss_samples: Vec<(f64, u64)> = Vec::new();
    let (mut fd_base, mut thread_base) = (0usize, 0u64);
    let mut max_pool = renderer.buffer_pool_len;
    let mut pixel_checks = 0u32;

    while start.elapsed() < budget {
        frame_no += 1;
        let elapsed = start.elapsed().as_secs_f64();
        if elapsed >= next_sample {
            next_sample = elapsed + 5.0;
            let rss = proc_status_kb("VmRSS:").unwrap_or(0);
            let hwm = proc_status_kb("VmHWM:").unwrap_or(0);
            let threads = proc_status_kb("Threads:").unwrap_or(0);
            let fds = proc_fd_count();
            if fd_base == 0 {
                fd_base = fds;
                thread_base = threads;
            }
            rss_samples.push((elapsed, rss));
            max_pool = max_pool.max(renderer.buffer_pool_len);
            eprintln!(
                "[soak] t={elapsed:.0}s rss={rss}kB hwm={hwm}kB threads={threads} fds={fds} \
                 pool={} grows={} detaches={} copied={} drawn={}",
                renderer.buffer_pool_len,
                renderer.buffer_growths,
                renderer.buffer_detaches,
                renderer.last_copied_rows,
                renderer.last_draw_rows,
            );
        }

        // 每 240 帧一次 resize（换尺寸 = 重新分配池）；每 120 帧一次整屏
        // （滚动历史/主题）；其余都是单行 damage。
        if frame_no % 240 == 0 {
            let cols = SOAK_COLS - 10 * ((frame_no / 240) % 3) as usize;
            renderer.resize_grid(cols, SOAK_ROWS);
            frame = blank(cols, SOAK_ROWS);
            for row in 0..SOAK_ROWS {
                for col in 0..cols {
                    put(
                        &mut frame,
                        row,
                        col,
                        (b'0' + (col % 10) as u8) as char,
                        0x00a6e3a1,
                    );
                }
            }
            frame.full = true;
        } else if frame_no % 120 == 0 {
            frame.full = true;
        } else {
            let row = (frame_no as usize) % SOAK_ROWS;
            for col in 0..frame.cols {
                put(
                    &mut frame,
                    row,
                    col,
                    (b'a' + (frame_no % 26) as u8) as char,
                    0x00cdd6f4,
                );
            }
            damage_line(&mut frame, row);
        }
        renderer.draw_without_cursor(&frame);
        held.push(renderer.image());
        if held.len() > 2 {
            held.remove(0);
        }
        max_pool = max_pool.max(renderer.buffer_pool_len);

        // 周期性逐像素差分：memcpy 出来的行错位/残影只有对拍能发现。
        if frame_no % 300 == 0 {
            assert_matches_fresh_redraw(&renderer, &fonts, &frame, "soak 对拍");
            pixel_checks += 1;
        }
    }

    let end_rss = proc_status_kb("VmRSS:").unwrap_or(0);
    let end_fds = proc_fd_count();
    let end_threads = proc_status_kb("Threads:").unwrap_or(0);
    // 前 1/4 的峰值当基线：允许池扩容/分配器热身的固定台阶，之后必须持平。
    let warm = rss_samples.len() / 4;
    let base = rss_samples
        .iter()
        .take(warm.max(1))
        .map(|(_, rss)| *rss)
        .max()
        .unwrap_or(end_rss);
    eprintln!(
        "[soak] done frames={frame_no} pixel_checks={pixel_checks} base_rss={base}kB \
         end_rss={end_rss}kB max_pool={max_pool} grows={} detaches={} fds={end_fds}",
        renderer.buffer_growths, renderer.buffer_detaches
    );
    assert!(pixel_checks > 0, "soak 没跑到差分检查（时长太短）");
    assert_eq!(max_pool, 3, "落后 2 帧时池应稳定在 3 块，实际 {max_pool}");
    assert!(
        renderer.buffer_growths <= 2,
        "整个 soak 只该扩容一次（resize 沿用学到的深度，不再爬坡），实际 {}",
        renderer.buffer_growths
    );
    assert!(
        renderer.buffer_detaches <= 2,
        "整个 soak 只该在最初发现持有深度时付 1 次整块 CoW，实际 {}",
        renderer.buffer_detaches
    );
    assert!(
        end_rss <= base + 32 * 1024,
        "RSS 从 {base}kB 涨到 {end_rss}kB（>32MB）：帧缓冲池泄漏"
    );
    assert_eq!(end_fds, fd_base, "FD 数量变化：{fd_base} -> {end_fds}");
    assert_eq!(
        end_threads, thread_base,
        "线程数变化：{thread_base} -> {end_threads}"
    );
}

fn lcg(seed: &mut u64) -> usize {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*seed >> 33) as usize
}

#[test]
fn random_damage_sequence_with_frame_hold_matches_fresh_redraw() {
    // 伪随机交错「单行/多行 damage、整屏、无变化帧、选区、内联图片移动/删除、
    // resize」，全程模拟渲染端持有 2 帧。每一步都要求与整屏参考逐像素一致：
    // 池轮换 + history 的不变式只要少同步一行就会留下残影。
    use std::sync::Arc;
    use xtervo_core::InlineImage;

    let fonts = fonts();
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    for row in 0..ROWS {
        for col in 0..COLS {
            put(&mut frame, row, col, 'x', 0x00cdd6f4);
        }
    }
    frame.full = true;
    renderer.draw_without_cursor(&frame);

    let mut held: Vec<slint::Image> = vec![renderer.image(), renderer.image()];
    let mut seed: u64 = 0x5eed;
    let cell_w = fonts.cell_w as u32;
    let cell_h = fonts.cell_h as u32;
    let mut image: Option<InlineImage> = None;

    for step in 0..140 {
        let op = lcg(&mut seed) % 10;
        match op {
            0..=4 => {
                let row = lcg(&mut seed) % frame.rows;
                let ch = (b'a' + (lcg(&mut seed) % 26) as u8) as char;
                let col = lcg(&mut seed) % frame.cols;
                put(&mut frame, row, col, ch, 0x00f38ba8);
                damage_line(&mut frame, row);
            }
            5 => {
                // 多行 damage（仍可能被判成 full）。
                frame.full = false;
                frame.damage.clear();
                for _ in 0..2 {
                    let row = lcg(&mut seed) % frame.rows;
                    for col in 0..frame.cols {
                        let ch = (b'0' + (lcg(&mut seed) % 10) as u8) as char;
                        put(&mut frame, row, col, ch, 0x00a6e3a1);
                    }
                    frame.damage.push(xtervo_core::LineDamage {
                        line: row as u16,
                        left: 0,
                        right: frame.cols as u16 - 1,
                    });
                }
            }
            6 => {
                frame.full = true;
            }
            7 => {
                // 真·无变化帧：没有 damage，也不改 cell。
                frame.full = false;
                frame.damage.clear();
            }
            8 => {
                frame.selection = if frame.selection.is_some() {
                    None
                } else {
                    let a = lcg(&mut seed) % frame.rows;
                    let b = lcg(&mut seed) % frame.rows;
                    Some(Span {
                        start_line: a.min(b) as u16,
                        start_col: 0,
                        end_line: a.max(b) as u16,
                        end_col: 3,
                    })
                };
            }
            _ => {
                // 内联图片：出现 → 挪行 → 删除。
                match image.as_mut() {
                    None => {
                        let img = InlineImage {
                            id: 11,
                            source_id: 11,
                            row_id: 11,
                            row: (lcg(&mut seed) % frame.rows) as i32,
                            col: (lcg(&mut seed) % frame.cols.max(1)) as u16,
                            width: cell_w * 2,
                            height: cell_h * 2,
                            data: Arc::from(
                                vec![0u8, 128, 255, 255].repeat((cell_w * 2 * cell_h * 2) as usize),
                            ),
                            z_index: -1,
                            above_text: false,
                        };
                        frame.images.push(img.clone());
                        image = Some(img);
                    }
                    Some(img) => {
                        if lcg(&mut seed) % 2 == 0 {
                            img.row = (lcg(&mut seed) % frame.rows) as i32;
                            frame.images[0] = img.clone();
                        } else {
                            frame.images.clear();
                            image = None;
                        }
                    }
                }
                frame.full = false;
                frame.damage.clear();
            }
        }

        renderer.draw_without_cursor(&frame);
        assert_matches_fresh_redraw(
            &renderer,
            &fonts,
            &frame,
            &format!("随机 seq={step} op={op}"),
        );
        held.push(renderer.image());
        if held.len() > 2 {
            held.remove(0);
        }
        if renderer.buffer_pool_len >= 3 {
            assert!(
                !renderer.last_buffer_detached,
                "随机 seq={step}：池已扩到 {} 仍整块 CoW",
                renderer.buffer_pool_len
            );
        }

        // 每 20 步 resize 一次（重新分配池，history 必须重建）。
        if step > 0 && step % 20 == 0 {
            let cols = 18 + 2 * (lcg(&mut seed) % 4);
            let rows = 5 + lcg(&mut seed) % 3;
            renderer.resize_grid(cols, rows);
            frame = blank(cols, rows);
            for row in 0..rows {
                for col in 0..cols {
                    put(&mut frame, row, col, 'z', 0x00f9e2af);
                }
            }
            frame.full = true;
            frame.images.clear();
            image = None;
            renderer.draw_without_cursor(&frame);
            assert_matches_fresh_redraw(
                &renderer,
                &fonts,
                &frame,
                &format!("随机 resize seq={step}"),
            );
        }
    }
    drop(held);
}
