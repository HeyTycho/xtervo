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
use std::sync::atomic::{AtomicBool, Ordering};
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
    if !requested() {
        return;
    }
    let mut guard = match STATE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_some() {
        return;
    }
    let limits = device.limits();
    eprintln!(
        "[gpu] TERVO_GPU=1：拿到 Slint WGPU30 同一 device/queue；\
         max_texture_dimension_2d={} max_buffer_size={}（终端栅格化仍在 CPU，只走纹理显示）",
        limits.max_texture_dimension_2d, limits.max_buffer_size
    );
    *guard = Some(State {
        device: device.clone(),
        queue: queue.clone(),
        frames: HashMap::new(),
    });
    READY.store(true, Ordering::Release);
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
) -> Option<slint::Image> {
    if !ready() {
        return None;
    }
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        fail("framebuffer 尺寸超出 u32");
        return None;
    };
    if width == 0 || height == 0 || pixels.len() < width as usize * height as usize {
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

    let bytes: &[u8] = bytemuck::cast_slice(pixels);
    let bytes_per_row = width * 4;
    let mut uploaded_rows = 0u32;
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
    }

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
