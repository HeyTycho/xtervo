//! WGPU textured-quad composition for [`tervo_scene::ImageInstance`]s.
//!
//! The terminal scene carries image *metadata* only: a [`SourceId`], crop and
//! destination geometry, clip, z and composition policy. This module owns the
//! GPU side of that contract:
//!
//! ```text
//! Frame.images RGBA (uploaded once per source_id)
//!   -> ImageTextureRegistry (Rgba8Unorm texture per source)
//!   -> image instances (destination + source_rect/source_size UV + scissor)
//!   -> one draw per image in scene order, alpha blended
//!   -> offscreen Rgba8Unorm target shared with the text pipeline
//! ```
//!
//! # Layers
//!
//! [`collect_image_layers`] turns a [`Scene`] into the two composition groups
//! around the text layer. `below` must be drawn after the text cell
//! backgrounds and before glyphs; `above` after glyphs. The text renderer
//! exposes that gap through [`GpuTextRenderer::prepare`] /
//! [`GpuTextRenderer::draw_backgrounds`] / [`GpuTextRenderer::draw_glyphs`], and
//! [`GpuImageRenderer::render_scene`] wires the whole order together.
//!
//! # Memory
//!
//! Source pixels are uploaded once and never re-uploaded while the id stays
//! live. The renderer never uploads or copies a full-screen CPU framebuffer;
//! per-frame CPU traffic is the instance buffer (32 bytes/image). Releasing a
//! source frees its texture; [`GpuImageRenderer::retain_sources`] is the
//! per-frame eviction hook.
//!
//! # Limitations (slice)
//!
//! * sampling is nearest-neighbour, no mipmaps; a scaled image is blocky but
//!   deterministic and pixel-comparable with the CPU reference,
//! * one draw call per image (source textures cannot share a bind group),
//! * fractional scissor edges are rounded outward; the shader still limits
//!   coverage to the destination quad. Tests use whole-pixel geometry.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use tervo_graphics::{PlacementId, Rect, SourceId};
use tervo_scene::{DrawItem, ImageInstance, PhysicalSize, Scene};

use crate::text::{FrameStats, GpuTextRenderer, TextRenderError, TextSection, TextTarget};
use crate::FRAME_FORMAT;

/// Texture format of uploaded image sources.
pub const IMAGE_SOURCE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Errors from image source upload, eviction and composition.
#[derive(Debug)]
pub enum ImageError {
    /// A source with zero width or height cannot be a texture.
    ZeroSizedSource { source_id: SourceId },
    /// The source exceeds the device's `max_texture_dimension_2d`.
    SourceTooLarge {
        source_id: SourceId,
        width: u32,
        height: u32,
        max: u32,
    },
    /// The RGBA payload length does not match `width * height * 4`.
    InvalidSourceData {
        source_id: SourceId,
        expected: usize,
        actual: usize,
    },
    /// A `SourceId` is immutable: re-registering it with different dimensions
    /// is a caller bug, not a silent replacement.
    SourceAlreadyRegistered {
        source_id: SourceId,
        width: u32,
        height: u32,
    },
    /// A scene image references pixels that are not in the registry.
    MissingSource { source_id: SourceId },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSizedSource { source_id } => {
                write!(f, "image source {} has zero dimensions", source_id.0)
            }
            Self::SourceTooLarge {
                source_id,
                width,
                height,
                max,
            } => write!(
                f,
                "image source {} ({}x{}) exceeds max_texture_dimension_2d ({max})",
                source_id.0, width, height
            ),
            Self::InvalidSourceData {
                source_id,
                expected,
                actual,
            } => write!(
                f,
                "image source {} needs {expected} RGBA bytes, got {actual}",
                source_id.0
            ),
            Self::SourceAlreadyRegistered {
                source_id,
                width,
                height,
            } => write!(
                f,
                "image source {} is already registered with different dimensions; \
                 new pixels must use a new SourceId (attempted {}x{})",
                source_id.0, width, height
            ),
            Self::MissingSource { source_id } => write!(
                f,
                "scene references image source {} before it was uploaded",
                source_id.0
            ),
        }
    }
}

impl std::error::Error for ImageError {}

/// One uploaded source texture.
struct SourceTexture {
    // Owned so the texture outlives the view; the bind group only holds the
    // view. `wgpu` keeps the allocation alive through either handle.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl SourceTexture {
    fn gpu_bytes(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * 4
    }
}

