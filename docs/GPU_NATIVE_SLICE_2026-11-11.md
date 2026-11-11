# TERVO_GPU_NATIVE vertical slice（2026-11-11）

分支：`agent/gpu-native`（worktree `.worktrees/agent-gpu-native`）
关键提交：

| commit | 内容 |
| --- | --- |
| `5cff84e` | Frame → Scene → WGPU 文字/背景 slice，默认 GPU-native，`TERVO_CPU=1` 关闭 |
| `e40f7b8` | Slint 在首次 render 前 import texture 会空白：改为 render 后 import |
| `6faefdc` | merge B 的 `GpuImageRenderer`/`ImageTextureRegistry`/staged `render_scene`，接图片分层合成 |
| `762800b` | 隔离 Xvfb CPU/GPU/fallback e2e + raw RGBA alpha 证据 |
| `0069006` | fallback 字体分组探测；首个 WGPU present 后重绘；Sixel e2e |

## 目标与边界

真实终端 `Frame` **不经过 CPU framebuffer** 直接进入 GPU：

```text
tervo_core::Frame
  -> tervo_scene::SceneBuilder::from_frame   (TextBatch + Background + ImageInstance)
  -> GpuImageRenderer::render_scene
       background clear
       -> cell backgrounds (wide 2-cell span, hidden 只画背景)
       -> below-text images (SourceId texture registry)
       -> glyph atlas glyphs
       -> above-text images
  -> offscreen Rgba8Unorm TextTarget
  -> slint::Image::try_from(texture)          (render 之后 import)
  -> PaneView.image
```

- Device/Queue 来自 Slint `RenderingSetup` 的 `GraphicsAPI::WGPU30`（`render_probe`），
  与 `TERVO_GPU=1` stage 0 共用同一个 device；不创建第二个 wgpu 实例。
- 显式开关：
  - `TERVO_GPU_NATIVE=1`：本 slice。默认开启；`TERVO_CPU=1` / `TERVO_GPU_NATIVE=0` 关闭。
  - `TERVO_GPU=1`：原有 stage 0，CPU framebuffer 上传纹理，行为未改。
  - 两者同时设置时：GPU native 优先；失败才回 CPU，stage 0 不接管。
- 任何失败（拿不到 device、atlas/target 创建失败、image upload/分层渲染失败、
  `Image::try_from` 失败）只打印一次日志，本进程剩余时间回 CPU；下一次
  `redraw_pane_inner` 会 `force_full_redraw`，避免 GPU 期间旧 damage 历史残影。
  **不会**把 CPU framebuffer 包装成 GPU 成功。
- 不每帧整屏 copy：文字是 per-cell 实例；图片按 `SourceId` 只上传一次；
  正常路径没有 GPU→CPU 回读。`TERVO_GPU_NATIVE_DUMP` 只在首帧做一次只读回读
  （PPM + raw RGBA）作为像素/alpha 证据。

## render-wgpu 侧改动

- atlas：
  - `GpuTextRenderer::with_font_paths` 使用 CPU `FontSet` 的四个真实 face 文件
    （JetBrainsMono-Medium/Bold/Italic/BoldItalic），保证字形与 CPU 基线可比。
  - 主字体缺字时按需加载 CJK/symbol/emoji 轮廓 fallback
    （WQY MicroHei / DroidSansFallback / Noto CJK / DejaVu / NotoEmoji / Nerd Font），
    先用 `owned_ttf_parser` 做 cmap 探测，避免为 emoji 解析整条 CJK 链。
  - 所有字体都没有的字符：画显式 `?` marker，`unknown_glyphs`/`missing_glyphs`
    计数；绝不静默丢字/错位。彩色 emoji（NotoColorEmoji）不在本 slice。
- text：
  - wide cell：背景跨度 2 列；`wide_spacer` 整格跳过；CJK 字形按 advance 居中。
  - hidden：只画背景，不画字形/下划线；dim 映射 bold（与 CPU `style_of` 一致）。
  - underline/double-underline/strikeout 按 CPU 几何（cell 底部 -2 / 中间）。
  - selection/search 颜色由 Scene builder 解析，renderer 只计数并消费。
  - `TextLayout.default_background`：默认背景 cell 不再画不透明白块，
    保留 Scene clear 色/透明度语义。
- images：B 的 `ImageTextureRegistry`（`SourceId` 不可变 RGBA，一次上传）+
  `GpuImageRenderer` textured quad（source_rect UV / scissor clip / below-above
  z-order）。

## 验收

### 单元/集成测试

```text
cargo test -p tervo-app --bin tervo
# 414 passed, 0 failed, 1 ignored

cargo test -p tervo-render-wgpu
# lib 16 + gpu_text 11 + gpu_images 10 passed，2m image soak ignored
```

新增覆盖：wide/spacer 背景跨度与不双画、CJK fallback 计数、hidden、dim→bold、
underline/double/strike、Scene background clear、`SceneBuilder::from_frame`
对 wide/hidden/dim/selection 的语义。

### 隔离 Xvfb 像素对照

脚本：

```bash
TERVO_BIN=target/debug/tervo scripts/e2e_gpu_native.sh
```

