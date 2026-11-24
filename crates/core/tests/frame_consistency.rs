// SPDX-License-Identifier: GPL-3.0-only
//! partial Frame 与 Term 一致性集成测试。
//!
//! 目标：不启动 GUI，只用 `Term + Processor + fill_frame` 复现 UI 的真实路径：
//!
//! 1. 每个 chunk 走一次 **partial** `fill_frame`（生产 UI 的每次 pull）；
//! 2. 立刻对同一个 `Term` 做一次 **force_full** `fill_frame` 作为唯一真相；
//! 3. 逐 cell 比较 partial 快照与 full 快照；
//! 4. 用 renderer 的规则（`frame.full || damage.len() > rows/2` 才整屏，
//!    否则只重绘 `frame.damage` 行）维护一份“屏幕 cell 缓冲”，再与 full 比较。
//!
//! 第 4 步是用户现象的直接模型：旧字符残留 = partial 帧内容不一致，或内容
//! 一致但 damage 没覆盖变化行（renderer 不重绘）。
//!
//! 真实 dsh 原始字节的语料测试通过 `XTERVO_DSH_RAW=/path/to/app-to-host.raw`
//! 启用（也可自动探测仓库内 `.logs/dsh-dump-run/pty/app-to-host.raw`）。

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;
use xtervo_core::{fill_frame, CellData, Frame, GridSize, Palette};
use xtervo_core::{spawn, CoreEvent, PaneConfig};

/// 本批字节是否含零宽码位。与 `crate::pane::has_zero_width_chars` 语义一致：
/// 生产 actor 遇到零宽输入会强制下一帧整屏，这里复刻同样的策略。
fn has_zero_width(bytes: &[u8]) -> bool {
    use unicode_width::UnicodeWidthChar;

    if bytes.is_ascii() {
        return false;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => match std::str::from_utf8(&bytes[..e.valid_up_to()]) {
            Ok(text) => text,
            Err(_) => return false,
        },
    };
    text.chars().any(|c| c.width() == Some(0))
}

fn uri_of<'a>(frame: &'a Frame, cell: &CellData) -> Option<&'a str> {
    if cell.link == 0 {
        None
    } else {
        frame.hyperlinks.get(cell.link as usize).map(String::as_str)
    }
}

/// 两个 cell 的可见内容是否相同。`CellData.link` 只是各帧 URI 表的下标，
/// partial 帧与 force-full 帧的编号可能不同；比较实际 URI，忽略下标。
fn same_visible_cell(fa: &Frame, a: &CellData, fb: &Frame, b: &CellData) -> bool {
    let (mut x, mut y) = (*a, *b);
    x.link = 0;
    y.link = 0;
    x == y && uri_of(fa, a) == uri_of(fb, b)
}

fn cell_repr(frame: &Frame, cell: &CellData) -> String {
    format!(
        "ch={:?}(U+{:04X}) fg={:#08x} bg={:#08x} flags={:#06x} link={:?}",
        cell.char(),
        cell.ch,
        cell.fg,
        cell.bg,
        cell.flags,
        uri_of(frame, cell)
    )
}

