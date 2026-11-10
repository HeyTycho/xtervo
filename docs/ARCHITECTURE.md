# Tervo 架构

状态：目标架构 + 当前实现边界。  
最后更新：2026-11-10（`feat/image-engine`）。

本文是终端核心、图形资源、Scene、Renderer、Slint Presenter 的总设计。协议字段、接口签名和验收约束以
[`IMAGE_ENGINE_CONTRACT.md`](IMAGE_ENGINE_CONTRACT.md) 为准；Alacritty row identity 补丁见
[`ALACRITTY_ROW_ID.md`](ALACRITTY_ROW_ID.md)。

## 1. 目标与非目标

### 目标

```text
原生 Rust + Slint 终端工作台
稳定、可回退
文本/图片/光标/选区统一场景
Kitty Graphics + Sixel 主路径
长期统一 GPU 合成
多种图片协议共享同一资源与所有权模型
```

### 非目标（第一阶段）

```text
自研 VT 状态机
Electron/Tabby overlay 架构
Kitty 动画、共享内存、文件传输全集
iTerm2 全参数
终端内视频/Web 内容
```

## 2. 核心原则

```text
1. Scene 不依赖 wgpu、Skia、Slint。
2. GPU 资源不属于 Scene；Renderer 管理纹理/Atlas/缓存。
3. Slint 只负责工作台 UI 和最终显示，不逐 Cell 绘制终端。
4. Renderer 不读取/修改 VT 状态，只消费不可变 Scene 和 Damage。
5. 图片生命周期绑定 RowId / Placement，不绑定 framebuffer 像素。
6. 图片不写入文本 framebuffer。
7. 协议解析、资源管理、场景构建、GPU 绘制四层解耦。
8. 增量更新优先：Cell damage、Placement damage、Texture upload queue。
```

## 3. 总体数据流

```text
PTY
 │
 ▼
VT Parser / Screen Model
（alacritty_terminal + Tervo 前置扫描器）
 │
 ├─ Cells / Scrollback / Modes / RowId
 ├─ GraphicsCommand
 │       │
 │       ▼
 │  GraphicsManager
 │  ImageResource + ImagePlacement
 │       │
 ▼       ▼
Scene Builder
  TextBatch / ImageInstance / Cursor / Damage
 │
 ▼
Scene Compiler
 │
 ▼
Renderer Trait
 ├─ CPU/Skia Backend
 └─ WGPU Backend
 │
 ▼
RenderedFrame
 ├─ CPU pixels
 └─ wgpu::Texture
 │
 ▼
Slint TerminalTexturePresenter
 │
 ▼
Slint Application Shell
Tabs / Panes / Menus / Settings
```

终端核心只产生场景；渲染后端只消费场景；Slint 只合成应用 UI。

## 4. Crate 边界

### 现有

```text
crates/core    tervo-core
  PTY / actor / VT / Frame / Screen
  Sixel/IIP 前置过滤
  图片 ownership 迁移中

crates/app     tervo-app
  Slint UI
  PaneRuntime
  CPU Renderer
  灯箱/菜单/设置
```

### 新增（目标）

```text
crates/graphics   tervo-graphics
  ImageId / SourceId / PlacementId / RowId
  ImageResource / ImagePlacement
  GraphicsManager
  Rect / CompositePolicy
  不依赖 wgpu / Slint / alacritty

crates/scene      tervo-scene
  Scene / SceneLayer / DrawItem
  TextBatch / ImageInstance / CursorInstance
  DamageRegion
  不依赖 wgpu / Skia / Slint

crates/render     tervo-render
  Renderer trait
  RenderedFrame
  后端中立类型

crates/render-cpu tervo-render-cpu（后续）
  CPU/Skia backend

crates/render-wgpu tervo-render-wgpu（后续）
  wgpu backend + texture import
```

`crates/graphics`、`crates/scene`、`crates/render` 已在 `feat/image-engine` 建立骨架；CPU/WGPU 后端尚未接入主路径。

## 5. 图形资源模型

### Identity

```rust
ImageId(u32)        // 协议层图片资源 id
SourceId(u64)       // 不可变像素数据 id，永不复用
PlacementId(u64)    // 一次放置
RowId(u64)          // 稳定终端行身份
```

### ImageResource

```rust
ImageResource {
    id: ImageId,
    source_id: SourceId,
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
    generation: u64,
}
```

- 只保存图像本身，不保存屏幕位置。
- 同一图片可有多个 Placement。
- CPU 数据和 GPU 纹理分离；GPU 纹理由 Renderer 按需上传/回收。

### ImagePlacement

