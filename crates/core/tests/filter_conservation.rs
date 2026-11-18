// SPDX-License-Identifier: GPL-3.0-only
//! 图片字节过滤链的**守恒审计**。
//!
//! 2026-11-11 的事故根因之一是 IIP filter 静默吞字节（`ESC x` 后的 `]`
//! 丢失），而当时没有任何“输入 = 透传 + 显式消费”的契约测试。本文件把
//! `crates/core/src/pane.rs::feed` 的三级分派顺序（Sixel → IIP → Kitty APC
//! → vte）镜像成一个可独立驱动的管线，并对三类 filter 断言：
//!
//! 1. 纯文本输入在任意 chunk 切分下**逐字节**等于输入；
//! 2. 图片序列只吞自己，前后文本完整、顺序不变；
//! 3. 非法/截断序列之后，后续转义/文本仍按原样到达 vte（到得了的字节不丢）；
//! 4. 任意 chunk 边界（ESC/OSC/0x9D/UTF-8 续字节中间）结果与单 chunk 一致；
//! 5. 守恒账本：`input == passed + swallowed(expected sequence bytes) + held`。
//!
//! 注意：真正的图片解码由各 filter 自己完成；这里统计 `Image` 事件数量，
//! 不复制解码逻辑。管线镜像与 `pane.rs` 的分派顺序一一对应，`pane.rs`
//! 改动后本文件需要同步（见文末 `pipeline_mirror_matches_pane_source`）。

use tervo_core::iip::{IipEvent, IipFilter};
use tervo_core::kitty_graphics::{KittyEvent, KittyParser};
use tervo_core::osc::encode_base64;
use tervo_core::sixel::{SixelEvent, SixelFilter};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;

/// 一次完整管线 feed 的结果。`pass` 是最终会交给 vte 的字节。
#[derive(Default, Debug, PartialEq, Eq)]
struct Output {
    pass: Vec<u8>,
    sixel_images: usize,
    iip_images: usize,
    /// 合法但非内联（inline 缺省/0）的 IIP：无图、无 pass，只发通知。
    iip_ignored: usize,
    kitty_commands: usize,
    kitty_errors: usize,
}

impl Output {
    fn absorb(&mut self, other: &mut Output) {
        self.pass.append(&mut other.pass);
        self.sixel_images += other.sixel_images;
        self.iip_images += other.iip_images;
        self.iip_ignored += other.iip_ignored;
        self.kitty_commands += other.kitty_commands;
        self.kitty_errors += other.kitty_errors;
    }
}

/// `pane.rs::feed` 的逐级镜像。字段顺序/分派条件必须与生产代码一致。
struct Pipeline {
    sixel: SixelFilter,
    iip: IipFilter,
    kitty: KittyParser,
}

impl Pipeline {
    fn new() -> Self {
        Self { sixel: SixelFilter::new(), iip: IipFilter::new(), kitty: KittyParser::new() }
    }

    /// 镜像 `PaneActor::feed`：Sixel 先过，Pass 字节进 IIP，再进 Kitty APC。
    fn feed(&mut self, bytes: &[u8]) -> Output {
        let mut out = Output::default();
        // pane.rs: `let filtered = self.sixel.feed(bytes, decode_sixel);`
        // `None` 表示本段没有 DCS 起点，原始字节直接进入下一级。
        match self.sixel.feed(bytes, true) {
            None => self.feed_iip(bytes, &mut out),
            Some(events) => {
                for event in events {
                    match event {
                        SixelEvent::Pass(chunk) => self.feed_iip(&chunk, &mut out),
                        SixelEvent::Image(_) => out.sixel_images += 1,
                    }
                }
            },
        }
        out
    }

    /// 镜像 `PaneActor::feed_iip`。
    fn feed_iip(&mut self, bytes: &[u8], out: &mut Output) {
        let Some(events) = self.iip.feed(bytes, true) else {
            self.feed_kitty(bytes, out);
            return;
        };
        for event in events {
            match event {
                IipEvent::Pass(chunk) => {
                    if !chunk.is_empty() {
                        self.feed_kitty(&chunk, out);
                    }
                },
                IipEvent::Image(_) => out.iip_images += 1,
                IipEvent::Ignored(_) => out.iip_ignored += 1,
            }
        }
    }

