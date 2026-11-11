//! Instanced WGPU text rendering from [`tervo_scene::TextBatch`].
//!
//! Pipeline shape:
//!
//! ```text
//! TextBatch cells
//!   -> per-cell background/underline solid instances
//!   -> per-ASCII-glyph instances (atlas UV + foreground color)
//!   -> one render pass, two instanced draw calls
//!   -> offscreen Rgba8Unorm target (no CPU terminal framebuffer)
//! ```
//!
//! Glyph rasterization happens only on an atlas miss. Reused glyphs cost no
//! upload. Instance buffers are grown only when the frame needs more instances;
//! they are not recreated every frame.
//!
//! Scope of this prototype: ASCII glyphs, foreground/background colors, bold /
//! italic faces when matching font files are installed, underline, and the
//! already-resolved `selected` colors. Non-ASCII characters deterministically
//! fall back to `?` and are counted in [`FrameStats::unknown_glyphs`]. Images,
//! cursor and z-order composition are intentionally out of scope.

use std::fmt;
use std::time::{Duration, Instant};

use tervo_scene::{PhysicalSize, TextBatch};

use crate::atlas::{
    discover_font_path, AtlasConfig, AtlasError, AtlasStats, FontMetrics, FontStyle, GlyphAtlas,
    GlyphLookup,
};
use crate::{BackendError, FRAME_FORMAT, FRAME_USAGE};

/// Vertex data for a solid rectangle (cell background or underline).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SolidInstance {
    /// `[x, y, width, height]` in physical pixels.
    pub rect: [f32; 4],
    /// Straight-alpha color.
    pub color: [f32; 4],
}

/// Vertex data for one glyph quad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphInstance {
    /// `[x, y, width, height]` in physical pixels.
    pub rect: [f32; 4],
    /// Atlas rectangle as `[u0, v0, u1, v1]`.
    pub uv: [f32; 4],
    /// Straight-alpha foreground color.
    pub color: [f32; 4],
}