/// GPU texture registry keyed by immutable [`SourceId`].
///
/// Uploading the same id twice is a no-op: a `SourceId` identifies exactly one
/// generation of pixels, so the renderer must not copy the same image every
/// frame. New pixels always arrive under a new id. The registry owns no CPU
/// mirror of the pixels (that would double the image memory).
pub struct ImageTextureRegistry {
    device: wgpu::Device,
    queue: wgpu::Queue,
    sources: HashMap<SourceId, SourceTexture>,
    uploads: u64,
    uploaded_bytes: u64,
    skipped_uploads: u64,
    released_sources: u64,
    released_bytes: u64,
}

impl ImageTextureRegistry {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self {
            device,
            queue,
            sources: HashMap::new(),
            uploads: 0,
            uploaded_bytes: 0,
            skipped_uploads: 0,
            released_sources: 0,
            released_bytes: 0,
        }
    }

    /// Uploads one source's tightly packed straight-alpha RGBA8 pixels.
    ///
    /// Returns `Ok(true)` when a new texture was created, `Ok(false)` when the
    /// id was already registered (the pixels are intentionally not copied
    /// again). Re-registering the same id with different dimensions returns
    /// [`ImageError::SourceAlreadyRegistered`].
    pub fn upload(
        &mut self,
        source_id: SourceId,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<bool, ImageError> {
        if width == 0 || height == 0 {
            return Err(ImageError::ZeroSizedSource { source_id });
        }
        let max = self.device.limits().max_texture_dimension_2d;
        if width > max || height > max {
            return Err(ImageError::SourceTooLarge {
                source_id,
                width,
                height,
                max,
            });
        }
        if let Some(existing) = self.sources.get(&source_id) {
            if existing.width != width || existing.height != height {
                return Err(ImageError::SourceAlreadyRegistered {
                    source_id,
                    width,
                    height,
                });
            }
            self.skipped_uploads += 1;
            return Ok(false);
        }
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(ImageError::InvalidSourceData {
                source_id,
                expected: usize::MAX,
                actual: rgba.len(),
            })?;
        if rgba.len() != expected {
            return Err(ImageError::InvalidSourceData {
                source_id,
                expected,
                actual: rgba.len(),
            });
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tervo-wgpu-image-source"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: IMAGE_SOURCE_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
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
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bytes = u64::from(width) * u64::from(height) * 4;
        self.sources.insert(
            source_id,
            SourceTexture {
                texture,
                view,
                width,
                height,
            },
        );
        self.uploads += 1;
        self.uploaded_bytes += bytes;
        Ok(true)
    }

    pub fn contains(&self, source_id: SourceId) -> bool {
        self.sources.contains_key(&source_id)
    }

    pub fn source_size(&self, source_id: SourceId) -> Option<(u32, u32)> {
        self.sources
            .get(&source_id)
            .map(|source| (source.width, source.height))
    }

    /// Drops one source texture. Returns whether it existed.
    pub fn release(&mut self, source_id: SourceId) -> bool {
        let Some(source) = self.sources.remove(&source_id) else {
            return false;
        };
        self.released_sources += 1;
        self.released_bytes += source.gpu_bytes();
        true
    }

    /// Drops every source for which `keep` returns false.
    ///
    /// This is the frame-lifecycle hook: after building a scene, keep only the
    /// `source_id`s still referenced by live placements. Returns the number of
    /// released sources.
    pub fn retain(&mut self, mut keep: impl FnMut(SourceId) -> bool) -> usize {
        let mut released = 0usize;
        let mut released_bytes = 0u64;
        self.sources.retain(|source_id, source| {
            if keep(*source_id) {
                true
            } else {
                released += 1;
                released_bytes += source.gpu_bytes();
                false
            }
        });
        self.released_sources += released as u64;
        self.released_bytes += released_bytes;
        released
    }

    /// Drops every source texture.
    pub fn clear(&mut self) {
        self.released_bytes += self
            .sources
            .values()
            .map(SourceTexture::gpu_bytes)
            .sum::<u64>();
        self.released_sources += self.sources.len() as u64;
        self.sources.clear();
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Logical payload bytes of all live source textures (`w * h * 4`).
    pub fn gpu_bytes(&self) -> u64 {
        self.sources.values().map(SourceTexture::gpu_bytes).sum()
    }

    /// Successful uploads (one per new source id).
    pub fn uploads(&self) -> u64 {
        self.uploads
    }

    pub fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes
    }

    /// Upload calls skipped because the id was already registered.
    pub fn skipped_uploads(&self) -> u64 {
        self.skipped_uploads
    }

    pub fn released_sources(&self) -> u64 {
        self.released_sources
    }

    pub fn released_bytes(&self) -> u64 {
        self.released_bytes
    }
}

impl fmt::Debug for ImageTextureRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageTextureRegistry")
            .field("sources", &self.sources.len())
            .field("gpu_bytes", &self.gpu_bytes())
            .field("uploads", &self.uploads)
            .field("skipped_uploads", &self.skipped_uploads)
            .field("released_sources", &self.released_sources)
            .finish()
    }
}

