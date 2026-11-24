//! End-to-end validation for WGPU image composition.
//!
//! These tests need the same headless wgpu adapter as `gpu_text.rs` and (for
//! the combined text/image test) a system monospace font. Every test compares
//! the GPU frame against a CPU reference rasterized from the *exact* quad
//! geometry the renderer submitted, so a failure points at the WGPU pipeline
//! (UV, scissor, blend, order), not at image decoding.
//!
//! Artifacts go to `$XTERVO_WGPU_IMAGE_OUT` or `target/wgpu-image-atlas/`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use xtervo_core::{Frame, ImageCrop, ImagePlacementMeta, InlineImage};
use xtervo_graphics::{CompositePolicy, PlacementId, Rect, RowId, SourceId};
use xtervo_render_wgpu::{
    collect_image_layers, compare_rgba, diff_rgba, discover_font_path, rasterize_glyphs_into,
    rasterize_image_reference, rasterize_images_into, rasterize_text_backgrounds_into,
    read_target_rgba8, request_headless_gpu, write_ppm, GpuImageRenderer, GpuTextRenderer,
    HeadlessGpu, ImageError, ImageSourceView, PixelDiff, TextLayout, TextSection, TextTarget,
};
use xtervo_scene::{
    scene_from_frame, DrawItem, ImageInstance, PhysicalSize, Scene, SceneLayer, TextAttributes,
    TextBatch, TextCell, Viewport,
};

const CLEAR: [u8; 4] = [16, 18, 24, 255];

struct Fixture {
    gpu: &'static HeadlessGpu,
    images: GpuImageRenderer,
    target: TextTarget,
}

fn test_gpu() -> &'static HeadlessGpu {
    static GPU: OnceLock<HeadlessGpu> = OnceLock::new();
    GPU.get_or_init(|| {
        let gpu = request_headless_gpu().unwrap_or_else(|error| {
            panic!("headless wgpu adapter required for image composition tests: {error}")
        });
        eprintln!(
            "adapter: {} backend={:?} device_type={:?} driver={}",
            gpu.info.name, gpu.info.backend, gpu.info.device_type, gpu.info.driver
        );
        gpu
    })
}

fn image_fixture(width: u32, height: u32) -> Fixture {
    let gpu = test_gpu();
    let images = GpuImageRenderer::new(gpu.device.clone(), gpu.queue.clone());
    let target =
        TextTarget::new(&gpu.device, PhysicalSize { width, height }).expect("create image target");
    Fixture {
        gpu,
        images,
        target,
    }
}

fn text_renderer(gpu: &HeadlessGpu) -> GpuTextRenderer {
    let font = discover_font_path().unwrap_or_else(|error| {
        panic!("monospace font required for combined text/image tests: {error}")
    });
    GpuTextRenderer::with_font_path(
        gpu.device.clone(),
        gpu.queue.clone(),
        font,
        Default::default(),
    )
    .expect("create text renderer")
}

fn readback(fixture: &Fixture) -> Vec<u8> {
    read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("read back offscreen target")
}

fn output_dir(test: &str) -> PathBuf {
    let root = std::env::var_os("XTERVO_WGPU_IMAGE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("target/wgpu-image-atlas")
        });
    root.join(test)
}

fn compare_and_write(
    test: &str,
    label: &str,
    width: u32,
    height: u32,
    gpu: &[u8],
    reference: &[u8],
) -> PixelDiff {
    let diff = compare_rgba(width, height, reference, gpu).expect("compare GPU and CPU reference");
    let amplification = diff_rgba(width, height, reference, gpu);
    let dir = output_dir(test);
    for (name, pixels) in [
        ("gpu", gpu),
        ("cpu-reference", reference),
        ("diff-x8", amplification.as_slice()),
    ] {
        let path = dir.join(format!("{label}-{name}.ppm"));
        write_ppm(&path, width, height, pixels).expect("write PPM artifact");
        eprintln!("wrote {}", path.display());
    }
    eprintln!(
        "{test}/{label}: {}x{} diff(mean={:.6} max={} pixels={} >1={} >2={})",
        width,
        height,
        diff.mean_abs,
        diff.max_abs,
        diff.differing_pixels,
        diff.pixels_over_1,
        diff.pixels_over_2
    );
    diff
}

fn assert_match(diff: &PixelDiff, label: &str) {
    assert!(
        diff.mean_abs <= 0.25,
        "{label}: GPU/CPU mean abs diff {:.6} exceeds 0.25",
        diff.mean_abs
    );
    assert!(
        diff.max_abs <= 2,
        "{label}: GPU/CPU max abs diff {} exceeds 2",
        diff.max_abs
    );
    assert_eq!(
        diff.pixels_over_2, 0,
        "{label}: {} pixels differ by more than 2",
        diff.pixels_over_2
    );
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * width + x) as usize) * 4;
    [
        rgba[offset],
        rgba[offset + 1],
        rgba[offset + 2],
        rgba[offset + 3],
    ]
}

fn solid(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        pixels.extend_from_slice(&color);
    }
    pixels
}

