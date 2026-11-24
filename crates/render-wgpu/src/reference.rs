//! Validation-only CPU reference compositor and pixel differ.
//!
//! This does **not** produce a terminal framebuffer in the production path. It
//! is a test oracle: it composites the exact [`TextGeometry`] instances emitted
//! for a frame and samples the same CPU mirror of the glyph atlas that the GPU
//! uploaded. A GPU/CPU match therefore isolates the WGPU pipeline (geometry,
//! UV mapping, alpha blending), not fontdue's rasterizer.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use xtervo_graphics::SourceId;

use crate::atlas::GlyphAtlas;
use crate::image::{ImageGeometry, ImageQuad};
use crate::text::{GlyphInstance, TextGeometry};

fn blend_channel(dst: u8, src: u8, alpha: u8) -> u8 {
    // Fixed-function GPU blending runs in the target's numeric space, then
    // rounds once to UNORM8. Mirror that here; differences of one unit are
    // expected for some coverage/background combinations.
    let a = f32::from(alpha) / 255.0;
    let value = f32::from(dst) * (1.0 - a) + f32::from(src) * a;
    value.round().clamp(0.0, 255.0) as u8
}

fn blend_pixel(dst: &mut [u8], src: [u8; 4], alpha: u8) {
    for channel in 0..4 {
        dst[channel] = blend_channel(dst[channel], src[channel], alpha);
    }
}

fn fill_rect(rgba: &mut [u8], width: u32, height: u32, rect: [f32; 4], color: [f32; 4]) {
    let x0 = rect[0];
    let y0 = rect[1];
    let x1 = x0 + rect[2];
    let y1 = y0 + rect[3];
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return;
    }
    let start_x = x0.floor().max(0.0) as i32;
    let end_x = x1.ceil().min(width as f32) as i32;
    let start_y = y0.floor().max(0.0) as i32;
    let end_y = y1.ceil().min(height as f32) as i32;
    let src = [
        (color[0] * 255.0).round().clamp(0.0, 255.0) as u8,
        (color[1] * 255.0).round().clamp(0.0, 255.0) as u8,
        (color[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        (color[3] * 255.0).round().clamp(0.0, 255.0) as u8,
    ];
    for y in start_y..end_y {
        for x in start_x..end_x {
            // A pixel is covered when its center is inside [x0, x1).
            let cx = x as f32 + 0.5;
            let cy = y as f32 + 0.5;
            if cx < x0 || cx >= x1 || cy < y0 || cy >= y1 {
                continue;
            }
            let offset = (y as u32 * width + x as u32) as usize * 4;
            blend_pixel(&mut rgba[offset..offset + 4], src, src[3]);
        }
    }
}

/// One CPU-composited frame and how much of it glyph coverage touched.
#[derive(Clone, Debug, Default)]
pub struct ReferenceImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub glyph_pixels: u64,
    pub solid_pixels: u64,
    /// Image-quad pixels touched by [`rasterize_image_reference`].
    pub image_pixels: u64,
}

/// One source image for CPU reference compositing.
#[derive(Clone, Copy, Debug)]
pub struct ImageSourceView<'a> {
    pub width: u32,
    pub height: u32,
    /// Tightly packed straight-alpha RGBA8, same bytes uploaded to the GPU.
    pub rgba: &'a [u8],
}

fn fill_rgba(width: u32, height: u32, clear: [u8; 4]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for _ in 0..u64::from(width) * u64::from(height) {
        rgba.extend_from_slice(&clear);
    }
    rgba
}

/// Composites the frame geometry on the CPU using the atlas CPU mirror.
///
/// Geometry order matches the GPU pass: all solid instances first, then all
/// glyph instances. Single-pass section overlap is intentionally unsupported.
pub fn rasterize_reference(
    width: u32,
    height: u32,
    clear: [u8; 4],
    geometry: &TextGeometry,
    atlas: &GlyphAtlas,
) -> ReferenceImage {
    let mut rgba = fill_rgba(width, height, clear);
    let (glyph_pixels, solid_pixels) =
        rasterize_text_into(&mut rgba, width, height, geometry, atlas);
    ReferenceImage {
        width,
        height,
        rgba,
        glyph_pixels,
        solid_pixels,
        image_pixels: 0,
    }
}

