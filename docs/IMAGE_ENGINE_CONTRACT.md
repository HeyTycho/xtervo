# Image Engine Contract (v1)

This file is the frozen interface for the image-engine integration. Implementations may not
change these signatures without integrator approval.

## Identity

```rust
pub struct ImageId(pub u32);
pub struct SourceId(pub u64);
pub struct PlacementId(pub u64);
pub struct RowId(pub u64);
```

- `SourceId` is immutable pixel data identity. It never gets reused.
- `PlacementId` identifies one visible placement. Multiple placements may share a source.
- `RowId` is the stable alacritty_terminal row identity provided by the vendored patch.
- Numeric protocol ids are converted to these types at the protocol boundary.

## Graphics model

```rust
pub struct ImageResource {
    pub id: ImageId,
    pub source_id: SourceId,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    pub generation: u64,
}

pub struct ImagePlacement {
    pub id: PlacementId,
    pub image_id: ImageId,
    pub source_id: SourceId,
    pub row_id: RowId,
    pub col: u16,
    pub source_rect: Option<Rect>,
    pub cells_w: u16,
    pub cells_h: u16,
    pub pixel_offset: (i32, i32),
    pub z_index: i32,
    pub protocol: ImageProtocol,
    pub policy: CompositePolicy,
}

pub enum ImageProtocol { Kitty, Sixel, Iterm2 }
pub enum CompositePolicy { BelowText, AboveText, CellOwned }
```

The screen model owns `RowId -> placement` ownership. `RowId` not found means the placement
must be dropped, never guessed. `retain_reachable`/`erase_overlapping_damage` are removed.

## Runtime commands

```rust
pub enum GraphicsCommand {
    Query { id: u32, payload: Vec<u8> },
    Transmit { image: ImageResource },
    Place(ImagePlacement),
    DeletePlacement(PlacementId),
    DeleteImage(ImageId),
    DeleteAll,
    SetCellSize { width: u16, height: u16 },
    Scroll,
    Resize,
    SwitchScreen,
    Reset,
}
```

Parser code emits commands. It must not create textures or touch the renderer.

## Scene

```rust
pub struct Scene {
    pub viewport: Viewport,
    pub revision: u64,
    pub damage: DamageRegion,
    pub layers: Vec<SceneLayer>,
}

pub struct SceneLayer { pub z_index: i32, pub items: Vec<DrawItem> }

pub enum DrawItem {
    Background(BackgroundInstance),
    Text(TextBatch),
    Image(ImageInstance),
    Cursor(CursorInstance),
}

pub struct ImageInstance {
    pub placement_id: PlacementId,
    pub source_id: SourceId,
    pub source_rect: Rect,
    pub destination: Rect,
    pub clip: Rect,
    pub z_index: i32,
    pub policy: CompositePolicy,
}
```

Selection is not a top overlay. It is emitted as cell background/foreground attributes in the
TextBatch.

## Renderer boundary

```rust
pub trait Renderer {
    type Error;
    type Frame;
    fn resize(&mut self, size: PhysicalSize) -> Result<(), Self::Error>;
    fn render(&mut self, scene: &Scene, damage: &DamageRegion) -> Result<Self::Frame, Self::Error>;
    fn invalidate(&mut self, damage: &DamageRegion);
    fn device_lost(&mut self);
}
```

No wgpu/Skia type may appear in `tervo-core` or the logical Scene.

## Slint presenter

The WGPU backend produces a texture and a `slint::Image::try_from(texture)` wrapper. The presenter
holds the texture alive, updates the UI image, and calls `request_redraw`. Tests must use
`slint::Window::take_snapshot()`, not `xwd`, for GPU windows.

## Validation invariants

```text
incremental render == full reference render
text scroll does not move a fixed image
image scroll moves placement with its row
ECH/EL/ED clears the owned placement only
alt-screen does not leak placements
resize/reflow either remaps RowId or drops the placement
no image pixels are ever written into the text framebuffer
```

## Ownership

- A: core RowId/ownership; `crates/core/**`
- B: Kitty parser; `crates/core/src/kitty*`, tests
- C: graphics manager; `crates/graphics/**`
- D: scene/renderers; `crates/scene/**`, `crates/render*/**`, `crates/app/src/render/**`
- E: e2e; `scripts/**`, `.logs/**`
- F: Cargo.toml, Cargo.lock, merges, contract
