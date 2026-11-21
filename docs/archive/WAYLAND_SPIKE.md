# Wayland 原生支持 Spike（W-06）

> **后续实施见 [`WAYLAND_ADAPTATION.md`](../WAYLAND_ADAPTATION.md)**（Phase 1：
> 能力探测 + 优雅降级，GNOME 46 实测）。本文保留 2026-11 的只读 spike 结论。
>
> 分支：`feat/wayland-spike`，基线 `0902954`（基于 master `0902954`，未合并 master）。
> 性质：**可行性报告 + 只读探测**。本轮不改默认架构、不新增依赖、不实现半套 IME。
> 结论先行：Tervo 在原生 Wayland 下能启动、渲染、全屏，resize 代码路径也已接上；
> 但**剪贴板/PRIMARY、文件拖放、全局快捷键**在原生 Wayland 存在真实阻断，
> GNOME 42 上目前只能把 **XWayland** 当作可用路径。IME 路径完整但本机无法端到端验证。

---

## 1. 支持矩阵

图例：✅ 实测可用；⚠️ 有代码路径/有条件可用，未完成验收；❌ 明确不可用或未实现。

| 能力 | 原生 Wayland | XWayland | 依据 / 说明 |
|---|---|---|---|
| 启动、窗口、PTY、软件渲染 | ✅ 实测 | ✅ 实测 | 无 `DISPLAY`、仅 `WAYLAND_DISPLAY` 下 headless Mutter 42.9 启动成功 |
| Skia + OpenGL/EGL | ✅ 实测（llvmpipe） | ✅ 既有 | `actual_renderer=skia-opengl`，`GL_RENDERER="llvmpipe"`；无真实 GPU |
| 启动尺寸、DPI、全屏 | ✅ 实测 | ✅ | 启动 configure `540x208`；`--fullscreen` 发出 `xdg_toplevel.set_fullscreen`，configure `1280x800` |
| 交互式拖边/角 resize | ⚠️ 代码路径完成，未实测 | ✅ 既有 | `winit::Window::drag_resize_window` → `xdg_toplevel.resize`；X11 分支另走 EWMH 预检 |
| 窗口定位、激活、Quake 几何 | ❌ | ⚠️ 仅 X11 窗口 | Wayland `set_outer_position` 是 no-op、`outer_position` 返回错误；Quake 依赖 `xconn::screen_size`/`activate_own_window` |
| IME（text-input-v3 / XIM） | ⚠️ 代码与协议就绪，未 e2e | ✅ XIM 既有路径 | 隐藏 `TextInput` + Slint winit 后端；headless compositor 的 `wl_seat.capabilities(0)`，无键盘/输入法可测 |
| 剪贴板（CLIPBOARD） | ❌ GNOME 42 纯 native；⚠️ 支持 data-control 的合成器可用 | ✅ 实测回退 | arboard 需要 ext-/wlr-data-control；Mutter 42 只提供 `wl_data_device` 与 primary-selection |
| PRIMARY 选区 | ❌ GNOME 42 纯 native；⚠️ 同上 | ✅ 实测回退 | `OSC 52 ; p` 与 copy-on-select 写 PRIMARY 均走 arboard |
| 文件拖放（XDND / wl_data_device） | ❌ winit 0.30.13 无 Wayland DnD 实现 | ✅ X11 XDND 既有验收；本轮未重测 | winit Wayland 后端源码中没有 `data_device` / `DroppedFile`；Slint 1.18 也不投递外部 DataTransfer |
| 全局快捷键 / Quake 热键 | ❌ 无 Wayland 全局 grab | ⚠️ 只在 XWayland 窗口聚焦时可能生效，且会误报“已注册” | `quake.rs` 仅 X11 passive grab；`org.freedesktop.portal.GlobalShortcuts` 在本机 portal 1.14.4 不存在 |
| 多窗口 | ✅ 进程级 | ✅ | 每窗口独立进程（`window.rs`）；但 Wayland 下位置/激活不可控 |
| 窗口图标 / app_id | ❌ | ✅ `_NET_WM_ICON` | X11 专属；Wayland 需要 `.desktop` + app_id，不能写窗口 icon |
| Bell | ⚠️ 仅 stderr BEL 回退 | ✅ X11 Bell | `bell.rs` X11 请求；无 X11 时写 BEL，是否发声取决于宿主终端/桌面 |
| 自动深/浅色 | ✅ `gsettings` | ✅ | 不依赖显示协议 |
| `--diagnose` / 运行日志 | ⚠️ 环境项可用，X11 项误导 | ⚠️ 同上 | 在“Wayland + XWayland”下会报告 XWayland 的屏幕/其他 X11 窗口，像是本进程信息 |
| 延迟/FPS/XTest/Damage 工具 | ❌ | ✅ | 测试工具，非产品路径 |

**一句话结论**：原生 Wayland 可以作为“能用的终端窗口”继续推进，但当前不能承诺
剪贴板、文件拖放、Quake 全局热键；这三项要么补 Wayland 协议实现，要么明确回退到
XWayland。GNOME 42（Ubuntu 22.04）上，**XWayland 是唯一完整的剪贴板方案**。

---

## 2. 实测环境与边界

本机不是 Wayland 会话（`XDG_SESSION_TYPE=x11`、`DISPLAY=:1`、无 `WAYLAND_DISPLAY`），
为了做只读验证，使用 **headless Mutter** 临时起了一个真实 Wayland compositor：

