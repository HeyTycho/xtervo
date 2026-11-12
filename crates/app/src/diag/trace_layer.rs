// SPDX-License-Identifier: GPL-3.0-only
//! `tervo diag trace-layer`：按层定位“第一个不一致的字节 / cell / 行”。
//!
//! 分层顺序（与 `docs/POSTMORTEM_2026-11-11_RENDER_RESIDUE.md` §5.1 对齐）：
//!
//! ```text
//! raw bytes
//!   -> Sixel / IIP / Kitty filter（字节守恒 + 分块确定性）
//!   -> Term（分块 vs 一次性喂入）
//!   -> 增量 Frame（生产 pull 路径）vs force-full Frame（一次性 Term 真相）
//!   -> cell 级 framebuffer（renderer 按 damage 增量重绘的模型）
//!   -> 图片 placement 裁剪（解码事件锚点检查）
//! ```
//!
//! 只要某一层出现第一处分歧就停止并输出它；全部通过才 PASS。完整像素
//! framebuffer / Slint 窗口不在本命令范围内（CLI 不创建 GUI），会在
//! `limitations` 里显式标出，不会伪装成 PASS 的一部分。
//!
//! 故障注入（`--fault`）只用于自测定位器：`filter-drop=N` 从 filter 输出
//! 里丢掉第 N 个字节，`frame-residue=R,C` 在 pull 后把某格改成旧内容，
//! `damage-drop=R` 删掉某行的 damage。生产诊断不要带 `--fault`。

use std::collections::HashMap;
use std::error::Error;
use std::io::Read;

use serde_json::{json, Value};
use tervo_core::kitty_graphics::{KittyCommand, KittyEvent, KittyParser};
use tervo_core::{Frame, IipEvent, IipFilter, SixelEvent, SixelFilter, TermSim};

use super::{opt, print_json, COMMIT, TARGET, VERSION};

const DEFAULT_CHUNK: usize = 16 * 1024;
const DEFAULT_CELL_W: u16 = 10;
const DEFAULT_CELL_H: u16 = 20;
const DEFAULT_SCROLLBACK: usize = 4000;

pub(crate) fn cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args
        .iter()
        .any(|a| a == "help" || a == "-h" || a == "--help")
    {
        println!("{}", help_text());
        return Ok(());
    }

    let input = opt(args, "--input");
    let fixture = opt(args, "--fixture");
    if input.is_none() && fixture.is_none() {
        return Err(
            "trace-layer 需要 --input FILE 或 --fixture NAME（见 tervo diag trace-layer --help）"
                .into(),
        );
    }
    if input.is_some() && fixture.is_some() {
        return Err("--input 与 --fixture 不能同时使用".into());
    }

    let mut opts = TraceOptions::default();
    let mut options_source = "defaults".to_string();
    if let Some(path) = opt(args, "--options").or_else(|| opt(args, "--meta")) {
        apply_options_file(&mut opts, &path)?;
        options_source = path;
    } else if let Some(path) = input.as_deref() {
        // 真实 dsh dump 旁边通常有 frame-snapshot.json，里面带 cols/rows。
        if let Some(sidecar) = find_sidecar(path) {
            apply_options_file(&mut opts, &sidecar)?;
            options_source = sidecar;
        }
    }
    if let Some(v) = opt(args, "--cols") {
        opts.cols = v
            .parse::<usize>()
            .map_err(|_| "--cols 必须是整数")?
            .clamp(1, 1000);
        options_source = "cli".to_string();
    }
    if let Some(v) = opt(args, "--rows") {
        opts.rows = v
            .parse::<usize>()
            .map_err(|_| "--rows 必须是整数")?
            .clamp(1, 1000);
        options_source = "cli".to_string();
    }
    if let Some(v) = opt(args, "--chunk") {
        opts.chunk = v
            .parse::<usize>()
            .map_err(|_| "--chunk 必须是整数")?
            .clamp(1, 1 << 20);
        options_source = "cli".to_string();
    }
    if let Some(v) = opt(args, "--cell-width") {
        opts.cell_width = v
            .parse::<u16>()
            .map_err(|_| "--cell-width 必须是 0..=65535")?
            .max(1);
        options_source = "cli".to_string();
    }
    if let Some(v) = opt(args, "--cell-height") {
        opts.cell_height = v
            .parse::<u16>()
            .map_err(|_| "--cell-height 必须是 0..=65535")?
            .max(1);
        options_source = "cli".to_string();
    }
    if args.iter().any(|a| a == "--no-images") {
        opts.decode_images = false;
    }

    let fault = Fault::parse(opt(args, "--fault").as_deref())?;
    let (raw, source) = match (input, fixture) {
        (Some(path), None) => {
            let bytes = if path == "-" {
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf)?;
                buf
            } else {
                let canonical = std::fs::canonicalize(&path)
                    .map_err(|e| format!("trace-layer 输入文件不存在/不可读: {path}: {e}"))?;
                std::fs::read(&canonical)
                    .map_err(|e| format!("trace-layer 读取输入失败 {}: {e}", canonical.display()))?
            };
            (bytes, path)
        }
        (None, Some(name)) => (synthetic_fixture(&name)?, format!("fixture:{name}")),
        _ => unreachable!(),
    };

    let report = analyze(&raw, &opts, &fault, &source, &options_source);
    if args.iter().any(|a| a == "--json") {
        print_json(&report.to_json())?;
    } else {
        print!("{}", report.to_text());
    }
    Ok(())
}

fn help_text() -> &'static str {
    "用法: tervo diag trace-layer --input FILE [选项]\n\
     \n\
     按 raw -> filter -> Term -> 增量/整屏 Frame -> cell framebuffer -> 图片裁剪\n\
     逐层比较，输出第一处分歧（PASS 表示所有已比较层一致）。\n\
     \n\
     --input FILE          raw PTY 字节文件；`-` 表示 stdin\n\
     --fixture NAME        合成夹具：clean | images | filter-edge\n\
     --options FILE        读取 cols/rows/cell_width/cell_height/chunk（JSON）\n\
     --cols N / --rows N   覆盖列/行（默认 120x32；存在 frame-snapshot.json 时自动读取）\n\
     --chunk N             filter/Term 分块大小（默认 16384）\n\
     --cell-width N        图片裁剪审计用 cell 宽（默认 10）\n\
     --cell-height N       图片裁剪审计用 cell 高（默认 20）\n\
     --no-images           不解码图片（仍会过滤图片协议，跳过图片层）\n\
     --json                输出 tervo.diag.v1 JSON（默认人类可读）\n\
     --fault SPEC          仅自测定位器：filter-drop=N | frame-residue=R:C[@STEP] | damage-drop=R\n\
     \n\
     退出码始终为 0；脚本请读 JSON 的 status=FAIL。完整像素 framebuffer 不在本命令范围。"
}

fn apply_options_file(opts: &mut TraceOptions, path: &str) -> Result<(), Box<dyn Error>> {
    let body =
        std::fs::read_to_string(path).map_err(|e| format!("读取 options 文件失败 {path}: {e}"))?;
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("options 文件不是 JSON {path}: {e}"))?;
    if let Some(v) = value.get("cols").and_then(Value::as_u64) {
        opts.cols = (v as usize).clamp(1, 1000);
    }
    if let Some(v) = value.get("rows").and_then(Value::as_u64) {
        opts.rows = (v as usize).clamp(1, 1000);
    }
    if let Some(v) = value.get("cell_width").and_then(Value::as_u64) {
        opts.cell_width = (v.min(u16::MAX as u64) as u16).max(1);
    }
    if let Some(v) = value.get("cell_height").and_then(Value::as_u64) {
        opts.cell_height = (v.min(u16::MAX as u64) as u16).max(1);
    }
    if let Some(v) = value.get("chunk").and_then(Value::as_u64) {
        opts.chunk = (v as usize).clamp(1, 1 << 20);
    }
    if let Some(v) = value.get("scrollback").and_then(Value::as_u64) {
        opts.scrollback = (v as usize).clamp(0, 1_000_000);
    }
    Ok(())
}

