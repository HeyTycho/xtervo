// SPDX-License-Identifier: GPL-3.0-only
//! `TERVO_GPU_NATIVE=1`：真实终端 `Frame` → `tervo_scene::Scene` → WGPU 分层合成。
//!
//! 这是默认 GPU（Skia/WGPU30）上的核心 vertical slice，**不是**默认路径：
//! - 只有显式 `TERVO_GPU_NATIVE=1` 才启用；`TERVO_GPU=1` 的 stage 0
//!   CPU framebuffer 纹理上传保持原样，两者互不替代。
//! - Device/Queue 来自 Slint `RenderingSetup` 的 `GraphicsAPI::WGPU30`
//!   （`render_probe` 安装，和 `gpu_present` 共用同源 device）。
//! - 文字来源是 `SceneBuilder::from_frame` 产出的 `TextBatch`，由
//!   `tervo_render_wgpu::GpuTextRenderer` 的 glyph atlas 实例化绘制到离屏
//!   `Rgba8Unorm`；**绝不读取 CPU 终端 framebuffer 当 GPU 文字源**。
//! - 图片来自 `Frame.images`：每个 `source_id` 只 `upload_source` 一次，随后
//!   `retain_sources`；`GpuImageRenderer::render_scene` 按
//!   background -> below-text images -> glyphs -> above-text images 顺序合成。
//! - 光标继续走现有 Slint overlay（明确保留，不在本 slice 合成）。
//! - 任何失败（无 device / atlas / image upload / 渲染 / 尺寸异常）只记录一次，
//!   本进程剩余时间返回 `None`，调用方立即回到 CPU `Renderer::image()`。
//!   不会把 CPU framebuffer 包装成 GPU 成功。
//!
//! 诊断：
//! - `TERVO_GPU_NATIVE_DEBUG=1`：每帧打印 draw calls/上传统计。
//! - `TERVO_GPU_NATIVE_DUMP=<prefix>`：每个 pane 首帧做一次只读回读，写 PPM
//!   作为像素证据（仅验证用；正常渲染路径不做 GPU→CPU 整帧拷贝）。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use slint::wgpu_30::wgpu;
use tervo_core::{Frame, Span};

use crate::runtime::WindowId;
use tervo_graphics::SourceId;
use tervo_render_wgpu::{
    read_target_rgba8, write_ppm, AtlasConfig, GpuImageRenderer, GpuTextRenderer, SceneLayerStats,
    TextLayout, TextSection, TextTarget,
};
use tervo_scene::{
    BackgroundInstance, DamageRegion, DrawItem, PhysicalSize, Scene, SceneBuilder, Viewport,
    BACKGROUND_LAYER_Z,
};

/// GPU-native 默认启用；`TERVO_CPU=1` / `TERVO_GPU=0` / `TERVO_GPU_NATIVE=0` 关闭。
pub fn requested() -> bool {
    static REQUESTED: OnceLock<bool> = OnceLock::new();
    *REQUESTED.get_or_init(|| {
        if crate::gpu_present::cpu_forced() {
            return false;
        }
        if let Ok(v) = std::env::var("TERVO_GPU") {
            if matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off") {
                return false;
            }
        }
        match std::env::var("TERVO_GPU_NATIVE") {
            Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
            Err(_) => true,
        }
    })
}

fn debug() -> bool {
    static DEBUG: OnceLock<bool> = OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("TERVO_GPU_NATIVE_DEBUG").is_some())
}

fn dump_prefix() -> Option<PathBuf> {
    std::env::var_os("TERVO_GPU_NATIVE_DUMP").map(PathBuf::from)
}

/// One offscreen texture per pane.
struct PaneTarget {
    target: TextTarget,
    frames: u64,
    dump_written: bool,
    last_selection: Option<Span>,
    last_search: Option<Span>,
}

// `slint::Image` is deliberately UI-thread-local (it contains `*mut ()`), so
// imported texture handles live here instead of inside the `Send` state.
thread_local! {
    static PANE_IMAGES: RefCell<HashMap<u64, slint::Image>> = RefCell::new(HashMap::new());
}