- Ubuntu LTS；GNOME Shell 42.9 / Mutter 42.9；`xdg-desktop-portal 1.14.4`
- 无 `/dev/dri`（无真实 GPU）；Mesa llvmpipe 软件 OpenGL
- 无 Wayland 会话可借用，因此用：
  - `gnome-shell --headless --wayland --no-x11 --virtual-monitor 1280x800`：纯 Wayland；
  - `gnome-shell --headless --wayland --virtual-monitor 1280x800`：同时启动 **Xwayland**（`DISPLAY=:0`），用于验证真正的 XWayland 路径。
- headless compositor 的 `wl_seat.capabilities(0)`，**没有键盘/指针输入**；因此
  IME、DnD、交互式 resize 只能做代码/协议审计，不能做端到端验收。
- 真实 NVIDIA/AMD/Intel GPU、GNOME/KDE 真机会话、fcitx5/ibus 都未测试。

实测使用了两个二进制：
- 仓库根 `target/release/tervo`（构建 commit `585fd21`，与基线 `0902954` 只差一个
  `docs/TASK_BOARD.md`，代码等价）；
- 本 worktree 自己构建的 `.target-wayland/debug/tervo`，用于确认当前 HEAD 行为一致。

日志与临时脚本在 `.logs/`（已 gitignore，不进入提交）。

---

## 3. 源码路径盘点

### 3.1 后端选择：Wayland 优先，且没有“强制 X11”的运行时开关

- Slint 依赖与 feature 在 `Cargo.toml:18`：
  `backend-winit` + `renderer-skia` + `renderer-skia-opengl` + `renderer-software`
  + `unstable-winit-030`。`backend-winit` 在 `i-slint-backend-selector-1.18.1/Cargo.toml:49-54`
  同时打开 `backend-winit-x11` 与 `backend-winit-wayland`，所以 **X11 和 Wayland 平台代码都编进了二进制**。
- Wayland feature 会打开 `winit/wayland`、`arboard/wayland-data-control`、
  `i-slint-renderer-skia/wayland`、`softbuffer/wayland`（`i-slint-backend-winit-1.18.1/Cargo.toml:88-99`）。
- winit 运行时选择：`WAYLAND_DISPLAY`/`WAYLAND_SOCKET` 非空就选 Wayland，否则才看
  `DISPLAY`（`winit-0.30.13/src/platform_impl/linux/mod.rs:734-753`）。
  winit 0.30 没有 `WINIT_UNIX_BACKEND` 之类的环境变量强制 X11；**要测 XWayland 路径，
  必须 `env -u WAYLAND_DISPLAY` 让 winit 选 X11**。这是本轮实验的重要操作约束。
- Tervo 自己不选平台，只在 `main.rs:7289-7305` 选渲染后端（默认 `winit-skia`；
  无 `/dev/dri` 自动 `winit-software`），并在 `main.rs:7486-7502`/`9938-9954`
  做 Skia→software 的兜底回退。`logging.rs:67-79` 已把
  `DISPLAY`/`WAYLAND_DISPLAY`/`XDG_SESSION_TYPE`/`SLINT_BACKEND` 写进日志。

### 3.2 窗口尺寸、resize、全屏

- UI 8 个 `ResizeHandle` 只发 `start-window-resize` / `window-resize-drag` /
  `end-window-resize`（`crates/app/ui/main.slint:6920-6972`）。
- Rust 侧 `main.rs:8090-8122` 开始拖动，`main.rs:1580-1645` 优先把拖动交给
  `winit::Window::drag_resize_window`；X11 下先查 `_NET_SUPPORTED`，Wayland 下直接走
  `xdg_toplevel.resize`。winit Wayland 实现见
  `winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:381-383` 与
  `.../window/state.rs:432-443`。
- 失败/超时后回退“自绘 resize”（`main.rs:8124-8160`），但自绘依赖
  `xconn::query_pointer`（X11 指针）。Wayland 下如果 winit 的
  `xdg_toplevel.resize` 失败，**没有可用的自绘回退**；`ensure_resize_conn` 还会去连
  XWayland，拿到的是 XWayland 全局坐标，不是 Wayland 窗口坐标。
- `on_resized`（`main.rs:9615-9665`）处理 configure/DPR/布局；全屏通过
  `ui.window().set_fullscreen(true)`（`main.rs:9981-9994`）交给 winit，Wayland 下
  实际发出 `xdg_toplevel.set_fullscreen`（本轮抓包确认）。
- winit Wayland 明确不支持窗口位置：`outer_position`/`inner_position` 返回
  `NotSupportedError`，`set_outer_position` 是空实现
  （`winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:258-275`）。
  这直接影响 Quake 的 `set_position(x, 0)`（`main.rs:1997-2010`）。

### 3.3 IME（text-input-v3 / zwp_text_input / XIM）

- Tervo 的 IME 宿主是 UI 里一个 2×2 的透明 `TextInput`
  （`crates/app/ui/main.slint:2618-2670`），点击 pane 时 `ime.focus()`；
  组合期间 `key-pressed` 直接 reject（回车/退格/Esc 归输入法），提交文本走
  `edited` → `ime-commit`。Rust 侧 `main.rs:7748-7766` 把提交文本交给统一输入分发；
  候选窗位置由 `update_ime_spot`（`main.rs:4603-4674`）计算并调用
  `winit::Window::set_ime_cursor_area`。
