//! Standalone WGPU backend and Slint texture-presenter primitives for Tervo.
//!
//! This crate intentionally has no dependency on `tervo-core` or `tervo-app`.
//! It consumes a backend-independent [`tervo_scene::Scene`] and produces an
//! offscreen RGBA8 texture that can be imported into Slint.
//!
//! # Scope
//!
//! Two independent pieces live here:
//!
//! - [`WgpuRenderer`] is the original frame-target skeleton: offscreen texture,
//!   resize/recreation and a single clear pass implementing
//!   [`tervo_render::Renderer`].
//! - [`GpuTextRenderer`] is the glyph-atlas prototype. It consumes
//!   [`tervo_scene::TextBatch`] directly, rasterizes ASCII glyphs with fontdue
//!   into an `R8Unorm` GPU atlas, and draws instanced quads into an offscreen
//!   `Rgba8Unorm` target in one render pass. No CPU terminal framebuffer is
//!   produced or uploaded.
//!
//! The text prototype is opt-in and is not wired into the app main loop. The
//! clear-only backend remains the [`tervo_render::Renderer`] implementation so
//! existing callers are unaffected.
//!
//! The Slint presenter lives behind the `slint-presenter` feature so that the
//! backend can be built and used headlessly without Slint's winit/femtovg stack.
//!
//! # Texture contract
//!
//! The frame texture uses [`FRAME_FORMAT`] ([`wgpu::TextureFormat::Rgba8Unorm`])
//! and [`FRAME_USAGE`]. Those are also the hard requirements of
//! `slint::Image::try_from(wgpu::Texture)`:
//!
//! - format is `Rgba8Unorm` or `Rgba8UnormSrgb`
//! - usage contains `TEXTURE_BINDING` **and** `RENDER_ATTACHMENT`
//!
//! The texture must be created from the exact same `wgpu::Device`/`wgpu::Queue`
//! that Slint renders with. With the winit backend that means creating it in the
//! `RenderingSetup` callback via `slint::GraphicsAPI::WGPU30 { device, queue, .. }`.

use std::fmt;

use tervo_render::{RenderedFrame, Renderer};
use tervo_scene::{DamageRegion, PhysicalSize, Scene};

pub mod atlas;
pub mod headless;
pub mod reference;
pub mod text;

#[cfg(feature = "slint-presenter")]
pub mod presenter;

#[cfg(feature = "slint-presenter")]
pub use presenter::{PresenterError, SlintTexturePresenter};

pub use atlas::{
    discover_font_path, AtlasConfig, AtlasError, AtlasStats, FontMetrics, FontStyle, GlyphAtlas,
    GlyphLookup, GlyphSlot, ATLAS_FORMAT, DEFAULT_ATLAS_SIZE,
};
pub use headless::{request_headless_gpu, HeadlessGpu};
pub use reference::{
    compare_rgba, diff_rgba, rasterize_reference, write_ppm, PixelDiff, ReferenceImage,
};
pub use text::{
    read_target_rgba8, FrameStats, GlyphInstance, GpuTextRenderer, SolidInstance, TextGeometry,
    TextLayout, TextRenderError, TextSection, TextTarget,
};

/// Pixel format of the offscreen terminal frame texture.
pub const FRAME_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Usage flags required for a terminal frame texture.
///
/// `RENDER_ATTACHMENT` lets the backend draw into it, `TEXTURE_BINDING` lets
/// Slint sample it, and `COPY_DST`/`COPY_SRC` keep the texture usable for
/// uploads and tests/diagnostics.
pub const FRAME_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::RENDER_ATTACHMENT
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(wgpu::TextureUsages::COPY_DST)
    .union(wgpu::TextureUsages::COPY_SRC);

/// Errors produced while creating or resizing the offscreen frame target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// A width or height of zero is not a valid texture size.
    ZeroSize,
    /// The requested size exceeds `max_texture_dimension_2d` of the device.
    SizeExceedsDeviceLimit { width: u32, height: u32, max: u32 },
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSize => f.write_str("frame size must not be zero"),
            Self::SizeExceedsDeviceLimit { width, height, max } => write!(
                f,
                "frame size {width}x{height} exceeds device max_texture_dimension_2d ({max})"
            ),
        }
    }
}