#[allow(clippy::too_many_arguments)]
fn image_instance(
    placement: u64,
    source: u64,
    above: bool,
    z_index: i32,
    destination: Rect,
    source_rect: Rect,
    source_size: (u32, u32),
    clip: Rect,
) -> ImageInstance {
    ImageInstance {
        placement_id: PlacementId(placement),
        source_id: SourceId(source),
        source_rect,
        source_size,
        destination,
        clip,
        cells_w: 0,
        cells_h: 0,
        row: 0,
        col: 0,
        row_id: RowId(0),
        z_index,
        above_text: above,
        policy: if above {
            CompositePolicy::AboveText
        } else {
            CompositePolicy::BelowText
        },
    }
}

fn sources<'a>(
    entries: &'a [(SourceId, u32, u32, Vec<u8>)],
) -> HashMap<SourceId, ImageSourceView<'a>> {
    entries
        .iter()
        .map(|(id, width, height, rgba)| {
            (
                *id,
                ImageSourceView {
                    width: *width,
                    height: *height,
                    rgba,
                },
            )
        })
        .collect()
}

/// Opaque native-size image: geometry, UV and blend must be pixel exact.
#[test]
fn opaque_image_matches_cpu_reference() {
    let mut fixture = image_fixture(96, 64);
    let (sw, sh) = (24u32, 16u32);
    let mut pixels = Vec::with_capacity((sw * sh * 4) as usize);
    for y in 0..sh {
        for x in 0..sw {
            let color = match (x / 8, y / 8) {
                (0, 0) => [255, 0, 0, 255],
                (1, 0) => [0, 255, 0, 255],
                (0, 1) => [0, 0, 255, 255],
                _ => [255, 255, 255, 255],
            };
            pixels.extend_from_slice(&color);
        }
    }
    assert!(fixture
        .images
        .upload_source(SourceId(1), sw, sh, &pixels)
        .expect("upload source"));

    let image = image_instance(
        1,
        1,
        false,
        -1,
        Rect::new(16.0, 12.0, 24.0, 16.0),
        Rect::new(0.0, 0.0, 24.0, 16.0),
        (sw, sh),
        Rect::new(16.0, 12.0, 24.0, 16.0),
    );
    let (stats, geometry) = fixture
        .images
        .render_with_geometry(&fixture.target, Some(CLEAR), &[&image])
        .expect("render image");
    assert_eq!(stats.images, 1);
    assert_eq!(stats.draw_calls, 1);
    assert_eq!(stats.render_passes, 1);
    assert_eq!(stats.vertex_uploaded_bytes, 32);
    assert_eq!(stats.source_gpu_bytes, u64::from(sw * sh * 4));

    let gpu = readback(&fixture);
    let entries = [(SourceId(1), sw, sh, pixels)];
    let map = sources(&entries);
    let reference = rasterize_image_reference(96, 64, CLEAR, &geometry, &map);
    let diff = compare_and_write(
        "opaque",
        "native",
        fixture.target.size().width,
        fixture.target.size().height,
        &gpu,
        &reference.rgba,
    );
    assert_match(&diff, "opaque");

    // TL quarter of the image is pure red and was drawn at (16,12).
    assert_eq!(pixel(&gpu, 96, 17, 13), [255, 0, 0, 255]);
    assert_eq!(pixel(&gpu, 96, 27, 13), [0, 255, 0, 255]);
    assert_eq!(pixel(&gpu, 96, 17, 23), [0, 0, 255, 255]);
}

/// Transparent source pixels must leave the clear color untouched; a partial
/// alpha must blend exactly once (straight-alpha contract).
#[test]
fn transparent_image_blends_with_background() {
    let mut fixture = image_fixture(64, 48);
    let (sw, sh) = (12u32, 12u32);
    let mut pixels = Vec::with_capacity((sw * sh * 4) as usize);
    for _y in 0..sh {
        for x in 0..sw {
            // Left half fully transparent, right half 50% red.
            let color = if x < sw / 2 {
                [255, 0, 0, 0]
            } else {
                [200, 40, 20, 128]
            };
            pixels.extend_from_slice(&color);
        }
    }
    fixture
        .images
        .upload_source(SourceId(2), sw, sh, &pixels)
        .expect("upload transparent source");
    let image = image_instance(
        2,
        2,
        false,
        -1,
        Rect::new(10.0, 10.0, 12.0, 12.0),
        Rect::new(0.0, 0.0, 12.0, 12.0),
        (sw, sh),
        Rect::new(10.0, 10.0, 12.0, 12.0),
    );
    let (_, geometry) = fixture
        .images
        .render_with_geometry(&fixture.target, Some(CLEAR), &[&image])
        .expect("render transparent image");
    let gpu = readback(&fixture);
    let entries = [(SourceId(2), sw, sh, pixels)];
    let map = sources(&entries);
    let reference = rasterize_image_reference(64, 48, CLEAR, &geometry, &map);
    let diff = compare_and_write("transparent", "alpha", 64, 48, &gpu, &reference.rgba);
    assert_match(&diff, "transparent");

    // Fully transparent half: target (including alpha) is unchanged.
    assert_eq!(pixel(&gpu, 64, 10, 10), CLEAR);
    // 50% red over the clear color: 16*0.498 + 200*0.502 = 108.4.
    let blended = pixel(&gpu, 64, 16, 16);
    assert!(
        blended[0].abs_diff(108) <= 1
            && blended[1].abs_diff(29) <= 1
            && blended[2].abs_diff(22) <= 1,
        "unexpected blended pixel {blended:?}"
    );
    assert_eq!(blended[3], 255);
}