    /// 镜像 `PaneActor::process_vte_chunk`，包括“没有 ESC 时不进 Kitty
    /// 状态机”的热路径。
    fn feed_kitty(&mut self, bytes: &[u8], out: &mut Output) {
        if bytes.is_empty() {
            return;
        }
        if !self.kitty.has_partial_sequence() && !bytes.contains(&ESC) {
            out.pass.extend_from_slice(bytes);
            return;
        }
        for event in self.kitty.feed(bytes) {
            match event {
                KittyEvent::Pass(chunk) => out.pass.extend_from_slice(&chunk),
                KittyEvent::Command(_) => out.kitty_commands += 1,
                KittyEvent::Error(_) => out.kitty_errors += 1,
            }
        }
    }

    /// 三个 filter 内部尚未决定去留的字节数。`input == pass + swallowed
    /// + held` 中的 held。
    fn held(&self) -> usize {
        self.sixel.held_bytes() + self.iip.held_bytes() + self.kitty.held_bytes()
    }
}

fn feed_chunks(chunks: &[&[u8]]) -> (Output, usize) {
    let mut pipeline = Pipeline::new();
    let mut out = Output::default();
    for chunk in chunks {
        let mut chunk_out = pipeline.feed(chunk);
        out.absorb(&mut chunk_out);
    }
    let held = pipeline.held();
    (out, held)
}

fn feed_one(data: &[u8]) -> (Output, usize) {
    feed_chunks(&[data])
}

/// 把 `data` 按 `cuts`（必须递增、< len）切开；返回拥有所有权的 chunks。
fn split_at(data: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
    let mut chunks = Vec::with_capacity(cuts.len() + 1);
    let mut start = 0;
    for &cut in cuts {
        assert!(cut > start && cut < data.len(), "cut {cut} 不合法（len={}）", data.len());
        chunks.push(data[start..cut].to_vec());
        start = cut;
    }
    chunks.push(data[start..].to_vec());
    chunks
}

fn as_refs(chunks: &[Vec<u8>]) -> Vec<&[u8]> {
    chunks.iter().map(Vec::as_slice).collect()
}

/// 只在单字节处逐点切分（含“每个字节一个 chunk”）。
fn byte_splits(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
    let mut all = Vec::new();
    for cut in 1..data.len() {
        all.push(split_at(data, &[cut]));
    }
    all.push(data.iter().map(|b| vec![*b]).collect());
    all
}

/// 确定性 xorshift64：测试不引入 rand，失败可复现（种子固定）。
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// 返回 `len` 个升序、互不相同的切点。
    fn cuts(&mut self, len: usize, count: usize) -> Vec<usize> {
        if len < 2 {
            return Vec::new();
        }
        let mut cuts: Vec<usize> = (0..count.min(len - 1))
            .map(|_| 1 + (self.next_u64() as usize % (len - 1)))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        cuts
    }
}

/// 守恒账本：输入字节必须恰好落在“透传 / 已识别的整段图片序列 / 暂存”
/// 三者之一。任何一类对不上都说明有字节被静默丢弃。
#[track_caller]
fn assert_conserved(input: &[u8], out: &Output, held: usize, swallowed: usize, label: &str) {
    let accounted = out.pass.len() + swallowed + held;
    assert_eq!(
        accounted,
        input.len(),
        "{label}: 字节不守恒：input={} pass={} swallowed={} held={}\npass={:?}",
        input.len(),
        out.pass.len(),
        swallowed,
        held,
        String::from_utf8_lossy(&out.pass)
    );
}

// ---------------------------------------------------------------- fixtures

/// 2x2 Sixel（左列红、右列绿），带完整 DCS 头。
fn sixel_payload() -> Vec<u8> {
    b"\"1;1;2;2#0;2;100;0;0#1;2;0;100;0#0B#1B".to_vec()
}

fn sixel_st() -> Vec<u8> {
    let mut out = b"\x1bPq".to_vec();
    out.extend_from_slice(&sixel_payload());
    out.extend_from_slice(b"\x1b\\");
    out
}

fn sixel_bel() -> Vec<u8> {
    let mut out = b"\x1bPq".to_vec();
    out.extend_from_slice(&sixel_payload());
    out.push(BEL);
    out
}

/// 2x2 RGBA PNG（红/绿/蓝/半透明白）。
const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEUlEQVR4nGP4z8DwHwyhxH8AQ84H+Qn+tmsAAAAASUVORK5CYII=";

fn iip_sequence() -> Vec<u8> {
    let mut out = b"\x1b]1337;File=inline=1;name=dGlueS5wbmc=:".to_vec();
    out.extend_from_slice(TINY_PNG_B64.as_bytes());
    out.push(BEL);
    out
}

