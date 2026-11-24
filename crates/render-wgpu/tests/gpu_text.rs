//! End-to-end validation for the WGPU text/glyph atlas prototype.
//!
//! These tests need a working headless wgpu adapter (Vulkan/GLES, hardware or
//! lavapipe) and a system monospace font. On this development machine the
//! adapter is llvmpipe/lavapipe under Xvfb-free headless Vulkan; the selected
//! adapter is printed by every test run.
//!
//! Artifacts (GPU, CPU reference, diff, scroll frames) are written as PPM to
//! `$XTERVO_WGPU_TEXT_OUT` or `target/wgpu-text-atlas/`.

use std::path::PathBuf;
use std::sync::OnceLock;

use xtervo_render_wgpu::{
    compare_rgba, diff_rgba, discover_font_path, rasterize_reference, read_target_rgba8,
    request_headless_gpu, write_ppm, AtlasConfig, AtlasError, FontStyle, GpuImageRenderer,
    GpuTextRenderer, HeadlessGpu, TextLayout, TextRenderError, TextSection, TextTarget,
};
use xtervo_scene::{
    BackgroundInstance, CursorState, DrawItem, PhysicalSize, Scene, SceneLayer, TextAttributes,
    TextBatch, TextCell, Viewport, BACKGROUND_LAYER_Z, TEXT_LAYER_Z,
};

const CLEAR: [u8; 4] = [16, 18, 24, 255];

struct Fixture {
    gpu: &'static HeadlessGpu,
    renderer: GpuTextRenderer,
    target: TextTarget,
}

/// One process-wide device avoids repeatedly creating/destroying lavapipe
/// devices (which is both slow and has been a source of teardown crashes).
fn test_gpu() -> &'static HeadlessGpu {
    static GPU: OnceLock<HeadlessGpu> = OnceLock::new();
    GPU.get_or_init(|| {
        let gpu = request_headless_gpu().unwrap_or_else(|error| {
            panic!("headless wgpu adapter required for the glyph-atlas tests: {error}")
        });
        eprintln!(
            "adapter: {} backend={:?} device_type={:?} driver={}",
            gpu.info.name, gpu.info.backend, gpu.info.device_type, gpu.info.driver
        );
        gpu
    })
}

fn fixture(width: u32, height: u32) -> Fixture {
    fixture_with_atlas(width, height, AtlasConfig::default())
}

/// Fixture with **no** system fallback chain: used by tests that need a
/// guaranteed-missing glyph, independent of which CJK/emoji fonts are installed.
fn fixture_without_fallbacks(width: u32, height: u32) -> Fixture {
    let gpu = test_gpu();
    let font = discover_font_path().expect("system font");
    let bytes = std::fs::read(&font).expect("read system font");
    let renderer = GpuTextRenderer::with_font_bytes(
        gpu.device.clone(),
        gpu.queue.clone(),
        bytes,
        &font,
        AtlasConfig::default(),
    )
    .expect("create no-fallback atlas renderer");
    let target =
        TextTarget::new(&gpu.device, PhysicalSize { width, height }).expect("create text target");
    Fixture {
        gpu,
        renderer,
        target,
    }
}

fn fixture_with_atlas(width: u32, height: u32, atlas_config: AtlasConfig) -> Fixture {
    let gpu = test_gpu();
    let font = discover_font_path().unwrap_or_else(|error| {
        panic!("monospace font required for the glyph-atlas tests: {error}")
    });
    eprintln!("font: {}", font.display());
    let renderer =
        GpuTextRenderer::with_font_path(gpu.device.clone(), gpu.queue.clone(), font, atlas_config)
            .expect("create atlas renderer");
    let target =
        TextTarget::new(&gpu.device, PhysicalSize { width, height }).expect("create text target");
    Fixture {
        gpu,
        renderer,
        target,
    }
}

fn output_dir(test: &str) -> PathBuf {
    let root = std::env::var_os("XTERVO_WGPU_TEXT_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("target/wgpu-text-atlas")
        });
    root.join(test)
}

fn cell(row: u16, col: u16, text: &str, fg: u32, bg: u32, attrs: TextAttributes) -> TextCell {
    TextCell {
        row,
        col,
        text: text.to_string(),
        foreground: fg,
        background: bg,
        attributes: attrs,
    }
}

fn push_line(batch: &mut TextBatch, row: u16, text: &str, fg: u32, bg: u32, attrs: TextAttributes) {
    for (index, ch) in text.chars().enumerate() {
        batch.push(cell(row, index as u16, &ch.to_string(), fg, bg, attrs));
    }
}

fn layout(renderer: &GpuTextRenderer, px: u16, x: f32, y: f32) -> TextLayout {
    TextLayout::from_metrics(renderer.metrics(px), x, y, px)
}

fn write_artifacts(
    test: &str,
    label: &str,
    width: u32,
    height: u32,
    gpu: &[u8],
    reference: &[u8],
    diff: &[u8],
) {
    let dir = output_dir(test);
    for (name, pixels) in [
        ("gpu", gpu),
        ("cpu-reference", reference),
        ("diff-x8", diff),
    ] {
        let path = dir.join(format!("{label}-{name}.ppm"));
        write_ppm(&path, width, height, pixels).expect("write PPM artifact");
        eprintln!("wrote {}", path.display());
    }
}

