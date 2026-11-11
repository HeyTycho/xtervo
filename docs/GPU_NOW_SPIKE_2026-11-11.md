# TERVO_GPU=1 WGPU 显示链路 spike（2026-11-11）

状态：**可选 opt-in，非默认**。默认路径（CPU 栅格化 -> `Image::from_rgba8` -> Skia/
software）不变。本分支只把“CPU 终端 framebuffer 的显示”改为走 Slint 渲染器同一个
WGPU 30 Device/Queue 上的纹理，**没有**做 GPU Scene / glyph atlas / 图片 GPU 合成。

## 链路

```text
Renderer（CPU 栅格化 RGBA8 framebuffer）
  -> Renderer::dirty_rows()（只上传本帧重绘行；首帧/resize 整帧）
  -> queue.write_texture(Rgba8Unorm, TEXTURE_BINDING|RENDER_ATTACHMENT|COPY_DST)
  -> slint::Image::try_from(texture)
  -> PaneView.image -> Slint 显示
```

Device/Queue 来源：`Window::set_rendering_notifier` 的
`RenderingState::RenderingSetup` + `GraphicsAPI::WGPU30 { device, queue, .. }`
（`crates/app/src/render_probe.rs` -> `crates/app/src/gpu_present.rs`）。
后端选择：`TERVO_GPU=1` 且未显式设置 `SLINT_BACKEND` 时，在任何 UI 初始化前调用
`slint::BackendSelector::new().require_wgpu_30(WGPUConfiguration::default()).select()`。

## 启用 / 回退

```bash
# 启用（无 GPU 的 headless 机器需 SLINT_WGPU_CPU=1 允许 llvmpipe CPU adapter）
SLINT_WGPU_CPU=1 TERVO_GPU=1 ./tervo

# 回退：不设置 TERVO_GPU（默认），或显式
TERVO_GPU=0 ./tervo

# adapter/feature 失败：require_wgpu_30/select 报错后自动 exec 重启并 TERVO_GPU=0，
# 日志写入原因，进程继续走默认 CPU/Skia（不会黑屏硬撑）；纹理/导入失败同理，
# 日志明确写“自动回退 CPU/Skia”，本进程剩余时间不再尝试 GPU。
```

无头验收钩子（仅诊断用）：

```bash
SLINT_WGPU_CPU=1 TERVO_GPU=1 \
TERVO_GPU_SNAPSHOT=/tmp/tervo-gpu TERVO_GPU_SNAPSHOT_EXIT=1 \
TERVO_GPU_SNAPSHOT_RESIZE=1 \
./tervo --cols 100 --rows 26 --shell /bin/bash -- -c 'printf "\nTERVO_GPU_REAL_CONTENT_2026\n\n"; sleep 120'
# 写 /tmp/tervo-gpu.before.ppm 与 /tmp/tervo-gpu.resized.ppm（程序化改窗口尺寸后）
```

## 已验证（本 worktree，孤立 Xvfb :97）

```text
cargo test -p tervo-app --bin tervo
  -> 413 passed; 0 failed; 1 ignored

# GPU：TERVO_GPU=1 + SLINT_WGPU_CPU=1，未设置 SLINT_BACKEND
BackendSelector::require_wgpu_30(...).select() = OK
actual_renderer=wgpu30（Slint Skia WGPU30）
[gpu] pane=1 已导入 1053x682 Rgba8Unorm 纹理到 Slint 同一 device

gpu.before.ppm  900x604  gpu_ready=true  nonbg=27319 distinct=404
gpu.resized.ppm 1060x724 gpu_ready=true  nonbg=31981 distinct=404
# 终端区域（y>=40）与 winit-software CPU 基线逐像素一致：
#   before 1549 nonbg 相同；resize 后 1351 相同；差异只在 y<31 的 tab bar 文字栅格化

# 自动回退：TERVO_GPU=1 但不设 SLINT_WGPU_CPU
require_wgpu_30/select: no GPU-backed WGPU adapter ... Set SLINT_WGPU_CPU=1
-> exec 重启 TERVO_GPU=0 -> winit-software，gpu_ready=false，快照非黑
```

证据文件（未入库，位于主仓库 `.tmp-gpu-agent/run-gpu-now/`）：
`gpu2.before.ppm`、`gpu2.resized.ppm`、`cpu.before.ppm`、`cpu.resized.ppm`、
`fallback.before.ppm`、`gpu2.stderr`、`cpu.stderr`、`fallback.stderr`。

## 15m GPU soak（release `5531643`）

```text
DURATION=15m TERVO_GPU=1 SLINT_WGPU_CPU=1 ./scripts/soak.sh
verdict: PASS
RSS: 803.1 -> 844.2 MiB (+41.1 MiB, +1.51 MiB/min, R2=0.12)
PSS: 772.9 -> 813.8 MiB (peak 884.4)
threads: 167 -> 167
fds:     93 -> 93
fatal=0 errors=0
```

说明：

```text
R2=0.12，非线性；15m 尾部已趋平，不是明显泄漏
GPU 路径用 llvmpipe（无 /dev/dri），CPU avg 95.5%
```

注意：soak 脚本在 `TERVO_GPU=1` 时必须不导出 `SLINT_BACKEND`，否则会走
Skia/OpenGL 而不是 WGPU；`scripts/soak.sh` 已修正。

## 未做 / 风险

- 未统一 GPU Scene，未做 glyph atlas、图片/光标 GPU 合成；光标仍是独立 CPU overlay。
- 纹理是直通 RGBA8，没有 CPU 预乘；终端默认背景不透明，opacity<1 时 alpha 合成
  可能与 `Image::from_rgba8` 路径有差异（未验证）。
- 行级上传依赖“纹理保存上一帧、`dirty_rows` 覆盖本帧变化”这一既有双缓冲不变量；
  若未来渲染器改成不轮换 buffer，需要重新评估。
- 首次 `pane_image` 前仍会短暂显示 CPU 图像；`RenderingSetup` 后通过事件循环
  `redraw_all()` 切换，窗口可见前完成，不影响验收。
- 当前机器无 `/dev/dri`，用 Vulkan llvmpipe；未验证真实 GPU 驱动、device lost、
  swapchain 重建路径。
- `TERVO_GPU=1` 与显式 `SLINT_BACKEND` 同时存在时尊重用户设置；若该后端仍提供
  WGPU30（Skia-on-wgpu 实测可以），链路照样启用，否则自动 CPU 回退。
