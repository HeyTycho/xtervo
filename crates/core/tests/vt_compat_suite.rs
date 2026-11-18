// SPDX-License-Identifier: GPL-3.0-only
//! 可复跑的 VT 兼容套件（TERM-001）。
//!
//! 与 `vt_conformance`（真实 PTY 行为）、`vt220_xterm`（序列契约）、
//! `vt_editing`（编辑/制表）、`vt_parser_robustness`（异常输入）互补：
//! 这里按 vttest / esctest 的菜单分组，把**规格期望**写成表，逐条跑出
//! PASS / FAIL / XFAIL 清单。已知与规格不一致、但上游 alacritty/vte 暂不支持
//! 的条目记为 XFAIL（附原因），不阻塞回归；一旦上游修复会显示 XPASS。
//!
//! 运行（CI 式命令，退出码非 0 表示有 FAIL）：
//!
//! ```text
//! cargo test -p tervo-core --test vt_compat_suite --offline -- --report
//! ```
//!
//! `--report` 打印全部条目；不带参数只打印汇总和失败条目。套件不依赖
//! 外部 vttest/esctest 二进制，也不新增 crate。

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use tervo_core::{spawn, CursorShape, PaneConfig, TermSim};

// ---------------------------------------------------------------------------
// 结果模型
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// 规格期望，必须通过。
    Required,
    /// 上游/实现已知偏差：期望仍按规格写，失败记 XFAIL 并附原因。
    KnownBroken(&'static str),
}

struct TableCase {
    id: &'static str,
    section: &'static str,
    cols: usize,
    rows: usize,
    bytes: &'static [u8],
    /// 期望的视口文本（行尾空白已去掉）；未列出的行必须为空。
    screen: &'static [&'static str],
    cursor: Option<(u16, u16)>,
    mode_set: &'static [TermMode],
    mode_clear: &'static [TermMode],
    expect: Expect,
}

struct CustomCase {
    id: &'static str,
    section: &'static str,
    expect: Expect,
    run: fn() -> Result<(), String>,
}

#[derive(Default)]
struct Counts {
    pass: usize,
    fail: usize,
    xfail: usize,
    xpass: usize,
}

