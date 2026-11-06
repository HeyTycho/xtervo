# 平台抽象与 GPU 渲染路线

> 状态：架构边界（2026-11-06）。当前实现仍是 X11 + 软件终端光栅；本文定义后续
> Wayland 移植与 GPU 渲染的拆分点，避免继续把平台代码绑进 `main.rs`。

## 1. 当前耦合点（审计结论）

| 位置 | X11 专属内容 | Wayland 风险 |
|---|---|---|
| `xconn.rs` | screen/pointer、`_NET_ACTIVE_WINDOW`、`_NET_WM_PID`、`_NET_WM_STATE`、图标、XShape 圆角 | Wayland 无这些全局窗口属性 |
| `quake.rs` | X11 passive grab（F12）、窗口查找 | Wayland 不允许全局 grab；需 compositor portal/快捷键协议 |
| `main.rs` resize | X11 优先 `_NET_WM_MOVERESIZE` 交 WM live resize；无 EWMH 时回退自绘 | Wayland 用 `xdg_toplevel.resize`，不能直接改坐标 |
| `render.rs` | fontdue 软件光栅 + `SharedPixelBuffer` 上传 | 与平台无关，但每帧全量拷贝，GPU 收益被吃掉 |
| `latency.rs` | XTest/XDamage 探针 | 仅测试工具，Wayland 需改用 compositor 帧回调 |

Slint 自身已通过 `slint` 的 `backend-winit` 处理窗口/输入/IME；`crates/app/Cargo.toml`
已启用 `renderer-skia` 与 `renderer-software`，有 GPU/驱动时 Slint UI 合成走 Skia GPU，
无 `/dev/dri` 时回退 `winit-software`。**终端字符帧仍是自绘软件光栅**，这是 GPU 化的主战场。

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
- `set_window_icon(...)`, `set_rounded_corners(win, radius)`（X11 实现；Wayland no-op）
- `begin_resize(dir) / resize_drag() / end_resize()`（X11 自己改 geometry；Wayland 交给 compositor）
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

### Phase 1（低成本，先做）：消除全量上传 + GPU UI 合成

- `Renderer` 长期持有 `SharedPixelBuffer`，只在尺寸变化时重建；`image()` 不再每帧
  `vec` + 全量 `copy_from_slice`（Codex 审计 P0）。
- 保留脏行更新；Slint 侧仍用 `Image`，但有 GPU 时 Skia 合成在 GPU。
- 指标：单 pane 1080p 每帧内存拷贝从 ~8MB 降到 ~0；多 pane FPS；RSS。

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
3. 新分支 `feat/gpu-renderer` 做 wgpu/Skia 原型；feature 开关 + 软回退。
4. 新分支 `feat/wayland-platform` 做 `WaylandPlatform`，用 Xephyr/Weston headless 验收。