struct State {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: GpuTextRenderer,
    images: GpuImageRenderer,
    panes: HashMap<u64, PaneTarget>,
    last_stats: Option<SceneLayerStats>,
    /// 创建 renderer/atlas 时的字体路径；设置页换字体后必须重建。
    font_paths: [String; 4],
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// 出现过不可恢复错误：本进程剩余时间一律回 CPU。
static FAILED: Mutex<bool> = Mutex::new(false);
/// 仍持有共享 WGPU device 的窗口。State 是惰性创建的，不能用 State 是否存在
/// 判断窗口数：第二个窗口 setup 时 State 可能还没建。
static CLIENTS: Mutex<Option<HashSet<WindowId>>> = Mutex::new(None);
/// 每窗口 active pane；retain 必须按全进程并集回收。
static PANES_BY_WINDOW: Mutex<Option<HashMap<WindowId, HashSet<u64>>>> = Mutex::new(None);

fn clients() -> std::sync::MutexGuard<'static, Option<HashSet<WindowId>>> {
    CLIENTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn panes_by_window() -> std::sync::MutexGuard<'static, Option<HashMap<WindowId, HashSet<u64>>>> {
    PANES_BY_WINDOW
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn window_count() -> usize {
    clients().as_ref().map_or(0, HashSet::len)
}

static FRAMES: AtomicU64 = AtomicU64::new(0);
static DRAW_CALLS: AtomicU64 = AtomicU64::new(0);
static ATLAS_UPLOADS: AtomicU64 = AtomicU64::new(0);
static ATLAS_UPLOADED_BYTES: AtomicU64 = AtomicU64::new(0);
static VERTEX_UPLOADED_BYTES: AtomicU64 = AtomicU64::new(0);
static IMAGE_UPLOADS: AtomicU64 = AtomicU64::new(0);
static IMAGE_SOURCES: AtomicU64 = AtomicU64::new(0);
static FULL_FRAMES: AtomicU64 = AtomicU64::new(0);
static PARTIAL_FRAMES: AtomicU64 = AtomicU64::new(0);
static REUSED_FRAMES: AtomicU64 = AtomicU64::new(0);
static DAMAGE_RECTS: AtomicU64 = AtomicU64::new(0);
static PARTIAL_PIXELS: AtomicU64 = AtomicU64::new(0);

/// 进程累计的 GPU-native 统计（用于快照验收/integration 证据）。
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeTotals {
    pub frames: u64,
    pub draw_calls: u64,
    pub atlas_uploads: u64,
    pub atlas_uploaded_bytes: u64,
    pub vertex_uploaded_bytes: u64,
    pub image_uploads: u64,
    pub image_sources: u64,
    pub full_frames: u64,
    pub partial_frames: u64,
    pub reused_frames: u64,
    pub damage_rects: u64,
    pub partial_pixels: u64,
}

pub fn totals() -> NativeTotals {
    NativeTotals {
        frames: FRAMES.load(Ordering::Relaxed),
        draw_calls: DRAW_CALLS.load(Ordering::Relaxed),
        atlas_uploads: ATLAS_UPLOADS.load(Ordering::Relaxed),
        atlas_uploaded_bytes: ATLAS_UPLOADED_BYTES.load(Ordering::Relaxed),
        vertex_uploaded_bytes: VERTEX_UPLOADED_BYTES.load(Ordering::Relaxed),
        image_uploads: IMAGE_UPLOADS.load(Ordering::Relaxed),
        image_sources: IMAGE_SOURCES.load(Ordering::Relaxed),
        full_frames: FULL_FRAMES.load(Ordering::Relaxed),
        partial_frames: PARTIAL_FRAMES.load(Ordering::Relaxed),
        reused_frames: REUSED_FRAMES.load(Ordering::Relaxed),
        damage_rects: DAMAGE_RECTS.load(Ordering::Relaxed),
        partial_pixels: PARTIAL_PIXELS.load(Ordering::Relaxed),
    }
}

/// GPU-native 链路是否可用：显式请求、未失败、且 Slint 已交出 WGPU30 device。
///
/// glyph atlas 在首帧惰性创建，所以这里不要求 `State` 已存在；真实出帧数以
/// `totals().frames` 为准。
pub fn ready() -> bool {
    requested() && !failed() && crate::gpu_present::device_queue().is_some()
}

fn failed() -> bool {
    *FAILED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fail(reason: impl std::fmt::Display) {
    {
        let mut failed = FAILED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *failed {
            return;
        }
        *failed = true;
    }
    eprintln!(
        "[gpu-native] 失败，自动回退 CPU（本进程不再尝试 GPU Scene）: {reason}"
    );
    if let Ok(mut guard) = STATE.lock() {
        *guard = None;
    }
}

/// `RenderingSetup` 里由 `render_probe` 调用，保存 Slint 的同一 device/queue。
///
/// 只保存 device/queue；glyph atlas 在首帧按 pane 的字体/字号惰性创建。
pub fn install(device: &wgpu::Device, queue: &wgpu::Queue) {
    install_for(WindowId::FIRST, device, queue);
}

/// 多窗口版本：同一 device 的 setup 只登记窗口，不清其它窗口的 atlas/target。
pub fn install_for(window: WindowId, device: &wgpu::Device, _queue: &wgpu::Queue) {
    if !requested() || failed() {
        return;
    }
    let mut same_device = false;
    if let Ok(mut guard) = STATE.lock() {
        if let Some(state) = guard.as_mut() {
            if state.device == *device {
                same_device = true;
            } else {
                eprintln!(
                    "[gpu-native] {window} RenderingSetup：device 变化，丢弃旧 atlas/target"
                );
                *guard = None;
                PANE_IMAGES.with(|images| images.borrow_mut().clear());
            }
        }
    }
    {
        let mut clients = clients();
        let set = clients.get_or_insert_with(HashSet::new);
        set.insert(window);
    }
    panes_by_window()
        .get_or_insert_with(HashMap::new)
        .entry(window)
        .or_default();
    if same_device {
        eprintln!(
            "[gpu-native] {window} RenderingSetup：复用同一 device/atlas（{} 个窗口共享）",
            window_count()
        );
        return;
    }
    let info = device.adapter_info();
    eprintln!(
        "[gpu-native] TERVO_GPU_NATIVE=1：{window} 拿到 Slint WGPU30 同一 device/queue；\
         adapter={:?} backend={:?} device_type={:?}",
        info.name, info.backend, info.device_type
    );
    // RenderingSetup can arrive after the terminal's first frame. Importing a
    // texture before Slint's first WGPU present could snapshot as an empty
    // pane until another redraw; request one after setup settles.
    slint::Timer::single_shot(std::time::Duration::from_millis(250), crate::redraw_all);
}

/// 丢弃旧 device（RenderingTeardown / 重建；无窗口上下文兼容路径）。
pub fn teardown() {
    if let Ok(mut guard) = STATE.lock() {
        if guard.is_some() {
            eprintln!("[gpu-native] rendering teardown：丢弃 GPU Scene atlas/target");
        }
        *guard = None;
    }
    PANE_IMAGES.with(|images| images.borrow_mut().clear());
    *clients() = None;
    *panes_by_window() = None;
}

/// 多窗口 teardown：只回收该窗口的 target；最后一个窗口关闭才清全局 atlas。
pub fn teardown_for(window: WindowId) {
    let panes = panes_by_window()
        .get_or_insert_with(HashMap::new)
        .remove(&window)
        .unwrap_or_default();
    let remaining = {
        let mut clients = clients();
        let set = clients.get_or_insert_with(HashSet::new);
        set.remove(&window);
        set.len()
    };
    PANE_IMAGES.with(|images| {
        images
            .borrow_mut()
            .retain(|pane, _| !panes.contains(pane));
    });
    if remaining == 0 {
        if let Ok(mut guard) = STATE.lock() {
            if guard.is_some() {
                eprintln!(
                    "[gpu-native] {window} rendering teardown：最后一个窗口，丢弃 atlas/target"
                );
            }
            *guard = None;
        }
    } else {
        if let Ok(mut guard) = STATE.lock() {
            if let Some(state) = guard.as_mut() {
                state.panes.retain(|pane, _| !panes.contains(pane));
            }
        }
        eprintln!(
            "[gpu-native] {window} rendering teardown：保留 atlas/target（{remaining} 个窗口仍存活）"
        );
    }
}

/// 回收已关闭 pane 的离屏 target，避免全局 HashMap 持有已删除 pane。
pub fn retain(active: &[u64]) {
    if let Ok(mut guard) = STATE.lock() {
        if let Some(state) = guard.as_mut() {
            state.panes.retain(|pane, _| active.contains(pane));
        }
    }
    PANE_IMAGES.with(|images| {
        images
            .borrow_mut()
            .retain(|pane, _| active.contains(pane));
    });
}

/// 多窗口 retain：更新该窗口的 pane 集合后按所有窗口并集回收。
pub fn retain_for(window: WindowId, active: &[u64]) {
    let set: HashSet<u64> = active.iter().copied().collect();
    let union = {
        let mut map = panes_by_window();
        let map = map.get_or_insert_with(HashMap::new);
        map.insert(window, set);
        map.values()
            .flat_map(|panes| panes.iter().copied())
            .collect::<HashSet<_>>()
    };
    if let Ok(mut guard) = STATE.lock() {
        if let Some(state) = guard.as_mut() {
            state.panes.retain(|pane, _| union.contains(pane));
        }
    }
    PANE_IMAGES.with(|images| {
        images
            .borrow_mut()
            .retain(|pane, _| union.contains(pane));
    });
}

/// 一帧 GPU-native 渲染的输入。
pub struct PaneFrame<'a> {
    pub pane: u64,
    pub frame: &'a Frame,
    /// Renderer 的物理像素网格（与 `frame` 必须一致，否则本帧回 CPU）。
    pub cols: usize,
    pub rows: usize,
    pub cell_w: usize,
    pub cell_h: usize,
    pub baseline: i32,
    pub font_size: f32,
    /// [regular, bold, italic, bold_italic]，与 CPU `FontSet` 一致。
    pub font_paths: [&'a str; 4],
    /// 主题默认背景（0x00RRGGBB），同时作为 Scene background/clear。
    pub theme_bg: u32,
}

fn ensure_state(input: &PaneFrame<'_>) -> bool {
    if let Ok(mut guard) = STATE.lock() {
        if let Some(state) = guard.as_mut() {
            let same = state
                .font_paths
                .iter()
                .map(String::as_str)
                .eq(input.font_paths.iter().copied());
            if same {
                return true;
            }
            // 字体切换：保留 device/images/panes，只重建文字 renderer/atlas。
            let Some((device, queue)) = crate::gpu_present::device_queue() else {
                return false;
            };
            let paths = input.font_paths;
            match GpuTextRenderer::with_font_paths(
                device,
                queue,
                std::path::Path::new(paths[0]),
                Some(std::path::Path::new(paths[1])),
                Some(std::path::Path::new(paths[2])),
                Some(std::path::Path::new(paths[3])),
                AtlasConfig::default(),
            ) {
                Ok(renderer) => {
                    state.renderer = renderer;
                    state.font_paths = paths.map(str::to_owned);
                    eprintln!("[gpu-native] 字体切换：已重建 atlas");
                    return true;
                },
                Err(error) => {
                    // 不能在这里调 fail()：它会再次锁 STATE，当前 guard 还活着。
                    eprintln!("[gpu-native] 字体切换重建 atlas 失败: {error}");
                    return false;
                },
            }
        }
    }
    // Device/queue 来自 `gpu_present`（同一个 `RenderingSetup` WGPU30 回调），
    // 保证离屏纹理与 Slint 采样的是同一 device。
    let Some((device, queue)) = crate::gpu_present::device_queue() else {
        return false;
    };

    let regular = std::path::Path::new(input.font_paths[0]);
    let bold = std::path::Path::new(input.font_paths[1]);
    let italic = std::path::Path::new(input.font_paths[2]);
    let bold_italic = std::path::Path::new(input.font_paths[3]);
    let renderer = match GpuTextRenderer::with_font_paths(
        device.clone(),
        queue.clone(),
        regular,
        Some(bold),
        Some(italic),
        Some(bold_italic),
        AtlasConfig::default(),
    ) {
        Ok(renderer) => renderer,
        Err(error) => {
            fail(format!("创建 GpuTextRenderer/atlas 失败: {error}"));
            return false;
        }
    };
    eprintln!(
        "[gpu-native] atlas 就绪：font={} atlas={}x{} R8；fallback 候选 {} 个（按需解析）",
        regular.display(),
        renderer.atlas().width(),
        renderer.atlas().height(),
        renderer.fallback_font_paths().len()
    );
    let images = GpuImageRenderer::new(device.clone(), queue.clone());
    let mut guard = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(State {
        device,
        queue,
        renderer,
        images,
        panes: HashMap::new(),
        last_stats: None,
        font_paths: input.font_paths.map(str::to_owned),
    });
    true
}

fn scene_clear_color(scene: &Scene) -> ([u8; 4], bool) {
    if let Some(layer) = scene.layer(BACKGROUND_LAYER_Z) {
        for item in &layer.items {
            if let DrawItem::Background(background) = item {
                let color = background.color & 0x00ff_ffff;
                return (
                    [
                        ((color >> 16) & 0xff) as u8,
                        ((color >> 8) & 0xff) as u8,
                        (color & 0xff) as u8,
                        255,
                    ],
                    true,
                );
            }
        }
    }
    ([0, 0, 0, 255], false)
}

/// damage 契约：只有已有 target、非 full、无行 damage、无图片变化时，
/// 才能复用上一帧 GPU image；否则必须重绘。
fn can_reuse_gpu_target(frame: &Frame, has_target: bool) -> bool {
    has_target && !frame.full && frame.damage.is_empty() && frame.images.is_empty()
}

/// Resolved selection/search, mirroring the cell colors built by
/// `SceneBuilder::from_frame`.
///
/// These spans are not part of terminal damage: on an idle pane (cursor blink,
/// drag selection, search navigation) `Frame::full == false` and
/// `Frame::damage` is empty while the highlight moved. Keeping this signature
/// per pane is what makes the GPU reuse short-circuit safe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HighlightSignature {
    selection: Option<Span>,
    search: Option<Span>,
}

impl HighlightSignature {
    fn of(frame: &Frame) -> Self {
        Self {
            selection: frame.selection,
            search: frame.search,
        }
    }
}

/// Decision for one incoming frame: hand back the cached image or repaint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderPlan {
    /// Cached image already matches the frame; no GPU work needed.
    Reuse,
    /// Repaint the target. `force_full` overrides the frame's damage because
    /// the current pixels cannot be incrementally repaired (first frame,
    /// resize/font-size change, highlight transition, missing image).
    Render { force_full: bool },
}