impl SolidInstance {
    const BYTES: u64 = 32;
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: Self::BYTES,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
    };

    fn append_bytes(self, out: &mut Vec<u8>) {
        for value in self.rect.into_iter().chain(self.color) {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

impl GlyphInstance {
    const BYTES: u64 = 48;
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: Self::BYTES,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![
            0 => Float32x4,
            1 => Float32x4,
            2 => Float32x4,
        ],
    };

    fn append_bytes(self, out: &mut Vec<u8>) {
        for value in self.rect.into_iter().chain(self.uv).chain(self.color) {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// Cell grid and font size for one [`tervo_scene::TextBatch`].
///
/// Positions are rounded to whole pixels before rasterization. That makes a
/// glyph quad cover exact coverage texels and lets the CUDA-free CPU reference
/// compositor compare byte-for-byte instead of fighting fractional geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextLayout {
    /// Left edge of the cell grid in physical pixels.
    pub origin_x: f32,
    /// Top edge of the cell grid in physical pixels.
    pub origin_y: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    /// Glyph rasterization size in pixels.
    pub font_size_px: u16,
    /// Distance from a cell's top edge to the text baseline.
    pub baseline: f32,
}

impl TextLayout {
    /// Builds a layout from fontdue-derived [`FontMetrics`].
    pub fn from_metrics(
        metrics: FontMetrics,
        origin_x: f32,
        origin_y: f32,
        font_size_px: u16,
    ) -> Self {
        Self {
            origin_x,
            origin_y,
            cell_width: metrics.cell_width,
            cell_height: metrics.cell_height,
            font_size_px,
            baseline: metrics.baseline,
        }
    }

    /// Same layout translated by `(dx, dy)`.
    pub fn translated(self, dx: f32, dy: f32) -> Self {
        Self {
            origin_x: self.origin_x + dx,
            origin_y: self.origin_y + dy,
            ..self
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.font_size_px == 0 {
            return Err("font_size_px must be non-zero");
        }
        for (name, value) in [
            ("origin_x", self.origin_x),
            ("origin_y", self.origin_y),
            ("cell_width", self.cell_width),
            ("cell_height", self.cell_height),
            ("baseline", self.baseline),
        ] {
            if !value.is_finite() {
                let reason = match name {
                    "origin_x" => "origin_x must be finite",
                    "origin_y" => "origin_y must be finite",
                    "cell_width" => "cell_width must be finite",
                    "cell_height" => "cell_height must be finite",
                    _ => "baseline must be finite",
                };
                return Err(reason);
            }
        }
        if self.cell_width <= 0.0 {
            return Err("cell_width must be positive");
        }
        if self.cell_height <= 0.0 {
            return Err("cell_height must be positive");
        }
        Ok(())
    }
}

/// One text batch plus the layout it should be drawn with.
#[derive(Clone, Copy, Debug)]
pub struct TextSection<'a> {
    pub batch: &'a TextBatch,
    pub layout: TextLayout,
}

impl<'a> TextSection<'a> {
    pub fn new(batch: &'a TextBatch, layout: TextLayout) -> Self {
        Self { batch, layout }
    }
}

/// Per-frame counters. Byte counts are deltas for this frame unless the field
/// says `*_total`.
#[derive(Clone, Debug)]
pub struct FrameStats {
    pub frame: u64,
    pub width: u32,
    pub height: u32,
    pub cells: usize,
    pub solid_quads: usize,
    pub glyph_quads: usize,
    pub render_passes: u32,
    /// Actual draw calls: solid quads (if any) + glyph quads (if any).
    pub draw_calls: u32,
    pub atlas_hits: u64,
    pub atlas_misses: u64,
    pub atlas_rasterized: u64,
    /// `queue.write_texture` calls for new glyphs this frame.
    pub atlas_uploads: u64,
    /// Glyph bytes uploaded this frame; never a full-screen framebuffer.
    pub atlas_uploaded_bytes: u64,
    /// Instance bytes uploaded this frame (proportional to cells, not pixels).
    pub vertex_uploaded_bytes: u64,
    /// Atlas texture memory.
    pub atlas_gpu_bytes: u64,
    /// Offscreen target texture memory (not uploaded from CPU).
    pub target_gpu_bytes: u64,
    pub atlas_cached_glyphs: usize,
    /// Cumulative instance-buffer growth events.
    pub buffer_reallocations: u64,
    /// Non-ASCII characters replaced by `?` this frame.
    pub unknown_glyphs: u64,
    /// Bold/italic glyphs rendered with the regular face because the variant
    /// font file is absent.
    pub style_fallbacks: u64,
    /// Non-whitespace characters whose fontdue bitmap was empty.
    pub missing_outlines: u64,
    /// Control characters skipped by the cell layout.
    pub skipped_control: u64,
    /// CPU time spent building instances, uploading and submitting the frame.
    /// This excludes GPU execution (no timestamp queries in this prototype).
    pub cpu_time: Duration,
}

/// Failures that prevent a text frame from being submitted.
#[derive(Debug)]
pub enum TextRenderError {
    Atlas(AtlasError),
    InvalidLayout {
        section: usize,
        reason: &'static str,
    },
}

impl fmt::Display for TextRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Atlas(error) => write!(f, "glyph atlas: {error}"),
            Self::InvalidLayout { section, reason } => {
                write!(f, "text section {section} has an invalid layout: {reason}")
            }
        }
    }
}

impl std::error::Error for TextRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Atlas(error) => Some(error),
            Self::InvalidLayout { .. } => None,
        }
    }
}

impl From<AtlasError> for TextRenderError {
    fn from(error: AtlasError) -> Self {
        Self::Atlas(error)
    }
}

/// Offscreen `Rgba8Unorm` render target for the text pipeline.
///
/// The texture stays on the GPU. It is Slint-importable
/// (`TEXTURE_BINDING | RENDER_ATTACHMENT`) and `COPY_SRC` is present only so
/// tests/examples can read it back for pixel comparison. Production frames do
/// not download it.
pub struct TextTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: PhysicalSize,
}

