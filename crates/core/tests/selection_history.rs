// SPDX-License-Identifier: GPL-3.0-only
//! 回滚历史里的选区回归：UI 只传「可见行/列」，core 必须把它投影到 grid 坐标。
//!
//! 症状：滚到历史第二页后拖选，`copy_selection()` 要么空、要么选到的不是
//! 屏幕上看得到的那几行。这里用真实 PTY 输出足够多行，再按 UI 可见行调用
//! `selection_start/selection_extend`，断言复制结果等于屏幕上看到的文本。
//!
//! 坐标约定（与 alacritty 一致）：
//! - `selection_start/extend(line, col)` 的 `line` 是**视口行**（0 = 当前屏幕顶行）；
//! - core 内部 `grid_line = viewport_line - display_offset`；
//! - `frame.selection` 再反向映射回视口行，供 UI 画高亮。

use std::time::{Duration, Instant};

use tervo_core::{spawn, Frame, PaneConfig, PaneHandle};

const COLS: u16 = 40;
const ROWS: u16 = 6;

fn seq_pane(scrollback: usize, count: usize) -> PaneHandle {
    spawn(PaneConfig {
        cols: COLS,
        rows: ROWS,
        scrollback,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), format!("seq 1 {count}; sleep 5")],
        ..PaneConfig::default()
    })
    .expect("spawn pane")
}

fn visible_line(frame: &Frame, row: usize) -> String {
    frame.row(row).iter().map(|c| c.char()).collect::<String>().trim_end().to_string()
}

fn frame_text(frame: &Frame) -> String {
    let mut out = String::with_capacity(frame.rows * (frame.cols + 1));
    for row in 0..frame.rows {
        out.push_str(visible_line(frame, row).as_str());
        out.push('\n');
    }
    out
}

/// 反复拉帧，直到 `pred` 满足；失败时把最后屏幕打印出来。
fn wait_for<F>(handle: &PaneHandle, mut frame: Box<Frame>, pred: F) -> Box<Frame>
where
    F: Fn(&Frame) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        frame = handle.pull(frame);
        if pred(&frame) {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "超时等待 PTY/滚动状态，当前屏幕:\n{}",
            frame_text(&frame)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// 滚到 `offset` 并等到 actor 发布同一 offset，再拉一帧（UI 看到的画面）。
fn scroll_to_and_frame(handle: &PaneHandle, offset: usize) -> Box<Frame> {
    handle.scroll_to(offset);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if handle.scroll_position().offset == offset {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "超时等待滚动到 offset={offset}: {:?}",
            handle.scroll_position()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    // 滚动的下一帧必须已经按新 offset 渲染。
    let frame = wait_for(handle, handle.new_frame(), |_| true);
    assert_eq!(handle.scroll_position().offset, offset);
    frame
}

/// 按「UI 看到的可见字符」计算 simple 选区应复制出的文本。
///
/// 中间行整行选中；首/末行按列裁剪；与 alacritty `bounds_to_string` 的
/// 换行/去尾空格行为对齐（seq 输出没有行内尾随空格）。
fn expected_simple_selection(
    frame: &Frame,
    (r0, c0): (usize, usize),
    (r1, c1): (usize, usize),
) -> String {
    if (r0, c0) == (r1, c1) {
        // alacritty Simple 选区起止同格视为空。
        return String::new();
    }
    let ((sr, sc), (er, ec)) =
        if (r0, c0) <= (r1, c1) { ((r0, c0), (r1, c1)) } else { ((r1, c1), (r0, c0)) };
    let mut lines = Vec::new();
    for row in sr..=er {
        let chars: Vec<char> = visible_line(frame, row).chars().collect();
        let from = if row == sr { sc } else { 0 };
        let to = if row == er { ec } else { chars.len().saturating_sub(1) };
        let mut s = String::new();
        for col in from..=to {
            if let Some(ch) = chars.get(col) {
                s.push(*ch);
            }
        }
        lines.push(s.trim_end().to_string());
    }
    lines.join("\n")
}

#[test]
fn select_visible_rows_in_history_second_page() {
    let handle = seq_pane(200, 60);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("60"));
    let history = handle.scroll_position().history;
    assert!(history >= (ROWS as usize) * 2, "需要至少两页历史，实际 {history}");

    // 第二页：偏移 = 2 个视口高度，顶部必须是历史内容而不是第 0 行。
    let offset = (ROWS as usize) * 2;
    let frame = scroll_to_and_frame(&handle, offset);
    assert!(!visible_line(&frame, 0).is_empty(), "第二页顶部为空:\n{}", frame_text(&frame));

    // UI：从可见第 0 行第 0 列拖到可见第 2 行第 2 列。
    let expected = expected_simple_selection(&frame, (0, 0), (2, 2));
    handle.selection_start(0, 0);
    handle.selection_extend(2, 2);
    let text = handle.copy_selection().expect("历史选区应有文本");
    assert_eq!(text, expected, "第二页选中的不是可见行: {text:?}");
}

#[test]
fn select_visible_rows_at_history_top_and_bottom() {
    let handle = seq_pane(200, 60);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("60"));
    let history = handle.scroll_position().history;
    assert!(history >= 12);

    // 顶部：最旧一行是 seq 的起点 1（屏幕末行可能只是光标空行）。
    let frame = scroll_to_and_frame(&handle, history);
    assert_eq!(visible_line(&frame, 0), "1", "历史顶部:\n{}", frame_text(&frame));
    let expected = expected_simple_selection(&frame, (0, 0), (1, 0));
    handle.selection_start(0, 0);
    handle.selection_extend(1, 0);
    let text = handle.copy_selection().expect("顶部选区应有文本");
    assert_eq!(text, expected, "顶部选中的不是可见行: {text:?}");

    // 底部：在真正有文本的最后两行上反向拖选。
    let frame = scroll_to_and_frame(&handle, 0);
    let last = (0..ROWS as usize)
        .rev()
        .find(|r| !visible_line(&frame, *r).is_empty())
        .expect("底部应有文本行");
    let prev = last - 1;
    let expected = expected_simple_selection(&frame, (last, 0), (prev, 0));
    handle.selection_start(last as u16, 0);
    handle.selection_extend(prev as u16, 0);
    let text = handle.copy_selection().expect("底部反向选区应有文本");
    assert_eq!(text, expected, "底部反向选区: {text:?}");
}

