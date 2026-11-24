// SPDX-License-Identifier: GPL-3.0-only
//! VT220 / 常用 xterm 序列契约（TERM-001）。
//!
//! 与 `vt_editing.rs`（编辑/滚动）、`vt_parser_robustness.rs`（异常输入）互补：
//! 这里覆盖“正常程序真的会发”的序列，并区分**已验证**与**仍不覆盖**。
//!
//! 已验证（本文件 + 既有 vt_conformance/vt_editing）：
//! - 光标：CUP/CUU/CUD/CUF/CUB、CHA/VPA、DECSC/DECRC、DECOM 原点模式；
//! - 擦除/编辑：ED/EL/ECH/DCH/ICH/IL/DL、REP；
//! - 屏幕：SU/SD、DECSTBM、备用屏 1049/1047/47、DECALN；
//! - 模式：DECAWM 7、IRM 4、DECTCEM 25、bracketed paste 2004、焦点 1004；
//! - 字符集：DEC Special Graphics（`ESC ( 0` 画线字符）；
//! - 查询：DA1 (`CSI c`)、DSR 5/6 (`CSI n`)、DECRQM；
//! - 标题：OSC 0/2（既有 vt_conformance）。
//!
//! 仍不覆盖：vttest/esctest 全套、DECCOLM 132 列、8-bit C1 控制（vte 0.15 不支持，
//! 已有 `vt_parser_robustness::eight_bit_c1_controls_are_not_interpreted` 锁行为）、
//! 字符集 G0/G1 的完整 DEC  multinational、鼠标/键盘协议（见 mouse_report.rs）。

use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use xtervo_core::{spawn, CursorShape, PaneConfig, TermSim};

fn sim(cols: usize, rows: usize) -> TermSim {
    TermSim::new(cols, rows, 100)
}

fn line(sim: &mut TermSim, row: usize) -> String {
    let frame = sim.pull();
    frame.row(row).iter().map(|c| c.char()).collect::<String>().trim_end().to_string()
}

fn cursor(sim: &mut TermSim) -> (u16, u16) {
    let frame = sim.pull();
    (frame.cursor.line, frame.cursor.col)
}

#[test]
fn save_restore_cursor_decsc_decrc() {
    let mut s = sim(20, 5);
    s.advance(b"\x1b[3;5H\x1b7\x1b[1;1H\x1b8X");
    // 保存 (2,4)；回 home 再恢复，X 落在保存位置并把光标推进到 (2,5)。
    assert_eq!(cursor(&mut s), (2, 5));
    assert_eq!(line(&mut s, 2), "    X");
}

#[test]
fn insert_mode_irm_shifts_rest_of_line() {
    let mut s = sim(20, 3);
    s.advance(b"abc\x1b[1;2H\x1b[4hXY\x1b[4l");
    assert_eq!(line(&mut s, 0), "aXYbc");
    // 关闭后覆盖写，不再插入。
    s.advance(b"\x1b[1;1HZ");
    assert_eq!(line(&mut s, 0), "ZXYbc");
}

#[test]
fn rep_repeats_last_printed_char() {
    let mut s = sim(20, 3);
    s.advance(b"a\x1b[3b\x1b[2b");
    assert_eq!(line(&mut s, 0), "aaaaaa");
}

#[test]
fn decaln_fills_screen_with_e_and_homes_cursor() {
    let mut s = sim(8, 3);
    s.advance(b"\x1b#8");
    let rows: Vec<String> = (0..3).map(|r| line(&mut s, r)).collect();
    assert_eq!(rows, vec!["EEEEEEEE", "EEEEEEEE", "EEEEEEEE"]);
    assert_eq!(cursor(&mut s), (0, 0));
}

#[test]
fn dec_special_graphics_line_drawing() {
    let mut s = sim(20, 3);
    // ESC ( 0：G0 = DEC Special Graphics；l/q/k 变成 ┌─┐；ESC ( B 回 ASCII。
    s.advance(b"\x1b(0lqk\x1b(Babc");
    assert_eq!(line(&mut s, 0), "┌─┐abc");
}

#[test]
fn cursor_visibility_and_shape() {
    let mut s = sim(20, 3);
    s.advance(b"\x1b[?25l");
    assert!(!s.pull().cursor.visible, "DECTCEM 25l 应隐藏光标");
    s.advance(b"\x1b[?25h");
    assert!(s.pull().cursor.visible, "DECTCEM 25h 应显示光标");
    // DECSCUSR：1 = 闪烁块，5 = 闪烁竖线（Frame 只记录形状）。
    s.advance(b"\x1b[1 q");
    assert_eq!(s.pull().cursor.shape, CursorShape::Block);
    s.advance(b"\x1b[5 q");
    assert_eq!(s.pull().cursor.shape, CursorShape::Beam);
    s.advance(b"\x1b[3 q");
    assert_eq!(s.pull().cursor.shape, CursorShape::Underline);
}

