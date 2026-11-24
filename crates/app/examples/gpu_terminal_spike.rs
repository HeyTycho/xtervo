// SPDX-License-Identifier: GPL-3.0-only
//! 最小验收：真实 PTY -> xtervo CPU `Renderer` RGBA framebuffer -> Slint 的
//! WGPU30 `Device/Queue` 上的 `Rgba8UnormSrgb` texture -> `slint::Image` ->
//! `Window::take_snapshot()` 逐像素对照。
//!
//! 这个 example 刻意**不启动 app 主循环**，只复用 app 的软件终端光栅化器
//! （`src/render.rs` + `src/glyph.rs`，通过 `#[path]` 引入）来回答一个问题：
//! “真实终端帧的像素，经过 Slint 自己的 WGPU device/queue 上传成 texture 后，
//! 是否会出现在同一个窗口的 snapshot 里？” 它不证明 GPU Scene、glyph atlas /
//! 图片 GPU 合成，也不代表 app 已切到 GPU 渲染。
//!
//! 运行（必须用隔离的 Xvfb，绝不能是用户的 :1）：
//!
//! ```text
//! Xvfb :77 -screen 0 1280x720x24 -nolisten tcp &
//! DISPLAY=:77 SLINT_WGPU_CPU=1 \
//!   CARGO_HOME=$PWD/.cargo CARGO_TARGET_DIR=$PWD/target \
//!   cargo run -p xtervo-app --example gpu_terminal_spike
//! ```
//!
//! 失败时不会打印 PASS：非 WGPU30 图形 API、没有字体、PTY 超时、snapshot
//! 尺寸不符或像素对照失败都会以非零码退出。快照/CPU framebuffer 写成 PPM，
//! 默认在 `target/agent-gpu-spike/`（可用 `XTERVO_GPU_SPIKE_OUT` 覆盖）。

#![allow(dead_code)]

// 复用 app 的真实终端光栅化器。两个模块之间只有 `crate::glyph` 依赖，因此
// 直接以 #[path] 编进 example crate，不需要给 xtervo-app 增加 lib target。
#[path = "../src/glyph.rs"]
mod glyph;
#[path = "../src/render.rs"]
mod render;

// `glyph.rs` 的 `FontOptions::from_config` 在主程序里读完整 Config；example
// 只复用光栅化器，不需要配置模块，提供同字段的最小占位类型即可。
mod config {
    #[allow(dead_code)]
    pub struct Config {
        pub font_family: String,
        pub font_weight: u16,
        pub line_height: f32,
        pub letter_spacing: f32,
        pub render_scale: f32,
    }
}

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::wgpu_30::wgpu;
use xtervo_core::{spawn, Frame, PaneConfig, Theme};