fn frame_text(frame: &Frame) -> String {
    let mut out = String::with_capacity(frame.rows * (frame.cols + 1));
    for row in 0..frame.rows {
        let line: String = frame.row(row).iter().map(CellData::char).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 逐步重放 + 对账。
struct Replay {
    cols: usize,
    rows: usize,
    term: Term<VoidListener>,
    processor: Processor,
    palette: Palette,
    /// 复用帧：生产路径里 UI 侧往返回复用的那块 `Frame`。
    partial: Frame,
    /// 强制整屏快照帧（仅测试用）。
    full: Frame,
    /// renderer 上一帧真正显示在屏幕上的 cell（按 damage 行更新）。
    screen: Vec<CellData>,
    /// 上一轮 partial 帧的 cells，用于判断 damage 是否覆盖所有变化行。
    prev: Vec<CellData>,
    /// 上一轮 partial 帧每个格子的零宽码位。
    prev_zerowidth: Vec<Vec<Vec<char>>>,
    screen_valid: bool,
    steps: usize,
    force_full: bool,
    refresh_zerowidth: bool,
    bytes_fed: usize,
}

#[derive(Debug)]
struct Mismatch {
    step: usize,
    kind: &'static str,
    row: usize,
    col: usize,
    detail: String,
    damage: Vec<(u16, u16, u16)>,
    full: bool,
    context: String,
}

impl Replay {
    fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let size = GridSize::new(cols, rows);
        let config = TermConfig {
            scrolling_history: scrollback,
            ..TermConfig::default()
        };
        let term = Term::new(config, &size, VoidListener);
        Self {
            cols,
            rows,
            term,
            processor: Processor::new(),
            palette: Palette::default(),
            partial: Frame::new(cols, rows),
            full: Frame::new(cols, rows),
            screen: vec![CellData::default(); cols * rows],
            prev: vec![CellData::default(); cols * rows],
            prev_zerowidth: vec![vec![Vec::new(); cols]; rows],
            screen_valid: false,
            steps: 0,
            force_full: false,
            refresh_zerowidth: false,
            bytes_fed: 0,
        }
    }

    fn dimensions(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    /// 喂一个 chunk 并完成一轮 partial/full 对账；返回本轮发现的不一致。
    fn feed(&mut self, bytes: &[u8]) -> Vec<Mismatch> {
        self.bytes_fed += bytes.len();
        if has_zero_width(bytes) {
            self.force_full = true;
            self.refresh_zerowidth = true;
        }
        self.processor.advance(&mut self.term, bytes);
        self.check()
    }

    fn check(&mut self) -> Vec<Mismatch> {
        self.steps += 1;
        let (cols, rows) = self.dimensions();
        assert_eq!(
            (cols, rows),
            (self.cols, self.rows),
            "term size changed without resize"
        );

        let force_full = std::mem::take(&mut self.force_full);
        let refresh_zerowidth = std::mem::take(&mut self.refresh_zerowidth);
        fill_frame(
            &mut self.term,
            &mut self.partial,
            &self.palette,
            force_full,
            None,
            refresh_zerowidth,
        );

        let damage: Vec<(u16, u16, u16)> = self
            .partial
            .damage
            .iter()
            .map(|d| (d.line, d.left, d.right))
            .collect();
        let mut damage_rows = vec![false; rows];
        for &(line, _, _) in &damage {
            if (line as usize) < rows {
                damage_rows[line as usize] = true;
            }
        }

        // ---- renderer 模拟：partial -> screen ----
        // 与 crates/app/src/render.rs 一致：full 或 damage 行数超过一半才整屏，
        // 否则只重绘 damage 行（整行）。这里 screen 是“像素”的 cell 级模型。
        let needs_full = self.partial.full || damage.len() > self.rows / 2;
        if needs_full || !self.screen_valid {
            self.screen.copy_from_slice(&self.partial.cells);
            self.screen_valid = true;
            damage_rows.iter_mut().for_each(|d| *d = true);
        } else {
            for &(line, _, _) in &damage {
                let line = line as usize;
                if line < rows {
                    let base = line * cols;
                    self.screen[base..base + cols]
                        .copy_from_slice(&self.partial.cells[base..base + cols]);
                }
            }
        }

        // ---- 强制整屏快照（唯一真相） ----
        fill_frame(
            &mut self.term,
            &mut self.full,
            &self.palette,
            true,
            None,
            true,
        );

        let mut mismatches = Vec::new();

        // 1. Frame 内容：partial 必须等于 force_full。
        'cells: for row in 0..rows {
            let base = row * cols;
            for col in 0..cols {
                let expected = &self.full.cells[base + col];
                let actual = &self.partial.cells[base + col];
                if !same_visible_cell(&self.partial, actual, &self.full, expected)
                    || self.partial.zerowidth_at(row, col) != self.full.zerowidth_at(row, col)
                {
                    mismatches.push(Mismatch {
                        step: self.steps,
                        kind: "partial-frame != force-full",
                        row,
                        col,
                        detail: format!(
                            "partial: {} | full: {} | partial_zerowidth={:?} full_zerowidth={:?}",
                            cell_repr(&self.partial, actual),
                            cell_repr(&self.full, expected),
                            self.partial.zerowidth_at(row, col),
                            self.full.zerowidth_at(row, col),
                        ),
                        damage: damage.clone(),
                        full: self.partial.full,
                        context: String::new(),
                    });
                    break 'cells;
                }
            }
        }

        // 2. 帧级字段。
        if self.partial.cursor != self.full.cursor {
            mismatches.push(self.frame_mismatch(
                "cursor",
                format!(
                    "partial={:?} full={:?}",
                    self.partial.cursor, self.full.cursor
                ),
                &damage,
            ));
        }
        if self.partial.mode != self.full.mode {
            mismatches.push(self.frame_mismatch(
                "mode",
                format!(
                    "partial={:#x} full={:#x}",
                    self.partial.mode, self.full.mode
                ),
                &damage,
            ));
        }
        if self.partial.display_offset != self.full.display_offset {
            mismatches.push(self.frame_mismatch(
                "display_offset",
                format!(
                    "partial={} full={}",
                    self.partial.display_offset, self.full.display_offset
                ),
                &damage,
            ));
        }
        if self.partial.selection != self.full.selection || self.partial.search != self.full.search
        {
            mismatches.push(self.frame_mismatch(
                "selection/search",
                format!(
                    "partial={:?}/{:?} full={:?}/{:?}",
                    self.partial.selection,
                    self.partial.search,
                    self.full.selection,
                    self.full.search
                ),
                &damage,
            ));
        }
        if self.partial.row_ids != self.full.row_ids {
            let row = self
                .partial
                .row_ids
                .iter()
                .zip(&self.full.row_ids)
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            mismatches.push(self.frame_mismatch(
                "row_ids",
                format!(
                    "row {row}: partial={:#x} full={:#x}",
                    self.partial.row_ids[row], self.full.row_ids[row]
                ),
                &damage,
            ));
        }

        // 3. damage 必须覆盖所有发生变化但 renderer 不会整屏重绘的行。
        if !needs_full {
            for row in 0..rows {
                let base = row * cols;
                let changed = (0..cols).any(|col| {
                    !same_visible_cell(
                        &self.partial,
                        &self.prev[base + col],
                        &self.partial,
                        &self.partial.cells[base + col],
                    )
                }) || (0..cols).any(|col| {
                    // zerowidth 变化也算需要重绘该行。
                    self.partial.zerowidth_at(row, col) != self.partial_prev_zerowidth(row, col)
                });
                if changed && !damage_rows[row] {
                    mismatches.push(Mismatch {
                        step: self.steps,
                        kind: "changed row missing from damage",
                        row,
                        col: 0,
                        detail: "Frame cell 变了但 frame.damage 未覆盖该行；renderer 不会重绘"
                            .to_string(),
                        damage: damage.clone(),
                        full: self.partial.full,
                        context: String::new(),
                    });
                }
            }
        }

        // 4. renderer 模拟结果必须等于 Term 真相（用户可见残留的模型）。
        if mismatches.is_empty() {
            'screen: for row in 0..rows {
                let base = row * cols;
                for col in 0..cols {
                    if !same_visible_cell(
                        &self.partial,
                        &self.screen[base + col],
                        &self.full,
                        &self.full.cells[base + col],
                    ) {
                        mismatches.push(Mismatch {
                            step: self.steps,
                            kind: "rendered screen != term (stale glyph)",
                            row,
                            col,
                            detail: format!(
                                "screen: {} | term: {}",
                                cell_repr(&self.full, &self.screen[base + col]),
                                cell_repr(&self.full, &self.full.cells[base + col]),
                            ),
                            damage: damage.clone(),
                            full: self.partial.full,
                            context: String::new(),
                        });
                        break 'screen;
                    }
                }
            }
        }

        for m in &mut mismatches {
            if m.context.is_empty() {
                m.context = frame_text(&self.full);
            }
        }

        self.prev.copy_from_slice(&self.partial.cells);
        let prev_zerowidth: Vec<Vec<Vec<char>>> = (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| self.partial.zerowidth_at(row, col).to_vec())
                    .collect()
            })
            .collect();
        self.prev_zerowidth = prev_zerowidth;
        mismatches
    }

    fn partial_prev_zerowidth(&self, row: usize, col: usize) -> &[char] {
        self.prev_zerowidth
            .get(row)
            .and_then(|r| r.get(col))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn frame_mismatch(
        &self,
        kind: &'static str,
        detail: String,
        damage: &[(u16, u16, u16)],
    ) -> Mismatch {
        Mismatch {
            step: self.steps,
            kind,
            row: 0,
            col: 0,
            detail,
            damage: damage.to_vec(),
            full: self.partial.full,
            context: frame_text(&self.partial),
        }
    }
}

