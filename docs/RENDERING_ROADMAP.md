# Tervo 渲染终极目标与路线图

状态：规划基线（2026-11-11，`feat/image-engine` @ `1435187`）。  
本文按**终极目标**排序，不按“最快看到 GPU”排序。

相关：

- [`ARCHITECTURE.md`](ARCHITECTURE.md)
- [`IMAGE_ENGINE_CONTRACT.md`](IMAGE_ENGINE_CONTRACT.md)
- [`POSTMORTEM_2026-11-11_RENDER_RESIDUE.md`](POSTMORTEM_2026-11-11_RENDER_RESIDUE.md)
- [`DIAGNOSTICS.md`](DIAGNOSTICS.md)

## 1. 终极目标

Tervo 是一个原生 Rust 终端工作台：

```text
稳定：不丢字节、不丢行、不丢图、不 panic
正确：文本/图片/选区/光标/搜索在所有缩放下一致
统一：一份 Scene，CPU 与 WGPU 两个后端，同一套语义
GPU-native：终端主体由 GPU 合成，而不是 CPU 画完整 framebuffer
可回退：GPU 不可用/device lost 时，CPU 路径仍可工作
可诊断：每一层都有第一处分歧工具，不靠截图猜
可发布：有内存/soak/回归门禁，不靠“看起来好了”
```

### 终极形态

```text
PTY
 -> alacritty Term（RowId 稳定）
 -> Immutable Scene
      TextBatch / ImageInstance / Cursor / Selection / Damage
 -> Renderer trait
      CPU backend（正确性基线/回退）
      WGPU backend（主路径）
 -> RenderedFrame
      CPU pixels（fallback）
      wgpu::Texture（GPU）
 -> Slint terminal texture
 -> Slint UI composite
 -> swapchain
```

关键约束：

```text
1. Scene 不依赖 wgpu/Skia/Slint/alacritty。
2. Renderer 不读 VT 状态，只消费 Scene + Damage。
3. 终端和 Slint UI 共用同一个 Device/Queue。
4. 默认路径不产生 CPU 终端 framebuffer。
5. GPU 失败自动回 CPU；不回退到“整屏重绘”。
6. 图片生命周期只认 RowId/Placement，不认 framebuffer 像素。
7. 所有增量路径都有 partial vs force-full 差分测试。
```

### 非目标

```text
自研 VT 状态机
自研窗口/输入法框架
iTerm2 全协议一次性支持
终端内视频/Web
为了 GPU 而放弃 CPU fallback
把“CPU framebuffer 上传 GPU”称为统一 GPU
```

## 2. 当前基线（不要高估）

已落地：

```text
alacritty RowId vendored 补丁
Kitty graphics parser + Actor 接线
Sixel/IIP 路径
图片裁顶/负 row/2J tail/zerowidth 修复
Scene / graphics / render / render-wgpu 骨架
SlintTexturePresenter（独立 spike）
诊断 CLI（metrics/observability/scene-diff）
frame_consistency / image_layer_diff / Slint presentation 测试
事故复盘与排障纪律
```

未落地：

```text
app 主路径仍是 legacy Frame + CPU Renderer
WGPU 只 clear，未接 app
没有 GPU glyph atlas
没有 GPU 图片纹理/z-order
Scene 没有接 app
live metrics/trace 未接渲染循环
没有默认 GPU
没有跨平台/驱动矩阵
```

当前 release：

```text
b5c8c6f0fcba
```

只在 feature 分支，**不是 master**，不发版。

## 3. 阶段与门禁

### P0：事故闭环与防复发（0.5–1 天）

目标：把这次事故变成不会复发的工程约束。

交付：

```text
[ ] 15m soak 结论（RSS/PSS/线程/FD 是否收敛）
[ ] CI gate：
    cargo test --workspace
    frame_consistency
    image_layer_diff_tests
    slint_image_presentation
    e2e_image_scroll_clip
    e2e_dsh_image_preview（已知 zoom 坐标 fail 单列）
[ ] FilterAudit：
    filter input/passed/consumed/dropped 计数
    debug 下 dropped != 0 直接失败
[ ] tervo diag trace-layer：
    bytes -> filter -> Term -> Frame -> framebuffer -> Slint
    输出第一处分歧
[ ] 15m/30m soak 报告
```

退出条件：

