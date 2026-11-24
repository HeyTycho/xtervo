// SPDX-License-Identifier: GPL-3.0-only
//! 可复现的渲染 damage 基准（默认忽略，手动运行）。
//!
//! 目的：P1-06「渲染 damage 失效」修复前后的同机对比。每个场景都走
//! `Renderer::draw_without_cursor` + `Renderer::image`（app 的真实每帧调用顺序），
//! 并在**同一进程**里对照两套负载，避免系统负载漂移污染跨进程对比：
//!
//! - `single_row_legacy_full` vs `single_row_damage`：单行输出/按键；
//! - `typing_burst_legacy_full` vs `typing_burst_damage`：连续按键（光标行轮换）；
//! - `full_200x50`：整屏 200x50（resize/主题/滚动历史等强制 full 的等价负载）；
//! - `scroll_legacy_full` vs `scroll_all_rows_damage`：普通屏幕滚动，alacritty
//!   给所有行 damage、`frame.full=false`。
//!
//! “legacy_full” 用 `frame.full=true` 走当前 full 路径，与旧实现「每次切换双
//! buffer 都整屏重绘」的栅格化工作量一致；同进程比值是可复现的 before/after。
//!
//! 运行（release，数字才有意义）：
//! ```bash
//! CARGO_TARGET_DIR=$PWD/.target-render \
//!   cargo test --release -p xtervo-app --bin xtervo render_damage_benchmark \
//!   -- --ignored --nocapture
//! ```
//! 输出为 `[bench] ...` 行，`docs/self-iteration.md` 记录同机结果。
//!
//! 计时中的 `image` 指 app 每帧把帧缓冲交给 Slint 的 refcount clone；Skia 的纹理
//! 上传发生在渲染线程，不在此基准内（Slint 1.18 没有持久 GPU texture API）。

use std::rc::Rc;
use std::time::Instant;

use xtervo_core::{flags, CellData, CursorShape, CursorState, Frame, LineDamage, Theme};

use crate::glyph::FontSet;
use crate::render::Renderer;

const COLS: usize = 200;
const ROWS: usize = 50;

/// 生成 200x50 的稳态画面：混合 ASCII、颜色、粗体/斜体与少量 CJK。
/// 先整屏画一次把字形缓存预热，基准里就不会把首次 raster 的字体解析算进去。
fn sample_frame(cols: usize, rows: usize, theme: Theme) -> Frame {
    let mut frame = Frame::new(cols, rows);
    let palette = [
        0x00cdd6f4, 0x00f38ba8, 0x00a6e3a1, 0x00f9e2af, 0x0089b4fa, 0x00cba6f7,
    ];
    let text = b"the quick brown fox jumps over 0123456789";
    for row in 0..rows {
        for col in 0..cols {
            let ch = text[(row * 7 + col) % text.len()] as char;
            let mut cell = CellData::default();
            cell.ch = ch as u32;
            cell.fg = palette[(row + col) % palette.len()];
            cell.bg = theme.bg();
            if row % 5 == 0 {
                cell.flags |= flags::BOLD;
            }
            if col % 11 == 0 {
                cell.flags |= flags::ITALIC;
            }
            frame.cells[row * cols + col] = cell;
        }
        // 行尾放一个宽字符，覆盖 WIDE_CHAR/SPACER 的局部重绘路径。
        if cols >= 4 {
            let mut wide = CellData::default();
            wide.ch = '你' as u32;
            wide.fg = palette[row % palette.len()];
            wide.bg = theme.bg();
            wide.flags = flags::WIDE_CHAR;
            frame.cells[row * cols + cols - 2] = wide;
            let mut spacer = CellData::default();
            spacer.bg = theme.bg();
            spacer.flags = flags::WIDE_CHAR_SPACER;
            frame.cells[row * cols + cols - 1] = spacer;
        }
    }
    frame.cursor = CursorState {
        line: 0,
        col: 0,
        shape: CursorShape::Block,
        visible: true,
    };
    frame
}

fn text_char(i: usize) -> char {
    let text = b"abcXYZ0123";
    text[i % text.len()] as char
}

/// 改动某一行的 3 个格子（模拟输出/按键）。
fn touch_row(frame: &mut Frame, i: usize) {
    let line = i % frame.rows;
    for k in 0..3 {
        let col = (i * 5 + k * 17) % frame.cols;
        frame.cells[line * frame.cols + col].ch = text_char(i + k) as u32;
    }
    frame.cursor.line = line as u16;
    frame.cursor.col = (i % frame.cols) as u16;
}

