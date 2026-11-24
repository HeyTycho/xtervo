// SPDX-License-Identifier: GPL-3.0-only
//! Single-process multi-window spike (throw-away evidence crate; **not** a
//! production dependency and must not be wired into `xtervo-app`).
//!
//! Three modes:
//!
//! * `wgpu`       – two independent `SpikeWindow` components in one Slint
//!                  event loop, rendered by the WGPU30 backend. Asserts that
//!                  both windows received the *same* `wgpu::Device`/`Queue`,
//!                  exchange textures across windows, resize independently,
//!                  close one without losing the other's surface, then open a
//!                  third window that reuses the surviving device.
//! * `software`   – same two-window lifecycle on the winit-software renderer
//!                  (fallback baseline; no GPU involved).
//! * `mainwindow` – two instances of the **unmodified** production
//!                  `crates/app/ui/main.slint` `MainWindow`, each with its own
//!                  callbacks/size/title; checks whether the exported globals
//!                  (`Palette`, `Lang`) are per-window or process-wide.
//!
//! The binary is deliberately self-contained: it does not link `xtervo-app`,
//! touch `AppState`, PTYs, plugins, MCP or the session writer.
//!
//! Exit code 0 only on an explicit `PASS`; every failed assertion prints
//! `FAIL <mode>: ...`, quits the event loop and returns non-zero.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::wgpu_30::wgpu;
use slint::winit_030::{winit, EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, GraphicsAPI, RenderingState, Rgba8Pixel, SharedPixelBuffer};

mod app_ui {
    // Generated from ../app/ui/main.slint by build.rs. The production markup is
    // consumed unchanged; only the spike crate compiles it a second time.
    slint::include_modules!();
}

slint::slint! {
    // Minimal stand-in for a xtervo window. Deliberately independent of the
    // production `MainWindow` so renderer/lifecycle questions are not coupled
    // to the real AppState.
    export component SpikeWindow inherits Window {
        title: root.label;
        in property <string> label: "spike";
        in property <color> accent: #7aa2f7;
        in property <string> status: "booting";
        in property <image> frame;
        in-out property <int> clicks: 0;
        callback poke(string);

        preferred-width: 520px;
        preferred-height: 340px;

        Rectangle {
            width: 100%;
            height: 100%;
            background: root.accent;

            Text {
                x: 16px;
                y: 10px;
                text: root.label;
                color: #101018;
                font-size: 22px;
                font-weight: 700;
            }
            Text {
                x: 16px;
                y: 46px;
                text: root.status;
                color: #101018;
                font-size: 13px;
            }

            Rectangle {
                x: 16px;
                y: 74px;
                width: 220px;
                height: 140px;
                background: #000000;

                Image {
                    width: 100%;
                    height: 100%;
                    source: root.frame;
                }
            }

            Rectangle {
                x: 260px;
                y: 74px;
                width: 220px;
                height: 90px;
                background: #24283b;
                border-radius: 8px;

                Text {
                    x: 12px;
                    y: 10px;
                    text: "CLICK ME";
                    color: #c0caf5;
                    font-size: 18px;
                }
                Text {
                    x: 12px;
                    y: 42px;
                    text: "clicks: " + root.clicks;
                    color: #a9b1d6;
                    font-size: 13px;
                }
                TouchArea {
                    clicked => {
                        root.clicks += 1;
                        root.poke("button " + root.clicks);
                    }
                }
            }
        }
    }
}

use SpikeWindow as Spike;

const MODE_WGPU: &str = "wgpu";
const MODE_SOFTWARE: &str = "software";
const MODE_MAINWINDOW: &str = "mainwindow";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| MODE_WGPU.to_string());
    let hold_ms = parse_arg(&args, "--hold-ms").unwrap_or(12_000);
    let out = parse_arg_str(&args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("docs/evidence/mw-spike/{mode}")));
    if let Err(error) = std::fs::create_dir_all(&out) {
        eprintln!(
            "FAIL {mode}: cannot create output dir {}: {error}",
            out.display()
        );
        std::process::exit(2);
    }

    eprintln!(
        "[mw-spike] mode={mode} out={} hold_ms={hold_ms} display={:?}",
        out.display(),
        std::env::var("DISPLAY").ok()
    );

    let result = match mode.as_str() {
        MODE_WGPU => run(world_wgpu, &out, hold_ms),
        MODE_SOFTWARE => run(world_software, &out, hold_ms),
        MODE_MAINWINDOW => run_mainwindow(&out, hold_ms),
        other => {
            eprintln!("unknown mode {other:?}; expected wgpu|software|mainwindow");
            std::process::exit(2);
        }
    };

    if let Err(error) = result {
        eprintln!("FAIL {mode}: {error}");
        std::process::exit(1);
    }
    println!("PASS mw_spike mode={mode} out={}", out.display());
}