#[test]
fn reverse_drag_in_history_covers_anchor_and_visible_rows() {
    let handle = seq_pane(200, 60);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("60"));

    let offset = (ROWS as usize) * 2;
    let frame = scroll_to_and_frame(&handle, offset);
    let expected = expected_simple_selection(&frame, (2, 2), (0, 0));

    // 反向：从第 2 行拖回第 0 行，两端格子都必须包含。
    handle.selection_start(2, 2);
    handle.selection_extend(0, 0);
    let text = handle.copy_selection().expect("历史反向选区应有文本");
    assert_eq!(text, expected, "历史反向拖选: {text:?}");
}

#[test]
fn selection_span_matches_visible_rows_in_history() {
    let handle = seq_pane(200, 60);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("60"));

    let offset = (ROWS as usize) * 2;
    scroll_to_and_frame(&handle, offset);
    handle.selection_start(0, 0);
    handle.selection_extend(2, 2);
    let frame = handle.pull(handle.new_frame());
    let span = frame.selection.expect("frame 里应有选区 Span");
    assert_eq!(
        (span.start_line, span.start_col, span.end_line, span.end_col),
        (0, 0, 2, 2),
        "Span 必须使用 UI 可见行坐标: {span:?}"
    );
}

/// 覆盖底部/第二页/顶部的所有可见行组合（正反向），逐格对比复制文本。
#[test]
fn selection_matrix_matches_visible_frame_at_all_offsets() {
    let handle = seq_pane(200, 80);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("80"));
    let history = handle.scroll_position().history;
    assert!(history > 20, "需要足够历史: {history}");

    let rows = ROWS as usize;
    let offsets = [
        0usize,
        1,
        rows - 1,
        rows,
        rows + 1,
        rows * 2,
        rows * 2 + 1,
        history - rows,
        history - 1,
        history,
    ];
    for offset in offsets {
        let frame = scroll_to_and_frame(&handle, offset);
        let non_empty: Vec<usize> =
            (0..rows).filter(|r| !visible_line(&frame, *r).is_empty()).collect();
        assert!(!non_empty.is_empty(), "offset={offset} 屏幕全空");
        for &r0 in &non_empty {
            for &r1 in &non_empty {
                let col0 = 0usize;
                let end_len = visible_line(&frame, r1).chars().count();
                let col1 = 2.min(end_len.saturating_sub(1));
                handle.selection_start(r0 as u16, col0 as u16);
                handle.selection_extend(r1 as u16, col1 as u16);
                let actual = handle.copy_selection().unwrap_or_default();
                let expected = expected_simple_selection(&frame, (r0, col0), (r1, col1));
                assert_eq!(
                    actual, expected,
                    "offset={offset} rows={r0}->{r1} cols={col0}->{col1}\n屏幕:\n{}",
                    frame_text(&frame)
                );

                // 反向调用（先 start 后 extend）必须得到同样的结果。
                handle.selection_start(r1 as u16, col1 as u16);
                handle.selection_extend(r0 as u16, col0 as u16);
                let actual2 = handle.copy_selection().unwrap_or_default();
                assert_eq!(actual2, actual, "offset={offset} 反向拖选不一致 rows={r0}->{r1}");
            }
        }
    }
}

