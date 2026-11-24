// SPDX-License-Identifier: GPL-3.0-only
//! 解析健壮性：不完整/分包/非法/超长序列（TERM-002）。
//!
//! 目标不是模拟“正确序列”，而是证明**任何字节流都不会 panic、不会越界、
//! 不会把后续输出吞掉**。三层各测一遍：
//!
//! 1. `TermSim`（vte + alacritty Term）随机/极端输入：确定性 LCG，不依赖外部 fuzz 工具；
//! 2. `OscScanner`（core 旁路状态机）的超长/跨包/损坏恢复，见 `src/osc.rs` 单测；
//! 3. 真实 PTY 只做一条基本冒烟（非法字节后 shell 仍能执行命令）。
//!
//! 明确的上限（代码常量，不是“碰巧”）：
//! - 普通 OSC 正文 16 KiB（[`MAX_OSC_BYTES`]，非 52）；
//! - OSC 52 原始序列约 1.4 MiB（解码后 1 MiB 上限）；
//! - CSI 只保留状态不缓存参数，参数个数/长度由 vte 自身有界解析。
//!
//! 不覆盖：真 fuzz 目标（cargo-fuzz/libFuzzer）、并发多 PTY 乱序到达。

use xtervo_core::{spawn, PaneConfig, TermSim};

fn sim() -> TermSim {
    TermSim::new(40, 8, 200)
}

/// 确定性 LCG：测试可复现，不引入 rand 依赖。
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn byte(&mut self, alphabet: &[u8]) -> u8 {
        alphabet[(self.next() as usize) % alphabet.len()]
    }
}

/// 覆盖 VT 解析器所有状态入口的字节表：C0/C1、ESC 引导、OSC/DCS/APC、
/// UTF-8 前导/续字节、非法 0xff。
const ALPHABET: &[u8] = b"\x1b[];:?hlmHJKABCDEFGILMPSXTZ@Prstqu\x07\x08\r\n\t\x00\x7f\x9b\x9d\x9c\x90\x98\x9e\x9f\xff\xc3\xa9\xe4\xb8\xad0123456789;=\"'";

#[test]
fn random_byte_soup_never_panics_and_keeps_grid_in_bounds() {
    for seed in 0..48u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut s = sim();
        for round in 0..120 {
            let len = 1 + (rng.next() as usize) % 96;
            let mut chunk = Vec::with_capacity(len);
            for _ in 0..len {
                chunk.push(rng.byte(ALPHABET));
            }
            s.advance(&chunk);
            // 每 8 轮拉一次帧：确认 fill_frame 对任意中间状态都安全。
            if round % 8 == 0 {
                let (rows, cols) = (s.rows(), s.cols());
                let frame = s.pull();
                assert_eq!(frame.rows, rows);
                assert_eq!(frame.cols, cols);
                assert!(usize::from(frame.cursor.line) < rows, "光标越界: {seed}");
                assert!(usize::from(frame.cursor.col) < cols, "光标越界: {seed}");
            }
        }
        let _ = s.pull();
    }
}

#[test]
fn split_csi_every_byte_boundary() {
    let seq = b"\x1b[3;7H";
    for split in 0..=seq.len() {
        let mut s = sim();
        s.advance(&seq[..split]);
        s.advance(&seq[split..]);
        let frame = s.pull();
        assert_eq!((frame.cursor.line, frame.cursor.col), (2, 6), "split={split}");
    }
}

#[test]
fn split_sgr_every_byte_boundary() {
    let seq = b"\x1b[1;31mZ";
    for split in 0..=seq.len() {
        let mut s = sim();
        s.advance(&seq[..split]);
        s.advance(&seq[split..]);
        let frame = s.pull();
        assert_eq!(frame.cell(0, 0).char(), 'Z', "split={split}");
        assert!(frame.cell(0, 0).has(xtervo_core::flags::BOLD), "split={split}");
    }
}

#[test]
fn huge_parameter_values_are_clamped_not_panicking() {
    let mut s = sim();
    // 参数远超 u16：CUP/ED/IL/DL/SU/SD 都必须落在屏幕/回滚范围内。
    s.advance(b"text\x1b[99999999999999999999;99999999999999999999H");
    s.advance(b"\x1b[99999999999999999999L\x1b[99999999999999999999M");
    s.advance(b"\x1b[99999999999999999999S\x1b[99999999999999999999T");
    s.advance(b"\x1b[99999999999999999999@\x1b[99999999999999999999P");
    s.advance(b"\x1b[99999999999999999999X\x1b[99999999999999999999b");
    let (rows, cols) = (s.rows(), s.cols());
    let frame = s.pull();
    assert!(usize::from(frame.cursor.line) < rows);
    assert!(usize::from(frame.cursor.col) < cols);
}

#[test]
fn long_parameter_list_is_ignored_gracefully() {
    let mut s = sim();
    let mut seq = Vec::from(&b"\x1b["[..]);
    for i in 0..512 {
        seq.extend_from_slice(format!("{i};").as_bytes());
    }
    seq.extend_from_slice(b"mZ");
    s.advance(&seq);
    let frame = s.pull();
    assert_eq!(frame.cell(0, 0).char(), 'Z');
}