/// One textured quad ready for the GPU, in composition order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageQuad {
    /// `[x, y, width, height]` unclipped destination in physical pixels.
    pub rect: [f32; 4],
    /// `[u0, v0, u1, v1]` normalized `source_rect / source_size`.
    pub uv: [f32; 4],
    /// `[x, y, width, height]` scissor in whole destination pixels.
    pub scissor: [u32; 4],
    pub source_id: SourceId,
    pub placement_id: PlacementId,
    /// Policy-aware layer z used for ordering.
    pub layer_z: i32,
    /// True for the above-text group, false for the below-text group.
    pub above_text: bool,
}

impl ImageQuad {
    const BYTES: u64 = 32;
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: Self::BYTES,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
    };

    fn append_bytes(self, out: &mut Vec<u8>) {
        for value in self.rect.into_iter().chain(self.uv) {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// All quads for one image pass plus how many instances were skipped as
/// degenerate (empty crop, non-finite geometry, no visible pixels).
#[derive(Clone, Debug, Default)]
pub struct ImageGeometry {
    pub quads: Vec<ImageQuad>,
    pub skipped: usize,
}

/// Per-pass image counters.
#[derive(Clone, Debug, Default)]
pub struct ImageFrameStats {
    pub frame: u64,
    pub width: u32,
    pub height: u32,
    /// Quads submitted.
    pub images: usize,
    /// Degenerate scene entries skipped (not a GPU error).
    pub skipped: usize,
    pub draw_calls: u32,
    pub render_passes: u32,
    /// Instance bytes uploaded this frame: 32 bytes per image, never pixels.
    pub vertex_uploaded_bytes: u64,
    /// Uploaded source texture memory (all live sources).
    pub source_gpu_bytes: u64,
    pub source_count: usize,
    /// Target texture memory (not uploaded from the CPU).
    pub target_gpu_bytes: u64,
    pub buffer_reallocations: u64,
    pub bind_group_creations: u64,
    pub cpu_time: Duration,
}

/// Per-layer stats from [`GpuImageRenderer::render_scene`].
#[derive(Clone, Debug, Default)]
pub struct SceneLayerStats {
    /// Text stats; `None` only if the composition did not reach the text stage.
    pub text: Option<FrameStats>,
    pub below: ImageFrameStats,
    pub above: ImageFrameStats,
}

/// Combined text+image composition failure.
#[derive(Debug)]
pub enum CompositeError {
    Text(TextRenderError),
    Image(ImageError),
}

impl fmt::Display for CompositeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(error) => write!(f, "text layer: {error}"),
            Self::Image(error) => write!(f, "image layer: {error}"),
        }
    }
}

impl std::error::Error for CompositeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Text(error) => Some(error),
            Self::Image(error) => Some(error),
        }
    }
}

impl From<TextRenderError> for CompositeError {
    fn from(error: TextRenderError) -> Self {
        Self::Text(error)
    }
}

impl From<ImageError> for CompositeError {
    fn from(error: ImageError) -> Self {
        Self::Image(error)
    }
}

/// Image instances grouped around the text layer, in composition order.
///
/// Layers are sorted by their policy-aware z. Equal z keeps the scene's item
/// order (`sort_by_key` with the original index as a secondary key), so two
/// images in the same layer never swap between frames.
#[derive(Clone, Debug, Default)]
pub struct ImageLayers<'a> {
    /// Drawn after cell backgrounds, before glyphs.
    pub below: Vec<&'a ImageInstance>,
    /// Drawn after glyphs.
    pub above: Vec<&'a ImageInstance>,
}