/// Selection/search are cell colors, not terminal damage. The GPU target may
/// only be reused when those resolved spans did not change; otherwise the old
/// texture would keep the previous highlight forever on an idle pane.
fn selection_or_search_changed(previous: HighlightSignature, frame: &Frame) -> bool {
    previous != HighlightSignature::of(frame)
}

/// Pure reuse decision behind [`render_pane_image`].
///
/// `previous` is the highlight signature of the frame whose pixels are in the
/// cached target (`None` = no valid target), `has_image` whether Slint still
/// holds that texture, and `target_size_matches` whether the cached target has
/// the size this frame needs. Any mismatch prevents reuse **and** forces a
/// full repaint so stale pixels are never patched over.
fn plan_render(
    previous: Option<HighlightSignature>,
    has_image: bool,
    target_size_matches: bool,
    frame: &Frame,
) -> RenderPlan {
    let has_target = previous.is_some();
    let changed = previous.is_some_and(|previous| selection_or_search_changed(previous, frame));
    if has_target
        && has_image
        && target_size_matches
        && !changed
        && can_reuse_gpu_target(frame, has_target)
    {
        return RenderPlan::Reuse;
    }
    RenderPlan::Render {
        // A target with unknown/old content can never take the partial path.
        force_full: !has_target || !has_image || !target_size_matches || changed,
    }
}

