//! Headless validation for the WGPU backend + Slint texture presenter.
//!
//! The example creates a Slint window rendered by `femtovg-wgpu`, asks the
//! `RenderingSetup` notifier for Slint's device/queue, creates the standalone
//! offscreen renderer on that device, imports its `Rgba8Unorm` texture with
//! `slint::Image::try_from`, and then continuously clears the texture to red,
//! blue and red again. After each color has been presented it takes a
//! `Window::take_snapshot()` and verifies the center/quarter pixels.
//!
//! This deliberately does **not** use `xwd`: with a GPU window under Xvfb the X
//! server does not own the rendered pixels and `xwd` returns an empty/black
//! image.
//!
//! Run it on a headless X server (the test harness does not need a real GPU):
//!
//! ```text
//! Xvfb :99 -screen 0 1280x720x24 &
//! DISPLAY=:99 SLINT_WGPU_CPU=1 \
//!   cargo run -p tervo-render-wgpu --features slint-presenter --example two_colors
//! ```
//!
//! Snapshots are written as binary PPM files to
//! `$TERVO_WGPU_SNAPSHOT_DIR` (default: `target/render-wgpu-snapshots`), so the
//! example never depends on a user-specific absolute path.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tervo_render::Renderer as _;
use tervo_render_wgpu::{SlintTexturePresenter, WgpuRenderer};
use tervo_scene::{DamageRegion, PhysicalSize, Scene};

slint::slint! {
    export component App inherits Window {
        title: "tervo-render-wgpu-two-colors";
        width: 320px;
        height: 200px;
        in-out property <image> frame;
        // A recognizable color in case the imported image is not sampled.
        Rectangle {
            background: #404040;
            Image {
                source: root.frame;
                width: 100%;
                height: 100%;
            }
        }
    }
}

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const FALLBACK_WIDTH: u32 = 320;
const FALLBACK_HEIGHT: u32 = 200;
const TIMER_INTERVAL: Duration = Duration::from_millis(250);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const CHANNEL_TOLERANCE: i32 = 40;

struct PresenterState {
    renderer: WgpuRenderer,
    presenter: SlintTexturePresenter,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    slint::BackendSelector::new()
        .require_wgpu_30(slint::wgpu_30::WGPUConfiguration::default())
        .select()?;

    let ui = App::new()?;
    let state: Rc<RefCell<Option<PresenterState>>> = Rc::new(RefCell::new(None));
    let tick = Rc::new(Cell::new(0u32));
    let outcome: Rc<RefCell<Result<(), String>>> = Rc::new(RefCell::new(Ok(())));
    let completed = Rc::new(Cell::new(false));