/// 选区开始后跨越滚动（display_offset 改变），锚点必须仍钉在原来的 grid 行，
/// 扩展时按新的可见行继续选；复制内容等于两次可见画面覆盖的行。
#[test]
fn selection_anchor_survives_history_scroll() {
    let handle = seq_pane(200, 80);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("80"));

    let offset = (ROWS as usize) * 2;
    let start_frame = scroll_to_and_frame(&handle, offset);
    let lines: Vec<String> = (0..3).map(|r| visible_line(&start_frame, r)).collect();
    handle.selection_start(0, 0);

    // 再向上滚 3 行：原锚点行从屏幕第 0 行移到第 3 行。
    let extended_frame = scroll_to_and_frame(&handle, offset + 3);
    assert_eq!(
        visible_line(&extended_frame, 3),
        lines[0],
        "滚动后原锚点行应下移 3 行:\n{}",
        frame_text(&extended_frame)
    );
    handle.selection_extend(5, 1);
    let text = handle.copy_selection().expect("跨滚动选区应有文本");
    assert_eq!(text, format!("{}\n{}\n{}", lines[0], lines[1], lines[2]), "跨滚动锚点: {text:?}");
}

/// 选区一端滚出视口上方时，可见部分必须从第 0 列开始高亮：
/// 锚点行 cols 30.. 与可见末行 cols 0..2 若按前者裁剪，高亮会整行消失。
#[test]
fn selection_span_keeps_left_edge_when_anchor_is_above_viewport() {
    let handle = seq_pane(200, 80);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("80"));

    // offset 24：可见 52..57。从第 0 行第 30 列拖到第 5 行第 2 列。
    scroll_to_and_frame(&handle, 24);
    handle.selection_start(0, 30);
    handle.selection_extend(5, 2);

    // 向下滚 5 行：锚点行 52 滚出视口上方，选区末行 57 变成可见第 0 行。
    let frame = scroll_to_and_frame(&handle, 19);
    assert_eq!(visible_line(&frame, 0), "57", "滚动后顶部行:\n{}", frame_text(&frame));
    let span = frame.selection.expect("选区可见部分仍应有 Span");
    assert_eq!(
        (span.start_line, span.start_col, span.end_line, span.end_col),
        (0, 0, 0, 2),
        "跨视口上边界的可见部分必须从第 0 列开始: {span:?}"
    );
}

/// 选区一端滚出视口下方时，可见末行必须高亮到最后一列。
#[test]
fn selection_span_keeps_right_edge_when_end_is_below_viewport() {
    let handle = seq_pane(200, 80);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("80"));

    // offset 19：可见 57..62。从第 0 行第 2 列拖到第 5 行第 30 列。
    scroll_to_and_frame(&handle, 19);
    handle.selection_start(0, 2);
    handle.selection_extend(5, 30);

    // 向上滚 5 行：选区末行滚出视口下方，选区首行 57 变成可见第 5 行。
    let frame = scroll_to_and_frame(&handle, 24);
    assert_eq!(visible_line(&frame, 5), "57", "滚动后底部行:\n{}", frame_text(&frame));
    let span = frame.selection.expect("选区可见部分仍应有 Span");
    assert_eq!(
        (span.start_line, span.start_col, span.end_line, span.end_col),
        (5, 2, 5, COLS - 1),
        "跨视口下边界的可见部分必须高亮到最后一列: {span:?}"
    );
}