/// Kitty APC：2x2 RGB 原图直接传输。返回 (sequence, image_id)。
fn kitty_transmit() -> Vec<u8> {
    // 2x2 RGB：红、绿、蓝、白，每个像素 3 字节。
    let rgb: &[u8] = &[
        255, 0, 0, 0, 255, 0, //
        0, 0, 255, 255, 255, 255,
    ];
    let payload = encode_base64(rgb);
    let mut out = b"\x1b_Ga=t,f=24,s=2,v=2,i=77,q=2;".to_vec();
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

// ---------------------------------------------------------------- 纯文本

/// 只含普通文本/非图片转义序列的输入。要求输出与输入逐字节一致。
fn plain_text_corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut corpus: Vec<(&'static str, Vec<u8>)> = vec![
        ("ascii", b"hello world\r\nsecond line\r\n".to_vec()),
        ("sgr", b"\x1b[31mred\x1b[0m plain\x1b[1Bnext".to_vec()),
        ("osc-title-bel", b"\x1b]0;my title\x07after title".to_vec()),
        ("osc-title-st", b"\x1b]2;my title\x1b\\after title".to_vec()),
        ("osc8-link", b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ tail".to_vec()),
        ("dsr", b"\x1b[6n\x1b[?25l\x1b[?25h".to_vec()),
        // 真实 dsh 输出里的中文/按钮/框线：UTF-8 续字节恰好是 0x98/0x9D/
        // 0x9F/0x90/0x9B/0x9E（C1 控制符区间）。
        ("dsh-cjk", "桌面是两块 256 色，某区域┐ ❯ ⛶ 图 目 果 实".as_bytes().to_vec()),
        ("cyrillic", "Русский текст: АБВГДЕЖ ПРИВЕТ".as_bytes().to_vec()),
        ("german", "Grüße, Straße, ẞ".as_bytes().to_vec()),
        ("c1-osc", b"\x9d0;c1 title\x07after".to_vec()),
        ("c1-dcs-st", b"\x90$q1;2\x9cafter".to_vec()),
        ("c1-csi", b"\x9b31mafter".to_vec()),
        ("c1-apc", b"\x9fGgarbage\x9cafter".to_vec()),
        ("can-sub", b"left\x18right\x1atail".to_vec()),
        // 2026-11-11 第一处根因回归（IIP filter 状态泄漏）：`ESC x` 后 state
        // 没回 Ground，紧跟的 `]` 被当 OSC 前缀丢弃。最小序列 + 真实 dsh 行。
        ("iip-esc-x-bracket", b"\x1b[m]\x1b[0m".to_vec()),
        (
            "dsh-stderr-line",
            b"\x1b[31m[stderr]\x1b[39m[Image #1]\x1b[1Cdone\x1b[0m".to_vec(),
        ),
    ];
    corpus.push(("mixed", {
        let mut v = b"\x1b[2J\x1b[1;1H".to_vec();
        v.extend_from_slice("中文 Mixed Русский 🐋 ❯".as_bytes());
        v.extend_from_slice(b"\r\n\x1b]0;title\x07done");
        v
    }));
    corpus
}

#[test]
fn plain_text_is_byte_identical_for_every_chunking() {
    for (name, input) in plain_text_corpus() {
        let (out, held) = feed_one(&input);
        assert_eq!(out.pass, input, "{name}: 单 chunk 输出必须逐字节等于输入");
        assert_eq!(held, 0, "{name}: 完整输入不应有暂存字节");
        assert_conserved(&input, &out, held, 0, name);
        assert_eq!(out.sixel_images + out.iip_images + out.kitty_commands, 0, "{name}");

        for (index, chunks) in byte_splits(&input).into_iter().enumerate() {
            let refs = as_refs(&chunks);
            let (out, held) = feed_chunks(&refs);
            assert_eq!(
                out.pass, input,
                "{name}: 切分 #{index}（chunks={}）破坏了逐字节守恒",
                chunks.len()
            );
            assert_eq!(held, 0, "{name}: 切分 #{index} 结束时仍有暂存字节");
        }

        // 随机多切点（覆盖“一次切断多个状态机”的组合）。
        for seed in 0..64u64 {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed ^ input.len() as u64);
            let cuts = rng.cuts(input.len(), 6);
            let chunks = split_at(&input, &cuts);
            let refs = as_refs(&chunks);
            let (out, held) = feed_chunks(&refs);
            assert_eq!(out.pass, input, "{name}: 随机切分 seed={seed} 破坏守恒");
            assert_eq!(held, 0, "{name}: seed={seed} 结束时仍有暂存字节");
        }
    }
}