/// Encode a forced full repaint in the scene damage consumed by
/// `GpuImageRenderer::render_scene`.
///
/// Selection/search pixels are not represented in `Frame::damage`, and a
/// target that was never fully painted has no valid undamaged pixels. Cell
/// damage cannot express either case, so the only correct encoding is a full
/// scene invalidation (an empty damage region would also take the full path,
/// but relying on that would hide real partial damage).
fn apply_render_plan_damage(scene: &mut Scene, force_full: bool) {
    if force_full {
        scene.damage = DamageRegion::full();
    }
}

/// Drop one pane's GPU target and imported image after a CPU fallback.
///
/// The caller only falls back when [`pane_image`] returned `None`; the GPU
/// texture is then older than the frame on screen. Dropping the target makes
/// the next GPU frame rebuild it from scratch instead of reusing stale pixels
/// or painting partial damage over them.
fn invalidate_pane(pane: u64) {
    let mut dropped_target = false;
    {
        let mut guard = STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = guard.as_mut() {
            dropped_target = state.panes.remove(&pane).is_some();
        }
    }
    let dropped_image = PANE_IMAGES.with(|images| images.borrow_mut().remove(&pane).is_some());
    if (dropped_target || dropped_image) && debug() {
        eprintln!(
            "[gpu-native] pane={pane} 回退 CPU：丢弃过期 GPU target（下一帧强制 full）"
        );
    }
}

/// 把真实 `Frame` 走 Scene → WGPU 分层合成，返回可交给 `PaneView.image` 的
/// `slint::Image`。返回 `None` 时调用方必须使用 CPU `Renderer::image()`。
pub fn pane_image(input: &PaneFrame<'_>) -> Option<slint::Image> {
    let image = render_pane_image(input);
    if image.is_none() {
        // CPU fallback means this frame's pixels never reached the cached GPU
        // target. Invalidate it so a later clean frame cannot reuse it.
        invalidate_pane(input.pane);
    }
    image
}