fn line_text(sim: &mut TermSim, row: usize) -> String {
    let f = sim.pull();
    f.row(row)
        .iter()
        .map(|c| c.char())
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn screen_text(sim: &mut TermSim) -> Vec<String> {
    let f = sim.pull();
    (0..f.rows)
        .map(|row| {
            f.row(row)
                .iter()
                .map(|c| c.char())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn check_table(case: &TableCase) -> Result<(), String> {
    let mut sim = TermSim::new(case.cols, case.rows, 200);
    sim.advance(case.bytes);
    let (cursor_line, cursor_col, mode_bits) = {
        let frame = sim.pull();
        (frame.cursor.line, frame.cursor.col, frame.mode)
    };
    let actual_screen = screen_text(&mut sim);
    let mut want = case
        .screen
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    if want.len() > case.rows {
        return Err(format!(
            "用例期望 {} 行，但终端只有 {} 行",
            want.len(),
            case.rows
        ));
    }
    want.resize(case.rows, String::new());
    if actual_screen != want {
        return Err(format!(
            "屏幕不符\n  期望: {want:?}\n  实际: {actual_screen:?}"
        ));
    }
    if let Some((line, col)) = case.cursor {
        if (cursor_line, cursor_col) != (line, col) {
            return Err(format!(
                "光标不符：期望 ({line},{col})，实际 ({cursor_line},{cursor_col})"
            ));
        }
    }
    let mode = TermMode::from_bits_truncate(mode_bits);
    for flag in case.mode_set {
        if !mode.contains(*flag) {
            return Err(format!("模式 {flag:?} 未置位"));
        }
    }
    for flag in case.mode_clear {
        if mode.contains(*flag) {
            return Err(format!("模式 {flag:?} 不应置位"));
        }
    }
    Ok(())
}

fn check_custom(case: &CustomCase) -> Result<(), String> {
    (case.run)()
}

// ---------------------------------------------------------------------------
// 表驱动用例：vttest 1（光标）、2（屏幕/编辑）、3（字符集）、7（光标样式）、
// xterm 模式位。所有期望按 DEC VT220 / xterm ctlseqs 写。
// ---------------------------------------------------------------------------

const TABLE: &[TableCase] = &[
    TableCase {
        id: "1.1 CUP + CHA + VPA",
        section: "vttest 1: cursor movement",
        cols: 20,
        rows: 6,
        bytes: b"\x1b[5;10H\x1b[3G\x1b[2d",
        screen: &[],
        cursor: Some((1, 2)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "1.2 CUU/CUD/CUF/CUB relative",
        section: "vttest 1: cursor movement",
        cols: 20,
        rows: 8,
        // (5,10) -2 行 +1 行 +3 列 -1 列 => (4,12) 1-based
        bytes: b"\x1b[5;10H\x1b[2A\x1b[1B\x1b[3C\x1b[1D",
        screen: &[],
        cursor: Some((3, 11)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "1.3 CUP clamps at screen edges",
        section: "vttest 1: cursor movement",
        cols: 20,
        rows: 5,
        bytes: b"\x1b[99;99H",
        screen: &[],
        cursor: Some((4, 19)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "1.4 IND / RI / NEL",
        section: "vttest 1: cursor movement",
        cols: 20,
        rows: 6,
        bytes: b"\x1b[3;5H\x1bE\x1bD\x1bM",
        screen: &[],
        cursor: Some((3, 0)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "1.5 DECSC/DECRC restores position",
        section: "vttest 1: cursor movement",
        cols: 20,
        rows: 6,
        bytes: b"\x1b[2;4H\x1b7\x1b[5;1H\x1b8X",
        screen: &["", "   X"],
        cursor: Some((1, 4)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        // DEC VT220: DECSC 保存 origin mode，DECRC 恢复。alacritty 只存光标点。
        id: "1.6 DECRC restores origin mode (spec)",
        section: "vttest 1: cursor movement",
        cols: 20,
        rows: 8,
        bytes: b"\x1b[3;6r\x1b[?6h\x1b[1;1H\x1b7\x1b[?6l\x1b[1;1H\x1b8\x1b[1;1HX",
        screen: &["", "", "X"],
        cursor: Some((2, 1)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::KnownBroken("alacritty Term 的 DECSC 只保存光标点，不保存 origin mode"),
    },
    TableCase {
        id: "2.1 DECOM CUP is relative to scroll region",
        section: "vttest 2: screen features",
        cols: 20,
        rows: 8,
        bytes: b"\x1b[3;6r\x1b[?6h\x1b[1;1HX",
        screen: &["", "", "X"],
        cursor: Some((2, 1)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.2 DECSTBM confines LF scrolling",
        section: "vttest 2: screen features",
        cols: 10,
        rows: 6,
        bytes: b"\x1b[2;4r\x1b[4;1HA\nB",
        screen: &["", "", "A", " B", "", ""],
        cursor: Some((3, 2)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.3 DECALN fills screen with E",
        section: "vttest 2: screen features",
        cols: 8,
        rows: 3,
        bytes: b"\x1b#8",
        screen: &["EEEEEEEE", "EEEEEEEE", "EEEEEEEE"],
        cursor: Some((0, 0)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.4 DECAWM off overwrites last column",
        section: "vttest 2: screen features",
        cols: 10,
        rows: 3,
        bytes: b"\x1b[?7l0123456789AB",
        screen: &["012345678B"],
        cursor: Some((0, 9)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.5 IRM insert mode shifts line",
        section: "vttest 2: screen features",
        cols: 20,
        rows: 3,
        bytes: b"abc\x1b[1;2H\x1b[4hXY\x1b[4l",
        screen: &["aXYbc"],
        cursor: Some((0, 3)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.6 EL to end of line",
        section: "vttest 2: screen features",
        cols: 20,
        rows: 3,
        bytes: b"abcdef\x1b[1;3H\x1b[K",
        screen: &["ab"],
        cursor: Some((0, 2)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.7 ICH/DCH/ECH keep text in bounds",
        section: "vttest 2: screen features",
        cols: 20,
        rows: 3,
        bytes: b"abcdef\x1b[1;2H\x1b[2@\x1b[1;1H\x1b[2P\x1b[3X",
        screen: &["   def"],
        cursor: Some((0, 0)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.8 IL/DL inside screen",
        section: "vttest 2: screen features",
        cols: 20,
        rows: 6,
        bytes: b"1\r\n2\r\n3\r\n4\r\n5\x1b[2;1H\x1b[L\x1b[3;1H\x1b[M",
        screen: &["1", "", "3", "4", "5"],
        cursor: Some((2, 0)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.9 SU/SD scroll whole screen",
        section: "vttest 2: screen features",
        cols: 20,
        rows: 6,
        bytes: b"1\r\n2\r\n3\r\n4\r\n5\x1b[2S\x1b[1T",
        screen: &["", "3", "4", "5"],
        cursor: None,
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "2.10 alternate screen 1049 restores main",
        section: "vttest 2: screen features",
        cols: 20,
        rows: 4,
        bytes: b"MAIN\x1b[?1049hALT\x1b[?1049l",
        screen: &["MAIN"],
        cursor: Some((0, 4)),
        mode_set: &[],
        mode_clear: &[TermMode::ALT_SCREEN],
        expect: Expect::Required,
    },
    TableCase {
        id: "3.1 DEC Special Graphics G0",
        section: "vttest 3: character sets",
        cols: 20,
        rows: 3,
        bytes: b"\x1b(0lqk\x1b(Babc",
        screen: &["\u{250c}\u{2500}\u{2510}abc"],
        cursor: Some((0, 6)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "3.2 G1 line drawing via SO/SI",
        section: "vttest 3: character sets",
        cols: 20,
        rows: 3,
        bytes: b"\x1b)0\x0eqx\x0fk",
        screen: &["\u{2500}\u{2502}k"],
        cursor: Some((0, 3)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "4.1 default tab stops every 8 columns",
        section: "vttest 4: tabs",
        cols: 30,
        rows: 2,
        bytes: b"a\tb\tc",
        screen: &["a       b       c"],
        cursor: Some((0, 17)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "4.2 HTS/TBC/CHT/CBT",
        section: "vttest 4: tabs",
        cols: 30,
        rows: 2,
        bytes: b"\x1b[3g\x1b[4G\x1bH\x1b[11G\x1bH\x1b[21G\x1bH\x1b[1GX\x1b[3IY\x1b[2ZZ",
        screen: &["X         Z         Y"],
        cursor: Some((0, 11)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "5.1 REP repeats last graphic char",
        section: "xterm: extensions",
        cols: 20,
        rows: 2,
        bytes: b"a\x1b[3b\x1b[2b",
        screen: &["aaaaaa"],
        cursor: Some((0, 6)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
    TableCase {
        id: "5.2 kitty/modifyOtherKeys/mouse mode bits",
        section: "xterm: extensions",
        cols: 20,
        rows: 3,
        bytes: b"\x1b[?1049h\x1b[?2004h\x1b[?1h\x1b[?25l\x1b[?1006h",
        screen: &[],
        cursor: None,
        mode_set: &[
            TermMode::ALT_SCREEN,
            TermMode::BRACKETED_PASTE,
            TermMode::APP_CURSOR,
            TermMode::SGR_MOUSE,
        ],
        mode_clear: &[TermMode::SHOW_CURSOR],
        expect: Expect::Required,
    },
    TableCase {
        id: "6.1 LNM makes LF do CR+LF (spec)",
        section: "vttest 6: modes",
        cols: 12,
        rows: 3,
        bytes: b"\x1b[20habc\nX",
        screen: &["abc", "X"],
        cursor: Some((1, 1)),
        mode_set: &[TermMode::LINE_FEED_NEW_LINE],
        mode_clear: &[],
        expect: Expect::KnownBroken(
            "vte 把 LF 映射到 linefeed()，alacritty 的 newline()（含 LNM 处理）不会被调用",
        ),
    },
    TableCase {
        id: "6.2 DECSTR soft reset (spec)",
        section: "vttest 6: modes",
        cols: 12,
        rows: 6,
        bytes: b"\x1b[2;4r\x1b[?6h\x1b[1;1H\x1b[5 q\x1b[!p\x1b[1;1HX",
        screen: &["X"],
        cursor: Some((0, 1)),
        mode_set: &[],
        mode_clear: &[TermMode::ORIGIN],
        expect: Expect::KnownBroken("vte 0.15 不解析 CSI ! p（DECSTR），软复位被整体忽略"),
    },
    TableCase {
        id: "6.3 DECSED selective erase (spec)",
        section: "vttest 6: modes",
        cols: 12,
        rows: 3,
        bytes: b"abcdef\x1b[1;3H\x1b[?0J",
        screen: &["abc"],
        cursor: Some((0, 2)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::KnownBroken("vte 0.15 不解析 CSI ? Ps J（DECSED），选择性擦除未实现"),
    },
    TableCase {
        id: "6.4 DECSEL selective erase in line (spec)",
        section: "vttest 6: modes",
        cols: 12,
        rows: 3,
        bytes: b"abcdef\x1b[1;3H\x1b[?0K",
        screen: &["abc"],
        cursor: Some((0, 2)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::KnownBroken("vte 0.15 不解析 CSI ? Ps K（DECSEL），选择性擦除未实现"),
    },
    TableCase {
        id: "6.5 8-bit C1 CSI (spec)",
        section: "vttest 6: modes",
        cols: 12,
        rows: 3,
        bytes: b"\x9b31mR",
        screen: &["R"],
        cursor: Some((0, 1)),
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::KnownBroken("vte 0.15 只接受 7-bit ESC [，8-bit C1 控制不解释"),
    },
    TableCase {
        id: "7.1 DECSCUSR 1-6 cursor shapes",
        section: "vttest 7: cursor style",
        cols: 8,
        rows: 2,
        bytes: b"\x1b[1 q",
        screen: &[],
        cursor: None,
        mode_set: &[],
        mode_clear: &[],
        expect: Expect::Required,
    },
];

// ---------------------------------------------------------------------------
// 自定义断言：属性、回包、光标可见性等无法用纯文本表表达的项。
// ---------------------------------------------------------------------------

fn sim(cols: usize, rows: usize) -> TermSim {
    TermSim::new(cols, rows, 200)
}

fn assert_eqf<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: 期望 {want:?}，实际 {got:?}"))
    }
}

fn case_decsc_saves_sgr() -> Result<(), String> {
    let mut s = sim(10, 3);
    s.advance(b"\x1b[31m\x1b7\x1b[0m\x1b[32m\x1b8A");
    let f = s.pull();
    assert_eqf("DECRC 后前景色", f.cell(0, 0).fg, 0x00f7768e)?;
    assert_eqf("字符", f.cell(0, 0).char(), 'A')
}

fn case_sgr_basic() -> Result<(), String> {
    let mut s = sim(10, 3);
    s.advance(b"\x1b[31mR\x1b[42mG\x1b[0mN");
    let f = s.pull();
    assert_eqf("R 前景", f.cell(0, 0).fg, 0x00f7768e)?;
    assert_eqf("G 背景", f.cell(0, 1).bg, 0x009ece6a)?;
    assert_eqf("N 前景", f.cell(0, 2).fg, 0x00c0caf5)?;
    assert_eqf("N 背景", f.cell(0, 2).bg, 0x001a1b26)
}

fn case_sgr_256_truecolor() -> Result<(), String> {
    let mut s = sim(10, 3);
    s.advance(b"\x1b[38;5;196mX\x1b[38;2;1;2;3mT\x1b[0mN");
    let f = s.pull();
    assert_eqf("256 色 196", f.cell(0, 0).fg, 0x00ff0000)?;
    assert_eqf("truecolor", f.cell(0, 1).fg, 0x00010203)?;
    assert_eqf("reset", f.cell(0, 2).fg, 0x00c0caf5)
}

fn case_sgr_colon_subparams() -> Result<(), String> {
    // xterm 现代写法：38:2::R:G:B（ISO 8613-6 冒号子参数）。
    let mut s = sim(10, 3);
    s.advance(b"\x1b[38:2::255:0:0mR\x1b[0mN");
    let f = s.pull();
    assert_eqf("冒号 truecolor", f.cell(0, 0).fg, 0x00ff0000)?;
    assert_eqf("reset", f.cell(0, 1).fg, 0x00c0caf5)
}

fn case_cursor_styles() -> Result<(), String> {
    for (id, want) in [
        (0u8, CursorShape::Block),
        (1, CursorShape::Block),
        (2, CursorShape::Block),
        (3, CursorShape::Underline),
        (4, CursorShape::Underline),
        (5, CursorShape::Beam),
        (6, CursorShape::Beam),
    ] {
        let mut s = sim(8, 2);
        s.advance(format!("\x1b[{id} q").as_bytes());
        let f = s.pull();
        assert_eqf(&format!("DECSCUSR {id}"), f.cursor.shape, want)?;
    }
    Ok(())
}

fn case_cursor_visibility() -> Result<(), String> {
    let mut s = sim(8, 2);
    s.advance(b"\x1b[?25l");
    assert_eqf("25l 光标可见性", s.pull().cursor.visible, false)?;
    s.advance(b"\x1b[?25h");
    assert_eqf("25h 光标可见性", s.pull().cursor.visible, true)
}

fn case_wide_and_rep() -> Result<(), String> {
    let mut s = sim(12, 2);
    s.advance("中\x1b[2b".as_bytes());
    let f = s.pull();
    assert_eqf("宽字首格", f.cell(0, 0).char(), '中')?;
    assert_eqf("宽字重复", f.cell(0, 4).char(), '中')?;
    assert_eqf("REP 后光标", (f.cursor.line, f.cursor.col), (0, 6))
}

// ---- 终端回包（真实 PTY；与 vt220_xterm 的 capture_reply 相同的做法）----

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

fn capture_reply(tag: &str, query: &[u8], min_len: usize) -> Result<Vec<u8>, String> {
    let dir = std::env::temp_dir().join(format!("tervo-vt-suite-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let out = dir.join("reply.bin");
    let script = format!(
        "stty raw -echo; printf '{}'; timeout --foreground 5 dd bs=1 count=256 of={} 2>/dev/null",
        printf_escape(query),
        out.display()
    );
    let handle = spawn(PaneConfig {
        cols: 80,
        rows: 24,
        scrollback: 50,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script],
        ..PaneConfig::default()
    })
    .map_err(|e| format!("spawn pane: {e}"))?;
    let start = Instant::now();
    loop {
        let got = std::fs::read(&out).unwrap_or_default();
        if got.len() >= min_len {
            drop(handle);
            let _ = std::fs::remove_dir_all(&dir);
            return Ok(got);
        }
        if start.elapsed() > Duration::from_secs(5) {
            drop(handle);
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("查询回包超时（已收 {got:?}）"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn case_da2() -> Result<(), String> {
    let got = capture_reply("da2", b"\x1b[>c", 12)?;
    // 格式：CSI > 0 ; version ; 1 c，version 由 vendored alacritty 版本换算。
    let text = String::from_utf8_lossy(&got);
    if !(text.starts_with("\x1b[>0;") && text.ends_with(";1c")) {
        return Err(format!("DA2 回包格式不符: {text:?}"));
    }
    let version = text
        .trim_start_matches("\x1b[>0;")
        .trim_end_matches(";1c")
        .parse::<u32>()
        .map_err(|_| format!("DA2 版本字段不是数字: {text:?}"))?;
    if version < 1001 {
        return Err(format!("DA2 版本号异常: {version}"));
    }
    Ok(())
}

fn case_text_area_chars() -> Result<(), String> {
    let got = capture_reply("ws", b"\x1b[18t", 10)?;
    assert_eqf("CSI 18 t 回包", got, b"\x1b[8;24;80t".to_vec())
}

fn case_decrqm() -> Result<(), String> {
    let got = capture_reply("decrqm3", b"\x1b[?3$p", 8)?;
    assert_eqf("DECRQM ?3 (DECCOLM 不支持)", got, b"\x1b[?3;0$y".to_vec())?;
    let got = capture_reply("decrqm1049", b"\x1b[?1049h\x1b[?1049$p", 11)?;
    assert_eqf("DECRQM ?1049 设置后", got, b"\x1b[?1049;1$y".to_vec())?;
    let got = capture_reply("decrqm4", b"\x1b[4h\x1b[4$p", 7)?;
    assert_eqf("DECRQM 4 (IRM) 设置后", got, b"\x1b[4;1$y".to_vec())?;
    Ok(())
}

fn case_deccolm_132() -> Result<(), String> {
    // 规格：DECCOLM 切到 132 列；alacritty 只清屏/复位边距，列数保持 20。
    let mut s = sim(20, 4);
    s.advance(b"abc\x1b[?3hX");
    assert_eqf("DECCOLM 后列数", s.cols(), 132)?;
    assert_eqf("DECCOLM 清屏后", line_text(&mut s, 0), "X".to_string())
}

fn case_decsnm_reverse_video() -> Result<(), String> {
    // 规格：DECSCNM 让整个屏幕反显；Frame 里默认前景/背景应互换。
    let mut s = sim(10, 3);
    s.advance(b"abc\x1b[?5h");
    let f = s.pull();
    assert_eqf("反显前景", f.cell(0, 0).fg, 0x001a1b26)?;
    assert_eqf("反显背景", f.cell(0, 0).bg, 0x00c0caf5)
}

fn case_alternate_screen_47() -> Result<(), String> {
    // 老式 47/1047 切换在 xterm 里也切备用屏；alacritty 只实现 1049，故 XFAIL。
    let mut s = sim(10, 4);
    s.advance(b"MAIN\x1b[?47hALT\x1b[?47l");
    assert_eqf("47 退出后主屏", line_text(&mut s, 0), "MAIN".to_string())
}

// ---------------------------------------------------------------------------
// 汇总 / 入口
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("用法: vt_compat_suite [--report]");
        println!("  --report  打印每条 PASS/XFAIL，而不只是失败项");
        return;
    }
    let report = args.iter().any(|a| a == "--report");

    let custom = [
        CustomCase {
            id: "1.7 DECSC/DECRC preserves SGR",
            section: "vttest 1: cursor movement",
            expect: Expect::Required,
            run: case_decsc_saves_sgr,
        },
        CustomCase {
            id: "2.13 DECCKM/DECTCEM cursor state",
            section: "vttest 2: screen features",
            expect: Expect::Required,
            run: case_cursor_visibility,
        },
        CustomCase {
            id: "2.14 SGR basic palette colors",
            section: "vttest 2: screen features",
            expect: Expect::Required,
            run: case_sgr_basic,
        },
        CustomCase {
            id: "2.15 SGR 256 / truecolor",
            section: "vttest 2: screen features",
            expect: Expect::Required,
            run: case_sgr_256_truecolor,
        },
        CustomCase {
            id: "2.16 SGR colon sub-parameters",
            section: "vttest 2: screen features",
            expect: Expect::Required,
            run: case_sgr_colon_subparams,
        },
        CustomCase {
            id: "2.11 DECCOLM selects 132 columns (spec)",
            section: "vttest 2: screen features",
            expect: Expect::KnownBroken("alacritty 的 DECCOLM 只清屏/复位边距，不切换 132 列"),
            run: case_deccolm_132,
        },
        CustomCase {
            id: "2.12 DECSCNM reverse video (spec)",
            section: "vttest 2: screen features",
            expect: Expect::KnownBroken("vte 0.15 不识别私有模式 5（DECSCNM），反显未实现"),
            run: case_decsnm_reverse_video,
        },
        CustomCase {
            id: "2.17 wide char + REP",
            section: "vttest 2: screen features",
            expect: Expect::Required,
            run: case_wide_and_rep,
        },
        CustomCase {
            id: "3.3 DECSCUSR 0-6 shape mapping",
            section: "vttest 7: cursor style",
            expect: Expect::Required,
            run: case_cursor_styles,
        },
        CustomCase {
            id: "6.6 alternate screen 47/1047 (spec)",
            section: "vttest 2: screen features",
            expect: Expect::KnownBroken(
                "vte 只把 1049 映射到 SwitchScreen；47/1047 被当作未知私有模式忽略",
            ),
            run: case_alternate_screen_47,
        },
        CustomCase {
            id: "8.1 DA2 secondary device attributes",
            section: "vttest 5: reports",
            expect: Expect::Required,
            run: case_da2,
        },
        CustomCase {
            id: "8.2 CSI 18 t text area in chars",
            section: "vttest 5: reports",
            expect: Expect::Required,
            run: case_text_area_chars,
        },
        CustomCase {
            id: "8.3 DECRQM public/private modes",
            section: "vttest 5: reports",
            expect: Expect::Required,
            run: case_decrqm,
        },
    ];

    let mut counts = Counts::default();
    let mut failures = String::new();
    let mut current_section = String::new();

    let mut emit = |section: &str,
                    id: &str,
                    expect: Expect,
                    result: Result<(), String>,
                    counts: &mut Counts,
                    report: bool| {
        if current_section != section {
            current_section = section.to_string();
            if report {
                println!("\n## {section}");
            }
        }
        match (expect, result) {
            (_, Ok(())) => {
                counts.pass += 1;
                if report {
                    println!("PASS  {id}");
                }
            }
            (Expect::Required, Err(reason)) => {
                counts.fail += 1;
                let _ = writeln!(failures, "FAIL  {id}\n      {reason}");
                if report {
                    println!("FAIL  {id}\n      {reason}");
                }
            }
            (Expect::KnownBroken(why), Err(reason)) => {
                counts.xfail += 1;
                if report {
                    println!("XFAIL {id}\n      原因: {why}\n      实际: {reason}");
                }
            }
        }
    };

    // KnownBroken 条目如果意外通过，单独提示（不影响退出码）。
    for case in TABLE {
        let result = check_table(case);
        if let (Expect::KnownBroken(why), Ok(())) = (case.expect, &result) {
            counts.xpass += 1;
            if report {
                println!(
                    "XPASS {} —— 偏差已修复，请把该条移出 XFAIL（{why}）",
                    case.id
                );
            }
            continue;
        }
        emit(
            case.section,
            case.id,
            case.expect,
            result,
            &mut counts,
            report,
        );
    }
    for case in &custom {
        let result = check_custom(case);
        if let (Expect::KnownBroken(why), Ok(())) = (case.expect, &result) {
            counts.xpass += 1;
            if report {
                println!(
                    "XPASS {} —— 偏差已修复，请把该条移出 XFAIL（{why}）",
                    case.id
                );
            }
            continue;
        }
        emit(
            case.section,
            case.id,
            case.expect,
            result,
            &mut counts,
            report,
        );
    }

    println!(
        "\nVT 兼容套件：{} passed, {} failed, {} xfail, {} xpass",
        counts.pass, counts.fail, counts.xfail, counts.xpass
    );
    if counts.fail > 0 {
        eprintln!("\n失败项：\n{failures}");
        std::process::exit(1);
    }
}