impl ImageLayers<'_> {
    pub fn len(&self) -> usize {
        self.below.len() + self.above.len()
    }

    pub fn is_empty(&self) -> bool {
        self.below.is_empty() && self.above.is_empty()
    }
}

/// Whether an instance belongs above the glyph layer.
///
/// The policy is authoritative; `above_text` is the protocol's flat mirror of
/// the same decision. Either signal is enough to keep an image above text, so a
/// protocol can never silently downgrade an above-text placement.
pub fn is_above_text(image: &ImageInstance) -> bool {
    image.above_text || image.policy.is_above_text()
}

/// Policy-aware layer z. `ImageInstance::z_index` is the raw protocol value;
/// the policy clamps it into the correct side of the text layer.
pub fn image_layer_z(image: &ImageInstance) -> i32 {
    image.policy.composition_z(image.z_index)
}

/// Groups and sorts scene images into the below-text and above-text passes.
///
/// This walks the scene's layers (not `Scene::images()`) so the original
/// insertion order is available as the stable tie-breaker even when a caller
/// assembled layers out of z order.
pub fn collect_image_layers(scene: &Scene) -> ImageLayers<'_> {
    let mut ordered: Vec<(i32, usize, &ImageInstance)> = Vec::new();
    for layer in &scene.layers {
        for item in &layer.items {
            if let DrawItem::Image(image) = item {
                ordered.push((image_layer_z(image), ordered.len(), image));
            }
        }
    }
    ordered.sort_by_key(|(layer_z, order, _)| (*layer_z, *order));

    let mut layers = ImageLayers::default();
    for (_, _, image) in ordered {
        if is_above_text(image) {
            layers.above.push(image);
        } else {
            layers.below.push(image);
        }
    }
    layers
}

/// Builds the GPU geometry for one scene image, or `None` when it cannot draw.
///
/// `uv` is exactly `source_rect / source_size`, per the [`ImageInstance`]
/// contract. `destination` stays unclipped so a negative row samples the
/// correct source rows; `clip` becomes the scissor.
pub fn build_image_quad(image: &ImageInstance, target: PhysicalSize) -> Option<ImageQuad> {
    let (source_w, source_h) = image.source_size;
    if source_w == 0 || source_h == 0 {
        return None;
    }
    if !image.destination.is_finite()
        || image.destination.is_empty()
        || !image.source_rect.is_finite()
        || image.source_rect.is_empty()
        || !image.clip.is_finite()
        || image.clip.is_empty()
    {
        return None;
    }
    // The frame adapter already intersects the crop with the source bounds.
    // A manual scene that violates that contract is skipped rather than
    // sampling outside the texture.
    if image.source_rect.x < 0.0
        || image.source_rect.y < 0.0
        || image.source_rect.right() > source_w as f32
        || image.source_rect.bottom() > source_h as f32
    {
        return None;
    }
    let scissor = scissor_for_clip(image.clip, target)?;
    let source_w = source_w as f32;
    let source_h = source_h as f32;
    Some(ImageQuad {
        rect: [
            image.destination.x,
            image.destination.y,
            image.destination.w,
            image.destination.h,
        ],
        uv: [
            image.source_rect.x / source_w,
            image.source_rect.y / source_h,
            image.source_rect.right() / source_w,
            image.source_rect.bottom() / source_h,
        ],
        scissor,
        source_id: image.source_id,
        placement_id: image.placement_id,
        layer_z: image_layer_z(image),
        above_text: is_above_text(image),
    })
}

/// Converts a logical clip rectangle into a whole-pixel scissor.
///
/// Edges are rounded outward (floor/ceil) and clamped to the target, matching
/// the GPU's scissor test. The destination quad still bounds the actual
/// coverage, so a fractional clip cannot bleed past the image.
fn scissor_for_clip(clip: Rect, target: PhysicalSize) -> Option<[u32; 4]> {
    if !clip.is_finite() || clip.is_empty() {
        return None;
    }
    let max_x = target.width as f32;
    let max_y = target.height as f32;
    let x0 = clip.x.floor().clamp(0.0, max_x) as u32;
    let y0 = clip.y.floor().clamp(0.0, max_y) as u32;
    let x1 = clip.right().ceil().clamp(0.0, max_x) as u32;
    let y1 = clip.bottom().ceil().clamp(0.0, max_y) as u32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some([x0, y0, x1 - x0, y1 - y0])
}