fn parse_arg(args: &[String], name: &str) -> Option<u64> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

fn parse_arg_str(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Everything owned by one run; kept in a `Result` cell so the event loop can
/// quit and `main` can still return a proper error.
#[derive(Default)]
struct Driver {
    mode: &'static str,
    phase: usize,
    started: Option<Instant>,
    pass: bool,
    failure: Option<String>,
    /// `Some` until phase "close B" drops the component.
    b: Option<Spike>,
    c: Option<Spike>,
    frames_a_before_close: u64,
    last_focus_log: Option<Instant>,
    pass_elapsed_ms: u64,
}

type Probes = Rc<RefCell<BTreeMap<&'static str, GpuProbe>>>;

#[derive(Default)]
struct GpuProbe {
    setups: u64,
    frames: u64,
    teardowns: u64,
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    adapter: Option<wgpu::AdapterInfo>,
}

struct World {
    a: Spike,
    b: Option<Spike>,
    probes: Probes,
    mode: &'static str,
}

fn world_wgpu() -> Result<World, String> {
    slint::BackendSelector::new()
        .require_wgpu_30(slint::wgpu_30::WGPUConfiguration::default())
        .select()
        .map_err(|e| format!("require_wgpu_30/select failed: {e}"))?;
    world_spike("wgpu")
}

fn world_software() -> Result<World, String> {
    // The backend selector reads SLINT_BACKEND; make the fallback explicit
    // instead of relying on the caller's environment.
    std::env::set_var("SLINT_BACKEND", "winit-software");
    slint::BackendSelector::new()
        .select()
        .map_err(|e| format!("winit-software select failed: {e}"))?;
    world_spike("software")
}

fn world_spike(mode: &'static str) -> Result<World, String> {
    let a = Spike::new().map_err(|e| format!("SpikeWindow A::new: {e}"))?;
    let b = Spike::new().map_err(|e| format!("SpikeWindow B::new: {e}"))?;

    configure_spike(
        &a,
        "A",
        "MW-Spike A",
        slint::Color::from_rgb_u8(0x7a, 0xa2, 0xf7),
    );
    configure_spike(
        &b,
        "B",
        "MW-Spike B",
        slint::Color::from_rgb_u8(0xf7, 0x76, 0x8e),
    );

    let probes: Probes = Rc::new(RefCell::new(BTreeMap::new()));
    // The winit-software renderer returns Unsupported for a rendering
    // notifier (no GPU API); that is the baseline, not a failure.
    if mode == MODE_WGPU {
        install_gpu_probe(&a, "A", probes.clone())?;
        install_gpu_probe(&b, "B", probes.clone())?;
    } else {
        // Record the concrete API limitation instead of silently skipping it:
        // the software renderer has no rendering notifier / GPU API.
        match a.window().set_rendering_notifier(|_, _| {}) {
            Ok(()) => eprintln!("[software] set_rendering_notifier unexpectedly succeeded"),
            Err(error) => {
                eprintln!(
                    "[software] set_rendering_notifier returned {error:?} (expected Unsupported)"
                )
            }
        }
    }
    install_focus_log(&a, "A");
    install_focus_log(&b, "B");

    a.show().map_err(|e| format!("show A: {e}"))?;
    b.show().map_err(|e| format!("show B: {e}"))?;
    place(&a, 40, 40, 520, 340);
    place(&b, 620, 40, 640, 400);

    Ok(World {
        a,
        b: Some(b),
        probes,
        mode,
    })
}

fn configure_spike(ui: &Spike, id: &'static str, title: &str, accent: slint::Color) {
    ui.set_label(title.into());
    ui.set_accent(accent);
    ui.set_status("waiting for renderer setup".into());
    ui.on_poke(move |message| {
        eprintln!("[callback] window={id} msg={message:?}");
    });
}

fn place(ui: &Spike, x: i32, y: i32, w: u32, h: u32) {
    // Position/size requests are asynchronous on X11; the driver waits and
    // re-reads both Slint and winit sizes before asserting.
    ui.window().set_position(slint::PhysicalPosition::new(x, y));
    ui.window().set_size(slint::PhysicalSize::new(w, h));
}

fn install_gpu_probe(ui: &Spike, id: &'static str, probes: Probes) -> Result<(), String> {
    ui.window()
        .set_rendering_notifier(move |state, api| {
            let mut map = probes.borrow_mut();
            let probe = map.entry(id).or_default();
            match state {
                RenderingState::RenderingSetup => {
                    probe.setups += 1;
                    match api {
                        GraphicsAPI::WGPU30 { device, queue, .. } => {
                            let info = device.adapter_info();
                            eprintln!(
                                "[gpu:{id}] RenderingSetup #{} adapter={:?} backend={:?} device_type={:?}",
                                probe.setups, info.name, info.backend, info.device_type
                            );
                            probe.device = Some(device.clone());
                            probe.queue = Some(queue.clone());
                            probe.adapter = Some(info);
                        }
                        other => {
                            eprintln!(
                                "[gpu:{id}] RenderingSetup #{} WITHOUT WGPU30: {other:?}",
                                probe.setups
                            );
                        }
                    }
                }
                RenderingState::BeforeRendering => {
                    probe.frames += 1;
                    if probe.frames == 1 || probe.frames % 120 == 0 {
                        eprintln!("[gpu:{id}] frames={}", probe.frames);
                    }
                }
                RenderingState::RenderingTeardown => {
                    probe.teardowns += 1;
                    eprintln!("[gpu:{id}] RenderingTeardown #{}", probe.teardowns);
                }
                _ => {}
            }
        })
        .map_err(|e| format!("set_rendering_notifier({id}): {e:?}"))
}

fn install_focus_log(ui: &Spike, id: &'static str) {
    ui.window().on_winit_window_event(move |_window, event| {
        match event {
            winit::event::WindowEvent::Focused(focused) => {
                eprintln!("[focus:{id}] focused={focused}");
            }
            winit::event::WindowEvent::Resized(size) => {
                eprintln!(
                    "[resize:{id}] winit inner_size={}x{}",
                    size.width, size.height
                );
            }
            winit::event::WindowEvent::CloseRequested => {
                eprintln!("[close-request:{id}] (Slint default: hide only)");
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

fn winit_facts(ui: &Spike) -> String {
    window_winit_facts(ui.window())
}

fn slint_facts(ui: &Spike) -> String {
    let size = ui.window().size();
    format!(
        "slint_size={}x{} visible={}",
        size.width,
        size.height,
        ui.window().is_visible()
    )
}

/// Run one driver to completion. The timer owns `Driver`, so the event loop
/// only needs to spin until the closure calls `quit_event_loop`.
fn run(init: fn() -> Result<World, String>, out: &Path, hold_ms: u64) -> Result<(), String> {
    let World { a, b, probes, mode } = init()?;
    let out = out.to_path_buf();

    // Clone the handles for the driver; the originals are retained by the
    // backend while a window is shown anyway.
    let driver = Rc::new(RefCell::new(Driver {
        mode,
        b,
        started: Some(Instant::now()),
        ..Default::default()
    }));

    // Keep `a` outside the driver for callbacks; both the timer and callbacks
    // hold their own clones.
    let a_for_tick = a.clone_strong();
    let probes_for_tick = probes.clone();
    let out_for_tick = out.clone();

    let timer = slint::Timer::default();
    let driver_for_timer = driver.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let mut d = driver_for_timer.borrow_mut();
            if d.failure.is_some() {
                return;
            }
            let started = d.started.unwrap();
            let elapsed = started.elapsed();
            let tick = tick(
                &mut d,
                &a_for_tick,
                &probes_for_tick,
                &out_for_tick,
                elapsed,
                hold_ms,
            );
            if let Err(error) = tick {
                eprintln!("FAIL {}: {error}", d.mode);
                d.failure = Some(error);
                slint::quit_event_loop().ok();
            } else if d.pass && elapsed > Duration::from_millis(d.pass_elapsed_ms + hold_ms) {
                slint::quit_event_loop().ok();
            }
        },
    );

    slint::run_event_loop().map_err(|e| format!("run_event_loop: {e}"))?;
    drop(timer);
    drop(a);

    let d = driver.borrow();
    if let Some(error) = &d.failure {
        return Err(error.clone());
    }
    if !d.pass {
        return Err("event loop exited before the spike reached PASS".into());
    }
    Ok(())
}

fn tick(
    d: &mut Driver,
    a: &Spike,
    probes: &Probes,
    out: &Path,
    elapsed: Duration,
    hold_ms: u64,
) -> Result<(), String> {
    // The first renderer setup can take several seconds on llvmpipe. Wait
    // explicitly instead of turning the first 100 ms tick into a failure.
    if d.phase == 0 {
        if d.mode == MODE_SOFTWARE {
            if elapsed < Duration::from_millis(1200) {
                return Ok(());
            }
            a.set_status("winit-software baseline".into());
            if let Some(b) = d.b.as_ref() {
                b.set_status("winit-software baseline".into());
                b.window().request_redraw();
            }
            a.window().request_redraw();
            eprintln!("[software] two windows created without a rendering notifier");
            d.phase = 1;
            return Ok(());
        }
        if elapsed < Duration::from_millis(700) {
            return Ok(());
        }
        let map = probes.borrow();
        let a_probe = map.get("A").ok_or("renderer probe A missing")?;
        let b_probe = map.get("B").ok_or("renderer probe B missing")?;
        if a_probe.device.is_none() || b_probe.device.is_none() {
            if elapsed > Duration::from_secs(12) {
                return Err(format!(
                    "timed out waiting for both RenderingSetup callbacks: A={:?} B={:?} \
                     (software backend cannot provide WGPU30 devices)",
                    a_probe.device.is_some(),
                    b_probe.device.is_some()
                ));
            }
            return Ok(());
        }

        let (da, db) = (
            a_probe.device.clone().unwrap(),
            b_probe.device.clone().unwrap(),
        );
        let (qa, qb) = (
            a_probe.queue.clone().unwrap(),
            b_probe.queue.clone().unwrap(),
        );
        let same_device = da == db;
        let same_queue = qa == qb;
        let same_adapter = a_probe.adapter == b_probe.adapter;
        eprintln!(
            "[gpu] cross-window identity: same_device={same_device} same_queue={same_queue} \
             same_adapter={same_adapter}"
        );
        eprintln!(
            "[gpu] A setup={} B setup={}",
            a_probe.setups, b_probe.setups
        );
        if !same_device || !same_queue {
            return Err(format!(
                "two windows did NOT share one WGPU30 device/queue (same_device={same_device}, \
                 same_queue={same_queue}); independent wgpu instances per window"
            ));
        }
        drop(map);

        // Cross-window texture exchange: the texture shown in B is created on
        // A's device/queue; the texture shown in A is created on B's device.
        // Deliberately create each texture on the *other* window's reported
        // device/queue: the pixels proving this is accepted by both surfaces.
        let green_from_b = solid_texture(&db, &qb, [0, 255, 0, 255], "mw-spike-green-on-B-device")?;
        let red_from_a = solid_texture(&da, &qa, [255, 0, 0, 255], "mw-spike-red-on-A-device")?;
        a.set_frame(green_from_b);
        if let Some(b) = d.b.as_ref() {
            b.set_frame(red_from_a);
        }
        a.set_status("A | shared device; texture from B's device".into());
        if let Some(b) = d.b.as_ref() {
            b.set_status("B | shared device; texture from A's device".into());
            b.window().request_redraw();
        }
        a.window().request_redraw();

        d.phase = 1;
        return Ok(());
    }

    if d.phase == 1 {
        if elapsed < Duration::from_millis(1800) {
            return Ok(());
        }
        let a_snap = snapshot(a, &out.join("A_initial.png"))?;
        check_rectangle(
            &a_snap,
            slint::Color::from_rgb_u8(0x7a, 0xa2, 0xf7),
            "A background",
        )?;
        if d.mode != MODE_SOFTWARE {
            check_pixel(
                &a_snap,
                tex_center(a, 16, 74, 220, 140),
                [0, 255, 0, 255],
                4,
                "A frame (texture created on B's device)",
            )?;
        }
        eprintln!("[snapshot] A_initial {}", a_snap.summary());

        let b =
            d.b.as_ref()
                .ok_or("B disappeared before initial snapshot")?;
        let b_snap = snapshot(b, &out.join("B_initial.png"))?;
        check_rectangle(
            &b_snap,
            slint::Color::from_rgb_u8(0xf7, 0x76, 0x8e),
            "B background",
        )?;
        if d.mode != MODE_SOFTWARE {
            check_pixel(
                &b_snap,
                tex_center(b, 16, 74, 220, 140),
                [255, 0, 0, 255],
                4,
                "B frame (texture created on A's device)",
            )?;
        }
        eprintln!("[snapshot] B_initial {}", b_snap.summary());

        eprintln!("[facts] A: {} {}", slint_facts(a), winit_facts(a));
        eprintln!("[facts] B: {} {}", slint_facts(b), winit_facts(b));
        assert_size(a, 520, 340, "A initial")?;
        assert_size(b, 640, 400, "B initial")?;

        a.invoke_poke("programmatic A".into());
        b.invoke_poke("programmatic B".into());
        d.phase = 2;
        return Ok(());
    }

    if d.phase == 2 {
        if elapsed < Duration::from_millis(2400) {
            return Ok(());
        }
        // Resize A only, then B only, and verify the other window's client
        // size is untouched.
        a.window().set_size(slint::PhysicalSize::new(480, 300));
        a.window().request_redraw();
        eprintln!("[resize] requested A 480x300; B stays 640x400");
        d.phase = 3;
        return Ok(());
    }

    if d.phase == 3 {
        if elapsed < Duration::from_millis(3200) {
            return Ok(());
        }
        assert_size(a, 480, 300, "A after resize")?;
        let b = d.b.as_ref().ok_or("B disappeared during A resize")?;
        assert_size(b, 640, 400, "B during A resize")?;

        b.window().set_size(slint::PhysicalSize::new(600, 360));
        b.window().request_redraw();
        a.window().request_redraw();
        d.phase = 4;
        return Ok(());
    }

    if d.phase == 4 {
        if elapsed < Duration::from_millis(4000) {
            return Ok(());
        }
        let a_snap = snapshot(a, &out.join("A_after_resize.png"))?;
        let b = d.b.as_ref().ok_or("B disappeared after resize")?;
        let b_snap = snapshot(b, &out.join("B_after_resize.png"))?;
        assert_snap_size(&a_snap, 480, 300, "A resized snapshot")?;
        assert_snap_size(&b_snap, 600, 360, "B resized snapshot")?;
        check_rectangle(
            &a_snap,
            slint::Color::from_rgb_u8(0x7a, 0xa2, 0xf7),
            "A resized bg",
        )?;
        check_rectangle(
            &b_snap,
            slint::Color::from_rgb_u8(0xf7, 0x76, 0x8e),
            "B resized bg",
        )?;
        eprintln!("[snapshot] A_after_resize {}", a_snap.summary());
        eprintln!("[snapshot] B_after_resize {}", b_snap.summary());
        eprintln!("[facts] A resized: {}", winit_facts(a));
        eprintln!("[facts] B resized: {}", winit_facts(b));
        d.phase = 5;
        return Ok(());
    }

    if d.phase == 5 {
        if elapsed < Duration::from_millis(8000) {
            return Ok(());
        }
        // Close B while A keeps running. hide() drops Slint's internal strong
        // reference; dropping the handle then destroys the component/window.
        let before = probes.borrow().get("A").map(|p| p.frames).unwrap_or(0);
        d.frames_a_before_close = before;
        eprintln!("[close] closing B (A frames before close={before})");
        if let Some(b) = d.b.take() {
            b.hide().map_err(|e| format!("hide B: {e}"))?;
            drop(b);
        }
        a.window().request_redraw();
        d.phase = 6;
        return Ok(());
    }

    if d.phase == 6 {
        if elapsed < Duration::from_millis(9500) {
            return Ok(());
        }
        let after = probes.borrow().get("A").map(|p| p.frames).unwrap_or(0);
        let b_teardown = probes.borrow().get("B").map(|p| p.teardowns).unwrap_or(0);
        eprintln!(
            "[close] after B closed: A frames={} (was {}), B teardowns={}",
            after, d.frames_a_before_close, b_teardown
        );
        if d.mode != MODE_SOFTWARE && after <= d.frames_a_before_close {
            return Err(format!(
                "A stopped rendering after B closed (frames {} -> {}); shared surface/device was lost",
                d.frames_a_before_close, after
            ));
        }

        // New window after B's teardown: it must reuse A's surviving device.
        let c = Spike::new().map_err(|e| format!("SpikeWindow C::new: {e}"))?;
        configure_spike(
            &c,
            "C",
            "MW-Spike C",
            slint::Color::from_rgb_u8(0x9e, 0xce, 0x6a),
        );
        c.set_status("third window after B closed".into());
        if d.mode == MODE_WGPU {
            install_gpu_probe(&c, "C", probes.clone())?;
        }
        install_focus_log(&c, "C");
        c.show().map_err(|e| format!("show C: {e}"))?;
        place(&c, 1080, 40, 500, 320);
        d.c = Some(c);
        a.window().request_redraw();
        d.phase = 7;
        return Ok(());
    }

    if d.phase == 7 {
        if elapsed < Duration::from_millis(11000) {
            return Ok(());
        }
        if d.mode == MODE_SOFTWARE {
            let c = d.c.as_ref().ok_or("C handle missing")?;
            c.set_status("C | third window after B closed (software)".into());
            c.window().request_redraw();
            a.window().request_redraw();
            d.phase = 8;
            return Ok(());
        }
        let c_ready = {
            let map = probes.borrow();
            map.get("C").and_then(|p| p.device.clone())
        };
        let Some(c_device) = c_ready else {
            return Err("C never received a RenderingSetup/WGPU30 device".into());
        };
        let c = d.c.as_ref().ok_or("C handle missing")?;
        if d.mode != MODE_SOFTWARE {
            let a_device = probes.borrow().get("A").and_then(|p| p.device.clone());
            eprintln!(
                "[gpu] C device equals A device after B close: {}",
                Some(&c_device) == a_device.as_ref()
            );
            if Some(&c_device) != a_device.as_ref() {
                return Err("C did not reuse A's WGPU30 device after B closed".into());
            }
            let c_queue = probes.borrow().get("C").and_then(|p| p.queue.clone());
            let Some(c_queue) = c_queue else {
                return Err("C RenderingSetup did not provide a queue".into());
            };
            let blue = solid_texture(
                &c_device,
                &c_queue,
                [0, 0, 255, 255],
                "mw-spike-blue-on-C-device",
            )?;
            c.set_frame(blue);
            c.set_status("C | reused A device after B closed".into());
        } else {
            c.set_status("C | third window after B closed (software)".into());
        }
        c.window().request_redraw();
        a.window().request_redraw();
        d.phase = 8;
        return Ok(());
    }

    if d.phase == 8 {
        if elapsed < Duration::from_millis(12000) {
            return Ok(());
        }
        let a_snap = snapshot(a, &out.join("A_after_B_closed.png"))?;
        check_rectangle(
            &a_snap,
            slint::Color::from_rgb_u8(0x7a, 0xa2, 0xf7),
            "A bg after B close",
        )?;
        if d.mode != MODE_SOFTWARE {
            check_pixel(
                &a_snap,
                tex_center(a, 16, 74, 220, 140),
                [0, 255, 0, 255],
                4,
                "A frame after B close",
            )?;
        }
        eprintln!("[snapshot] A_after_B_closed {}", a_snap.summary());
        eprintln!("[facts] A after B close: {}", winit_facts(a));

        let c = d.c.as_ref().ok_or("C handle missing at snapshot")?;
        let c_snap = snapshot(c, &out.join("C_after_B_closed.png"))?;
        check_rectangle(&c_snap, slint::Color::from_rgb_u8(0x9e, 0xce, 0x6a), "C bg")?;
        if d.mode != MODE_SOFTWARE {
            check_pixel(
                &c_snap,
                tex_center(c, 16, 74, 220, 140),
                [0, 0, 255, 255],
                4,
                "C frame",
            )?;
        }
        eprintln!("[snapshot] C_after_B_closed {}", c_snap.summary());
        eprintln!("[facts] C: {} {}", slint_facts(c), winit_facts(c));
        assert_size(c, 500, 320, "C")?;

        c.invoke_poke("programmatic C".into());
        eprintln!(
            "[result] {} independent sizes; A survived B close",
            if d.mode == MODE_SOFTWARE {
                "winit-software multi-window:".to_string()
            } else {
                "shared WGPU30 device/queue across A/B/C;".to_string()
            }
        );
        d.pass = true;
        d.pass_elapsed_ms = elapsed.as_millis() as u64;
        eprintln!(
            "[hold] PASS reached at {} ms; keeping both windows alive for {} ms for external X11 checks",
            elapsed.as_millis(),
            hold_ms
        );
        d.phase = 9;
        return Ok(());
    }

    if d.phase == 9 {
        // Keep logging focus/frames during the external observation window.
        if d.last_focus_log
            .map(|t| t.elapsed() >= Duration::from_secs(1))
            .unwrap_or(true)
        {
            eprintln!("[hold] A: {}", winit_facts(a));
            if let Some(c) = d.c.as_ref() {
                eprintln!("[hold] C: {}", winit_facts(c));
            }
            let map = probes.borrow();
            eprintln!(
                "[hold] frames A={} C={} teardown B={}",
                map.get("A").map(|p| p.frames).unwrap_or(0),
                map.get("C").map(|p| p.frames).unwrap_or(0),
                map.get("B").map(|p| p.teardowns).unwrap_or(0)
            );
            d.last_focus_log = Some(Instant::now());
        }
        if elapsed > Duration::from_millis(d.pass_elapsed_ms + hold_ms) {
            slint::quit_event_loop().ok();
        }
        return Ok(());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WGPU helper
// ---------------------------------------------------------------------------

fn solid_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    rgba: [u8; 4],
    label: &str,
) -> Result<slint::Image, String> {
    const W: u32 = 220;
    const H: u32 = 140;
    let pixels: Vec<u8> = rgba
        .iter()
        .copied()
        .cycle()
        .take((W * H * 4) as usize)
        .collect();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(W * 4),
            rows_per_image: Some(H),
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    slint::Image::try_from(texture).map_err(|e| format!("Image::try_from({label}): {e:?}"))
}

// ---------------------------------------------------------------------------
// Snapshot helpers
// ---------------------------------------------------------------------------

struct Snap {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Snap {
    fn summary(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }
}

fn snapshot(ui: &Spike, path: &Path) -> Result<Snap, String> {
    snapshot_window(ui.window(), path)
}

fn snapshot_window(window: &slint::Window, path: &Path) -> Result<Snap, String> {
    let buffer: SharedPixelBuffer<Rgba8Pixel> = window
        .take_snapshot()
        .map_err(|e| format!("take_snapshot {}: {e}", path.display()))?;
    let width = buffer.width();
    let height = buffer.height();
    let pixels: Vec<u8> = buffer
        .as_slice()
        .iter()
        .flat_map(|p| [p.r, p.g, p.b, p.a])
        .collect();
    write_png(path, width, height, &pixels)?;
    Ok(Snap {
        width,
        height,
        pixels,
    })
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let file = File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("png header: {e}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| format!("png data: {e}"))?;
    eprintln!("[shot] {} ({}x{})", path.display(), width, height);
    Ok(())
}

/// Sample coordinate for the centre of the frame image, in physical pixels.
/// The layout uses fixed logical pixels; Xvfb/Openbox runs at scale factor 1,
/// but scaling here keeps the check honest if the display DPR changes.
fn tex_center(ui: &Spike, x: u32, y: u32, w: u32, h: u32) -> (u32, u32) {
    let scale = ui.window().scale_factor();
    let px = ((x as f32 + w as f32 / 2.0) * scale) as u32;
    let py = ((y as f32 + h as f32 / 2.0) * scale) as u32;
    (px, py)
}

fn assert_size(ui: &Spike, w: u32, h: u32, what: &str) -> Result<(), String> {
    let actual = ui.window().size();
    if actual.width.abs_diff(w) > 2 || actual.height.abs_diff(h) > 2 {
        return Err(format!(
            "{what}: Slint window size is {}x{}, expected {w}x{h}",
            actual.width, actual.height
        ));
    }
    Ok(())
}

fn assert_snap_size(snap: &Snap, w: u32, h: u32, what: &str) -> Result<(), String> {
    if snap.width != w || snap.height != h {
        return Err(format!(
            "{what}: snapshot is {}x{}, expected {w}x{h}",
            snap.width, snap.height
        ));
    }
    Ok(())
}

fn check_rectangle(snap: &Snap, expected: slint::Color, what: &str) -> Result<(), String> {
    let (x, y) = (3, 3);
    let expected = [expected.red(), expected.green(), expected.blue(), 255];
    check_pixel(snap, (x, y), expected, 2, what)
}

fn check_pixel(
    snap: &Snap,
    (x, y): (u32, u32),
    expected: [u8; 4],
    tolerance: u8,
    what: &str,
) -> Result<(), String> {
    if x >= snap.width || y >= snap.height {
        return Err(format!(
            "{what}: sample ({x},{y}) outside {}x{} snapshot",
            snap.width, snap.height
        ));
    }
    let actual = snap.px(x, y);
    let close = actual
        .iter()
        .zip(expected.iter())
        .all(|(a, e)| a.abs_diff(*e) <= tolerance);
    if !close {
        return Err(format!(
            "{what}: pixel ({x},{y}) = {actual:?}, expected {expected:?} (+-{tolerance})"
        ));
    }
    eprintln!("[pixel] {what} ({x},{y})={actual:?} ok");
    Ok(())
}

// ---------------------------------------------------------------------------
// Production MainWindow: two instances in one process
// ---------------------------------------------------------------------------

fn run_mainwindow(out: &Path, hold_ms: u64) -> Result<(), String> {
    // Default backend selection; the run script sets SLINT_BACKEND=winit-software
    // for deterministic Xvfb captures. This mode answers the component and
    // callback question, not the GPU question.
    slint::BackendSelector::new()
        .select()
        .map_err(|e| format!("BackendSelector::select: {e}"))?;

    let a = app_ui::MainWindow::new().map_err(|e| format!("MainWindow A::new: {e}"))?;
    let b = app_ui::MainWindow::new().map_err(|e| format!("MainWindow B::new: {e}"))?;

    a.set_window_title("MW-Spike MainWindow A".into());
    b.set_window_title("MW-Spike MainWindow B".into());
    a.set_language("en".into());
    b.set_language("zh".into());
    a.set_settings_open(false);
    b.set_settings_open(false);

    // Each closure captures a different logical window id. If callbacks were
    // process-global (not per-instance), these lines would be indistinguishable
    // in the log.
    let id_a = "A";
    a.on_new_tab(move || eprintln!("[callback] MainWindow id={id_a} callback=new-tab"));
    let id_b = "B";
    b.on_new_tab(move || eprintln!("[callback] MainWindow id={id_b} callback=new-tab"));

    a.show().map_err(|e| format!("show MainWindow A: {e}"))?;
    b.show().map_err(|e| format!("show MainWindow B: {e}"))?;
    a.window()
        .set_position(slint::PhysicalPosition::new(30, 30));
    a.window().set_size(slint::PhysicalSize::new(820, 560));
    b.window()
        .set_position(slint::PhysicalPosition::new(870, 30));
    b.window().set_size(slint::PhysicalSize::new(700, 480));

    // Slint exported globals are the interesting production-risk surface:
    // `Lang` and `Palette` are used by every MainWindow subtree. Probe whether
    // writing through one component handle leaks into the other instance.
    let a_bg_before = a.global::<app_ui::Palette>().get_colors().bg;
    let b_bg_before = b.global::<app_ui::Palette>().get_colors().bg;
    let mut red = a.global::<app_ui::Palette>().get_colors();
    red.bg = slint::Color::from_rgb_u8(255, 0, 0);
    a.global::<app_ui::Palette>().set_colors(red);
    let a_bg_after = a.global::<app_ui::Palette>().get_colors().bg;
    let b_bg_after = b.global::<app_ui::Palette>().get_colors().bg;
    eprintln!(
        "[globals] Palette before A={a_bg_before:?} B={b_bg_before:?}; after write A={a_bg_after:?} B={b_bg_after:?}; shared={}",
        b_bg_after == a_bg_after
    );

    a.global::<app_ui::Lang>().set_code("en".into());
    b.global::<app_ui::Lang>().set_code("zh".into());
    eprintln!(
        "[globals] Lang A={:?} B={:?} shared={}",
        a.global::<app_ui::Lang>().get_code(),
        b.global::<app_ui::Lang>().get_code(),
        a.global::<app_ui::Lang>().get_code() == b.global::<app_ui::Lang>().get_code()
    );

    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let passed = Rc::new(std::cell::Cell::new(false));
    let started = Instant::now();
    let mut phase = 0u8;
    let mut last_hold = Instant::now();

    let a_t = a.clone_strong();
    let b_t = b.clone_strong();
    let out = out.to_path_buf();
    let failure_t = failure.clone();
    let passed_t = passed.clone();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
        let elapsed = started.elapsed();
        if phase == 0 && elapsed >= Duration::from_millis(1600) {
            let a_snap = match snapshot_window(a_t.window(), &out.join("MainWindow_A.png")) {
                Ok(s) => s,
                Err(e) => {
                    *failure_t.borrow_mut() = Some(e);
                    slint::quit_event_loop().ok();
                    return;
                },
            };
            let b_snap = match snapshot_window(b_t.window(), &out.join("MainWindow_B.png")) {
                Ok(s) => s,
                Err(e) => {
                    *failure_t.borrow_mut() = Some(e);
                    slint::quit_event_loop().ok();
                    return;
                },
            };
            eprintln!("[snapshot] MainWindow_A {}", a_snap.summary());
            eprintln!("[snapshot] MainWindow_B {}", b_snap.summary());
            eprintln!("[facts] MainWindow A: {}", window_winit_facts(a_t.window()));
            eprintln!("[facts] MainWindow B: {}", window_winit_facts(b_t.window()));
            eprintln!(
                "[facts] MainWindow A title_prop={:?} lang={:?}",
                a_t.get_window_title(),
                a_t.get_language()
            );
            eprintln!(
                "[facts] MainWindow B title_prop={:?} lang={:?}",
                b_t.get_window_title(),
                b_t.get_language()
            );
            a_t.invoke_new_tab();
            b_t.invoke_new_tab();
            phase = 1;
        } else if phase == 1 && elapsed >= Duration::from_millis(2600) {
            eprintln!(
                "[result] two unmodified MainWindow instances are alive; per-instance callbacks fired"
            );
            passed_t.set(true);
            phase = 2;
            last_hold = Instant::now();
        } else if phase == 2 {
            if last_hold.elapsed() >= Duration::from_secs(1) {
                eprintln!("[hold] MainWindow A: {}", window_winit_facts(a_t.window()));
                eprintln!("[hold] MainWindow B: {}", window_winit_facts(b_t.window()));
                last_hold = Instant::now();
            }
            if elapsed > Duration::from_millis(2600 + hold_ms) {
                slint::quit_event_loop().ok();
            }
        }
    });

    slint::run_event_loop().map_err(|e| format!("run_event_loop: {e}"))?;
    drop(timer);
    drop(a);
    drop(b);

    if let Some(error) = failure.borrow_mut().take() {
        return Err(error);
    }
    if !passed.get() {
        return Err("event loop exited before MainWindow PASS".into());
    }
    Ok(())
}

fn window_winit_facts(window: &slint::Window) -> String {
    window
        .with_winit_window(|w| {
            format!(
                "winit_id={:?} title={:?} inner={}x{} has_focus={} visible={:?}",
                w.id(),
                w.title(),
                w.inner_size().width,
                w.inner_size().height,
                w.has_focus(),
                w.is_visible()
            )
        })
        .unwrap_or_else(|| "no-winit-window".into())
}