fn report_and_panic(mismatches: &[Mismatch], raw_len: usize, steps: usize) {
    let m = &mismatches[0];
    let mut msg = format!(
        "\n帧一致性失败（已喂 {raw_len} 字节 / {steps} 步）\n\
         step={} kind={} row={} col={} full={}\n\
         {}\n\
         damage={:?}\n\
         ---- partial/term 屏幕 ----\n{}\n",
        m.step, m.kind, m.row, m.col, m.full, m.detail, m.damage, m.context
    );
    if m.kind == "rendered screen != term (stale glyph)" {
        msg.push_str("---- 屏幕（按 damage 增量更新后） ----\n");
        // screen 文本在 Mismatch 里没有保存；detail 已给出差异 cell。
    }
    panic!("{msg}");
}

/// 把一段字节按固定 chunk 重放，返回首个不一致（若有）。
fn replay_chunks(
    cols: usize,
    rows: usize,
    scrollback: usize,
    bytes: &[u8],
    chunk: usize,
) -> (usize, Vec<Mismatch>, Replay) {
    let mut replay = Replay::new(cols, rows, scrollback);
    let mut step = 0;
    for piece in bytes.chunks(chunk.max(1)) {
        step += 1;
        let mismatches = replay.feed(piece);
        if !mismatches.is_empty() {
            return (step, mismatches, replay);
        }
    }
    (step, Vec::new(), replay)
}