/// `source_rect` selects the crop and `clip` becomes a hard scissor; pixels
/// between the clip edge and the destination right edge stay untouched.
#[test]
fn source_crop_and_clip_match_cpu_reference() {
    let mut fixture = image_fixture(80, 64);
    let (sw, sh) = (40u32, 30u32);
    let mut pixels = Vec::with_capacity((sw * sh * 4) as usize);
    for y in 0..sh {
        for x in 0..sw {
            let color = match (x / 20, y / 15) {
                (0, 0) => [255, 0, 0, 255],
                (1, 0) => [0, 255, 0, 255],
                (0, 1) => [0, 0, 255, 255],
                _ => [255, 255, 255, 255],
            };
            pixels.extend_from_slice(&color);
        }
    }
    fixture
        .images
        .upload_source(SourceId(3), sw, sh, &pixels)
        .expect("upload cropped source");
    let image = image_instance(
        3,
        3,
        false,
        -1,
        Rect::new(20.0, 10.0, 20.0, 15.0),
        Rect::new(10.0, 5.0, 20.0, 15.0),
        (sw, sh),
        // Right half of the destination is clipped away.
        Rect::new(20.0, 10.0, 10.0, 15.0),
    );
    let (stats, geometry) = fixture
        .images
        .render_with_geometry(&fixture.target, Some(CLEAR), &[&image])
        .expect("render cropped image");
    assert_eq!(geometry.quads[0].uv, [0.25, 1.0 / 6.0, 0.75, 2.0 / 3.0]);
    assert_eq!(geometry.quads[0].scissor, [20, 10, 10, 15]);
    assert_eq!(stats.draw_calls, 1);

    let gpu = readback(&fixture);
    let entries = [(SourceId(3), sw, sh, pixels)];
    let map = sources(&entries);
    let reference = rasterize_image_reference(80, 64, CLEAR, &geometry, &map);
    let diff = compare_and_write("crop", "scissor", 80, 64, &gpu, &reference.rgba);
    assert_match(&diff, "crop");

    // Inside the scissor: crop starts at source (10,5), still red.
    assert_eq!(pixel(&gpu, 80, 25, 12), [255, 0, 0, 255]);
    // Destination continues to x=40, but the clip ends at x=30.
    assert_eq!(pixel(&gpu, 80, 35, 12), CLEAR);
}

/// An image anchored above the viewport must sample the source rows that are
/// visible, not re-normalize UVs to the clipped rectangle.
#[test]
fn negative_row_samples_correct_source_rows() {
    let mut fixture = image_fixture(64, 48);
    let (sw, sh) = (16u32, 32u32);
    let mut pixels = Vec::with_capacity((sw * sh * 4) as usize);
    for y in 0..sh {
        let color = if y < 16 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        };
        for _x in 0..sw {
            pixels.extend_from_slice(&color);
        }
    }
    fixture
        .images
        .upload_source(SourceId(4), sw, sh, &pixels)
        .expect("upload tall source");
    let image = image_instance(
        4,
        4,
        false,
        -1,
        Rect::new(16.0, -16.0, 16.0, 32.0),
        Rect::new(0.0, 0.0, 16.0, 32.0),
        (sw, sh),
        Rect::new(16.0, 0.0, 16.0, 16.0),
    );
    let (_, geometry) = fixture
        .images
        .render_with_geometry(&fixture.target, Some(CLEAR), &[&image])
        .expect("render negative-row image");
    // Destination stays at y=-16 so the shader samples source rows 16..31;
    // only the scissor is clipped to the viewport.
    assert_eq!(geometry.quads[0].rect[1], -16.0);
    assert_eq!(geometry.quads[0].scissor, [16, 0, 16, 16]);

    let gpu = readback(&fixture);
    let entries = [(SourceId(4), sw, sh, pixels)];
    let map = sources(&entries);
    let reference = rasterize_image_reference(64, 48, CLEAR, &geometry, &map);
    let diff = compare_and_write("negative-row", "band", 64, 48, &gpu, &reference.rgba);
    assert_match(&diff, "negative row");

    // If the renderer had re-normalized UVs to `clip`, these would be red.
    assert_eq!(pixel(&gpu, 64, 20, 0), [0, 0, 255, 255]);
    assert_eq!(pixel(&gpu, 64, 20, 15), [0, 0, 255, 255]);
    assert_eq!(pixel(&gpu, 64, 16, 16), CLEAR);
}

