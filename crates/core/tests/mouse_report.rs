//! 鼠标上报契约：模式位 → xterm 鼠标序列。
//!
//! alacritty_terminal 只跟踪 1000/1002/1003/1005/1006 的模式位，**不生成**转义序列
//! （那部分在 alacritty 的 app 层）。所以编码器是我们自己的，必须逐字节钉死：
//! vim / htop / tmux 全靠这几个字节判断点击落在哪一格。

use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use rusttabby_core::mouse::{self, Modifiers, MouseAction, MouseButton, MouseEvent};
use rusttabby_core::{spawn, PaneConfig, PaneHandle};

fn ev(button: MouseButton, action: MouseAction, line: u16, col: u16) -> MouseEvent {
    MouseEvent { button, action, line, col, modifiers: Modifiers::default() }
}

fn sgr() -> TermMode {
    TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

// ---------------------------------------------------------------- SGR 1006

#[test]
fn sgr_left_press_is_one_based() {
    let out = mouse::encode(ev(MouseButton::Left, MouseAction::Press, 1, 2), sgr()).unwrap();
    // 视口 (row=1, col=2) → 报文 (y=2, x=3)
    assert_eq!(text(&out), "\x1b[<0;3;2M");
}

#[test]
fn sgr_release_uses_lowercase_m() {
    let out = mouse::encode(ev(MouseButton::Left, MouseAction::Release, 1, 2), sgr()).unwrap();
    assert_eq!(text(&out), "\x1b[<0;3;2m");
}

#[test]
fn sgr_motion_sets_bit_32() {
    // 移动只在 1002（按住拖动）或 1003（悬停）下有意义，1000 由下面的用例覆盖。
    let mode = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
    let out = mouse::encode(ev(MouseButton::Left, MouseAction::Motion, 0, 0), mode).unwrap();
    assert_eq!(text(&out), "\x1b[<32;1;1M");
}

#[test]
fn sgr_hover_motion_uses_button_three() {
    let mode = TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE;
    let out = mouse::encode(ev(MouseButton::None, MouseAction::Motion, 0, 0), mode).unwrap();
    // 无按键移动 = 3 + 32
    assert_eq!(text(&out), "\x1b[<35;1;1M");
}

#[test]
fn sgr_modifiers_are_shift_4_alt_8_ctrl_16() {
    let mut e = ev(MouseButton::Left, MouseAction::Press, 0, 0);
    e.modifiers = Modifiers { shift: true, alt: false, ctrl: false };
    assert_eq!(text(&mouse::encode(e, sgr()).unwrap()), "\x1b[<4;1;1M");

    e.modifiers = Modifiers { shift: false, alt: true, ctrl: false };
    assert_eq!(text(&mouse::encode(e, sgr()).unwrap()), "\x1b[<8;1;1M");

    e.modifiers = Modifiers { shift: false, alt: false, ctrl: true };
    assert_eq!(text(&mouse::encode(e, sgr()).unwrap()), "\x1b[<16;1;1M");

    e.modifiers = Modifiers { shift: true, alt: true, ctrl: true };
    assert_eq!(text(&mouse::encode(e, sgr()).unwrap()), "\x1b[<28;1;1M");
}

#[test]
fn sgr_right_and_middle_buttons() {
    assert_eq!(
        text(&mouse::encode(ev(MouseButton::Right, MouseAction::Press, 0, 0), sgr()).unwrap()),
        "\x1b[<2;1;1M"
    );
    assert_eq!(
        text(&mouse::encode(ev(MouseButton::Middle, MouseAction::Press, 0, 0), sgr()).unwrap()),
        "\x1b[<1;1;1M"
    );
}

#[test]
fn sgr_wheel_is_press_only() {
    assert_eq!(
        text(&mouse::encode(ev(MouseButton::WheelUp, MouseAction::Press, 0, 0), sgr()).unwrap()),
        "\x1b[<64;1;1M"
    );
    assert_eq!(
        text(&mouse::encode(ev(MouseButton::WheelDown, MouseAction::Press, 0, 0), sgr()).unwrap()),
        "\x1b[<65;1;1M"
    );
    // 滚轮没有 release 语义；编码器必须吞掉，否则 vim 会收到多余事件。
    assert!(mouse::encode(ev(MouseButton::WheelUp, MouseAction::Release, 0, 0), sgr()).is_none());
}

// ------------------------------------------------------------- legacy X10

#[test]
fn legacy_encodes_32_offset_bytes() {
    let out = mouse::encode(ev(MouseButton::Left, MouseAction::Press, 1, 2), TermMode::MOUSE_REPORT_CLICK)
        .unwrap();
    assert_eq!(out, vec![0x1b, b'[', b'M', 32 + 0, 32 + 3, 32 + 2]);
}

#[test]
fn legacy_release_reports_button_three() {
    let out = mouse::encode(ev(MouseButton::Left, MouseAction::Release, 0, 0), TermMode::MOUSE_REPORT_CLICK)
        .unwrap();
    assert_eq!(out, vec![0x1b, b'[', b'M', 32 + 3, 32 + 1, 32 + 1]);
}

#[test]
fn legacy_clamps_coordinates_to_223() {
    let out = mouse::encode(ev(MouseButton::Left, MouseAction::Press, 299, 300), TermMode::MOUSE_REPORT_CLICK)
        .unwrap();
    assert_eq!(out[3..], [32, 255, 255]);
}

#[test]
fn utf8_mode_emits_multibyte_codepoint() {
    let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
    let out = mouse::encode(ev(MouseButton::Left, MouseAction::Press, 0, 299), mode).unwrap();
    // 32 + (299 + 1) = 332 → U+014C，UTF-8 两字节
    assert_eq!(out[..3], [0x1b, b'[', b'M']);
    assert_eq!(out[3], 32);
    // x 两个 UTF-8 字节；y 仍是单字节（32 + 1）
    assert_eq!(&out[4..6], "\u{14c}".as_bytes());
    assert_eq!(out[6], 33);
}

// ---------------------------------------------------------- 模式门禁

#[test]
fn no_mouse_mode_means_no_report() {
    assert!(!mouse::reporting(TermMode::empty()));
    assert!(mouse::encode(ev(MouseButton::Left, MouseAction::Press, 0, 0), TermMode::empty()).is_none());
    // 备用屏/光标等无关模式不得误触发
    assert!(!mouse::reporting(TermMode::ALT_SCREEN | TermMode::SHOW_CURSOR | TermMode::SGR_MOUSE));
}

#[test]
fn click_only_mode_drops_drag_motion() {
    // 1000：只报按下/抬起，拖动移动必须丢弃，否则 vim 里拖选会乱跳。
    assert!(mouse::encode(ev(MouseButton::Left, MouseAction::Motion, 0, 0), sgr()).is_none());
}

#[test]
fn drag_mode_reports_motion_while_pressed() {
    let mode = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
    let out = mouse::encode(ev(MouseButton::Left, MouseAction::Motion, 0, 0), mode).unwrap();
    assert_eq!(text(&out), "\x1b[<32;1;1M");
    assert!(mouse::encode(ev(MouseButton::None, MouseAction::Motion, 0, 0), mode).is_none());
}

#[test]
fn motion_mode_reports_hover() {
    let mode = TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE;
    assert!(mouse::encode(ev(MouseButton::None, MouseAction::Motion, 0, 0), mode).is_some());
}

// ------------------------------------------------------- actor 端到端

/// 真 PTY：shell 先打开 1000+1006，再把收到的字节原样落盘。
/// 用于证明「模式门禁在 actor 里、字节确实写到了 PTY」，而不只是编码器单测。
#[test]
fn actor_writes_reported_bytes_to_pty() {
    let dir = std::env::temp_dir().join(format!("rt-mouse-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("bytes.bin");
    let _ = std::fs::remove_file(&out);

    // 期望长度：SGR 左键按下 "\x1b[<0;3;2M" = 9 字节
    let script = format!(
        "printf '\\033[?1000h\\033[?1006h'; stty raw -echo 2>/dev/null; dd bs=1 count=9 of={} 2>/dev/null; printf 'DONE'",
        out.display()
    );
    let handle = spawn(PaneConfig {
        cols: 20,
        rows: 5,
        scrollback: 10,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script],
        ..PaneConfig::default()
    })
    .expect("spawn pane");

    wait_mouse_mode(&handle, Duration::from_secs(5));

    handle.mouse(ev(MouseButton::Left, MouseAction::Press, 1, 2));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = Vec::new();
    while Instant::now() < deadline {
        got = std::fs::read(&out).unwrap_or_default();
        if got.len() >= 9 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(text(&got), "\x1b[<0;3;2M");
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

fn wait_mouse_mode(handle: &PaneHandle, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if handle.wants_mouse() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("5s 内没有看到 1000/1006 模式位");
}