fn find_sidecar(input: &str) -> Option<String> {
    let path = std::path::Path::new(input);
    let parent = path.parent()?;
    for dir in [parent, parent.parent()?] {
        let candidate = dir.join("frame-snapshot.json");
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

#[derive(Clone, Debug)]
pub(crate) struct TraceOptions {
    pub cols: usize,
    pub rows: usize,
    pub chunk: usize,
    pub scrollback: usize,
    pub cell_width: u16,
    pub cell_height: u16,
    pub decode_images: bool,
}

impl Default for TraceOptions {
    fn default() -> Self {
        Self {
            cols: 120,
            rows: 32,
            chunk: DEFAULT_CHUNK,
            scrollback: DEFAULT_SCROLLBACK,
            cell_width: DEFAULT_CELL_W,
            cell_height: DEFAULT_CELL_H,
            decode_images: true,
        }
    }
}

// ---------------------------------------------------------------------------
// 图片协议账本：独立于生产 filter 的整段扫描，用来判断“删掉的字节是否
// 都属于图片协议”。只认 Sixel DCS、OSC 1337;File=、Kitty `ESC _ G`。
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolKind {
    Sixel,
    Iip,
    Kitty,
}

impl ProtocolKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sixel => "sixel",
            Self::Iip => "iip",
            Self::Kitty => "kitty",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProtocolSpan {
    pub start: usize,
    pub end: usize,
    pub kind: ProtocolKind,
}

const ESC: u8 = 0x1b;

fn is_st_at(raw: &[u8], i: usize) -> bool {
    raw.get(i) == Some(&ESC) && raw.get(i + 1) == Some(&b'\\')
}

fn terminator_end(raw: &[u8], mut i: usize, allow_bel: bool) -> Option<usize> {
    while i < raw.len() {
        if allow_bel && raw[i] == 0x07 {
            return Some(i + 1);
        }
        if raw[i] == 0x9c || matches!(raw[i], 0x18 | 0x1a) {
            return Some(i + 1);
        }
        if is_st_at(raw, i) {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

fn is_utf8_continuation(raw: &[u8], i: usize) -> bool {
    // 复刻 IipFilter::looks_like_utf8_continuation：0x9D 也可能是 UTF-8
    // 的续字节（`”` = E2 80 9D）。整段扫描时能直接按 UTF-8 前缀判断。
    fn cont(b: u8) -> bool {
        (0x80..=0xBF).contains(&b)
    }
    fn lead2(b: u8) -> bool {
        (0xC2..=0xDF).contains(&b)
    }
    fn lead3(b: u8) -> bool {
        (0xE0..=0xEF).contains(&b)
    }
    fn lead4(b: u8) -> bool {
        (0xF0..=0xF4).contains(&b)
    }
    let prev = |n: usize| -> Option<u8> {
        if i >= n {
            Some(raw[i - n])
        } else {
            None
        }
    };
    let (p1, p2, p3) = (prev(1), prev(2), prev(3));
    let as_second = p1.is_some_and(|p| lead2(p) || lead3(p) || lead4(p));
    let as_third = matches!((p1, p2), (Some(p1), Some(p2)) if cont(p1) && (lead3(p2) || lead4(p2)));
    let as_fourth = matches!(
        (p1, p2, p3),
        (Some(p1), Some(p2), Some(p3)) if cont(p1) && cont(p2) && lead4(p3)
    );
    as_second || as_third || as_fourth
}

fn utf8_len(raw: &[u8], i: usize) -> usize {
    match std::str::from_utf8(&raw[i..]) {
        Ok(s) => s.chars().next().map(char::len_utf8).unwrap_or(1),
        Err(e) => {
            if e.valid_up_to() == 0 {
                1
            } else {
                std::str::from_utf8(&raw[i..i + e.valid_up_to()])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .map(char::len_utf8)
                    .unwrap_or(1)
            }
        }
    }
}

fn scan_dcs_sixel(raw: &[u8], start: usize) -> Option<(usize, bool)> {
    let mut i = start + if raw[start] == ESC { 2 } else { 1 };
    let mut intermediate = false;
    while i < raw.len() {
        let b = raw[i];
        if (0x20..=0x2f).contains(&b) {
            intermediate = true;
            i += 1;
            continue;
        }
        if (0x30..=0x3f).contains(&b) || b == b';' {
            i += 1;
            continue;
        }
        if (0x40..=0x7e).contains(&b) {
            if b != b'q' || intermediate {
                return None;
            }
            let end = terminator_end(raw, i + 1, true).unwrap_or(raw.len());
            return Some((end, true));
        }
        return None;
    }
    None
}

fn scan_osc_iip(raw: &[u8], content_start: usize) -> Option<usize> {
    const PREFIX: &[u8] = b"1337;File=";
    if !raw.get(content_start..)?.starts_with(PREFIX) {
        return None;
    }
    terminator_end(raw, content_start + PREFIX.len(), true).or(Some(raw.len()))
}

fn scan_apc_kitty(raw: &[u8], payload_start: usize) -> Option<usize> {
    if raw.get(payload_start) != Some(&b'G') {
        return None;
    }
    terminator_end(raw, payload_start + 1, false).or(Some(raw.len()))
}

pub(crate) fn scan_protocol_spans(raw: &[u8]) -> Vec<ProtocolSpan> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            ESC => match raw.get(i + 1) {
                Some(b'P') => {
                    if let Some((end, true)) = scan_dcs_sixel(raw, i) {
                        spans.push(ProtocolSpan {
                            start: i,
                            end,
                            kind: ProtocolKind::Sixel,
                        });
                        i = end;
                    } else {
                        i += 2;
                    }
                }
                Some(b']') => {
                    if let Some(end) = scan_osc_iip(raw, i + 2) {
                        spans.push(ProtocolSpan {
                            start: i,
                            end,
                            kind: ProtocolKind::Iip,
                        });
                        i = end;
                    } else {
                        i += 2;
                    }
                }
                Some(b'_') => {
                    if let Some(end) = scan_apc_kitty(raw, i + 2) {
                        spans.push(ProtocolSpan {
                            start: i,
                            end,
                            kind: ProtocolKind::Kitty,
                        });
                        i = end;
                    } else {
                        i += 2;
                    }
                }
                _ => i += 1,
            },
            0x90 => {
                if let Some((end, true)) = scan_dcs_sixel(raw, i) {
                    spans.push(ProtocolSpan {
                        start: i,
                        end,
                        kind: ProtocolKind::Sixel,
                    });
                    i = end;
                } else {
                    i += 1;
                }
            }
            0x9d if !is_utf8_continuation(raw, i) => {
                if let Some(end) = scan_osc_iip(raw, i + 1) {
                    spans.push(ProtocolSpan {
                        start: i,
                        end,
                        kind: ProtocolKind::Iip,
                    });
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += utf8_len(raw, i).max(1),
        }
    }
    spans
}

fn ledger_expected(raw: &[u8], spans: &[ProtocolSpan]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut offsets = Vec::with_capacity(raw.len());
    let mut cursor = 0;
    for span in spans {
        for (off, &b) in raw[cursor..span.start].iter().enumerate() {
            bytes.push(b);
            offsets.push(cursor + off);
        }
        cursor = span.end.max(cursor);
    }
    for (off, &b) in raw[cursor..].iter().enumerate() {
        bytes.push(b);
        offsets.push(cursor + off);
    }
    (bytes, offsets)
}

// ---------------------------------------------------------------------------
// 生产 filter 链回放
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FilterStats {
    pub sixel_images: usize,
    pub iip_images: usize,
    pub kitty_commands: usize,
    pub kitty_errors: usize,
    pub kitty_passes: usize,
    pub swallowed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageAnchor {
    pub kind: ProtocolKind,
    pub width: u32,
    pub height: u32,
    pub cursor_row: usize,
    pub col: u16,
    pub row_id: u64,
    pub source_rect: Option<(u32, u32, u32, u32)>,
}

#[derive(Debug)]
pub(crate) struct PipelineRun {
    pub filtered: Vec<u8>,
    pub pieces: Vec<Vec<u8>>,
    pub anchors: Vec<ImageAnchor>,
    pub stats: FilterStats,
}

struct PipelineBuilder<'a> {
    opts: &'a TraceOptions,
    sixel: SixelFilter,
    iip: IipFilter,
    kitty: KittyParser,
    image_sim: TermSim,
    filtered: Vec<u8>,
    pieces: Vec<Vec<u8>>,
    anchors: Vec<ImageAnchor>,
    kitty_dims: HashMap<u32, (u32, u32)>,
    drop_at: Option<usize>,
    emitted: usize,
    stats: FilterStats,
}

impl<'a> PipelineBuilder<'a> {
    fn new(opts: &'a TraceOptions, drop_at: Option<usize>) -> Self {
        Self {
            opts,
            sixel: SixelFilter::new(),
            iip: IipFilter::new(),
            kitty: KittyParser::new(),
            image_sim: TermSim::new(opts.cols, opts.rows, opts.scrollback),
            filtered: Vec::with_capacity(4096),
            pieces: Vec::new(),
            anchors: Vec::new(),
            kitty_dims: HashMap::new(),
            drop_at,
            emitted: 0,
            stats: FilterStats::default(),
        }
    }

    fn feed_raw(&mut self, chunk: &[u8]) {
        self.pieces.push(Vec::new());
        let decode = self.opts.decode_images;
        match self.sixel.feed(chunk, decode) {
            None => self.feed_iip(chunk),
            Some(events) => {
                for event in events {
                    match event {
                        SixelEvent::Pass(pass) => self.feed_iip(&pass),
                        SixelEvent::Image(image) => {
                            self.stats.sixel_images += 1;
                            self.record_image(ProtocolKind::Sixel, image.width, image.height, None);
                        }
                    }
                }
            }
        }
    }

    fn feed_iip(&mut self, bytes: &[u8]) {
        let decode = self.opts.decode_images;
        match self.iip.feed(bytes, decode) {
            None => self.feed_kitty(bytes),
            Some(events) => {
                for event in events {
                    match event {
                        IipEvent::Pass(pass) => self.feed_kitty(&pass),
                        IipEvent::Image(image) => {
                            self.stats.iip_images += 1;
                            self.record_image(ProtocolKind::Iip, image.width, image.height, None);
                        }
                    }
                }
            }
        }
    }

    fn feed_kitty(&mut self, bytes: &[u8]) {
        let events = self.kitty.feed(bytes);
        for event in events {
            match event {
                KittyEvent::Pass(pass) => {
                    if !pass.is_empty() {
                        self.stats.kitty_passes += 1;
                        self.feed_vte(&pass);
                    }
                }
                KittyEvent::Command(command) => {
                    self.stats.kitty_commands += 1;
                    match command {
                        KittyCommand::Transmit(transmit) => {
                            self.kitty_dims.insert(
                                transmit.image.id,
                                (transmit.image.width, transmit.image.height),
                            );
                        }
                        KittyCommand::Place(place) => {
                            let (w, h) = place
                                .source_rect
                                .map(|r| (r.w, r.h))
                                .or_else(|| self.kitty_dims.get(&place.image_id).copied())
                                .unwrap_or((0, 0));
                            self.record_image(
                                ProtocolKind::Kitty,
                                w,
                                h,
                                place.source_rect.map(|r| (r.x, r.y, r.w, r.h)),
                            );
                        }
                        KittyCommand::Delete(_) | KittyCommand::Response(_) => {}
                    }
                }
                KittyEvent::Error(_) => self.stats.kitty_errors += 1,
            }
        }
    }

    fn record_image(
        &mut self,
        kind: ProtocolKind,
        width: u32,
        height: u32,
        source_rect: Option<(u32, u32, u32, u32)>,
    ) {
        // 图片锚点取“事件发生时”的 Term 光标/RowId；只 pull 一次，不改变
        // 生产帧路径的 pull 节奏（生产帧用另一个 TermSim 回放）。
        let (cursor_row, col, row_id) = {
            let frame = self.image_sim.pull();
            let row = (frame.cursor.line as usize).min(frame.rows.saturating_sub(1));
            (
                row,
                frame.cursor.col,
                frame.row_ids.get(row).copied().unwrap_or(0),
            )
        };
        self.anchors.push(ImageAnchor {
            kind,
            width,
            height,
            cursor_row,
            col,
            row_id,
            source_rect,
        });
    }

    fn feed_vte(&mut self, bytes: &[u8]) {
        let mut kept = Vec::with_capacity(bytes.len());
        for &b in bytes {
            let index = self.emitted;
            self.emitted += 1;
            if self.drop_at == Some(index) {
                self.stats.swallowed += 1;
                continue;
            }
            kept.push(b);
        }
        if kept.is_empty() {
            return;
        }
        self.image_sim.advance(&kept);
        self.filtered.extend_from_slice(&kept);
        if let Some(piece) = self.pieces.last_mut() {
            piece.extend_from_slice(&kept);
        }
    }

    fn finish(self) -> PipelineRun {
        PipelineRun {
            filtered: self.filtered,
            pieces: self.pieces,
            anchors: self.anchors,
            stats: self.stats,
        }
    }
}

pub(crate) fn run_pipeline(
    raw: &[u8],
    opts: &TraceOptions,
    chunk_size: usize,
    drop_at: Option<usize>,
) -> PipelineRun {
    let mut builder = PipelineBuilder::new(opts, drop_at);
    let chunk_size = chunk_size.max(1);
    for chunk in raw.chunks(chunk_size) {
        builder.feed_raw(chunk);
    }
    builder.finish()
}

// ---------------------------------------------------------------------------
// Frame / Term 比较
// ---------------------------------------------------------------------------

fn uri_of<'a>(frame: &'a Frame, cell: &tervo_core::CellData) -> Option<&'a str> {
    if cell.link == 0 {
        None
    } else {
        frame.hyperlinks.get(cell.link as usize).map(String::as_str)
    }
}

fn same_cell(a: &Frame, ai: usize, b: &Frame, bi: usize) -> bool {
    let (ac, bc) = (a.cells[ai], b.cells[bi]);
    let (mut x, mut y) = (ac, bc);
    x.link = 0;
    y.link = 0;
    if x != y || uri_of(a, &ac) != uri_of(b, &bc) {
        return false;
    }
    let acol = ai % a.cols;
    let arow = ai / a.cols;
    let bcol = bi % b.cols;
    let brow = bi / b.cols;
    a.zerowidth_at(arow, acol) == b.zerowidth_at(brow, bcol)
}

fn cell_repr(frame: &Frame, row: usize, col: usize) -> String {
    let cell = frame.cell(row, col);
    let uri = uri_of(frame, cell)
        .map(|u| format!(" uri={u:?}"))
        .unwrap_or_default();
    let zero = frame.zerowidth_at(row, col);
    let zero = if zero.is_empty() {
        String::new()
    } else {
        format!(" zero={zero:?}")
    };
    format!(
        "{:?} U+{:04X} fg={:#08x} bg={:#08x} flags={:#06x}{uri}{zero}",
        cell.char(),
        cell.ch,
        cell.fg,
        cell.bg,
        cell.flags
    )
}

fn frame_row_text(frame: &Frame, row: usize) -> String {
    if row >= frame.rows {
        return String::new();
    }
    frame
        .row(row)
        .iter()
        .map(|c| c.char())
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn first_cell_diff(a: &Frame, b: &Frame) -> Option<(usize, usize)> {
    if a.cols != b.cols || a.rows != b.rows {
        return Some((0, 0));
    }
    for row in 0..a.rows {
        let base = row * a.cols;
        for col in 0..a.cols {
            if !same_cell(a, base + col, b, base + col) {
                return Some((row, col));
            }
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameDiff {
    kind: &'static str,
    row: usize,
    col: usize,
    left: String,
    right: String,
}

fn frame_diff(partial: &Frame, full: &Frame) -> Option<FrameDiff> {
    if let Some((row, col)) = first_cell_diff(partial, full) {
        return Some(FrameDiff {
            kind: "cell",
            row,
            col,
            left: cell_repr(
                partial,
                row.min(partial.rows.saturating_sub(1)),
                col.min(partial.cols.saturating_sub(1)),
            ),
            right: cell_repr(
                full,
                row.min(full.rows.saturating_sub(1)),
                col.min(full.cols.saturating_sub(1)),
            ),
        });
    }
    if partial.cursor != full.cursor {
        return Some(FrameDiff {
            kind: "cursor",
            row: partial.cursor.line as usize,
            col: partial.cursor.col as usize,
            left: format!("{:?}", partial.cursor),
            right: format!("{:?}", full.cursor),
        });
    }
    if partial.mode != full.mode {
        return Some(FrameDiff {
            kind: "mode",
            row: 0,
            col: 0,
            left: format!("{:#x}", partial.mode),
            right: format!("{:#x}", full.mode),
        });
    }
    if partial.display_offset != full.display_offset {
        return Some(FrameDiff {
            kind: "display_offset",
            row: 0,
            col: 0,
            left: partial.display_offset.to_string(),
            right: full.display_offset.to_string(),
        });
    }
    None
}

fn screen_cell_repr(cell: &tervo_core::CellData) -> String {
    format!(
        "{:?} U+{:04X} fg={:#08x} bg={:#08x} flags={:#06x}",
        cell.char(),
        cell.ch,
        cell.fg,
        cell.bg,
        cell.flags
    )
}

fn same_screen_cell(a: &tervo_core::CellData, b: &tervo_core::CellData) -> bool {
    a.ch == b.ch && a.fg == b.fg && a.bg == b.bg && a.flags == b.flags
}

// ---------------------------------------------------------------------------
// 图片 row 裁剪
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageWindow {
    /// 视口内第一个可见行。
    pub first_row: usize,
    /// 视口内最后一个可见行（不含）。
    pub end_row: usize,
    /// 源图从第几个像素行开始可见（`row < 0` 时 > 0）。
    pub source_y0: u32,
    /// 当前（正确）公式写入 framebuffer 的起始像素 y。
    pub dest_y0: u32,
    pub clipped_top: bool,
    pub clipped_bottom: bool,
    /// 旧 `top.max(0.0) + dy0` 公式会写到的 y（用于回归测试，
    /// 实际 renderer 已不使用）。
    pub legacy_dest_y0: u32,
}

/// 把 `Frame.images[*].row` 投影成“这一行 framebuffer 真正能画哪些源像素”。
///
/// `row` 是 cell 行，可以为负（图片顶部滚出视口）。这个函数只做几何，
/// 不读像素；`--pty` 之外的 CLI 路径靠它审计解码事件的锚点。
pub(crate) fn image_window(
    row: i32,
    height: u32,
    cell_height: u16,
    viewport_rows: usize,
) -> Option<ImageWindow> {
    if height == 0 || viewport_rows == 0 || cell_height == 0 {
        return None;
    }
    let cell_h = u32::from(cell_height.max(1));
    let top_px = (row as i64) * i64::from(cell_h);
    let bottom_px = top_px + i64::from(height);
    let view_h = (viewport_rows as u32).saturating_mul(cell_h);
    if bottom_px <= 0 || top_px >= i64::from(view_h) {
        return None;
    }
    let first_row = row.max(0) as usize;
    let cell_rows = ((height + cell_h - 1) / cell_h) as i64;
    let end_row = (row as i64 + cell_rows).clamp(0, viewport_rows as i64) as usize;
    if end_row <= first_row {
        return None;
    }
    let source_y0 = if top_px < 0 { (-top_px) as u32 } else { 0 };
    let dest_y0 = (first_row as u32).saturating_mul(cell_h);
    let legacy_dest_y0 = if top_px < 0 {
        (-top_px) as u32
    } else {
        top_px as u32
    };
    Some(ImageWindow {
        first_row,
        end_row,
        source_y0,
        dest_y0,
        clipped_top: top_px < 0,
        clipped_bottom: bottom_px > i64::from(view_h),
        legacy_dest_y0,
    })
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageIssue {
    pub image_index: usize,
    pub row: i32,
    pub kind: String,
    pub detail: String,
}

/// 审计真实 `Frame.images` 的裁剪几何。供本模块测试与将来的 live trace 使用；
/// CLI 纯字节路径没有 GUI，只能用解码锚点做 placement-time 审计。
#[cfg(test)]
pub(crate) fn audit_frame_images(frame: &Frame, opts: &TraceOptions) -> Vec<ImageIssue> {
    let mut issues = Vec::new();
    for (index, image) in frame.images.iter().enumerate() {
        let cell_h = opts.cell_height.max(1);
        let cell_rows = image.height.div_ceil(u32::from(cell_h)).max(1) as i64;
        if image.row as i64 + cell_rows <= 0 || image.row as i64 >= frame.rows as i64 {
            issues.push(ImageIssue {
                image_index: index,
                row: image.row,
                kind: "fully_outside_but_present".into(),
                detail: format!(
                    "row={} height={} cell_h={} rows={}，core 的 place() 应丢弃完全不可见的图片",
                    image.row, image.height, cell_h, frame.rows
                ),
            });
            continue;
        }
        match image_window(image.row, image.height, cell_h, frame.rows) {
            None => issues.push(ImageIssue {
                image_index: index,
                row: image.row,
                kind: "no_visible_window".into(),
                detail: format!(
                    "row={} height={} cell_h={} rows={}",
                    image.row, image.height, cell_h, frame.rows
                ),
            }),
            Some(window) => {
                if window.source_y0 >= image.height {
                    issues.push(ImageIssue {
                        image_index: index,
                        row: image.row,
                        kind: "clipped_source_y0_past_image".into(),
                        detail: format!(
                            "source_y0={} >= height={}（row={}），该 placement 没有可画的源像素",
                            window.source_y0, image.height, image.row
                        ),
                    });
                }
                if window.dest_y0 != window.first_row as u32 * u32::from(cell_h) {
                    issues.push(ImageIssue {
                        image_index: index,
                        row: image.row,
                        kind: "dest_y0_mismatch".into(),
                        detail: format!(
                            "dest_y0={} != first_row({})*cell_h({})",
                            window.dest_y0, window.first_row, cell_h
                        ),
                    });
                }
            }
        }
    }
    issues
}

// ---------------------------------------------------------------------------
// 分层报告
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Pass,
    Fail,
    Skipped,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skipped => "SKIPPED",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    Filter,
    Term,
    Frame,
    Framebuffer,
    Image,
}

impl Layer {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Term => "term",
            Self::Frame => "frame",
            Self::Framebuffer => "framebuffer",
            Self::Image => "image",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Side {
    label: &'static str,
    value: String,
    byte: Option<u8>,
    hex: Option<String>,
    cell: Option<String>,
    missing: bool,
}

impl Side {
    fn byte(label: &'static str, byte: Option<u8>) -> Self {
        Self {
            label,
            value: match byte {
                Some(b) => format!("0x{b:02x} {:?}", b as char),
                None => "<missing>".into(),
            },
            byte,
            hex: byte.map(|b| format!("{b:02x}")),
            cell: None,
            missing: byte.is_none(),
        }
    }

    fn text(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
            ..Self::default()
        }
    }

    fn cell(label: &'static str, value: String) -> Self {
        let cell = Some(value.clone());
        Self {
            label,
            value,
            cell,
            ..Self::default()
        }
    }

    fn to_json(&self) -> Value {
        let mut map = json!({
            "label": self.label,
            "value": self.value,
            "missing": self.missing,
        });
        if let Some(byte) = self.byte {
            map["byte"] = json!(byte);
        }
        if let Some(hex) = &self.hex {
            map["hex"] = json!(hex);
        }
        if let Some(cell) = &self.cell {
            map["cell"] = json!(cell);
        }
        map
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Divergence {
    pub layer: Layer,
    pub check: &'static str,
    pub step: usize,
    pub chunk: Option<(usize, usize)>,
    pub byte: Option<usize>,
    pub row: Option<usize>,
    pub col: Option<usize>,
    pub left: Side,
    pub right: Side,
    pub context: String,
    pub explain: String,
}

impl Divergence {
    fn to_json(&self) -> Value {
        let mut at = json!({ "step": self.step });
        if let Some(byte) = self.byte {
            at["byte"] = json!(byte);
        }
        if let Some((start, end)) = self.chunk {
            at["chunk"] = json!({ "start": start, "end": end });
        }
        if let Some(row) = self.row {
            at["row"] = json!(row);
        }
        if let Some(col) = self.col {
            at["col"] = json!(col);
        }
        json!({
            "layer": self.layer.as_str(),
            "check": self.check,
            "at": at,
            "left": self.left.to_json(),
            "right": self.right.to_json(),
            "context": self.context,
            "explain": self.explain,
        })
    }

    fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("first divergence:\n");
        out.push_str(&format!("  layer: {}\n", self.layer.as_str()));
        out.push_str(&format!("  check: {}\n", self.check));
        out.push_str(&format!("  step : {}\n", self.step));
        if let Some((start, end)) = self.chunk {
            out.push_str(&format!("  chunk: raw bytes [{start},{end})\n"));
        }
        if let Some(byte) = self.byte {
            out.push_str(&format!("  byte : {byte}\n"));
        }
        if let Some(row) = self.row {
            out.push_str(&format!("  row  : {row}\n"));
        }
        if let Some(col) = self.col {
            out.push_str(&format!("  col  : {col}\n"));
        }
        out.push_str(&format!(
            "  left  ({}): {}\n",
            self.left.label, self.left.value
        ));
        out.push_str(&format!(
            "  right ({}): {}\n",
            self.right.label, self.right.value
        ));
        out.push_str(&format!("  context: {}\n", self.context));
        out.push_str(&format!("  explain: {}\n", self.explain));
        out
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LayerSummary {
    layer: Layer,
    status: Status,
    detail: Vec<(&'static str, Value)>,
}

impl LayerSummary {
    fn new(layer: Layer) -> Self {
        Self {
            layer,
            status: Status::Pass,
            detail: Vec::new(),
        }
    }

    fn with(mut self, key: &'static str, value: Value) -> Self {
        self.detail.push((key, value));
        self
    }

    fn status(mut self, status: Status) -> Self {
        self.status = status;
        self
    }

    fn to_json(&self) -> Value {
        let mut detail = serde_json::Map::new();
        for (key, value) in &self.detail {
            detail.insert((*key).to_string(), value.clone());
        }
        json!({
            "layer": self.layer.as_str(),
            "status": self.status.as_str(),
            "detail": Value::Object(detail),
        })
    }

    fn line(&self) -> String {
        let mut out = format!("{:<11} {:<7}", self.layer.as_str(), self.status.as_str());
        for (key, value) in &self.detail {
            out.push_str("  ");
            let value = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.push_str(&format!("{key}={value}"));
        }
        out
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TraceReport {
    pub status: Status,
    pub input_bytes: usize,
    pub options: TraceOptions,
    pub divergence: Option<Divergence>,
    pub layers: Vec<LayerSummary>,
    pub limitations: Vec<&'static str>,
    pub input: String,
    pub options_source: String,
}

fn context_window(raw: &[u8], at: usize, radius: usize) -> (String, String) {
    let start = at.saturating_sub(radius);
    let end = (at + radius).min(raw.len());
    let mut ascii = String::new();
    let mut hex = String::new();
    for (index, &b) in raw[start..end].iter().enumerate() {
        let abs = start + index;
        let printable = matches!(b, 0x20..=0x7e) || b >= 0x80;
        if printable {
            ascii.push(b as char);
        } else {
            ascii.push('.');
        }
        if abs == at {
            ascii.push_str(" <<<");
        }
        hex.push_str(&format!("{b:02x}"));
        if abs == at {
            hex.push('|');
        }
    }
    (ascii, hex)
}

fn chunk_bounds(options: &TraceOptions, step: usize, raw_len: usize) -> Option<(usize, usize)> {
    let start = step.saturating_sub(1).saturating_mul(options.chunk.max(1));
    if start >= raw_len {
        return None;
    }
    Some((start, (start + options.chunk.max(1)).min(raw_len)))
}

fn step_for_raw_offset(options: &TraceOptions, offset: usize) -> usize {
    offset / options.chunk.max(1) + 1
}

fn filter_divergence(
    chunked: &PipelineRun,
    oneshot: &PipelineRun,
    expected: &[u8],
    offsets: &[usize],
    raw: &[u8],
    options: &TraceOptions,
) -> Option<Divergence> {
    let expected_vs_chunked = first_diff(expected, &chunked.filtered);
    if let Some(index) = expected_vs_chunked {
        let raw_at = offsets
            .get(index)
            .copied()
            .unwrap_or_else(|| raw.len().saturating_sub(1));
        let step = step_for_raw_offset(options, raw_at);
        let (ascii, hex) = context_window(raw, raw_at, 24);
        // 区分“少了一个字节（吞字节）”与“改写成另一个字节”。只看首个
        // 不同点会被后续字节左移误导；用剩余流是否整体左移一位判断删除。
        let missing = expected.len() == chunked.filtered.len() + 1
            || (expected.len() > chunked.filtered.len()
                && suffix_after_delete(expected, &chunked.filtered, index));
        let extra = chunked.filtered.len() == expected.len() + 1
            || (chunked.filtered.len() > expected.len()
                && suffix_after_delete(&chunked.filtered, expected, index));
        let (left, right, explain) = if missing {
            let next = chunked.filtered.get(index).copied();
            (
                Side::byte("ledger_expected", expected.get(index).copied()),
                Side {
                    label: "pipeline",
                    value: match next {
                        Some(b) => format!("<missing; next byte 0x{b:02x} {:?}>", b as char),
                        None => "<missing>".into(),
                    },
                    byte: None,
                    hex: None,
                    cell: None,
                    missing: true,
                },
                format!(
                    "字节守恒失败：协议账本认为 raw byte {raw_at} 不属于任何图片序列，但 filter 链没有输出它（疑似 filter 吞字节）"
                ),
            )
        } else if extra {
            (
                Side {
                    label: "ledger_expected",
                    value: "<missing>".into(),
                    byte: None,
                    hex: None,
                    cell: None,
                    missing: true,
                },
                Side::byte("pipeline", chunked.filtered.get(index).copied()),
                format!("字节守恒失败：filter 链多输出了 {raw_at} 附近的字节（疑似重复处理）"),
            )
        } else {
            (
                Side::byte("ledger_expected", expected.get(index).copied()),
                Side::byte("pipeline", chunked.filtered.get(index).copied()),
                format!("字节守恒失败：filter 链改写了 raw byte {raw_at} 附近的字节"),
            )
        };
        return Some(Divergence {
            layer: Layer::Filter,
            check: "byte_conservation",
            step,
            chunk: chunk_bounds(options, step, raw.len()),
            byte: Some(raw_at),
            row: None,
            col: None,
            left,
            right,
            context: format!("ascii={ascii:?} hex={hex}"),
            explain,
        });
    }

    if let Some(index) = first_diff(&chunked.filtered, &oneshot.filtered) {
        let raw_at = offsets.get(index).copied().unwrap_or(0);
        let step = step_for_raw_offset(options, raw_at);
        let (ascii, hex) = context_window(raw, raw_at, 24);
        return Some(Divergence {
            layer: Layer::Filter,
            check: "chunking",
            step,
            chunk: chunk_bounds(options, step, raw.len()),
            byte: Some(raw_at),
            row: None,
            col: None,
            left: Side::byte("oneshot_filter", oneshot.filtered.get(index).copied()),
            right: Side::byte("chunked_filter", chunked.filtered.get(index).copied()),
            context: format!("ascii={ascii:?} hex={hex}"),
            explain: format!(
                "filter 输出依赖分块边界：同一 raw bytes 按 {} B 分块与一次性喂入的输出在第 {index} 个 filtered byte 分叉",
                options.chunk
            ),
        });
    }
    None
}

/// `long` 是否等于 `short` 在 `at` 处删除一个字节后的结果（用于把
/// “左移一位”识别成吞字节，而不是逐字节改写）。
fn suffix_after_delete(long: &[u8], short: &[u8], at: usize) -> bool {
    if long.len() != short.len() + 1 || at >= long.len() {
        return false;
    }
    long[..at] == short[..at] && long[at + 1..] == short[at..]
}

fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    let common = a.len().min(b.len());
    for i in 0..common {
        if a[i] != b[i] {
            return Some(i);
        }
    }
    if a.len() != b.len() {
        Some(common)
    } else {
        None
    }
}

fn make_frame_divergence(
    layer: Layer,
    check: &'static str,
    step: usize,
    chunk: Option<(usize, usize)>,
    diff: &FrameDiff,
    partial: &Frame,
    full: &Frame,
    options: &TraceOptions,
) -> Divergence {
    let (left_label, right_label) = match layer {
        Layer::Term => ("chunked_term", "oneshot_term"),
        Layer::Frame => ("cached_partial_frame", "force_full_frame"),
        Layer::Framebuffer => ("renderer_screen", "term_truth"),
        _ => ("left", "right"),
    };
    let damage: Vec<Value> = partial
        .damage
        .iter()
        .map(|d| json!({"line": d.line, "left": d.left, "right": d.right}))
        .collect();
    let row_text = frame_row_text(full, diff.row);
    let context = format!(
        "row_text={row_text:?} partial_full={} partial_damage={} cols={} rows={}",
        partial.full,
        Value::Array(damage.clone()),
        options.cols,
        options.rows
    );
    Divergence {
        layer,
        check,
        step,
        chunk,
        byte: None,
        row: Some(diff.row),
        col: Some(diff.col),
        left: Side::cell(left_label, diff.left.clone()),
        right: Side::cell(right_label, diff.right.clone()),
        context,
        explain: match layer {
            Layer::Term => "Term 解析结果依赖分块：同一 filtered bytes 分块/一次性喂入后的 cell 不一致".into(),
            Layer::Frame => {
                "增量 Frame 与 force-full Frame 的第一处 cell/字段分歧（Frame 缓存残留或 damage/对账漏格）".into()
            },
            Layer::Framebuffer => {
                "renderer 按 damage 增量重绘后的 cell 与 Term 真相不一致（用户可见的旧字符/残影）".into()
            },
            _ => "第一处分歧".into(),
        },
    }
}

// ---------------------------------------------------------------------------
// 故障注入（仅自测）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    pub filter_drop: Option<usize>,
    pub frame_residue: Option<(usize, usize)>,
    pub frame_residue_step: usize,
    pub damage_drop: Option<usize>,
}

impl Default for Fault {
    fn default() -> Self {
        Self {
            filter_drop: None,
            frame_residue: None,
            frame_residue_step: 1,
            damage_drop: None,
        }
    }
}

impl Fault {
    fn parse(spec: Option<&str>) -> Result<Self, Box<dyn Error>> {
        let Some(spec) = spec else {
            return Ok(Self::default());
        };
        let mut fault = Self::default();
        for item in spec.split(',') {
            if let Some(value) = item.strip_prefix("filter-drop=") {
                fault.filter_drop = Some(value.parse().map_err(|_| "filter-drop=N 需要整数")?);
            } else if let Some(value) = item.strip_prefix("frame-residue=") {
                let (position, step) = match value.split_once('@') {
                    Some((position, step)) => (position, step.parse().unwrap_or(1)),
                    None => (value, 1),
                };
                let (row, col) = position
                    .split_once(':')
                    .ok_or("frame-residue=ROW:COL[@STEP] 需要行列")?;
                fault.frame_residue = Some((
                    row.parse().map_err(|_| "frame-residue 行必须是整数")?,
                    col.parse().map_err(|_| "frame-residue 列必须是整数")?,
                ));
                fault.frame_residue_step = step;
            } else if let Some(value) = item.strip_prefix("damage-drop=") {
                fault.damage_drop = Some(value.parse().map_err(|_| "damage-drop=R 需要整数")?);
            } else {
                return Err(format!("未知 --fault 项: {item}").into());
            }
        }
        Ok(fault)
    }

    fn apply(&self, frame: &mut Frame, step: usize) {
        if let Some((row, col)) = self.frame_residue {
            if step >= self.frame_residue_step && row < frame.rows && col < frame.cols {
                let cell = &mut frame.cells[row * frame.cols + col];
                cell.ch = u32::from('#');
                cell.fg = 0;
                cell.bg = 0;
                cell.flags = 0;
                cell.link = 0;
            }
        }
        if let Some(row) = self.damage_drop {
            frame.damage.retain(|d| d.line as usize != row);
        }
    }
}

// ---------------------------------------------------------------------------
// 主分析
// ---------------------------------------------------------------------------

pub(crate) fn analyze(
    raw: &[u8],
    options: &TraceOptions,
    fault: &Fault,
    input: &str,
    options_source: &str,
) -> TraceReport {
    let mut layers = Vec::new();
    let mut limitations = vec![
        "不比较完整像素 framebuffer / Slint 窗口：CLI 不创建 GUI；framebuffer 层是 cell 级 damage 模型",
        "图片层审计解码事件的 placement 裁剪几何；不比较 chunked/oneshot 锚点（SixelFilter 同 chunk 事件顺序是 core 已知边界）",
        "Term 只有 alacritty 解析器一种实现；Term 层比较的是“分块 vs 一次性”，不是与第二个终端模拟器对照",
    ];

    let spans = scan_protocol_spans(raw);
    let (expected, offsets) = ledger_expected(raw, &spans);
    let chunked = run_pipeline(raw, options, options.chunk, fault.filter_drop);
    let oneshot = run_pipeline(raw, options, raw.len().max(1), None);

    let span_count = |kind: ProtocolKind| spans.iter().filter(|s| s.kind == kind).count();
    let filter_summary_base = || {
        LayerSummary::new(Layer::Filter)
            .with("raw_bytes", json!(raw.len()))
            .with("filtered_bytes", json!(chunked.filtered.len()))
            .with(
                "consumed_bytes",
                json!(raw.len().saturating_sub(chunked.filtered.len())),
            )
            .with("ledger_bytes", json!(expected.len()))
            .with("ledger_spans", json!(spans.len()))
            .with("sixel_spans", json!(span_count(ProtocolKind::Sixel)))
            .with("iip_spans", json!(span_count(ProtocolKind::Iip)))
            .with("kitty_spans", json!(span_count(ProtocolKind::Kitty)))
            .with("sixel_images", json!(chunked.stats.sixel_images))
            .with("iip_images", json!(chunked.stats.iip_images))
            .with("kitty_commands", json!(chunked.stats.kitty_commands))
            .with("kitty_errors", json!(chunked.stats.kitty_errors))
            .with("chunk", json!(options.chunk))
    };

    if let Some(divergence) =
        filter_divergence(&chunked, &oneshot, &expected, &offsets, raw, options)
    {
        layers.push(filter_summary_base().status(Status::Fail));
        layers.push(LayerSummary::new(Layer::Term).status(Status::Skipped));
        layers.push(LayerSummary::new(Layer::Frame).status(Status::Skipped));
        layers.push(LayerSummary::new(Layer::Framebuffer).status(Status::Skipped));
        layers.push(LayerSummary::new(Layer::Image).status(Status::Skipped));
        return TraceReport {
            status: Status::Fail,
            input_bytes: raw.len(),
            options: options.clone(),
            divergence: Some(divergence),
            layers,
            limitations: std::mem::take(&mut limitations),
            input: input.to_string(),
            options_source: options_source.to_string(),
        };
    }

    let mut filter_summary = filter_summary_base().with(
        "chunking_stable",
        json!(chunked.filtered == oneshot.filtered),
    );
    if chunked.filtered != oneshot.filtered {
        // 理论上 filter_divergence 已覆盖；保留 defensive 标记。
        filter_summary = filter_summary.status(Status::Fail);
    }
    layers.push(filter_summary);

    // ---- Term / Frame / framebuffer ----
    let mut prod = TermSim::new(options.cols, options.rows, options.scrollback);
    let mut screen: Vec<tervo_core::CellData> = Vec::new();
    let mut screen_valid = false;
    let mut pulls = 0usize;
    let mut full_pulls = 0usize;
    let mut partial_pulls = 0usize;
    let mut max_damage_rows = 0usize;
    let mut final_truth: Option<Frame> = None;
    let mut frame_divergence: Option<Divergence> = None;
    let mut framebuffer_divergence: Option<Divergence> = None;
    let mut cumulative = 0usize;
    // 大输入 / 极小 chunk 时避免 O(pieces × total_bytes) 的重复解析：最多
    // 做 256 次“全新 Term 真相”对账，仍保持生产 pull 节奏并每步更新
    // renderer 模型；最后一次 pull 一定对账。
    let check_stride = (chunked.pieces.len() / 256).max(1);

    for (index, piece) in chunked.pieces.iter().enumerate() {
        cumulative += piece.len();
        prod.advance(piece);
        let mut partial = prod.pull().clone();
        pulls += 1;
        if partial.full {
            full_pulls += 1;
        } else {
            partial_pulls += 1;
        }
        max_damage_rows = max_damage_rows.max(partial.damage.len());
        fault.apply(&mut partial, index + 1);

        let should_check = index % check_stride == 0 || index + 1 == chunked.pieces.len();
        // renderer 模型每步都要更新；真相对账按 stride 抽样。
        let needs_full = partial.full || partial.damage.len() > options.rows / 2;
        if !screen_valid || needs_full {
            screen = partial.cells.clone();
            screen_valid = true;
        } else {
            for damage in &partial.damage {
                let row = damage.line as usize;
                if row >= options.rows {
                    continue;
                }
                let base = row * options.cols;
                screen[base..base + options.cols]
                    .copy_from_slice(&partial.cells[base..base + options.cols]);
            }
        }
        if !should_check {
            continue;
        }

        // force-full 真相：同一批 filtered bytes 一次性喂入一个全新 Term，首帧必为整屏。
        let mut truth_sim = TermSim::new(options.cols, options.rows, options.scrollback);
        truth_sim.advance(&chunked.filtered[..cumulative]);
        let truth = truth_sim.pull().clone();

        if let Some(diff) = frame_diff(&partial, &truth) {
            // 先判定究竟是 Term 分块差异，还是 Frame 缓存/对账差异。
            let mut chunked_sim = TermSim::new(options.cols, options.rows, options.scrollback);
            for previous in &chunked.pieces[..=index] {
                chunked_sim.advance(previous);
            }
            let chunked_frame = chunked_sim.pull().clone();
            let chunk = chunk_bounds(options, index + 1, raw.len());
            frame_divergence = Some(
                if let Some(term_diff) = frame_diff(&chunked_frame, &truth) {
                    make_frame_divergence(
                        Layer::Term,
                        "term_chunking",
                        index + 1,
                        chunk,
                        &term_diff,
                        &chunked_frame,
                        &truth,
                        options,
                    )
                } else {
                    make_frame_divergence(
                        Layer::Frame,
                        "partial_vs_force_full",
                        index + 1,
                        chunk,
                        &diff,
                        &partial,
                        &truth,
                        options,
                    )
                },
            );
            final_truth = Some(truth);
            break;
        }

        let mut stale: Option<(usize, usize)> = None;
        'screen: for row in 0..options.rows {
            let base = row * options.cols;
            for col in 0..options.cols {
                if !same_screen_cell(&screen[base + col], &truth.cells[base + col]) {
                    stale = Some((row, col));
                    break 'screen;
                }
            }
        }
        if let Some((row, col)) = stale {
            let diff = FrameDiff {
                kind: "stale_cell",
                row,
                col,
                left: screen_cell_repr(&screen[row * options.cols + col]),
                right: screen_cell_repr(&truth.cells[row * options.cols + col]),
            };
            framebuffer_divergence = Some(make_frame_divergence(
                Layer::Framebuffer,
                "damage_residue",
                index + 1,
                chunk_bounds(options, index + 1, raw.len()),
                &diff,
                &partial,
                &truth,
                options,
            ));
            final_truth = Some(truth);
            break;
        }
        final_truth = Some(truth);
    }

    let truth = final_truth.unwrap_or_else(|| {
        let mut sim = TermSim::new(options.cols, options.rows, options.scrollback);
        if !chunked.filtered.is_empty() {
            sim.advance(&chunked.filtered);
        }
        sim.pull().clone()
    });

    if frame_divergence.is_none() && framebuffer_divergence.is_none() {
        // 最终再做一次显式的 chunked vs oneshot Term 对账（覆盖末尾空 piece 等情况）。
        let mut chunked_sim = TermSim::new(options.cols, options.rows, options.scrollback);
        for piece in &chunked.pieces {
            chunked_sim.advance(piece);
        }
        let mut oneshot_sim = TermSim::new(options.cols, options.rows, options.scrollback);
        oneshot_sim.advance(&chunked.filtered);
        let chunked_frame = chunked_sim.pull().clone();
        let oneshot_frame = oneshot_sim.pull().clone();
        if let Some(diff) = frame_diff(&chunked_frame, &oneshot_frame) {
            frame_divergence = Some(make_frame_divergence(
                Layer::Term,
                "term_chunking_final",
                chunked.pieces.len().max(1),
                chunk_bounds(options, chunked.pieces.len().max(1), raw.len()),
                &diff,
                &chunked_frame,
                &oneshot_frame,
                options,
            ));
        }
    }

    layers.push(
        LayerSummary::new(Layer::Term)
            .with("checked", json!(true))
            .with("filtered_bytes", json!(chunked.filtered.len()))
            .with("chunk", json!(options.chunk))
            .with("cols", json!(options.cols))
            .with("rows", json!(options.rows))
            .status(
                if matches!(
                    frame_divergence.as_ref().map(|d| d.layer),
                    Some(Layer::Term)
                ) {
                    Status::Fail
                } else {
                    Status::Pass
                },
            ),
    );
    if let Some(divergence) = frame_divergence {
        layers.push(
            LayerSummary::new(Layer::Frame)
                .with("pulls", json!(pulls))
                .with("full_pulls", json!(full_pulls))
                .with("partial_pulls", json!(partial_pulls))
                .with("max_damage_rows", json!(max_damage_rows))
                .status(if divergence.layer == Layer::Frame {
                    Status::Fail
                } else {
                    Status::Skipped
                }),
        );
        layers.push(
            LayerSummary::new(Layer::Framebuffer)
                .with("pulls", json!(pulls))
                .status(Status::Skipped),
        );
        layers.push(LayerSummary::new(Layer::Image).status(Status::Skipped));
        return TraceReport {
            status: Status::Fail,
            input_bytes: raw.len(),
            options: options.clone(),
            divergence: Some(divergence),
            layers,
            limitations,
            input: input.to_string(),
            options_source: options_source.to_string(),
        };
    }

    layers.push(
        LayerSummary::new(Layer::Frame)
            .with("pulls", json!(pulls))
            .with("full_pulls", json!(full_pulls))
            .with("partial_pulls", json!(partial_pulls))
            .with("max_damage_rows", json!(max_damage_rows))
            .with("final_generation", json!(truth.generation))
            .status(Status::Pass),
    );

    if let Some(divergence) = framebuffer_divergence {
        layers.push(
            LayerSummary::new(Layer::Framebuffer)
                .with("pulls", json!(pulls))
                .with("stale_cells", json!(1))
                .status(Status::Fail),
        );
        layers.push(LayerSummary::new(Layer::Image).status(Status::Skipped));
        return TraceReport {
            status: Status::Fail,
            input_bytes: raw.len(),
            options: options.clone(),
            divergence: Some(divergence),
            layers,
            limitations,
            input: input.to_string(),
            options_source: options_source.to_string(),
        };
    }

    layers.push(
        LayerSummary::new(Layer::Framebuffer)
            .with("pulls", json!(pulls))
            .with("stale_cells", json!(0))
            .with("model", json!("cell_damage"))
            .status(Status::Pass),
    );

    // ---- 图片层 ----
    let image_divergence = image_divergence(&chunked, &oneshot, options, raw);
    if let Some(divergence) = image_divergence {
        layers.push(
            LayerSummary::new(Layer::Image)
                .with("anchors", json!(chunked.anchors.len()))
                .status(Status::Fail),
        );
        return TraceReport {
            status: Status::Fail,
            input_bytes: raw.len(),
            options: options.clone(),
            divergence: Some(divergence),
            layers,
            limitations,
            input: input.to_string(),
            options_source: options_source.to_string(),
        };
    }
    let image_status = if chunked.anchors.is_empty() {
        Status::Skipped
    } else {
        Status::Pass
    };
    layers.push(
        LayerSummary::new(Layer::Image)
            .with("anchors", json!(chunked.anchors.len()))
            .with("sixel_images", json!(chunked.stats.sixel_images))
            .with("iip_images", json!(chunked.stats.iip_images))
            .with("kitty_commands", json!(chunked.stats.kitty_commands))
            .with(
                "clip_checks",
                json!(chunked.anchors.iter().filter(|a| a.height > 0).count()),
            )
            .status(image_status),
    );
    if chunked.anchors.is_empty() {
        limitations.push("输入里没有解码出的图片；图片层显示 SKIPPED");
    }
    limitations.push(
        "Filter 层校验“非图片协议字节必须原样保留”；协议账本是独立整段扫描器，不跑生产 filter",
    );

    let status = if layers.iter().any(|l| l.status == Status::Fail) {
        Status::Fail
    } else {
        Status::Pass
    };
    TraceReport {
        status,
        input_bytes: raw.len(),
        options: options.clone(),
        divergence: None,
        layers,
        limitations,
        input: input.to_string(),
        options_source: options_source.to_string(),
    }
}

fn image_divergence(
    chunked: &PipelineRun,
    _oneshot: &PipelineRun,
    options: &TraceOptions,
    raw: &[u8],
) -> Option<Divergence> {
    // 说明：不比较 chunked/oneshot 的 ImageAnchor。SixelFilter 在同一个
    // feed() 里会先发 Image、最后才发 Pass，图片前的文本可能尚未喂给
    // Term；这会让 anchor 依赖 chunk 边界（core 侧已知边界，不在本切片
    // 修复）。这里只审计锚点本身的裁剪几何，避免把 core 的事件顺序问题
    // 误报成 trace-layer 的图片层。
    for (index, anchor) in chunked.anchors.iter().enumerate() {
        if anchor.width == 0 || anchor.height == 0 {
            // Kitty transmit 缺失时只看 Place；不把未跟踪到尺寸当成裁剪分歧。
            continue;
        }
        let window = image_window(
            anchor.cursor_row as i32,
            anchor.height,
            options.cell_height,
            options.rows,
        );
        let Some(window) = window else {
            return Some(Divergence {
                layer: Layer::Image,
                check: "placement_window",
                step: 1,
                chunk: None,
                byte: None,
                row: Some(anchor.cursor_row),
                col: Some(anchor.col as usize),
                left: Side::text("anchor", format!("{anchor:?}")),
                right: Side::text("image_window", "None"),
                context: format!(
                    "image #{index} kind={} width={} height={} cursor_row={} cols={} rows={}",
                    anchor.kind.as_str(),
                    anchor.width,
                    anchor.height,
                    anchor.cursor_row,
                    options.cols,
                    options.rows
                ),
                explain: "图片锚点落在视口内，但裁剪窗口为空（锚点/行数不一致）".into(),
            });
        };
        if window.source_y0 >= anchor.height {
            return Some(Divergence {
                layer: Layer::Image,
                check: "placement_clip",
                step: 1,
                chunk: None,
                byte: None,
                row: Some(anchor.cursor_row),
                col: Some(anchor.col as usize),
                left: Side::text("anchor", format!("{anchor:?}")),
                right: Side::text("window", format!("{window:?}")),
                context: format!(
                    "image #{index} input_bytes={} chunk={}",
                    raw.len(),
                    options.chunk
                ),
                explain: "row 裁剪后 source_y0 超过图片高度：该 placement 没有可见源像素".into(),
            });
        }
    }
    None
}

impl TraceReport {
    pub(crate) fn to_json(&self) -> Value {
        let layers: Vec<Value> = self.layers.iter().map(LayerSummary::to_json).collect();
        json!({
            "schema": "tervo.diag.v1",
            "kind": "trace_layer",
            "version": VERSION,
            "commit": COMMIT,
            "target": TARGET,
            "input": self.input,
            "options_source": self.options_source,
            "input_bytes": self.input_bytes,
            "status": self.status.as_str(),
            "options": {
                "cols": self.options.cols,
                "rows": self.options.rows,
                "chunk": self.options.chunk,
                "scrollback": self.options.scrollback,
                "cell_width": self.options.cell_width,
                "cell_height": self.options.cell_height,
                "decode_images": self.options.decode_images,
            },
            "first_divergence": self.divergence.as_ref().map(Divergence::to_json),
            "layers": layers,
            "limitations": self.limitations,
        })
    }

    pub(crate) fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "tervo diag trace-layer: {}\n",
            self.status.as_str()
        ));
        out.push_str(&format!(
            "input: {} ({} bytes)\n",
            self.input, self.input_bytes
        ));
        out.push_str(&format!(
            "options: {}x{} chunk={} cell={}x{} ({})\n",
            self.options.cols,
            self.options.rows,
            self.options.chunk,
            self.options.cell_width,
            self.options.cell_height,
            self.options_source,
        ));
        out.push_str("layers:\n");
        for layer in &self.layers {
            out.push_str("  ");
            out.push_str(&layer.line());
            out.push('\n');
        }
        if let Some(divergence) = &self.divergence {
            out.push('\n');
            out.push_str(&divergence.to_text());
        } else {
            out.push_str("\nPASS: 所有已比较层一致\n");
        }
        out.push_str("\nlimitations:\n");
        for limitation in &self.limitations {
            out.push_str(&format!("  - {limitation}\n"));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 合成 fixture
// ---------------------------------------------------------------------------

fn small_sixel() -> Vec<u8> {
    let mut out = b"\x1bPq\"1;1;2;2#0;2;100;0;0#1;2;0;100;0#0B#1B".to_vec();
    out.push(0x07);
    out
}

pub(crate) fn synthetic_fixture(name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut out = Vec::new();
    match name {
        "clean" => {
            out.extend_from_slice(b"\x1b[32mhello\x1b[0m world\r\n");
            out.extend_from_slice(b"line2 [stderr] ] [Image #1]\x1b[1Cdone\r\n");
            out.extend_from_slice(
                "\x1b]8;;https://example.com/a\x1b\\link\x1b]8;;\x1b\\\r\n".as_bytes(),
            );
            out.extend_from_slice(b"\x1b[?1049hALT-screen\x1b[?1049l");
            out.extend_from_slice(b"\x1b[2J\x1b[Hfinal\x1b[K\r\n");
            out.extend_from_slice(&small_sixel());
            out.extend_from_slice(b"after-image\r\n");
            // 注意：这里放在 Sixel 之后。SixelFilter 会把正文里的 C1 0x9D
            // 当作 OSC 起点；如果后面紧跟 Sixel DCS，一次性喂入会把 DCS
            // 当 OSC 内容（chunked 与 oneshot 分叉）。这是 filter 的已知
            // 边界，单独由 `filter_c1_text_before_dcs_is_chunk_dependent`
            // 测试暴露；clean 夹具不混入这个输入。
            out.extend_from_slice("UTF-8 \u{201d}] tail\r\n".as_bytes());
        }
        "images" => {
            out.extend_from_slice(b"\x1b[2J\x1b[H");
            for row in 1..=6 {
                out.extend_from_slice(format!("row-{row:02} text\r\n").as_bytes());
            }
            out.extend_from_slice(b"before-image\r\n");
            out.extend_from_slice(&small_sixel());
            out.extend_from_slice(b"after-image\r\n");
            out.extend_from_slice(&small_sixel());
        }
        "filter-edge" => {
            // 历史 IIP 缺陷：`ESC x` 后状态未回 Ground，正文里的 `]` 被当
            // OSC 前缀吞掉；0x9D 也可能是 UTF-8 续字节。这个夹具要求 filter
            // 链逐字节保留所有正文。
            out.extend_from_slice(b"plain \x1b[31m[stderr]\x1b[39m [Image #1]\x1b[1Cend\r\n");
            out.extend_from_slice("curly \u{201d}] then ESC \x1b[2C!\r\n".as_bytes());
            out.extend_from_slice(b"\x1bXos-command-suffix]x\x1b\\tail]after\r\n");
            out.extend_from_slice(b"plain-ascii-]esc\x1b[0m-ok\r\n");
        }
        other => {
            return Err(format!("未知 fixture: {other}（clean | images | filter-edge）").into())
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> TraceOptions {
        TraceOptions {
            cols: 20,
            rows: 6,
            chunk: 7,
            ..TraceOptions::default()
        }
    }

    #[test]
    fn ledger_keeps_text_and_strips_only_image_protocols() {
        let raw = synthetic_fixture("clean").unwrap();
        let spans = scan_protocol_spans(&raw);
        assert!(spans.iter().any(|s| s.kind == ProtocolKind::Sixel));
        let (expected, offsets) = ledger_expected(&raw, &spans);
        assert_eq!(offsets.len(), expected.len());
        assert!(expected.windows(5).any(|w| w == b"hello"));
        assert!(expected.windows(10).any(|w| w == b"[stderr] ]"));
        assert!(!expected.windows(3).any(|w| w == b"\x1bPq"));
        assert_eq!(offsets.first(), Some(&0));
    }

    #[test]
    fn utf8_curly_quote_0x9d_is_not_an_osc_start() {
        let raw = "pre \u{201d}] post\r\n".as_bytes();
        let spans = scan_protocol_spans(raw);
        assert!(spans.is_empty(), "UTF-8 续字节不能当 C1 OSC: {spans:?}");
        let run = run_pipeline(raw, &opts(), 3, None);
        assert_eq!(run.filtered, raw, "filter 不能吞 UTF-8/右引号后的字节");
    }

    #[test]
    fn clean_fixture_passes_all_layers() {
        let raw = synthetic_fixture("clean").unwrap();
        let report = analyze(&raw, &opts(), &Fault::default(), "fixture:clean", "test");
        assert_eq!(report.status, Status::Pass, "{}", report.to_text());
        assert!(report.divergence.is_none());
        assert!(report
            .layers
            .iter()
            .any(|l| l.layer == Layer::Frame && l.status == Status::Pass));
    }

    #[test]
    fn filter_swallow_is_located_at_the_exact_raw_byte() {
        let raw = b"alpha [stderr] ] [Image #1] omega\r\n".to_vec();
        let needle = b"[stderr]".as_slice();
        let at = raw.windows(needle.len()).position(|w| w == needle).unwrap() + 3;
        let report = analyze(
            &raw,
            &opts(),
            &Fault {
                filter_drop: Some(at),
                ..Fault::default()
            },
            "synthetic",
            "test",
        );
        assert_eq!(report.status, Status::Fail, "{}", report.to_text());
        let divergence = report.divergence.expect("must locate divergence");
        assert_eq!(divergence.layer, Layer::Filter);
        assert_eq!(divergence.check, "byte_conservation");
        assert_eq!(divergence.byte, Some(at));
        assert_eq!(divergence.left.byte, Some(raw[at]));
        assert!(divergence.right.missing);
        assert!(
            divergence.context.contains("alpha") && divergence.context.contains("err"),
            "context={}",
            divergence.context
        );
    }

    #[test]
    fn filter_swallow_of_utf8_continuation_is_located() {
        // 复刻历史缺陷：`”` (E2 80 9D) 的续字节被吞。
        let raw = "curly \u{201d}] tail\r\n".as_bytes().to_vec();
        let at = raw.iter().position(|&b| b == 0x9d).unwrap();
        let report = analyze(
            &raw,
            &opts(),
            &Fault {
                filter_drop: Some(at),
                ..Fault::default()
            },
            "synthetic",
            "test",
        );
        assert_eq!(report.status, Status::Fail);
        let divergence = report.divergence.expect("must locate divergence");
        assert_eq!(divergence.layer, Layer::Filter);
        assert_eq!(divergence.byte, Some(at));
    }

    #[test]
    fn frame_residue_is_located_by_row_and_col() {
        let raw = b"\x1b[2J\x1b[HAAAAAAAA\r\nBBBBBBBB\r\nCCCCCCCC".to_vec();
        let report = analyze(
            &raw,
            &opts(),
            &Fault {
                frame_residue: Some((1, 2)),
                frame_residue_step: 5,
                ..Fault::default()
            },
            "synthetic",
            "test",
        );
        assert_eq!(report.status, Status::Fail, "{}", report.to_text());
        let divergence = report.divergence.expect("must locate divergence");
        assert_eq!(divergence.layer, Layer::Frame);
        assert_eq!(divergence.row, Some(1));
        assert_eq!(divergence.col, Some(2));
        assert!(divergence.left.value.contains('#'));
        assert!(divergence.right.value.contains('B'));
        assert!(divergence.context.contains("row_text"));
    }

    #[test]
    fn dropped_damage_row_is_located_as_framebuffer_residue() {
        let raw = b"first line\r\nsecond line\r\nthird line".to_vec();
        let report = analyze(
            &raw,
            &opts(),
            &Fault {
                damage_drop: Some(0),
                ..Fault::default()
            },
            "synthetic",
            "test",
        );
        assert_eq!(report.status, Status::Fail, "{}", report.to_text());
        let divergence = report.divergence.expect("must locate divergence");
        assert_eq!(divergence.layer, Layer::Framebuffer);
        assert_eq!(divergence.check, "damage_residue");
        assert_eq!(divergence.row, Some(0));
    }

    #[test]
    fn image_row_clip_geometry_matches_renderer_contract() {
        // 2 行高、锚点 row=-1：只有第 0 行可见，源图从第 cell_h 像素开始画。
        let window = image_window(-1, 40, 20, 2).expect("partially visible");
        assert_eq!(window.first_row, 0);
        assert_eq!(window.end_row, 1);
        assert_eq!(window.source_y0, 20);
        assert_eq!(window.dest_y0, 0);
        assert!(window.clipped_top);
        // 旧公式 `top.max(0.0) + dy0` 会写到 y=20（第 1 行），超出图片
        // 自己合法的 row_span（仅第 0 行）。这个断言把事故公式钉在测试里。
        assert_eq!(window.legacy_dest_y0, 20);
        assert!(
            window.legacy_dest_y0 >= window.end_row as u32 * 20,
            "legacy dest_y0 应落在合法 row span 之外"
        );

        // 完全在视口上方：必须返回 None（core 的 place() 也应丢弃）。
        assert!(image_window(-2, 20, 20, 2).is_none());
    }

    fn tall_sixel() -> Vec<u8> {
        // 4x60、颜色 #0 全红：高度跨 3 个 20px cell，row=-1 时仍有可见像素。
        let mut out = b"\x1bPq\"1;1;4;60#0;2;100;0;0#0".to_vec();
        for _ in 0..10 {
            out.extend_from_slice(b"!4@-");
        }
        out.extend_from_slice(b"\x1b\\");
        out
    }

    #[test]
    fn pty_image_with_negative_row_is_clipped_and_audited() {
        let path =
            std::env::temp_dir().join(format!("tervo-trace-layer-img-{}.bin", std::process::id()));
        std::fs::write(&path, tall_sixel()).expect("write sixel");
        let script = format!(
            "printf '\\033[1;1H'; cat '{}'; printf '\\033[2;1H\\r\\n'; sleep 0.5",
            path.display()
        );
        let handle = tervo_core::spawn(tervo_core::PaneConfig {
            cols: 20,
            rows: 2,
            scrollback: 100,
            shell: Some("/bin/bash".to_string()),
            args: vec!["-c".to_string(), script],
            ..tervo_core::PaneConfig::default()
        })
        .expect("spawn pane");
        let mut frame = handle.new_frame();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            frame = handle.pull(frame);
            if frame.images.iter().any(|image| image.row == -1) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等不到 row=-1 的图片；当前 images={:?}",
                frame.images
            );
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let options = TraceOptions {
            cols: 20,
            rows: 2,
            cell_width: 10,
            cell_height: 20,
            ..TraceOptions::default()
        };
        let issues = audit_frame_images(&frame, &options);
        assert!(
            issues.is_empty(),
            "真实 Frame.images 裁剪审计失败: {issues:?}"
        );
        let image = frame.images.iter().find(|i| i.row == -1).unwrap();
        let window = image_window(image.row, image.height, 20, 2).expect("partially visible");
        assert_eq!(window.first_row, 0);
        assert_eq!(window.source_y0, 20, "顶部应被裁掉一个 cell 的源像素");
        assert!(window.clipped_top);
        assert!(
            window.legacy_dest_y0 < 40,
            "legacy 公式会写到第 1 行（y=20），仍应落在测试可见范围"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn audit_frame_images_flags_fully_outside_placement() {
        let mut frame = Frame::new(10, 2);
        frame.images.push(tervo_core::InlineImage {
            id: 1,
            source_id: 1,
            row_id: 1,
            row: -2,
            col: 0,
            width: 2,
            height: 20,
            data: std::sync::Arc::from(vec![0u8; 2 * 20 * 4].into_boxed_slice()),
            z_index: -1,
            above_text: false,
        });
        let opts = TraceOptions {
            cols: 10,
            rows: 2,
            cell_height: 20,
            ..TraceOptions::default()
        };
        let issues = audit_frame_images(&frame, &opts);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "fully_outside_but_present");
    }

    #[test]
    fn real_dsh_raw_passes_when_fixture_exists() {
        let Some(path) = locate_dsh_raw() else {
            eprintln!("跳过：未找到 dsh app-to-host.raw");
            return;
        };
        let raw = std::fs::read(&path).expect("read dsh raw");
        let mut options = TraceOptions {
            cols: 130,
            rows: 40,
            chunk: 32 * 1024,
            ..TraceOptions::default()
        };
        if let Some(sidecar) = find_sidecar(&path.display().to_string()) {
            let _ = apply_options_file(&mut options, &sidecar);
        }
        let report = analyze(
            &raw,
            &options,
            &Fault::default(),
            &path.display().to_string(),
            "dsh",
        );
        assert_eq!(report.status, Status::Pass, "{}", report.to_text());
    }

    #[test]
    fn filter_c1_text_before_dcs_is_stable_after_core_fix() {
        // 真实边界：正文里的 0x9D（UTF-8 `”` 的续字节）会被 SixelFilter
        // 当作 C1 OSC 起点；若同一个 feed chunk 后面还有 Sixel DCS，
        // 一次性喂入会把 DCS 当 OSC 内容，而分块喂入可能在 0x9D 所在
        // chunk 走“无 DCS 快速路径”从而正确识别。trace-layer 必须把
        // 这种 chunk 依赖定位到 filter 层，而不是只打印统计。
        let mut raw = "pre \u{201d}] text\r\n".as_bytes().to_vec();
        raw.extend_from_slice(&small_sixel());
        let mut options = opts();
        options.chunk = 7;
        let report = analyze(&raw, &options, &Fault::default(), "synthetic", "test");
        assert_eq!(report.status, Status::Pass, "{}", report.to_text());
        assert!(report.divergence.is_none());
    }

    fn locate_dsh_raw() -> Option<std::path::PathBuf> {
        if let Some(path) = std::env::var_os("TERVO_DSH_RAW") {
            let path = std::path::PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let candidates = [
            manifest.join("../../.logs/dsh-dump-run/pty/app-to-host.raw"),
            manifest.join("../../../.logs/dsh-dump-run/pty/app-to-host.raw"),
            manifest.join("../../../../.logs/dsh-dump-run/pty/app-to-host.raw"),
        ];
        candidates.into_iter().find(|p| p.is_file())
    }
}