fn wgpu_color(color: [u8; 4]) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(color[0]) / 255.0,
        g: f64::from(color[1]) / 255.0,
        b: f64::from(color[2]) / 255.0,
        a: f64::from(color[3]) / 255.0,
    }
}

fn ensure_vertex_buffer(
    device: &wgpu::Device,
    buffer: &mut wgpu::Buffer,
    capacity: &mut u64,
    needed: u64,
    label: &str,
) -> bool {
    if needed == 0 || needed <= *capacity {
        return false;
    }
    let new_capacity = needed.max(256).next_power_of_two();
    *buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: new_capacity,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    *capacity = new_capacity;
    true
}

/// Textured-quad renderer sharing the offscreen [`TextTarget`] and wgpu
/// device/queue with [`GpuTextRenderer`].
pub struct GpuImageRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    source_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    screen_buffer: wgpu::Buffer,
    screen_bind_group: wgpu::BindGroup,
    screen_size: (u32, u32),
    registry: ImageTextureRegistry,
    source_bind_groups: HashMap<SourceId, wgpu::BindGroup>,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: u64,
    vertex_bytes: Vec<u8>,
    frame_index: u64,
    buffer_reallocations: u64,
    bind_group_creations: u64,
}

impl GpuImageRenderer {
    /// Creates the pipeline and an empty source registry on `device`/`queue`.
    ///
    /// Use the exact same device/queue as [`GpuTextRenderer`] and the target:
    /// all three must belong to one wgpu device.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tervo-wgpu-image-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });

