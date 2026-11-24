# WGPU 文字 / Glyph Atlas 原型（2026-11-11）

分支：`agent/wgpu-text`
Worktree：`.worktrees/agent-wgpu-text`
基线：`98bbc2e` (`perf(gpu): 统计纹理上传帧/行/字节/耗时，提供 GPU profile 基线`)

这是 **原型/vertical slice**，不是 `XTERVO_GPU=1` 的最终替代，也没有接进
`xtervo-app` 主循环。目标是用证据回答一件事：

> 终端文字能否从 `xtervo_scene::TextBatch` 直接进入 GPU glyph atlas +
> instanced quad 管线，渲染到离屏 `Rgba8Unorm`，且不产生、不上传 CPU 终端
> framebuffer？

答案是能。GPU 结果与 CPU 参考合成逐像素对照：mean 0.004–0.020，max 1，
超过阈值 2 的像素为 0；Slint 的 `take_snapshot()` 与离屏 target 完全一致
（mean 0 / max 0）。

## 架构

```text
xtervo_scene::TextBatch
  ├─ 每个 cell → 背景 solid instance（cell.background）
  ├─ underline → 额外 solid instance
  └─ 每个 ASCII 字符
       ├─ 非 ASCII → '?' fallback（计数 unknown_glyphs）
       ├─ GlyphAtlas::ensure(char, px, style)
       │    ├─ 命中：复用 atlas slot，0 上传
       │    └─ 未命中：fontdue 光栅化 → 写入 R8Unorm atlas 的 glyph 矩形
       └─ GlyphInstance { pixel rect, atlas UV, foreground RGBA }

一个 render pass
  ├─ draw 1: solid pipeline   (6 vertices × N instances)
  └─ draw 2: glyph pipeline   (6 vertices × M instances)
        ↓
离屏 Rgba8Unorm texture（TEXTURE_BINDING | RENDER_ATTACHMENT | COPY_SRC）
        ↓
slint::Image::try_from（同 device/queue）→ Slint 窗口
```

关键点：

- **Atlas 格式选 R8**：终端 glyph 只需要 coverage mask；颜色由 instance
  携带。1024×1024 R8 = 1 MiB。若用 RGBA 是 4 MiB，其中 3 MiB 恒为冗余。
- **只在 miss 时上传**：`queue.write_texture` 只写新 glyph 的
  `w×h` 矩形，不是 atlas，更不是整屏。warmup 后每帧 atlas 上传为 0。
- **instance buffer 复用**：只有容量不足时才重建（`next_power_of_two` 增长），
  不是每帧新 buffer。
- **失败先于提交**：atlas 满、glyph 过大、layout 非法都会在创建
  command encoder 之前返回；target 保留上一帧内容，不会半帧。
- **无 CPU framebuffer**：生产路径中没有 `Renderer::pixels()`、没有整屏
  RGBA buffer、没有 `copy_texture_to_buffer`。`read_target_rgba8` 只用于
  test/example 的验证，Slint 展示路径不调用它。
- **v1 坐标系**：布局按整像素取整，glyph quad 正好覆盖 atlas texel；linear
  sampler 在像素中心采样到精确 texel，因此 GPU/CPU 可做到 max diff = 1
  （固定管线 UNORM 舍入）。

## 代码位置

| 文件 | 内容 |
| --- | --- |
| `crates/render-wgpu/src/atlas.rs` | 字体发现/bold-italic 变体、fontdue 光栅化、shelf 打包、R8 atlas、上传与统计 |
| `crates/render-wgpu/src/text.rs` | `TextBatch` → instance 构建、布局、两条 pipeline、单 render pass、`TextTarget`、readback |
| `crates/render-wgpu/src/text.wgsl` | instanced solid/glyph shader，screen uniform + atlas texture/sampler |
| `crates/render-wgpu/src/reference.rs` | 仅验证用 CPU 参考合成器、pixel diff、PPM |
| `crates/render-wgpu/src/headless.rs` | 无窗口 wgpu adapter/device 获取 |
| `crates/render-wgpu/tests/gpu_text.rs` | 8 个端到端 GPU 测试（含失败场景） |
| `crates/render-wgpu/examples/wgpu_text_bench.rs` | 无窗口 release 性能探针 |
| `crates/render-wgpu/examples/text_atlas.rs` | Slint 同 device/queue 显示 + `take_snapshot` 验证 |