```rust
ImagePlacement {
    id: PlacementId,
    image_id: ImageId,
    source_id: SourceId,
    row_id: RowId,
    col: u16,
    source_rect: Option<Rect>,
    cells_w: u16,
    cells_h: u16,
    pixel_offset: (i32, i32),
    z_index: i32,
    protocol: ImageProtocol,
    policy: CompositePolicy,
}
```

- 锚点是 `RowId + column`，不是易变的 row index 或 `scroll_total`。
- `RowId` 找不到 = 该行已消失/被 reset/被 reflow 合并，必须显式删除 placement，不能猜。
- `CompositePolicy` 决定图片相对文字的层序。

## 6. RowId / Alacritty 补丁

Tervo 使用 vendored `alacritty_terminal` 0.26.1，补丁位于 `vendor/alacritty_terminal`。

```text
Row 有稳定 u64 id
scroll/swap 时 id 跟着行走
Row::reset -> 新 id（复用存储不等于同一逻辑行）
Grid::row_id(Line)
Grid::find_row_id(RowId)
```

约束：

```text
reflow 拆分：第一段保留原 id，后续段新 id
reflow 合并/历史淘汰：原 id 消失，placement 显式 drop
alt-screen：两份 screen，各自 RowId 空间
```

不允许在 Tervo 侧用 `scroll_total`、文本指纹或空白 damage 重新推断图片行身份。

## 7. 场景模型

### Scene

```rust
Scene {
    viewport: Viewport,
    revision: u64,
    damage: DamageRegion,
    layers: Vec<SceneLayer>,
}

SceneLayer { z_index: i32, items: Vec<DrawItem> }

DrawItem {
    Background(BackgroundInstance),
    Text(TextBatch),
    Image(ImageInstance),
    Cursor(CursorInstance),
}
```

### Selection

Selection **不是顶层 overlay**。它是 cell 背景/前景属性：

```text
selection background -> cell background pass
selection foreground -> glyph pass
```

否则会盖住图片、文字和光标，颜色也不一致。

### Damage

```text
DamageRegion:
  full: bool
  rects: Vec<DamageRect>   // 多区域，不是单一矩形
```

分别跟踪：

```text
Cell damage
Placement damage
Cursor damage
Texture upload damage
```

不要求每帧重建全部 DrawItem。

## 8. 合成顺序

```text
Pass 0  Clear / terminal background
Pass 1  Negative-Z images (protocol policy)
Pass 2  Cell backgrounds (including selection background)
Pass 3  Glyphs (including selection foreground / underline)
Pass 4  Non-negative-Z images (Kitty C=1 / above-text policy)
Pass 5  Cursor
```

注意：

```text
Z-index 不是全局唯一排序键。
Kitty 的 C=1/正 z、Sixel、iTerm2 各有协议语义。
同 Z 需要稳定顺序。
Cell 背景必须先正确合成，负 Z 图片不能被不透明背景无故盖掉。
```

## 9. 协议

### Kitty（主路径，今晚最小集）

dsh-tui 0.14 所需：

```text
a=q 查询/应答
APC 过滤
a=t / a=p / a=d
RGBA f=32 / o=z zlib
4096 分块 + m=0
i/p/c/r/x/y/w/h/z/C
d=i / d=I
q=1/2 应答
错误 ENOENT/EINVAL
```

暂不做：

```text
动画
共享内存/文件传输
Virtual/Relative placement
Unicode placeholder
```

### Sixel（兼容路径）

```text
解码 -> 统一 RGBA ImageResource
Placement 走 RowId ownership
不再扩大 ECH/空白 damage 猜测
```

### iTerm2（延后）

```text
OSC 1337 File inline 最小实现排在 Kitty/Sixel 之后
不提交半成品
```

## 10. Renderer

```rust
pub trait Renderer {
    type Error;
    type Frame;

    fn resize(&mut self, size: PhysicalSize) -> Result<(), Self::Error>;
    fn render(
        &mut self,
        scene: &Scene,
        damage: &DamageRegion,
    ) -> Result<Self::Frame, Self::Error>;
    fn invalidate(&mut self, damage: &DamageRegion);
    fn device_lost(&mut self);
}
```

后端：

```text
CPU/Skia    当前稳定路径；测试/兼容/无 GPU 回退
WGPU        frame_texture + glyph atlas + image textures
```

公共 Scene/Core 不暴露 wgpu/Skia 类型。

### WGPU 资源

```text
frame_texture   终端最终输出，窗口尺寸变化才重建
glyph_atlas     字形缓存
image_cache     SourceId -> GPU texture
pipelines       background/image/text/cursor passes
```

不允许每帧重建 texture。

## 11. Slint + WGPU

Slint 1.18.1 支持：

```text
slint::BackendSelector::require_wgpu_30
slint::GraphicsAPI::WGPU30 { device, queue, .. }
slint::Image::try_from(wgpu::Texture)
```