fn render_and_compare(
    fixture: &mut Fixture,
    test: &str,
    label: &str,
    sections: &[TextSection<'_>],
) -> xtervo_render_wgpu::PixelDiff {
    let (stats, geometry) = fixture
        .renderer
        .render_with_geometry(&fixture.target, CLEAR, sections)
        .expect("render text frame");
    let size = fixture.target.size();
    let gpu = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("read back offscreen text target");
    let reference = rasterize_reference(
        size.width,
        size.height,
        CLEAR,
        &geometry,
        fixture.renderer.atlas(),
    );
    let diff = compare_rgba(size.width, size.height, &reference.rgba, &gpu)
        .expect("compare GPU and CPU reference");
    let amplification = diff_rgba(size.width, size.height, &reference.rgba, &gpu);
    eprintln!(
        "{test}/{label}: {}x{} cells={} glyphs={} solids={} draw_calls={} \
         atlas(hits={} misses={} uploads={} bytes={}) cpu={:?} \
         diff(mean={:.6} max={} pixels={} >1={} >2={})",
        size.width,
        size.height,
        stats.cells,
        stats.glyph_quads,
        stats.solid_quads,
        stats.draw_calls,
        stats.atlas_hits,
        stats.atlas_misses,
        stats.atlas_uploads,
        stats.atlas_uploaded_bytes,
        stats.cpu_time,
        diff.mean_abs,
        diff.max_abs,
        diff.differing_pixels,
        diff.pixels_over_1,
        diff.pixels_over_2,
    );
    write_artifacts(
        test,
        label,
        size.width,
        size.height,
        &gpu,
        &reference.rgba,
        &amplification,
    );
    diff
}

fn assert_tight_match(diff: &xtervo_render_wgpu::PixelDiff, label: &str) {
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

fn sample_scene() -> (TextBatch, TextBatch, TextBatch, TextBatch) {
    let mut main = TextBatch::new(1);
    push_line(
        &mut main,
        0,
        "XTERVO WGPU TEXT ATLAS",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes::default(),
    );
    push_line(
        &mut main,
        1,
        "abc DEF 012 !@#$%^&*()",
        0x007e_e787,
        CLEAR_RGB,
        TextAttributes::default(),
    );
    push_line(
        &mut main,
        2,
        "colored cells and underline",
        0x00ff_d33d,
        0x001f_2937,
        TextAttributes {
            underline: true,
            ..TextAttributes::default()
        },
    );
    push_line(
        &mut main,
        3,
        "bold italic and selected",
        0x00ff_79c6,
        CLEAR_RGB,
        TextAttributes {
            bold: true,
            italic: true,
            ..TextAttributes::default()
        },
    );
    if let Some(first) = main.cells.first_mut() {
        first.attributes.selected = true;
    }

    let mut big = TextBatch::new(1);
    push_line(
        &mut big,
        0,
        "BIG 28px glyphs",
        0x0067_e8f9,
        CLEAR_RGB,
        TextAttributes::default(),
    );

    let mut small = TextBatch::new(1);
    for (row, line) in [
        "small 13px line one",
        "line two: 0123456789",
        "line three: ~!@#$%",
    ]
    .iter()
    .enumerate()
    {
        push_line(
            &mut small,
            row as u16,
            line,
            0x00d1_d5db,
            CLEAR_RGB,
            TextAttributes::default(),
        );
    }

    let mut scroll = TextBatch::new(1);
    for row in 0..3u16 {
        let text = format!("scroll row {row}: XTERVO-GPU-ATLAS");
        push_line(
            &mut scroll,
            row,
            &text,
            0x00f0_abfc,
            CLEAR_RGB,
            TextAttributes::default(),
        );
    }

    (main, big, small, scroll)
}

const CLEAR_RGB: u32 = 0x0010_1218;

#[test]
fn gpu_text_matches_cpu_reference_for_multiline_multisize_scene() {
    let mut fixture = fixture(640, 360);
    let (main, big, small, _) = sample_scene();

    let main_layout = layout(&fixture.renderer, 18, 12.0, 12.0);
    let big_layout = layout(&fixture.renderer, 28, 12.0, 130.0);
    let small_layout = layout(&fixture.renderer, 13, 12.0, 180.0);
    let sections = [
        TextSection::new(&main, main_layout),
        TextSection::new(&big, big_layout),
        TextSection::new(&small, small_layout),
    ];

    let stats = fixture
        .renderer
        .render(&fixture.target, CLEAR, &sections)
        .expect("render sample scene");
    assert_eq!(stats.render_passes, 1);
    assert_eq!(stats.draw_calls, 2);
    assert!(stats.glyph_quads > 50);
    assert!(stats.atlas_uploads > 0);
    assert_eq!(stats.atlas_gpu_bytes, 1024 * 1024);
    assert_eq!(
        stats.target_gpu_bytes,
        u64::from(fixture.target.size().width) * u64::from(fixture.target.size().height) * 4
    );
    assert!(stats.vertex_uploaded_bytes > 0);
    // The offscreen target receives no full-frame CPU upload: only glyph
    // rectangles (atlas misses) and instance data are written from the CPU.
    let full_screen_bytes =
        u64::from(fixture.target.size().width) * u64::from(fixture.target.size().height) * 4;
    assert!(
        stats.atlas_uploaded_bytes + stats.vertex_uploaded_bytes < full_screen_bytes,
        "CPU uploads {} exceeded one full-screen RGBA image ({full_screen_bytes}); \
         this would indicate a CPU framebuffer upload path",
        stats.atlas_uploaded_bytes + stats.vertex_uploaded_bytes
    );

    let diff = render_and_compare(&mut fixture, "multiline", "sample", &sections);
    assert_tight_match(&diff, "multiline/multisize");
}

#[test]
fn scroll_frames_match_reference_and_do_not_reupload_atlas() {
    let mut fixture = fixture(520, 180);
    let mut previous: Option<Vec<u8>> = None;
    let mut cpu_times = Vec::new();

    for offset in 0..5u16 {
        let mut batch = TextBatch::new(u64::from(offset) + 1);
        for row in 0..7u16 {
            let letter = char::from(b'A' + ((row + offset) % 7) as u8);
            let text = format!("scroll line {letter} XTERVO");
            push_line(
                &mut batch,
                row,
                &text,
                0x00e5_e7eb,
                CLEAR_RGB,
                TextAttributes::default(),
            );
        }
        let text_layout = layout(&fixture.renderer, 16, 10.0, 10.0);
        let sections = [TextSection::new(&batch, text_layout)];

        let (stats, geometry) = fixture
            .renderer
            .render_with_geometry(&fixture.target, CLEAR, &sections)
            .expect("render scroll frame");
        let size = fixture.target.size();
        let gpu = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
            .expect("scroll readback");
        let reference = rasterize_reference(
            size.width,
            size.height,
            CLEAR,
            &geometry,
            fixture.renderer.atlas(),
        );
        let diff =
            compare_rgba(size.width, size.height, &reference.rgba, &gpu).expect("scroll compare");
        assert_tight_match(&diff, &format!("scroll offset {offset}"));

        if offset > 0 {
            assert_eq!(
                stats.atlas_uploads, 0,
                "scroll frame {offset} re-uploaded {} atlas glyphs; cached glyphs must not upload again",
                stats.atlas_uploads
            );
            assert_eq!(stats.atlas_misses, 0);
            assert_eq!(stats.buffer_reallocations, 0);
            let previous = previous.as_ref().expect("previous frame");
            assert_ne!(
                previous,
                &gpu,
                "scroll frame {offset} is identical to frame {}",
                offset - 1
            );
        }
        assert_eq!(stats.draw_calls, 2);
        previous = Some(gpu);
        cpu_times.push(stats.cpu_time);
        eprintln!(
            "scroll offset={offset} cpu={:?} glyphs={} uploads={}",
            stats.cpu_time, stats.glyph_quads, stats.atlas_uploads
        );
    }

    let total: std::time::Duration = cpu_times.iter().sum();
    let average = total / cpu_times.len() as u32;
    eprintln!("scroll average CPU frame time: {average:?}");
    assert!(
        average < std::time::Duration::from_millis(50),
        "scroll average CPU frame time {average:?} is unexpectedly high"
    );
}

#[test]
fn empty_scene_clears_without_draw_calls_or_uploads() {
    let mut fixture = fixture(320, 200);
    let (stats, geometry) = fixture
        .renderer
        .render_with_geometry(&fixture.target, [10, 20, 30, 255], &[])
        .expect("render empty scene");
    assert_eq!(stats.cells, 0);
    assert_eq!(stats.glyph_quads, 0);
    assert_eq!(stats.solid_quads, 0);
    assert_eq!(stats.draw_calls, 0);
    assert_eq!(stats.render_passes, 1);
    assert_eq!(stats.atlas_uploads, 0);
    let gpu = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("empty readback");
    assert!(
        gpu.chunks_exact(4).all(|pixel| pixel == [10, 20, 30, 255]),
        "empty scene must be a pure clear of the target"
    );
    let reference = rasterize_reference(
        fixture.target.size().width,
        fixture.target.size().height,
        [10, 20, 30, 255],
        &geometry,
        fixture.renderer.atlas(),
    );
    let diff = compare_rgba(
        fixture.target.size().width,
        fixture.target.size().height,
        &reference.rgba,
        &gpu,
    )
    .expect("empty compare");
    assert_eq!(diff.max_abs, 0);

    // A batch with backgrounds but no text still issues one solid draw call.
    let mut background_only = TextBatch::new(2);
    background_only.push(cell(
        0,
        0,
        "",
        0x00ff_ffff,
        0x0033_4455,
        TextAttributes::default(),
    ));
    let text_layout = layout(&fixture.renderer, 16, 4.0, 4.0);
    let stats = fixture
        .renderer
        .render(
            &fixture.target,
            [10, 20, 30, 255],
            &[TextSection::new(&background_only, text_layout)],
        )
        .expect("background-only render");
    assert_eq!(stats.glyph_quads, 0);
    assert_eq!(stats.solid_quads, 1);
    assert_eq!(stats.draw_calls, 1);
}

#[test]
fn resize_recreates_target_and_preserves_pixel_geometry() {
    let mut fixture = fixture(320, 200);
    let (main, _, _, _) = sample_scene();
    let mut batch = TextBatch::new(7);
    for source in &main.cells {
        if source.row == 0 {
            batch.push(source.clone());
        }
    }
    let text_layout = layout(&fixture.renderer, 18, 8.0, 8.0);
    let sections = [TextSection::new(&batch, text_layout)];
    let stats = fixture
        .renderer
        .render(&fixture.target, CLEAR, &sections)
        .expect("render before resize");
    assert!(stats.glyph_quads > 0);
    let old = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("readback before resize");

    let resized = TextTarget::new(
        &fixture.gpu.device,
        PhysicalSize {
            width: 640,
            height: 400,
        },
    )
    .expect("resize target");
    let stats = fixture
        .renderer
        .render(&resized, CLEAR, &sections)
        .expect("render after resize");
    assert_eq!(stats.width, 640);
    assert_eq!(stats.height, 400);
    assert!(
        stats.atlas_uploads == 0,
        "same glyphs must stay cached across resize"
    );
    let new = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &resized)
        .expect("readback after resize");
    let mut cropped = Vec::with_capacity(old.len());
    for row in 0..200 {
        let start = (row * 640) * 4;
        cropped.extend_from_slice(&new[start..start + 320 * 4]);
    }
    let diff = compare_rgba(320, 200, &old, &cropped).expect("resize crop compare");
    assert_eq!(
        diff.max_abs, 0,
        "top-left content changed after target recreation"
    );

    let zero = TextTarget::new(
        &fixture.gpu.device,
        PhysicalSize {
            width: 0,
            height: 10,
        },
    );
    assert!(zero.is_err(), "zero-width target must be rejected");
    let too_wide = fixture.gpu.device.limits().max_texture_dimension_2d + 1;
    let oversized = TextTarget::new(
        &fixture.gpu.device,
        PhysicalSize {
            width: too_wide,
            height: 10,
        },
    );
    assert!(oversized.is_err(), "oversized target must be rejected");
}

#[test]
fn atlas_overflow_is_reported_and_leaves_target_untouched() {
    let mut fixture = fixture_with_atlas(200, 120, AtlasConfig::new(64, 64));
    // Establish a known target state.
    fixture
        .renderer
        .render(&fixture.target, [1, 2, 3, 255], &[])
        .expect("clear baseline");
    let before = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("read baseline");

    let mut batch = TextBatch::new(9);
    push_line(
        &mut batch,
        0,
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        0x00ff_ffff,
        0x0000_0000,
        TextAttributes::default(),
    );
    let text_layout = layout(&fixture.renderer, 32, 4.0, 4.0);
    let error = fixture
        .renderer
        .render(
            &fixture.target,
            [200, 0, 0, 255],
            &[TextSection::new(&batch, text_layout)],
        )
        .expect_err("64x64 atlas must overflow at 32px");
    match &error {
        TextRenderError::Atlas(AtlasError::Full {
            atlas_width,
            atlas_height,
            ..
        }) => {
            assert_eq!((*atlas_width, *atlas_height), (64, 64));
        }
        other => panic!("expected AtlasError::Full, got {other}"),
    }
    eprintln!("atlas overflow error: {error}");
    let after = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("read after overflow");
    let diff = compare_rgba(200, 120, &before, &after).expect("overflow target compare");
    assert_eq!(
        diff.max_abs, 0,
        "failed frame must not partially modify the target"
    );
    assert!(fixture.renderer.atlas_stats().full_errors >= 1);

    // A single glyph larger than the atlas is a distinct error.
    let mut tiny = fixture_with_atlas(80, 80, AtlasConfig::new(16, 16));
    let mut huge = TextBatch::new(10);
    push_line(
        &mut huge,
        0,
        "M",
        0x00ff_ffff,
        0x0000_0000,
        TextAttributes::default(),
    );
    let huge_layout = layout(&tiny.renderer, 64, 0.0, 0.0);
    let error = tiny
        .renderer
        .render(&tiny.target, CLEAR, &[TextSection::new(&huge, huge_layout)])
        .expect_err("64px glyph must not fit a 16x16 atlas");
    assert!(
        matches!(
            error,
            TextRenderError::Atlas(AtlasError::GlyphTooLarge { .. })
        ),
        "expected GlyphTooLarge, got {error}"
    );

    // Invalid atlas dimensions are rejected up front.
    let invalid = GpuTextRenderer::with_font_path(
        tiny.gpu.device.clone(),
        tiny.gpu.queue.clone(),
        discover_font_path().expect("font"),
        AtlasConfig::new(0, 64),
    );
    assert!(matches!(
        invalid,
        Err(AtlasError::InvalidConfig { width: 0, .. })
    ));
    let bad_padding = GpuTextRenderer::with_font_path(
        tiny.gpu.device.clone(),
        tiny.gpu.queue.clone(),
        discover_font_path().expect("font"),
        AtlasConfig {
            width: 64,
            height: 64,
            padding: 64,
        },
    );
    assert!(matches!(bad_padding, Err(AtlasError::InvalidConfig { .. })));

    // A failed frame must not poison the renderer: after the overflow, a small
    // batch still renders and replaces the target contents.
    let mut small = TextBatch::new(19);
    push_line(
        &mut small,
        0,
        "ok",
        0x00ff_ffff,
        0x0000_0000,
        TextAttributes::default(),
    );
    let small_layout = layout(&fixture.renderer, 16, 0.0, 0.0);
    let stats = fixture
        .renderer
        .render(
            &fixture.target,
            [0, 50, 0, 255],
            &[TextSection::new(&small, small_layout)],
        )
        .expect("renderer recovers after atlas overflow");
    assert!(stats.glyph_quads > 0);
}

#[test]
fn unknown_glyphs_fall_back_to_question_mark_and_are_counted() {
    // No fallback chain: U+10FFFF can never be covered, so the substituted `?`
    // is observable independent of installed fonts.
    let mut fixture = fixture_without_fallbacks(240, 80);
    let mut unknown = TextBatch::new(11);
    unknown.push(cell(
        0,
        0,
        "\u{10FFFF}",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes::default(),
    ));
    let text_layout = layout(&fixture.renderer, 20, 4.0, 4.0);
    let (stats, geometry) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&unknown, text_layout)],
        )
        .expect("render unknown glyph");
    assert_eq!(stats.unknown_glyphs, 1);
    assert_eq!(stats.missing_glyphs, 1);
    assert_eq!(stats.placeholder_quads, 0);
    assert_eq!(geometry.glyphs.len(), 1);
    let unknown_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
            .expect("unknown readback");

    // The same cell containing ASCII '?' must render identically: the fallback
    // is deterministic, not a dropped or random glyph.
    let mut replacement = TextBatch::new(12);
    replacement.push(cell(
        0,
        0,
        "?",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes::default(),
    ));
    let (stats, geometry) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&replacement, text_layout)],
        )
        .expect("render question mark");
    assert_eq!(stats.unknown_glyphs, 0);
    assert_eq!(geometry.glyphs.len(), 1);
    let replacement_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
            .expect("replacement readback");
    assert_eq!(
        unknown_pixels, replacement_pixels,
        "a missing glyph must render exactly like its deterministic '?' marker"
    );

    // Controls are skipped, whitespace draws background but no glyph.
    let mut controls = TextBatch::new(13);
    controls.push(cell(
        0,
        0,
        "a\tb",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes::default(),
    ));
    controls.push(cell(
        1,
        0,
        " ",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes::default(),
    ));
    let stats = fixture
        .renderer
        .render(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&controls, text_layout)],
        )
        .expect("render controls");
    assert_eq!(stats.skipped_control, 1);
    assert_eq!(stats.missing_outlines, 0);
    assert_eq!(stats.glyph_quads, 2);
}

