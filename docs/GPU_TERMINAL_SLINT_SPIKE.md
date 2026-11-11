# GPU 显示链路最小验收：真实 PTY framebuffer → Slint WGPU texture → take_snapshot

- 分支：`agent/gpu-spike`
- worktree：`.worktrees/agent-gpu-spike`
- 基于：`3aa402e`（`feat/image-engine`）
- 验收日期：2026-11-11
- 结论：**PASS（仅限“显示链路”这一层）**。真实 PTY 的终端帧被 app 现有 CPU
  `Renderer` 光栅化成 RGBA framebuffer，上传到 Slint 自己的 WGPU30
  `Device/Queue` 上，经 `slint::Image::try_from(wgpu::Texture)` 显示，
  `Window::take_snapshot()` 读回的像素与 CPU framebuffer **逐像素完全一致**
  （`mean_abs=0`、`max_abs=0`、`mismatch>8=0%`）。

本文只证明“真实终端 framebuffer 能经 Slint 的 WGPU device/queue 显示出来”。
**不证明** GPU Scene、glyph atlas、图片/光标 GPU 合成，也不代表 app 主流程已
切到 GPU。

## 1. 验收链路

```text
/bin/sh（真实 PTY，tervo_core::spawn）
  → Frame（tervo-core）
  → app 现有 CPU Renderer（examples/gpu_terminal_spike.rs 以 #[path] 复用
    crates/app/src/render.rs + glyph.rs）
  → RGBA8 framebuffer (414x198)
  → RenderingSetup 拿到 Slint GraphicsAPI::WGPU30 { device, queue }
  → device.create_texture(Rgba8UnormSrgb, TEXTURE_BINDING|COPY_DST|COPY_SRC|RENDER_ATTACHMENT)
  → queue.write_texture(framebuffer)
  → slint::Image::try_from(texture)
  → 最小 Slint Window 的 Image 组件
  → Window::take_snapshot() 与 CPU framebuffer 逐像素对照
```

example **不启动 app 主循环**，不依赖 `tervo-app` 的 Session/Workspace/window
事件循环；只复用 app 的软件终端光栅化模块。

## 2. 启用命令（本机实测）

本机没有系统级 `Xvfb`（`apt` 未安装，agent 无 root）。为了**不使用用户的
`:1`**，把 `xvfb` deb 解包到 worktree 局部目录，并在隔离的 `:77` 上运行：

```bash
cd .worktrees/agent-gpu-spike

# 仅本机环境：无 root 时把 xvfb 解到本地（一次性）
mkdir -p .xvfb-local && cd .xvfb-local
apt-get download xvfb
dpkg-deb -x xvfb_*.deb .
cd ..

# 1) 编译 example（复用主 target 的 Slint WGPU 构建）
CARGO_HOME=$PWD/../../.cargo \
CARGO_TARGET_DIR=$PWD/../../target \
CARGO_NET_OFFLINE=true \
  cargo build -p tervo-app --example gpu_terminal_spike

# 2) 隔离 Xvfb :77 + CPU WGPU adapter 跑验收
.xvfb-local/usr/bin/Xvfb :77 -screen 0 1280x720x24 -nolisten tcp &
XPID=$!
sleep 2
DISPLAY=:77 SLINT_WGPU_CPU=1 \
TERVO_GPU_SPIKE_OUT=$PWD/docs/evidence/gpu-spike \
  ../../target/debug/examples/gpu_terminal_spike
kill $XPID
```

也可以直接用辅助脚本（要求 PATH 或 `TERVO_XVFB` 里有 Xvfb；拒绝 `:1`）：

```bash
DISPLAY_NUM=:77 scripts/gpu-terminal-spike.sh
```

本机必须带 `SLINT_WGPU_CPU=1`：没有硬件 GPU，Slint 只提供 Vulkan
`llvmpipe` CPU adapter；不带该变量会明确报错
（`no GPU-backed WGPU adapter ... Set SLINT_WGPU_CPU=1`），不会静默假装成功。

## 3. take_snapshot 结果

实测日志（`docs/evidence/gpu-spike/run1.log`）：

```text
pty: generation=2 grid=46x9 text="TERVO-GPU-PTY ... RED-BG ... BLUE-BG ... PTY-FRAME-OK ..."
gpu: Slint WGPU30 adapter="llvmpipe (LLVM 15.0.7, 256 bits)" backend=Vulkan device_type=Cpu
gpu: uploaded 414x198 Rgba8UnormSrgb from PTY framebuffer; window=414x198
PASS gpu_terminal_spike: real PTY framebuffer 414x198 -> Slint WGPU30 texture -> take_snapshot;
  cpu_ink=5300 snap_ink=5300 ratio=1.000
  mismatch>8=0.000% mismatch>32=0.000% mean_abs=0.000 max_abs=0
  generation=2
```