/// Higher policy-aware z wins; equal z keeps scene insertion order stable.
#[test]
fn multiple_images_z_order_and_ties_are_stable() {
    let mut fixture = image_fixture(48, 48);
    let colors = [
        (1u64, [255, 0, 0, 255]),
        (2, [0, 255, 0, 255]),
        (3, [0, 0, 255, 255]),
    ];
    for (id, color) in colors {
        let pixels = solid(8, 8, color);
        fixture
            .images
            .upload_source(SourceId(id), 8, 8, &pixels)
            .expect("upload solid source");
    }
    let red = image_instance(
        1,
        1,
        false,
        -3,
        Rect::new(8.0, 8.0, 24.0, 24.0),
        Rect::new(0.0, 0.0, 8.0, 8.0),
        (8, 8),
        Rect::new(8.0, 8.0, 24.0, 24.0),
    );
    let green = image_instance(
        2,
        2,
        false,
        -2,
        Rect::new(8.0, 8.0, 24.0, 24.0),
        Rect::new(0.0, 0.0, 8.0, 8.0),
        (8, 8),
        Rect::new(8.0, 8.0, 24.0, 24.0),
    );
    let blue = image_instance(
        3,
        3,
        false,
        -2,
        Rect::new(8.0, 8.0, 24.0, 24.0),
        Rect::new(0.0, 0.0, 8.0, 8.0),
        (8, 8),
        Rect::new(8.0, 8.0, 24.0, 24.0),
    );

    // Layers are deliberately unsorted; z -2 contains green then blue, so blue
    // is the stable tie winner. z -3 (red) is drawn first and covered.
    let scene = Scene {
        layers: vec![
            SceneLayer {
                z_index: -2,
                items: vec![
                    DrawItem::Image(green.clone()),
                    DrawItem::Image(blue.clone()),
                ],
            },
            SceneLayer {
                z_index: -3,
                items: vec![DrawItem::Image(red.clone())],
            },
        ],
        ..Scene::default()
    };
    let layers = collect_image_layers(&scene);
    assert_eq!(
        layers
            .below
            .iter()
            .map(|image| image.placement_id)
            .collect::<Vec<_>>(),
        vec![PlacementId(1), PlacementId(2), PlacementId(3)]
    );
    let stats = fixture
        .images
        .render(&fixture.target, Some(CLEAR), &layers.below)
        .expect("render z stack");
    assert_eq!(stats.images, 3);
    assert_eq!(stats.draw_calls, 3);
    let gpu = readback(&fixture);
    assert_eq!(pixel(&gpu, 48, 20, 20), [0, 0, 255, 255]);

    // Swap the green/blue insertion order: the same z now resolves to green.
    let swapped = Scene {
        layers: vec![SceneLayer {
            z_index: -2,
            items: vec![DrawItem::Image(blue), DrawItem::Image(green)],
        }],
        ..Scene::default()
    };
    let layers = collect_image_layers(&swapped);
    let stats = fixture
        .images
        .render(&fixture.target, Some(CLEAR), &layers.below)
        .expect("render swapped z stack");
    assert_eq!(stats.images, 2);
    let gpu = readback(&fixture);
    assert_eq!(pixel(&gpu, 48, 20, 20), [0, 255, 0, 255]);
}