impl TextTarget {
    pub fn new(device: &wgpu::Device, size: PhysicalSize) -> Result<Self, BackendError> {
        crate::validate_size(size, device.limits().max_texture_dimension_2d)?;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tervo-wgpu-text-target"),
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
        Ok(Self {
            texture,
            view,
            size,
        })
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn size(&self) -> PhysicalSize {
        self.size
    }

    /// GPU memory of the RGBA target, in bytes.
    pub fn gpu_bytes(&self) -> u64 {
        u64::from(self.size.width) * u64::from(self.size.height) * 4
    }
}

impl fmt::Debug for TextTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextTarget")
            .field("size", &self.size)
            .field("format", &self.texture.format())
            .finish_non_exhaustive()
    }
}

/// Instances built for one frame. Returned by
/// [`GpuTextRenderer::render_with_geometry`] so validation code can rasterize
/// the exact same geometry on the CPU.
#[derive(Clone, Debug, Default)]
pub struct TextGeometry {
    pub solids: Vec<SolidInstance>,
    pub glyphs: Vec<GlyphInstance>,
    pub cells: usize,
    pub unknown_glyphs: u64,
    pub style_fallbacks: u64,
    pub missing_outlines: u64,
    pub skipped_control: u64,
}

fn rgba(color: u32) -> [f32; 4] {
    [
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
        1.0,
    ]
}

struct GlyphPlacement {
    ch: char,
    px: u16,
    style: FontStyle,
    x: f32,
    row_top: f32,
    baseline: f32,
    color: [f32; 4],
}

fn push_glyph(
    atlas: &mut GlyphAtlas,
    placement: GlyphPlacement,
    frame: &mut TextGeometry,
) -> Result<(), AtlasError> {
    let GlyphLookup {
        slot,
        style_fallback,
    } = atlas.ensure(placement.ch, placement.px, placement.style)?;
    if style_fallback {
        frame.style_fallbacks += 1;
    }
    let Some(slot) = slot else {
        if !placement.ch.is_whitespace() {
            frame.missing_outlines += 1;
        }
        return Ok(());
    };
    if slot.is_empty() {
        return Ok(());
    }

    let x0 = placement.x.round() + slot.xmin as f32;
    let baseline_y = placement.row_top.round() + placement.baseline.round();
    let y0 = baseline_y - (slot.ymin + slot.h as i32) as f32;
    frame.glyphs.push(GlyphInstance {
        rect: [x0, y0, slot.w as f32, slot.h as f32],
        uv: slot.uv(atlas.width(), atlas.height()),
        color: placement.color,
    });
    Ok(())
}

