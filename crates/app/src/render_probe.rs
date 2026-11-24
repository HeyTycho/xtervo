// SPDX-License-Identifier: GPL-3.0-only
//! 渲染后端自检：把 Slint 实际使用的渲染器、GL renderer 和帧耗时写进日志。
//!
//! 为什么需要它：用户机器上 Skia/GL 与软件渲染的 resize 表现完全不同，只靠
//! `SLINT_BACKEND` 环境变量无法证明**实际**跑的是哪条路径（Slint 会在 Skia/GL
//! 初始化失败时静默回退到 software surface 或 winit-software）。这里用 Slint
//! 公开的 `Window::set_rendering_notifier`：
//! - 注册成功 => 实际是 Skia 渲染器；`GraphicsAPI::NativeOpenGL` 回调出现 =>
//!   GL 上下文可用，顺带查 `GL_RENDERER`/`GL_VENDOR`/`GL_VERSION`；
//! - 注册返回 `Unsupported` => 实际是 winit 软件渲染器；
//! - 注册成功但 1.2s 内没有 GL 回调 => Skia 的 CPU software surface（GPU 不可用，
//!   headless 环境不能据此声称验证了 GPU）。
//!
//! GL 查询只通过 Slint 文档化的 `get_proc_address` 取 `glGetString`，检查空指针
//! 后调用；不猜测上下文、不导入纹理、不保留任何 GL 资源。

use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use slint::{GraphicsAPI, RenderingState, SetRenderingNotifierError};

/// 实际拿到了 GPU 图形 API（OpenGL/WGPU 等）。Skia software surface 永远不会设置。
static GPU_CONFIRMED: AtomicBool = AtomicBool::new(false);
/// 渲染通知器注册成功 = Slint 确实选了 Skia 渲染器（不是 winit-software）。
static NOTIFIER_REGISTERED: AtomicBool = AtomicBool::new(false);
/// 实际发生的渲染帧数。Skia software surface 在 Slint 1.18 不触发通知器（frames=0），
/// 而 wgpu surface 可能已渲染但拿不到 GraphicsAPI 回调；用它区分，避免误回退。
static RENDER_FRAMES: AtomicU64 = AtomicU64::new(0);
static BACKEND_LABEL: std::sync::OnceLock<std::sync::Mutex<String>> = std::sync::OnceLock::new();

pub fn gpu_confirmed() -> bool {
    GPU_CONFIRMED.load(Ordering::Relaxed)
}

pub fn notifier_registered() -> bool {
    NOTIFIER_REGISTERED.load(Ordering::Relaxed)
}

pub fn frames() -> u64 {
    RENDER_FRAMES.load(Ordering::Relaxed)
}