未修改 `crates/app` 主循环、未修改 `crates/scene`、未添加图片/光标/z-order。

## 运行命令

环境变量：`CARGO_HOME=$PWD/.cargo`、`CARGO_TARGET_DIR=$PWD/target`、
`CARGO_NET_OFFLINE=true`（本机 cargo 缓存已够，未访问网络）。

```bash
# 1) 单元 + 端到端 GPU 测试（需要 headless wgpu adapter 和字体）
cargo test -p xtervo-render-wgpu --test gpu_text -- --nocapture

# 2) release 性能 / 像素探针（无窗口，不需 Xvfb）
XTERVO_WGPU_TEXT_FRAMES=300 \
  cargo run --release -p xtervo-render-wgpu --example wgpu_text_bench

# 3) Slint 同 device/queue 显示 + snapshot（隔离 Xvfb，绝不碰用户 :1）
Xvfb :79 -screen 0 1400x800x24 -nolisten tcp &
DISPLAY=:79 SLINT_WGPU_CPU=1 env -u SLINT_BACKEND \
  cargo run -p xtervo-render-wgpu --features slint-presenter --example text_atlas
```

本机实际执行说明：

- 系统未安装 `Xvfb`；使用了本仓库其他 worktree 已有的解包二进制
  `.worktrees/agent-wgpu/.tools/xvfb-root/usr/bin/Xvfb`（未联网、未装包）。
  其他机器若安装 `xvfb` 包，可直接用上面的标准命令。
- 字体：`/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf`（另有系统
  `-Bold/-Oblique/-BoldOblique`，bold/italic 是真实字形而非伪粗体）。
  可用 `XTERVO_WGPU_FONT=/path/font.ttf` 覆盖；`XTERVO_WGPU_FONT_BOLD` /
  `_ITALIC` / `_BOLD_ITALIC` 可单独指定变体。
- wgpu adapter 只有 `llvmpipe (LLVM 15.0.7) / Vulkan / device_type=Cpu`。
  因此下面的性能数字是 **软件 Vulkan 上的提交侧 CPU 时间**，不是硬件
  GPU 吞吐。

## 像素对照

参考实现是 validation-only 的 CPU 合成器：它使用 **同一份 atlas CPU
mirror 字节** 和 **同一批 instance geometry**，按与 GPU 相同的顺序合成。
因此对照隔离的是 WGPU 管线（几何、UV、linear sampling、alpha blend），
而不是 fontdue 本身；这比“再跑一遍 app CPU 渲染器”更严格地定位 GPU 路径。

| 场景 | 分辨率 | glyph quads | draw calls | mean abs diff | max abs diff | >2 像素 |
| --- | --- | --- | --- | --- | --- | --- |
| 多行 + 多字号 + 颜色/下划线/bold/italic/selected | 640×360 | 145 | 2 | 0.004472 | 1 | 0 |
| release bench 最终帧（80×24 + 24px + 12px） | 1280×720 | 1867 | 2 | 0.019493 | 1 | 0 |
| Slint overview snapshot vs target | 1280×720 | 221 | 2 | 0.0000 | 0 | 0 |
| Slint scroll/style snapshots (5 scenes) | 1280×720 | 157–2304 | 2 | 0.0000 | 0 | 0 |

- `pixels_over_2 = 0`；differing pixels 的量级说明差异只在抗锯齿 round-to-
  nearest 边界的 ±1，不是布局/UV 错位。
- 产物示例：`docs/evidence/wgpu-text/overview-snapshot.png`、
  `styles-snapshot.png`、`scroll-snapshot.png`、`bench-frame.png`。
- 运行产物（PPM）默认在 worktree `target/wgpu-text-atlas/`；`gpu` /
  `cpu-reference` / `diff-x8` 三份可逐像素复查。
- 滚动场景 5 帧：offset 0 有 19 个新 glyph 上传，offset 1–4 均为
  `atlas_uploads=0`、`atlas_misses=0`，证明命中缓存后不重复上传。

Slint 路径的关键输出：