- Slint winit 后端把 winit 的 `Ime::Preedit` / `Ime::Commit` 转成
  `UpdateComposition` / `CommitComposition`（`i-slint-backend-winit-1.18.1/winitwindowadapter.rs:1429-1446`）；
  只有获得焦点的 `TextInput` 才会收到组合事件，`WindowEvent` 里的
  `UpdateComposition`/`CommitComposition` 被过滤掉
  （`i-slint-core-1.18.1/platform.rs:648-656`），所以**终端 pane 本身不直接收 IME 事件**，
  必须依赖这个隐藏 TextInput。
- IME 只在 TextInput 聚焦时请求：`input_method_request(Enable)` 调
  `winit_window.set_ime_allowed(true)`，`Disable` 时关闭
  （`i-slint-backend-winit-1.18.1/winitwindowadapter.rs:2114-2139`）。
  `set_ime_cursor_area` 在 Wayland 下会转成逻辑坐标再发
  `zwp_text_input_v3.set_cursor_rectangle`（`winit-0.30.13/.../wayland/window/mod.rs:602-609`）。
- winit 0.30.13 的 Wayland IME **只绑定 `zwp_text_input_manager_v3` v1**
  （`winit-0.30.13/.../wayland/seat/text_input/mod.rs:28`），没有 v1/v2 兼容实现；
  只支持 text-input-v3 的合成器/输入法组合才可用。X11 则走 winit 的 XIM 实现
  （`winit-0.30.13/src/platform_impl/linux/x11/event_processor.rs` 中 `ime`/XIM 路径），
  这是现有中文输入在 X11/XWayland 下的基础。
- **未验证点**：候选窗位置、preedit 是否可见（TextInput 是透明 2px，应用内不画
  preedit，只能依赖输入法自己的候选窗）、菜单/设置关闭后的焦点归还、kwin/GNOME
  对 text-input-v3 的行为差异。本轮 headless seat 没有键盘，无法触发。

### 3.4 剪贴板 / PRIMARY / OSC 52

- 应用直接使用 `arboard`（`crates/app/Cargo.toml:29`），没有使用 Slint 的 clipboard API。
  启动时 `clipboard: arboard::Clipboard::new().ok()`（`main.rs:7691`），之后
  `with_clipboard` 惰性创建（`main.rs:5335-5341`）。
- `set_clipboard` / `set_primary` / `get_primary` 在 `main.rs:5343-5381`；
  OSC 52 的 store/query/clear 在 `main.rs:6694-6743`。
  **没有 X11/Wayland 分支**：可用性完全由 arboard 决定。
- arboard 3.7.0 的 Linux 逻辑：只要 `WAYLAND_DISPLAY` 存在，就先尝试
  Wayland data-control；失败则回退 X11；没有 `DISPLAY` 时直接失败
  （`arboard-3.7.0/src/platform/linux/mod.rs:131-150`）。
  `wayland-data-control` feature 已通过 Slint 的 Wayland feature 打开，
  依赖 `wl-clipboard-rs 0.9.5`。
- `wl-clipboard-rs` 要求合成器支持 **ext-data-control 或 wlr-data-control**；
  PRIMARY 还要求 ext-data-control 任意版本，或 wlr-data-control v2
  （`wl-clipboard-rs-0.9.5/README.md:18-21`）。它明确建议有窗口的应用使用
  `wl_data_device`/`smithay-clipboard`，而不是 data-control。
- **Mutter 42.9 不提供 data-control**：本轮 `WAYLAND_DEBUG=1` 抓到的全局列表有
  `zwp_text_input_manager_v3`、`wl_data_device_manager v3`、
  `zwp_primary_selection_device_manager_v1`，但没有
  `ext_data_control_manager_v1` / `zwlr_data_control_manager_v1`。
  因此纯 Wayland、无 XWayland 时 arboard 返回失败，Tervo 只能打印
  `[clipboard] 不可用`。这与实机结果一致（见 §4）。
- XWayland 下 arboard 回退到 X11，Mutter 负责 X11↔Wayland 剪贴板桥接；
  本轮用真正的 Xwayland 验证了 CLIPBOARD 和 PRIMARY 的写入/回读。
- 注意 `main.rs:5360` 的注释“arboard 的 Linux 扩展 API 不需要 Wayland data-control”
  只对“已回退到 X11”成立；**纯 Wayland PRIMARY 同样需要 data-control**，注释应修正。

### 3.5 文件拖放（XDND / wl_data_device）

- Slint UI 有 `DropArea`（`crates/app/ui/main.slint:2871-2882`），但 Slint 1.18 的
  winit 后端不把外部文件拖放转换成 `DataTransfer`；Tervo 因此用
  `on_winit_window_event` 监听 `HoveredFile` / `DroppedFile`
  （`main.rs:5588-5643`），并把路径插入聚焦 pane（`drop_files_into_pane`）。
- 命中哪个 pane 依赖 `drop_target_at`（`main.rs:5562-5585`）：它明确在
  **非 X11 窗口**直接返回 `None`，然后查 X11 root 指针再换算。Wayland 下没有
  root 指针，这段逻辑不成立；`DroppedFile` 的兜底才是“当前聚焦 pane”。
- 关键阻断：**winit 0.30.13 的 Wayland 后端完全没有 DnD 实现**。
  `winit-0.30.13/src/platform_impl/linux/wayland/` 下搜索不到
  `data_device`/`DroppedFile`/`HoveredFile`；只有 X11 的
  `src/platform_impl/linux/x11/event_processor.rs:473-602` 实现 XDND。
  因此原生 Wayland 下不会产生 `HoveredFile`/`DroppedFile`，Tervo 的桥接收不到任何事件，
  文件拖放表现为“没反应”。
