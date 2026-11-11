# GPU 渲染性能目标与基线（2026-11-11）

状态：**stage 0 已可运行；本文定义“做到最好”的验收，不把 stage 0 当完成。**

## 1. 现状

`TERVO_GPU=1` 当前链路：

```text
CPU Renderer 逐像素画终端 framebuffer
-> queue.write_texture（只传 dirty rows，首帧/resize 整帧）
-> slint::Image::try_from
-> Slint/Skia WGPU 合成
```

这解决了“显示走 GPU/同 device/无回读”，但终端内容仍是 CPU 合成。

小 pane 实测（80x24，Xvfb llvmpipe）：

```text
[render] pane=1 draw=0.05..0.26ms image=0.00ms
[gpu-profile] upload_frames=1 rows=528 bytes=1520640 upload_us=487.79
```

结论：

```text
CPU raster 本身不贵
full 纹理上传 1.5MB/约 0.49ms 已经是可测成本
4K 约 33MB/帧，不能每帧整帧上传
```

因此性能优化重点不是“把 CPU 画得更快”，而是：

```text
去掉 CPU 终端 framebuffer
把变换/合成/上传移到 Scene + GPU
```

## 2. 终极目标

```text
Term -> Scene
  TextBatch / ImageInstance / Cursor / Selection / Damage
-> WGPU Renderer
  glyph atlas
  instanced glyph quads
  image textures + UV/source_rect + z-order
  damage scissor
-> 终端 texture
-> Slint UI 合成
```

硬约束：

```text
不产生 CPU 终端 framebuffer（默认路径）
CPU fallback 仍可用，且与 GPU 像素对齐
不每帧整屏 upload
不每帧新建 texture/buffer
不 GPU->CPU readback
终端与 Slint 同一 Device/Queue
```

## 3. 性能预算（待测量校准）

### 3.1 帧

```text
普通输出/滚动 p95: < 16ms
连续滚动/图片移动: 60fps 稳定
输入到上屏 p95: < 20ms（非 GPU 同步等待）
```

### 3.2 CPU

```text
Scene build: < 1ms（典型 200x50）
每帧 CPU 分配: 稳态 0
默认路径 CPU 终端 framebuffer: 0
glyph raster: 只在 cache miss
```

### 3.3 GPU 上传

```text
普通输入: <= 1 行/少量 glyph，不上传整屏
滚动: 只上传变化行/instance
图片: 只上传新 source_id，一次
resize/主题: 允许一次整帧
```

### 3.4 内存

```text
atlas: 上限固定，LRU 可回收
image texture: 随 source 生命周期释放
pane texture: 随 pane 释放
15m soak RSS 非线性增长；30m release 收敛
```

## 4. 分阶段验收

### Stage 0：CPU framebuffer -> WGPU texture（已有）

```text
[✓] 同 device/queue
[✓] per-pane texture
[✓] dirty rows
[✓] CPU fallback
[ ] 4K/真实 GPU 性能
[ ] 30m
```

### Stage 1：Scene + CPU 对照

```text
[ ] SceneBuilder 从 Term/Frame 生成不可变 Scene
[ ] CPU Scene 后端与 legacy 逐像素一致
[ ] damage/row_id/图片语义固定
```

### Stage 2：WGPU 文字

```text
[ ] glyph atlas（R8/RGBA）
[ ] instanced quads + WGSL
[ ] 一个 render pass 到 offscreen texture
[ ] CPU/WGPU 文字像素对照
[ ] 无 CPU framebuffer 文字路径
[ ] 每帧 draw call/upload 统计
```

### Stage 3：WGPU 图片/光标

```text
[ ] image texture/source_rect/UV
[ ] z-order/above_text
[ ] RowId placement
[ ] cursor overlay
[ ] 图片生命周/回收
```

### Stage 4：默认 GPU

```text
[ ] 真实 NVIDIA/AMD + llvmpipe
[ ] device lost -> CPU fallback
[ ] 15m/30m soak
[ ] 默认切换
```

## 5. 基准矩阵

```text
终端尺寸: 120x32 / 200x50 / 3840x2160
内容: ASCII / CJK / emoji / 组合字符
操作: typing / scroll / alt-screen / images / resize
后端: CPU / WGPU llvmpipe / real GPU
测量: frame p50/p95, CPU%, upload bytes/frame, draw calls, atlas hit, RSS
```

## 6. 禁止的“伪优化”

```text
每帧整屏 queue.write_texture
每帧新建 texture/SharedPixelBuffer
每帧 take_snapshot/copy_texture_to_buffer
为 GPU 再跑一个 clear/quad 全屏 pass
整窗 CPU 合成多 pane
在 UI 线程 device.poll(wait)
只 request_redraw 不重设 image 属性
```

## 7. 当前判定

```text
TERVO_GPU=1 = GPU 显示链路，不是统一 GPU 渲染
不能作为默认
不能因为“窗口正常/截图正常”验收性能
```

下一阶段从 **Scene + WGPU 文字 glyph atlas** 开始；图片/光标随后，默认 GPU 最后。
