// Textured-quad composition for terminal images (Sixel / IITM / Kitty).
//
// One instance per image placement. The instance carries the unclipped
// destination rectangle and the normalized source crop (`source_rect /
// source_size`); the renderer clips with a per-image scissor, so the shader
// never needs to know destination/clip intersection geometry.
//
// group 0: frame size uniform (shared by every image of a frame)
// group 1: one source texture + sampler (one bind group per source_id)

struct Screen {
    size: vec2<f32>,
    _padding: vec2<f32>,
};

@group(0) @binding(0) var<uniform> screen: Screen;
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

const CORNERS = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(1.0, 1.0),
    vec2<f32>(0.0, 1.0),
);

fn to_clip(pixel: vec2<f32>) -> vec4<f32> {
    let ndc = vec2<f32>(
        pixel.x / screen.size.x * 2.0 - 1.0,
        1.0 - pixel.y / screen.size.y * 2.0,
    );
    return vec4<f32>(ndc, 0.0, 1.0);
}

struct ImageVertex {
    // [x, y, width, height] in physical pixels, before viewport clipping.
    @location(0) rect: vec4<f32>,
    // [u0, v0, u1, v1] normalized source crop.
    @location(1) uv: vec4<f32>,
};

struct ImageOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_image(
    input: ImageVertex,
    @builtin(vertex_index) vertex_index: u32,
) -> ImageOut {
    var out: ImageOut;
    let corner = CORNERS[vertex_index];
    let pixel = input.rect.xy + input.rect.zw * corner;
    out.position = to_clip(pixel);
    out.uv = mix(input.uv.xy, input.uv.zw, corner);
    return out;
}

@fragment
fn fs_image(input: ImageOut) -> @location(0) vec4<f32> {
    // Source textures are straight-alpha RGBA8. The pipeline uses
    // `BlendState::ALPHA_BLENDING`, so the fragment output stays straight.
    return textureSample(source_texture, source_sampler, input.uv);
}
