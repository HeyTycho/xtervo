# 平台抽象与 GPU 渲染路线

> 状态（2026-11-06）：渲染后端默认已切到 Slint Skia/GL（`renderer-skia` +
> `renderer-skia-opengl`），终端字符光栅仍是 CPU，但已用双缓冲持久帧去掉每帧
> 整屏 `SharedPixelBuffer` 拷贝。无 DRM / GPU 初始化失败会自动回退
> `winit-software`。本文仍定义 Platform/FrameRenderer 边界与后续 GPU 化路线。

## 1. 当前耦合点（审计结论）

| 位置 | X11 专属内容 | Wayland 风险 |
|---|---|---|
| `xconn.rs` | screen/pointer、`_NET_ACTIVE_WINDOW`、`_NET_WM_PID`、`_NET_WM_STATE`、图标、XShape 圆角 | Wayland 无这些全局窗口属性 |
| `quake.rs` | X11 passive grab（F12）、窗口查找 | Wayland 不允许全局 grab；需 compositor portal/快捷键协议 |
| `main.rs` resize | winit accessor 调 `drag_resize_window`：X11 在 winit 自己的连接 ungrab 后发 `_NET_WM_MOVERESIZE`；不支持/无 WM 时回退自绘 | Wayland 用 `xdg_toplevel.resize`，不能直接改坐标 |
| `render.rs` | fontdue 软件光栅 + 双缓冲 `SharedPixelBuffer`（隔帧交替，`Image` 仅 refcount clone）；Skia 仍每帧上传纹理 | 与平台无关；已去掉每帧整屏 CPU 拷贝，但还没有安全的持久 GPU texture |
| `latency.rs` | XTest/XDamage 探针 | 仅测试工具，Wayland 需改用 compositor 帧回调 |

Slint 自身已通过 `slint` 的 `backend-winit` 处理窗口/输入/IME；`crates/app/Cargo.toml`
已启用 `renderer-skia`、`renderer-skia-opengl` 与 `renderer-software`：有 GPU/驱动时
Slint UI 合成走 Skia（优先 OpenGL，失败再试 wgpu/software surface）；无 `/dev/dri`
或 GL 初始化失败时回退 `winit-software`。**终端字符帧仍是自绘软件光栅**，这是 GPU
化的主战场；当前帧交接已持久化，Skia 纹理上传仍每帧发生。

## 2. 目标抽象（按依赖方向，不引入平台 cfg 到业务层）

```text
main/AppState ──► trait Platform
                    ├── X11Platform   (现有 xconn/quake)
                    └── WaylandPlatform (后续 winit + xdg-desktop-portal)

main/AppState ──► trait FrameRenderer
                    ├── SoftwareRenderer (现有 render.rs)
                    └── GpuRenderer       (后续 wgpu/Skia 纹理)
```

### Platform（窗口/输入/系统集成）

首批方法（从 `main.rs` 现有调用归纳）：

- `screen_size()`, `pointer_position()`
- `own_windows()`, `active_window()`, `activate_window(id)`
- `set_window_icon(...)`, `set_rounded_corners(win, radius)`（X11 SHAPE 默认关闭，实验开关；Wayland no-op）
- `begin_resize(dir) / resize_drag() / end_resize()`（winit accessor 交 compositor；无 WM/不支持时回退自绘 geometry）
- `register_quake_hotkey(...)`（X11 grab；Wayland 走 portal / compositor 快捷键，失败则提示）
- `diagnostics()`（平台相关窗口/焦点信息）

约束：业务层不出现 `x11rb` 类型；`Platform` 返回稳定的小结构体（`WindowId`、`Rect`、
`Capabilities`），而不是 X11 atom。Linux 编译时可按 feature 选 X11/Wayland 后端，运行时可
探测 `WAYLAND_DISPLAY`，但 **不让 UI 代码感知平台**。

### FrameRenderer（终端帧）

- `resize(cols, rows, cell, theme, opacity)`
- `draw(&Frame)`（脏行）
- `present() -> SurfaceFrame`（软件：像素缓冲；GPU：纹理/绘制列表）
- `screenshot()`（测试/诊断）
- `backend_name()`/`capabilities()`

软件实现先保持行为；GPU 实现独立 crate/模块，失败可回退软件，不能阻塞启动。

## 3. GPU 渲染分阶段

### Phase 1（已完成 2026-11-06）：消除全帧 CPU 拷贝 + GPU UI 合成

- `Renderer` 持有两块 `SharedPixelBuffer`，隔帧交替写入；`image()` 只把当前 buffer
  refcount clone 成 Slint `Image`，正常路径没有 `vec`/`copy_from_slice`。
