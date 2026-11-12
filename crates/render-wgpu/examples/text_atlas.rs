//! Minimal Slint presentation of the WGPU glyph-atlas target.
//!
//! The Slint winit/WGPU30 `RenderingSetup` callback provides the same
//! `wgpu::Device`/`wgpu::Queue` Slint renders with. The offscreen
//! `Rgba8Unorm` text target is imported with `slint::Image::try_from` (through
//! [`SlintTexturePresenter`]) and displayed live. Each scene is also read back
//! once for validation and compared with `Window::take_snapshot()`; that CPU
//! readback is test-only and is not part of the render path.
//!
//! Run on an isolated Xvfb display (never the user's `:1`):
//!
//! ```text
//! Xvfb :79 -screen 0 1400x800x24 -nolisten tcp &
//! DISPLAY=:79 SLINT_WGPU_CPU=1 env -u SLINT_BACKEND \
//!   cargo run -p tervo-render-wgpu --features slint-presenter --example text_atlas
//! ```
//!
//! Artifacts are written as PPM to
//! `$TERVO_WGPU_TEXT_OUT` (default `<workspace>/target/wgpu-text-atlas/slint`).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tervo_render_wgpu::{
    compare_rgba, discover_font_path, read_target_rgba8, write_ppm, AtlasConfig, FontMetrics,
    GpuTextRenderer, SlintTexturePresenter, TextLayout, TextSection, TextTarget,
};
use tervo_scene::{PhysicalSize, TextAttributes, TextBatch, TextCell};

slint::slint! {
    export component App inherits Window {
        title: "tervo-wgpu-text-atlas";
        width: 1280px;
        height: 720px;
        in-out property <image> frame;
        // Magenta fallback: if the imported texture is not sampled, snapshots
        // become obviously wrong instead of silently black.
        Rectangle {
            background: #ff00ff;
            Image {
                source: root.frame;
                width: 100%;
                height: 100%;
            }
        }
    }
}

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const CLEAR: [u8; 4] = [12, 14, 20, 255];
const CLEAR_RGB: u32 = 0x000c_0e14;
const TIMER_INTERVAL: Duration = Duration::from_millis(250);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const SNAPSHOT_MEAN_TOLERANCE: f64 = 12.0;
const SNAPSHOT_MAX_TOLERANCE: u8 = 96;

struct PresentState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: GpuTextRenderer,
    target: TextTarget,
    presenter: SlintTexturePresenter,
}