fn touch_full(frame: &mut Frame, i: usize) {
    for k in 0..3 {
        let col = (i * 11 + k * 29) % COLS;
        frame.cells[(i % ROWS) * COLS + col].ch = text_char(i + k) as u32;
    }
    frame.cursor.line = (i % ROWS) as u16;
    frame.cursor.col = (i * 7 % COLS) as u16;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 旧实现：双缓冲切换后整屏重绘（当前 full 路径的等价负载）。
    LegacyFull,
    /// 新实现：只按 frame.damage 重绘。
    Damage,
}

/// 单行输出/连续按键的帧准备。
fn prep_one_line(frame: &mut Frame, i: usize, mode: Mode) {
    touch_row(frame, i);
    frame.damage.clear();
    match mode {
        Mode::LegacyFull => frame.full = true,
        Mode::Damage => {
            frame.full = false;
            let line = i % frame.rows;
            frame.damage.push(LineDamage {
                line: line as u16,
                left: 0,
                right: frame.cols.saturating_sub(1) as u16,
            });
        },
    }
}

/// 整屏负载（resize/主题/滚动历史）：两种实现的等价负载都是全屏重绘。
fn prep_full(frame: &mut Frame, i: usize, _mode: Mode) {
    touch_full(frame, i);
    frame.full = true;
    frame.damage.clear();
}