约束：

```text
同一个 Device/Queue
Rgba8Unorm 或 Rgba8UnormSrgb
TEXTURE_BINDING | RENDER_ATTACHMENT
启用 unstable-wgpu-30 + renderer-femtovg-wgpu
```

### Presenter

```text
TerminalTexturePresenter
  - 持有 texture
  - 持有 slint::Image
  - 每帧更新后 request_redraw
  - 必要时重新创建 Image wrapper
  - 管理旧 texture 生命周期
```

GPU 窗口测试必须使用：

```text
slint::Window::take_snapshot()
```

不要用 `xwd` 判断 GPU framebuffer，Xvfb/winit GPU surface 下会得到全黑。

### 并发/阶段

```text
Terminal Renderer -> offscreen texture
Slint Image -> Slint UI compositor -> window surface
```

这仍是两个 GPU 阶段，不是单个 Render Pass；不能假设零成本。

## 12. 线程模型

```text
PTY Reader / Pane Actor
  ├─ VT / Screen
  ├─ Graphics Parser
  └─ Scene snapshots
          │
          ▼
UI Main Thread
  ├─ Slint
  ├─ WGPU Renderer
  └─ Presenter
```

- GPU 资源只在 UI/渲染线程使用。
- 图片解码可放有界 worker pool；不要一图一线程。
- PTY 线程不做 GPU upload。
- Scene/Parser 不持有 wgpu 类型。

## 13. 资源限制（初始值，可配置）

```text
单图最大尺寸        8192 x 8192
单图解码内存        128 MB
传输缓存            64 MB
CPU 图片缓存        256 MB
GPU 图片缓存        512 MB
单会话 Placement    4096
动画帧数            256
并行动画            32
```

超限必须拒绝/回收，不能静默泄漏。

## 14. 验收不变量

```text
incremental render == full reference render
text scroll 不移动固定图片
image scroll 跟随其 RowId
ECH/EL/ED 只清除所属 placement
alt-screen 不泄漏 placement
resize/reflow 要么重映射 RowId，要么显式 drop
图片像素永不写入文本 framebuffer
首次点击灯箱有 source
关闭灯箱/预览无旧像素
```

测试矩阵：

```text
Kitty / Sixel / iTerm2(min)
主屏 / alt-screen
上滚 / 下滚 / history eviction
ECH / EL / ED / clear
单图 / 双图 / 透明 / 重叠
resize / 多 Pane
CPU / WGPU
15m soak
```

## 15. 当前实现状态

```text
master:
  CPU framebuffer + Slint Skia/OpenGL
  图片像素仍在文字 framebuffer
  Sixel/IIP + ECH/retain 启发式
  右键菜单已扁平化

feat/image-engine:
  vendor alacritty_terminal RowId
  docs/IMAGE_ENGINE_CONTRACT.md
  crates/graphics
  crates/scene
  crates/render
  尚未接入主渲染路径
  WGPU spike 已验证 texture import + take_snapshot（软件 adapter）
```

WGPU spike 结论：

```text
external texture 可持续更新并被 Slint 采样
xwd 对 GPU window 不可用
真实 NVIDIA adapter 仍需 :1 小窗口验证
```

## 16. 迁移顺序

```text
Phase 0  技术盲点验证（RowId / WGPU / dsh fixture）
Phase 1  RowId -> Frame，ImageStore -> Placement ownership
Phase 2  Scene + CPU backend 适配
Phase 3  Kitty MVP + Sixel 统一 ownership
Phase 4  WGPU backend + Slint presenter
Phase 5  真实 dsh 验收 + CPU/GPU parity
Phase 6  iTerm2 最小实现（延后）
Phase 7  移除旧 image_sig / retain / ECH 启发式
```

## 17. 已知风险

```text
Alacritty reflow/合并的行身份
dsh Kitty 实际字节与 z-order
WGPU/Slint unstable API 与真实 adapter
GPU texture 生命周期/持续更新
CPU/GPU 像素 parity
图片解码内存与 GPU 预算
```

## 18. 相关文档

- [`IMAGE_ENGINE_CONTRACT.md`](IMAGE_ENGINE_CONTRACT.md)：冻结接口
- [`ALACRITTY_ROW_ID.md`](ALACRITTY_ROW_ID.md)：vendored 补丁
- [`IMAGE_COMPOSITING.md`](IMAGE_COMPOSITING.md)：当前 CPU 图片合成与历史修复
- [`PLATFORM_GPU.md`](PLATFORM_GPU.md)：GPU 后端现状
- [`DIAGNOSTICS.md`](DIAGNOSTICS.md)：诊断/快照