        let screen_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tervo-wgpu-image-screen-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let source_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tervo-wgpu-image-source-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tervo-wgpu-image-pipeline-layout"),
            bind_group_layouts: &[Some(&screen_layout), Some(&source_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tervo-wgpu-image-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_image"),
                compilation_options: Default::default(),
                buffers: &[Some(ImageQuad::LAYOUT)],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_image"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FRAME_FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tervo-wgpu-image-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Nearest keeps source texels exact for the CPU reference; the
            // slice intentionally has no mipmap/filter quality path yet.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let screen_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tervo-wgpu-image-screen"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let screen_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tervo-wgpu-image-screen-bind-group"),
            layout: &screen_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen_buffer.as_entire_binding(),
            }],
        });
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tervo-wgpu-image-instances"),
            size: 256,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let registry = ImageTextureRegistry::new(device.clone(), queue.clone());
        Self {
            device,
            queue,
            pipeline,
            source_layout,
            sampler,
            screen_buffer,
            screen_bind_group,
            screen_size: (0, 0),
            registry,
            source_bind_groups: HashMap::new(),
            vertex_buffer,
            vertex_capacity: 256,
            vertex_bytes: Vec::new(),
            frame_index: 0,
            buffer_reallocations: 0,
            bind_group_creations: 0,
        }
    }

    /// Read-only view of the source texture registry (stats, sizes).
    pub fn registry(&self) -> &ImageTextureRegistry {
        &self.registry
    }

    /// Uploads one `Frame.images` RGBA payload once, keyed by `source_id`.
    ///
    /// Returns `Ok(false)` when this id was already uploaded; the renderer
    /// deliberately does not copy the pixels a second time.
    pub fn upload_source(
        &mut self,
        source_id: SourceId,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<bool, ImageError> {
        self.registry.upload(source_id, width, height, rgba)
    }

    /// Releases one source texture and its cached bind group.
    pub fn release_source(&mut self, source_id: SourceId) -> bool {
        self.source_bind_groups.remove(&source_id);
        self.registry.release(source_id)
    }

    /// Releases all sources whose id `keep` rejects, plus their bind groups.
    pub fn retain_sources(&mut self, mut keep: impl FnMut(SourceId) -> bool) -> usize {
        let released = self.registry.retain(&mut keep);
        let registry = &self.registry;
        self.source_bind_groups
            .retain(|source_id, _| registry.contains(*source_id));
        released
    }

    /// Releases every uploaded source texture.
    pub fn clear_sources(&mut self) {
        self.registry.clear();
        self.source_bind_groups.clear();
    }

    /// Renders `images` in the given order. `clear = None` preserves the
    /// target's current contents (needed when compositing over text).
    pub fn render(
        &mut self,
        target: &TextTarget,
        clear: Option<[u8; 4]>,
        images: &[&ImageInstance],
    ) -> Result<ImageFrameStats, ImageError> {
        self.render_with_geometry(target, clear, images)
            .map(|(stats, _)| stats)
    }

    /// Same as [`Self::render`], but returns the exact GPU geometry so a CPU
    /// reference can rasterize the same quads.
    pub fn render_with_geometry(
        &mut self,
        target: &TextTarget,
        clear: Option<[u8; 4]>,
        images: &[&ImageInstance],
    ) -> Result<(ImageFrameStats, ImageGeometry), ImageError> {
        let started = Instant::now();
        let size = target.size();
        self.write_screen(size);

        let mut geometry = ImageGeometry::default();
        for image in images {
            match build_image_quad(image, size) {
                Some(quad) => geometry.quads.push(quad),
                None => geometry.skipped += 1,
            }
        }

        // Validate and resolve every source before opening a render pass. A
        // missing upload is a lifecycle bug; reporting it early keeps the
        // target's previous contents intact instead of producing a half frame.
        let bind_group_before = self.bind_group_creations;
        for quad in &geometry.quads {
            self.ensure_source_bind_group(quad.source_id)?;
        }

        let needed = geometry.quads.len() as u64 * ImageQuad::BYTES;
        let reallocated = ensure_vertex_buffer(
            &self.device,
            &mut self.vertex_buffer,
            &mut self.vertex_capacity,
            needed,
            "tervo-wgpu-image-instances",
        );
        if reallocated {
            self.buffer_reallocations += 1;
        }
        self.vertex_bytes.clear();
        for quad in &geometry.quads {
            quad.append_bytes(&mut self.vertex_bytes);
        }
        if !self.vertex_bytes.is_empty() {
            self.queue
                .write_buffer(&self.vertex_buffer, 0, &self.vertex_bytes);
        }

        let mut draw_calls = 0u32;
        let mut render_passes = 0u32;
        if clear.is_some() || !geometry.quads.is_empty() {
            let load = match clear {
                Some(color) => wgpu::LoadOp::Clear(wgpu_color(color)),
                None => wgpu::LoadOp::Load,
            };
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("tervo-wgpu-image-encoder"),
                });
            {
                let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                    view: target.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                })];
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tervo-wgpu-image-pass"),
                    color_attachments: &color_attachments,
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                if !geometry.quads.is_empty() {
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &self.screen_bind_group, &[]);
                    pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                    for (index, quad) in geometry.quads.iter().enumerate() {
                        let bind_group = self
                            .source_bind_groups
                            .get(&quad.source_id)
                            .expect("source bind group resolved before the pass");
                        pass.set_bind_group(1, bind_group, &[]);
                        pass.set_scissor_rect(
                            quad.scissor[0],
                            quad.scissor[1],
                            quad.scissor[2],
                            quad.scissor[3],
                        );
                        pass.draw(0..6, index as u32..index as u32 + 1);
                        draw_calls += 1;
                    }
                }
            }
            self.queue.submit(Some(encoder.finish()));
            render_passes = 1;
        }

        self.frame_index = self.frame_index.wrapping_add(1);
        let stats = ImageFrameStats {
            frame: self.frame_index,
            width: size.width,
            height: size.height,
            images: geometry.quads.len(),
            skipped: geometry.skipped,
            draw_calls,
            render_passes,
            vertex_uploaded_bytes: self.vertex_bytes.len() as u64,
            source_gpu_bytes: self.registry.gpu_bytes(),
            source_count: self.registry.len(),
            target_gpu_bytes: u64::from(size.width) * u64::from(size.height) * 4,
            buffer_reallocations: u64::from(reallocated),
            bind_group_creations: self.bind_group_creations - bind_group_before,
            cpu_time: started.elapsed(),
        };
        Ok((stats, geometry))
    }

    /// Composites one full scene: text backgrounds, below-text images, glyphs,
    /// above-text images.
    ///
    /// This is the layering contract from the scene module:
    ///
    /// ```text
    /// background -> below-text images (z < 0) -> glyphs -> above-text images (z > 0)
    /// ```
    ///
    /// All source textures referenced by the scene must already be uploaded.
    /// The below/above image pass is submitted separately so the shared queue
    /// order is exactly the draw order; no full-screen copy is involved.
    pub fn render_scene(
        &mut self,
        text: &mut GpuTextRenderer,
        target: &TextTarget,
        clear: [u8; 4],
        sections: &[TextSection<'_>],
        scene: &Scene,
    ) -> Result<SceneLayerStats, CompositeError> {
        let layers = collect_image_layers(scene);
        let size = target.size();
        // Preflight both image layers: a missing upload must fail before the
        // target is cleared, otherwise the caller would present a half frame.
        for image in layers.below.iter().chain(layers.above.iter()) {
            if build_image_quad(image, size).is_some() && !self.registry.contains(image.source_id) {
                return Err(ImageError::MissingSource {
                    source_id: image.source_id,
                }
                .into());
            }
        }

        let prepared = text.prepare(target, sections)?;
        let text_passes = if prepared.glyph_count() > 0 { 2 } else { 1 };
        let mut text_draw_calls = text.draw_backgrounds(target, clear, &prepared)?;
        let below = self.render(target, None, &layers.below)?;
        text_draw_calls += text.draw_glyphs(target, &prepared)?;
        let above = self.render(target, None, &layers.above)?;
        let (text_stats, _geometry) = text.finish(prepared, text_draw_calls, text_passes);

        Ok(SceneLayerStats {
            text: Some(text_stats),
            below,
            above,
        })
    }

    fn write_screen(&mut self, size: PhysicalSize) {
        if self.screen_size == (size.width, size.height) {
            return;
        }
        let screen = [size.width as f32, size.height as f32, 0.0_f32, 0.0_f32];
        let mut bytes = Vec::with_capacity(16);
        for value in screen {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        self.queue.write_buffer(&self.screen_buffer, 0, &bytes);
        self.screen_size = (size.width, size.height);
    }

    fn ensure_source_bind_group(&mut self, source_id: SourceId) -> Result<(), ImageError> {
        if self.source_bind_groups.contains_key(&source_id) {
            return Ok(());
        }
        let view = self
            .registry
            .sources
            .get(&source_id)
            .map(|source| source.view.clone())
            .ok_or(ImageError::MissingSource { source_id })?;
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tervo-wgpu-image-source-bind-group"),
            layout: &self.source_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.source_bind_groups.insert(source_id, bind_group);
        self.bind_group_creations += 1;
        Ok(())
    }
}

impl fmt::Debug for GpuImageRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuImageRenderer")
            .field("registry", &self.registry)
            .field("bind_groups", &self.source_bind_groups.len())
            .field("frame_index", &self.frame_index)
            .field("buffer_reallocations", &self.buffer_reallocations)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tervo_graphics::{CompositePolicy, RowId};
    use tervo_scene::{SceneLayer, Viewport};

    fn instance(
        placement_id: u64,
        source_id: u64,
        policy: CompositePolicy,
        z_index: i32,
        above_text: bool,
        destination: Rect,
        source_rect: Rect,
        source_size: (u32, u32),
        clip: Rect,
    ) -> ImageInstance {
        ImageInstance {
            placement_id: PlacementId(placement_id),
            source_id: SourceId(source_id),
            source_rect,
            source_size,
            destination,
            clip,
            cells_w: 1,
            cells_h: 1,
            row: 0,
            col: 0,
            row_id: RowId(1),
            z_index,
            above_text,
            policy,
        }
    }

    fn simple(source_id: u64, policy: CompositePolicy, z_index: i32) -> ImageInstance {
        instance(
            source_id,
            source_id,
            policy,
            z_index,
            policy.is_above_text(),
            Rect::new(0.0, 0.0, 8.0, 8.0),
            Rect::new(0.0, 0.0, 8.0, 8.0),
            (8, 8),
            Rect::new(0.0, 0.0, 100.0, 100.0),
        )
    }

    #[test]
    fn source_crop_maps_to_normalized_uv() {
        let image = instance(
            1,
            7,
            CompositePolicy::BelowText,
            -1,
            false,
            Rect::new(0.0, 0.0, 20.0, 10.0),
            Rect::new(10.0, 5.0, 20.0, 10.0),
            (40, 20),
            Rect::new(0.0, 0.0, 20.0, 10.0),
        );
        let quad = build_image_quad(
            &image,
            PhysicalSize {
                width: 100,
                height: 100,
            },
        )
        .expect("valid quad");
        assert_eq!(quad.uv, [0.25, 0.25, 0.75, 0.75]);
        assert_eq!(quad.rect, [0.0, 0.0, 20.0, 10.0]);
        assert_eq!(quad.scissor, [0, 0, 20, 10]);
    }

    #[test]
    fn negative_row_keeps_unclipped_rect_and_scissors_to_viewport() {
        let image = instance(
            1,
            7,
            CompositePolicy::BelowText,
            -1,
            false,
            Rect::new(0.0, -10.0, 20.0, 30.0),
            Rect::new(0.0, 0.0, 20.0, 30.0),
            (20, 30),
            Rect::new(0.0, 0.0, 20.0, 20.0),
        );
        let quad = build_image_quad(
            &image,
            PhysicalSize {
                width: 100,
                height: 100,
            },
        )
        .expect("valid quad");
        // destination y stays negative: the shader samples source row 10+,
        // while the scissor clips the quad to the viewport.
        assert_eq!(quad.rect[1], -10.0);
        assert_eq!(quad.scissor, [0, 0, 20, 20]);
    }

    #[test]
    fn clip_rounds_outward_and_clamps_to_target() {
        let scissor = scissor_for_clip(
            Rect::new(-4.0, 10.2, 30.0, 9.6),
            PhysicalSize {
                width: 24,
                height: 24,
            },
        )
        .expect("non-empty scissor");
        assert_eq!(scissor, [0, 10, 24, 10]);
    }

    #[test]
    fn empty_clip_or_source_rect_is_skipped() {
        let mut image = simple(1, CompositePolicy::BelowText, -1);
        image.clip = Rect::new(50.0, 50.0, 10.0, 10.0);
        assert!(build_image_quad(
            &image,
            PhysicalSize {
                width: 40,
                height: 40
            }
        )
        .is_none());

        let mut image = simple(2, CompositePolicy::BelowText, -1);
        image.source_rect = Rect::new(0.0, 0.0, 9.0, 8.0);
        assert!(build_image_quad(
            &image,
            PhysicalSize {
                width: 40,
                height: 40
            }
        )
        .is_none());
    }

    #[test]
    fn collect_layers_sorts_by_policy_z_and_keeps_ties_stable() {
        // Layer order deliberately not sorted. Effective z is derived from the
        // policy, not from the raw protocol z-index.
        let below_a = simple(1, CompositePolicy::BelowText, -1);
        let below_b = simple(2, CompositePolicy::BelowText, -1);
        let above = simple(3, CompositePolicy::AboveText, 1);
        let scene = Scene {
            viewport: Viewport::default(),
            layers: vec![
                SceneLayer {
                    z_index: 1,
                    items: vec![DrawItem::Image(above.clone())],
                },
                SceneLayer {
                    z_index: -1,
                    items: vec![
                        DrawItem::Image(below_a.clone()),
                        DrawItem::Image(below_b.clone()),
                    ],
                },
            ],
            ..Scene::default()
        };
        let layers = collect_image_layers(&scene);
        assert_eq!(layers.below.len(), 2);
        assert_eq!(layers.above.len(), 1);
        assert_eq!(layers.below[0].placement_id, PlacementId(1));
        assert_eq!(layers.below[1].placement_id, PlacementId(2));
        assert_eq!(layers.above[0].placement_id, PlacementId(3));
        assert!(layers.above[0].above_text);
        assert_eq!(layers.above[0].policy, CompositePolicy::AboveText);
    }

    #[test]
    fn above_text_field_keeps_image_above_even_with_below_policy() {
        let image = instance(
            1,
            1,
            CompositePolicy::BelowText,
            -3,
            true,
            Rect::new(0.0, 0.0, 8.0, 8.0),
            Rect::new(0.0, 0.0, 8.0, 8.0),
            (8, 8),
            Rect::new(0.0, 0.0, 100.0, 100.0),
        );
        assert!(is_above_text(&image));
        // The scene layer z was policy-derived; group membership follows the
        // explicit above_text signal, never the raw z number.
        assert_eq!(image_layer_z(&image), -3);
    }
}