slint::slint! {
    export component App inherits Window {
        title: "xtervo-gpu-terminal-spike";
        in property <length> frame-w: 320px;
        in property <length> frame-h: 160px;
        width: root.frame-w;
        height: root.frame-h;
        in-out property <image> frame;
        // 洋红兜底：如果 texture 没被采样，snapshot 会看到它而不是终端内容。
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

const FONT_SIZE: f32 = 15.0;
const TERMINAL_COLS: u16 = 46;
const TERMINAL_ROWS: u16 = 9;
/// 固定 PTY 输出：1 行亮白文字 + 红/蓝背景块 + 一行普通文字。
const PTY_SCRIPT: &str = "printf '\\033[2J\\033[H\\033[1;97mXTERVO-GPU-PTY\\033[0m\\n\\033[41m  RED-BG  \\033[0m\\n\\033[44m  BLUE-BG  \\033[0m\\nPTY-FRAME-OK\\n'; exec sleep 300";
const PTY_TIMEOUT: Duration = Duration::from_secs(8);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);

struct CpuFrame {
    width: u32,
    height: u32,
    /// 直通 RGBA（非预乘）；Renderer::pixels() 的 u32 按小端拆成 R,G,B,A。
    rgba: Vec<u8>,
    /// 从 Frame grid 还原的文本，用于证明快照来自真实 PTY 内容。
    text: String,
    generation: u64,
}

#[derive(Default)]
struct GpuState {
    /// 保持 texture 存活；Image 被采样期间不能释放。
    texture: Option<wgpu::Texture>,
    ready: bool,
    done: bool,
    frames_after_ready: u32,
    error: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cpu = capture_pty_frame()?;
    eprintln!(
        "pty: generation={} grid={}x{} text={:?}",
        cpu.generation, TERMINAL_COLS, TERMINAL_ROWS, cpu.text
    );

    // 在创建 Slint 窗口之前把真实终端 framebuffer 准备好；之后不再依赖
    // xtervo-app 的事件循环、Session 或 Workspace。
    slint::BackendSelector::new()
        .require_wgpu_30(slint::wgpu_30::WGPUConfiguration::default())
        .select()?;

    let ui = App::new()?;
    ui.set_frame_w(cpu.width as f32);
    ui.set_frame_h(cpu.height as f32);
    ui.window()
        .set_size(slint::PhysicalSize::new(cpu.width, cpu.height));

    let state = Rc::new(RefCell::new(GpuState::default()));
    let completed = Rc::new(Cell::new(false));
    let cpu = Rc::new(cpu);

    install_rendering_notifier(&ui, cpu.clone(), state.clone())?;

    let timer = slint::Timer::default();
    let timer_weak = ui.as_weak();
    let timer_state = state.clone();
    let timer_cpu = cpu.clone();
    let timer_completed = completed.clone();
    let started = Instant::now();

    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(ui) = timer_weak.upgrade() else { return };
            let action = {
                let mut st = timer_state.borrow_mut();
                if st.done {
                    return;
                }
                if let Some(error) = st.error.take() {
                    st.done = true;
                    Some(Err(error))
                } else if !st.ready {
                    if started.elapsed() > SNAPSHOT_TIMEOUT {
                        st.done = true;
                        Some(Err(format!(
                            "timed out after {SNAPSHOT_TIMEOUT:?} waiting for WGPU RenderingSetup"
                        )))
                    } else {
                        None
                    }
                } else {
                    st.frames_after_ready += 1;
                    // 等一帧真正合成后再读回，避免拿到属性赋值前的旧 scene。
                    if st.frames_after_ready >= 2 {
                        st.done = true;
                        Some(verify_snapshot(&ui, &timer_cpu))
                    } else {
                        None
                    }
                }
            };

            match action {
                Some(Ok(summary)) => {
                    timer_completed.set(true);
                    println!("{summary}");
                    slint::quit_event_loop().ok();
                },
                Some(Err(error)) => {
                    eprintln!("FAIL gpu_terminal_spike: {error}");
                    timer_state.borrow_mut().error = Some(error);
                    slint::quit_event_loop().ok();
                },
                None => {},
            }
        },
    );

    ui.run()?;

    if let Some(error) = state.borrow().error.clone() {
        return Err(std::io::Error::other(error).into());
    }
    if !completed.get() {
        return Err(std::io::Error::other(
            "event loop exited before take_snapshot verification completed",
        )
        .into());
    }
    Ok(())
}