fn render_pane_image(input: &PaneFrame<'_>) -> Option<slint::Image> {
    if !requested() || failed() {
        return None;
    }
    if input.frame.cols != input.cols || input.frame.rows != input.rows {
        // 布局/PTY resize 的中间态：本帧不冒险用错网格，回 CPU。
        return None;
    }
    if input.cell_w == 0 || input.cell_h == 0 {
        return None;
    }
    if !ensure_state(input) {
        return None;
    }

    let width = (input.cols * input.cell_w) as u32;
    let height = (input.rows * input.cell_h) as u32;
    if width == 0 || height == 0 {
        return None;
    }

    let viewport = Viewport {
        cell_w: input.cell_w as f32,
        cell_h: input.cell_h as f32,
        cols: input.cols,
        rows: input.rows,
    };
    let mut scene = SceneBuilder::from_frame(input.frame, viewport)
        .source_namespace(input.pane)
        .background(BackgroundInstance {
            color: input.theme_bg & 0x00ff_ffff,
        })
        .build();
    let layout = TextLayout {
        origin_x: 0.0,
        origin_y: 0.0,
        cell_width: input.cell_w as f32,
        cell_height: input.cell_h as f32,
        font_size_px: input.font_size.round().clamp(1.0, u16::MAX as f32) as u16,
        baseline: input.baseline as f32,
        default_background: Some(input.theme_bg & 0x00ff_ffff),
    };
    let (clear, background_from_scene) = scene_clear_color(&scene);

    let mut guard = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let state = guard.as_mut()?;

    let target_size = PhysicalSize { width, height };
    let has_image = PANE_IMAGES.with(|images| images.borrow().contains_key(&input.pane));
    let (previous, target_size_matches) = state
        .panes
        .get(&input.pane)
        .map(|pane| {
            (
                Some(HighlightSignature {
                    selection: pane.last_selection,
                    search: pane.last_search,
                }),
                pane.target.size() == target_size,
            )
        })
        .unwrap_or((None, false));
    if let Some(previous) = previous {
        if selection_or_search_changed(previous, input.frame) {
            // Always-on diagnostic: one line per selection/search transition, never
            // per drag cell. This is enough to tell a missed frame from a missed
            // mouse event without asking the user to restart with env flags.
            eprintln!(
                "[select] pane={} selection={:?}->{:?} search={:?}->{:?} force_full",
                input.pane,
                previous.selection,
                input.frame.selection,
                previous.search,
                input.frame.search,
            );
        }
    }

    // 光标闪烁/纯 overlay redraw 没有终端 damage：不要重绘整张 target。
    // 只有已有可复用 GPU image 时才跳过；首帧、resize、图片帧、选区/search
    // 变化、以及回退 CPU 后的旧 target 仍走完整路径。
    let force_full = match plan_render(previous, has_image, target_size_matches, input.frame) {
        RenderPlan::Reuse => {
            if let Some(image) =
                PANE_IMAGES.with(|images| images.borrow().get(&input.pane).cloned())
            {
                REUSED_FRAMES.fetch_add(1, Ordering::Relaxed);
                return Some(image);
            }
            // The image vanished between the check and the lookup; repaint it
            // from scratch instead of partially patching the target.
            true
        },
        RenderPlan::Render { force_full } => force_full,
    };

    // Upload each immutable Frame image source at most once, then drop sources
    // that are no longer visible. This is the only image pixel upload; no
    // per-frame full-image copy.
    let mut visible_sources: Vec<SourceId> = Vec::with_capacity(input.frame.images.len());
    for image in &input.frame.images {
        if image.width == 0 || image.height == 0 {
            continue;
        }
        // 每个 pane 的 ImageStore 都从 1 开始分配 source_id；GPU 纹理注册表是
        // 进程级共享的，必须把 pane id 拼进高位，否则两个 pane 的第一张图撞车。
        let source = SourceId((input.pane << 32) | (image.source_id & 0xffff_ffff));
        let expected = image.width as usize * image.height as usize * 4;
        if image.data.len() != expected {
            drop(guard);
            fail(format!(
                "pane={} image source={} has {} bytes, expected {expected}",
                input.pane,
                image.source_id,
                image.data.len()
            ));
            return None;
        }
        match state
            .images
            .upload_source(source, image.width, image.height, &image.data)
        {
            Ok(uploaded) => {
                if uploaded {
                    IMAGE_UPLOADS.fetch_add(1, Ordering::Relaxed);
                }
                visible_sources.push(source);
            }
            Err(error) => {
                drop(guard);
                fail(format!(
                    "pane={} 上传 image source={} 失败: {error}",
                    input.pane, image.source_id
                ));
                return None;
            }
        }
    }
    state
        .images
        .retain_sources(|source| visible_sources.contains(&source));
    IMAGE_SOURCES.store(visible_sources.len() as u64, Ordering::Relaxed);

    // A missing target or a target for another size/font-size cannot be partial
    // painted. This mirrors `plan_render`'s `target_size_matches`.
    let recreate = !target_size_matches;
    if recreate {
        let target = match TextTarget::new(&state.device, target_size) {
            Ok(target) => target,
            Err(error) => {
                drop(guard);
                fail(format!("创建离屏 {width}x{height} Rgba8Unorm target 失败: {error}"));
                return None;
            }
        };
        state.panes.insert(
            input.pane,
            PaneTarget {
                target,
                frames: 0,
                dump_written: false,
                last_selection: None,
                last_search: None,
            },
        );
    }

    // A fresh target has no previous pixels, and a highlight transition has no
    // terminal line damage describing the old pixels. Never take the partial
    // path in either case, even if the incoming Frame reports a small damage
    // region (or none at all).
    apply_render_plan_damage(&mut scene, recreate || force_full);

    let section;
    let sections = match scene.text_batch() {
        Some(batch) => {
            section = TextSection::new(batch, layout);
            std::slice::from_ref(&section)
        }
        None => &[],
    };

    let partial_rect = if !scene.damage.full {
        scene.damage.bounds().filter(|rect| !rect.is_empty())
    } else {
        None
    };

    let stats = {
        let pane = state.panes.get_mut(&input.pane)?;
        match state.images.render_scene(
            &mut state.renderer,
            &pane.target,
            clear,
            sections,
            &scene,
        ) {
            Ok(stats) => stats,
            Err(error) => {
                drop(guard);
                fail(format!(
                    "pane={} GPU Scene 分层渲染失败: {error}",
                    input.pane
                ));
                return None;
            }
        }
    };
    let text = stats
        .text
        .as_ref()
        .expect("successful Scene composition includes text stats");
    let draw_calls = text.draw_calls + stats.below.draw_calls + stats.above.draw_calls;
    let render_passes = text.render_passes + stats.below.render_passes + stats.above.render_passes;
    let image_quads = stats.below.images + stats.above.images;

    if let Some(rect) = partial_rect {
        PARTIAL_FRAMES.fetch_add(1, Ordering::Relaxed);
        DAMAGE_RECTS.fetch_add(scene.damage.rects.len() as u64, Ordering::Relaxed);
        PARTIAL_PIXELS.fetch_add(u64::from(rect.w) * u64::from(rect.h), Ordering::Relaxed);
    } else {
        FULL_FRAMES.fetch_add(1, Ordering::Relaxed);
    }
    FRAMES.fetch_add(1, Ordering::Relaxed);
    DRAW_CALLS.fetch_add(u64::from(draw_calls), Ordering::Relaxed);
    ATLAS_UPLOADS.fetch_add(text.atlas_uploads, Ordering::Relaxed);
    ATLAS_UPLOADED_BYTES.fetch_add(text.atlas_uploaded_bytes, Ordering::Relaxed);
    VERTEX_UPLOADED_BYTES.fetch_add(text.vertex_uploaded_bytes, Ordering::Relaxed);
    VERTEX_UPLOADED_BYTES.fetch_add(stats.below.vertex_uploaded_bytes, Ordering::Relaxed);
    VERTEX_UPLOADED_BYTES.fetch_add(stats.above.vertex_uploaded_bytes, Ordering::Relaxed);

    let pane = state.panes.get_mut(&input.pane)?;
    pane.frames += 1;
    let first = pane.frames == 1;
    // Re-import after rendering. Slint's Skia/WGPU bridge is known to show an
    // empty image when a freshly created texture is imported before its first
    // render pass; importing after the submit matches the stage-0 path that is
    // proven to snapshot correctly. This wraps the live texture (no pixel copy).
    let image = match slint::Image::try_from(pane.target.texture().clone()) {
        Ok(image) => image,
        Err(error) => {
            drop(guard);
            fail(format!("slint::Image::try_from 拒绝 GPU target: {error}"));
            return None;
        }
    };
    PANE_IMAGES.with(|images| {
        images.borrow_mut().insert(input.pane, image.clone());
    });
    // Commit the highlight signature only now: it describes exactly the frame
    // whose pixels were rendered into `pane.target` and imported above. Every
    // earlier `None` return keeps the previous signature (or invalidates it),
    // so a later reuse decision can never claim a highlight was drawn when it
    // was not.
    pane.last_selection = input.frame.selection;
    pane.last_search = input.frame.search;

    if first || debug() {
        eprintln!(
            "[gpu-native] pane={} frame={} rev={} grid={}x{} target={}x{} \
             draw_calls={} passes={} cells={} solids={} glyphs={} \
             atlas_uploads={} atlas_bytes={} vertex_bytes={} \
             fallback_glyphs={} missing_glyphs={} style_fallbacks={} \
             wide={} spacer={} hidden={} dim={} selected={} searched={} \
             image_quads={} image_draw_calls={} image_uploads_total={} image_sources={} \
             frame_images={} scene_images={} cursor_overlay_external=true \
             background_from_scene={} cpu_us={}",
            input.pane,
            text.frame,
            scene.revision,
            input.cols,
            input.rows,
            width,
            height,
            draw_calls,
            render_passes,
            text.cells,
            text.solid_quads,
            text.glyph_quads,
            text.atlas_uploads,
            text.atlas_uploaded_bytes,
            text.vertex_uploaded_bytes
                + stats.below.vertex_uploaded_bytes
                + stats.above.vertex_uploaded_bytes,
            text.fallback_glyphs,
            text.missing_glyphs,
            text.style_fallbacks,
            text.wide_cells,
            text.wide_spacer_cells,
            text.hidden_cells,
            text.dim_cells,
            text.selected_cells,
            text.searched_cells,
            image_quads,
            stats.below.draw_calls + stats.above.draw_calls,
            IMAGE_UPLOADS.load(Ordering::Relaxed),
            IMAGE_SOURCES.load(Ordering::Relaxed),
            input.frame.images.len(),
            scene.image_count(),
            background_from_scene,
            text.cpu_time.as_micros()
                + stats.below.cpu_time.as_micros()
                + stats.above.cpu_time.as_micros()
        );
    }

    // One-shot validation readback (only when explicitly requested; never in
    // the normal present path).
    if first && !pane.dump_written {
        if let Some(prefix) = dump_prefix() {
            pane.dump_written = true;
            let device = state.device.clone();
            let queue = state.queue.clone();
            let mut path = prefix.into_os_string();
            path.push(format!("-pane{}-{}x{}.ppm", input.pane, width, height));
            let path = PathBuf::from(path);
            match read_target_rgba8(&device, &queue, &pane.target) {
                Ok(pixels) => {
                    // PPM drops alpha; keep the raw RGBA next to it so the
                    // evidence can prove the composited target is opaque.
                    let mut raw_path = path.clone().into_os_string();
                    raw_path.push(".rgba");
                    let raw_path = PathBuf::from(raw_path);
                    let alpha_min = pixels.chunks_exact(4).map(|p| p[3]).min().unwrap_or(0);
                    let alpha_max = pixels.chunks_exact(4).map(|p| p[3]).max().unwrap_or(0);
                    let translucent = pixels.chunks_exact(4).filter(|p| p[3] != 255).count();
                    let _ = std::fs::write(&raw_path, &pixels);
                    match write_ppm(&path, width, height, &pixels) {
                        Ok(()) => eprintln!(
                            "[gpu-native] 像素证据（一次性只读回读）path={} raw={} \
                             size={width}x{height} alpha_min={alpha_min} alpha_max={alpha_max} \
                             non_opaque_pixels={translucent}",
                            path.display(),
                            raw_path.display()
                        ),
                        Err(error) => eprintln!(
                            "[gpu-native] 写像素证据失败 path={} error={error}",
                            path.display()
                        ),
                    }
                },
                Err(error) => {
                    eprintln!("[gpu-native] 像素证据回读失败: {error}");
                }
            }
        }
    }

    state.last_stats = Some(stats);
    Some(image)
}