#[test]
fn cjk_fallback_is_explicit_and_wide_cells_span_two_columns() {
    let mut fixture = fixture(320, 120);
    let text_layout = layout(&fixture.renderer, 22, 0.0, 0.0);

    // Wide cell paints red across both columns; the spacer would paint blue if
    // it were (incorrectly) drawn as an independent cell.
    let mut wide = TextBatch::new(20);
    wide.push(cell(
        0,
        0,
        "你",
        0x00ff_ffff,
        0x00ff_0000,
        TextAttributes {
            wide: true,
            ..TextAttributes::default()
        },
    ));
    wide.push(cell(
        0,
        1,
        " ",
        0x00ff_ffff,
        0x0000_00ff,
        TextAttributes {
            wide_spacer: true,
            ..TextAttributes::default()
        },
    ));
    let (stats, geometry) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&wide, text_layout)],
        )
        .expect("render wide cell");
    assert_eq!(stats.cells, 2);
    assert_eq!(stats.wide_cells, 1);
    assert_eq!(stats.wide_spacer_cells, 1);
    // One span-wide background quad for the wide cell; spacer contributes none.
    assert_eq!(stats.solid_quads, 1);
    assert!(
        stats.glyph_quads >= 1,
        "wide glyph or explicit '?' marker missing"
    );
    // CJK coverage is host-dependent, but either a fallback glyph or the
    // counted '?' marker is acceptable; silently drawing nothing is not.
    assert!(stats.fallback_glyphs >= 1 || stats.unknown_glyphs == 1);
    assert_eq!(stats.missing_glyphs, stats.unknown_glyphs);
    eprintln!(
        "cjk fallback: glyphs={} fallback={} missing={} font={}",
        stats.glyph_quads,
        stats.fallback_glyphs,
        stats.missing_glyphs,
        fixture.renderer.font_path().display()
    );

    let pixels = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("wide readback");
    let cw = text_layout.cell_width as u32;
    let ch = text_layout.cell_height as u32;
    let pixel = |x: u32, y: u32| -> [u8; 4] {
        let i = ((y * 320 + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    };
    // Both columns are the wide cell's red background, not the spacer blue.
    // Sample the bottom padding so the glyph ink itself is not in the way.
    for x in [1, cw + 1] {
        assert_eq!(
            pixel(x, ch - 2),
            [255, 0, 0, 255],
            "wide background did not span/protect column x={x}"
        );
    }
    assert_eq!(geometry.solids.len(), 1);
}

#[test]
fn hidden_dim_underline_and_selection_semantics_are_applied() {
    let mut fixture = fixture(360, 160);
    let text_layout = layout(&fixture.renderer, 22, 0.0, 0.0);

    // Hidden: background painted, no glyph, no underline.
    let mut hidden = TextBatch::new(21);
    hidden.push(cell(
        0,
        0,
        "H",
        0x00ff_ffff,
        0x0012_3456,
        TextAttributes {
            hidden: true,
            underline: true,
            ..TextAttributes::default()
        },
    ));
    let (hidden_stats, hidden_geometry) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&hidden, text_layout)],
        )
        .expect("hidden");
    assert_eq!(hidden_stats.hidden_cells, 1);
    assert_eq!(hidden_stats.glyph_quads, 0);
    assert_eq!(
        hidden_stats.solid_quads, 1,
        "hidden must paint only its background"
    );
    assert_eq!(hidden_geometry.solids.len(), 1);

    // Dim resolves to the bold face, exactly like the CPU renderer.
    let mut dim = TextBatch::new(22);
    dim.push(cell(
        0,
        0,
        "A",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes {
            dim: true,
            ..TextAttributes::default()
        },
    ));
    let mut bold = TextBatch::new(23);
    bold.push(cell(
        0,
        0,
        "A",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes {
            bold: true,
            ..TextAttributes::default()
        },
    ));
    let (dim_stats, _) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&dim, text_layout)],
        )
        .expect("dim");
    let dim_pixels = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("dim readback");
    let (bold_stats, _) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&bold, text_layout)],
        )
        .expect("bold");
    let bold_pixels = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("bold readback");
    assert_eq!(dim_stats.dim_cells, 1);
    assert_eq!(dim_stats.style_fallbacks, bold_stats.style_fallbacks);
    assert_eq!(
        dim_pixels, bold_pixels,
        "dim must map to the same face as bold"
    );

    // Underline + double underline + strikethrough are cell geometry solids.
    let mut decorated = TextBatch::new(24);
    decorated.push(cell(
        0,
        0,
        "U",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes {
            underline: true,
            double_underline: true,
            strikethrough: true,
            ..TextAttributes::default()
        },
    ));
    let (decorated_stats, _) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&decorated, text_layout)],
        )
        .expect("decorated");
    // background + underline + double underline + strikeout
    assert_eq!(decorated_stats.solid_quads, 4);

    // Selection/search colors are already resolved by the Scene builder; the
    // renderer must count them and paint the resolved background.
    let mut highlighted = TextBatch::new(25);
    highlighted.push(cell(
        0,
        0,
        "S",
        0x0011_111b,
        0x00f9_e2af,
        TextAttributes {
            selected: true,
            searched: true,
            ..TextAttributes::default()
        },
    ));
    let (highlighted_stats, _) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&highlighted, text_layout)],
        )
        .expect("highlighted");
    assert_eq!(highlighted_stats.selected_cells, 1);
    assert_eq!(highlighted_stats.searched_cells, 1);
    let pixels = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("highlight readback");
    let corner = 3usize * 4;
    assert_eq!(&pixels[corner..corner + 3], &[0xf9, 0xe2, 0xaf]);
}

