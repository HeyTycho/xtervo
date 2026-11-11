// Instanced terminal text: one quad per glyph, one coverage byte per texel.
//
// Both pipelines share group 0:
//   binding 0: screen size uniform (vertex stage)
//   binding 1: R8 coverage atlas (glyph fragment stage)
//   binding 2: linear sampler (glyph fragment stage)

struct Screen {
    size: vec2<f32>,
    _padding: vec2<f32>,
};

@group(0) @binding(0) var<uniform> screen: Screen;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

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

struct SolidVertex {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
};

struct SolidOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_solid(
    input: SolidVertex,
    @builtin(vertex_index) vertex_index: u32,
) -> SolidOut {
    var out: SolidOut;
    let pixel = input.rect.xy + input.rect.zw * CORNERS[vertex_index];
    out.position = to_clip(pixel);
    out.color = input.color;
    return out;
}

@fragment
fn fs_solid(input: SolidOut) -> @location(0) vec4<f32> {
    return input.color;
}

struct GlyphVertex {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
};

struct GlyphOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_glyph(
    input: GlyphVertex,
    @builtin(vertex_index) vertex_index: u32,
) -> GlyphOut {
    var out: GlyphOut;
    let corner = CORNERS[vertex_index];
    let pixel = input.rect.xy + input.rect.zw * corner;
    out.position = to_clip(pixel);
    out.uv = mix(input.uv.xy, input.uv.zw, corner);
    out.color = input.color;
    return out;
}

@fragment
fn fs_glyph(input: GlyphOut) -> @location(0) vec4<f32> {
    let coverage = textureSample(atlas_texture, atlas_sampler, input.uv).r;
    return vec4<f32>(input.color.rgb, input.color.a * coverage);
}