/// 普通屏幕滚动：alacritty 给所有行 damage、frame.full=false。
fn prep_scroll(frame: &mut Frame, i: usize, mode: Mode) {
    touch_full(frame, i);
    frame.damage.clear();
    match mode {
        Mode::LegacyFull => frame.full = true,
        Mode::Damage => {
            frame.full = false;
            for line in 0..frame.rows {
                frame.damage.push(LineDamage {
                    line: line as u16,
                    left: 0,
                    right: frame.cols.saturating_sub(1) as u16,
                });
            }
        },
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn percentile_u32(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(((sorted.len() - 1) as f64 * p).round() as usize).min(sorted.len() - 1)]
}

struct Stats {
    draw: Vec<f64>,
    image: Vec<f64>,
    rows: Vec<u32>,
}

impl Stats {
    fn new(n: usize) -> Self {
        Self {
            draw: Vec::with_capacity(n),
            image: Vec::with_capacity(n),
            rows: Vec::with_capacity(n),
        }
    }

    /// 打印一行统计，返回 draw+image 的 p50（微秒），供同进程 A/B 汇总。
    fn report(&mut self, name: &str, full_redraws: u64, detached: usize) -> f64 {
        self.draw.sort_by(f64::total_cmp);
        self.image.sort_by(f64::total_cmp);
        self.rows.sort_unstable();
        let p50 = |v: &[f64]| percentile(v, 0.50);
        let p95 = |v: &[f64]| percentile(v, 0.95);
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        let total_p50 = p50(&self.draw) + p50(&self.image);
        eprintln!(
            "[bench] scenario={name} frames={} grid={COLS}x{ROWS} \
             draw_p50_us={:.2} draw_p95_us={:.2} draw_mean_us={:.2} \
             image_p50_us={:.2} image_p95_us={:.2} \
             total_p50_us={total_p50:.2} rows_p50={} rows_p95={} \
             full_redraws_delta={full_redraws} detach_frames={detached}",
            self.draw.len(),
            p50(&self.draw),
            p95(&self.draw),
            mean(&self.draw),
            p50(&self.image),
            p95(&self.image),
            percentile_u32(&self.rows, 0.50),
            percentile_u32(&self.rows, 0.95),
        );
        total_p50
    }
}

/// 场景切换前整屏重建两块缓冲，并替掉 app 侧“上一帧 image”的引用。
fn reset(frame: &mut Frame, renderer: &mut Renderer, front: &mut slint::Image) {
    frame.full = true;
    frame.damage.clear();
    renderer.draw_without_cursor(frame);
    *front = renderer.image();
}

/// 同进程跑一个子场景；每帧调用 `prep` 生成 full/damage 状态。
fn run_frames(
    renderer: &mut Renderer,
    frame: &mut Frame,
    front: &mut slint::Image,
    name: &str,
    frames: usize,
    mode: Mode,
    prep: fn(&mut Frame, usize, Mode),
) -> f64 {
    reset(frame, renderer, front);
    let full_before = renderer.full_redraws;
    let mut stats = Stats::new(frames);
    let mut detached = 0usize;
    for i in 0..frames {
        prep(frame, i, mode);
        let t0 = Instant::now();
        renderer.draw_without_cursor(frame);
        stats.draw.push(t0.elapsed().as_secs_f64() * 1e6);
        let t1 = Instant::now();
        *front = renderer.image();
        stats.image.push(t1.elapsed().as_secs_f64() * 1e6);
        stats.rows.push(renderer.last_draw_rows);
        detached += renderer.last_buffer_detached as usize;
    }
    stats.report(name, renderer.full_redraws - full_before, detached)
}

#[test]
#[ignore = "渲染 damage 基准：手动 cargo test --release ... --ignored --nocapture"]
fn render_damage_benchmark() {
    let Some(fonts) = FontSet::load(15.0) else {
        eprintln!("[bench] skipped=no_font");
        return;
    };
    let cell_w = fonts.cell_w;
    let cell_h = fonts.cell_h;
    let theme = Theme::default();
    let mut renderer = Renderer::new(
        Rc::new(fonts),
        COLS,
        ROWS,
        theme.bg(),
        theme.cursor(),
        1.0,
    );
    let mut frame = sample_frame(COLS, ROWS, theme);

    // 预热：整屏画一次，把所有字形 raster/缓存、分配都排除在计时外。
    frame.full = true;
    renderer.draw_without_cursor(&frame);
    let mut front = renderer.image();
    eprintln!(
        "[bench] setup=grid_{COLS}x{ROWS} cell={cell_w}x{cell_h} \
         buffer={}x{} bytes={}",
        renderer.width,
        renderer.height,
        renderer.width * renderer.height * 4
    );

    // 同进程 A/B：同一帧数据、同一字体缓存、同一系统负载。
    // 旧实现=每次切换缓冲整屏重绘；新实现=只按 damage 重绘。
    const N_SINGLE: usize = 600;
    let single_full = run_frames(
        &mut renderer,
        &mut frame,
        &mut front,
        "single_row_legacy_full",
        N_SINGLE,
        Mode::LegacyFull,
        prep_one_line,
    );
    let single_damage = run_frames(
        &mut renderer,
        &mut frame,
        &mut front,
        "single_row_damage",
        N_SINGLE,
        Mode::Damage,
        prep_one_line,
    );

    const N_TYPING: usize = 400;
    let typing_full = run_frames(
        &mut renderer,
        &mut frame,
        &mut front,
        "typing_burst_legacy_full",
        N_TYPING,
        Mode::LegacyFull,
        prep_one_line,
    );
    let typing_damage = run_frames(
        &mut renderer,
        &mut frame,
        &mut front,
        "typing_burst_damage",
        N_TYPING,
        Mode::Damage,
        prep_one_line,
    );

    // 整屏 200x50：resize/主题/滚动历史等强制 full 的等价负载。
    // 新旧实现走同一条 full 路径（旧实现切 buffer 后也是整屏重绘），
    // 这里测的是当前 full 路径的绝对成本与波动，不引入额外复制。
    const N_FULL: usize = 200;
    let full = run_frames(
        &mut renderer,
        &mut frame,
        &mut front,
        "full_200x50",
        N_FULL,
        Mode::LegacyFull,
        prep_full,
    );

    // 滚动：旧实现整屏；新实现收到全行 damage 时同样走 full 路径。
    const N_SCROLL: usize = 200;
    let scroll_full = run_frames(
        &mut renderer,
        &mut frame,
        &mut front,
        "scroll_legacy_full",
        N_SCROLL,
        Mode::LegacyFull,
        prep_scroll,
    );
    let scroll_damage = run_frames(
        &mut renderer,
        &mut frame,
        &mut front,
        "scroll_all_rows_damage",
        N_SCROLL,
        Mode::Damage,
        prep_scroll,
    );

    eprintln!(
        "[bench] summary single_row_speedup={:.2}x typing_speedup={:.2}x \
         scroll_speedup={:.2}x full_total_us={full:.2} \
         legacy_single_total_us={single_full:.2} damage_single_total_us={single_damage:.2} \
         legacy_typing_total_us={typing_full:.2} damage_typing_total_us={typing_damage:.2} \
         legacy_scroll_total_us={scroll_full:.2} damage_scroll_total_us={scroll_damage:.2}",
        single_full / single_damage.max(0.001),
        typing_full / typing_damage.max(0.001),
        scroll_full / scroll_damage.max(0.001),
    );
}