```text
text_atlas: Slint WGPU30 adapter="llvmpipe ..." backend=Vulkan device_type=Cpu
text_atlas: imported 1280x720 Rgba8Unorm texture on Slint's device
scene=overview glyphs=221 solids=252 draw_calls=2 ...
scene=scroll-0 glyphs=2304 solids=2616 draw_calls=2 ...
...
PASS text_atlas: 5 scenes presented through slint::Image on Slint's WGPU30 device;
Window::take_snapshot matches the offscreen target
```

## 性能（release，llvmpipe/Cpu）

`XTERVO_WGPU_TEXT_FRAMES=300`，1280×720，主区域 80×24 @16px，另有 24px/12px
section：

```text
BENCH cpu avg=988.095µs p50=845.255µs p95=1.304263ms max=4.09328ms
BENCH wall avg=1.052573ms p95=1.368825ms (包含 TextBatch 构造)
BENCH draw_calls/frame=2.00 render_pass/frame=1
      glyph_quads max=1867 solid_quads max=2273
      atlas_uploads=0 atlas_cached=570
BENCH bytes atlas=1048576 target=3686400 total=4734976
      vertex_upload/frame_avg=162352 full_screen_rgba=3686400
      full_screen_upload_ratio=0.0440
DIFF mean=0.019493 max=1 pixels_over_1=0 pixels_over_2=0
```

- 每帧 2 个 draw call（背景 solid + glyph），1 个 render pass。
- 每帧 CPU 上传只有 instance 数据：平均 162,352 B，是 1280×720 RGBA
  （3,686,400 B）的 4.4%；atlas 上传为 0。若走 CPU framebuffer 上传，
  至少会是 3.69 MB/帧，量级差 22 倍以上。
- 纹理内存：atlas 1,048,576 B + target 3,686,400 B = 4,735,976 B。
- CPU 时间包含 instance 构建、atlas 查询、buffer 写入、encoder 录制和
  `queue.submit`；不包含 GPU 执行（无 timestamp query）。llvmpipe 下
  `submit` 可能同步做工作，因此数字偏保守。

## 失败场景 / 非 happy path 证据

| 场景 | 行为 | 证据 |
| --- | --- | --- |
| Atlas 溢出 | 64×64 atlas + 32px `A..Z`：第 7 个 glyph 触发 `AtlasError::Full`（`'G' 17x25` 放不下），target 逐字节保持上一帧 | `atlas_overflow_is_reported_and_leaves_target_untouched` |
| 单 glyph 过大 | 16×16 atlas + 64px `M` → `GlyphTooLarge` | 同上测试 |
| 空场景 | 只 clear，`draw_calls=0`、`glyph_quads=0`、`atlas_uploads=0`，输出每像素等于 clear | `empty_scene_clears_without_draw_calls_or_uploads` |
| resize | 320×200 → 640×400 重建 target 后重渲染；新图左上 320×200 与旧图 max diff = 0；0 宽/超限被拒 | `resize_recreates_target_and_preserves_pixel_geometry` |
| 未知 glyph | 非 ASCII `☃` → `'?'`，`unknown_glyphs=1`；与直接画 `'?'` 的像素完全一致 | `unknown_glyphs_fall_back_to_question_mark_and_are_counted` |
| 非法 layout | `cell_width=0` / `font_size_px=0` → `InvalidLayout`，不提交 | `invalid_layout_is_rejected_before_rendering` |
| 失败后恢复 | overflow 后同一 renderer 仍能渲染新帧 | 同 overflow 测试 |
| style 缺失 | 没有 bold/italic 字体文件时 regular fallback 并计数；本机有变体，因此验证 bold/italic 像素确实改变 | `bold_italic_faces_and_underline_are_observable` |

## 限制 / 未完成项

1. **未接 app 主路径**：按要求不修改主循环，`XTERVO_GPU=1` 仍是原来的
   CPU framebuffer → texture 阶段 0；本原型不默认切 GPU。
2. **ASCII only**：没有 shaping、组合字符、CJK、emoji；所有非 ASCII 统一
   `?` 并计数。真实终端需要 fallback 字体链和宽字符/组合标记处理。