impl std::error::Error for BackendError {}

fn validate_size(size: PhysicalSize, max_dimension: u32) -> Result<(), BackendError> {
    if size.width == 0 || size.height == 0 {
        return Err(BackendError::ZeroSize);
    }
    if size.width > max_dimension || size.height > max_dimension {
        return Err(BackendError::SizeExceedsDeviceLimit {
            width: size.width,
            height: size.height,
            max: max_dimension,
        });
    }
    Ok(())
}

fn wgpu_clear_color(color: [u8; 4]) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(color[0]) / 255.0,
        g: f64::from(color[1]) / 255.0,
        b: f64::from(color[2]) / 255.0,
        a: f64::from(color[3]) / 255.0,
    }
}

/// The offscreen texture Slint imports as an image.
///
/// This type owns both the texture and a cached default view. The presenter
/// keeps its own clone of [`wgpu::Texture`] alive after this object is dropped.
pub struct FrameTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: PhysicalSize,
}

impl FrameTexture {
    fn new(device: &wgpu::Device, size: PhysicalSize) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tervo-render-wgpu-frame"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FRAME_FORMAT,
            usage: FRAME_USAGE,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            size,
        }
    }

    /// The underlying wgpu texture. Clone it to hand it to a presenter.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// A default texture view for rendering into the frame.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Physical size of the frame texture in pixels.
    pub fn size(&self) -> PhysicalSize {
        self.size
    }
}

impl fmt::Debug for FrameTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameTexture")
            .field("size", &self.size)
            .field("format", &self.texture.format())
            .finish_non_exhaustive()
    }
}

/// Minimal WGPU renderer that owns an offscreen terminal frame texture.
///
/// The renderer implements [`tervo_render::Renderer`] but currently only runs a
/// clear pass. The clear color is configured with [`Self::set_clear_color`];
/// the scene and damage arguments are accepted for API compatibility and
/// intentionally not inspected yet.
///
/// The renderer does not create a device or queue. Callers must supply the same
/// `wgpu::Device` and `wgpu::Queue` that Slint uses (see the crate docs).
pub struct WgpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    frame: FrameTexture,
    clear_color: [u8; 4],
    revision: u64,
}

impl WgpuRenderer {
    /// Creates an offscreen frame texture for `size` on `device`.
    ///
    /// Returns [`BackendError`] for zero-sized frames or frames larger than the
    /// device limit. Device loss is not handled here.
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        size: PhysicalSize,
    ) -> Result<Self, BackendError> {
        validate_size(size, device.limits().max_texture_dimension_2d)?;
        let frame = FrameTexture::new(&device, size);
        Ok(Self {
            device,
            queue,
            frame,
            clear_color: [0, 0, 0, 255],
            revision: 0,
        })
    }

    /// The current offscreen frame target.
    pub fn frame(&self) -> &FrameTexture {
        &self.frame
    }

    /// Convenience accessor for [`FrameTexture::texture`].
    pub fn texture(&self) -> &wgpu::Texture {
        self.frame.texture()
    }

    /// Color used by [`tervo_render::Renderer::render`].
    pub fn clear_color(&self) -> [u8; 4] {
        self.clear_color
    }

    /// Sets the color used by [`tervo_render::Renderer::render`].
    pub fn set_clear_color(&mut self, color: [u8; 4]) {
        self.clear_color = color;
    }

    /// Runs one full-frame clear pass and returns the new frame revision.
    ///
    /// The clear is submitted to the renderer's queue. This is intentionally a
    /// public primitive so tests/examples can produce a frame without building a
    /// full scene.
    pub fn clear(&mut self, color: [u8; 4]) -> RenderedFrame {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tervo-render-wgpu-clear"),
            });

        {
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: self.frame.view(),
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu_clear_color(color)),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tervo-render-wgpu-clear-pass"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        self.queue.submit(Some(encoder.finish()));
        self.revision = self.revision.wrapping_add(1);
        RenderedFrame {
            revision: self.revision,
            size: self.frame.size(),
        }
    }
}