    let setup_weak = ui.as_weak();
    let setup_state = Rc::clone(&state);
    ui.window()
        .set_rendering_notifier(move |rendering_state, api| {
            match rendering_state {
                slint::RenderingState::RenderingSetup => {
                    let slint::GraphicsAPI::WGPU30 { device, queue, .. } = api else {
                        eprintln!(
                            "two_colors: expected a WGPU30 graphics API; \
                             set SLINT_WGPU_CPU=1 when only a CPU adapter is available"
                        );
                        return;
                    };

                    let Some(ui) = setup_weak.upgrade() else {
                        return;
                    };
                    let adapter = device.adapter_info();
                    eprintln!(
                        "two_colors: Slint WGPU30 adapter={:?} backend={:?} device_type={:?}",
                        adapter.name, adapter.backend, adapter.device_type
                    );
                    let window_size = ui.window().size();
                    let size = if window_size.width == 0 || window_size.height == 0 {
                        PhysicalSize {
                            width: FALLBACK_WIDTH,
                            height: FALLBACK_HEIGHT,
                        }
                    } else {
                        PhysicalSize {
                            width: window_size.width,
                            height: window_size.height,
                        }
                    };

                    let renderer = WgpuRenderer::new(device.clone(), queue.clone(), size)
                        .expect("create offscreen WGPU renderer on Slint's device");
                    let texture = renderer.texture().clone();
                    let image_weak = setup_weak.clone();
                    let presenter = SlintTexturePresenter::new(texture, move |image| {
                        if let Some(ui) = image_weak.upgrade() {
                            ui.set_frame(image);
                        }
                    })
                    .expect("import offscreen texture as slint::Image");

                    eprintln!(
                        "two_colors: imported {}x{} {:?} texture on Slint's device",
                        presenter.size().width,
                        presenter.size().height,
                        tervo_render_wgpu::FRAME_FORMAT,
                    );
                    *setup_state.borrow_mut() = Some(PresenterState {
                        renderer,
                        presenter,
                    });
                }
                slint::RenderingState::RenderingTeardown => {
                    // Drop GPU handles while the renderer/device is still alive.
                    *setup_state.borrow_mut() = None;
                }
                _ => {}
            }
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
                "timed out after {STARTUP_TIMEOUT:?} waiting for rendering setup"
            ));
            slint::quit_event_loop().ok();
            return;
        }

        let mut state = timer_state.borrow_mut();
        let Some(frame_state) = state.as_mut() else {
            // The rendering notifier has not fired yet.
            return;
        };

        let n = tick.get() + 1;
        tick.set(n);

        // Red, blue, red: the second red proves that the texture imported once
        // keeps being sampled live after later GPU writes.
        let (color, expected, label) = match n {
            1 | 2 => (RED, RED, "red-1"),
            3 | 4 => (BLUE, BLUE, "blue"),
            5 | 6 => (RED, RED, "red-2"),
            7 => {
                timer_completed.set(true);
                slint::quit_event_loop().ok();
                return;
            }
            _ => {
                *timer_outcome.borrow_mut() = Err(format!("unexpected timer tick {n}"));
                slint::quit_event_loop().ok();
                return;
            }
        };

        frame_state.renderer.set_clear_color(color);
        let rendered = frame_state
            .renderer
            .render(&Scene::default(), &DamageRegion::full())
            .expect("clear render pass");
        let presented = frame_state.presenter.present(ui.window());
        eprintln!(
            "two_colors: tick={n} render_revision={} present_revision={presented} color={label}",
            rendered.revision
        );

        if n <= 6 && n % 2 == 0 {
            if let Err(error) = snapshot_and_verify(&ui, expected, label) {
                *timer_outcome.borrow_mut() = Err(error);
                slint::quit_event_loop().ok();
            }
        }
    });

    ui.run()?;

    let result = match outcome.borrow().as_ref() {
        Ok(()) if completed.get() => {
            println!(
                "PASS two_colors: red -> blue -> red snapshots verified with Window::take_snapshot"
            );
            Ok(())
        }
        Ok(()) => Err("event loop exited before the final snapshot was verified".into()),
        Err(error) => Err(std::io::Error::other(error.clone()).into()),
    };
    result
}

fn snapshot_and_verify(ui: &App, expected: [u8; 4], label: &str) -> Result<(), String> {
    let snapshot = ui
        .window()
        .take_snapshot()
        .map_err(|error| format!("Window::take_snapshot failed: {error}"))?;
    let width = snapshot.width();
    let height = snapshot.height();
    if width == 0 || height == 0 {
        return Err(format!("snapshot for {label} is empty ({width}x{height})"));
    }

    // Uniform texture, but sampling several points catches a partially
    // composited image (e.g. only one quadrant being updated).
    let points = [
        (width / 2, height / 2),
        (width / 4, height / 4),
        (width.saturating_mul(3) / 4, height.saturating_mul(3) / 4),
    ];
    let bytes = snapshot.as_bytes();
    for (index, (x, y)) in points.iter().enumerate() {
        let offset = (y * width + x) as usize * 4;
        let actual = [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ];
        if !close_enough(actual, expected) {
            return Err(format!(
                "snapshot {label} sample {index} at ({x},{y}) is {actual:?}, expected {expected:?}"
            ));
        }
    }

    let center_offset = (height / 2 * width + width / 2) as usize * 4;
    let center = &bytes[center_offset..center_offset + 4];
    let path = write_ppm(&snapshot, label)
        .map_err(|error| format!("failed to write snapshot {label} to disk: {error}"))?;
    println!(
        "snapshot {label}: {}x{} center={center:?} -> {}",
        width,
        height,
        path.display()
    );
    Ok(())
}

fn close_enough(actual: [u8; 4], expected: [u8; 4]) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(&a, e)| (i32::from(a) - i32::from(e)).abs() <= CHANNEL_TOLERANCE)
}

fn write_ppm(
    snapshot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    label: &str,
) -> std::io::Result<PathBuf> {
    let directory = std::env::var_os("TERVO_WGPU_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/render-wgpu-snapshots"));
    std::fs::create_dir_all(&directory)?;

    let width = snapshot.width();
    let height = snapshot.height();
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    ppm.reserve(width as usize * height as usize * 3);
    for pixel in snapshot.as_bytes().chunks_exact(4) {
        ppm.extend_from_slice(&pixel[..3]);
    }

    let path = directory.join(format!("two-colors-{label}.ppm"));
    std::fs::write(&path, ppm)?;
    Ok(path)
}
