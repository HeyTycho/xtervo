# Checkpoint: 图片架构 / GPU 显示链路（2026-11-11）

状态：**本地 checkpoint，可回退；不是 master，不是正式发布。**

```text
branch: feat/image-engine
tag:    checkpoint-2026-11-11-image-engine
```

## 包含

```text
Image/row ownership
  alacritty RowId vendored
  Frame.row_ids
  ImageStore RowId 锚定
  Kitty graphics parser + Actor 接线
  Sixel/IIP 路径

2026-11-11 事故修复
  IIP filter 吞 ]/0x9D
  Frame::put zerowidth 清理
  图片 row<0 目标 y / row_span 防空
  ImageClearScanner 2J tail 重放
  partial Frame vs force_full 一致性测试
  Frame partial 逐 cell 对账

GPU 显示链路（opt-in，不是统一 GPU）
  XTERVO_GPU=1
  CPU framebuffer -> Slint 同一 Skia WGPU30 Device/Queue
  per-pane Rgba8Unorm texture
  slint::Image::try_from
  resize 期间 CPU，结束重建
  关闭 pane 释放纹理 / 重连 forget
  失败自动回 CPU/Skia
  fps/femtovg 误判修正

观测/文档
  xtervo.diag.v1 metrics/observability/scene-diff
  XTERVO_FRAME_DUMP / XTERVO_LINE_NUMBERS
  POSTMORTEM_2026-11-11_RENDER_RESIDUE.md
  RENDERING_ROADMAP.md
  scripts/ci_gate.sh
```

## 验证

```text
cargo test --workspace                          PASS
cargo test -p xtervo-app --bin xtervo             413 passed / 1 ignored
cargo test -p xtervo-core                        139 lib + 4 frame_consistency
scripts/ci_gate.sh                              PASS
scripts/e2e_image_scroll_clip.sh                5/5 PASS
15m CPU soak                                    PASS
15m GPU soak                                    PASS
  RSS 803.1 -> 844.2 MiB (+41.1, R²=0.12)
  threads 167 -> 167, fds 93 -> 93
```

## 启用

```bash
# 默认 CPU/Skia
./target/release/xtervo

# 可选 GPU 显示链路
XTERVO_GPU=1 ./target/release/xtervo

# 无 GPU adapter 的 headless 验证
SLINT_WGPU_CPU=1 XTERVO_GPU=1 ./target/release/xtervo

# 显式回退
XTERVO_GPU=0 ./target/release/xtervo
```

## 明确未完成

```text
不是统一 GPU Scene
没有 GPU glyph atlas
没有 GPU 图片/光标/z-order
没有默认切换 GPU
没有真实 NVIDIA/AMD 验证（当前 llvmpipe）
没有 device lost 注入
没有 30m release soak
没有合 master/发布
iTerm2 仍延后
```

## 回退

```text
git switch feat/image-engine
git reset --hard checkpoint-2026-11-11-image-engine
# 或从 tag 另开分支
```

不要把这个 checkpoint 当成 master 或正式 release；默认路径仍是 CPU/Skia。