// ---------------------------------------------------------------- 图片序列

#[derive(Clone, Copy)]
enum ImageKind {
    SixelSt,
    SixelBel,
    Iip,
    Kitty,
}

impl ImageKind {
    fn sequence(self) -> Vec<u8> {
        match self {
            ImageKind::SixelSt => sixel_st(),
            ImageKind::SixelBel => sixel_bel(),
            ImageKind::Iip => iip_sequence(),
            ImageKind::Kitty => kitty_transmit(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            ImageKind::SixelSt => "sixel-st",
            ImageKind::SixelBel => "sixel-bel",
            ImageKind::Iip => "iip",
            ImageKind::Kitty => "kitty",
        }
    }
}

#[test]
fn image_sequence_swallows_only_itself() {
    let prefix = b"BEFORE\r\n".to_vec();
    let suffix = b"\r\nAFTER".to_vec();
    for kind in [ImageKind::SixelSt, ImageKind::SixelBel, ImageKind::Iip, ImageKind::Kitty] {
        let seq = kind.sequence();
        let mut input = prefix.clone();
        input.extend_from_slice(&seq);
        input.extend_from_slice(&suffix);
        let expected: Vec<u8> = [prefix.as_slice(), suffix.as_slice()].concat();

        let (out, held) = feed_one(&input);
        assert_eq!(out.pass, expected, "{}: 前后文本必须完整且顺序不变", kind.name());
        assert_eq!(held, 0, "{}: 完整序列不应留暂存字节", kind.name());
        assert_conserved(&input, &out, held, seq.len(), kind.name());
        match kind {
            ImageKind::SixelSt | ImageKind::SixelBel => {
                assert_eq!(out.sixel_images, 1, "{}: 应识别 1 张 Sixel", kind.name());
                assert_eq!(out.iip_images, 0);
            },
            ImageKind::Iip => {
                assert_eq!(out.iip_images, 1, "IIP: 应识别 1 张图片");
                assert_eq!(out.sixel_images, 0);
            },
            ImageKind::Kitty => {
                assert_eq!(out.kitty_commands, 1, "kitty: 应识别 1 条命令");
                assert_eq!(out.kitty_errors, 0);
            },
        }

        // 每一个切点（含逐字节 drip）都必须是同一结果。
        for (index, chunks) in byte_splits(&input).into_iter().enumerate() {
            let refs = as_refs(&chunks);
            let (chunked, held) = feed_chunks(&refs);
            assert_eq!(chunked, out, "{}: 切分 #{index} 结果与单 chunk 不一致", kind.name());
            assert_eq!(held, 0, "{}: 切分 #{index} 残留暂存字节", kind.name());
        }
        for seed in 0..64u64 {
            let mut rng = Rng(0xdead_beef ^ seed ^ seq.len() as u64);
            let cuts = rng.cuts(input.len(), 8);
            let chunks = split_at(&input, &cuts);
            let refs = as_refs(&chunks);
            let (chunked, held) = feed_chunks(&refs);
            assert_eq!(chunked, out, "{}: 随机切分 seed={seed} 结果不一致", kind.name());
            assert_eq!(held, 0, "{}: seed={seed} 残留暂存字节", kind.name());
        }
    }
}

#[test]
fn multiple_images_keep_text_and_order() {
    let mut input = b"one\n".to_vec();
    input.extend_from_slice(&sixel_bel());
    input.extend_from_slice(b"two\n");
    input.extend_from_slice(&iip_sequence());
    input.extend_from_slice(b"three\n");
    input.extend_from_slice(&kitty_transmit());
    input.extend_from_slice(b"four\n");
    let expected = b"one\ntwo\nthree\nfour\n".to_vec();
    let seq_len = sixel_bel().len() + iip_sequence().len() + kitty_transmit().len();

    let (out, held) = feed_one(&input);
    assert_eq!(out.pass, expected);
    assert_eq!(held, 0);
    assert_conserved(&input, &out, held, seq_len, "mixed-images");
    assert_eq!((out.sixel_images, out.iip_images, out.kitty_commands), (1, 1, 1));

    for (index, chunks) in byte_splits(&input).into_iter().enumerate() {
        let refs = as_refs(&chunks);
        let (chunked, held) = feed_chunks(&refs);
        assert_eq!(chunked, out, "混合序列切分 #{index} 结果不一致");
        assert_eq!(held, 0);
    }
}

// ---------------------------------------------------------------- UTF-8/C1 冲突

/// 真实 dsh raw 里出现过的字符：UTF-8 续字节落在 0x90/0x98/0x9B/0x9D/
/// 0x9E/0x9F（C1 控制符区间）。filter 不能把它们误当 C1 控制符，否则
/// 状态机会停在 OSC/DCS/APC 里，紧跟其后的图片序列被当成正文放给 vte。
fn c1_continuation_text() -> Vec<u8> {
    // 每个字符至少命中一个 C1 续字节：
    // 是 E6 98 AF / 面 E9 9D A2 / 某 E6 9F 90 / 域 E5 9F 9F /
    // ┐ E2 94 90 / ⛶ E2 9B B6 / 果 E6 9E 9C / Русский А D0 90.
    "桌面是两块 256 色，某区域┐ ❯ ⛶ 图 目 果 实 Русский АБВ"
        .as_bytes()
        .to_vec()
}

#[test]
fn utf8_c1_continuation_then_sixel_is_not_swallowed_by_vte() {
    let text = c1_continuation_text();
    let mut input = text.clone();
    input.extend_from_slice(&sixel_bel());
    input.extend_from_slice(b"AFTER");

    let (out, _held) = feed_one(&input);
    assert_eq!(out.pass, [text.as_slice(), b"AFTER"].concat(), "CJK 文本后紧跟 Sixel 时文本必须完整");
    assert_eq!(out.sixel_images, 1, "Sixel 必须被识别，而不是漏给 vte 当 DCS");
}

#[test]
fn utf8_c1_continuation_then_iip_is_not_swallowed_by_vte() {
    let text = c1_continuation_text();
    let mut input = text.clone();
    input.extend_from_slice(&iip_sequence());
    input.extend_from_slice(b"AFTER");

    let (out, _held) = feed_one(&input);
    assert_eq!(out.pass, [text.as_slice(), b"AFTER"].concat());
    assert_eq!(out.iip_images, 1);
}

// ---------------------------------------------------------------- 截断/非法

#[test]
fn truncated_iip_is_aborted_by_next_escape_sequence() {
    // 截断的 IIP（没有 BEL/ST），紧跟一条 SGR：SGR 必须原样到达 vte，
    // 不能因为 filter 停在 body 里把 `ESC[31m` 丢掉。
    let mut input = b"\x1b]1337;File=inline=1;".to_vec();
    input.extend_from_slice(b"!!!not-base64!!!");
    input.extend_from_slice(b"\x1b[31mAFTER\x1b[0m");
    let (out, _held) = feed_one(&input);
    assert_eq!(out.pass, b"\x1b[31mAFTER\x1b[0m");
    assert_eq!(out.iip_images, 0);
}

#[test]
fn truncated_iip_is_aborted_by_can_and_following_text_survives() {
    // CAN 是 OSC 的合法中止符：中止后 `AFTER` 必须继续走 vte。
    let mut input = b"left".to_vec();
    input.extend_from_slice(b"\x1b]1337;File=inline=1;");
    input.extend_from_slice(b"garbage");
    input.push(CAN);
    input.extend_from_slice(b"AFTER");
    let (out, _held) = feed_one(&input);
    assert_eq!(out.pass, b"leftAFTER");
    assert_eq!(out.iip_images, 0);
}

#[test]
fn truncated_sixel_is_aborted_by_next_escape_sequence() {
    // 截断的 Sixel（没有 BEL/ST），紧跟 SGR：SGR 必须原样到达 vte。
    let mut input = b"\x1bPq".to_vec();
    input.extend_from_slice(&sixel_payload());
    input.push(b'!'); // 故意截断 terminator
    input.extend_from_slice(b"\x1b[31mAFTER\x1b[0m");
    let (out, _held) = feed_one(&input);
    assert_eq!(out.pass, b"\x1b[31mAFTER\x1b[0m", "截断 Sixel 后的 SGR 不能丢 ESC");
    assert_eq!(out.sixel_images, 0);
}

#[test]
fn malformed_kitty_apc_does_not_swallow_following_text() {
    // 非法 APC（ANSI 序列导致解析错误）：只吞 APC，后文完整。
    let mut input = b"left".to_vec();
    input.extend_from_slice(b"\x1b_Ga=t,f=24,INVALID\x1b\\");
    input.extend_from_slice(b"\x1b[32mright\x1b[0m");
    let (out, held) = feed_one(&input);
    assert_eq!(out.pass, b"left\x1b[32mright\x1b[0m", "非法 Kitty APC 之后文本必须完整");
    assert_eq!(held, 0);
    assert!(out.kitty_commands + out.kitty_errors >= 1);
}

#[test]
fn unterminated_kitty_apc_is_bounded_and_aborts_on_can() {
    // 没有 ST 的 APC：CAN 中止后，后续文本必须恢复。
    let mut input = b"\x1b_Ga=t,f=24,s=2,v=2;".to_vec();
    input.extend_from_slice(b"AAAA");
    input.push(CAN);
    input.extend_from_slice(b"AFTER");
    let (out, held) = feed_one(&input);
    assert_eq!(out.pass, b"AFTER");
    assert_eq!(held, 0);
}

#[test]
fn c1_st_terminators_are_preserved_for_vte() {
    // 非图片的 C1 字符串/DCS 必须原样放给 vte（过滤器只负责图片序列）。
    for input in [
        b"\x1b]0;x\x07t".as_slice(),
        b"\x9d0;x\x07t".as_slice(),
        b"\x9b31mt\x9b0m".as_slice(),
        b"\x90$q1\x9ct".as_slice(),
        b"\x98sos\x9ct".as_slice(),
    ] {
        let (out, held) = feed_one(input);
        assert_eq!(out.pass, input, "input={input:?}");
        assert_eq!(held, 0);
    }
}

// ---------------------------------------------------------------- 随机 chunk

#[test]
fn random_chunk_boundaries_mixed_corpus_are_chunking_invariant() {
    let mut corpus = Vec::new();
    for (name, mut bytes) in plain_text_corpus() {
        bytes.extend_from_slice(b"\r\n");
        corpus.push((name, bytes));
    }
    for kind in [ImageKind::SixelSt, ImageKind::SixelBel, ImageKind::Iip, ImageKind::Kitty] {
        let mut bytes = b"marker\n".to_vec();
        bytes.extend_from_slice(&kind.sequence());
        bytes.extend_from_slice(b"\r\nend\n");
        corpus.push((kind.name(), bytes));
    }
    // 截断序列 + 恢复用的转义。
    corpus.push((
        "truncated-iip",
        b"\x1b]1337;File=inline=1;garbage\x1b[0mTAIL".to_vec(),
    ));
    corpus.push((
        "truncated-sixel",
        {
            let mut v = b"\x1bPq#0;2;100;0;0AAAAAAAAA".to_vec();
            v.extend_from_slice(b"\x1b[0mTAIL");
            v
        },
    ));

    for (name, input) in corpus {
        let (reference, _) = feed_one(&input);
        for seed in 0..128u64 {
            let mut rng = Rng(0x1234_5678_9abc_def1 ^ seed ^ input.len() as u64);
            let cuts = rng.cuts(input.len(), 10);
            let chunks = split_at(&input, &cuts);
            let refs = as_refs(&chunks);
            let (out, held) = feed_chunks(&refs);
            assert_eq!(
                out, reference,
                "{name}: 随机切分 seed={seed} cuts={cuts:?} 与单 chunk 结果不同"
            );
            assert_eq!(held, 0, "{name}: seed={seed} 结束时仍有暂存字节");
        }
    }
}

#[test]
fn esc_and_osc_split_at_every_boundary() {
    // 需求里点名的边界：ESC 单独成 chunk、OSC 前缀被切断、0x9D 前后切断。
    let cases: Vec<Vec<u8>> = vec![
        b"\x1b[31mRED\x1b[0m".to_vec(),
        b"\x1b]0;title\x07text".to_vec(),
        b"\x1b]8;;https://e.com\x1b\\x\x1b]8;;\x1b\\".to_vec(),
        {
            let mut v = b"text\x1b]1337;File=inline=1;".to_vec();
            v.extend_from_slice(TINY_PNG_B64.as_bytes());
            v.push(BEL);
            v.extend_from_slice(b"tail");
            v
        },
        b"text\x9d0;c1\x07tail".to_vec(),
    ];
    for input in cases {
        let (reference, _) = feed_one(&input);
        for split in 1..input.len() {
            let chunks = split_at(&input, &[split]);
            let refs = as_refs(&chunks);
            let (out, held) = feed_chunks(&refs);
            assert_eq!(out, reference, "split={split} input={input:?}");
            assert_eq!(held, 0, "split={split} 残留暂存");
        }
        // 0x9D 前后各断一次（真实 dsh raw 里的 `”` / `面`）。
        let positions: Vec<usize> = input
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == 0x9d)
            .map(|(i, _)| i)
            .collect();
        for pos in positions {
            for cut in [pos, pos + 1] {
                if cut == 0 || cut >= input.len() {
                    continue;
                }
                let chunks = split_at(&input, &[cut]);
                let refs = as_refs(&chunks);
                let (out, _) = feed_chunks(&refs);
                assert_eq!(out, reference, "0x9D cut={cut} input={input:?}");
            }
        }
    }
}