/// Text cell backgrounds are opaque, so a below-text image must composite
/// *between* backgrounds and glyphs, and an above-text image after glyphs.
/// The test chooses uncovered cells so the below image is observable at all.
#[test]
fn below_and_above_text_layers_match_cpu_reference() {
    let mut fixture = image_fixture(240, 120);
    let mut text = text_renderer(fixture.gpu);
    let metrics = text.metrics(16);
    let layout = TextLayout::from_metrics(metrics, 8.0, 8.0, 16);
    let cell_w = metrics.cell_width;
    let cell_h = metrics.cell_height;

    let background = 0x0011_2233;
    let mut batch = TextBatch::new(1);
    // Four blank cells: opaque backgrounds but no glyphs, so background color
    // and image visibility are unambiguous.
    for col in 0..4u16 {
        batch.push(TextCell {
            row: 0,
            col,
            text: if col == 0 {
                "M".to_string()
            } else {
                " ".to_string()
            },
            foreground: 0x00ff_ffff,
            background,
            attributes: TextAttributes::default(),
        });
    }

    let below_pixels = solid(8, 8, [255, 0, 255, 255]);
    let above_pixels = solid(8, 8, [0, 255, 255, 255]);
    fixture
        .images
        .upload_source(SourceId(10), 8, 8, &below_pixels)
        .expect("upload below source");
    fixture
        .images
        .upload_source(SourceId(11), 8, 8, &above_pixels)
        .expect("upload above source");

    // Below: starts under the text cells and extends two cells past them.
    let below = image_instance(
        10,
        10,
        false,
        -1,
        Rect::new(8.0, 8.0, cell_w * 2.0, cell_h),
        Rect::new(0.0, 0.0, 8.0, 8.0),
        (8, 8),
        Rect::new(8.0, 8.0, cell_w * 2.0, cell_h),
    );
    // Above: overlaps text cells 2..4 and covers their backgrounds/glyphs.
    let above = image_instance(
        11,
        11,
        true,
        1,
        Rect::new(8.0 + cell_w, 8.0, cell_w * 2.0, cell_h),
        Rect::new(0.0, 0.0, 8.0, 8.0),
        (8, 8),
        Rect::new(8.0 + cell_w, 8.0, cell_w * 2.0, cell_h),
    );
    let scene = Scene {
        layers: vec![
            SceneLayer {
                z_index: -1,
                items: vec![DrawItem::Image(below)],
            },
            SceneLayer {
                z_index: 0,
                items: vec![DrawItem::Text(batch.clone())],
            },
            SceneLayer {
                z_index: 1,
                items: vec![DrawItem::Image(above)],
            },
        ],
        ..Scene::default()
    };
    let layers = collect_image_layers(&scene);
    assert_eq!(layers.below.len(), 1);
    assert_eq!(layers.above.len(), 1);

    let sections = [TextSection::new(&batch, layout)];
    let size = fixture.target.size();
    let prepared = text
        .prepare(&fixture.target, &sections)
        .expect("prepare text");
    let mut text_draws = text
        .draw_backgrounds(&fixture.target, CLEAR, &prepared)
        .expect("draw text backgrounds");
    let (below_stats, below_geometry) = fixture
        .images
        .render_with_geometry(&fixture.target, None, &layers.below)
        .expect("draw below images");
    text_draws += text
        .draw_glyphs(&fixture.target, &prepared)
        .expect("draw glyphs");
    let (above_stats, above_geometry) = fixture
        .images
        .render_with_geometry(&fixture.target, None, &layers.above)
        .expect("draw above images");
    let (text_stats, text_geometry) = text.finish(prepared, text_draws, 2);

    assert_eq!(below_stats.render_passes, 1);
    assert_eq!(below_stats.draw_calls, 1);
    assert_eq!(above_stats.render_passes, 1);
    assert_eq!(above_stats.draw_calls, 1);
    assert_eq!(text_stats.render_passes, 2);
    // Text background pass clears the frame even though glyphs are separate.
    assert_eq!(text_stats.solid_quads, 4);
    assert_eq!(text_stats.glyph_quads, 1);

    let gpu = readback(&fixture);
    let entries = [
        (SourceId(10), 8, 8, below_pixels),
        (SourceId(11), 8, 8, above_pixels),
    ];
    let map = sources(&entries);
    let mut reference = Vec::with_capacity((size.width * size.height * 4) as usize);
    for _ in 0..u64::from(size.width) * u64::from(size.height) {
        reference.extend_from_slice(&CLEAR);
    }
    // Layered order: cell solids -> below images -> glyphs -> above images.
    rasterize_text_backgrounds_into(&mut reference, size.width, size.height, &text_geometry);
    rasterize_images_into(
        &mut reference,
        size.width,
        size.height,
        &below_geometry,
        &map,
    );
    rasterize_glyphs_into(
        &mut reference,
        size.width,
        size.height,
        &text_geometry,
        text.atlas(),
    );
    rasterize_images_into(
        &mut reference,
        size.width,
        size.height,
        &above_geometry,
        &map,
    );
    let diff = compare_and_write(
        "layers",
        "below-above",
        size.width,
        size.height,
        &gpu,
        &reference,
    );
    assert_match(&diff, "layers");

    let sample_y = 8.0 + cell_h * 0.5;
    // Cell 0: the below-text image is visible over the cell background, but
    // the glyph is drawn after it. Both signals must be present.
    let cell0 = text_geometry.solids[0].rect;
    let mut saw_below = false;
    let mut saw_glyph = false;
    for y in cell0[1] as u32..(cell0[1] + cell0[3]) as u32 {
        for x in cell0[0] as u32..(cell0[0] + cell0[2]) as u32 {
            let p = pixel(&gpu, size.width, x, y);
            if p == [255, 0, 255, 255] {
                saw_below = true;
            }
            // White glyph blended over magenta: green/blue rise above the
            // pure-image value.
            if p[1] > 128 && p[2] > 128 {
                saw_glyph = true;
            }
        }
    }
    assert!(
        saw_below,
        "below-text image must be visible over cell background"
    );
    assert!(saw_glyph, "glyph must composite after the below-text image");
    // Cells 1..3: above-text image overwrites the text background.
    let over = pixel(
        &gpu,
        size.width,
        (8.0 + cell_w * 1.5) as u32,
        sample_y as u32,
    );
    assert_eq!(over, [0, 255, 255, 255], "above-text image");
    // Cell 3: no image covers the opaque text background.
    let bg = pixel(
        &gpu,
        size.width,
        (8.0 + cell_w * 3.5) as u32,
        sample_y as u32,
    );
    assert_eq!(bg, [0x11, 0x22, 0x33, 255], "cell background");
    // Past both images: still the clear color.
    let clear = pixel(
        &gpu,
        size.width,
        (8.0 + cell_w * 6.0) as u32,
        sample_y as u32,
    );
    assert_eq!(clear, CLEAR);

    // The convenience path must submit the same passes in the same order.
    let second = TextTarget::new(
        &fixture.gpu.device,
        PhysicalSize {
            width: size.width,
            height: size.height,
        },
    )
    .expect("second target");
    fixture
        .images
        .render_scene(&mut text, &second, CLEAR, &sections, &scene)
        .expect("render_scene");
    let second_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &second).expect("read second");
    assert_eq!(second_pixels, gpu, "render_scene must match staged order");
}