#[test]
fn origin_mode_positions_relative_to_scroll_region() {
    let mut s = sim(20, 8);
    s.advance(b"1\r\n2\r\n3\r\n4\r\n5");
    // 滚动区 3..6，DECOM 开启后 CUP 1;1 = 区内第一行（屏幕第 3 行）。
    s.advance(b"\x1b[3;6r\x1b[?6h\x1b[1;1HX");
    assert_eq!(s.pull().cell(2, 0).char(), 'X', "DECOM 下 CUP 1;1 应落在滚动区顶");
    // 关闭 DECOM 后 CUP 1;1 回到屏幕左上。
    s.advance(b"\x1b[?6l\x1b[1;1HY");
    assert_eq!(s.pull().cell(0, 0).char(), 'Y');
}

#[test]
fn alternate_screen_1049_restores_main_screen() {
    let mut s = sim(20, 4);
    s.advance(b"MAIN\x1b[?1049hALT\x1b[?1049l");
    assert_eq!(line(&mut s, 0), "MAIN", "1049 退出后主屏内容应恢复");
    let mode = TermMode::from_bits_truncate(s.pull().mode);
    assert!(!mode.contains(TermMode::ALT_SCREEN));
}

/// 与 osc_palette.rs 相同的 printf 转义：让子进程打印查询，dd 读终端回包。
fn printf_escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            0x1b => out.push_str("\\033"),
            b'\\' => out.push_str("\\\\"),
            b'%' => out.push_str("%%"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{:03o}", b)),
        }
    }
    out
}

fn capture_reply(tag: &str, count: usize, query: &[u8]) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!("xtervo-vt220-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("reply.bin");
    let script = format!(
        "stty raw -echo; printf '{}'; timeout --foreground 5 dd bs=1 count={count} of={} 2>/dev/null",
        printf_escape(query),
        out.display()
    );
    let _handle = spawn(PaneConfig {
        cols: 80,
        rows: 24,
        scrollback: 50,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script],
        ..PaneConfig::default()
    })
    .expect("spawn pane");
    let start = Instant::now();
    loop {
        let got = std::fs::read(&out).unwrap_or_default();
        if got.len() >= count {
            let _ = std::fs::remove_dir_all(&dir);
            return got[..count].to_vec();
        }
        assert!(start.elapsed() < Duration::from_secs(5), "查询回包超时: {got:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn da1_dsr5_dsr6_and_decrqm_replies() {
    // DA1：默认 sixel 开启 → 带参数 4 的 Primary DA。
    assert_eq!(
        capture_reply("da1", 13, b"\x1b[c"),
        b"\x1b[?62;4;9;22c".to_vec(),
        "开启 Sixel 时 DA1 必须带参数 4"
    );
    // DSR 5n → CSI 0 n。
    assert_eq!(capture_reply("dsr5", 4, b"\x1b[5n"), b"\x1b[0n".to_vec());
    // DSR 6n → CSI row;col R（光标在 3;7）。
    assert_eq!(
        capture_reply("dsr6", 6, b"\x1b[3;7H\x1b[6n"),
        b"\x1b[3;7R".to_vec()
    );
    // DECRQM：CSI ? 25 $ p → 已设置 = 1。
    assert_eq!(
        capture_reply("decrqm", 9, b"\x1b[?25$p"),
        b"\x1b[?25;1$y".to_vec()
    );
}

#[test]
fn focus_reporting_mode_bit() {
    let mut s = sim(20, 3);
    s.advance(b"\x1b[?1004h");
    let mode = TermMode::from_bits_truncate(s.pull().mode);
    assert!(mode.contains(TermMode::FOCUS_IN_OUT));
    s.advance(b"\x1b[?1004l");
    let mode = TermMode::from_bits_truncate(s.pull().mode);
    assert!(!mode.contains(TermMode::FOCUS_IN_OUT));
}

#[test]
fn frame_mode_matches_term_mode_for_common_flags() {
    let mut s = sim(20, 3);
    s.advance(b"\x1b[?1049h\x1b[?2004h\x1b[?1h");
    let frame = s.pull();
    let mode = TermMode::from_bits_truncate(frame.mode);
    assert!(mode.contains(TermMode::ALT_SCREEN));
    assert!(mode.contains(TermMode::BRACKETED_PASTE));
    assert!(mode.contains(TermMode::APP_CURSOR));
}
