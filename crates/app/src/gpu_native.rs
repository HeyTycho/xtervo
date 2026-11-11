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
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use slint::wgpu_30::wgpu;
use tervo_core::Frame;
use tervo_graphics::SourceId;
use tervo_render_wgpu::{
    read_target_rgba8, write_ppm, AtlasConfig, GpuImageRenderer, GpuTextRenderer, SceneLayerStats,
    TextLayout, TextSection, TextTarget,
};
use tervo_scene::{
    BackgroundInstance, DrawItem, PhysicalSize, Scene, SceneBuilder, Viewport, BACKGROUND_LAYER_Z,
};

/// `TERVO_GPU_NATIVE` 是否请求 Stage 1（默认 false）。
pub fn requested() -> bool {
    static REQUESTED: OnceLock<bool> = OnceLock::new();
    *REQUESTED.get_or_init(|| {
        std::env::var("TERVO_GPU_NATIVE").is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
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
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// 出现过不可恢复错误：本进程剩余时间一律回 CPU。
static FAILED: Mutex<bool> = Mutex::new(false);

static FRAMES: AtomicU64 = AtomicU64::new(0);
static DRAW_CALLS: AtomicU64 = AtomicU64::new(0);
static ATLAS_UPLOADS: AtomicU64 = AtomicU64::new(0);
static ATLAS_UPLOADED_BYTES: AtomicU64 = AtomicU64::new(0);
static VERTEX_UPLOADED_BYTES: AtomicU64 = AtomicU64::new(0);
static IMAGE_UPLOADS: AtomicU64 = AtomicU64::new(0);
static IMAGE_SOURCES: AtomicU64 = AtomicU64::new(0);

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
pub fn install(device: &wgpu::Device, _queue: &wgpu::Queue) {
    if !requested() || failed() {
        return;
    }
    if let Ok(mut guard) = STATE.lock() {
        if guard.is_some() {
            eprintln!("[gpu-native] RenderingSetup 重新安装：丢弃旧 device 上的 atlas/target");
        }
        *guard = None;
    }
    let info = device.adapter_info();
    eprintln!(
        "[gpu-native] TERVO_GPU_NATIVE=1：拿到 Slint WGPU30 同一 device/queue；\
         adapter={:?} backend={:?} device_type={:?}",
        info.name, info.backend, info.device_type
    );
}

/// 丢弃旧 device（RenderingTeardown / 重建）。
pub fn teardown() {
    if let Ok(mut guard) = STATE.lock() {
        if guard.is_some() {
            eprintln!("[gpu-native] rendering teardown：丢弃 GPU Scene atlas/target");
        }
        *guard = None;
    }
    PANE_IMAGES.with(|images| images.borrow_mut().clear());
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
    if let Ok(guard) = STATE.lock() {
        if guard.is_some() {
            return true;
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

/// 把真实 `Frame` 走 Scene → WGPU 分层合成，返回可交给 `PaneView.image` 的
/// `slint::Image`。返回 `None` 时调用方必须使用 CPU `Renderer::image()`。
pub fn pane_image(input: &PaneFrame<'_>) -> Option<slint::Image> {
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
    let scene = SceneBuilder::from_frame(input.frame, viewport)
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

    // Upload each immutable Frame image source at most once, then drop sources
    // that are no longer visible. This is the only image pixel upload; no
    // per-frame full-image copy.
    let mut visible_sources: Vec<SourceId> = Vec::with_capacity(input.frame.images.len());
    for image in &input.frame.images {
        if image.width == 0 || image.height == 0 {
            continue;
        }
        let source = SourceId(image.source_id);
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

    let recreate = state
        .panes
        .get(&input.pane)
        .is_none_or(|pane| pane.target.size() != PhysicalSize { width, height });
    if recreate {
        let target = match TextTarget::new(&state.device, PhysicalSize { width, height }) {
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
            },
        );
    }

    let section;
    let sections = match scene.text_batch() {
        Some(batch) => {
            section = TextSection::new(batch, layout);
            std::slice::from_ref(&section)
        }
        None => &[],
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
                Ok(pixels) => match write_ppm(&path, width, height, &pixels) {
                    Ok(()) => eprintln!(
                        "[gpu-native] 像素证据（一次性只读回读）path={} size={width}x{height}",
                        path.display()
                    ),
                    Err(error) => eprintln!(
                        "[gpu-native] 写像素证据失败 path={} error={error}",
                        path.display()
                    ),
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
    use tervo_core::{flags, CellData, Span};

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
