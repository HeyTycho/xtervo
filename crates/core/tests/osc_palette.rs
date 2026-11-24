// SPDX-License-Identifier: GPL-3.0-only
//! OSC 颜色查询/设置 + XTWINOPS 尺寸查询 + OSC 133 的端到端契约（TERM-011）。
//!
//! 用真实 PTY：shell 打开 raw、`dd` 把回包落盘，断言**字节级**应答格式与主题色。
//! 覆盖：
//! - `OSC 4 ; idx ; ?` / `OSC 10|11|12 ; ?` 查询 → xterm `rgb:rrrr/gggg/bbbb` 回包；
//! - `OSC 4/10/11/12` 设置 → 后续网格颜色与查询都反映新值；
//! - `OSC 104/110/111/112` 复位 → 回到主题色；
//! - `CSI 14 t` / `CSI 18 t`（TextAreaSizeRequest 的字符/像素两种形态）；
//! - `OSC 133 A/B/C/D` shell integration 事件端到端。
//!
//! 不覆盖：`OSC 4;?` 一次查询多个索引的多回包顺序（vte 逐个回调，行为同上）；
//! 动态配色跨主题重载（主题切换会重置 Palette，但 alacritty 的运行时覆盖保留）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use xtervo_core::{spawn, CoreEvent, Frame, Palette, PaneConfig, PaneHandle, ShellMark};

fn sh(script: &str) -> PaneConfig {
    PaneConfig {
        cols: 100,
        rows: 24,
        scrollback: 200,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        ..PaneConfig::default()
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtervo-osc-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 把字节转成 POSIX printf 格式串（ESC/BEL/反斜杠/百分号转义）。
fn printf_escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            0x1b => out.push_str("\\033"),
            0x07 => out.push_str("\\007"),
            b'\\' => out.push_str("\\\\"),
            b'%' => out.push_str("%%"),
            b'\'' => out.push_str("\\047"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{:03o}", b)),
        }
    }
    out
}

