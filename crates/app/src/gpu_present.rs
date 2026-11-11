// SPDX-License-Identifier: GPL-3.0-only
//! `TERVO_GPU=1` 可选显示链路（spike / 非默认）。
//!
//! 把 CPU 终端 framebuffer 用 `queue.write_texture` 写进一张 **Slint 渲染器同一个
//! wgpu Device/Queue** 上的 `Rgba8Unorm` 纹理，再用 `slint::Image::try_from(texture)`
//! 交给 Slint 显示。Device/Queue 来自 `Window::set_rendering_notifier` 的
//! `RenderingState::RenderingSetup` + `GraphicsAPI::WGPU30 { device, queue, .. }`。
//!
//! 边界（刻意不做的事）：
//! - 不做 GPU Scene / glyph atlas / 图片 GPU 合成：终端栅格化仍在 CPU（`Renderer`）。
//! - 不是默认路径：只有 `TERVO_GPU=1` 且 WGPU30 后端选中有 device/queue 才启用；
//!   其余情况返回 `None`，调用方继续用原来的 CPU/Skia `Image::from_rgba8`。
//! - 不做整屏重绘兜底：只上传本帧实际重绘的行（`Renderer::dirty_rows`）；首帧/
//!   resize 后纹理新建时上传整帧。
//! - 纹理导入失败/尺寸超限：记录一次明确日志并永久回退 CPU，不每帧重试。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use slint::wgpu_30::wgpu;

/// `TERVO_GPU` 是否请求 GPU 显示链路（默认 false）。
pub fn requested() -> bool {
    static REQUESTED: OnceLock<bool> = OnceLock::new();
    *REQUESTED.get_or_init(|| match std::env::var("TERVO_GPU") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    })
}

/// 是否每帧打印上传明细（默认只打首帧/错误）。
fn debug() -> bool {
    static DEBUG: OnceLock<bool> = OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("TERVO_GPU_DEBUG").is_some())
}

struct FrameSlot {
    texture: wgpu::Texture,
    width: u32,
    height: u32,
    /// 是否已经打印过“该 pane 的纹理已导入”日志。
    logged_import: bool,
}