/// 默认 scrollback 就是 2000；输出超过 2000 行后历史被截断到上限，
/// 在最旧一页（第 2000 行附近）仍能按可见行选中。
#[test]
fn default_scrollback_2000_selects_at_history_limit() {
    assert_eq!(PaneConfig::default().scrollback, 2000);
    let handle = seq_pane(PaneConfig::default().scrollback, 2100);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("2100"));
    let pos = handle.scroll_position();
    let history = pos.history;
    assert_eq!(history, 2000, "历史必须被截断到默认上限: {pos:?}");

    // 顶部（最旧一页）。
    let frame = scroll_to_and_frame(&handle, history);
    assert!(!visible_line(&frame, 0).is_empty(), "历史上限顶部为空:\n{}", frame_text(&frame));
    let expected = expected_simple_selection(&frame, (0, 0), (2, 0));
    handle.selection_start(0, 0);
    handle.selection_extend(2, 0);
    let text = handle.copy_selection().expect("历史上限选区应有文本");
    assert_eq!(text, expected, "历史上限选中的不是可见行: {text:?}");
}

/// 100_000 行安全上限：不填满、不长跑，只走配置/滚动/选区坐标路径。
#[test]
fn scrollback_unlimited_cap_100k_coordinates_do_not_panic() {
    let handle = seq_pane(100_000, 30);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("30"));
    let pos = handle.scroll_position();
    assert!(pos.history > 0, "需要历史: {pos:?}");
    assert!(pos.history < 100_000, "不必填满 100k: {pos:?}");

    // 越界滚动必须夹到 history，不能 panic/回绕。
    handle.scroll_to(usize::MAX);
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.scroll_position().offset != pos.history {
        assert!(
            Instant::now() < deadline,
            "scroll_to 越界未夹到顶部: {:?}",
            handle.scroll_position()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let frame = handle.pull(handle.new_frame());
    let expected = expected_simple_selection(&frame, (0, 0), (1, 0));
    handle.selection_start(0, 0);
    handle.selection_extend(1, 0);
    let text = handle.copy_selection().expect("100k 配置下选区应有文本");
    assert_eq!(text, expected, "100k 配置选中的不是可见行: {text:?}");

    // 反向极限滚动也不 panic，并回到顶部/底部之间的合法位置。
    handle.scroll(i32::MAX);
    handle.scroll(i32::MIN);
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.scroll_position().offset != 0 {
        assert!(
            Instant::now() < deadline,
            "极限滚动后 offset 未回到 0: {:?}",
            handle.scroll_position()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let pos = handle.scroll_position();
    assert!(pos.offset <= pos.history, "offset 越界: {pos:?}");
}

/// 选区同时跨出视口上下边界时，整个可见视口都必须高亮（两端列都要补齐）。
#[test]
fn selection_span_covers_viewport_when_selection_crosses_both_edges() {
    let handle = seq_pane(200, 80);
    wait_for(&handle, handle.new_frame(), |f| frame_text(f).contains("80"));

    // 底部第 0 行起选（第 76 行），向上滚 20 行后在顶部第 0 行（第 56 行）扩展。
    scroll_to_and_frame(&handle, 0);
    handle.selection_start(0, 10);
    scroll_to_and_frame(&handle, 20);
    handle.selection_extend(0, 2);

    // 再滚到中间：选区的首尾都在视口外，可见 6 行应整行高亮。
    let frame = scroll_to_and_frame(&handle, 10);
    let span = frame.selection.expect("跨上下边界的选区应覆盖整个视口");
    assert_eq!(
        (span.start_line, span.start_col, span.end_line, span.end_col),
        (0, 0, ROWS - 1, COLS - 1),
        "跨上下边界必须覆盖整个视口: {span:?}"
    );
}