- X11 的 XDND 路径已有既有验收（`docs/FEATURE_MATRIX.md:178`）；
  XWayland 下理论上走同一条 X11/XDND 路径，但本轮没有重新注入验证。
- 结论：原生 Wayland DnD 不是“补一个 hover 高亮”的问题，需要 winit 升级
  或单独接 `wl_data_device`；**本轮明确不做半套实现**。

### 3.6 全局快捷键 / Quake

- `quake.rs:1-58` 只实现 X11 被动 grab：连 `xconn`、`grab_key` root、后台线程
  `wait_for_event`。`main.rs:9591-9596` 在窗口初始化时**无条件**调用
  `quake::spawn_listener`（即使配置 `quake_enabled=false`，默认就是 false），
  失败只打日志。
- Wayland 没有等价的全局键盘 grab 协议；可移植方向是
  `org.freedesktop.portal.GlobalShortcuts`（需要 D-Bus/portal 支持），
  或合成器专用接口。本机 `xdg-desktop-portal 1.14.4` 的 portal introspection
  里没有 `GlobalShortcuts` 接口；GNOME 42 也没有。
- XWayland 下代码能连上 XWayland 的 X server 并 grab，但 XWayland 的键盘焦点只在
  X11 客户端窗口上；原生 Wayland 窗口聚焦时 XWayland grab 收不到 F12。
  本轮实测在 Wayland 窗口下仍打印 `[quake] 全局热键 F12 已注册`，**这是误报**；
  用户会以为 Quake 可用。修复至少需要：Wayland 后端不注册 X11 grab，并明确
  UI 提示“原生 Wayland 不支持全局热键”；是否接 portal 另立任务。

### 3.7 GPU / EGL

- 默认 `winit-skia`（有 `/dev/dri`）或 `winit-software`（无 DRM），代码在
  `main.rs:7289-7305`；Slint 的 Skia OpenGL 链路由 `glutin 0.32.3` /
  `glutin-winit 0.5.0` 提供 EGL 上下文。
- 本轮 headless Wayland + llvmpipe 下强制 `SLINT_BACKEND=winit-skia` 成功：
  `[render] actual_renderer=skia-opengl`，`GL_VENDOR="Mesa"`，
  `GL_RENDERER="llvmpipe (LLVM 15.0.7, 256 bits)"`，
  `GL_VERSION="OpenGL ES 3.2 Mesa 23.2.1"`。说明 **Wayland EGL surface 路径是通的**，
  但 llvmpipe 是 CPU 光栅，不能当真实 GPU 性能证据。
- 真实 NVIDIA/AMD/Intel、混合显卡、挂起恢复、HDR/分数缩放在 Wayland 下未测。
  `render_probe` 已能区分 Skia/GL 与 winit-software，不需要新探针。

### 3.8 X11-only 辅助能力（Wayland 下的降级点）

`crates/app/src/xconn.rs` 中的这些函数都是 X11/XWayland 语义：

- `screen_size`（`:461`）、`pointer_position`/`query_pointer`（`:434`）、
  `active_window`（`:517`）、`own_windows`（`:556`）、`find_own_window`（`:322`）、
  `activate_own_window`（`:471`）、`set_window_icon`（`:392`）、
  `wm_supports_moveresize`（`:209`）、`start_wm_resize`（`:269`）、`connect`（`:441`）。
- 启动时 `main.rs:9958-9979` 用 `own_windows`/`tervo_windows` 打日志、告警；
  `main.rs:10002-10015` 用 `_NET_WM_ICON` 设图标；`main.rs:2010` 用
  `activate_own_window` 拉起 Quake。
- 这些调用在“Wayland + XWayland”下会连上 XWayland，看到的是 **XWayland 的屏幕/其他
  X11 窗口**，而不是本进程的 Wayland surface。本轮实测：
  - `--diagnose` 报告 `screen=5120x1440`、`pointer=0,0`、其他 Tervo 的 X11 窗口；
    而实际虚拟显示器是 1280x800；
  - native Wayland 窗口下 `[ui] 设置窗口图标失败: 找不到本进程的窗口`；
  - 启动日志把另一个 X11 Tervo 窗口误报为“可能挡住点击”。
- `bell.rs:38-58` 用 X11 `Bell`；`latency.rs`、`xkey/xclick/xdrop/xwin` 都依赖 XTest/
  XDamage/XDND，属于测试工具，不应作为产品 Wayland 能力。
- 结论：需要“平台能力门控”，而不是让 X11 函数在 Wayland 下“碰巧连上 XWayland”。

---

## 4. 实测记录（只读）

### 4.1 纯 Wayland：能启动，剪贴板/快捷键不可用

```text
[render] requested_backend=winit-software（显式设置 SLINT_BACKEND）
[render] requested_backend=winit-software；actual_renderer=winit-software（GL/GPU 不可用或已显式回退）
[ime] spot pane=1 cell=0,0 win=0.0,32.0 phys=0,32 scale=1.000 anchor=false
READY TIMING ... cell=9x22
[clipboard] ts_ms=... osc52 store pane=1 selection=clipboard bytes=19
[clipboard] 复制 19 字节: "NATIVE-WAYLAND-CLIP"
[clipboard] 不可用
[clipboard] ts_ms=... osc52 query pane=1 selection=clipboard bytes=0
[quake] DISPLAY 未设置
[ui] 设置窗口图标失败: DISPLAY 未设置
```

