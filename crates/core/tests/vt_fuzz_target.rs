// SPDX-License-Identifier: GPL-3.0-only
//! 确定性 VT fuzz 目标（TERM-002）。
//!
//! 不依赖 cargo-fuzz/libFuzzer：自带 LCG + 变异语料 + 字节汤，固定种子可复现。
//! 每次运行断言四件事：
//!
//! 1. **不 panic**：任意字节流（不完整/分包/非法/超长）都能喂完；
//! 2. **不越界**：Frame 尺寸恒定，光标永远落在 `rows × cols` 内；
//! 3. **有界内存**：回滚历史 ≤ 配置的 scrollback，fuzz 期间 live heap 峰值有上限；
//! 4. **确定性**：同一种子重复跑，帧校验和与吞吐统计相同。
//!
//! 用法：
//!
//! ```text
//! cargo test -p xtervo-core --test vt_fuzz_target --offline
//! cargo test -p xtervo-core --test vt_fuzz_target --offline -- --seed 7 --iters 200000
//! ```
//!
//! 默认参数压到 1–2 秒，`cargo test -p xtervo-core` 会直接跑到它；CI/soak
//! 用 `scripts/fuzz_vt.sh` 加种子/轮数（`--long` 约 1 分钟）。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use xtervo_core::TermSim;

// ---------------------------------------------------------------------------
// 计数分配器：只用于本测试二进制，量 fuzz 期间的 live heap 峰值。
// ---------------------------------------------------------------------------

struct CountingAlloc;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