- 拒绝使用 `:1`；默认 `:77`；每个 mode 独立 XDG/config 目录。
- 跑三路真实 app：CPU baseline、`TERVO_GPU_NATIVE=1`、失败路径
  （`TERVO_GPU_NATIVE=1` + 显式 software backend，拿不到 WGPU device）。
- 只比较 `take_snapshot()` 终端区域（去掉 32px tab bar），并检查
  draw calls / atlas uploads / gpu_native_frames / raw RGBA alpha。

本机（llvmpipe Vulkan，非硬件 GPU）实测，纯文字夹具：

```text
terminal_diff: size=720x528 differing=6134 ratio=1.6135%
  mae=0.403 pixels_over_8=4758 ink_cpu=8288 ink_gpu=8142 ink_iou=0.912
fallback_vs_cpu: differing=0 ratio=0.0000% mae=0.000
offscreen alpha: min=255 max=255 non255=0 count=380160
gpu_snapshot: gpu_native_frames=2 draw_calls=4 atlas_uploads=71
  atlas_bytes=7179 vertex_bytes=13376 fallback_glyphs=10 missing=0
  wide=7 spacer=7 hidden=6 dim=3 cursor_skipped=true background_from_scene=true
```

带 Sixel 图片夹具（`image_quads=1 image_draw_calls=1 image_uploads=1
image_sources=1 frame_images=1 scene_images=1`），连续 3 次 PASS：

```text
sixel_red_pixels: cpu=2373 gpu=2340
terminal_diff: size=720x528 differing=7527 ratio=1.9800%
  mae=0.742 pixels_over_8=6121 ink_cpu=9205 ink_gpu=8924 ink_iou=0.841
fallback_vs_cpu: differing=766 ratio=0.2015% mae=0.326
gpu_snapshot: gpu_native_frames=7 draw_calls=21 atlas_uploads=76
  image_uploads=1 image_sources=1
initial GPU frame cpu_us≈1.16s（fallback 分组探测前同一夹具 3.42s）
```

`fallback_vs_cpu` 的 0.20% 来自两次 CPU 进程之间 cursor blink 相位/AA；
fallback 仍是纯 CPU，没有 GPU 帧（快照行 `gpu_native_frames=0`）。

`TERVO_GPU_SNAPSHOT_DELAY_MS` 是测试旋钮：llvmpipe 首次 WGPU present 可能晚于
2.5s，e2e 用 6000ms，等 import 的 texture 真正被 Slint 采样后再
`take_snapshot()`。生产路径不依赖这个延迟。

### 15 分钟内存门禁

```
elapsed_s rss_kb pss_kb threads fds
0         548020 522303 72      22
...（每分钟采样，13 次稳定在 548–551 MB / 488–525 MB PSS）
841       215396 189147 72      22   # 结束后页回收，非增长
```

- 4516 个 GPU-native 帧（持续 CJK/彩色/下划线/滚动输出，约 5.4 fps）。
- RSS 峰值 551 MB，无单调增长；线程 72、FD 22 全程恒定；0 次 GPU fallback/panic。
- 证据：`docs/evidence/gpu-native/2026-11-11-soak-mem.log`、
  `docs/evidence/gpu-native/2026-11-11-e2e.txt`。
- 2 分钟 image soak 是 B 的 ignored 测试 `image_render_soak_two_minutes`；
  本 slice 未单独重跑 15m 图片 soak。

证据目录（默认 `target/gpu-native-e2e/`）：`cpu.png` / `gpu.png` /
`fallback.png`、`diff.txt`、`alpha.txt`、三个 `app.log`、GPU 离屏 `*.ppm` +
`*.rgba`。截图里 GPU 版与 CPU 版的文字、CJK、选中色/背景色、下划线一致；
1.61% 差异主要来自 GPU coverage 混合 vs CPU gamma 校正的抗锯齿边缘。

## 已知限制 / 后续

1. 默认已切换 GPU-native；`TERVO_CPU=1` 强制 CPU；`TERVO_GPU_NATIVE=0` 仅用于显式兼容 stage 0。
2. 光标仍走原有 Slint overlay（明确保留）；Scene 里有 cursor 但 GPU 不合成。
3. 彩色 emoji 不做（fontdue 只支持轮廓）；无字形时显式 `?` + 计数。
4. 背景透明度：Scene background 使用主题不透明 RGB；`background-opacity < 1`
   时 GPU 路径与 CPU 的窗口透明混合还未逐像素验证（默认 1.0）。
5. 图片：Sixel 单帧已在 Xvfb 夹具中端到端验证（GPU Scene 合成、source
   只上传一次、红块像素与 CPU 对齐）。多图 z-order/crop/透明图由 B 的
   `gpu_images`（10 passed）覆盖；长时图片 soak 未在本分支重跑。
6. atlas 固定 1024×1024、无 eviction；溢出会让整帧回 CPU（有明确错误）。
   CJK/emoji fallback 按字符类别排序探测（CJK/emoji/PUA），避免首次 emoji
   触发整条 CJK 链解析；首帧仍有一次 fontdue 字体解析成本（本机 1.2s）。
7. llvmpipe 是 CPU 光栅 Vulkan adapter；上面的数字是正确性证据，不是 GPU 性能。
8. 2 分钟 image soak 是 B 的 ignored 测试；本分支跑的是 15 分钟文字/CJK soak。