```text
全部门禁绿
没有默认整屏兜底/每帧全屏 copy
15m soak 无线性内存增长
```

### P1：Scene 提取与 CPU 后端对齐（2–4 天）

目标：在 GPU 之前，先把“终端到底要画什么”固定下来。

交付：

```text
[ ] SceneBuilder 从 Term/Frame 生成不可变 Scene
[ ] TextBatch：cell 文本 + 属性 + 选中/搜索
[ ] ImageInstance：source_id、source_rect、cells、z、policy
[ ] Cursor/Selection 作为 Scene 层
[ ] DamageRegion 统一
[ ] CPU Renderer 实现 Renderer trait，消费 Scene
[ ] app 通过 adapter 使用 CPU Scene 后端
[ ] legacy 逐像素路径与 Scene CPU 路径逐像素对照
```

退出条件：

```text
CPU Scene 路径与 legacy 路径像素一致
现有 dsh/image/scroll e2e 全过
无 VT/图片行为变化
性能不回退超过预算
```

> 不能跳过 P1 直接做 WGPU。否则会在 GPU 端再重复一次“多份语义、多个几何实现”。

### P2：WGPU 文字主路径（3–5 天）

目标：终端文本真正由 GPU 画，不再是 CPU framebuffer 上传。

交付：

```text
[ ] 从 Slint RenderingSetup 取同一 Device/Queue
[ ] 终端离屏 texture（Rgba8Unorm/Srgb）
[ ] glyph raster -> atlas texture（R8/RGBA）
[ ] instanced glyph quads + WGSL shader
[ ] damage scissor/局部重绘
[ ] text texture -> slint::Image::try_from
[ ] TERVO_GPU=1 可选启用；失败回 CPU
```

退出条件：

```text
ASCII/CJK/emoji/组合字符/宽字符与 CPU 基线一致
resize/DPI/滚动正确
take_snapshot 与 CPU 截图对照
无每帧整屏 CPU 合成
```

### P3：WGPU 图片与 z-order（2–4 天）

交付：

```text
[ ] ImageResource -> GPU texture（source_id 持久）
[ ] source_rect 作为 UV
[ ] cells_w/h 目标几何
[ ] z-index / above_text / CompositePolicy
[ ] Kitty/Sixel 图片 GPU 合成
[ ] RowId placement 滚动
[ ] 删除/2J/alt-screen/清屏
[ ] 图片 GPU 测试矩阵
```

退出条件：

```text
负 row/裁顶/重叠/z/删除与 CPU 逐像素一致
dsh 图片预览/滚动无残留
无 CPU 图片合成
```

### P4：统一呈现与默认 GPU（2–3 天，**release blocker，不可延后**）

默认 GPU 不是“后续可选优化”。本 milestone 的发布门禁要求：

```text
不设置 TERVO_GPU 时默认选择 GPU
显式 TERVO_CPU=1 才强制 CPU
无 adapter/device lost 自动回 CPU
默认路径必须是 GPU-native 文字/图片，不是 CPU framebuffer 上传
```

因此 P4 依赖 P2/P3；如果 P2/P3 未完成，发布延期，而不是把默认 GPU 推迟。

交付：

```text
[ ] 多 pane texture / presenter 生命周期
[ ] CPU/WGPU 运行时切换
[ ] device lost -> CPU fallback + 重建
[ ] 默认 TERVO_GPU（先可选，后默认）
[ ] live metrics：frame/damage/image/atlas/texture
[ ] 15m/30m soak
```

退出条件：

```text
默认 GPU 在 NVIDIA + llvmpipe 通过
无 GPU 环境自动回 CPU
30m soak 内存/线程/FD 收敛
发布候选 gate 全绿
```

### P5：协议/体验打磨（持续）

```text
iTerm2 IIP（延后，不半实现）
Kitty 动画/共享内存/文件传输
Sixel 完整参数
字体 shaping/color emoji 完善
多窗口/多显示器/DPR
性能 p95/延迟目标
```

## 4. 为什么这个顺序

```text
先 Scene，后 GPU
  否则 GPU 端会再实现一套 cell/image/z 语义。

先 CPU Scene 对照，后 WGPU
  CPU 是唯一正确性基线；GPU 是优化后端。

先文字，后图片
  文字是热路径，图片是复杂生命周期；分开验收。

先可选 GPU，后默认
  驱动/device lost/soak 没过，不能默认。

先 correctness，后性能
  这次事故的根因就是增量/缓存没有真相校验。
```