- 无 `DISPLAY`、仅 `WAYLAND_DISPLAY=wayland-0`，headless Mutter 42.9，debug 与 release
  二进制行为一致；窗口/PTY/渲染正常，**没有任何 panic**。
- `[ime] spot` 说明隐藏 TextInput 聚焦和 `set_ime_cursor_area` 调用路径确实执行了；
  但 headless seat 无键盘，无法验证 text-input-v3 的 preedit/commit。
- CLIPBOARD/PRIMARY 均 `不可用`，与 Mutter 42 不提供 data-control 的协议列表一致。

### 4.2 真 Xwayland：winit 仍是 native Wayland，剪贴板回退成功

在 `gnome-shell --headless --wayland` + `Xwayland :0` 下，同时设置
`WAYLAND_DISPLAY=wayland-0 DISPLAY=:0`：

- 应用日志 `[ui] 设置窗口图标失败: 找不到本进程的窗口` → 没有自己的 X11 窗口，
  winit 按优先级选了 Wayland（在另一组“Wayland + host X11”实验中，
  `xwininfo -root -tree` 也确实找不到 Tervo 的 X11 窗口）；
- `OSC 52 ; c` / `OSC 52 ; p` 写入后日志显示 `复制/PRIMARY 写入` 成功，
  随后的 query/reply 读到 19/20 字节 → arboard 回退 X11 的读写链路是通的；
- 在“Wayland + host X11（:1）”对照实验中，独立用 `xsel -b -o` / `xsel -p -o`
  读到了 marker，证明确实落到 X11 CLIPBOARD/PRIMARY；
- 同一进程下 `[quake] 全局热键 F12 已注册`（XWayland grab 误报），
  `[ui] 设置窗口图标失败: 找不到本进程的窗口`。
- 把 `WAYLAND_DISPLAY` unset、只留 `DISPLAY=:0` 时，Tervo 作为 X11/XWayland 客户端：
  `xwininfo` 能看到窗口，`_NET_WM_ICON` 设置成功，剪贴板/PRIMARY 正常。

### 4.3 Skia/EGL 与全屏

- `SLINT_BACKEND=winit-skia TERVO_FORCE_GPU=1`（纯 Wayland、llvmpipe）：
  `actual_renderer=skia-opengl`，`GL_RENDERER="llvmpipe (LLVM 15.0.7, 256 bits)"`，
  `GL_VERSION="OpenGL ES 3.2 Mesa 23.2.1-1ubuntu3.1~22.04.4"`。
- `--fullscreen` + `WAYLAND_DEBUG=1`：客户端发出
  `xdg_toplevel.set_fullscreen(nil)`，compositor 回 `configure(1280, 800, ...)`，
  应用日志 `[ui] fullscreen=true (--fullscreen)`。

### 4.4 Wayland 全局能力（从客户端 `WAYLAND_DEBUG=1` 抓取）

```text
wl_compositor v5                  xdg_wm_base v4
wl_data_device_manager v3         wl_seat v5 (capabilities(0) in headless)
zwp_text_input_manager_v3 v1      zwp_primary_selection_device_manager_v1 v1
xdg_activation_v1 v1              zxdg_output_manager_v1 (v3)
```

- 有 `zwp_text_input_manager_v3`：说明 Mutter 42 支持 text-input-v3，winit 可以绑定；
- 有 `wl_data_device_manager` 与 primary-selection：它们是“有窗口客户端”的正常协议；
- **没有** `ext_data_control_manager_v1` / `zwlr_data_control_manager_v1`：
  arboard 的 Wayland 剪贴板在这台 GNOME 42 上必然失败，只能回退 X11；
- headless `wl_seat.capabilities(0)`：没有键盘/指针，所以 IME、DnD、交互 resize 未测。

### 4.5 构建与测试

- `cargo build --offline`：成功（本 worktree，`.target-wayland`）。
- `cargo test --offline --workspace`：**545 passed, 0 failed, 1 ignored**；
  `cargo test exit=0`。未因本 spike 改动代码。
- 本轮提交只有文档，无 `Cargo.toml`/`Cargo.lock` 变化，无新依赖。

---

## 5. 阻塞点（按优先级）

### B1. 文件拖放：winit 0.30.13 在 Wayland 上没有 DnD（硬阻断）

winit Wayland 后端没有 `wl_data_device` 的 DnD 实现，连 `DroppedFile` 事件都不会产生；
Tervo 的 X11 bridge 和 `drop_target_at` 都无从触发。要支持，只有两条路：
1. 升级到实现 Wayland DnD 的 winit（需确认上游版本/API，`unstable-winit-030` 会受影响）；
2. 自己维护一条 Wayland 连接，接 `wl_data_device`/`wl_data_offer`，并与 winit/Slint
   的 seat/serial 协调（新增 `wayland-client`/`smithay-client-toolkit` 依赖，复杂度高）。

**在解决前，原生 Wayland 的文件拖放应明确标记“不支持”，不能保留 hover 高亮假象。**

### B2. 全局快捷键：无协议，XWayland grab 是假阳性（硬阻断）