#[inline]
fn note_alloc(size: usize) {
    let now = LIVE.fetch_add(size as isize, Ordering::Relaxed) + size as isize;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

#[inline]
fn note_free(size: usize) {
    LIVE.fetch_sub(size as isize, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            note_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            note_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note_free(layout.size());
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            note_free(layout.size());
            note_alloc(new_size);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

// ---------------------------------------------------------------------------
// 确定性 RNG（xorshift64*；不引入 rand）
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    #[inline]
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    #[inline]
    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }
}

/// 覆盖 CSI / ESC / OSC / DCS / APC / 私有模式 / UTF-8 / C0 的字节表。
const ALPHABET: &[u8] = b"\x1b[];:?<>=!$'\"#()*+hlmHJKABCDEFGILMPSXTZ@Prstqu0w\x07\x08\t\n\r\x00\x01\x02\x7f\x9b\x9c\x9d\x90\x9e\x9f\xff\xc3\xa9\xe4\xb8\xad0123456789";

/// 真实程序/测试套件里出现过的序列，fuzz 在它们上面做字节级变异。
const CORPUS: &[&[u8]] = &[
    b"\x1b[31mred\x1b[0m",
    b"\x1b[1;4;7;38;5;196mX\x1b[38;2;1;2;3mY\x1b[0m",
    b"\x1b[2J\x1b[H",
    b"\x1b[?1049hALT\x1b[?1049l",
    b"\x1b[?47h\x1b[?47l",
    b"\x1b[?25l\x1b[?25h",
    b"\x1b[?1000h\x1b[?1006h\x1b[?1002h",
    b"\x1b[?2004h\x1b[?1004h",
    b"\x1b[?7l0123456789ABCDEF\x1b[?7h",
    b"\x1b[3;6r\x1b[?6h\x1b[1;1HX\x1b[?6l",
    b"\x1b[3g\x1bH\x1b[10G\x1bH\x1b[1G\tX",
    b"\x1b[2;3Habc\x1b[K\x1b[1;1H\x1b[2P\x1b[3X",
    b"\x1b[5L\x1b[3M\x1b[2S\x1b[1T",
    b"\x1b#8",
    b"\x1b(0lqk\x1b(B",
    b"\x1b)0\x0eqx\x0fk",
    b"\x1b]0;title\x07",
    b"\x1b]2;title\x1b\\",
    b"\x1b]52;c;YWJj\x07",
    b"\x1b]4;1;rgb:ff/00/00\x07",
    b"\x1b]10;?\x07\x1b]11;?\x07",
    b"\x1bP1;2|payload\x1b\\",
    b"\x1b_apc\x1b\\\x1b^pm\x1b\\",
    b"\x1b[?2026hSYNC\x1b[?2026l",
    b"\x1b[!p",
    b"\x1b[?5h\x1b[?5l",
    b"\x1b[999;999H",
    b"\x1b[999999b\x1b[999999@\x1b[999999P",
    b"\x1b[200~paste\x1b[201~",
    b"\xc3\xa9\xe4\xb8\xad\xf0\x9f\x98\x80",
    "e\u{301}\u{200d}👨‍👩‍👧".as_bytes(),
    b"\x00\x01\x02\x03\x7f\xff",
    b"\x1b[\x1b[\x1b[",
    b"\x1b]0;\x1b\\",
];

#[derive(Default)]
struct Stats {
    bytes: usize,
    checksum: u64,
    max_history: usize,
    peak_live: isize,
    frames: usize,
}

fn frame_checksum(frame: &xtervo_core::Frame) -> u64 {
    // FNV-1a：帧内容（字符+颜色+标志）一个字节一个字节地折进去。
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for cell in &frame.cells {
        for b in cell.ch.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
        for b in cell
            .fg
            .to_le_bytes()
            .iter()
            .chain(cell.bg.to_le_bytes().iter())
        {
            h = (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
        }
        for b in cell.flags.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

fn gen_long(rng: &mut Rng, buf: &mut Vec<u8>) {
    match rng.below(4) {
        // 超长 OSC（超过 core 的 16 KiB 上限），带/不带终止符各半。
        0 => {
            buf.extend_from_slice(b"\x1b]0;");
            buf.resize(buf.len() + 64 * 1024, b'A');
            if rng.below(2) == 0 {
                buf.push(0x07);
            }
        }
        // 超长 CSI 参数表。
        1 => {
            buf.extend_from_slice(b"\x1b[");
            for _ in 0..4096 {
                buf.push(b';');
            }
            buf.push(b'm');
        }
        // 超长 DCS。
        2 => {
            buf.extend_from_slice(b"\x1bP1;2|");
            buf.resize(buf.len() + 32 * 1024, b'D');
            buf.extend_from_slice(b"\x1b\\");
        }
        // 大量换行，专门压回滚历史上限。
        _ => {
            buf.resize(16 * 1024, b'\n');
        }
    }
}

fn gen_chunk(rng: &mut Rng, buf: &mut Vec<u8>) {
    buf.clear();
    match rng.below(10) {
        0..=5 => {
            // 拼接 1–3 段语料，再做字节翻转/截断。
            let parts = 1 + rng.below(3);
            for _ in 0..parts {
                let piece = CORPUS[rng.below(CORPUS.len())];
                buf.extend_from_slice(piece);
            }
            if !buf.is_empty() {
                match rng.below(4) {
                    0 => {
                        let pos = rng.below(buf.len());
                        buf[pos] ^= 1 << rng.below(8);
                    }
                    1 => {
                        let pos = rng.below(buf.len());
                        buf.truncate(pos);
                    }
                    2 => {
                        let pos = rng.below(buf.len() + 1);
                        buf.insert(pos, ALPHABET[rng.below(ALPHABET.len())]);
                    }
                    _ => {}
                }
            }
        }
        6..=8 => {
            // 原始字节汤。
            let len = 1 + rng.below(192);
            for _ in 0..len {
                buf.push(ALPHABET[rng.below(ALPHABET.len())]);
            }
        }
        _ => gen_long(rng, buf),
    }
}

fn feed_split(sim: &mut TermSim, bytes: &[u8], rng: &mut Rng) {
    let mut off = 0;
    while off < bytes.len() {
        let remaining = bytes.len() - off;
        let take = 1 + rng.below(remaining.min(7));
        sim.advance(&bytes[off..off + take]);
        off += take;
    }
}

fn check_invariants(sim: &mut TermSim, scrollback: usize, stats: &mut Stats, seed: u64, tag: &str) {
    let rows = sim.rows();
    let cols = sim.cols();
    assert!(rows >= 1 && cols >= 1, "seed={seed} {tag}: 网格尺寸为 0");
    let checksum = {
        let frame = sim.pull();
        assert_eq!(
            frame.rows, rows,
            "seed={seed} {tag}: Frame 行数与终端不一致"
        );
        assert_eq!(
            frame.cols, cols,
            "seed={seed} {tag}: Frame 列数与终端不一致"
        );
        assert_eq!(
            frame.cells.len(),
            rows * cols,
            "seed={seed} {tag}: Frame 单元格数不是 rows*cols"
        );
        assert!(
            usize::from(frame.cursor.line) < rows,
            "seed={seed} {tag}: 光标行越界 {} >= {rows}",
            frame.cursor.line
        );
        assert!(
            usize::from(frame.cursor.col) < cols,
            "seed={seed} {tag}: 光标列越界 {} >= {cols}",
            frame.cursor.col
        );
        assert_eq!(
            frame.display_offset, 0,
            "seed={seed} {tag}: fuzz 不应把视口留在历史里"
        );
        frame_checksum(frame)
    };
    let history = sim.history_lines();
    assert!(
        history <= scrollback,
        "seed={seed} {tag}: 回滚历史 {history} 超过配置上限 {scrollback}"
    );
    stats.max_history = stats.max_history.max(history);
    stats.checksum ^= checksum;
    stats.frames += 1;
}

fn run_seed(
    seed: u64,
    iters: usize,
    scrollback: usize,
    live_before: isize,
    _peak_before: isize,
) -> Stats {
    let mut rng = Rng::new(seed);
    let mut sim = TermSim::new(80, 24, scrollback);
    let mut stats = Stats::default();
    let mut chunk = Vec::new();

    for i in 0..iters {
        gen_chunk(&mut rng, &mut chunk);
        stats.bytes += chunk.len();
        feed_split(&mut sim, &chunk, &mut rng);

        // 随机 resize（含极小网格），覆盖 resize 与 parser 的交互。
        if i % 97 == 96 {
            let cols = 2 + rng.below(119);
            let rows = 1 + rng.below(39);
            sim.resize(cols, rows);
        }

        if i % 16 == 15 {
            check_invariants(&mut sim, scrollback, &mut stats, seed, &format!("iter={i}"));
            let live = LIVE.load(Ordering::Relaxed) - live_before;
            stats.peak_live = stats.peak_live.max(live);
            assert!(
                live < 32 * 1024 * 1024,
                "seed={seed} iter={i}: live heap 增长 {live} 字节，超过 32 MiB"
            );
        }
    }

    // 末尾单独拉一帧，确保最后一块不完整序列没有把解析器卡死。
    check_invariants(&mut sim, scrollback, &mut stats, seed, "final");
    let history = sim.history_lines();
    assert!(
        history <= scrollback,
        "seed={seed}: 结束时历史 {history} > {scrollback}"
    );
    stats.max_history = stats.max_history.max(history);

    drop(sim);
    let leaked = LIVE.load(Ordering::Relaxed) - live_before;
    assert!(
        leaked < 2 * 1024 * 1024,
        "seed={seed}: 释放 TermSim 后仍残留 {leaked} 字节 live heap"
    );
    stats.peak_live = stats.peak_live.max(leaked);
    stats
}

fn main() {
    let mut seed = 1u64;
    let mut iters = 600usize;
    let mut seeds = 3u64;
    let mut scrollback = 500usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => seed = args.next().and_then(|v| v.parse().ok()).unwrap_or(seed),
            "--iters" => iters = args.next().and_then(|v| v.parse().ok()).unwrap_or(iters),
            "--seeds" => seeds = args.next().and_then(|v| v.parse().ok()).unwrap_or(seeds),
            "--scrollback" => {
                scrollback = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(scrollback)
            }
            "--long" => {
                iters = 20_000;
                seeds = 8;
            }
            "--help" | "-h" => {
                println!(
                    "用法: vt_fuzz_target [--seed N] [--seeds N] [--iters N] [--scrollback N] [--long]"
                );
                return;
            }
            _ => {}
        }
    }
    assert!(iters > 0 && seeds > 0, "iters/seeds 必须大于 0");

    let live_before = LIVE.load(Ordering::Relaxed);
    let peak_before = PEAK.load(Ordering::Relaxed);
    let started = Instant::now();
    let mut total = Stats::default();

    for i in 0..seeds {
        let s = seed.wrapping_add(i);
        let stats = run_seed(s, iters, scrollback, live_before, peak_before);
        println!(
            "seed={s:<4} iters={iters:<7} bytes={:<9} checksum={:#018x} history<={:<4} live_peak={}B frames={}",
            stats.bytes, stats.checksum, stats.max_history, stats.peak_live, stats.frames
        );
        total.bytes += stats.bytes;
        total.checksum ^= stats.checksum.rotate_left(i as u32 % 64);
        total.max_history = total.max_history.max(stats.max_history);
        total.peak_live = total.peak_live.max(stats.peak_live);
        total.frames += stats.frames;
    }

    // 确定性：同一种子重复跑，字节数/校验和/历史峰值必须完全一致。
    let first = run_seed(seed, iters, scrollback, live_before, peak_before);
    let again = run_seed(seed, iters, scrollback, live_before, peak_before);
    assert_eq!(
        first.bytes, again.bytes,
        "fuzz 不确定：同种子输入字节数不同"
    );
    assert_eq!(
        first.checksum, again.checksum,
        "fuzz 不确定：同种子帧校验和不同"
    );
    assert_eq!(
        first.max_history, again.max_history,
        "fuzz 不确定：同种子历史峰值不同"
    );

    let elapsed = started.elapsed();
    println!(
        "VT fuzz PASS: {} 种子 × {} 轮，{:.1} MiB 输入，{} 帧，历史峰值 {}，live 峰值 {}B，用时 {:.2}s",
        seeds,
        iters,
        total.bytes as f64 / (1024.0 * 1024.0),
        total.frames,
        total.max_history,
        total.peak_live,
        elapsed.as_secs_f64()
    );
    assert!(
        elapsed < Duration::from_secs(120),
        "默认/单次 fuzz 超出时间盒"
    );
}
