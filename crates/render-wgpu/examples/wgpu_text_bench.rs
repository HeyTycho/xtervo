//! Headless performance/pixel probe for the WGPU glyph-atlas text renderer.
//!
//! It never creates a window or surface. One warmup pass rasterizes every
//! printable ASCII glyph at each used size/style; measured frames then consume
//! `TextBatch` cells with zero atlas uploads. A single final readback compares
//! the GPU target against the CPU reference compositor.
//!
//! ```text
//! CARGO_HOME=$PWD/.cargo CARGO_TARGET_DIR=$PWD/target \
//!   cargo run --release -p tervo-render-wgpu --example wgpu_text_bench
//! ```
//!
//! Environment:
//! - `TERVO_WGPU_TEXT_FRAMES` measured frames (default 240)
//! - `TERVO_WGPU_TEXT_OUT` artifact dir
//!   (default `<workspace>/target/wgpu-text-atlas/bench`)
//! - `TERVO_WGPU_FONT` font path override
//!
//! The adapter is printed. On a machine without a hardware GPU wgpu selects
//! llvmpipe/lavapipe; the CPU-frame numbers then describe command recording and
//! instance building, not hardware GPU throughput.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tervo_render_wgpu::{
    compare_rgba, diff_rgba, discover_font_path, rasterize_reference, read_target_rgba8,
    request_headless_gpu, write_ppm, AtlasConfig, FontMetrics, FrameStats, GpuTextRenderer,
    TextLayout, TextSection, TextTarget,
};
use tervo_scene::{PhysicalSize, TextAttributes, TextBatch, TextCell};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const COLS: u16 = 80;
const ROWS: u16 = 24;
const MAIN_PX: u16 = 16;
const BIG_PX: u16 = 24;
const SMALL_PX: u16 = 12;
const CLEAR: [u8; 4] = [12, 14, 20, 255];
const CLEAR_RGB: u32 = 0x000c_0e14;
const DEFAULT_FRAMES: usize = 240;

fn output_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("TERVO_WGPU_TEXT_OUT") {
        return PathBuf::from(value);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/wgpu-text-atlas/bench")
}

