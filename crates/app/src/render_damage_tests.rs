// SPDX-License-Identifier: GPL-3.0-only
//! P1-06 渲染 damage 回归测试。
//!
//! 独立于 `render.rs` 的既有单测：这里只通过 `Renderer` 的公开 API 和像素缓冲
//! 做黑盒断言，覆盖「单行只重绘一行」「双缓冲轮换后旧行不丢」「full 触发矩阵」
//! 「增量结果与整屏重绘逐像素一致」四类风险。字形/光标内部细节仍由 render.rs
//! 的单测负责。

use std::rc::Rc;

use tervo_core::{flags, CellData, CursorShape, CursorState, Frame, LineDamage, Span, Theme};

use crate::glyph::FontSet;
use crate::render::Renderer;

const COLS: usize = 24;
const ROWS: usize = 6;

fn fonts() -> Option<Rc<FontSet>> {
    FontSet::load(15.0).map(Rc::new)
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
    let Some(fonts) = fonts() else { return };
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
    assert_eq!(renderer.last_copied_rows, (ROWS - 1) as u32, "整屏后的首帧要补齐未重绘行");
    assert_eq!(renderer.full_redraws, full_redraws, "partial 不应增加 full_redraws");
    assert!(ink_in_row(&renderer, 1) > 0, "上一帧第 1 行的 A 在换 buffer 后丢失");
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
    let Some(fonts) = fonts() else { return };
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
    assert_eq!(renderer.full_redraws, before, "连续按键不应累积 full_redraws");
}

#[test]
fn full_frame_draws_every_row() {
    let Some(fonts) = fonts() else { return };
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
    let Some(fonts) = fonts() else { return };
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
    let Some(fonts) = fonts() else { return };
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
    assert_eq!(empty & 0x00ff_ffff, pack(new_bg) & 0x00ff_ffff, "主题切换后背景未更新");
    assert_eq!(empty >> 24, 255);
}

#[test]
fn resize_forces_full_and_allocates_new_frame() {
    let Some(fonts) = fonts() else { return };
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
    let Some(fonts) = fonts() else { return };
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let full_redraws = renderer.full_redraws;

    // 选区出现：第 1..=2 行。
    frame.full = false;
    frame.damage.clear();
    frame.selection = Some(Span { start_line: 1, start_col: 0, end_line: 2, end_col: 10 });
    renderer.draw_without_cursor(&frame);
    assert!(!renderer.last_draw_full);
    assert_eq!(renderer.last_draw_rows, 2);
    assert_eq!(renderer.full_redraws, full_redraws);

    // 选区收缩到第 4 行：旧 1..=2 行要恢复、新 4 行要高亮。
    frame.selection = Some(Span { start_line: 4, start_col: 0, end_line: 4, end_col: 10 });
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
    let Some(fonts) = fonts() else { return };
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let full_redraws = renderer.full_redraws;

    frame.full = false;
    frame.damage.clear();
    frame.search = Some(Span { start_line: 3, start_col: 0, end_line: 3, end_col: 5 });
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
    let Some(fonts) = fonts() else { return };
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
    assert_eq!(renderer.last_draw_rows, 0, "光标 overlay 移动不应重绘终端行");
    assert_eq!(renderer.full_redraws, full_redraws);
    assert_eq!(renderer.pixels(), before.as_slice(), "光标 overlay 改动污染了终端帧");
}

#[test]
fn frame_cursor_path_erases_old_cursor_row_on_move() {
    let Some(fonts) = fonts() else { return };
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
        !renderer.pixels()[1 * renderer.cell_h * renderer.width..2 * renderer.cell_h * renderer.width]
            .iter()
            .any(|p| *p == cursor_px),
        "旧光标行残留光标像素"
    );
    assert!(
        renderer.pixels()[3 * renderer.cell_h * renderer.width..4 * renderer.cell_h * renderer.width]
            .iter()
            .any(|p| *p == cursor_px),
        "新光标行没有画光标"
    );
}

#[test]
fn incremental_sequence_matches_fresh_full_redraw_pixel_for_pixel() {
    let Some(fonts) = fonts() else { return };
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
    frame.selection = Some(Span { start_line: 1, start_col: 0, end_line: 2, end_col: 9 });
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "选区出现");

    // 4) 带颜色背景的行变化。
    frame.cells[0].bg = 0x0031_2443;
    damage_line(&mut frame, 0);
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "彩色背景");

    // 5) 搜索高亮出现，再和选区一起变化。
    frame.search = Some(Span { start_line: 2, start_col: 2, end_line: 2, end_col: 8 });
    renderer.draw_without_cursor(&frame);
    assert_matches_fresh_redraw(&renderer, &fonts, &frame, "搜索高亮");
    frame.selection = Some(Span { start_line: 0, start_col: 0, end_line: 5, end_col: 3 });
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
fn tall_glyphs_do_not_leave_stale_pixels_in_neighbor_rows() {
    // 字形位图只做水平裁剪；这里用一批可能向上/下伸出的字符验证：只重绘本行
    // 也不会在相邻行留下上一帧的墨迹（与整屏参考逐像素一致）。
    let Some(fonts) = fonts() else { return };
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
    let Some(fonts) = fonts() else { return };
    let mut renderer = renderer(&fonts, COLS, ROWS);
    let mut frame = blank(COLS, ROWS);
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let mut front = renderer.image();

    for i in 0..20 {
        for col in 0..COLS {
            put(&mut frame, i % ROWS, col, (b'a' + (i % 26) as u8) as char, 0x00cdd6f4);
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