struct State {
    device: wgpu::Device,
    queue: wgpu::Queue,
    frames: HashMap<u64, FrameSlot>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// 真实 GPU 上传/呈现统计（仅用于 profile，不影响渲染）。
static UPLOAD_FRAMES: AtomicU64 = AtomicU64::new(0);
static UPLOAD_ROWS: AtomicU64 = AtomicU64::new(0);
static UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static UPLOAD_NS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
pub struct UploadStats {
    pub frames: u64,
    pub rows: u64,
    pub bytes: u64,
    pub upload_nanos: u64,
}

/// 当前进程累计的 GPU 纹理上传统计。
pub fn upload_stats() -> UploadStats {
    UploadStats {
        frames: UPLOAD_FRAMES.load(Ordering::Relaxed),
        rows: UPLOAD_ROWS.load(Ordering::Relaxed),
        bytes: UPLOAD_BYTES.load(Ordering::Relaxed),
        upload_nanos: UPLOAD_NS.load(Ordering::Relaxed),
    }
}
/// Slint 已经给出 WGPU30 device/queue，且纹理文件仍可用。
static READY: AtomicBool = AtomicBool::new(false);
/// 出现过不可恢复的纹理/上传错误：本进程剩余时间一律走 CPU。
static FAILED: AtomicBool = AtomicBool::new(false);

/// 当前是否真的在用 WGPU 纹理显示（供日志/快照证据使用）。
pub fn ready() -> bool {
    requested() && READY.load(Ordering::Acquire) && !FAILED.load(Ordering::Acquire)
}

fn fail(reason: impl std::fmt::Display) {
    if !FAILED.swap(true, Ordering::AcqRel) {
        eprintln!("[gpu] 纹理链路失败，自动回退 CPU/Skia（本进程不再尝试）: {reason}");
    }
    READY.store(false, Ordering::Release);
}

/// 在 `RenderingSetup` 回调里调用，保存 Slint 实际使用的 device/queue。
///
/// 幂等：渲染器重建时会再次回调；只有第一次（或 device 重建时）安装。
pub fn install(device: &wgpu::Device, queue: &wgpu::Queue) {
    // Stage 0 (`TERVO_GPU=1`) 和 Stage 1 (`TERVO_GPU_NATIVE=1`) 共用 Slint 的
    // 同一个 WGPU30 device/queue；是否启用各自链路由各自 ready() 决定。
    if !requested() && !crate::gpu_native::requested() {
        return;
    }
    let mut guard = match STATE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_some() {
        // Slint 可能在 suspend/resume 或后端重建时再次发 RenderingSetup，
        // 但没有成对 teardown；旧 device 的纹理不能给新 renderer 用。
        eprintln!("[gpu] RenderingSetup 重新安装：丢弃旧 device 纹理");
        *guard = None;
        READY.store(false, Ordering::Release);
    }
    let limits = device.limits();
    eprintln!(
        "[gpu] 拿到 Slint WGPU30 同一 device/queue；max_texture_dimension_2d={} \
         max_buffer_size={}（TERVO_GPU={} stage0 CPU 纹理; TERVO_GPU_NATIVE={} Scene 文字）",
        limits.max_texture_dimension_2d,
        limits.max_buffer_size,
        requested(),
        crate::gpu_native::requested()
    );
    *guard = Some(State {
        device: device.clone(),
        queue: queue.clone(),
        frames: HashMap::new(),
    });
    READY.store(true, Ordering::Release);
}

/// 取 Slint WGPU30 实际使用的 device/queue（与 GPU-native 链路同源）。
///
/// 返回 clone 的 handle；Slint 在 RenderingSetup 之前返回 None。
pub fn device_queue() -> Option<(wgpu::Device, wgpu::Queue)> {
    let guard = STATE.lock().ok()?;
    guard
        .as_ref()
        .map(|state| (state.device.clone(), state.queue.clone()))
}

/// 丢弃某个 pane 的纹理；pane id 被重连复用时必须先调用，否则会复用旧会话纹理。
pub fn forget(pane: u64) {
    if let Ok(mut guard) = STATE.lock() {
        if let Some(state) = guard.as_mut() {
            state.frames.remove(&pane);
        }
    }
}

/// 丢弃已关闭 pane 的纹理；只保留当前仍存在的 pane。
///
/// 纹理若只放在全局 HashMap，关闭 Tab/pane 后不会随 PaneRuntime 释放，
/// 会表现为“开关 pane 时 VRAM 单调增长”。每次重绘用当前 pane 列表回收。
pub fn retain(active: &[u64]) {
    if let Ok(mut guard) = STATE.lock() {
        if let Some(state) = guard.as_mut() {
            state.frames.retain(|pane, _| active.contains(pane));
        }
    }
}

/// 渲染器销毁时丢弃旧 device 上的纹理。
pub fn teardown() {
    if let Ok(mut guard) = STATE.lock() {
        if guard.is_some() {
            eprintln!("[gpu] rendering teardown：丢弃 GPU 帧纹理");
        }
        *guard = None;
    }
    READY.store(false, Ordering::Release);
}

/// 把本帧 CPU framebuffer 上传到该 pane 的持久纹理，并返回可显示的 Slint Image。
///
/// `full=true`（首帧/resize/整屏重绘）上传整帧；否则只上传 `rows` 中实际重绘的
/// 连续行区间。返回 `None` 时调用方必须继续使用 CPU `Image::from_rgba8`。
pub fn pane_image(
    pane: u64,
    pixels: &[u32],
    width: usize,
    height: usize,
    full: bool,
    rows: &[usize],
    allow_resize: bool,
) -> Option<slint::Image> {
    if !ready() {
        return None;
    }
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        fail("framebuffer 尺寸超出 u32");
        return None;
    };
    if width == 0 || height == 0 || pixels.len() < width as usize * height as usize {
        fail("framebuffer 为空或像素数不足");
        return None;
    }
    let mut guard = match STATE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(state) = guard.as_mut() else {
        return None;
    };
    if width > state.device.limits().max_texture_dimension_2d
        || height > state.device.limits().max_texture_dimension_2d
    {
        fail(format!(
            "pane={pane} framebuffer {width}x{height} 超过 max_texture_dimension_2d={}",
            state.device.limits().max_texture_dimension_2d
        ));
        return None;
    }

