//! Long-run memory probe for the glyph-atlas renderer (headless, Linux).
//!
//! This deliberately reuses one `TextBatch` or rebuilds it per frame to
//! distinguish renderer growth from harness allocation. It prints the process
//! RSS/HWM at regular intervals; it does not read the framebuffer back.
//!
//! ```text
//! # static batch (no per-frame TextBatch allocation)
//! MODE=static FRAMES=60000 \
//!   cargo run --release -p tervo-render-wgpu --example memory_probe
//!
//! # dynamic batch (rebuild cells every frame)
//! MODE=dynamic FRAMES=60000 \
//!   cargo run --release -p tervo-render-wgpu --example memory_probe
//! ```
//!
//! On the llvmpipe adapter both modes plateau after the first few seconds
//! (~189 MB RSS, 114 driver threads, 4 FDs) with no atlas/buffer growth. The
//! benchmark's 130k-frame RSS slope is its own `Vec<Duration>` sample storage,
//! not renderer retention; this probe separates the two.

use std::time::Instant;

use tervo_render_wgpu::{
    discover_font_path, request_headless_gpu, AtlasConfig, GpuTextRenderer, TextLayout,
    TextSection, TextTarget,
};
use tervo_scene::{PhysicalSize, TextAttributes, TextBatch, TextCell};

const FRAMES_DEFAULT: usize = 30_000;

fn rss_kb() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut rss = 0;
    let mut hwm = 0;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            rss = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        }
        if let Some(value) = line.strip_prefix("VmHWM:") {
            hwm = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        }
    }
    (rss, hwm)
}

fn push_line(batch: &mut TextBatch, row: u16, text: &str) {
    for (col, ch) in text.chars().take(80).enumerate() {
        batch.push(TextCell {
            row,
            col: col as u16,
            text: ch.to_string(),
            foreground: 0x00e5e7eb,
            background: 0x000c0e14,
            attributes: if row % 7 == 3 {
                TextAttributes {
                    bold: true,
                    ..TextAttributes::default()
                }
            } else {
                TextAttributes::default()
            },
        });
    }
}

fn build_batch(frame: usize) -> TextBatch {
    let mut batch = TextBatch::new(frame as u64);
    for row in 0..24u16 {
        let text = format!(
            "{frame:04} row {row:02} Tervo WGPU glyph atlas 0123456789 \
             ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz"
        );
        push_line(&mut batch, row, &text);
    }
    batch
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::var("MODE").unwrap_or_else(|_| "static".into());
    let frames = std::env::var("FRAMES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(FRAMES_DEFAULT);
    if mode != "static" && mode != "dynamic" {
        return Err(std::io::Error::other("MODE must be static or dynamic").into());
    }

    let gpu = request_headless_gpu()?;
    eprintln!(
        "memory_probe adapter={} backend={:?} device_type={:?} mode={mode} frames={frames}",
        gpu.info.name, gpu.info.backend, gpu.info.device_type
    );
    let font = discover_font_path()?;
    let mut renderer = GpuTextRenderer::with_font_path(
        gpu.device.clone(),
        gpu.queue.clone(),
        font,
        AtlasConfig::default(),
    )?;
    let target = TextTarget::new(
        &gpu.device,
        PhysicalSize {
            width: 1280,
            height: 720,
        },
    )?;
    let layout = TextLayout::from_metrics(renderer.metrics(16), 16.0, 12.0, 16);
    let static_batch = build_batch(0);

    let started = Instant::now();
    for frame in 0..frames {
        let owned;
        let batch = if mode == "static" {
            &static_batch
        } else {
            owned = build_batch(frame);
            &owned
        };
        renderer.render(
            &target,
            [12, 14, 20, 255],
            &[TextSection::new(batch, layout)],
        )?;
        if frame % 5000 == 0 {
            let (rss, hwm) = rss_kb();
            eprintln!(
                "mode={mode} frame={frame} elapsed={:?} rss_kb={rss} hwm_kb={hwm}",
                started.elapsed()
            );
        }
    }
    let (rss, hwm) = rss_kb();
    eprintln!(
        "mode={mode} done frames={frames} elapsed={:?} rss_kb={rss} hwm_kb={hwm}",
        started.elapsed()
    );
    Ok(())
}
