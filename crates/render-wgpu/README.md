# tervo-render-wgpu

Standalone WGPU backend and Slint texture-presenter primitive for Tervo's planned
image-engine renderer. This crate deliberately does **not** depend on `tervo-core`
or `tervo-app`; it only consumes `tervo-scene` and implements `tervo-render`'s
`Renderer` trait.

Status: primitive/spike. The backend currently owns an offscreen frame texture and
runs only a full-frame clear pass. Glyph, image and cursor drawing are not part of
this crate yet.

## Features

| feature | effect |
| --- | --- |
| _(default)_ | Backend only. Depends on `wgpu` (pinned to `=30.0.1`) and the backend-independent `tervo-*` crates. |
| `slint-presenter` | Adds `slint = "=1.18.1"` with `std`, `backend-winit`, `compat-1-18`, `unstable-wgpu-30`, `renderer-femtovg-wgpu` and `renderer-femtovg`, plus `SlintTexturePresenter`. |

`renderer-femtovg` (OpenGL) is enabled only because Slint 1.18.1's winit backend
otherwise fails to compile when `tervo-app`'s `renderer-skia-opengl` is unified in
the same workspace build (`cargo check --workspace --all-features`). Runtime
renderer selection is unchanged: `require_wgpu_30` selects the WGPU renderer.

The `two_colors` example requires `slint-presenter`:

```text
cargo run -p tervo-render-wgpu --features slint-presenter --example two_colors
```

## API

```rust
use tervo_scene::PhysicalSize;
use tervo_render_wgpu::{SlintTexturePresenter, WgpuRenderer};

// device/queue must be the exact ones Slint renders with (RenderingSetup).
let mut renderer = WgpuRenderer::new(device, queue, PhysicalSize { width: 320, height: 200 })?;

// Import once; the presenter keeps the wgpu::Texture alive.
let presenter = SlintTexturePresenter::new(renderer.texture().clone(), move |image| {
    ui.set_frame(image); // component property, e.g. `in-out property <image> frame`
})?;

// Per frame: backend write first, then present. No CPU pixel copy.
renderer.set_clear_color([255, 0, 0, 255]);
renderer.render(&scene, &damage)?;
presenter.present(ui.window()); // re-sets the image property + request_redraw()
```

`WgpuRenderer::render()` implements `tervo_render::Renderer` but currently ignores
`Scene`/`DamageRegion` and just clears to `set_clear_color()`. `clear()` is exposed
for tests/examples that only need a solid frame.

`SlintTexturePresenter::replace_texture()` exists for a backend resize/recreation:
call it with the new `renderer.texture().clone()`, then `present()` so the UI
property observes the new image. The current example uses a fixed-size texture.

## Slint texture-import constraints (1.18.1)

Verified against the `slint 1.18.1` crate source:

- `slint::Image::try_from(wgpu::Texture)` requires `TextureFormat::Rgba8Unorm` or
  `Rgba8UnormSrgb`; everything else returns `TextureImportError::InvalidFormat`.
- The texture usage must contain **both** `TEXTURE_BINDING` and
  `RENDER_ATTACHMENT`; otherwise `TextureImportError::InvalidUsage`.
- The texture must be created on the **same** `wgpu::Device`/`wgpu::Queue` that
  Slint uses. `wgpu` types from a second device or a second `wgpu` crate version
  are not interchangeable. This crate pins `wgpu = "=30.0.1"` to match Slint
  1.18.1's `unstable-wgpu-30` re-export.
- The texture must stay alive as long as the imported image is sampled. The
  presenter stores its own `wgpu::Texture` clone in addition to the
  `slint::Image` handle.
- Import does not copy pixels. Subsequent `queue.write_texture`/render-pass
  writes to the same texture are visible to Slint on the next render; call
  `Window::request_redraw()` (the presenter does this) after updating it.
- `Window::take_snapshot()` is the supported way to read pixels back from a GPU
  window. It may re-render the scene, is synchronous/slow, and is only available
  with Slint's `std` feature. `xwd` must not be used for GPU windows: the X
  server does not own the composited pixels and returns black/empty data.

## Headless validation

The example writes red, blue, and red again through the backend, imports the
texture once, presents it continuously, and verifies three successive
`take_snapshot()` results. It fails if the image is still the rectangle
background (`#404040`) or an old color.

```text
Xvfb :99 -screen 0 1280x720x24 -nolisten tcp &
DISPLAY=:99 SLINT_WGPU_CPU=1 \
  cargo run -p tervo-render-wgpu --features slint-presenter --example two_colors
```

Expected key output:

```text
two_colors: Slint WGPU30 adapter=... device_type=Cpu
two_colors: imported 320x200 Rgba8Unorm texture on Slint's device
snapshot red-1: 320x200 center=[255, 0, 0, 255] -> ...
snapshot blue: 320x200 center=[0, 0, 255, 255] -> ...
snapshot red-2: 320x200 center=[255, 0, 0, 255] -> ...
PASS two_colors: red -> blue -> red snapshots verified with Window::take_snapshot
```

Environment notes:

- On a CPU-only/headless host, `SLINT_WGPU_CPU=1` is required. Without it the
  Slint backend selector aborts with:
  `WGPU 30.x rendering was required but no GPU-backed WGPU adapter is available
  for the requested backends. Set SLINT_WGPU_CPU=1 to allow CPU adapters.`
- If `SLINT_BACKEND` forces a renderer this build does not contain (for example
  `winit-skia`), Slint warns `unrecognized renderer skia, falling back to
  FemtoVG` and then still uses the WGPU-backed FemtoVG renderer. For deterministic
  logs unset it: `env -u SLINT_BACKEND ...`.
- Snapshots are written as binary PPM to `$TERVO_WGPU_SNAPSHOT_DIR`, default
  `target/render-wgpu-snapshots`. No absolute/user-specific path is baked into the
  example; run from the crate or workspace root so the relative path lands under
  `target/`.
- The validation run in this worktree produced exact `[255, 0, 0, 255]` /
  `[0, 0, 255, 255]` centers at 320x200 with the Vulkan `llvmpipe` CPU adapter
  (`device_type=Cpu`).

## Known limitations

- **Not integrated**: nothing in `tervo-app` or `tervo-core` references this
  crate. It is a standalone primitive.
- **Clear pass only**: `Scene` layers/DrawItems and `DamageRegion` are not drawn or
  tracked. There is no glyph atlas, image cache, cursor pass, or z-order
  compositing.
- **No incremental rendering**: every render clears the full frame; `invalidate`
  is a no-op.
- **No device-loss recovery**: `device_lost()` is an explicit no-op. Recreate the
  device/queue and the renderer/presenter.
- **Resize requires re-import**: `WgpuRenderer::resize()` recreates the texture;
  existing presenters keep the old one until `replace_texture()` is called.
- **One device/queue only**: no cross-adapter sharing or CPU readback.
- **Slint 1.18 unstable API**: `unstable-wgpu-30` / `renderer-femtovg-wgpu` are
  explicitly unstable and may change in future Slint releases. The winit backend
  is needed for `set_rendering_notifier`, and only its `WGPU30` graphics API
  provides a device/queue compatible with the import.
- **MSRV**: the workspace declares Rust 1.85, and the backend-only build needs
  Rust 1.87+ (`wgpu 30`). Enabling `slint-presenter` requires Rust 1.92+ because
  Slint 1.18.1's winit backend and renderer require it. Validation used Rust
  1.99.0.
- **Two GPU stages, not one pass**: the backend renders to its offscreen texture,
  then Slint samples that texture while compositing the window. This is not a
  zero-copy single-render-pass path.