fn install_rendering_notifier(
    ui: &App,
    cpu: Rc<CpuFrame>,
    state: Rc<RefCell<GpuState>>,
) -> Result<(), slint::SetRenderingNotifierError> {
    let weak = ui.as_weak();
    ui.window().set_rendering_notifier(move |rendering_state, api| {
        if !matches!(rendering_state, slint::RenderingState::RenderingSetup) {
            return;
        }
        let mut st = state.borrow_mut();
        if st.texture.is_some() {
            return;
        }

        // 只接受 Slint 真正的 WGPU30 device/queue；其它 API 明确失败，不能
        // 退化成 CPU Image 冒充 GPU 链路验证。
        let slint::GraphicsAPI::WGPU30 { device, queue, .. } = api else {
            st.error = Some(format!(
                "expected Slint GraphicsAPI::WGPU30, got {api:?}; \
                 run with SLINT_WGPU_CPU=1 and require_wgpu_30 (no fake PASS)"
            ));
            st.done = true;
            slint::quit_event_loop().ok();
            return;
        };

        let adapter = device.adapter_info();
        eprintln!(
            "gpu: Slint WGPU30 adapter={:?} backend={:?} device_type={:?}",
            adapter.name, adapter.backend, adapter.device_type
        );

        let (width, height) = (cpu.width, cpu.height);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("xtervo-gpu-spike-terminal-frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // sRGB 变体让 Slint 采样/写回 surface 时与 CPU framebuffer 的
            // sRGB 字节语义一致；两种格式都满足 Image::try_from 的契约。
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
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
            &cpu.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        match slint::Image::try_from(texture.clone()) {
            Ok(image) => {
                let Some(ui) = weak.upgrade() else { return };
                let window_size = ui.window().size();
                eprintln!(
                    "gpu: uploaded {width}x{height} Rgba8UnormSrgb from PTY framebuffer; \
                     window={}x{}",
                    window_size.width, window_size.height
                );
                ui.set_frame(image);
                ui.window().request_redraw();
                st.texture = Some(texture);
                st.ready = true;
            },
            Err(error) => {
                st.error = Some(format!("slint::Image::try_from(texture) failed: {error}"));
                st.done = true;
                slint::quit_event_loop().ok();
            },
        }
    })
}

fn verify_snapshot(ui: &App, cpu: &CpuFrame) -> Result<String, String> {
    let window_size = ui.window().size();
    let snapshot = ui
        .window()
        .take_snapshot()
        .map_err(|error| format!("Window::take_snapshot failed: {error}"))?;
    let (width, height) = (snapshot.width(), snapshot.height());
    if width != cpu.width || height != cpu.height {
        return Err(format!(
            "snapshot is {width}x{height}, expected CPU framebuffer {}x{} (window {}x{})",
            cpu.width, cpu.height, window_size.width, window_size.height
        ));
    }
    let snap = snapshot.as_bytes();
    if snap.len() != cpu.rgba.len() {
        return Err(format!(
            "snapshot bytes {} != CPU framebuffer bytes {}",
            snap.len(),
            cpu.rgba.len()
        ));
    }

    let out_dir = output_dir();
    std::fs::create_dir_all(&out_dir)
        .map_err(|error| format!("create {}: {error}", out_dir.display()))?;
    let snapshot_path = out_dir.join("slint_snapshot.ppm");
    let cpu_path = out_dir.join("cpu_framebuffer.ppm");
    write_ppm(&snapshot_path, width, height, snap)?;
    write_ppm(&cpu_path, width, height, &cpu.rgba)?;

    // 背景参考取 CPU framebuffer 右下角：PTY 只在顶部 4 行输出，右下角
    // 一定是终端默认背景（光标没有烘焙进 framebuffer）。
    let bg = pixel(&cpu.rgba, width, width - 1, height - 1);

    let total = (width * height) as usize;
    let mut cpu_ink = 0usize;
    let mut snap_ink = 0usize;
    let mut mismatch8 = 0usize;
    let mut mismatch32 = 0usize;
    let mut sum_abs = 0u64;
    let mut max_abs = 0u8;
    // CPU 端“最像墨迹”的像素，逐点核对快照；比整屏均值更能定位漏图/错位。
    let mut strongest: Vec<(usize, [u8; 4], u8)> = Vec::new();

    for i in 0..total {
        let offset = i * 4;
        let c = [
            cpu.rgba[offset],
            cpu.rgba[offset + 1],
            cpu.rgba[offset + 2],
            cpu.rgba[offset + 3],
        ];
        let s = [
            snap[offset],
            snap[offset + 1],
            snap[offset + 2],
            snap[offset + 3],
        ];
        let from_bg = channel_diff(c, bg);
        let diff = channel_diff(c, s);
        if from_bg > 24 {
            cpu_ink += 1;
        }
        if channel_diff(s, bg) > 24 {
            snap_ink += 1;
        }
        if diff > 8 {
            mismatch8 += 1;
        }
        if diff > 32 {
            mismatch32 += 1;
        }
        sum_abs += u64::from(diff);
        max_abs = max_abs.max(diff);
        strongest.push((i, c, from_bg));
    }

    strongest.sort_unstable_by_key(|&(_, _, from_bg)| std::cmp::Reverse(from_bg));
    strongest.truncate(64);

    let mean_abs = sum_abs as f64 / total as f64;
    let pct = |n: usize| (n as f64) * 100.0 / total as f64;

    // ---- 断言 1：真实终端内容确实被 CPU 光栅化（不是全背景） ----
    if cpu_ink < 200 {
        return Err(format!(
            "CPU framebuffer has only {cpu_ink} non-background pixels; PTY frame was not rendered"
        ));
    }
    // ---- 断言 2：GPU 窗口里确实显示了这个 texture（非背景像素数量级一致） ----
    if snap_ink < 200 {
        return Err(format!(
            "snapshot has only {snap_ink} non-background pixels; imported texture not sampled \
             (magenta fallback would also trip this)"
        ));
    }
    let ink_ratio = snap_ink as f64 / cpu_ink as f64;
    if !(0.6..=1.5).contains(&ink_ratio) {
        return Err(format!(
            "snapshot ink/cpu ink ratio {ink_ratio:.3} out of range (snap={snap_ink} cpu={cpu_ink})"
        ));
    }
    // ---- 断言 3：逐像素与 CPU 输入对照 ----
    // sRGB texture 往返只应有 <=1 LSB 的舍入误差；超过 1% 像素差 >8 说明
    // 不是同一份 framebuffer（错位/缩放/颜色空间/未采样）。
    if pct(mismatch8) > 1.0 {
        return Err(format!(
            "pixel mismatch: {:.2}% pixels differ from CPU framebuffer by >8 (mean_abs={mean_abs:.3}, \
             max_abs={max_abs}); see {}",
            pct(mismatch8),
            out_dir.display()
        ));
    }
    if mean_abs > 2.0 {
        return Err(format!(
            "mean abs channel difference {mean_abs:.3} > 2.0; snapshot is not the CPU framebuffer"
        ));
    }
    for &(i, expected, from_bg) in &strongest {
        let offset = i * 4;
        let actual = [
            snap[offset],
            snap[offset + 1],
            snap[offset + 2],
            snap[offset + 3],
        ];
        if channel_diff(actual, expected) > 24 {
            return Err(format!(
                "strong ink pixel {} expected {expected:?} (distance {from_bg} from bg), \
                 snapshot has {actual:?}",
                i
            ));
        }
    }

    // 洋红兜底色不应该出现（`#ff00ff`）。
    let magenta = snap
        .chunks_exact(4)
        .any(|p| p[0] > 200 && p[1] < 40 && p[2] > 200);
    if magenta {
        return Err(format!(
            "snapshot contains the #ff00ff fallback background; Image item was not drawn"
        ));
    }

    Ok(format!(
        "PASS gpu_terminal_spike: real PTY framebuffer {width}x{height} -> Slint WGPU30 texture -> \
         take_snapshot; cpu_ink={cpu_ink} snap_ink={snap_ink} ratio={ink_ratio:.3} \
         mismatch>8={:.3}% mismatch>32={:.3}% mean_abs={mean_abs:.3} max_abs={max_abs} \
         generation={} snapshot={} cpu={}",
        pct(mismatch8),
        pct(mismatch32),
        cpu.generation,
        snapshot_path.display(),
        cpu_path.display(),
    ))
}

fn capture_pty_frame() -> Result<CpuFrame, Box<dyn std::error::Error>> {
    let theme = Theme::default();
    let Some(fonts) = glyph::FontSet::load(FONT_SIZE) else {
        return Err(std::io::Error::other(
            "FontSet::load returned None; cannot rasterize a real terminal framebuffer",
        )
        .into());
    };

    let mut config = PaneConfig {
        cols: TERMINAL_COLS,
        rows: TERMINAL_ROWS,
        scrollback: 64,
        shell: Some("/bin/sh".into()),
        args: vec!["-c".into(), PTY_SCRIPT.into()],
        ..PaneConfig::default()
    };
    config.cell_width = fonts.cell_w as u16;
    config.cell_height = fonts.cell_h as u16;

    let pane = spawn(config)?;
    eprintln!(
        "pty: spawned /bin/sh -c <printf {TERMINAL_COLS}x{TERMINAL_ROWS} ...>; waiting for output"
    );

    let started = Instant::now();
    let mut frame: Box<Frame> = pane.new_frame();
    let (captured, text) = loop {
        frame = pane.pull(frame);
        if !frame.skipped {
            let text = frame_text(&frame);
            if text.contains("XTERVO-GPU-PTY") && text.contains("PTY-FRAME-OK") {
                break (frame, text);
            }
        }
        if started.elapsed() > PTY_TIMEOUT {
            return Err(std::io::Error::other(format!(
                "PTY did not produce the expected frame within {PTY_TIMEOUT:?}; \
                 last text = {text:?}",
                text = frame_text(&frame)
            ))
            .into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let mut renderer = render::Renderer::new(
        Rc::new(fonts),
        captured.cols,
        captured.rows,
        theme.bg(),
        theme.cursor(),
        1.0,
    );
    renderer.draw_without_cursor(&captured);
    let width = renderer.width as u32;
    let height = renderer.height as u32;
    let rgba: Vec<u8> = renderer
        .pixels()
        .iter()
        .flat_map(|pixel| pixel.to_le_bytes())
        .collect();
    if rgba.len() != (width * height * 4) as usize {
        return Err(std::io::Error::other(format!(
            "CPU framebuffer size mismatch: {}x{} -> {} bytes",
            width,
            height,
            rgba.len()
        ))
        .into());
    }

    Ok(CpuFrame {
        width,
        height,
        rgba,
        text,
        generation: captured.generation,
    })
}

fn frame_text(frame: &Frame) -> String {
    let mut out = String::with_capacity(frame.rows * (frame.cols + 1));
    for row in 0..frame.rows {
        for col in 0..frame.cols {
            let cell = frame.cells[row * frame.cols + col];
            out.push(cell.char());
        }
        out.push('\n');
    }
    out
}

fn output_dir() -> PathBuf {
    std::env::var_os("XTERVO_GPU_SPIKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new("target/agent-gpu-spike").to_path_buf())
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let offset = (y * width + x) as usize * 4;
    [
        rgba[offset],
        rgba[offset + 1],
        rgba[offset + 2],
        rgba[offset + 3],
    ]
}

fn channel_diff(a: [u8; 4], b: [u8; 4]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(&x, y)| x.abs_diff(y))
        .max()
        .unwrap_or(0)
}

fn write_ppm(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let mut out = Vec::with_capacity(32 + (width * height * 3) as usize);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for pixel in rgba.chunks_exact(4) {
        out.extend_from_slice(&pixel[..3]);
    }
    std::fs::write(path, out).map_err(|error| format!("write {}: {error}", path.display()))
}