#[test]
fn render_scene_uses_background_layer_and_reports_skipped_overlays() {
    let mut fixture = fixture(200, 80);
    let text_layout = layout(&fixture.renderer, 20, 0.0, 0.0);

    let mut batch = TextBatch::new(31);
    batch.push(cell(
        0,
        0,
        "S",
        0x00ff_ffff,
        0x0011_2233,
        TextAttributes::default(),
    ));
    let scene = Scene {
        viewport: Viewport {
            cell_w: text_layout.cell_width,
            cell_h: text_layout.cell_height,
            cols: 10,
            rows: 4,
        },
        revision: 77,
        damage: Default::default(),
        selection: None,
        search: None,
        cursor: Some(CursorState {
            row: 0,
            col: 0,
            shape: Default::default(),
            visible: true,
        }),
        layers: vec![
            SceneLayer {
                z_index: BACKGROUND_LAYER_Z,
                items: vec![DrawItem::Background(BackgroundInstance {
                    color: 0x0011_2233,
                })],
            },
            SceneLayer {
                z_index: TEXT_LAYER_Z,
                items: vec![DrawItem::Text(batch.clone())],
            },
        ],
    };
    let mut images = GpuImageRenderer::new(fixture.gpu.device.clone(), fixture.gpu.queue.clone());
    let section = TextSection::new(&batch, text_layout);
    let stats = images
        .render_scene(
            &mut fixture.renderer,
            &fixture.target,
            [0x11, 0x22, 0x33, 255],
            &[section],
            &scene,
        )
        .expect("render scene");
    let text = stats.text.as_ref().expect("text stage stats");
    assert_eq!(text.cells, 1);
    assert_eq!(stats.below.images + stats.above.images, 0);
    let pixels = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("scene readback");
    // A far corner keeps the caller's Scene background clear color.
    let offset = (79 * 200 + 199) * 4;
    assert_eq!(&pixels[offset..offset + 3], &[0x11, 0x22, 0x33]);
}