fn build_frame(
    atlas: &mut GlyphAtlas,
    sections: &[TextSection<'_>],
) -> Result<TextGeometry, TextRenderError> {
    let mut frame = TextGeometry {
        solids: Vec::new(),
        glyphs: Vec::new(),
        cells: 0,
        unknown_glyphs: 0,
        style_fallbacks: 0,
        missing_outlines: 0,
        skipped_control: 0,
    };

    for (section_index, section) in sections.iter().enumerate() {
        section
            .layout
            .validate()
            .map_err(|reason| TextRenderError::InvalidLayout {
                section: section_index,
                reason,
            })?;
        let layout = section.layout;
        for cell in &section.batch.cells {
            frame.cells += 1;
            let x0 = (layout.origin_x + f32::from(cell.col) * layout.cell_width).round();
            let y0 = (layout.origin_y + f32::from(cell.row) * layout.cell_height).round();
            frame.solids.push(SolidInstance {
                rect: [x0, y0, layout.cell_width, layout.cell_height],
                color: rgba(cell.background),
            });

            let style = FontStyle::from_attributes(cell.attributes);
            let foreground = rgba(cell.foreground);
            let mut glyph_count = 0usize;
            for (index, ch) in cell.text.chars().enumerate() {
                if ch.is_ascii_control() {
                    frame.skipped_control += 1;
                    continue;
                }
                glyph_count += 1;
                let (ch, substituted) = if ch.is_ascii() {
                    (ch, false)
                } else {
                    ('?', true)
                };
                if substituted {
                    frame.unknown_glyphs += 1;
                }
                let x = x0 + index as f32 * layout.cell_width;
                push_glyph(
                    atlas,
                    GlyphPlacement {
                        ch,
                        px: layout.font_size_px,
                        style,
                        x,
                        row_top: y0,
                        baseline: layout.baseline,
                        color: foreground,
                    },
                    &mut frame,
                )?;
            }

            if cell.attributes.underline {
                let baseline_y = y0 + layout.baseline.round();
                let thickness = (f32::from(layout.font_size_px) / 14.0).round().max(1.0);
                frame.solids.push(SolidInstance {
                    rect: [
                        x0,
                        baseline_y + 2.0,
                        layout.cell_width * glyph_count.max(1) as f32,
                        thickness,
                    ],
                    color: foreground,
                });
            }
        }
    }
    Ok(frame)
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

/// Offscreen text renderer that owns the atlas, pipelines and reusable
/// instance buffers.
pub struct GpuTextRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    atlas: GlyphAtlas,
    solid_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    screen_buffer: wgpu::Buffer,
    solid_buffer: wgpu::Buffer,
    solid_capacity: u64,
    glyph_buffer: wgpu::Buffer,
    glyph_capacity: u64,
    solid_bytes: Vec<u8>,
    glyph_bytes: Vec<u8>,
    frame_index: u64,
    buffer_reallocations: u64,
}

impl fmt::Debug for GpuTextRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuTextRenderer")
            .field("atlas", &self.atlas)
            .field("frame_index", &self.frame_index)
            .field("buffer_reallocations", &self.buffer_reallocations)
            .finish_non_exhaustive()
    }
}