    let recreate = state
        .frames
        .get(&pane)
        .is_none_or(|slot| slot.width != width || slot.height != height);
    // live resize 期间继续用 CPU Image：拖动时每个中间尺寸都新建一张
    // 大纹理会造成几十次 VRAM 分配/释放。resize 结束后再建最终纹理。
    if recreate && !allow_resize {
        return None;
    }
    if recreate {
        let texture = state.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tervo-terminal-frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Slint 1.18.1 的 Image::try_from 只接受 Rgba8Unorm/Rgba8UnormSrgb；
            // usage 必须同时含 TEXTURE_BINDING | RENDER_ATTACHMENT。
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        state.frames.insert(
            pane,
            FrameSlot {
                texture,
                width,
                height,
                logged_import: false,
            },
        );
    }
    let slot = state.frames.get_mut(&pane)?;
    let full = full || recreate;

    // 行区间：整帧重绘 -> 一行区间；否则把 dirty_rows 合并成连续区间。
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    if full {
        ranges.push((0, height));
    } else {
        let mut sorted: Vec<u32> = rows
            .iter()
            .map(|&r| r as u32)
            .filter(|&r| r < height)
            .collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut start = None;
        let mut prev = 0u32;
        for row in sorted {
            match start {
                None => {
                    start = Some(row);
                    prev = row;
                },
                Some(_) if row == prev + 1 => prev = row,
                Some(s) => {
                    ranges.push((s, prev - s + 1));
                    start = Some(row);
                    prev = row;
                },
            }
        }
        if let Some(s) = start {
            ranges.push((s, prev - s + 1));
        }
    }
    if ranges.is_empty() {
        // 没有重绘行（例如只更新 overlay）：纹理内容没变，直接重新导入同一纹理。
        ranges.push((0, 0));
    }

    let upload_started = std::time::Instant::now();
    let bytes: &[u8] = bytemuck::cast_slice(pixels);
    let bytes_per_row = width * 4;
    let mut uploaded_rows = 0u32;
    let mut uploaded_bytes = 0u64;
    for (y, count) in ranges {
        if count == 0 {
            continue;
        }
        let offset = y as usize * bytes_per_row as usize;
        let len = count as usize * bytes_per_row as usize;
        let Some(data) = bytes.get(offset..offset + len) else {
            fail(format!("pane={pane} 行区间 {y}+{count} 越界"));
            return None;
        };
        state.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &slot.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(count),
            },
            wgpu::Extent3d {
                width,
                height: count,
                depth_or_array_layers: 1,
            },
        );
        uploaded_rows += count;
        uploaded_bytes = uploaded_bytes.saturating_add(len as u64);
    }
    UPLOAD_FRAMES.fetch_add(1, Ordering::Relaxed);
    UPLOAD_ROWS.fetch_add(uploaded_rows as u64, Ordering::Relaxed);
    UPLOAD_BYTES.fetch_add(uploaded_bytes, Ordering::Relaxed);
    UPLOAD_NS.fetch_add(upload_started.elapsed().as_nanos() as u64, Ordering::Relaxed);

    let image = match slint::Image::try_from(slot.texture.clone()) {
        Ok(image) => image,
        Err(error) => {
            fail(format!("slint::Image::try_from 拒绝纹理: {error}"));
            return None;
        }
    };
    if !slot.logged_import {
        slot.logged_import = true;
        eprintln!(
            "[gpu] pane={pane} 已导入 {width}x{height} Rgba8Unorm 纹理到 Slint 同一 device；\
             本帧上传 {uploaded_rows} 行（full={full}）"
        );
    } else if debug() {
        eprintln!("[gpu] pane={pane} upload rows={uploaded_rows} full={full}");
    }
    Some(image)
}