#[test]
fn bold_italic_faces_and_underline_are_observable() {
    let mut fixture = fixture(320, 120);
    let text_layout = layout(&fixture.renderer, 22, 6.0, 6.0);

    let mut regular = TextBatch::new(14);
    regular.push(cell(
        0,
        0,
        "A",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes::default(),
    ));
    let mut bold = TextBatch::new(15);
    bold.push(cell(
        0,
        0,
        "A",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes {
            bold: true,
            ..TextAttributes::default()
        },
    ));
    let mut italic = TextBatch::new(16);
    italic.push(cell(
        0,
        0,
        "A",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes {
            italic: true,
            ..TextAttributes::default()
        },
    ));

    let bold_available = fixture.renderer.atlas().style_available(FontStyle::Bold);
    let italic_available = fixture.renderer.atlas().style_available(FontStyle::Italic);
    eprintln!("bold face available={bold_available} italic face available={italic_available}");

    let (regular_stats, _) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&regular, text_layout)],
        )
        .expect("regular");
    let regular_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
            .expect("regular readback");
    let (bold_stats, _) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&bold, text_layout)],
        )
        .expect("bold");
    let bold_pixels = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("bold readback");
    let (italic_stats, _) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&italic, text_layout)],
        )
        .expect("italic");
    let italic_pixels = read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
        .expect("italic readback");

    if bold_available {
        assert_ne!(
            regular_pixels, bold_pixels,
            "bold face did not change pixels"
        );
        assert_eq!(bold_stats.style_fallbacks, 0);
    } else {
        assert_eq!(bold_pixels, regular_pixels);
        assert_eq!(bold_stats.style_fallbacks, 1);
    }
    if italic_available {
        assert_ne!(
            regular_pixels, italic_pixels,
            "italic face did not change pixels"
        );
        assert_eq!(italic_stats.style_fallbacks, 0);
    } else {
        assert_eq!(italic_pixels, regular_pixels);
        assert_eq!(italic_stats.style_fallbacks, 1);
    }

    let mut underlined = TextBatch::new(17);
    underlined.push(cell(
        0,
        0,
        "A",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes {
            underline: true,
            ..TextAttributes::default()
        },
    ));
    let (underline_stats, underline_geometry) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&underlined, text_layout)],
        )
        .expect("underline");
    assert_eq!(underline_stats.solid_quads, 2);
    assert_eq!(underline_geometry.solids.len(), 2);
    let underline_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
            .expect("underline readback");
    assert_ne!(regular_pixels, underline_pixels);
    assert_eq!(regular_stats.glyph_quads, 1);
}