/// Composites text geometry over the target's current pixels.
///
/// This is the layered counterpart of [`rasterize_reference`]: callers can
/// fill a clear color, composite below-text images, call this, then composite
/// above-text images. Returns `(glyph_pixels_touched, solid_quads)`.
pub fn rasterize_text_into(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    geometry: &TextGeometry,
    atlas: &GlyphAtlas,
) -> (u64, u64) {
    let solid_pixels = rasterize_text_backgrounds_into(rgba, width, height, geometry);
    let glyph_pixels = rasterize_glyphs_into(rgba, width, height, geometry, atlas);
    (glyph_pixels, solid_pixels)
}

/// Composites only the cell solids (backgrounds and underlines).
///
/// This is the first text stage in the layered path: images below text are
/// drawn after these solids and before glyphs.
pub fn rasterize_text_backgrounds_into(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    geometry: &TextGeometry,
) -> u64 {
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return 0;
    }
    let mut solid_pixels = 0u64;
    for solid in &geometry.solids {
        fill_rect(rgba, width, height, solid.rect, solid.color);
        solid_pixels += 1;
    }
    solid_pixels
}

/// Composites only the glyph quads over the target's current pixels.
pub fn rasterize_glyphs_into(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    geometry: &TextGeometry,
    atlas: &GlyphAtlas,
) -> u64 {
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return 0;
    }
    let atlas_width = atlas.width();
    let atlas_height = atlas.height();
    let atlas_pixels = atlas.cpu_pixels();
    let mut glyph_pixels = 0u64;
    for glyph in &geometry.glyphs {
        glyph_pixels += composite_glyph(
            rgba,
            width,
            height,
            glyph,
            atlas_pixels,
            atlas_width,
            atlas_height,
        );
    }
    glyph_pixels
}