impl fmt::Debug for WgpuRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WgpuRenderer")
            .field("frame", &self.frame)
            .field("clear_color", &self.clear_color)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl Renderer for WgpuRenderer {
    type Error = BackendError;
    type Frame = RenderedFrame;

    fn resize(&mut self, size: PhysicalSize) -> Result<(), Self::Error> {
        if size == self.frame.size() {
            return Ok(());
        }
        validate_size(size, self.device.limits().max_texture_dimension_2d)?;
        self.frame = FrameTexture::new(&self.device, size);
        // The texture handle changed; callers holding a presenter must re-import
        // the new texture. The revision stays until content is rendered.
        Ok(())
    }

    fn render(
        &mut self,
        _scene: &Scene,
        _damage: &DamageRegion,
    ) -> Result<Self::Frame, Self::Error> {
        Ok(self.clear(self.clear_color))
    }

    fn invalidate(&mut self, _damage: &DamageRegion) {
        // Clear-only backend: every render is a full-frame clear, so there is
        // nothing to track. Kept as a no-op until partial redraw exists.
    }

    fn device_lost(&mut self) {
        // No automatic recovery: the owner must recreate device/queue and
        // therefore a new renderer/presenter. Kept as an explicit no-op so the
        // limitation is visible in the API.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_sized_frame_is_rejected() {
        assert_eq!(
            validate_size(
                PhysicalSize {
                    width: 0,
                    height: 10
                },
                8192
            ),
            Err(BackendError::ZeroSize)
        );
        assert_eq!(
            validate_size(
                PhysicalSize {
                    width: 10,
                    height: 0
                },
                8192
            ),
            Err(BackendError::ZeroSize)
        );
    }

    #[test]
    fn oversized_frame_is_rejected() {
        assert_eq!(
            validate_size(
                PhysicalSize {
                    width: 8193,
                    height: 10
                },
                8192
            ),
            Err(BackendError::SizeExceedsDeviceLimit {
                width: 8193,
                height: 10,
                max: 8192,
            })
        );
        assert_eq!(
            validate_size(
                PhysicalSize {
                    width: 10,
                    height: 8193
                },
                8192
            ),
            Err(BackendError::SizeExceedsDeviceLimit {
                width: 10,
                height: 8193,
                max: 8192,
            })
        );
    }

    #[test]
    fn valid_size_is_accepted() {
        assert_eq!(
            validate_size(
                PhysicalSize {
                    width: 8192,
                    height: 8192
                },
                8192
            ),
            Ok(())
        );
        assert_eq!(
            validate_size(
                PhysicalSize {
                    width: 1,
                    height: 1
                },
                1
            ),
            Ok(())
        );
    }

    #[test]
    fn frame_format_meets_slint_import_contract() {
        assert_eq!(FRAME_FORMAT, wgpu::TextureFormat::Rgba8Unorm);
        assert!(FRAME_USAGE.contains(wgpu::TextureUsages::TEXTURE_BINDING));
        assert!(FRAME_USAGE.contains(wgpu::TextureUsages::RENDER_ATTACHMENT));
    }

    #[test]
    fn clear_color_conversion_matches_byte_values() {
        let black = wgpu_clear_color([0, 0, 0, 255]);
        assert_eq!((black.r, black.g, black.b, black.a), (0.0, 0.0, 0.0, 1.0));

        let red = wgpu_clear_color([255, 0, 0, 255]);
        assert_eq!((red.r, red.g, red.b, red.a), (1.0, 0.0, 0.0, 1.0));

        let mid = wgpu_clear_color([128, 64, 32, 128]);
        assert!((mid.r - 128.0 / 255.0).abs() < f64::EPSILON);
        assert!((mid.g - 64.0 / 255.0).abs() < f64::EPSILON);
        assert!((mid.b - 32.0 / 255.0).abs() < f64::EPSILON);
        assert!((mid.a - 128.0 / 255.0).abs() < f64::EPSILON);
    }
}