#[test]
fn invalid_layout_is_rejected_before_rendering() {
    let mut fixture = fixture(120, 60);
    let mut batch = TextBatch::new(18);
    batch.push(cell(
        0,
        0,
        "A",
        0x00ff_ffff,
        CLEAR_RGB,
        TextAttributes::default(),
    ));
    let mut bad = layout(&fixture.renderer, 16, 0.0, 0.0);
    bad.cell_width = 0.0;
    let error = fixture
        .renderer
        .render(&fixture.target, CLEAR, &[TextSection::new(&batch, bad)])
        .expect_err("zero cell width must be rejected");
    assert!(
        matches!(
            error,
            TextRenderError::InvalidLayout {
                section: 0,
                reason: "cell_width must be positive"
            }
        ),
        "unexpected error: {error}"
    );
    let mut bad_font = layout(&fixture.renderer, 16, 0.0, 0.0);
    bad_font.font_size_px = 0;
    let error = fixture
        .renderer
        .render(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&batch, bad_font)],
        )
        .expect_err("zero font size must be rejected");
    assert!(matches!(error, TextRenderError::InvalidLayout { .. }));
}

#[test]
fn damage_scissor_matches_full_redraw_and_preserves_untouched_rows() {
    use xtervo_scene::{DamageRect, DamageRegion};
    let mut fixture = fixture(240, 80);
    let text_layout = layout(&fixture.renderer, 20, 0.0, 0.0);
    let cell_h = text_layout.cell_height;
    let cell_w = text_layout.cell_width;
    let viewport = Viewport {
        cell_w,
        cell_h,
        cols: 12,
        rows: 4,
    };

    let make_scene = |row0: &str, damage: DamageRegion| {
        let mut batch = TextBatch::new(1);
        push_line(
            &mut batch,
            0,
            row0,
            0x00ff_ffff,
            0x0011_2233,
            TextAttributes::default(),
        );
        push_line(
            &mut batch,
            1,
            "BBBB",
            0x00ff_ffff,
            0x0011_2233,
            TextAttributes::default(),
        );
        let section_batch = batch.clone();
        let scene = Scene {
            viewport,
            revision: 2,
            damage,
            selection: None,
            search: None,
            cursor: None,
            layers: vec![SceneLayer {
                z_index: TEXT_LAYER_Z,
                items: vec![DrawItem::Text(batch)],
            }],
        };
        (scene, section_batch)
    };

    let mut images = GpuImageRenderer::new(fixture.gpu.device.clone(), fixture.gpu.queue.clone());
    let (initial, initial_batch) = make_scene("AAAA", DamageRegion::default());
    let initial_section = TextSection::new(&initial_batch, text_layout);
    images
        .render_scene(
            &mut fixture.renderer,
            &fixture.target,
            CLEAR,
            &[initial_section],
            &initial,
        )
        .expect("initial full render");

    let (partial, partial_batch) = make_scene(
        "CCCC",
        DamageRegion {
            rects: vec![DamageRect::new(
                0,
                0,
                (cell_w * 4.0).ceil() as u32,
                cell_h.ceil() as u32,
            )],
            full: false,
        },
    );
    let partial_section = TextSection::new(&partial_batch, text_layout);
    images
        .render_scene(
            &mut fixture.renderer,
            &fixture.target,
            CLEAR,
            &[partial_section],
            &partial,
        )
        .expect("partial render");

    let fresh = TextTarget::new(
        &fixture.gpu.device,
        PhysicalSize {
            width: 240,
            height: 80,
        },
    )
    .expect("fresh target");
    let (full, full_batch) = make_scene("CCCC", DamageRegion::full());
    let full_section = TextSection::new(&full_batch, text_layout);
    images
        .render_scene(&mut fixture.renderer, &fresh, CLEAR, &[full_section], &full)
        .expect("full final render");

    let partial_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
            .expect("partial readback");
    let full_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fresh).expect("full readback");
    assert_eq!(
        partial_pixels, full_pixels,
        "partial damage must match full redraw"
    );
}