#[test]
fn incomplete_sequence_at_end_of_stream_is_not_flushed_as_text() {
    let mut s = sim();
    s.advance(b"OK\x1b[38;5;");
    // 未完成的 CSI 不能把参数当普通字符写进网格。
    let text = s.viewport_text();
    assert!(text.starts_with("OK"), "{text:?}");
    assert!(!text.contains("38;5"), "CSI 参数泄漏进了网格: {text:?}");
}

#[test]
fn interrupted_osc_is_dropped_and_stream_recovers() {
    // OSC 内出现 ESC + 非字符串引导字符：整条丢弃，后续文本照常解析。
    // （ESC X / ESC P / ESC _ 会进入 SOS/DCS/APC 字符串状态，那是另一条路径，
    // 见下一个用例。）
    let mut s = sim();
    s.advance(b"\x1b]9;never-delivered\x1bqafter");
    let text = s.viewport_text();
    assert!(text.contains("after"), "损坏 OSC 后输出被吞: {text:?}");
    assert!(!text.contains("never-delivered"), "损坏 OSC 正文泄漏: {text:?}");
    // ESC [ 也能从 OSC 里中止并执行新的 CSI（0m 复位）。
    let mut s = sim();
    s.advance(b"\x1b]9;never-delivered\x1b[0mafter");
    let text = s.viewport_text();
    assert!(text.contains("after"), "CSI 中止 OSC 后输出被吞: {text:?}");
}

#[test]
fn dcs_apc_and_sos_payloads_are_swallowed_whole() {
    let mut s = sim();
    s.advance(b"A\x1bP1$q#0;2;0;0;0\x07\x1b\\B");
    s.advance(b"\x1b_kitty\x07payload\x1b\\C");
    s.advance(b"\x1bXsos data\x1b\\D\x1b^pm data\x1b\\E");
    let frame = s.pull();
    let row: String = frame.row(0).iter().map(|c| c.char()).collect();
    assert_eq!(row.trim_end(), "ABCDE", "字符串类序列必须整段吞掉: {row:?}");
}

#[test]
fn eight_bit_c1_controls_are_not_interpreted() {
    // 现状（与 alacritty/vte 0.15 一致）：只支持 7-bit ESC 形式，0x9b/0x9d/0x9c
    // 按普通字节丢弃。这里锁住行为，避免以后误以为 C1 已支持。
    let mut s = sim();
    s.advance(b"A\x9b2;3HX");
    let frame = s.pull();
    assert_eq!(frame.cell(0, 0).char(), 'A');
    assert_eq!(frame.cell(0, 1).char(), '2', "0x9b 不应被当成 CSI 起点");
    assert_eq!(frame.cell(0, 2).char(), ';');
}

#[test]
fn invalid_utf8_interleaved_with_escapes_does_not_break_following_output() {
    let mut s = sim();
    s.advance(b"\xff\xfe\xc3(\x1b[1;1H\xe4\xb8");
    s.advance(b"\xad\x00END");
    let text = s.viewport_text();
    assert!(text.contains("END"), "非法 UTF-8 后输出丢失: {text:?}");
}

#[test]
fn megabyte_of_broken_escapes_does_not_panic() {
    let mut s = sim();
    let junk = vec![0x1bu8; 64 * 1024];
    s.advance(&junk);
    s.advance(&[0x9b, b'9', b'9', b'9', b'H']);
    s.advance(b"still-alive");
    let text = s.viewport_text();
    assert!(text.contains("still-alive"), "长串 ESC 后解析器卡死: {text:?}");
}

#[test]
fn osc_overflow_does_not_leak_into_grid() {
    let mut s = sim();
    // 20 KiB OSC 正文（超过 16 KiB 上限）：整条吞掉，结束符后正常解析。
    let mut seq = Vec::from(&b"\x1b]9;"[..]);
    seq.extend(std::iter::repeat(b'A').take(20 * 1024));
    seq.push(0x07);
    seq.extend_from_slice(b"after-overflow");
    s.advance(&seq);
    let text = s.viewport_text();
    assert!(text.contains("after-overflow"), "{text:?}");
    assert!(!text.contains("AAAA"), "超长 OSC 正文泄漏进网格: {text:?}");
}

#[test]
fn real_pty_survives_invalid_bytes_and_runs_next_command() {
    // 真 PTY：先灌非法/不完整序列，再让 shell 执行 echo，证明解析器没有卡死。
    let script = "printf '\\377\\376\\033[38;5;'; printf '\\033[0m'; printf 'PTY-OK\\n'";
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 6,
        scrollback: 50,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn pane");
    let frame = handle.new_frame();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut frame = frame;
    loop {
        frame = handle.pull(frame);
        let text: String = (0..frame.rows)
            .map(|r| frame.row(r).iter().map(|c| c.char()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains("PTY-OK") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "非法字节后 PTY 解析卡住");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