- snapshot 尺寸：`414x198`，与 CPU framebuffer、窗口尺寸一致（1:1，无缩放）。
- 真实 PTY 文本：`TERVO-GPU-PTY`、`RED-BG`（红底）、`BLUE-BG`（蓝底）、
  `PTY-FRAME-OK`。
- 非背景像素：CPU `5300`，snapshot `5300`，比值 `1.000`。
- 逐像素对照：`>8` 与 `>32` 通道差的像素均为 `0.000%`；
  `mean_abs=0.000`、`max_abs=0`，即 **snapshot 与 CPU 输入逐字节一致**。
- 最强 64 个“墨迹”像素逐点复核通过；snapshot 中不存在兜底洋红
  `#ff00ff`（说明 Image 确实被绘制）。
- 证据（无损 PNG）：
  - `docs/evidence/gpu-spike/slint_snapshot.png`
  - `docs/evidence/gpu-spike/cpu_framebuffer.png`
- 同一命令用 `scripts/gpu-terminal-spike.sh` 在隔离 `:77` 上复跑一次，
  结果相同（`docs/evidence/gpu-spike/run2-script.log`）。
- 运行日志中的 `snapshot=...ppm` / `cpu=...ppm` 是原始快照路径；提交时无损
  转成上面的 PNG 以减小体积，像素内容未变。

## 4. 失败行为（不伪造 PASS）

example 在以下任一情况**不会**打印 `PASS`，并以非零码退出：

- `BackendSelector::require_wgpu_30` 选不到 WGPU30 后端；
- `RenderingSetup` 给出的不是 `GraphicsAPI::WGPU30`（会打印实际 API 后退出）；
- PTY 在 8s 内没有产生预期帧；
- 本机没有可用字体（`FontSet::load` 返回 `None`）；
- `slint::Image::try_from(texture)` 失败；
- 15s 内没有完成 WGPU setup 或 snapshot；
- snapshot 尺寸与 framebuffer 不一致；
- CPU framebuffer 非背景像素 < 200（终端内容没画出来）；
- snapshot 非背景像素 < 200（texture 没被采样）；
- snapshot/CPU 墨迹像素比不在 0.6–1.5；
- `>8` 通道差像素超过 1%，或 `mean_abs > 2.0`（不是同一份 framebuffer）；
- 最强墨迹像素逐点对照失败；
- snapshot 含洋红兜底背景。

没有“自动 CPU fallback 后仍报 GPU PASS”的路径：失败就是明确报错。

## 5. 已知限制 / 未完成项

1. **只验证显示链路**：Scene、damage 重绘、glyph atlas、图片（Sixel/IIP/Kitty）、
   光标 overlay 仍是 CPU 光栅化后整帧上传；没有 GPU Scene 或 GPU 文字/图片。
2. **两段式合成**：CPU 画到 RGBA → 上传 texture → Slint 再采样合成窗口；
   不是单 render pass / 零拷贝。
3. **一次性静态帧**：example 只上传一帧并验证；没有每帧更新、resize、
   device loss 恢复、present revision 验证。
4. **sRGB 依赖**：使用 `Rgba8UnormSrgb` 才得到逐字节一致；若改成
   `Rgba8Unorm`，中间色调会出现颜色空间差异（需重新定义容差，不能当等价）。
5. **Slint 1.18 unstable API**：`unstable-wgpu-30` / `renderer-Skia WGPU30`
   是 unstable feature，未来 Slint 升级可能破坏。
6. **无硬件 GPU**：只能用 `llvmpipe` CPU adapter（`SLINT_WGPU_CPU=1`）；
   本结果证明 Vulkan CPU adapter 上的 WGPU 纹理导入/显示链路成立，
   不等于真实 GPU 驱动/硬件路径也成立。
7. **环境**：本机无系统 Xvfb、无 root，验收用 worktree 内解包的 `xvfb`
   二进制 + 隔离 `:77`；用户 `:1` 窗口/进程全程未被触碰。
8. **未接入 app**：`tervo-app` 主程序没有任何代码引用本 example；它只是
   spike/兜底验收工具。

## 6. 变更文件

- `crates/app/examples/gpu_terminal_spike.rs`：新增验收 example。
- `crates/app/Cargo.toml`：仅 `[dev-dependencies]` 追加 Slint 的
  `unstable-wgpu-30` / `renderer-Skia WGPU` / `renderer-Skia WGPU30` features
  （与工作区主依赖取并集后等于已验证的构建，不影响 app 正常依赖）。
- `scripts/gpu-terminal-spike.sh`：隔离 Xvfb + 运行 example 的辅助脚本。
- `docs/evidence/gpu-spike/`：运行日志、基线测试日志、snapshot/CPU 帧 PNG。

## 7. 基线测试

`cargo test -p tervo-app --bin tervo` 结果见
`docs/evidence/gpu-spike/cargo-test-bin-tervo.log`，不改变 413 基线
（日志末尾为 `test result: ok. 413 passed; ...`）。