/// GPU-mirroring straight-alpha blend for one texture sample.
///
/// `wgpu::BlendState::ALPHA_BLENDING` blends color with
/// `src * src.a + dst * (1 - src.a)` but blends the alpha channel with
/// `src.a + dst.a * (1 - src.a)`. Mirror both exactly instead of reusing the
/// glyph coverage blend, which treats alpha as one more color channel.
fn blend_image_pixel(dst: &mut [u8], src: [u8; 4]) {
    let alpha = f32::from(src[3]) / 255.0;
    if alpha <= 0.0 {
        return;
    }
    for channel in 0..3 {
        let value = f32::from(dst[channel]) * (1.0 - alpha) + f32::from(src[channel]) * alpha;
        dst[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    let alpha_value = f32::from(src[3]) + f32::from(dst[3]) * (1.0 - alpha);
    dst[3] = alpha_value.round().clamp(0.0, 255.0) as u8;
}

/// Composites every [`ImageQuad`] in `geometry` over `rgba`, in order.
///
/// Sampling mirrors the GPU pipeline: nearest-neighbour lookup at pixel
/// centers, scissor clipping, and alpha blending. Returns the number of
/// destination pixels touched.
pub fn rasterize_images_into(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    geometry: &ImageGeometry,
    sources: &HashMap<SourceId, ImageSourceView<'_>>,
) -> u64 {
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return 0;
    }
    let mut touched = 0u64;
    for quad in &geometry.quads {
        let Some(source) = sources.get(&quad.source_id) else {
            continue;
        };
        if source.width == 0
            || source.height == 0
            || source.rgba.len() != (source.width as usize) * (source.height as usize) * 4
        {
            continue;
        }
        touched += composite_image_quad(rgba, width, height, quad, *source);
    }
    touched
}

/// Composites image geometry over a fresh clear color and returns the frame.
pub fn rasterize_image_reference(
    width: u32,
    height: u32,
    clear: [u8; 4],
    geometry: &ImageGeometry,
    sources: &HashMap<SourceId, ImageSourceView<'_>>,
) -> ReferenceImage {
    let mut rgba = fill_rgba(width, height, clear);
    let image_pixels = rasterize_images_into(&mut rgba, width, height, geometry, sources);
    ReferenceImage {
        width,
        height,
        rgba,
        glyph_pixels: 0,
        solid_pixels: 0,
        image_pixels,
    }
}

fn composite_image_quad(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    quad: &ImageQuad,
    source: ImageSourceView<'_>,
) -> u64 {
    let [x0, y0, quad_w, quad_h] = quad.rect;
    if !(x0.is_finite() && y0.is_finite() && quad_w.is_finite() && quad_h.is_finite())
        || quad_w <= 0.0
        || quad_h <= 0.0
    {
        return 0;
    }
    let [scissor_x, scissor_y, scissor_w, scissor_h] = quad.scissor;
    let start_x = scissor_x.min(width);
    let end_x = scissor_x.saturating_add(scissor_w).min(width);
    let start_y = scissor_y.min(height);
    let end_y = scissor_y.saturating_add(scissor_h).min(height);
    let atlas_w = source.width as f32;
    let atlas_h = source.height as f32;
    let mut touched = 0u64;
    for y in start_y..end_y {
        let cy = y as f32 + 0.5;
        if cy < y0 || cy >= y0 + quad_h {
            continue;
        }
        let v = quad.uv[1] + (cy - y0) / quad_h * (quad.uv[3] - quad.uv[1]);
        let tex_y = ((v * atlas_h).floor() as i64).clamp(0, i64::from(source.height) - 1) as u32;
        for x in start_x..end_x {
            let cx = x as f32 + 0.5;
            if cx < x0 || cx >= x0 + quad_w {
                continue;
            }
            let u = quad.uv[0] + (cx - x0) / quad_w * (quad.uv[2] - quad.uv[0]);
            let tex_x = ((u * atlas_w).floor() as i64).clamp(0, i64::from(source.width) - 1) as u32;
            let source_offset = ((tex_y * source.width + tex_x) as usize) * 4;
            let sample = [
                source.rgba[source_offset],
                source.rgba[source_offset + 1],
                source.rgba[source_offset + 2],
                source.rgba[source_offset + 3],
            ];
            let offset = ((y * width + x) as usize) * 4;
            blend_image_pixel(&mut rgba[offset..offset + 4], sample);
            touched += 1;
        }
    }
    touched
}

fn composite_glyph(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    glyph: &GlyphInstance,
    atlas_pixels: &[u8],
    atlas_width: u32,
    atlas_height: u32,
) -> u64 {
    let rect = glyph.rect;
    let uv = glyph.uv;
    let gw = rect[2].round() as i32;
    let gh = rect[3].round() as i32;
    if gw <= 0 || gh <= 0 {
        return 0;
    }
    let x0 = rect[0].round() as i32;
    let y0 = rect[1].round() as i32;
    let color = [
        (glyph.color[0] * 255.0).round().clamp(0.0, 255.0) as u8,
        (glyph.color[1] * 255.0).round().clamp(0.0, 255.0) as u8,
        (glyph.color[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        (glyph.color[3] * 255.0).round().clamp(0.0, 255.0) as u8,
    ];
    let mut touched = 0u64;
    for py in 0..gh {
        let ty = y0 + py;
        if ty < 0 || ty >= height as i32 {
            continue;
        }
        // The GPU samples at pixel centers; the quad maps exactly onto texels.
        let v = uv[1] + (py as f32 + 0.5) / gh as f32 * (uv[3] - uv[1]);
        let ay = ((v * atlas_height as f32).floor() as i32).clamp(0, atlas_height as i32 - 1);
        for px in 0..gw {
            let tx = x0 + px;
            if tx < 0 || tx >= width as i32 {
                continue;
            }
            let u = uv[0] + (px as f32 + 0.5) / gw as f32 * (uv[2] - uv[0]);
            let ax = ((u * atlas_width as f32).floor() as i32).clamp(0, atlas_width as i32 - 1);
            let coverage = atlas_pixels[(ay * atlas_width as i32 + ax) as usize];
            if coverage == 0 {
                continue;
            }
            touched += 1;
            let offset = (ty as u32 * width + tx as u32) as usize * 4;
            blend_pixel(&mut rgba[offset..offset + 4], color, coverage);
        }
    }
    touched
}

/// Pixel-level difference between two tightly packed RGBA8 images.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PixelDiff {
    pub width: u32,
    pub height: u32,
    /// Mean absolute channel difference (0..=255).
    pub mean_abs: f64,
    /// Largest absolute channel difference.
    pub max_abs: u8,
    /// Pixels where any RGBA channel differs.
    pub differing_pixels: usize,
    /// Pixels where any channel differs by more than 1.
    pub pixels_over_1: usize,
    /// Pixels where any channel differs by more than 2.
    pub pixels_over_2: usize,
}

/// Compares two equal-sized RGBA8 images.
pub fn compare_rgba(
    width: u32,
    height: u32,
    expected: &[u8],
    actual: &[u8],
) -> Result<PixelDiff, String> {
    let expected_len = width as usize * height as usize * 4;
    if expected.len() != expected_len || actual.len() != expected_len {
        return Err(format!(
            "image size mismatch: {width}x{height} needs {expected_len} bytes, got {} and {}",
            expected.len(),
            actual.len()
        ));
    }
    let mut total = 0u64;
    let mut max_abs = 0u8;
    let mut differing_pixels = 0usize;
    let mut pixels_over_1 = 0usize;
    let mut pixels_over_2 = 0usize;
    for (expected_pixel, actual_pixel) in expected.chunks_exact(4).zip(actual.chunks_exact(4)) {
        let mut pixel_max = 0u8;
        for (&e, &a) in expected_pixel.iter().zip(actual_pixel) {
            let diff = e.abs_diff(a);
            total += u64::from(diff);
            pixel_max = pixel_max.max(diff);
        }
        max_abs = max_abs.max(pixel_max);
        if pixel_max > 0 {
            differing_pixels += 1;
        }
        if pixel_max > 1 {
            pixels_over_1 += 1;
        }
        if pixel_max > 2 {
            pixels_over_2 += 1;
        }
    }
    Ok(PixelDiff {
        width,
        height,
        mean_abs: total as f64 / (expected_len as f64),
        max_abs,
        differing_pixels,
        pixels_over_1,
        pixels_over_2,
    })
}

/// Amplified absolute difference image (validation artifacts).
pub fn diff_rgba(width: u32, height: u32, expected: &[u8], actual: &[u8]) -> Vec<u8> {
    let mut diff = Vec::with_capacity((width * height * 4) as usize);
    for (e, a) in expected.chunks_exact(4).zip(actual.chunks_exact(4)) {
        let mut pixel = [0u8; 4];
        for channel in 0..3 {
            pixel[channel] = e[channel].abs_diff(a[channel]).saturating_mul(8);
        }
        pixel[3] = 255;
        diff.extend_from_slice(&pixel);
    }
    diff
}

/// Writes a binary PPM (P6) image. Used for visual artifacts in tests/examples.
pub fn write_ppm(path: impl AsRef<Path>, width: u32, height: u32, rgba: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "PPM expected {expected} RGBA bytes for {width}x{height}, got {}",
                rgba.len()
            ),
        ));
    }
    let mut output = format!("P6\n{width} {height}\n255\n").into_bytes();
    output.reserve(width as usize * height as usize * 3);
    for pixel in rgba.chunks_exact(4) {
        output.extend_from_slice(&pixel[..3]);
    }
    std::fs::write(path, output)
}