// ---------------------------------------------------------------- 防漂移

/// 管线镜像必须与 `pane.rs` 的分派顺序一致。这个测试不评判语义，只在
/// `pane.rs` 的 `feed` 里不再按 `sixel -> iip -> kitty` 的顺序调用时失败，
/// 提示审计方同步本文件。
#[test]
fn pipeline_mirror_matches_pane_source() {
    let source = include_str!("../src/pane.rs");
    let feed_start = source.find("fn feed(&mut self, bytes: &[u8]) -> bool {").expect("pane::feed");
    let feed_end = source[feed_start..]
        .find("fn feed_iip")
        .map(|offset| feed_start + offset)
        .expect("pane::feed_iip");
    let feed_body = &source[feed_start..feed_end];
    let sixel = feed_body.find("self.sixel.feed").expect("feed 必须调用 sixel");
    let iip = feed_body.find("self.feed_iip").expect("feed 必须调用 feed_iip");
    assert!(sixel < iip, "分派顺序变化：sixel 必须早于 iip");

    let iip_end = source[feed_end..]
        .find("fn process_vte_chunk")
        .map(|offset| feed_end + offset)
        .expect("pane::process_vte_chunk");
    let iip_body = &source[feed_end..iip_end];
    assert!(iip_body.contains("self.process_vte_chunk"), "feed_iip 必须调用 process_vte_chunk");

    let chunk_end = source[iip_end..]
        .find("fn process_vte_bytes")
        .map(|offset| iip_end + offset)
        .expect("pane::process_vte_bytes");
    let chunk_body = &source[iip_end..chunk_end];
    assert!(chunk_body.contains("self.kitty.feed"), "process_vte_chunk 必须调用 kitty.feed");
}