impl GpuTextRenderer {
    /// Loads the discovered system font and creates a default 1024x1024 atlas.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Result<Self, AtlasError> {
        let path = discover_font_path()?;
        Self::with_font_path(device, queue, path, AtlasConfig::default())
    }

    /// Creates a renderer with an explicit font file and atlas configuration.
    pub fn with_font_path(
        device: wgpu::Device,
        queue: wgpu::Queue,
        font_path: impl AsRef<std::path::Path>,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let atlas = GlyphAtlas::from_path(&device, &queue, font_path, config)?;
        Self::with_atlas(device, queue, atlas)
    }

    /// Creates a renderer from in-memory font bytes (deterministic tests).
    pub fn with_font_bytes(
        device: wgpu::Device,
        queue: wgpu::Queue,
        bytes: Vec<u8>,
        source: impl AsRef<std::path::Path>,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let atlas = GlyphAtlas::from_font_bytes(&device, &queue, bytes, source, config)?;
        Self::with_atlas(device, queue, atlas)
    }

    fn with_atlas(
        device: wgpu::Device,
        queue: wgpu::Queue,
        atlas: GlyphAtlas,
    ) -> Result<Self, AtlasError> {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tervo-wgpu-text-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("text.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tervo-wgpu-text-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let screen_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tervo-wgpu-text-screen"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tervo-wgpu-text-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tervo-wgpu-text-bind-group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: screen_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(atlas.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tervo-wgpu-text-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let color_target = wgpu::ColorTargetState {
            format: FRAME_FORMAT,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        };
        let targets = [Some(color_target.clone())];

        let solid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tervo-wgpu-text-solid-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_solid"),
                compilation_options: Default::default(),
                buffers: &[Some(SolidInstance::LAYOUT)],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_solid"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });
        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tervo-wgpu-text-glyph-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_glyph"),
                compilation_options: Default::default(),
                buffers: &[Some(GlyphInstance::LAYOUT)],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_glyph"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });

        let solid_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tervo-wgpu-text-solid-instances"),
            size: 256,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let glyph_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tervo-wgpu-text-glyph-instances"),
            size: 256,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            device,
            queue,
            atlas,
            solid_pipeline,
            glyph_pipeline,
            bind_group,
            screen_buffer,
            solid_buffer,
            solid_capacity: 256,
            glyph_buffer,
            glyph_capacity: 256,
            solid_bytes: Vec::new(),
            glyph_bytes: Vec::new(),
            frame_index: 0,
            buffer_reallocations: 0,
        })
    }

    /// Atlas accessor for metrics and the validation-only CPU reference.
    pub fn atlas(&self) -> &GlyphAtlas {
        &self.atlas
    }

    /// Monospace metrics for a font size.
    pub fn metrics(&self, font_size_px: u16) -> FontMetrics {
        self.atlas.metrics(font_size_px)
    }

    /// Font file backing the regular face.
    pub fn font_path(&self) -> &std::path::Path {
        self.atlas.font_path()
    }

    /// Renders `sections` into `target` in one render pass.
    ///
    /// The target is cleared to `clear` (RGBA8) regardless of whether any
    /// instances exist. Build failures (including atlas overflow) return before
    /// a command encoder is created, so the target keeps its previous contents.
    pub fn render(
        &mut self,
        target: &TextTarget,
        clear: [u8; 4],
        sections: &[TextSection<'_>],
    ) -> Result<FrameStats, TextRenderError> {
        self.render_with_geometry(target, clear, sections)
            .map(|(stats, _)| stats)
    }

    /// Same as [`Self::render`], but also returns the instance geometry for
    /// pixel-comparison tests/examples.
    pub fn render_with_geometry(
        &mut self,
        target: &TextTarget,
        clear: [u8; 4],
        sections: &[TextSection<'_>],
    ) -> Result<(FrameStats, TextGeometry), TextRenderError> {
        let started = Instant::now();
        let atlas_before = self.atlas.stats();
        let reallocations_before = self.buffer_reallocations;

        let frame = build_frame(&mut self.atlas, sections)?;
        let atlas_after = self.atlas.stats();

        let width = target.size().width;
        let height = target.size().height;
        let screen = [width as f32, height as f32, 0.0_f32, 0.0_f32];
        let mut screen_bytes = Vec::with_capacity(16);
        for value in screen {
            screen_bytes.extend_from_slice(&value.to_le_bytes());
        }
        self.queue
            .write_buffer(&self.screen_buffer, 0, &screen_bytes);

        let solid_needed = frame.solids.len() as u64 * SolidInstance::BYTES;
        let glyph_needed = frame.glyphs.len() as u64 * GlyphInstance::BYTES;
        if ensure_vertex_buffer(
            &self.device,
            &mut self.solid_buffer,
            &mut self.solid_capacity,
            solid_needed,
            "tervo-wgpu-text-solid-instances",
        ) {
            self.buffer_reallocations += 1;
        }
        if ensure_vertex_buffer(
            &self.device,
            &mut self.glyph_buffer,
            &mut self.glyph_capacity,
            glyph_needed,
            "tervo-wgpu-text-glyph-instances",
        ) {
            self.buffer_reallocations += 1;
        }

        self.solid_bytes.clear();
        for instance in &frame.solids {
            instance.append_bytes(&mut self.solid_bytes);
        }
        self.glyph_bytes.clear();
        for instance in &frame.glyphs {
            instance.append_bytes(&mut self.glyph_bytes);
        }
        if !self.solid_bytes.is_empty() {
            self.queue
                .write_buffer(&self.solid_buffer, 0, &self.solid_bytes);
        }
        if !self.glyph_bytes.is_empty() {
            self.queue
                .write_buffer(&self.glyph_buffer, 0, &self.glyph_bytes);
        }

        let mut draw_calls = 0u32;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tervo-wgpu-text-encoder"),
            });
        {
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: target.view(),
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: f64::from(clear[0]) / 255.0,
                        g: f64::from(clear[1]) / 255.0,
                        b: f64::from(clear[2]) / 255.0,
                        a: f64::from(clear[3]) / 255.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tervo-wgpu-text-pass"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.bind_group, &[]);
            if !frame.solids.is_empty() {
                pass.set_pipeline(&self.solid_pipeline);
                pass.set_vertex_buffer(0, self.solid_buffer.slice(..));
                pass.draw(0..6, 0..frame.solids.len() as u32);
                draw_calls += 1;
            }
            if !frame.glyphs.is_empty() {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_vertex_buffer(0, self.glyph_buffer.slice(..));
                pass.draw(0..6, 0..frame.glyphs.len() as u32);
                draw_calls += 1;
            }
        }
        self.queue.submit(Some(encoder.finish()));

        self.frame_index = self.frame_index.wrapping_add(1);
        let delta = |before: u64, after: u64| after.saturating_sub(before);
        let stats = FrameStats {
            frame: self.frame_index,
            width,
            height,
            cells: frame.cells,
            solid_quads: frame.solids.len(),
            glyph_quads: frame.glyphs.len(),
            render_passes: 1,
            draw_calls,
            atlas_hits: delta(atlas_before.hits, atlas_after.hits),
            atlas_misses: delta(atlas_before.misses, atlas_after.misses),
            atlas_rasterized: delta(atlas_before.rasterized, atlas_after.rasterized),
            atlas_uploads: delta(atlas_before.uploads, atlas_after.uploads),
            atlas_uploaded_bytes: delta(atlas_before.uploaded_bytes, atlas_after.uploaded_bytes),
            vertex_uploaded_bytes: (self.solid_bytes.len() + self.glyph_bytes.len()) as u64,
            atlas_gpu_bytes: self.atlas.gpu_bytes(),
            target_gpu_bytes: target.gpu_bytes(),
            atlas_cached_glyphs: self.atlas.cached_glyph_count(),
            buffer_reallocations: delta(reallocations_before, self.buffer_reallocations),
            unknown_glyphs: frame.unknown_glyphs,
            style_fallbacks: frame.style_fallbacks,
            missing_outlines: frame.missing_outlines,
            skipped_control: frame.skipped_control,
            cpu_time: started.elapsed(),
        };
        Ok((stats, frame))
    }

    /// Cumulative atlas counters. Useful for soak/perf checks.
    pub fn atlas_stats(&self) -> AtlasStats {
        self.atlas.stats()
    }
}