// ---------------------------------------------------------------------------
// 确定性场景
// ---------------------------------------------------------------------------

/// 每个场景在多种 chunk 粒度下重放，确保 chunk 边界不影响结论。
fn assert_scenarios(scenarios: &[(&str, Vec<u8>)]) {
    for (name, bytes) in scenarios {
        for chunk in [1usize, 2, 3, 7, 64, 4096] {
            let (steps, mismatches, _) = replay_chunks(12, 5, 100, bytes, chunk);
            if !mismatches.is_empty() {
                eprintln!("场景 {name:?} chunk={chunk} 在 step {steps} 失败");
                report_and_panic(&mismatches, bytes.len(), steps);
            }
        }
    }
}

#[test]
fn multiline_output_scroll_and_rewrite_is_consistent() {
    let scenarios: Vec<(&str, Vec<u8>)> = vec![
        (
            "plain multiline + scroll",
            (0..40).flat_map(|i| format!("row {i:02} abcdefghij\r\n").into_bytes()).collect(),
        ),
        (
            "CUP rewrite shorter line leaves blanks",
            b"AAAAAAAAAAAAAAAAAAAA\r\nBBBBBBBBBBBBBBBBBBBB\r\nCCCCCCCCCCCCCCCCCCCC\r\n\x1b[2;1Hxyz\x1b[3;1Hqq\r\n".to_vec(),
        ),
        (
            "CUP rewrite after scroll",
            {
                let mut v: Vec<u8> = (0..20)
                    .flat_map(|i| format!("line-{i:02} 1234567890\r\n").into_bytes())
                    .collect();
                v.extend_from_slice(b"\x1b[2;1H\x1b[Kshort\x1b[4;1H\x1b[2Kxx");
                v
            },
        ),
        (
            "scroll region + linefeed at bottom",
            {
                let mut v = b"top\r\n".to_vec();
                v.extend_from_slice(b"\x1b[2;4r");
                for i in 0..10 {
                    v.extend_from_slice(format!("region {i}\r\n").as_bytes());
                }
                v.extend_from_slice(b"\x1b[r");
                v
            },
        ),
        (
            "scroll region + index/reverse index",
            {
                let mut v = b"\x1b[2;5r\x1b[5;1Hbottom".to_vec();
                v.extend_from_slice(b"\x1b[5;1H\n\n");
                v.extend_from_slice(b"\x1b[2;1H\x1bM\x1bM");
                v.extend_from_slice(b"\x1b[3;1Hafter");
                v
            },
        ),
        (
            "SU/SD",
            b"1\r\n2\r\n3\r\n4\r\n5\r\n\x1b[2S\x1b[1Ttail".to_vec(),
        ),
        (
            "alt screen enter/leave",
            b"primary\r\nsecond\r\n\x1b[?1049h\x1b[2J\x1b[Halt-line-1\r\nalt-line-2\r\n\x1b[3;1Halt rewrite longer?\x1b[?1049l\x1b[3;1Hback".to_vec(),
        ),
        (
            "alt screen scroll",
            {
                let mut v = b"\x1b[?1049h".to_vec();
                for i in 0..12 {
                    v.extend_from_slice(format!("alt {i}\r\n").as_bytes());
                }
                v.extend_from_slice(b"\x1b[?1049l");
                v
            },
        ),
        (
            "wide chars overwritten by narrow",
            "汉字宽字符测试行\r\n第二行宽字符\r\n\x1b[1;1Habcdefg\x1b[2;2Hxy\r\n".as_bytes().to_vec(),
        ),
        (
            "combining chars then scroll",
            "e\u{301}cole\r\nnormal\r\ne\u{301}\u{302}cole2\r\nscroll-wheel\r\nmore\r\n".as_bytes().to_vec(),
        ),
        (
            "OSC 8 hyperlink rewrite",
            b"\x1b]8;;https://example.com/a\x1b\\link-a\x1b]8;;\x1b\\\r\nplain\r\n\x1b]8;;https://example.com/b\x1b\\link-b\x1b]8;;\x1b\\\r\n\x1b[1;1H\x1b]8;;\x1b\\overwrite\x1b]8;;\x1b\\".to_vec(),
        ),
        (
            "wrap + exact width line",
            {
                let mut v = Vec::new();
                for i in 0..6 {
                    v.extend_from_slice(format!("{}{:02}", "0123456789".repeat(2), i).as_bytes());
                }
                v
            },
        ),
    ];
    assert_scenarios(&scenarios);
}