/// Registry lifecycle: one upload per source id, explicit release, retention,
/// and a hard error when a scene outlives its pixels.
#[test]
fn source_upload_once_release_retain_and_missing() {
    let mut fixture = image_fixture(32, 32);
    let pixels = solid(8, 8, [1, 2, 3, 255]);
    assert!(fixture
        .images
        .upload_source(SourceId(20), 8, 8, &pixels)
        .expect("first upload"));
    // Same id again: no second GPU copy.
    assert!(!fixture
        .images
        .upload_source(SourceId(20), 8, 8, &pixels)
        .expect("duplicate upload is a no-op"));
    assert_eq!(fixture.images.registry().uploads(), 1);
    assert_eq!(fixture.images.registry().skipped_uploads(), 1);
    assert_eq!(fixture.images.registry().gpu_bytes(), 8 * 8 * 4);
    assert_eq!(
        fixture.images.registry().source_size(SourceId(20)),
        Some((8, 8))
    );

    let image = image_instance(
        20,
        20,
        false,
        -1,
        Rect::new(0.0, 0.0, 8.0, 8.0),
        Rect::new(0.0, 0.0, 8.0, 8.0),
        (8, 8),
        Rect::new(0.0, 0.0, 8.0, 8.0),
    );
    assert!(fixture
        .images
        .render(&fixture.target, Some(CLEAR), &[&image])
        .is_ok());

    assert!(fixture.images.release_source(SourceId(20)));
    let error = fixture
        .images
        .render(&fixture.target, Some(CLEAR), &[&image])
        .expect_err("released source must be reported");
    assert!(matches!(
        error,
        ImageError::MissingSource { source_id } if source_id == SourceId(20)
    ));
    assert_eq!(fixture.images.registry().len(), 0);
    assert_eq!(fixture.images.registry().gpu_bytes(), 0);

    // A new placement may re-upload the same source id after release.
    assert!(fixture
        .images
        .upload_source(SourceId(20), 8, 8, &pixels)
        .expect("re-upload after release"));
    let other = solid(4, 4, [9, 8, 7, 255]);
    assert!(fixture
        .images
        .upload_source(SourceId(21), 4, 4, &other)
        .expect("second source"));
    assert_eq!(fixture.images.registry().len(), 2);
    let released = fixture.images.retain_sources(|id| id == SourceId(20));
    assert_eq!(released, 1);
    assert_eq!(fixture.images.registry().len(), 1);
    assert_eq!(fixture.images.registry().gpu_bytes(), 8 * 8 * 4);
    assert!(fixture.images.registry().contains(SourceId(20)));
    assert!(!fixture.images.registry().contains(SourceId(21)));

    fixture.images.clear_sources();
    assert_eq!(fixture.images.registry().len(), 0);
    assert!(fixture.images.registry().is_empty());
}

/// An empty image pass either clears (first layer) or leaves the target alone
/// (subsequent layer), and never allocates a draw call.
#[test]
fn empty_frame_is_clear_only_or_preserved() {
    let mut fixture = image_fixture(48, 32);
    let magenta = [255, 0, 255, 255];
    let stats = fixture
        .images
        .render(&fixture.target, Some(magenta), &[])
        .expect("clear with empty image list");
    assert_eq!(stats.images, 0);
    assert_eq!(stats.draw_calls, 0);
    assert_eq!(stats.render_passes, 1);
    assert_eq!(stats.vertex_uploaded_bytes, 0);
    assert_eq!(readback(&fixture), solid(48, 32, magenta));

    let stats = fixture
        .images
        .render(&fixture.target, None, &[])
        .expect("preserve with empty image list");
    assert_eq!(stats.render_passes, 0);
    assert_eq!(stats.draw_calls, 0);
    assert_eq!(readback(&fixture), solid(48, 32, magenta));

    // A completely clipped image is skipped without a source upload check.
    let invisible = image_instance(
        30,
        30,
        false,
        -1,
        Rect::new(0.0, -20.0, 16.0, 10.0),
        Rect::new(0.0, 0.0, 16.0, 10.0),
        (16, 10),
        Rect::new(0.0, 0.0, 16.0, 0.0),
    );
    let stats = fixture
        .images
        .render(&fixture.target, None, &[&invisible])
        .expect("skip clipped image");
    assert_eq!(stats.images, 0);
    assert_eq!(stats.skipped, 1);
    assert_eq!(stats.render_passes, 0);
    assert_eq!(readback(&fixture), solid(48, 32, magenta));
}