- 原生 Wayland 没有全局键盘 grab；`quake.rs` 的 X11 grab 在 Wayland 窗口聚焦时无效。
- 本机 GNOME 42 的 portal 1.14.4 没有 `GlobalShortcuts`，无法走 portal。
- 当前代码还会在 Wayland 下打印“已注册”，属于误导性成功。
- 需要：Wayland 下禁用 Quake 的 X11 grab + UI 明确提示；后续评估 portal
  GlobalShortcuts（新依赖/新权限）或合成器专用接口。

### B3. 剪贴板/PRIMARY：GNOME 42 上没有 data-control（硬阻断，XWayland 可绕过）

- arboard 3.7 的 Wayland 实现依赖 ext-/wlr-data-control；Mutter 42 不提供。
- 纯 Wayland（无 XWayland/DISPLAY）下 CLIPBOARD 与 PRIMARY 都不可用；
  OSC 52、copy-on-select、中键粘贴、右键粘贴全部受影响。
- XWayland 下回退到 X11 可用，但要求桌面启动 XWayland 且 `DISPLAY` 可达；
  这不是“原生支持”，需要文档明确。
- 新 GNOME/KDE 若支持 ext-data-control，可能纯 Wayland 可用；必须在目标发行版上
  用 `wayland-info`/`WAYLAND_DEBUG` 验证，不能按版本猜。

### B4. IME：路径完整但未验收，且依赖 text-input-v3

- winit 只绑 `zwp_text_input_manager_v3` v1；只实现 v1/v2 的合成器不可用。
- preedit 在应用内不可见（2px 透明 TextInput），候选窗完全依赖输入法/合成器；
  KDE 与 GNOME 的呈现方式可能不同。
- `update_ime_spot` 与 Slint 自身对 TextInput 的 `set_ime_cursor_area` 是两套来源，
  候选窗位置可能被后一次调用覆盖；需要真机验证。
- 需要真实 GNOME/KDE + fcitx5/ibus 的验收，不能凭 headless 结论上线。

### B5. 窗口定位/激活/Quake 几何

- Wayland 不允许客户端设置全局位置；`set_position` 为空操作，`outer_position` 报错。
- Quake 依赖 `screen_size`/`activate_own_window`（X11）；纯 Wayland 下屏幕尺寸未知、
  激活失败、位置不可控。GNOME 也没有 layer-shell（Quake 类下拉窗口通常需要
  wlr-layer-shell，GNOME 不提供）。
- 可行方向：用 winit monitor 信息计算尺寸，接受“合成器决定位置”；或把 Quake 改为
  普通窗口/全屏式；不要承诺精确 `x,0` 定位。

### B6. X11/XWayland 误判与副作用

- 只要 `DISPLAY` 存在，`xconn` 就能连 XWayland；`own_windows`、`screen_size`、
  `active_window`、`set_window_icon`、`quake` 都会看到 XWayland 的世界。
- 需要以 winit raw window handle 为准（项目已有 `winit_window_is_x11`，`main.rs:1560-1568`，
  但只用于 resize 和 drop），把所有 X11-only 能力统一门控。
- 同进程同时有 Wayland 窗口和 XWayland 连接是正常的；日志/诊断必须区分。

### B7. 低优先级

- 窗口图标/app_id：Wayland 需要 `.desktop` 和 app_id；`_NET_WM_ICON` 无效。
- Bell：无 X11 时只有 BEL 回退；不影响终端功能，但 UX 与 X11 不同。
- 多窗口：进程级多窗口可用，但位置/激活/会话恢复中的窗口几何不可靠。
- 真实 GPU 性能/挂起恢复/混合 DPI：未测。

---

## 6. 分阶段方案（建议）

### Phase 0（本 spike，已完成）

- 产出本报告；只读实验，不改默认路径、不加依赖。
- 结论：XWayland 可作为近期可用路径；原生 Wayland 暂不承诺剪贴板/DnD/quake。

### Phase 1：能力探测 + 优雅降级（低风险，1–2 天）

- 新增只读平台探测（不改架构）：记录实际 winit 后端（raw handle `Xlib/Xcb/Wayland`）、
  会话类型、`WAYLAND_DISPLAY`、`DISPLAY`、`SLINT_BACKEND`、`/dev/dri`。
- 把 `xconn` 调用按后端门控：Wayland 窗口下不再用 XWayland 的
  screen/pointer/active_window/icon 结果做决策；`--diagnose` 明确标注“XWayland 数据，
  非本进程窗口”。
- Wayland 下**不要注册** X11 Quake grab；设置页/日志明确“原生 Wayland 全局热键不支持”。
- 剪贴板不可用时，OSC 52 / 粘贴路径给出可读提示（当前只有 `[clipboard] 不可用`）。
- 验收：纯 Wayland 启动无 panic；无“F12 已注册”假日志；`--diagnose` 不再把 XWayland
  屏幕误当本窗口；X11/XWayland 行为不变。

### Phase 2：明确支持 XWayland（近期可交付）

- 把“Wayland 会话 + XWayland 剪贴板/PRIMARY”作为正式支持组合；文档写清前提：
  桌面启动了 XWayland 且 `DISPLAY` 可达。
- 回归：CLIPBOARD/PRIMARY/OSC 52 store/query、copy-on-select、中键粘贴、
  XDND 从 X11 文件管理器拖入、XIM 中文输入。
- 保留 native Wayland 窗口（winit 优先 Wayland），同时接受剪贴板走 XWayland；
  这是当前 GNOME 42 唯一现实的组合。