/// Reads an offscreen target back to tightly packed RGBA8.
///
/// Validation only: this is the one place in the prototype that copies a full
/// frame from GPU to CPU, and it is never called by [`GpuTextRenderer::render`]
/// or by the Slint presenter.
pub fn read_target_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    target: &TextTarget,
) -> Result<Vec<u8>, String> {
    const COPY_ALIGNMENT: u32 = 256;
    let size = target.size();
    let unpadded = size.width * 4;
    let padded = unpadded.div_ceil(COPY_ALIGNMENT) * COPY_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("tervo-wgpu-text-readback"),
        size: u64::from(padded) * u64::from(size.height),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("tervo-wgpu-text-readback-encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: target.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(size.height),
            },
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|error| format!("device.poll failed during readback: {error}"))?;
    receiver
        .recv()
        .map_err(|error| format!("readback map callback dropped: {error}"))?
        .map_err(|error| format!("readback map failed: {error}"))?;

    let mapped = slice
        .get_mapped_range()
        .map_err(|error| format!("readback get_mapped_range failed: {error}"))?;
    let mut rgba = Vec::with_capacity((unpadded * size.height) as usize);
    for row in 0..size.height as usize {
        let start = row * padded as usize;
        rgba.extend_from_slice(&mapped[start..start + unpadded as usize]);
    }
    drop(mapped);
    buffer.unmap();
    Ok(rgba)
}