/// 实际渲染器标签，供 HUD/诊断显示（由 RenderingSetup 写入）。
pub fn backend_label() -> String {
    BACKEND_LABEL
        .get()
        .and_then(|m| m.lock().ok().map(|s| s.clone()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn set_backend_label(label: &str) {
    let mutex = BACKEND_LABEL.get_or_init(|| std::sync::Mutex::new(label.to_string()));
    if let Ok(mut guard) = mutex.lock() {
        *guard = label.to_string();
    }
}

const GL_VENDOR: u32 = 0x1F00;
const GL_RENDERER: u32 = 0x1F01;
const GL_VERSION: u32 = 0x1F02;

/// `XTERVO_RENDER_PROFILE=1`：每帧打印渲染通知器测得的帧间隔/渲染耗时。
fn frame_profile() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XTERVO_RENDER_PROFILE").is_some())
}

#[derive(Default)]
struct ProbeState {
    gl_reported: bool,
    frames: u64,
    last_frame: Option<Instant>,
    before: Option<Instant>,
}

/// 在 `ui.show()` 之前安装自检；不会改变渲染行为。
pub fn install(window: &slint::Window, requested: &str, fell_back: bool) {
    install_for(crate::runtime::WindowId::FIRST, window, requested, fell_back);
}

/// 多窗口版本：RenderingSetup/Teardown 必须带上 WindowId，GPU 状态才能按窗口
/// 引用计数。关闭 B 的 teardown 只移除 B，不会让 A 掉回 CPU。
pub fn install_for(
    window_id: crate::runtime::WindowId,
    window: &slint::Window,
    requested: &str,
    fell_back: bool,
) {
    let state = Rc::new(RefCell::new(ProbeState::default()));
    let callback_state = state.clone();
    let result = window.set_rendering_notifier(move |rendering_state, api| {
        let mut st = callback_state.borrow_mut();
        match rendering_state {
            RenderingState::RenderingSetup => {
                match api {
                    GraphicsAPI::NativeOpenGL { get_proc_address } => {
                        set_backend_label("skia-gl");
                        report_gl(get_proc_address, &mut st);
                    },
                    GraphicsAPI::WGPU30 { device, queue, .. } => {
                        // XTERVO_GPU=1 / XTERVO_GPU_NATIVE=1：Slint 的 WGPU30 API
                        // 把它实际使用的同一 device/queue 交出来，两个链路共用；
                        // gpu_present 只存 device、gpu_native 惰性建 atlas。
                        st.gl_reported = true;
                        set_backend_label("wgpu30");
                        GPU_CONFIRMED.store(true, Ordering::Relaxed);
                        eprintln!(
                            "[render] {window_id} actual_renderer=wgpu30（Slint WGPU30；\
                             device/queue 与 GPU 终端链路同源）"
                        );
                        crate::gpu_present::install_for(window_id, device, queue);
                        crate::gpu_native::install_for(window_id, device, queue);
                    },
                    _ if !st.gl_reported => {
                        // 其他 GPU surface：没有 GL_RENDERER 可读，但已经拿到 GPU
                        // surface，不能误判成 software。
                        st.gl_reported = true;
                        set_backend_label("skia-gpu");
                        GPU_CONFIRMED.store(true, Ordering::Relaxed);
                        let kind = format!("{api:?}");
                        eprintln!("[render] actual_renderer=skia-gpu（{kind}；非 OpenGL，读不到 GL_RENDERER）");
                    },
                    _ => {},
                }
            },
            RenderingState::BeforeRendering => {
                let now = Instant::now();
                let gap = st.last_frame.map(|t| now.duration_since(t));
                st.last_frame = Some(now);
                st.before = Some(now);
                st.frames += 1;
                RENDER_FRAMES.store(st.frames, Ordering::Relaxed);
                // A texture imported before Slint's first WGPU present can
                // stay unsampled (empty pane) until a later redraw. Once the
                // first WGPU frame exists, request one fresh terminal frame so
                // the import happens after a real present.
                if st.frames == 1 && crate::gpu_native::ready() {
                    slint::Timer::single_shot(Duration::from_millis(0), crate::redraw_all);
                }
                if frame_profile() {
                    eprintln!(
                        "[render] frame={} gap={}ms",
                        st.frames,
                        gap.map(|d| format!("{:.2}", d.as_secs_f64() * 1000.0))
                            .unwrap_or_else(|| "-".into())
                    );
                }
            },
            RenderingState::AfterRendering => {
                if let Some(before) = st.before.take() {
                    if frame_profile() {
                        eprintln!(
                            "[render] frame={} render={:.2}ms",
                            st.frames,
                            before.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                }
            },
            RenderingState::RenderingTeardown => {
                crate::gpu_present::teardown_for(window_id);
                crate::gpu_native::teardown_for(window_id);
                eprintln!("[render] {window_id} rendering teardown frame={}", st.frames);
            },
            _ => {},
        }
    });

    match result {
        Ok(()) => {
            NOTIFIER_REGISTERED.store(true, Ordering::Relaxed);
            eprintln!(
                "[render] requested_backend={requested}{}；notifier=已安装（Skia），等待 GL 上下文自检",
                if fell_back { "（已从失败的后端回退）" } else { "" }
            );
            // 某些 Skia software surface 不会触发 RenderingSetup/NativeOpenGL；
            // 1.2s 后若仍未拿到 GL 信息，明确写“没验证到 GPU”，不能把软件 surface
            // 当成 GPU 性能数据。
            let state = state.clone();
            slint::Timer::single_shot(Duration::from_millis(1200), move || {
                let st = state.borrow();
                if !st.gl_reported {
                    if st.frames == 0 {
                        eprintln!(
                            "[render] actual_renderer=skia-software-surface（未收到 GL 上下文、\
                             也没有渲染帧；headless/无 GPU 环境不能验证 GPU 性能）frames=0"
                        );
                    } else {
                        // Slint 1.18 没启用 unstable-wgpu-30 时，wgpu surface 不暴露
                        // GraphicsAPI 回调，但确实在出帧；不能当 software 黑屏回退。
                        eprintln!(
                            "[render] actual_renderer=skia-gpu-or-software?（未拿到 GraphicsAPI \
                             回调，但已有 {} 帧渲染；可能是 wgpu；读不到 GL_RENDERER）",
                            st.frames
                        );
                    }
                }
            });
        },
        Err(SetRenderingNotifierError::Unsupported) => {
            eprintln!(
                "[render] requested_backend={requested}{}；actual_renderer=winit-software\
                 （GL/GPU 不可用或已显式回退；headless 不能验证 GPU）",
                if fell_back { "（已从失败的后端回退）" } else { "" }
            );
        },
        Err(SetRenderingNotifierError::AlreadySet) => {
            eprintln!("[render] requested_backend={requested}；notifier 已被其他代码占用");
        },
        _ => {
            eprintln!("[render] requested_backend={requested}；notifier 安装失败（未知原因）");
        },
    }
}

fn report_gl(get_proc_address: &dyn Fn(&CStr) -> *const c_void, st: &mut ProbeState) {
    if st.gl_reported {
        return;
    }
    match query_gl_strings(get_proc_address) {
        Some((vendor, renderer, version)) => {
            st.gl_reported = true;
            GPU_CONFIRMED.store(true, Ordering::Relaxed);
            eprintln!("[render] actual_renderer=skia-opengl");
            eprintln!("[render] GL_VENDOR={vendor:?}");
            eprintln!("[render] GL_RENDERER={renderer:?}");
            eprintln!("[render] GL_VERSION={version:?}");
            if renderer.to_ascii_lowercase().contains("llvmpipe")
                || renderer.to_ascii_lowercase().contains("softpipe")
            {
                eprintln!(
                    "[render] 注意：GL_RENDERER 是 CPU 光栅（llvmpipe/softpipe），\
                     这不是 NVIDIA/真 GPU 的性能证据"
                );
            }
        },
        None => {
            st.gl_reported = true;
            GPU_CONFIRMED.store(true, Ordering::Relaxed);
            eprintln!(
                "[render] actual_renderer=skia-opengl（NativeOpenGL 已确认，但 glGetString 不可用，\
                 读不到 GL_RENDERER）"
            );
        },
    }
}

type GlGetString = unsafe extern "system" fn(u32) -> *const c_char;

/// 只取 `glGetString` 一个入口，调用前检查非空；不创建上下文、不持有 GL 资源。
fn query_gl_strings(
    get_proc_address: &dyn Fn(&CStr) -> *const c_void,
) -> Option<(String, String, String)> {
    let name = c"glGetString";
    let sym = get_proc_address(name);
    if sym.is_null() {
        return None;
    }
    // SAFETY: `get_proc_address` 是 Slint 在 OpenGL rendering notifier 中提供的
    // 文档化入口；`glGetString` 的 ABI 与签名在 GL 规范中固定。这里只读字符串，
    // 不改变 GL 状态。
    let get_string: GlGetString = unsafe { std::mem::transmute(sym) };
    unsafe fn read(get_string: GlGetString, which: u32) -> Option<String> {
        // SAFETY: 同上；glGetString 返回以 NUL 结尾的静态字符串，可能在无上下文时为空。
        let ptr = unsafe { get_string(which) };
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
    }
    // SAFETY: 仅调用上面校验过的 glGetString。
    unsafe {
        let vendor = read(get_string, GL_VENDOR).unwrap_or_else(|| "(unknown)".into());
        let renderer = read(get_string, GL_RENDERER).unwrap_or_else(|| "(unknown)".into());
        let version = read(get_string, GL_VERSION).unwrap_or_else(|| "(unknown)".into());
        Some((vendor, renderer, version))
    }
}