- 每次切换 buffer 整屏重绘（2560x1413 实测 0.55–1.1ms），避免新 buffer 缺上一帧
  内容；`detach=false` 实测证明没有触发 `SharedVector` 的 copy-on-write。
- 保留脏行/光标/选区逻辑（当前整屏重绘以换取简单正确；见 render.rs 注释）。
- 指标：单 pane 2560x1413 `image()` 4.8ms → 0.00ms；resize apply 10.5ms（含两块
  14MB buffer 分配）与旧实现 11.0ms 基本持平；滚动/输入持续帧受益。
- 未做：Skia 持久 GPU texture（Slint 1.18 无安全 API），纹理上传仍每帧发生。

### Phase 2（中期）：GPU 终端光栅

- 字形 atlas（fontdue 生成一次）上传为 GPU 纹理；每 cell 以实例化 quad/纹理绘制。
- 首选 wgpu（跨 Vulkan/GL/软件 llvmpipe）或 Skia `Canvas`；**不作为默认依赖**，
  通过 feature 编译，运行时探测失败回退软件。
- 保持 `FrameRenderer` 接口不变；颜色/光标/选择/搜索高亮走同一 shader 状态。
- 指标：4K 60fps、P95 输入延迟、GPU/CPU 占用、无撕裂；软件后端行为不变。

### Phase 3（Wayland）

- `WaylandPlatform` 实现 `Platform`：窗口操作走 winit，全局快捷键走
  `xdg-desktop-portal` GlobalShortcuts（不可用则降级）；圆角由 compositor/CSD；
- 删除 XShape 作为 Wayland 路径（X11 保留 fallback）；
- 输入延迟探针改用 compositor frame callback / 内部 `present` 时间戳；
- 验收：GNOME/KDE Wayland 下启动、resize、多窗口、quake、IME、剪贴板、截图。

## 4. 明确不做

- 不把 X11 `xconn` 直接 cfg 到 UI 文件；不在 Slint 场景里塞 X11 指针。
- 不为 Wayland 重写 Slint 窗口层；复用 winit 后端。
- GPU 不是“打开一个开关”：终端光栅是自绘管线，必须单独实现并保留软件回退。

## 5. 迁移顺序（建议）

1. 抽 `Platform` trait + X11 实现，迁移 `main.rs` 的 xconn/quake 调用（行为不变）。
2. `Renderer` 持久缓冲（Phase 1），测量 FPS/RSS。
3. ~~新分支 `feat/gpu-renderer` 做 wgpu/Skia 原型~~ → 已完成默认 Skia/GL + 自动软件回退；
   下一步是 GPU 终端光栅/持久纹理。
4. 新分支 `feat/wayland-platform` 做 `WaylandPlatform`，用 Xephyr/Weston headless 验收。

## 6. 2026-11-06 headless 实测（Skia/llvmpipe）

测试环境：`Xvfb :104 -screen 0 2560x1440x24` + `.perf/openbox`，`LIBGL_ALWAYS_SOFTWARE=1`，
`SLINT_BACKEND=winit-skia`；`render_probe` 确认 `actual_renderer=skia-opengl`、
`GL_RENDERER="llvmpipe (LLVM 15.0.7, 256 bits)"`。**这是 Mesa CPU 光栅，不是 NVIDIA**。

| 场景 | 软件 (winit-software) | Skia/llvmpipe |
|---|---|---|
| 1080x736→1380x736，80 步/4ms | 11 applies，中位 3.10ms，最大 4.10ms | 14 applies，中位 1.80ms，最大 3.00ms；35 帧，渲染中位 2.85ms、最大 6.84ms |
| 2538x1396→2238x1396，80 步/4ms | 1 apply 9.60ms | 2 applies，中位 10.85ms，最大 12.70ms |
| watchdog 回退 | 0 | 0 |

帧缓冲（2560x1413，Skia/llvmpipe）：

- `Renderer::image()`：4.8ms（整屏 memcpy）→ **0.00ms**（双缓冲 refcount clone，`detach=false`）。
- 整屏 draw：0.55–1.11ms；resize apply 10.5–12.7ms（含两块 14MB buffer 分配，旧实现 11.0ms）。
- 背景图 cover 成本：同一 2560x1440 窗口持续帧，有背景 render 中位 **3.53ms**，
  背景隐藏后 **1.49ms**；因此仅在窗口面积 >1.5MP 的拖动期间临时隐藏，停下 200ms 恢复。

限制：llvmpipe 不能代表 NVIDIA 驱动；没有真机 GPU 就**不能**声称 GPU 帧率/延迟达标。