/// R4 helper: a glyph bitmap may overhang its cell (italic faces, fallback
/// advances that differ from the grid, combining marks). The damage path only
/// clears/scissors the damaged cell rect, so an unclipped quad leaves ink in
/// the neighbour cell and that residue survives a later partial repaint.
///
/// The base-glyph case overrides the cell width to 6px while rasterizing at
/// 32px: the bitmap is wider than 6px by construction, so the quad overhangs
/// unless `push_glyph_quad` clips it. The combining-mark case keeps the normal
/// 32px grid, where the centered mark bitmap still crosses the cell edge. The
/// partial frame then replaces that cell with a blank one and reports damage
/// for that single cell only, exactly what a terminal line damage for one
/// changed column produces. The result must be byte-identical to a full redraw
/// of the same frame.
fn assert_overhanging_glyph_damage_matches_full(
    text: &str,
    cell_width_override: Option<f32>,
    min_glyphs: usize,
    label: &str,
) {
    use xtervo_scene::{DamageRect, DamageRegion};

    let mut fixture = fixture(160, 120);
    let mut text_layout = layout(&fixture.renderer, 32, 0.0, 0.0);
    if let Some(cell_width) = cell_width_override {
        text_layout.cell_width = cell_width;
    }
    let cell_w = text_layout.cell_width;
    let cell_h = text_layout.cell_height;
    let viewport = Viewport {
        cell_w,
        cell_h,
        cols: 16,
        rows: 3,
    };

    let make_scene = |text: &str, damage: DamageRegion| {
        let mut batch = TextBatch::new(1);
        batch.push(cell(
            0,
            0,
            text,
            0x00ff_ffff,
            CLEAR_RGB,
            TextAttributes {
                italic: true,
                ..TextAttributes::default()
            },
        ));
        let section_batch = batch.clone();
        let scene = Scene {
            viewport,
            revision: 2,
            damage,
            selection: None,
            search: None,
            cursor: None,
            layers: vec![SceneLayer {
                z_index: TEXT_LAYER_Z,
                items: vec![DrawItem::Text(batch)],
            }],
        };
        (scene, section_batch)
    };

    let mut images = GpuImageRenderer::new(fixture.gpu.device.clone(), fixture.gpu.queue.clone());

    // Full frame with the overhanging glyph.
    let (_initial, initial_batch) = make_scene(text, DamageRegion::full());
    let (_, initial_geometry) = fixture
        .renderer
        .render_with_geometry(
            &fixture.target,
            CLEAR,
            &[TextSection::new(&initial_batch, text_layout)],
        )
        .expect("initial overhanging glyph render");
    assert!(
        initial_geometry.glyphs.len() >= min_glyphs,
        "{label}: expected at least {min_glyphs} glyph quads, got {}; \
         test glyph rasterized to nothing, cannot exercise overhang",
        initial_geometry.glyphs.len()
    );

    // Partial frame: same cell becomes blank, damage is that cell only.
    let damage = DamageRegion {
        rects: vec![DamageRect::new(
            0,
            0,
            cell_w.ceil() as u32,
            cell_h.ceil() as u32,
        )],
        full: false,
    };
    let (partial, partial_batch) = make_scene(" ", damage);
    images
        .render_scene(
            &mut fixture.renderer,
            &fixture.target,
            CLEAR,
            &[TextSection::new(&partial_batch, text_layout)],
            &partial,
        )
        .expect("partial blank-cell render");

    // Reference: full redraw of the same blank frame on a fresh target.
    let fresh = TextTarget::new(
        &fixture.gpu.device,
        PhysicalSize {
            width: 160,
            height: 120,
        },
    )
    .expect("fresh target");
    let (full, full_batch) = make_scene(" ", DamageRegion::full());
    images
        .render_scene(
            &mut fixture.renderer,
            &fresh,
            CLEAR,
            &[TextSection::new(&full_batch, text_layout)],
            &full,
        )
        .expect("full blank-frame render");

    let partial_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fixture.target)
            .expect("partial readback");
    let full_pixels =
        read_target_rgba8(&fixture.gpu.device, &fixture.gpu.queue, &fresh).expect("full readback");
    if let Some(index) = partial_pixels
        .iter()
        .zip(&full_pixels)
        .position(|(a, b)| a != b)
    {
        let pixel = index / 4;
        let width = 160usize;
        panic!(
            "{label}: cell damage left glyph overhang outside the damaged cell: \
             first diff at x={} y={} partial={:?} full={:?} (cell {}x{})",
            pixel % width,
            pixel / width,
            &partial_pixels[index - index % 4..][..4],
            &full_pixels[index - index % 4..][..4],
            cell_w,
            cell_h,
        );
    }

    // Direct geometry contract: every quad must stay inside its own cell, like
    // the CPU renderer's blit clip. This catches an overhang even when the
    // overhang happens to land on identical background pixels.
    for glyph in &initial_geometry.glyphs {
        let [x, y, w, h] = glyph.rect;
        assert!(
            x >= 0.0 && y >= 0.0 && x + w <= cell_w + 0.01 && y + h <= cell_h + 0.01,
            "{label}: glyph quad {:?} must be clipped to its {}x{} cell",
            glyph.rect,
            cell_w,
            cell_h
        );
    }
}

#[test]
fn overhanging_glyph_does_not_survive_cell_damage() {
    assert_overhanging_glyph_damage_matches_full("M", Some(6.0), 1, "overhanging base glyph");
}

#[test]
fn overhanging_combining_mark_is_clipped_to_base_cell_damage() {
    // The mark is zero-width and centered on the base glyph's ink, so its
    // bitmap is the one most likely to cross the cell edge in normal layouts.
    // Normal 32px grid: the base `M` fits its cell, but the centered mark's
    // bitmap extends past the cell's right edge, which is exactly the residue
    // this regression must keep from surviving a single-cell damage pass.
    assert_overhanging_glyph_damage_matches_full("M\u{0301}", None, 2, "combining mark");
}