不按这个顺序的代价：

```text
直接 CPU framebuffer -> texture
  只能算 GPU 显示，不能算 GPU 渲染；之后还要重做。

直接上 WGPU 全量
  文本/图片/z/DPI 同时爆炸，无法定位。

先默认 GPU
  驱动问题会掩盖语义问题。
```

## 5. 测试矩阵

### 文本

```text
ASCII / CJK / emoji / 组合字符 / 宽字符
粗体/斜体/下划线/删除线
选区/搜索/光标
多字号/DPI
```

### 图片

```text
Kitty / Sixel
source_rect / cells / pixel offset
z<0 / z=0 / z>0 / above_text
row<0 / 裁顶 / 完全滚出
删除 placement / 删除 image / 2J / alt screen
```

### 终端状态

```text
滚动/CUP/scroll region/alt screen
resize/DPR
历史/清屏/RIS
```

### 后端

```text
CPU
WGPU NVIDIA
WGPU llvmpipe（CI/headless）
device lost
无 adapter -> CPU fallback
```

### 性能/稳定性

```text
p95 frame time
CPU 使用率
RSS/PSS
线程/FD
atlas/texture 峰值
15m/30m soak
```

## 6. 性能预算（初稿，后续测量校准）

```text
输入到上屏 p95：目标 < 16ms（普通输出）
滚动/图片移动：目标 60fps 稳定
每帧 CPU 分配：稳态 0（复用）
终端 framebuffer CPU 合成：默认路径 0
GPU 纹理上传：只上传变化资源，不整屏每帧
atlas 命中率：> 95%（普通文本）
```

## 7. 并行 agent 编制

只在 18:00 后开大任务；白天只测量/文档/小修。

```text
A. core-filter
   FilterAudit + IIP/Sixel/Kitty 字节守恒

B. core-frame
   Frame/Scene 契约、damage 校验、row_id

C. layout-image
   PlacementRect 单一几何、图片生命周期

D. render-cpu
   Scene CPU backend + legacy 对照

E. render-wgpu
   Device/Queue、glyph atlas、text pipeline

F. render-wgpu-image
   图片 texture/z-order

G. observability
   live metrics/trace/layer diff

H. qa-soak
   CI gate、Xvfb/dsh e2e、15m/30m soak
```

规则：

```text
一个层一个 owner
独立 worktree
先交失败测试/测量证据
一个 integrator 合并
禁止同时改 render.rs 的多个分支
```

## 8. 第一晚（18:00 后）执行清单

```text
18:00–18:20
  冻结 Scene/Renderer/ImagePlacement 契约
  确认 15m soak 结论与内存归因

18:20–22:00
  A: FilterAudit
  B: trace-layer + frame 校验
  C: PlacementRect 收敛
  G: live observability 设计/接线
  H: CI gate

22:00–23:00
  集成、全量测试、review

23:00–00:00
  15m soak；P1 启动条件评审
```

GPU/Scene 的实现从 **P1/P2** 开始，不在第一晚直接“全量 WGPU”。

## 9. 发布门禁

```text
cargo test --workspace
frame_consistency（真实 raw）
image_layer_diff
slint_image_presentation
image scroll/dsh preview e2e
15m soak（触渲染）
30m soak（发布候选）
CPU fallback 验证
无默认整屏重绘/每帧 copy
```

不满足：

```text
不合 master
不 tag
不发版
```

## 10. 终极完成定义

```text
[ ] 默认终端主体由 GPU 合成
[ ] CPU fallback 仍可用且像素对齐
[ ] 同一 Scene 两后端
[ ] glyph atlas / image texture / z-order 完整
[ ] RowId/Placement 生命周期可靠
[ ] damage 驱动，无整屏兜底
[ ] live diagnostics 可定位第一处分歧
[ ] 30m soak 收敛
[ ] NVIDIA + llvmpipe + 无 GPU 三套路径通过
[ ] 发布文档/回滚路径完整
```

在此之前，所有“GPU 已接好”的说法都必须带限定：

```text
GPU 显示链路?
GPU 文字?
GPU 图片?
默认 GPU?
```

不允许再把“CPU framebuffer 上传 texture”说成“统一 GPU 渲染”。