- 验收：真机 GNOME 42 X11/XWayland 下上述路径全 PASS；日志显示回退来源。

### Phase 3：原生 Wayland 关键能力（按协议可行性排序）

1. **剪贴板/PRIMARY**：优先确认新版 GNOME/KDE 的 ext-data-control；若支持，
   arboard 3.7 已能直接走 Wayland（无需新代码，仅需目标环境验证）。若目标环境不支持，
   评估 `smithay-clipboard` + `wl_data_device`（新增依赖，且要处理 seat/serial）；
   在此之前保持 XWayland 回退/明示不可用。
2. **IME**：在真机 GNOME/KDE 上做 fcitx5/ibus 验收；加 preedit/commit 日志
   （`TERVO_IME_LOG` 已有 spot 日志，可扩展只读事件日志）；确认候选窗位置与焦点归还；
   评估 `ImePurpose::Terminal`（winit 支持，但 Slint 1.18 固定 `Normal`，需要上游/补丁）。
3. **DnD**：等待 winit Wayland DnD 或单独接 `wl_data_device`；在实现前不保留
   “拖放已支持”的 UI 文案。
4. **全局快捷键/Quake**：评估 portal `GlobalShortcuts`（新依赖/权限），或
   合成器专用方案；不可用时明确禁用，不能假报注册。
5. **窗口定位/激活**：使用 winit monitor 信息；接受合成器位置；Quake 若必须精确定位，
   需要 layer-shell（GNOME 不支持）或改变交互形态。

### Phase 4：打包与回归

- `.desktop` + app_id；Wayland 下不要依赖 `_NET_WM_ICON`。
- CI 增加 headless Mutter 启动冒烟（本报告 §7 的命令即可）。
- 真机矩阵：GNOME 42/XWayland、GNOME 新版本 Wayland、KDE Plasma Wayland；
  NVIDIA/AMD/Intel；fcitx5/ibus；混合 DPI。

---

## 7. 最小验收命令

> 以下命令不需要真实 Wayland 桌面；用 headless Mutter 复现。
> 运行时把 `<worktree>` 替换为仓库路径，二进制可用 `.target-wayland/debug/tervo`。

### 7.1 起一个纯 Wayland compositor

```bash
cd <worktree>
mkdir -p .logs/wayland-rt && chmod 700 .logs/wayland-rt
XDG_RUNTIME_DIR="$PWD/.logs/wayland-rt" \
LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
dbus-run-session -- gnome-shell --headless --wayland --no-x11 \
  --virtual-monitor 1280x800
```

### 7.2 原生 Wayland 启动 + 诊断

```bash
# 另开一个 shell；确认 compositor 的 wayland-0 已出现
env -u DISPLAY XDG_RUNTIME_DIR="$PWD/.logs/wayland-rt" \
  WAYLAND_DISPLAY=wayland-0 XDG_SESSION_TYPE=wayland \
  TERVO_LOG=0 ./.target-wayland/debug/tervo --diagnose
```

预期：打印 `WAYLAND_DISPLAY=Some(...)`、`DISPLAY=None`；不出现 `screen=`/`pointer=`
（X11 查询不可用）；不因没有 X11 而崩溃。

### 7.3 剪贴板/PRIMARY 只读探针

用一个临时 shell 脚本（不提交）向 PTY 打印 OSC 52：

```sh
#!/bin/sh
B64=$(printf 'WAYLAND-CLIP' | base64 | tr -d '\n')
printf '\033]52;c;%s\007' "$B64"      # CLIPBOARD 写入
printf '\033]52;c;?\007'             # CLIPBOARD 查询
printf '\033]52;p;%s\007' "$B64"      # PRIMARY 写入
printf '\033]52;p;?\007'             # PRIMARY 查询
sleep 4
```

```bash
env -u DISPLAY XDG_RUNTIME_DIR="$PWD/.logs/wayland-rt" \
  WAYLAND_DISPLAY=wayland-0 XDG_SESSION_TYPE=wayland \
  SLINT_BACKEND=winit-software TERVO_IME_LOG=1 \
  ./.target-wayland/debug/tervo --title wl-probe --cols 60 --rows 8 \
  --shell "$PWD/.logs/osc52-probe.sh"
```

- Mutter 42 预期：`[clipboard] 复制 ...` 后出现 `[clipboard] 不可用`，query 0 字节；
  这是**当前已知限制**，不是回归。
- 支持 data-control 的合成器应看到 query/reply 有内容。

### 7.4 真 XWayland（可选，验证回退）

起 headless Mutter 时**不要加** `--no-x11`；确认出现 `/tmp/.X11-unix/X0` 后：

```bash
# 1) 原生 Wayland 窗口 + XWayland 剪贴板回退
env XDG_RUNTIME_DIR="$PWD/.logs/wayland-rt" \
  WAYLAND_DISPLAY=wayland-0 DISPLAY=:0 XDG_SESSION_TYPE=wayland \
  SLINT_BACKEND=winit-software ./.target-wayland/debug/tervo --title wl-xwl ... 
DISPLAY=:0 xsel -b -o    # 应读到探针 marker
DISPLAY=:0 xsel -p -o

# 2) 强制 winit 走 X11/XWayland
env -u WAYLAND_DISPLAY XDG_RUNTIME_DIR="$PWD/.logs/wayland-rt" \
  DISPLAY=:0 ./.target-wayland/debug/tervo --title x11-probe ...
```