/// 让子进程自己打印查询序列（走 stdout → 终端），再用 raw dd 读取终端回包。
///
/// 关键：`handle.input()` 写的是**子进程 stdin**，终端看不到；只有子进程
/// 打印出来的 OSC/CSI 才会被 actor 解析并产生回包。
fn spawn_capture(tag: &str, count: usize, query: &[u8]) -> (PaneHandle, PathBuf) {
    let dir = temp_dir(tag);
    let out = dir.join("reply.bin");
    let script = format!(
        "stty raw -echo; printf '{}'; timeout --foreground 5 dd bs=1 count={count} of={} 2>/dev/null",
        printf_escape(query),
        out.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    // 查询输出与回包都在 pty 输入队列里排队，dd 启动后按序读出。
    std::thread::sleep(Duration::from_millis(50));
    (handle, out)
}

fn read_exact(path: &Path, count: usize) -> Vec<u8> {
    let start = Instant::now();
    loop {
        let got = std::fs::read(path).unwrap_or_default();
        if got.len() >= count {
            return got[..count].to_vec();
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!("回包未到达 PTY（期望 {count} 字节，收到 {}）: {got:?}", got.len());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 和 [`spawn_capture`] 相同，但把查询拆成多个 `printf` 写入（中间 sleep），
/// 用于对照“同一 chunk”与“拆批”的颜色查询语义。
fn spawn_capture_chunks(tag: &str, count: usize, chunks: &[&[u8]]) -> (PaneHandle, PathBuf) {
    let dir = temp_dir(tag);
    let out = dir.join("reply.bin");
    let mut script = String::from("stty raw -echo; ");
    for chunk in chunks {
        script.push_str(&format!(
            "printf '{}'; sleep 0.1; ",
            printf_escape(chunk)
        ));
    }
    script.push_str(&format!(
        "timeout --foreground 5 dd bs=1 count={count} of={} 2>/dev/null",
        out.display()
    ));
    let handle = spawn(sh(&script)).expect("spawn pane");
    std::thread::sleep(Duration::from_millis(50));
    (handle, out)
}

/// xterm 颜色回包：`rgb:rrrr/gggg/bbbb`（每通道重复两位十六进制）。
fn rgb_reply(prefix: &str, color: u32, bel: bool) -> Vec<u8> {
    let (r, g, b) = ((color >> 16) as u8, (color >> 8) as u8, color as u8);
    let terminator = if bel { "\x07" } else { "\x1b\\" };
    format!(
        "\x1b]{prefix};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}{terminator}"
    )
    .into_bytes()
}

fn frame_text(frame: &Frame) -> String {
    (0..frame.rows)
        .map(|r| frame.row(r).iter().map(|c| c.char()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_frame(handle: &PaneHandle, needle: &str) -> Box<Frame> {
    let start = Instant::now();
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains(needle) {
            return frame;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "等待 {needle:?} 超时，当前屏幕:\n{}",
            frame_text(&frame)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn osc4_query_returns_theme_color() {
    let palette = Palette::default();
    let mut want = Vec::new();
    for index in [0u8, 1, 4, 15] {
        want.extend(rgb_reply(&format!("4;{index}"), palette.at(index as usize).unwrap(), true));
    }
    let mut query = Vec::new();
    for index in [0u8, 1, 4, 15] {
        query.extend_from_slice(format!("\x1b]4;{index};?\x07").as_bytes());
    }
    let (_handle, out) = spawn_capture("osc4-query", want.len(), &query);
    let got = read_exact(&out, want.len());
    assert_eq!(got, want, "OSC 4 查询回包与主题色不一致");
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

#[test]
fn osc10_11_12_query_returns_fg_bg_cursor() {
    let palette = Palette::default();
    let mut want = Vec::new();
    for (code, color) in
        [("10", palette.fg), ("11", palette.bg), ("12", palette.cursor)]
    {
        want.extend(rgb_reply(code, color, true));
    }
    let (_handle, out) = spawn_capture(
        "osc10-query",
        want.len(),
        b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07",
    );
    let got = read_exact(&out, want.len());
    assert_eq!(got, want, "OSC 10/11/12 查询回包与主题色不一致");
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

#[test]
fn osc4_set_is_reflected_in_grid_and_reply() {
    // raw 模式下直接打印；不回显，所以查询回包不会污染网格。
    let handle = spawn(sh(
        "stty raw -echo; printf '\\033]4;1;rgb:ff/00/00\\007\\033[31mR'; sleep 2",
    ))
    .expect("spawn pane");
    let frame = wait_frame(&handle, "R");
    assert_eq!(frame.cell(0, 0).char(), 'R');
    assert_eq!(
        frame.cell(0, 0).fg, 0x00ff_0000,
        "OSC 4 设置后 ANSI 1 应解析成新颜色，而不是主题色"
    );
    drop(handle);

    // 设置后立即查询（同一次输入写入，验证 actor 的顺序/可见性）。
    let want = rgb_reply("4;1", 0x00ff_0000, true);
    let (_handle, out) = spawn_capture(
        "osc4-set-query",
        want.len(),
        b"\x1b]4;1;rgb:ff/00/00\x07\x1b]4;1;?\x07",
    );
    let got = read_exact(&out, want.len());
    assert_eq!(got, want, "OSC 4 设置后再查询必须返回新颜色");
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

#[test]
fn osc104_reset_restores_theme_color() {
    let palette = Palette::default();
    let want = rgb_reply("4;2", palette.at(2).unwrap(), true);
    // 先改到红色再复位（OSC 104;2），查询应回到主题色。
    let (_handle, out) = spawn_capture(
        "osc104-reset",
        want.len(),
        b"\x1b]4;2;rgb:ff/00/00\x07\x1b]104;2\x07\x1b]4;2;?\x07",
    );
    let got = read_exact(&out, want.len());
    assert_eq!(got, want, "OSC 104 复位后应回到主题色");
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

#[test]
fn osc10_set_changes_default_foreground_and_reset_restores_it() {
    let handle = spawn(sh(
        "stty raw -echo; printf '\\033]10;rgb:11/22/33\\007PLAIN'; sleep 2",
    ))
    .expect("spawn pane");
    let frame = wait_frame(&handle, "PLAIN");
    assert_eq!(frame.cell(0, 0).fg, 0x0011_2233, "OSC 10 设置默认前景色");
    drop(handle);

    let palette = Palette::default();
    let want = rgb_reply("10", palette.fg, true);
    let (_handle, out) = spawn_capture(
        "osc110-reset",
        want.len(),
        b"\x1b]10;rgb:11/22/33\x07\x1b]110\x07\x1b]10;?\x07",
    );
    let got = read_exact(&out, want.len());
    assert_eq!(got, want, "OSC 110 复位默认前景色");
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

#[test]
fn csi14_and_18_report_text_area_size() {
    // CSI 14 t（像素，core 的 XTWINOPS 扫描回包）→ CSI 4;h;w t
    // CSI 18 t（字符，alacritty 的 TextAreaSizeRequest 等价路径）→ CSI 8;rows;cols t
    let want = b"\x1b[4;384;800t\x1b[8;24;100t";
    let (_handle, out) =
        spawn_capture("xtwinops", want.len(), b"\x1b[14t\x1b[18t");
    let got = read_exact(&out, want.len());
    assert_eq!(got, want, "CSI 14t/18t 回包错误");
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

#[test]
fn osc133_shell_marks_arrive_as_events() {
    let handle = spawn(sh(
        "printf '\\033]133;A\\007\\033]133;B\\007\\033]133;C\\007\\033]133;D;3\\007'; sleep 1",
    ))
    .expect("spawn pane");
    let start = Instant::now();
    let mut marks = Vec::new();
    while marks.len() < 4 {
        let mut events = Vec::new();
        handle.drain_events(&mut events);
        for ev in events {
            if let CoreEvent::Shell(mark) = ev {
                marks.push(mark);
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "OSC 133 事件未到齐: {marks:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        marks,
        vec![
            ShellMark::PromptStart,
            ShellMark::InputStart,
            ShellMark::OutputStart,
            ShellMark::CommandFinished { exit_code: Some(3) },
        ]
    );
}

/// O5：同一 chunk 内 `query → set → query`，第一个 query 必须返回 set 之前
/// 的旧颜色，第二个返回新颜色；拆成三个 chunk 的结果必须与同 chunk 一致。
#[test]
fn osc4_same_chunk_query_set_query_returns_query_time_colors() {
    let old = Palette::default().at(1).unwrap();
    let new = 0x00ff_0000;
    let mut want = rgb_reply("4;1", old, true);
    want.extend(rgb_reply("4;1", new, true));
    let query = b"\x1b]4;1;?\x07\x1b]4;1;rgb:ff/00/00\x07\x1b]4;1;?\x07";

    let (_handle, out) = spawn_capture("osc4-query-set-query-same", want.len(), query);
    let same = read_exact(&out, want.len());
    let _ = std::fs::remove_dir_all(out.parent().unwrap());

    let (_handle, out) = spawn_capture_chunks(
        "osc4-query-set-query-split",
        want.len(),
        &[
            b"\x1b]4;1;?\x07",
            b"\x1b]4;1;rgb:ff/00/00\x07",
            b"\x1b]4;1;?\x07",
        ],
    );
    let split = read_exact(&out, want.len());
    let _ = std::fs::remove_dir_all(out.parent().unwrap());

    assert_eq!(same, want, "同 chunk：第一个 query 必须是 set 之前的旧色");
    assert_eq!(split, want, "拆批对照回包错误");
    assert_eq!(same, split, "同批与拆批的颜色查询结果必须一致");
}