3. **Atlas 无淘汰/无多页**：只做 shelf 打包，满即报错。长时间多字号会话需要
   eviction 或分页 atlas。
4. **z-order/图片/光标未做**：单 pass 中所有 solid（背景、下划线）先于所有
   glyph；多 section 重叠语义未实现。图片、光标按边界要求排除。
5. **bold/italic 依赖系统变体字体**；缺失时静默 fallback regular，并记录
   `style_fallbacks`，不是伪粗体。
6. **无硬件 GPU 数据**：本机只有 llvmpipe；release 数字证明“没有每帧整屏
   CPU copy”和提交侧开销，不能代表真实 GPU 吞吐。
7. **未做 15m soak**：只做了约 2 分钟 release 连续帧 soak（见下）；没有改
   app/soak 阈值。原型分支不合并，因此未按大功能 15m 门禁执行。
8. **Xvfb 环境缺口**：系统包缺失，使用了仓库内其他 worktree 的解包二进制；
   这是环境限制，不是代码依赖。
9. **Slint 路径**是“最小显示 + snapshot”，不是完整多 pane 窗口；同一
   device/queue 导入已验证。
10. **无 GPU timestamp / 无 vsync 数据**；性能报告是 CPU 提交侧。

## 约 2.5 分钟 soak / 内存

`XTERVO_WGPU_TEXT_FRAMES=130000` release 连续帧（160 s，约 812 fps，适配器
llvmpipe CPU）：

```text
BENCH cpu avg=1.048939ms p50=931.896µs p95=1.4183ms max=6.222956ms
BENCH draw_calls/frame=2.00 render_pass/frame=1 atlas_uploads=0 atlas_cached=570
DIFF mean=0.019694 max=1 pixels_over_1=0 pixels_over_2=0
PASS ... 130000 frames
```

每 10 s 采样 `/proc/<pid>/status`：

| 时刻 | RSS | HWM | 线程 | FD |
| --- | --- | --- | --- | --- |
| 10 s | 189,484 KB | 189,484 KB | 114 | 4 |
| 60 s | 191,468 KB | 191,468 KB | 114 | 4 |
| 120 s | 193,056 KB | 193,056 KB | 114 | 4 |
| 150 s | 193,856 KB | 193,856 KB | 114 | 4 |

RSS 的线性斜率 **不是渲染器泄漏**：benchmark 把每帧 `Duration` 存进
`cpu_times` / `wall_times` 以便算 p50/p95，2×130,000×16 B ≈ 4.16 MB，与
+4.37 MB 吻合。独立的 `memory_probe`（不保存每帧样本）验证：

- static（复用同一个 TextBatch）60,000 帧：5 s 后 188,852 KB；
  45 s 仍 188,852 KB；50 s 一次分配器台阶到 189,380 KB 后保持到结束。
- dynamic（每帧重建 TextBatch/字符串）60,000 帧：5 s 后 189,236 KB；
  30 s 台阶到 189,768 KB 后保持到结束。

线程数 114、FD 4 全程不变；atlas cached 570、uploads 0、instance buffer
无增长。因此 renderer 的 atlas/buffer 生命周期在 60k–130k 帧内稳定；剩余
RSS 台阶来自 llvmpipe/glibc 分配器，而非每帧保留对象。

复现：

```bash
MODE=static FRAMES=60000 \
  cargo run --release -p xtervo-render-wgpu --example memory_probe
MODE=dynamic FRAMES=60000 \
  cargo run --release -p xtervo-render-wgpu --example memory_probe
```

说明：这不是 app 的 15m soak，也没有修改任何 app/soak 阈值；原型分支不
合并主路径。

## 结论

- vertical slice 可运行、可测试：`cargo test -p xtervo-render-wgpu` 10 个
  unit + 8 个 GPU 测试全绿；release bench PASS；Slint example PASS。
- 核心目标“不走 CPU 终端 framebuffer”成立：TextBatch → fontdue atlas →
  instanced quads → offscreen texture，CPU 每帧只上传实例数据；atlas 命中后
  0 上传。
- 主要 blocker 不是管线正确性，而是产品化范围：非 ASCII/shaping、atlas
  eviction、app 接线、硬件 GPU 实测，以及完整 z-order/图片/光标。