#[test]
fn deterministic_minimal_rewrites() {
    // 逐条最小序列，chunk=1 时逐字节 pull（最苛刻的 damage 合并）。
    let cases: &[&[u8]] = &[
        b"abc\r\n\x1b[1;1Hx",
        b"abcdef\x1b[1;1Hx",
        b"abc\r\ndef\r\n\x1b[2;1Hz",
        b"abc\r\ndef\r\n\x1b[1;1Hxyz",
        b"\x1b[2;1Hhello\x1b[1;1Hworld",
        b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE\r\n\x1b[2Sx",
        b"\x1b[?1049halt\r\n\x1b[?1049lprim",
        b"0123456789\r\n\x1b[1;5Hxy",
        b"a\xcc\x81b\x1b[1;1Hxy",
    ];
    for bytes in cases {
        for chunk in [1usize, 2, 5, 64] {
            let (steps, mismatches, _) = replay_chunks(10, 4, 50, bytes, chunk);
            if !mismatches.is_empty() {
                eprintln!("最小序列 {bytes:?} chunk={chunk} 失败");
                report_and_panic(&mismatches, bytes.len(), steps);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 真实 dsh 原始字节语料
// ---------------------------------------------------------------------------

fn locate_raw() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("XTERVO_DSH_RAW") {
        let path = std::path::PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        // 主仓库 / worktree 根目录下的 dump
        manifest.join("../../.logs/dsh-dump-run/pty/app-to-host.raw"),
        manifest.join("../../../.logs/dsh-dump-run/pty/app-to-host.raw"),
        manifest.join("../../../../.logs/dsh-dump-run/pty/app-to-host.raw"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

#[test]
fn raw_dsh_replay_matches_force_full() {
    let Some(raw_path) = locate_raw() else {
        eprintln!("跳过：未找到 app-to-host.raw；设置 XTERVO_DSH_RAW 指向 dsh 原始输出后再跑");
        return;
    };
    let bytes = std::fs::read(&raw_path).expect("读取 dsh raw");
    assert!(!bytes.is_empty(), "raw 为空: {raw_path:?}");

    // PTY 由 scripts/e2e_dsh_pty_proxy.py 以 130x40 启动（见 run_dsh.sh 与
    // frame-snapshot.json）。
    let (cols, rows) = (130usize, 40usize);
    // 真实 PTY read chunk 是 32KiB；再用更小的块检查 chunk 边界。
    for chunk in [4096usize, 257] {
        let (steps, mismatches, replay) = replay_chunks(cols, rows, 4000, &bytes, chunk);
        if !mismatches.is_empty() {
            eprintln!("raw {raw_path:?} chunk={chunk} 在 step {steps} 失败");
            report_and_panic(&mismatches, bytes.len(), steps);
        }
        if let Some(out) = std::env::var_os("XTERVO_FRAME_DUMP_OUT") {
            let mut path = std::path::PathBuf::from(out);
            path.set_extension(format!("chunk{chunk}.txt"));
            std::fs::write(&path, frame_text(&replay.full))
                .unwrap_or_else(|e| panic!("写最终屏幕失败 {path:?}: {e}"));
            eprintln!("最终屏幕 -> {path:?}");
        }
        eprintln!(
            "raw replay OK: {} bytes, chunk={chunk}, {steps} pulls, cursor=({},{}), mode={:#x}",
            bytes.len(),
            replay.full.cursor.line,
            replay.full.cursor.col,
            replay.full.mode,
        );
    }
}

// ---------------------------------------------------------------------------
// 生产 PTY 管道回归：过滤层不能吞字节
// ---------------------------------------------------------------------------

/// dsh 的整行重绘大量出现 `]` + 紧跟 ESC（`[stderr]` + `ESC[..C`、
/// `[Image #1]` + `ESC[1C`）。IIP 过滤器曾经因为 `ESC x` 后状态没回到
/// Ground，把正文里的 `]` 当成 OSC 前缀丢掉；丢一个字符会让整行右移，
/// 行尾保留旧字符（用户看到的“旧行字符残留”）。
///
/// 这里走真实 PTY + PaneActor + 各过滤层，断言最终 Frame 文本完整。
#[test]
fn pty_pipeline_preserves_bracket_before_escape() {
    let script = "printf '\\033[31m[stderr]\\033[39m[Image #1]\\033[1Cdone\\r\\n'";
    let cfg = PaneConfig {
        cols: 80,
        rows: 4,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        ..PaneConfig::default()
    };
    let handle = spawn(cfg).expect("spawn pane");
    let mut frame = handle.new_frame();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        frame = handle.pull(frame);
        while let Some(event) = handle.try_event() {
            if matches!(event, CoreEvent::Exited) {
                exited = true;
            }
        }
        if exited {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(exited, "pane 未在超时前退出");
    // EOF 后再拉两次，确保 actor 已排空 PTY 队列。
    for _ in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        frame = handle.pull(frame);
    }
    let text: String = frame.row(0).iter().map(CellData::char).collect();
    assert!(
        text.contains("[stderr]"),
        "`]` 后紧跟 ESC 时不能被过滤层吞掉；实际行: {text:?}"
    );
    assert!(
        text.contains("[Image #1]"),
        "`[Image #1]` 的 `]` 被吞；实际行: {text:?}"
    );
    assert!(text.contains("done"), "后续正文不能丢失；实际行: {text:?}");
}