/// 最近一帧 GPU-native 分层统计（快照验收日志用）。
pub fn last_stats() -> Option<SceneLayerStats> {
    STATE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|state| state.last_stats.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tervo_core::{flags, CellData, LineDamage, Span};

    fn span(start_line: u16, start_col: u16, end_line: u16, end_col: u16) -> Span {
        Span {
            start_line,
            start_col,
            end_line,
            end_col,
        }
    }

    fn clean_frame() -> Frame {
        Frame::new(8, 4)
    }

    fn signature(selection: Option<Span>, search: Option<Span>) -> HighlightSignature {
        HighlightSignature { selection, search }
    }

    const SIZE: PhysicalSize = PhysicalSize {
        width: 80,
        height: 40,
    };

    #[test]
    fn damage_contract_reuses_only_clean_non_full_frames() {
        let mut frame = Frame::new(4, 2);
        assert!(!can_reuse_gpu_target(&frame, false), "没有 target 不能复用");
        assert!(can_reuse_gpu_target(&frame, true), "无 damage 且非 full 应复用");

        frame.full = true;
        assert!(!can_reuse_gpu_target(&frame, true), "full 必须重绘");

        frame.full = false;
        frame.damage.push(LineDamage {
            line: 1,
            left: 0,
            right: 3,
        });
        assert!(!can_reuse_gpu_target(&frame, true), "有 damage 必须重绘");
    }

    #[test]
    fn selection_and_search_changes_break_gpu_target_reuse() {
        let mut frame = clean_frame();
        let selection = span(0, 1, 0, 2);
        let empty = signature(None, None);
        assert!(!selection_or_search_changed(empty, &frame));
        frame.selection = Some(selection);
        assert!(selection_or_search_changed(empty, &frame));
        assert!(!selection_or_search_changed(
            signature(Some(selection), None),
            &frame
        ));
        frame.selection = None;
        assert!(selection_or_search_changed(
            signature(Some(selection), None),
            &frame
        ));
        frame.search = Some(selection);
        assert!(selection_or_search_changed(empty, &frame));
        assert!(!selection_or_search_changed(
            signature(None, Some(selection)),
            &frame
        ));
        // Same rows, different end column: still a different highlight.
        frame.search = Some(span(0, 1, 1, 2));
        assert!(selection_or_search_changed(
            signature(None, Some(selection)),
            &frame
        ));
    }

    #[test]
    fn plan_reuses_only_for_identical_highlight_and_valid_target() {
        let frame = clean_frame();
        assert_eq!(
            plan_render(Some(HighlightSignature::default()), true, true, &frame),
            RenderPlan::Reuse,
            "clean frame + unchanged (empty) highlight must reuse"
        );

        let mut highlighted = clean_frame();
        highlighted.selection = Some(span(1, 0, 1, 2));
        highlighted.search = Some(span(2, 1, 2, 2));
        assert_eq!(
            plan_render(
                Some(HighlightSignature::of(&highlighted)),
                true,
                true,
                &highlighted
            ),
            RenderPlan::Reuse,
            "an unchanged selection/search must still take the reuse path"
        );
    }

    #[test]
    fn selection_change_and_clear_force_full_and_never_reuse() {
        let previous = signature(Some(span(0, 0, 0, 1)), None);

        let mut started = clean_frame();
        started.selection = Some(span(0, 0, 0, 1));
        assert_eq!(
            plan_render(Some(signature(None, None)), true, true, &started),
            RenderPlan::Render { force_full: true },
            "None -> Some selection must repaint"
        );

        let mut moved = clean_frame();
        moved.selection = Some(span(0, 1, 0, 2));
        assert_eq!(
            plan_render(Some(previous), true, true, &moved),
            RenderPlan::Render { force_full: true },
            "moving a selection must repaint"
        );

        let cleared = clean_frame();
        assert_eq!(
            plan_render(Some(previous), true, true, &cleared),
            RenderPlan::Render { force_full: true },
            "clearing a selection is a highlight transition too"
        );
    }

    #[test]
    fn search_change_and_clear_force_full_and_never_reuse() {
        let previous = signature(None, Some(span(2, 0, 2, 2)));

        let mut moved = clean_frame();
        moved.search = Some(span(2, 1, 2, 3));
        assert_eq!(
            plan_render(Some(previous), true, true, &moved),
            RenderPlan::Render { force_full: true },
            "moving the search match must repaint"
        );

        let cleared = clean_frame();
        assert_eq!(
            plan_render(Some(previous), true, true, &cleared),
            RenderPlan::Render { force_full: true },
            "clearing the search highlight is a highlight transition too"
        );
    }

    #[test]
    fn first_frame_missing_image_and_wrong_size_force_full() {
        let frame = clean_frame();
        assert_eq!(
            plan_render(None, false, false, &frame),
            RenderPlan::Render { force_full: true },
            "the first frame has no previous pixels"
        );
        assert_eq!(
            plan_render(
                Some(HighlightSignature::of(&frame)),
                false,
                true,
                &frame
            ),
            RenderPlan::Render { force_full: true },
            "a target without an imported image must be rebuilt"
        );
        assert_eq!(
            plan_render(
                Some(HighlightSignature::of(&frame)),
                true,
                false,
                &frame
            ),
            RenderPlan::Render { force_full: true },
            "resize/font-size change: cached pixels belong to another target"
        );
    }

    #[test]
    fn unchanged_highlight_keeps_partial_damage_path() {
        let mut frame = clean_frame();
        frame.damage.push(LineDamage {
            line: 1,
            left: 0,
            right: 3,
        });
        assert_eq!(
            plan_render(Some(HighlightSignature::of(&frame)), true, true, &frame),
            RenderPlan::Render { force_full: false },
            "terminal damage alone must not escalate to a full repaint"
        );

        let mut full = clean_frame();
        full.full = true;
        assert!(
            matches!(
                plan_render(Some(HighlightSignature::of(&full)), true, true, &full),
                RenderPlan::Render { .. }
            ),
            "a full frame must never be reused"
        );
    }

    #[test]
    fn highlight_transition_overrides_partial_damage_to_full_scene() {
        let mut frame = clean_frame();
        frame.damage.push(LineDamage {
            line: 0,
            left: 0,
            right: 1,
        });
        frame.selection = Some(span(2, 0, 2, 3));
        let plan = plan_render(Some(signature(None, None)), true, true, &frame);
        assert_eq!(plan, RenderPlan::Render { force_full: true });

        let viewport = Viewport {
            cell_w: 8.0,
            cell_h: 16.0,
            cols: frame.cols,
            rows: frame.rows,
        };
        let mut scene = SceneBuilder::from_frame(&frame, viewport).build();
        assert!(
            !scene.damage.full,
            "selection change is not terminal damage"
        );
        assert!(
            scene.damage.bounds().is_some(),
            "the frame reports exactly one damaged line"
        );
        apply_render_plan_damage(&mut scene, false);
        assert!(
            !scene.damage.full,
            "the partial path must stay partial when nothing forces full"
        );

        apply_render_plan_damage(&mut scene, true);
        assert!(scene.damage.full);
        assert!(
            scene.damage.bounds().is_none(),
            "full damage makes render_scene take the full path"
        );
    }

    #[test]
    fn invalidate_pane_drops_the_cached_image() {
        let pane = 0x5eed;
        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(2, 2);
        PANE_IMAGES.with(|images| {
            images
                .borrow_mut()
                .insert(pane, slint::Image::from_rgba8(buffer));
        });
        assert!(PANE_IMAGES.with(|images| images.borrow().contains_key(&pane)));

        invalidate_pane(pane);

        assert!(
            !PANE_IMAGES.with(|images| images.borrow().contains_key(&pane)),
            "a CPU fallback must make the next GPU frame rebuild instead of reuse"
        );
    }

    /// Mirrors the per-pane bookkeeping `render_pane_image` keeps between
    /// frames: highlight signature and target size of the cached pixels, plus
    /// whether Slint holds an imported image. Exercises the same decisions
    /// without a GPU device.
    #[derive(Default)]
    struct TargetModel {
        highlight: Option<HighlightSignature>,
        size: Option<PhysicalSize>,
        has_image: bool,
    }

    impl TargetModel {
        fn plan(&self, frame: &Frame, size: PhysicalSize) -> RenderPlan {
            plan_render(self.highlight, self.has_image, self.size == Some(size), frame)
        }

        fn step(&mut self, expected: RenderPlan, frame: &Frame, size: PhysicalSize) {
            assert_eq!(self.plan(frame, size), expected);
            if let RenderPlan::Render { .. } = expected {
                self.highlight = Some(HighlightSignature::of(frame));
                self.size = Some(size);
                self.has_image = true;
            }
        }

        fn invalidate(&mut self) {
            *self = Self::default();
        }
    }

    #[test]
    fn pane_state_sequence_never_reuses_a_stale_target() {
        let mut model = TargetModel::default();
        let clean = clean_frame();

        // First frame: full. The same clean frame afterwards: reuse.
        model.step(RenderPlan::Render { force_full: true }, &clean, SIZE);
        model.step(RenderPlan::Reuse, &clean, SIZE);

        // Selection change while idle: bypass reuse and force full.
        let mut selected = clean.clone();
        selected.selection = Some(span(0, 0, 0, 2));
        model.step(RenderPlan::Render { force_full: true }, &selected, SIZE);
        model.step(RenderPlan::Reuse, &selected, SIZE);

        // Clearing the selection is the same transition class.
        model.step(RenderPlan::Render { force_full: true }, &clean, SIZE);
        model.step(RenderPlan::Reuse, &clean, SIZE);

        // Search highlight behaves identically.
        let mut searched = clean.clone();
        searched.search = Some(span(2, 0, 2, 3));
        model.step(RenderPlan::Render { force_full: true }, &searched, SIZE);
        model.step(RenderPlan::Reuse, &searched, SIZE);

        // A partial frame with an unchanged highlight still uses damage.
        let mut damaged = searched.clone();
        damaged.damage.push(LineDamage {
            line: 1,
            left: 0,
            right: 2,
        });
        model.step(RenderPlan::Render { force_full: false }, &damaged, SIZE);

        // CPU fallback invalidates the target: even a clean frame with the
        // same highlight must be fully repainted, never reused/partially patched.
        model.invalidate();
        model.step(RenderPlan::Render { force_full: true }, &searched, SIZE);
        model.step(RenderPlan::Reuse, &searched, SIZE);
    }

    /// The app's GPU path must consume the same resolved semantics as the CPU
    /// renderer: wide/spacer identity, hidden/dim/bold flags, selection colors.
    #[test]
    fn scene_from_frame_preserves_wide_hidden_dim_and_selection() {
        let mut frame = Frame::new(4, 2);
        frame.generation = 42;
        frame.full = true;

        let cell = |ch: char, fg: u32, bg: u32, flags: u16| CellData {
            ch: ch as u32,
            fg,
            bg,
            flags,
            link: 0,
        };
        frame.cells[0] = cell('你', 0x00ff_ffff, 0x0000_0000, flags::WIDE_CHAR);
        frame.cells[1] = cell(' ', 0x00ff_ffff, 0x0000_0000, flags::WIDE_CHAR_SPACER);
        frame.cells[2] = cell('H', 0x00ff_ffff, 0x0012_3456, flags::HIDDEN);
        frame.cells[3] = cell('D', 0x00ff_ffff, 0x0000_0000, flags::DIM);
        frame.cells[4] = cell('B', 0x00ff_ffff, 0x0000_0000, flags::BOLD);
        frame.cells[5] = cell('S', 0x00aa_bbcc, 0x0011_2233, flags::INVERSE);
        frame.cells[6] = cell(' ', 0x00ff_ffff, 0x0000_0000, 0);
        frame.cells[7] = cell(' ', 0x00ff_ffff, 0x0000_0000, 0);
        frame.selection = Some(Span {
            start_line: 1,
            start_col: 3,
            end_line: 1,
            end_col: 3,
        });
        frame.cursor.visible = true;
        frame.cursor.line = 0;
        frame.cursor.col = 0;

        let viewport = Viewport {
            cell_w: 8.0,
            cell_h: 16.0,
            cols: 4,
            rows: 2,
        };
        let scene = SceneBuilder::from_frame(&frame, viewport)
            .background(BackgroundInstance { color: 0x0011_2233 })
            .build();
        let batch = scene.text_batch().expect("frame adapter emits one text batch");
        assert_eq!(batch.cells.len(), 8);
        assert!(batch.cells[0].attributes.wide);
        assert!(batch.cells[1].attributes.wide_spacer);
        assert!(batch.cells[2].attributes.hidden);
        assert!(batch.cells[3].attributes.dim);
        assert!(batch.cells[4].attributes.bold);
        // Inverse + selection swap: fg/bg end up swapped twice = original.
        assert_eq!(batch.cells[5].foreground, 0x0011_2233);
        assert_eq!(batch.cells[5].background, 0x00aa_bbcc);
        // Wide cell background is resolved, not copied from a framebuffer.
        assert_eq!(batch.cells[0].background, 0x0000_0000);
        assert_eq!(scene.revision, 42);
        assert_eq!(
            scene.layer(tervo_scene::BACKGROUND_LAYER_Z).map(|layer| layer.items.len()),
            Some(1)
        );
        // The cursor is a Scene item; the app keeps its own Slint overlay for it.
        assert!(scene.cursor.is_some());
        assert_eq!(scene.image_count(), 0);
    }
}