- 1) 预期 `xwininfo -root -tree | grep wl-xwl` **找不到** X11 窗口（winit 仍 Wayland），
  但 xsel 能读到剪贴板（arboard 回退 X11）。
- 2) 预期 xwininfo 能找到 X11 窗口；`_NET_WM_ICON` 设置成功。

### 7.5 GPU/EGL 与全屏

```bash
# 纯 Wayland 下强制 Skia；无 /dev/dri 时 llvmpipe 也能验证 EGL 路径
env -u DISPLAY ... SLINT_BACKEND=winit-skia TERVO_FORCE_GPU=1 \
  ./.target-wayland/debug/tervo ...
# 预期日志：actual_renderer=skia-opengl，GL_RENDERER=llvmpipe...
# 注意：llvmpipe 不是真实 GPU 性能证据。

# 全屏协议路径
... WAYLAND_DEBUG=1 ./.target-wayland/debug/tervo --fullscreen ...
# 预期客户端发 xdg_toplevel.set_fullscreen，compositor 回 configure(1280,800)
```

### 7.6 构建/测试

```bash
CARGO_HOME=/home/user/AIworkspace/rusttabbyterminal/.cargo \
CARGO_TARGET_DIR="$PWD/.target-wayland" \
cargo test --offline --workspace
```

本轮结果：545 passed / 0 failed / 1 ignored。

---

## 8. 尚未验证（不要当成已支持）

- 真实 GNOME/KDE Wayland 会话（本机只有 headless Mutter）；
- 真实 NVIDIA/AMD/Intel GPU、Wayland 下 EGL/GL 选择、挂起恢复、混合 DPI；
- fcitx5/ibus 的 text-input-v3 端到端（preedit、候选窗位置、回车/退格归属）；
- 交互式拖边/角 resize（headless seat 无指针）；
- 原生 Wayland DnD（winit 未实现，无法测）；
- portal `GlobalShortcuts`（本机 portal 1.14.4 无此接口）；
- 窗口图标/app_id、桌面文件关联；
- XWayland 下从原生 Wayland 文件管理器拖入（XDND 桥接）。

---

## 9. 源码索引（本报告引用）

| 主题 | 位置 |
|---|---|
| Slint/arboard 依赖与 feature | `Cargo.toml:18`、`crates/app/Cargo.toml:29` |
| 环境日志 | `crates/app/src/logging.rs:67-79` |
| 后端选择/回退 | `crates/app/src/main.rs:7289-7305`、`7407-7422`、`7486-7502`、`9938-9954` |
| 窗口 resize 入口 | `crates/app/ui/main.slint:6920-6972`、`crates/app/src/main.rs:8090-8122` |
| resize 通道 | `crates/app/src/main.rs:1580-1645`、`9615-9665` |
| 全屏 | `crates/app/src/main.rs:9981-9994` |
| 隐藏 IME TextInput | `crates/app/ui/main.slint:2618-2670` |
| IME spot/提交 | `crates/app/src/main.rs:4603-4674`、`7748-7766` |
| 剪贴板/PRIMARY | `crates/app/src/main.rs:5335-5381`、`7691` |
| OSC 52 | `crates/app/src/main.rs:6694-6743` |
| DnD 桥接 | `crates/app/src/main.rs:5562-5585`、`5588-5643`；`crates/app/ui/main.slint:2871-2882` |
| Quake/全局热键 | `crates/app/src/quake.rs:1-58`、`crates/app/src/main.rs:9591-9596`、`1997-2010` |
| X11-only 辅助 | `crates/app/src/xconn.rs:209/269/322/392/434/441/461/471/517/556` |
| 多窗口 | `crates/app/src/window.rs:1-50` |
| 延迟/性能工具 | `crates/app/src/latency.rs`、`crates/app/src/bin/x*.rs` |
| winit 选平台 | `winit-0.30.13/src/platform_impl/linux/mod.rs:734-753` |
| winit Wayland 位置限制 | `winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:258-275` |
| winit Wayland resize | `winit-0.30.13/src/platform_impl/linux/wayland/window/state.rs:432-443` |
| winit Wayland text-input-v3 | `winit-0.30.13/src/platform_impl/linux/wayland/seat/text_input/mod.rs:28` |
| winit Wayland IME cursor | `winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:602-609` |
| winit X11 DnD | `winit-0.30.13/src/platform_impl/linux/x11/event_processor.rs:473-602` |
| Slint IME 转发 | `i-slint-backend-winit-1.18.1/winitwindowadapter.rs:1429-1446` |
| Slint IME 请求 | `i-slint-backend-winit-1.18.1/winitwindowadapter.rs:2114-2139` |
| Slint 过滤组合事件 | `i-slint-core-1.18.1/platform.rs:648-656` |
| arboard Wayland→X11 回退 | `arboard-3.7.0/src/platform/linux/mod.rs:131-150` |
| data-control 要求 | `wl-clipboard-rs-0.9.5/README.md:18-21` |
| Slint feature 链 | `i-slint-backend-selector-1.18.1/Cargo.toml:49-54`、`i-slint-backend-winit-1.18.1/Cargo.toml:88-99` |

---

## 10. 本轮明确没做

- 没有改默认架构/后端选择/输入路径；
- 没有升 winit/Slint、没有加 Wayland 依赖；
- 没有实现半套 IME、没有接 portal、没有做 DnD 兼容层；
- 没有合 master、没有改 P0/W-02/W-03/W-04 代码。