// ---------------------------------------------------------------- 真实 raw

/// 真实 dsh-tui 会话 capture 的逐字节切片（见 `tests/fixtures/README.md`）。
fn dsh_text_slice() -> &'static [u8] {
    include_bytes!("fixtures/dsh-2026-11-10-text-cjk.raw")
}

fn dsh_kitty_slice() -> &'static [u8] {
    include_bytes!("fixtures/dsh-2026-11-10-kitty-chunked.raw")
}

/// 完整 Kitty APC（`ESC _ G ... ESC \`）在 `data` 中的字节区间。
fn apc_spans(data: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == ESC && data[i + 1] == b'_' && data[i + 2] == b'G' {
            // 终止符：ESC \ / C1 ST / BEL / CAN / SUB 里最先出现的。
            let mut end = None;
            let mut j = i + 3;
            while j < data.len() {
                if data[j] == ESC && j + 1 < data.len() && data[j + 1] == b'\\' {
                    end = Some(j + 2);
                    break;
                }
                if matches!(data[j], 0x9c | BEL | CAN | 0x1a) {
                    end = Some(j + 1);
                    break;
                }
                j += 1;
            }
            let end = end.expect("fixture 里的 APC 必须有终止符");
            spans.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    spans
}

fn without_spans(data: &[u8], spans: &[(usize, usize)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut cursor = 0;
    for &(start, end) in spans {
        assert!(start >= cursor);
        out.extend_from_slice(&data[cursor..start]);
        cursor = end;
    }
    out.extend_from_slice(&data[cursor..]);
    out
}

#[test]
fn real_dsh_cjk_text_slice_is_byte_identical() {
    let input = dsh_text_slice();
    let (out, held) = feed_one(input);
    assert_eq!(held, 0, "真实文本 slice 结束时不应有暂存字节");
    assert_eq!(
        out.pass, input,
        "真实 dsh 文本（含 UTF-8 续字节 0x90/0x98/0x9D/0x9F）必须逐字节透传"
    );
    assert_eq!(out.sixel_images + out.iip_images + out.kitty_commands, 0);

    // 逐字节 drip：PTY 理论上可以这样切。
    let chunks: Vec<Vec<u8>> = input.iter().map(|b| vec![*b]).collect();
    let refs = as_refs(&chunks);
    let (drip, held) = feed_chunks(&refs);
    assert_eq!(held, 0);
    assert_eq!(drip, out, "逐字节 drip 与单 chunk 结果必须一致");

    // 随机切分（含 ESC/OSC 中间）。
    for seed in 0..128u64 {
        let mut rng = Rng(0xd5_0000 ^ seed);
        let cuts = rng.cuts(input.len(), 24);
        let chunks = split_at(input, &cuts);
        let refs = as_refs(&chunks);
        let (chunked, held) = feed_chunks(&refs);
        assert_eq!(held, 0);
        assert_eq!(chunked, out, "真实文本 slice 随机切分 seed={seed} 结果不一致");
    }
}

#[test]
fn real_dsh_kitty_chunked_is_conserved() {
    let input = dsh_kitty_slice();
    let spans = apc_spans(input);
    assert!(spans.len() >= 4, "fixture 应包含 m=1 分片 + m=0 尾片，实际 {}", spans.len());
    let swallowed: usize = spans.iter().map(|(s, e)| e - s).sum();
    let expected = without_spans(input, &spans);

    let (out, held) = feed_one(input);
    assert_eq!(held, 0, "真实 Kitty 传输结束时不应有暂存字节");
    assert_conserved(input, &out, held, swallowed, "dsh-kitty");
    assert_eq!(
        out.pass, expected,
        "真实 raw 的透传字节必须恰好等于输入去掉 APC 后的内容"
    );
    assert!(out.kitty_commands >= 2, "应识别分片传输 + placement，实际 {}", out.kitty_commands);
    assert_eq!(out.sixel_images + out.iip_images, 0);

    // 每个 APC 内部（control/payload/ST 中间）切成两半。
    for &(start, end) in &spans {
        for cut in [start + 1, start + 4, (start + end) / 2, end - 1] {
            if cut == 0 || cut >= input.len() {
                continue;
            }
            let chunks = split_at(input, &[cut]);
            let refs = as_refs(&chunks);
            let (chunked, held) = feed_chunks(&refs);
            assert_eq!(held, 0, "APC 切点 {cut} 残留暂存");
            assert_eq!(chunked, out, "APC 切点 {cut} 改变了结果");
        }
    }

    // 随机边界：包含把 `ESC \` 拆开、把 base64 中间拆开的情况。
    for seed in 0..64u64 {
        let mut rng = Rng(0xd5_c0de ^ seed);
        let cuts = rng.cuts(input.len(), 16);
        let chunks = split_at(input, &cuts);
        let refs = as_refs(&chunks);
        let (chunked, held) = feed_chunks(&refs);
        assert_eq!(held, 0, "seed={seed} 残留暂存");
        assert_eq!(chunked, out, "真实 Kitty slice 随机切分 seed={seed} 结果不一致");
    }
}

// ---------------------------------------------------------------- 回归

/// 回归（2026-11-11 审计发现，同 commit 修复）：Sixel filter 在
/// `SixelEscape` 只有 ESC 一个字节时必须返回 `Some`。旧实现返回 `None`，
/// 调用方会把这个 ESC 原样交给 vte，造成**字节重复**：`ESC \` 被 PTY 切成
/// 两块时，vte 收到孤立 ESC，和下一个文本字节组成转义序列，吞掉后续字符。
#[test]
fn regression_sixel_lone_esc_must_not_be_passed() {
    let mut filter = SixelFilter::new();
    let mut input = b"\x1bPq".to_vec();
    input.extend_from_slice(&sixel_payload());
    assert!(filter.feed(&input, true).is_some());
    // ESC 单独成 chunk（`ESC \` 的前半）。
    assert!(
        filter.feed(b"\x1b", true).is_some(),
        "处于 SixelEscape 时不能返回 None：原始 ESC 会被重复交给 vte"
    );
}

/// 回归：UTF-8 续字节 0x90 不能被当 C1 DCS 起点。旧实现会把普通文本暂存
/// 在 DCS 头里；若随后第一个 0x40..=0x7E 字节是 `q`，会误判成 Sixel 并
/// 丢弃已暂存文本。真实 dsh raw 里 `┐`(E2 94 90)/`某`(E6 9F 90) 都含 0x90。
#[test]
fn regression_c1_continuation_bytes_are_not_controls() {
    // `各` = E5 90 84，后面紧跟 ASCII `q`：会被误判成 Sixel 起点。
    let input = b"\xe5\x90\x84qHELLO".to_vec();
    let (out, _held) = feed_one(&input);
    assert_eq!(out.pass, input, "UTF-8 文本不能触发 Sixel 状态机");
    assert_eq!(out.sixel_images, 0);
}