struct BuiltScene {
    label: String,
    batches: Vec<TextBatch>,
    layouts: Vec<TextLayout>,
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
    for (index, ch) in text.chars().enumerate() {
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

fn build_scene(index: usize, renderer: &GpuTextRenderer) -> BuiltScene {
    match index {
        0 => {
            let mut title = TextBatch::new(1);
            push_line(
                &mut title,
                0,
                "TERVO - WGPU GLYPH ATLAS",
                0x00ff_ffff,
                CLEAR_RGB,
                TextAttributes::default(),
            );
            push_line(
                &mut title,
                1,
                "no CPU terminal framebuffer",
                0x007e_e787,
                CLEAR_RGB,
                TextAttributes::default(),
            );
            let mut body = TextBatch::new(2);
            for (row, line) in [
                "TextBatch -> ASCII glyphs -> R8 atlas",
                "instanced quads -> one render pass",
                "Rgba8Unorm target -> slint::Image",
                "same wgpu::Device and Queue as Slint",
            ]
            .iter()
            .enumerate()
            {
                push_line(
                    &mut body,
                    row as u16,
                    line,
                    0x00d1_d5db,
                    CLEAR_RGB,
                    TextAttributes::default(),
                );
            }
            let mut size12 = TextBatch::new(3);
            push_line(
                &mut size12,
                0,
                "12px: 0123456789 !@#$%^&*()",
                0x00d1_d5db,
                CLEAR_RGB,
                TextAttributes::default(),
            );
            let mut size24 = TextBatch::new(4);
            push_line(
                &mut size24,
                0,
                "24px: multi-size glyphs",
                0x0067_e8f9,
                CLEAR_RGB,
                TextAttributes::default(),
            );
            let mut size32 = TextBatch::new(5);
            push_line(
                &mut size32,
                0,
                "32px: ATLAS",
                0x00ff_d33d,
                CLEAR_RGB,
                TextAttributes::default(),
            );
            BuiltScene {
                label: "overview".into(),
                batches: vec![title, body, size12, size24, size32],
                layouts: vec![
                    layout(renderer.metrics(26), 26, 32.0, 32.0),
                    layout(renderer.metrics(18), 18, 32.0, 150.0),
                    layout(renderer.metrics(12), 12, 32.0, 285.0),
                    layout(renderer.metrics(24), 24, 32.0, 315.0),
                    layout(renderer.metrics(32), 32, 32.0, 355.0),
                ],
            }
        }
        1..=3 => {
            let offset = index - 1;
            let mut batch = TextBatch::new(index as u64 + 10);
            for row in 0..24u16 {
                let source_row = row + offset as u16;
                let text = format!(
                    "{source_row:02} | Tervo WGPU atlas scroll test | row {source_row:02} | \
                     0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz"
                );
                let fg = if source_row % 7 == 0 {
                    0x00ff_d33d
                } else if source_row % 5 == 0 {
                    0x007e_e787
                } else {
                    0x00e5_e7eb
                };
                let bg = if source_row % 9 == 0 {
                    0x0018_1b24
                } else {
                    CLEAR_RGB
                };
                push_line(&mut batch, row, &text, fg, bg, TextAttributes::default());
            }
            BuiltScene {
                label: format!("scroll-{offset}"),
                batches: vec![batch],
                layouts: vec![layout(renderer.metrics(16), 16, 24.0, 20.0)],
            }
        }
        _ => {
            let mut batch = TextBatch::new(4);
            let lines: [(&str, u32, TextAttributes); 6] = [
                (
                    "regular: the quick brown fox",
                    0x00e5_e7eb,
                    TextAttributes::default(),
                ),
                (
                    "bold: the quick brown fox",
                    0x007e_e787,
                    TextAttributes {
                        bold: true,
                        ..TextAttributes::default()
                    },
                ),
                (
                    "italic: the quick brown fox",
                    0x0067_e8f9,
                    TextAttributes {
                        italic: true,
                        ..TextAttributes::default()
                    },
                ),
                (
                    "bold italic: the quick brown fox",
                    0x00ff_abfc,
                    TextAttributes {
                        bold: true,
                        italic: true,
                        ..TextAttributes::default()
                    },
                ),
                (
                    "underline: selected colors below",
                    0x00ff_d33d,
                    TextAttributes {
                        underline: true,
                        ..TextAttributes::default()
                    },
                ),
                (
                    "selected cell foreground/background",
                    0x00ff_ffff,
                    TextAttributes {
                        selected: true,
                        ..TextAttributes::default()
                    },
                ),
            ];
            for (row, (text, fg, attrs)) in lines.iter().enumerate() {
                let bg = if attrs.selected {
                    0x003b_2f55
                } else {
                    CLEAR_RGB
                };
                push_line(&mut batch, row as u16, text, *fg, bg, *attrs);
            }
            BuiltScene {
                label: "styles".into(),
                batches: vec![batch],
                layouts: vec![layout(renderer.metrics(24), 24, 32.0, 48.0)],
            }
        }
    }
}

fn output_dir() -> PathBuf {
    std::env::var_os("TERVO_WGPU_TEXT_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("target/wgpu-text-atlas/slint")
        })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    slint::BackendSelector::new()
        .require_wgpu_30(slint::wgpu_30::WGPUConfiguration::default())
        .select()?;

    let ui = App::new()?;
    ui.window()
        .set_size(slint::PhysicalSize::new(WIDTH, HEIGHT));

    let state: Rc<RefCell<Option<PresentState>>> = Rc::new(RefCell::new(None));
    let tick = Rc::new(Cell::new(0usize));
    let outcome: Rc<RefCell<Result<(), String>>> = Rc::new(RefCell::new(Ok(())));
    let completed = Rc::new(Cell::new(false));

    let setup_weak = ui.as_weak();
    let setup_state = Rc::clone(&state);
    ui.window()
        .set_rendering_notifier(move |rendering_state, api| match rendering_state {
            slint::RenderingState::RenderingSetup => {
                let slint::GraphicsAPI::WGPU30 { device, queue, .. } = api else {
                    eprintln!(
                        "text_atlas: expected WGPU30; set SLINT_WGPU_CPU=1 and \
                         unset SLINT_BACKEND"
                    );
                    return;
                };
                let info = device.adapter_info();
                eprintln!(
                    "text_atlas: Slint WGPU30 adapter={:?} backend={:?} device_type={:?}",
                    info.name, info.backend, info.device_type
                );
                let font = match discover_font_path() {
                    Ok(path) => path,
                    Err(error) => {
                        eprintln!("text_atlas: no font: {error}");
                        return;
                    }
                };
                eprintln!("text_atlas: font={}", font.display());
                let renderer = match GpuTextRenderer::with_font_path(
                    device.clone(),
                    queue.clone(),
                    &font,
                    AtlasConfig::default(),
                ) {
                    Ok(renderer) => renderer,
                    Err(error) => {
                        eprintln!("text_atlas: atlas creation failed: {error}");
                        return;
                    }
                };
                let target = match TextTarget::new(
                    device,
                    PhysicalSize {
                        width: WIDTH,
                        height: HEIGHT,
                    },
                ) {
                    Ok(target) => target,
                    Err(error) => {
                        eprintln!("text_atlas: target creation failed: {error}");
                        return;
                    }
                };
                let image_weak = setup_weak.clone();
                let presenter =
                    match SlintTexturePresenter::new(target.texture().clone(), move |image| {
                        if let Some(ui) = image_weak.upgrade() {
                            ui.set_frame(image);
                        }
                    }) {
                        Ok(presenter) => presenter,
                        Err(error) => {
                            eprintln!("text_atlas: slint::Image import failed: {error}");
                            return;
                        }
                    };
                eprintln!(
                    "text_atlas: imported {}x{} {:?} texture on Slint's device",
                    presenter.size().width,
                    presenter.size().height,
                    target.texture().format(),
                );
                *setup_state.borrow_mut() = Some(PresentState {
                    device: device.clone(),
                    queue: queue.clone(),
                    renderer,
                    target,
                    presenter,
                });
            }
            slint::RenderingState::RenderingTeardown => {
                *setup_state.borrow_mut() = None;
            }
            _ => {}
        })?;

    let timer = slint::Timer::default();
    let started = Instant::now();
    let timer_weak = ui.as_weak();
    let timer_state = Rc::clone(&state);
    let timer_outcome = Rc::clone(&outcome);
    let timer_completed = Rc::clone(&completed);
    timer.start(slint::TimerMode::Repeated, TIMER_INTERVAL, move || {
        let Some(ui) = timer_weak.upgrade() else {
            return;
        };
        if started.elapsed() > STARTUP_TIMEOUT {
            *timer_outcome.borrow_mut() = Err(format!(
                "timed out after {STARTUP_TIMEOUT:?} waiting for WGPU RenderingSetup"
            ));
            slint::quit_event_loop().ok();
            return;
        }
        let mut state = timer_state.borrow_mut();
        let Some(state) = state.as_mut() else {
            return;
        };
        let scene_index = tick.get();
        if scene_index >= 5 {
            return;
        }
        let built = build_scene(scene_index, &state.renderer);
        let sections: Vec<TextSection<'_>> = built
            .batches
            .iter()
            .zip(&built.layouts)
            .map(|(batch, layout)| TextSection::new(batch, *layout))
            .collect();
        let stats = match state.renderer.render(&state.target, CLEAR, &sections) {
            Ok(stats) => stats,
            Err(error) => {
                *timer_outcome.borrow_mut() = Err(format!("render failed: {error}"));
                slint::quit_event_loop().ok();
                return;
            }
        };
        state.presenter.present(ui.window());

        // Validation only: read back the offscreen target and compare it to
        // Slint's own snapshot of the window.
        let target_pixels = match read_target_rgba8(&state.device, &state.queue, &state.target) {
            Ok(pixels) => pixels,
            Err(error) => {
                *timer_outcome.borrow_mut() = Err(format!("target readback failed: {error}"));
                slint::quit_event_loop().ok();
                return;
            }
        };
        let snapshot = match ui.window().take_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                *timer_outcome.borrow_mut() = Err(format!("take_snapshot failed: {error}"));
                slint::quit_event_loop().ok();
                return;
            }
        };
        if snapshot.width() != WIDTH || snapshot.height() != HEIGHT {
            *timer_outcome.borrow_mut() = Err(format!(
                "snapshot is {}x{}, expected {WIDTH}x{HEIGHT}",
                snapshot.width(),
                snapshot.height()
            ));
            slint::quit_event_loop().ok();
            return;
        }

        let fallback_pixels = snapshot
            .as_bytes()
            .chunks_exact(4)
            .filter(|pixel| pixel[0] > 200 && pixel[2] > 200 && pixel[1] < 80)
            .count();
        if fallback_pixels > (WIDTH * HEIGHT) as usize / 10 {
            *timer_outcome.borrow_mut() = Err(format!(
                "snapshot still shows {fallback_pixels} magenta fallback pixels; \
                 imported texture was not sampled"
            ));
            slint::quit_event_loop().ok();
            return;
        }

        let diff = match compare_rgba(WIDTH, HEIGHT, &target_pixels, snapshot.as_bytes()) {
            Ok(diff) => diff,
            Err(error) => {
                *timer_outcome.borrow_mut() = Err(error);
                slint::quit_event_loop().ok();
                return;
            }
        };
        eprintln!(
            "scene={} glyphs={} solids={} draw_calls={} cpu={:?} \
             snapshot_vs_target(mean={:.4} max={} pixels={}) fallback_pixels={}",
            built.label,
            stats.glyph_quads,
            stats.solid_quads,
            stats.draw_calls,
            stats.cpu_time,
            diff.mean_abs,
            diff.max_abs,
            diff.differing_pixels,
            fallback_pixels,
        );
        let dir = output_dir();
        let target_path = dir.join(format!("{}-target.ppm", built.label));
        let snapshot_path = dir.join(format!("{}-snapshot.ppm", built.label));
        if let Err(error) = write_ppm(&target_path, WIDTH, HEIGHT, &target_pixels)
            .and_then(|_| write_ppm(&snapshot_path, WIDTH, HEIGHT, snapshot.as_bytes()))
        {
            *timer_outcome.borrow_mut() = Err(format!("failed to write artifacts: {error}"));
            slint::quit_event_loop().ok();
            return;
        }
        if diff.mean_abs > SNAPSHOT_MEAN_TOLERANCE || diff.max_abs > SNAPSHOT_MAX_TOLERANCE {
            *timer_outcome.borrow_mut() = Err(format!(
                "snapshot/target mismatch: mean={:.4} max={} (target {} snapshot {})",
                diff.mean_abs,
                diff.max_abs,
                target_path.display(),
                snapshot_path.display()
            ));
            slint::quit_event_loop().ok();
            return;
        }
        eprintln!(
            "wrote {} and {}",
            target_path.display(),
            snapshot_path.display()
        );
        let next = scene_index + 1;
        tick.set(next);
        if next >= 5 {
            timer_completed.set(true);
            slint::quit_event_loop().ok();
        }
    });

    ui.run()?;

    if completed.get() {
        println!(
            "PASS text_atlas: 5 scenes presented through slint::Image on Slint's WGPU30 \
             device; Window::take_snapshot matches the offscreen target"
        );
        Ok(())
    } else {
        let message = outcome
            .borrow()
            .as_ref()
            .err()
            .cloned()
            .unwrap_or_else(|| "event loop exited before all scenes were verified".into());
        Err(std::io::Error::other(message).into())
    }
}