/// End-to-end: `Frame.images` RGBA uploads by `source_id`, the frame adapter
/// preserves crop/negative row/above-text, and the scene layers into below and
/// above groups that the renderer draws unchanged.
#[test]
fn frame_images_upload_and_scene_composition() {
    let mut fixture = image_fixture(160, 96);
    let (cell_w, cell_h) = (8.0f32, 16.0f32);
    let viewport = Viewport {
        cell_w,
        cell_h,
        cols: 20,
        rows: 6,
    };

    let below_pixels = solid(16, 48, [255, 0, 255, 255]);
    let above_pixels = solid(8, 8, [0, 255, 255, 255]);
    let mut frame = Frame::new(viewport.cols, viewport.rows);
    frame.generation = 7;
    frame.full = true;
    frame.images = vec![
        InlineImage {
            id: 100,
            source_id: 900,
            row_id: 5,
            row: -1,
            col: 1,
            width: 16,
            height: 48,
            data: Arc::from(below_pixels.clone()),
            z_index: -2,
            above_text: false,
        },
        InlineImage {
            id: 101,
            source_id: 901,
            row_id: 6,
            row: 2,
            col: 4,
            width: 8,
            height: 8,
            data: Arc::from(above_pixels.clone()),
            z_index: 1,
            above_text: true,
        },
    ];
    frame.image_placements = vec![
        ImagePlacementMeta {
            placement_id: 100,
            image_id: 1,
            row_id: 5,
            source_rect: Some(ImageCrop {
                x: 0,
                y: 0,
                w: 16,
                h: 32,
            }),
            cells_w: 2,
            cells_h: 3,
            z_index: -2,
            above_text: false,
        },
        ImagePlacementMeta {
            placement_id: 101,
            image_id: 2,
            row_id: 6,
            source_rect: None,
            cells_w: 1,
            cells_h: 1,
            z_index: 1,
            above_text: true,
        },
    ];

    let scene = scene_from_frame(&frame, viewport);
    assert_eq!(scene.image_count(), 2);
    assert!(scene.text_batch().is_some());

    // The renderer's upload path is literally Frame.images: source_id + RGBA.
    for image in &frame.images {
        assert!(fixture
            .images
            .upload_source(
                SourceId(image.source_id),
                image.width,
                image.height,
                &image.data
            )
            .expect("upload Frame.images payload"));
    }
    assert_eq!(fixture.images.registry().len(), 2);

    let layers = collect_image_layers(&scene);
    assert_eq!(layers.below.len(), 1);
    assert_eq!(layers.above.len(), 1);
    assert_eq!(layers.below[0].source_id, SourceId(900));
    assert_eq!(layers.above[0].source_id, SourceId(901));
    assert!(layers.above[0].above_text);
    assert_eq!(layers.below[0].above_text, false);

    let target_size = fixture.target.size();
    let below_quad =
        xtervo_render_wgpu::build_image_quad(layers.below[0], target_size).expect("below quad");
    // row -1 projects to y=-16; clip starts at y=0. The source crop is 16x32
    // but explicit cells (2x3) make the destination 16x48.
    assert_eq!(below_quad.rect, [8.0, -16.0, 16.0, 48.0]);
    assert_eq!(below_quad.scissor, [8, 0, 16, 32]);
    assert_eq!(below_quad.uv, [0.0, 0.0, 1.0, 32.0 / 48.0]);
    let above_quad =
        xtervo_render_wgpu::build_image_quad(layers.above[0], target_size).expect("above quad");
    assert_eq!(above_quad.rect, [32.0, 32.0, 8.0, 8.0]);

    let (_, below_geometry) = fixture
        .images
        .render_with_geometry(&fixture.target, Some(CLEAR), &layers.below)
        .expect("render below frame image");
    let (_, above_geometry) = fixture
        .images
        .render_with_geometry(&fixture.target, None, &layers.above)
        .expect("render above frame image");
    let gpu = readback(&fixture);
    let entries = [
        (SourceId(900), 16, 48, below_pixels),
        (SourceId(901), 8, 8, above_pixels),
    ];
    let map = sources(&entries);
    let mut reference = Vec::with_capacity((target_size.width * target_size.height * 4) as usize);
    for _ in 0..u64::from(target_size.width) * u64::from(target_size.height) {
        reference.extend_from_slice(&CLEAR);
    }
    rasterize_images_into(
        &mut reference,
        target_size.width,
        target_size.height,
        &below_geometry,
        &map,
    );
    rasterize_images_into(
        &mut reference,
        target_size.width,
        target_size.height,
        &above_geometry,
        &map,
    );
    let diff = compare_and_write(
        "frame",
        "images",
        target_size.width,
        target_size.height,
        &gpu,
        &reference,
    );
    assert_match(&diff, "frame images");

    // Visible part of the below image starts at y=0 and samples source row 16
    // onwards (magenta), while the above image sits at (32,32).
    assert_eq!(pixel(&gpu, target_size.width, 10, 0), [255, 0, 255, 255]);
    assert_eq!(pixel(&gpu, target_size.width, 33, 33), [0, 255, 255, 255]);
    assert_eq!(pixel(&gpu, target_size.width, 100, 80), CLEAR);
}

