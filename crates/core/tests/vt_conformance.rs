//! VT 语义回归：真实 PTY + shell 输出 → Term → Frame，断言单元格/标志/模式。
//!
//! 这些用例是终端模拟器的“行为契约”，改动解析、调色板、宽字符、备用屏时必须保持。
//! 全部用 `/bin/sh -c 'printf ...'` 产生确定输出，不依赖交互式 shell 配置。

use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use rusttabby_core::{flags, spawn, Frame, PaneConfig, PaneHandle};

fn run(script: &str) -> (PaneHandle, Box<Frame>) {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 8,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn pane");
    let frame = handle.new_frame();
    let (frame, _) = wait_for(&handle, frame, Duration::from_secs(5), |f| {
        frame_text(f).contains("END")
    });
    (handle, frame)
}

fn frame_text(frame: &Frame) -> String {
    let mut out = String::new();
    for row in 0..frame.rows {
        let line: String = frame.row(row).iter().map(|c| c.char()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn wait_for<F: Fn(&Frame) -> bool>(
    handle: &PaneHandle,
    frame: Box<Frame>,
    timeout: Duration,
    pred: F,
) -> (Box<Frame>, Duration) {
    let start = Instant::now();
    let mut frame = frame;
    loop {
        frame = handle.pull(frame);
        if pred(&frame) {
            return (frame, start.elapsed());
        }
        if start.elapsed() > timeout {
            panic!("超时等待 VT 输出，当前屏幕:\n{}", frame_text(&frame));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn cell_text(frame: &Frame, row: usize, col: usize) -> String {
    frame.cell(row, col).char().to_string()
}

#[test]
fn sgr_basic_colors_resolve_to_palette() {
    // R 红字；G 红字绿底；N 重置回默认色
    let (_h, f) = run("printf '\\033[31mR\\033[42mG\\033[0mN'; printf END");
    assert_eq!(cell_text(&f, 0, 0), "R");
    assert_eq!(cell_text(&f, 0, 1), "G");
    assert_eq!(cell_text(&f, 0, 2), "N");
    // Default 主题（Tokyo Night）的 ansi1/ansi2
    assert_eq!(f.cell(0, 0).fg, 0x00f7768e);
    assert_eq!(f.cell(0, 1).fg, 0x00f7768e);
    assert_eq!(f.cell(0, 1).bg, 0x009ece6a);
    assert_eq!(f.cell(0, 2).fg, 0x00c0caf5);
    assert_eq!(f.cell(0, 2).bg, 0x001a1b26);
}

#[test]
fn sgr_256_and_truecolor() {
    let (_h, f) = run(
        "printf '\\033[38;5;196mX\\033[38;2;1;2;3mT\\033[0mN'; printf END",
    );
    assert_eq!(f.cell(0, 0).fg, 0x00ff0000, "256 色 196 应为纯红");
    assert_eq!(f.cell(0, 1).fg, 0x00010203, "truecolor 应原样透传");
    assert_eq!(f.cell(0, 2).fg, 0x00c0caf5);
}

#[test]
fn sgr_flags_are_preserved() {
    let (_h, f) = run("printf '\\033[1;3;4;7mU\\033[0m'; printf END");
    let c = f.cell(0, 0);
    assert_eq!(c.char(), 'U');
    assert!(c.has(flags::BOLD), "bold 丢失");
    assert!(c.has(flags::ITALIC), "italic 丢失");
    assert!(c.has(flags::UNDERLINE), "underline 丢失");
    assert!(c.has(flags::INVERSE), "inverse 丢失");
}

#[test]
fn wide_cjk_spans_two_cells() {
    let (_h, f) = run("printf '你好END'");
    assert_eq!(f.cell(0, 0).char(), '你');
    assert!(f.cell(0, 0).has(flags::WIDE_CHAR), "首格缺 WIDE_CHAR");
    assert!(f.cell(0, 1).has(flags::WIDE_CHAR_SPACER), "次格缺 SPACER");
    assert_eq!(f.cell(0, 2).char(), '好');
    assert!(f.cell(0, 2).has(flags::WIDE_CHAR));
    assert!(f.cell(0, 3).has(flags::WIDE_CHAR_SPACER));
}

#[test]
fn cursor_move_and_erase_line() {
    // 打印 abc，光标左移 2 到 b，删到行尾，再打印 END → "aEND"
    let (_h, f) = run("printf 'abc\\033[2D\\033[KEND'");
    let line: String = f.row(0).iter().map(|c| c.char()).collect();
    assert_eq!(line.trim_end(), "aEND");
}

#[test]
fn autowrap_moves_to_next_line() {
    // 40 列：打满 40 个字符后第 41 个应换行
    let (_h, f) = run("printf '0123456789012345678901234567890123456789X'; printf END");
    assert_eq!(f.cell(0, 0).char(), '0');
    assert_eq!(f.cell(0, 39).char(), '9');
    assert_eq!(f.cell(1, 0).char(), 'X');
}

#[test]
fn alternate_screen_mode_roundtrip() {
    // 进入备用屏，打 ALT，退出后回到主屏（主屏内容应还在）
    let (_h, f) = run(
        "printf 'MAIN'; printf '\\033[?1049h'; printf 'ALT'; printf '\\033[?1049l'; printf END",
    );
    let mode = TermMode::from_bits_truncate(f.mode);
    assert!(!mode.contains(TermMode::ALT_SCREEN), "退出备用屏后模式位未清除");
    // 主屏内容仍应该是 MAIN（备用屏期间写的不该留在主屏）
    let line: String = f.row(0).iter().map(|c| c.char()).collect();
    assert!(line.starts_with("MAIN"), "主屏内容丢失: {line:?}");
    assert!(!line.contains("ALT"), "备用屏内容泄漏到主屏: {line:?}");
}

#[test]
fn bracketed_paste_mode_bit() {
    let (_h, f) = run("printf '\\033[?2004h'; printf END");
    let mode = TermMode::from_bits_truncate(f.mode);
    assert!(mode.contains(TermMode::BRACKETED_PASTE), "bracketed paste 模式未置位");
}

#[test]
fn clear_screen_and_home() {
    let (_h, f) = run("printf 'garbage'; printf '\\033[2J\\033[H'; printf 'OK\\n'; printf END");
    assert_eq!(cell_text(&f, 0, 0), "O");
    assert_eq!(cell_text(&f, 0, 1), "K");
    let line: String = f.row(0).iter().map(|c| c.char()).collect();
    assert_eq!(line.trim_end(), "OK");
}

#[test]
fn title_event_and_grid_coexist() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 8,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec![
            "-c".to_string(),
            "printf '\\033]0;vt-conformance-title\\007BODY'; printf END".to_string(),
        ],
        ..PaneConfig::default()
    })
    .expect("spawn");
    let frame = handle.new_frame();
    let (frame, _) = wait_for(&handle, frame, Duration::from_secs(5), |f| {
        frame_text(f).contains("END")
    });
    assert_eq!(cell_text(&frame, 0, 0), "B");
    let mut got_title = false;
    while let Some(ev) = handle.try_event() {
        if let rusttabby_core::CoreEvent::Title(t) = ev {
            assert_eq!(t, "vt-conformance-title");
            got_title = true;
        }
    }
    assert!(got_title, "OSC 0 标题事件没有送达");
}

#[test]
fn osc8_hyperlink_is_exposed_in_frame() {
    // OSC 8 ; params ; URI ST  文本  OSC 8 ; ; ST
    // printf 里 \033 = ESC，\\ = 单个反斜杠（ST 的第二字节）
    let script = "printf '\\033]8;;https://example.com/x\\033\\\\click\\033]8;;\\033\\\\\\r\\nEND'";
    let (_, frame) = run(script);

    // "click" 占第 0 行第 0..4 列，整段都应指向同一个 URI
    for col in 0..5u16 {
        assert_eq!(
            frame.hyperlink_at(0, col as usize),
            Some("https://example.com/x"),
            "第 {col} 列应带上超链接"
        );
    }
    // 链接结束后的格子不能再带上一个链接
    assert_eq!(frame.hyperlink_at(0, 5), None);
    // 没有链接的普通行也为 None
    assert_eq!(frame.hyperlink_at(1, 0), None);
}