fn frames_to_run() -> usize {
    std::env::var("TERVO_WGPU_TEXT_FRAMES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(DEFAULT_FRAMES)
}

fn layout(metrics: FontMetrics, px: u16, x: f32, y: f32) -> TextLayout {
    TextLayout::from_metrics(metrics, x, y, px)
}

fn push_line(
    batch: &mut TextBatch,
    row: u16,
    text: &str,
    fg: u32,
    bg: u32,
    attributes: TextAttributes,
) {
    for (index, ch) in text.chars().take(COLS as usize).enumerate() {
        batch.push(TextCell {
            row,
            col: index as u16,
            text: ch.to_string(),
            foreground: fg,
            background: bg,
            attributes,
        });
    }
}

/// Every printable ASCII glyph in one style, so measured frames never touch
/// fontdue or upload a new atlas texel.
fn warmup_batch(attributes: TextAttributes) -> TextBatch {
    let mut batch = TextBatch::new(1);
    let chars: Vec<char> = (0x20u8..=0x7e).map(char::from).collect();
    for (row, chunk) in chars.chunks(COLS as usize).enumerate() {
        for (col, ch) in chunk.iter().enumerate() {
            batch.push(TextCell {
                row: row as u16,
                col: col as u16,
                text: ch.to_string(),
                foreground: 0x00ff_ffff,
                background: CLEAR_RGB,
                attributes,
            });
        }
    }
    batch
}

fn main_batch(frame: usize) -> TextBatch {
    let mut batch = TextBatch::new(frame as u64 + 1000);
    for row in 0..ROWS {
        let text = format!(
            "{frame:04} row {row:02} Tervo WGPU glyph atlas: \
             0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz !@#$%^&*()"
        );
        let fg = match row % 4 {
            0 => 0x00e5_e7eb,
            1 => 0x007e_e787,
            2 => 0x00ff_d33d,
            _ => 0x0067_e8f9,
        };
        let bg = if row % 3 == 0 { 0x0018_1b24 } else { CLEAR_RGB };
        let attributes = if row % 11 == 3 {
            TextAttributes {
                bold: true,
                ..TextAttributes::default()
            }
        } else if row % 13 == 5 {
            TextAttributes {
                italic: true,
                underline: true,
                ..TextAttributes::default()
            }
        } else {
            TextAttributes::default()
        };
        push_line(&mut batch, row, &text, fg, bg, attributes);
    }
    batch
}

fn big_batch(frame: usize) -> TextBatch {
    let mut batch = TextBatch::new(frame as u64 + 2000);
    push_line(
        &mut batch,
        0,
        &format!("BIG 24px FRAME {frame:04}"),
        0x00ff_abfc,
        CLEAR_RGB,
        TextAttributes::default(),
    );
    batch
}

fn small_batch(frame: usize) -> TextBatch {
    let mut batch = TextBatch::new(frame as u64 + 3000);
    for row in 0..3u16 {
        push_line(
            &mut batch,
            row,
            &format!("small 12px row {row} frame {frame:04} glyph atlas: 0123456789 <>[]{{}}"),
            0x00d1_d5db,
            CLEAR_RGB,
            TextAttributes::default(),
        );
    }
    batch
}

fn percentile(sorted: &[Duration], percent: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() - 1) as f64 * percent).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let frame_count = frames_to_run();
    let gpu = request_headless_gpu().map_err(|error| {
        std::io::Error::other(format!(
            "no headless wgpu adapter: {error}; the benchmark cannot fabricate numbers"
        ))
    })?;
    eprintln!(
        "adapter={} backend={:?} device_type={:?} driver={}",
        gpu.info.name, gpu.info.backend, gpu.info.device_type, gpu.info.driver
    );
    let font_path = discover_font_path()?;
    eprintln!("font={}", font_path.display());

    let mut renderer = GpuTextRenderer::with_font_path(
        gpu.device.clone(),
        gpu.queue.clone(),
        &font_path,
        AtlasConfig::default(),
    )?;
    let target = TextTarget::new(
        &gpu.device,
        PhysicalSize {
            width: WIDTH,
            height: HEIGHT,
        },
    )?;
    let main_layout = layout(renderer.metrics(MAIN_PX), MAIN_PX, 16.0, 12.0);
    let big_layout = layout(renderer.metrics(BIG_PX), BIG_PX, 16.0, 560.0);
    let small_layout = layout(renderer.metrics(SMALL_PX), SMALL_PX, 16.0, 630.0);

    // Warm every printable ASCII glyph at every size. Bold/italic faces are
    // only used at 16px by the measured scene, so warm those separately.
    for px in [MAIN_PX, BIG_PX, SMALL_PX] {
        let batch = warmup_batch(TextAttributes::default());
        let warm_layout = layout(renderer.metrics(px), px, 0.0, 0.0);
        let (stats, _) = renderer.render_with_geometry(
            &target,
            CLEAR,
            &[TextSection::new(&batch, warm_layout)],
        )?;
        eprintln!(
            "warmup px={px}: glyphs={} atlas_uploads={} uploaded_bytes={} cached={}",
            stats.glyph_quads,
            stats.atlas_uploads,
            stats.atlas_uploaded_bytes,
            stats.atlas_cached_glyphs
        );
    }
    for attributes in [
        TextAttributes {
            bold: true,
            ..TextAttributes::default()
        },
        TextAttributes {
            italic: true,
            ..TextAttributes::default()
        },
        TextAttributes {
            bold: true,
            italic: true,
            ..TextAttributes::default()
        },
    ] {
        let batch = warmup_batch(attributes);
        let warm_layout = layout(renderer.metrics(MAIN_PX), MAIN_PX, 0.0, 0.0);
        let stats = renderer.render(&target, CLEAR, &[TextSection::new(&batch, warm_layout)])?;
        eprintln!(
            "warmup style {:?}: glyphs={} atlas_uploads={} cached={}",
            (attributes.bold, attributes.italic),
            stats.glyph_quads,
            stats.atlas_uploads,
            stats.atlas_cached_glyphs
        );
    }
    let atlas_stats_after_warmup = renderer.atlas_stats();
    eprintln!("atlas after warmup: {atlas_stats_after_warmup:?}");

    let mut cpu_times = Vec::with_capacity(frame_count);
    let mut wall_times = Vec::with_capacity(frame_count);
    let mut total_renderer_cpu = Duration::ZERO;
    let mut total_wall = Duration::ZERO;
    let mut max_glyph_quads = 0usize;
    let mut max_solid_quads = 0usize;
    let mut total_vertex_bytes = 0u64;
    let mut total_atlas_uploads = 0u64;
    let mut draw_calls = 0u32;
    let mut last_stats: Option<FrameStats> = None;
    let poll_maintenance = std::env::var_os("TERVO_WGPU_TEXT_POLL").is_some();

    for frame in 0..frame_count {
        let started = Instant::now();
        let main = main_batch(frame);
        let big = big_batch(frame);
        let small = small_batch(frame);
        let sections = [
            TextSection::new(&main, main_layout),
            TextSection::new(&big, big_layout),
            TextSection::new(&small, small_layout),
        ];
        let stats = renderer.render(&target, CLEAR, &sections)?;
        let wall = started.elapsed();

        if stats.atlas_uploads != 0 {
            return Err(std::io::Error::other(format!(
                "measured frame {frame} uploaded {} new glyphs; warmup must cover all glyphs",
                stats.atlas_uploads
            ))
            .into());
        }
        if stats.draw_calls != 2 || stats.render_passes != 1 {
            return Err(std::io::Error::other(format!(
                "measured frame {frame} used {} draw calls / {} passes, expected 2 / 1",
                stats.draw_calls, stats.render_passes
            ))
            .into());
        }
        total_renderer_cpu += stats.cpu_time;
        total_wall += wall;
        cpu_times.push(stats.cpu_time);
        wall_times.push(wall);
        max_glyph_quads = max_glyph_quads.max(stats.glyph_quads);
        max_solid_quads = max_solid_quads.max(stats.solid_quads);
        total_vertex_bytes += stats.vertex_uploaded_bytes;
        total_atlas_uploads += stats.atlas_uploads;
        draw_calls += stats.draw_calls;
        last_stats = Some(stats);
        if poll_maintenance && frame % 32 == 0 {
            let _ = gpu.device.poll(wgpu::PollType::Poll);
        }
    }

    let mut sorted_cpu = cpu_times.clone();
    sorted_cpu.sort_unstable();
    let mut sorted_wall = wall_times.clone();
    sorted_wall.sort_unstable();

    let last_stats = last_stats.expect("at least one benchmark frame");
    let target_bytes = target.gpu_bytes();
    let atlas_bytes = renderer.atlas().gpu_bytes();
    let average_cpu = total_renderer_cpu / frame_count as u32;
    let average_wall = total_wall / frame_count as u32;
    let avg_vertex = total_vertex_bytes / frame_count as u64;

    println!(
        "BENCH adapter={:?} adapter_name={:?} frames={frame_count} size={WIDTH}x{HEIGHT}",
        gpu.info.device_type, gpu.info.name
    );
    println!(
        "BENCH cpu avg={average_cpu:?} p50={:?} p95={:?} max={:?}",
        percentile(&sorted_cpu, 0.50),
        percentile(&sorted_cpu, 0.95),
        percentile(&sorted_cpu, 1.0)
    );
    println!(
        "BENCH wall avg={average_wall:?} p50={:?} p95={:?} max={:?} (includes TextBatch construction)",
        percentile(&sorted_wall, 0.50),
        percentile(&sorted_wall, 0.95),
        percentile(&sorted_wall, 1.0)
    );
    println!(
        "BENCH draw_calls/frame={:.2} render_pass/frame=1 glyph_quads max={max_glyph_quads} \
         solid_quads max={max_solid_quads} atlas_uploads={total_atlas_uploads} \
         atlas_cached={}",
        draw_calls as f64 / frame_count as f64,
        last_stats.atlas_cached_glyphs
    );
    println!(
        "BENCH bytes atlas={atlas_bytes} target={target_bytes} total={} \
         vertex_upload/frame_avg={avg_vertex} full_screen_rgba={} \
         full_screen_upload_ratio={:.4}",
        atlas_bytes + target_bytes,
        target_bytes,
        avg_vertex as f64 / target_bytes as f64
    );
    println!(
        "BENCH warmup atlas: uploads={} uploaded_bytes={} rasterized={} style_fallbacks={}",
        atlas_stats_after_warmup.uploads,
        atlas_stats_after_warmup.uploaded_bytes,
        atlas_stats_after_warmup.rasterized,
        atlas_stats_after_warmup.style_fallbacks
    );

    // One readback (validation only) for pixel comparison and artifacts.
    let frame = frame_count;
    let main = main_batch(frame);
    let big = big_batch(frame);
    let small = small_batch(frame);
    let sections = [
        TextSection::new(&main, main_layout),
        TextSection::new(&big, big_layout),
        TextSection::new(&small, small_layout),
    ];
    let (stats, geometry) = renderer.render_with_geometry(&target, CLEAR, &sections)?;
    let gpu_pixels = read_target_rgba8(&gpu.device, &gpu.queue, &target)?;
    let reference = rasterize_reference(WIDTH, HEIGHT, CLEAR, &geometry, renderer.atlas());
    let diff = compare_rgba(WIDTH, HEIGHT, &reference.rgba, &gpu_pixels)?;
    let amplified = diff_rgba(WIDTH, HEIGHT, &reference.rgba, &gpu_pixels);
    let dir = output_dir();
    write_ppm(dir.join("final-gpu.ppm"), WIDTH, HEIGHT, &gpu_pixels)?;
    write_ppm(
        dir.join("final-cpu-reference.ppm"),
        WIDTH,
        HEIGHT,
        &reference.rgba,
    )?;
    write_ppm(dir.join("final-diff-x8.ppm"), WIDTH, HEIGHT, &amplified)?;
    println!(
        "DIFF mean={:.6} max={} pixels_over_1={} pixels_over_2={} glyph_pixels={}",
        diff.mean_abs, diff.max_abs, diff.pixels_over_1, diff.pixels_over_2, reference.glyph_pixels
    );
    println!(
        "ARTIFACTS dir={} final_glyph_quads={} final_solid_quads={} cpu={:?}",
        dir.display(),
        stats.glyph_quads,
        stats.solid_quads,
        stats.cpu_time
    );

    if diff.mean_abs > 0.25 || diff.max_abs > 2 || diff.pixels_over_2 != 0 {
        return Err(std::io::Error::other(format!(
            "GPU/CPU reference mismatch: mean={:.6} max={} over2={}",
            diff.mean_abs, diff.max_abs, diff.pixels_over_2
        ))
        .into());
    }
    if total_atlas_uploads != 0 {
        return Err(std::io::Error::other("measured frames uploaded atlas glyphs").into());
    }
    println!(
        "PASS wgpu_text_bench: {frame_count} frames, 2 draw calls/frame, zero per-frame atlas \
         uploads, no full-screen CPU upload, pixel match mean={:.6} max={}",
        diff.mean_abs, diff.max_abs
    );
    Ok(())
}
