# GPU Damage Scissor Plan (0.4.0 blocker)

Status: implementation pending (2026-11-12). Current RC6 only reuses the
previous target for frames with no terminal damage. A damaged frame still
clears and redraws the whole Scene/target.

## Contract

```text
Scene.damage.full == true        -> full clear + full text/image passes
Scene.damage.rects.is_empty()    -> reuse previous GPU target
otherwise                         -> scissor each damage rect
```

For each damage rect:

```text
render pass LoadOp::Load
set_scissor_rect(rect)
clear just that rect with the scene background color
draw text backgrounds/glyphs clipped by the scissor
draw only image layers/instances intersecting the rect
```

Fallbacks (safe, explicit):

```text
images cross scissor boundary  -> full image pass for that frame
target resized / first frame   -> full frame
atlas/resource failure         -> CPU renderer, no half frame
more than N damage rects       -> coalesce or full frame
```

## API sketch

```rust
GpuTextRenderer::draw_backgrounds_damaged(target, clear, prepared, &[DamageRect])
GpuTextRenderer::draw_glyphs_damaged(target, prepared, &[DamageRect])
GpuImageRenderer::render_scene_damaged(target, scene, text_passes)
```

`encode_pass` gains optional scissor; clear is done by a small solid instance
covering the rect (WGPU clear ignores scissor).

## Required tests

```text
1. no damage -> target reused, no draw calls
2. full -> clear + full passes
3. one damaged row -> untouched rows byte-identical to previous frame
4. two disjoint damage rows -> both updated, middle preserved
5. damage row with wide/CJK/selection/search -> same pixels as full render
6. image intersecting damage -> image pixels same as full render
7. image outside damage -> not redrawn / not clipped away
8. resize/first frame -> full path
9. partial vs full pixel diff == 0 for text and image fixtures
10. HUD counts partial/full/damage rects
```

## HUD

Add counters:

```text
full_frames
partial_frames
reused_frames
damage_rects
```

## Gates

```text
cargo test -p tervo-render-wgpu
cargo test -p tervo-app --bin tervo
GPU native e2e
image e2e
15m GPU soak during development
30m GPU soak on exact release candidate
NVIDIA RTX 4060 real-device run
```

Do not claim "no full redraw" until damaged frames are scissored and the
partial-vs-full diff tests pass.