#[cfg(target_os = "linux")]
fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// 2-minute memory gate for the image path.
///
/// Repeatedly renders 64 textured quads, churns source textures (upload +
/// release), forces GPU completion periodically, and checks that resident
/// memory and the registry's GPU byte count converge. Ignored by default
/// because it takes two minutes; run explicitly:
///
/// ```text
/// cargo test -p xtervo-render-wgpu --test gpu_images -- --ignored --nocapture
/// ```
#[cfg(target_os = "linux")]
#[test]
#[ignore = "2m memory gate; run explicitly with --ignored"]
fn image_render_soak_two_minutes() {
    use std::time::{Duration, Instant};

    let mut fixture = image_fixture(240, 120);
    let source_pixels = solid(8, 8, [40, 120, 200, 255]);
    let mut source_ids: Vec<u64> = (0..8).map(|index| 1000 + index).collect();
    for id in &source_ids {
        assert!(fixture
            .images
            .upload_source(SourceId(*id), 8, 8, &source_pixels)
            .expect("upload soak source"));
    }

    let start_rss = rss_kib().expect("read VmRSS");
    let started = Instant::now();
    let budget = Duration::from_secs(120);
    let mut iterations = 0u64;
    let mut churn_slot = 0usize;
    let mut next_source = 2000u64;
    let mut last_stats = None;
    while started.elapsed() < budget {
        if iterations % 256 == 255 {
            // Churn one slot: release the old texture and upload a new id, so
            // source creation/destruction is exercised without unbounded ids.
            let old = source_ids[churn_slot];
            assert!(fixture.images.release_source(SourceId(old)));
            assert!(fixture
                .images
                .upload_source(SourceId(next_source), 8, 8, &source_pixels)
                .expect("upload churn source"));
            source_ids[churn_slot] = next_source;
            next_source += 1;
            churn_slot = (churn_slot + 1) % source_ids.len();
        }

        let instances: Vec<ImageInstance> = (0..64)
            .map(|index| {
                let source = source_ids[index % source_ids.len()];
                let x = ((index % 8) as f32) * 30.0;
                let y = ((index / 8) as f32) * 15.0;
                image_instance(
                    index as u64 + 1,
                    source,
                    false,
                    -1,
                    Rect::new(x, y, 24.0, 12.0),
                    Rect::new(0.0, 0.0, 8.0, 8.0),
                    (8, 8),
                    Rect::new(x, y, 24.0, 12.0),
                )
            })
            .collect();
        let refs: Vec<&ImageInstance> = instances.iter().collect();
        let stats = fixture
            .images
            .render(&fixture.target, Some(CLEAR), &refs)
            .expect("soak render");
        assert_eq!(stats.images, 64);
        assert_eq!(stats.draw_calls, 64);
        assert_eq!(stats.vertex_uploaded_bytes, 64 * 32);
        last_stats = Some(stats);

        // Force completion periodically so queued submissions cannot grow
        // without bound and mask a leak.
        if iterations % 4096 == 0 {
            let _ = readback(&fixture);
            fixture
                .gpu
                .device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("soak device poll");
        }
        iterations += 1;
    }
    let _ = readback(&fixture);
    fixture
        .gpu
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("final soak device poll");

    let registry_bytes = fixture.images.registry().gpu_bytes();
    assert_eq!(registry_bytes, 8 * 8 * 8 * 4, "registry leaked textures");
    fixture.images.clear_sources();
    assert!(fixture.images.registry().is_empty());
    let end_rss = rss_kib().expect("read VmRSS");
    let growth = end_rss.saturating_sub(start_rss);
    let stats = last_stats.expect("at least one soak frame");
    eprintln!(
        "soak: iterations={iterations} budget={budget:?} last_cpu={:?} avg_wall={:?} \
         rss_start={start_rss}KiB rss_end={end_rss}KiB growth={growth}KiB \
         registry_gpu_bytes_before_clear={registry_bytes}",
        stats.cpu_time,
        started.elapsed() / iterations.max(1) as u32
    );
    assert!(
        growth < 128 * 1024,
        "RSS grew by {growth} KiB over {iterations} frames (limit 128 MiB)"
    );
}

/// A newer `prepare` must invalidate the older staged frame instead of
/// silently drawing the wrong vertex buffers.
#[test]
fn stale_staged_text_frame_is_rejected() {
    use xtervo_render_wgpu::TextRenderError;

    let mut fixture = image_fixture(64, 48);
    let mut text = text_renderer(fixture.gpu);
    let metrics = text.metrics(16);
    let layout = TextLayout::from_metrics(metrics, 0.0, 0.0, 16);
    let batch = TextBatch::new(1);
    let sections = [TextSection::new(&batch, layout)];

    let stale = text
        .prepare(&fixture.target, &sections)
        .expect("prepare stale");
    let fresh = text
        .prepare(&fixture.target, &sections)
        .expect("prepare fresh");
    let error = text
        .draw_backgrounds(&fixture.target, CLEAR, &stale)
        .expect_err("stale prepared frame must be rejected");
    assert!(matches!(error, TextRenderError::StalePreparedFrame));
    // The fresh frame is still valid and its backgrounds stage clears.
    text.draw_backgrounds(&fixture.target, CLEAR, &fresh)
        .expect("fresh frame draws");
    assert_eq!(readback(&fixture), solid(64, 48, CLEAR));
}
