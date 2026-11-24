# Wayland 原生适配（Phase 1）

> 分支：`feat/wayland-phase1`，基线 `6102bb9`（v0.5.0）。
> 前置调研：[`WAYLAND_SPIKE.md`](archive/WAYLAND_SPIKE.md)（GNOME 42 / headless 只读 spike）。
> 本轮目标：**能力探测 + 优雅降级**，不升级 winit/Slint、不新增系统依赖；
> X11/XWayland 行为必须与适配前一致。

---

## 1. 结论先行

在 Ubuntu 24.04.5 + GNOME Shell 46.0 / Mutter 46.2 + 真实 Wayland 会话上：

- **原生 Wayland 窗口可以正常启动、渲染、输入、全屏、resize、IME**；
- **剪贴板/PRIMARY 在原生 Wayland 仍不可用**：Mutter 46 不提供
  `ext_data_control_manager_v1`，也不提供 `zwlr_data_control_manager_v1`；
  `arboard 3.7` 只能回退 XWayland（`DISPLAY` 存在时），本轮实测 CLIPBOARD/PRIMARY
  写入 + 回读均成功；
- **文件拖放、全局热键、窗口定位、运行时窗口图标**仍不可用，已明确降级/拒绝，
  不再打印假成功日志；
- **X11 回归**：强制 X11 客户端路径的 Quake 注册、`_NET_WM_ICON`、剪贴板/
  PRIMARY、窗口/渲染均保持原行为。

一句话：**“Wayland 窗口 + XWayland 剪贴板”是当前唯一完整可用的组合；
纯 Wayland（无 XWayland）是可用终端，但剪贴板明确不可用。**

---

## 2. 平台探测设计

新增 `crates/app/src/platform.rs`，分两层判定，禁止用 `DISPLAY` 或
`xconn::connect().is_ok()` 判断“本进程是不是 X11”：

| 层 | 来源 | 使用场景 |
|---|---|---|
| **hint** | `WAYLAND_DISPLAY` / `WAYLAND_SOCKET` / `DISPLAY`，顺序与 winit 0.30.13 `EventLoop::new` 完全一致（空串按未设置） | `ui.show()` 之前：Quake 注册、圆角 worker、`--diagnose` |
| **confirmed** | winit raw window handle：`Xlib/Xcb` → X11，`Wayland` → Wayland | `ui.show()` 之后：图标、窗口扫描、最大化状态机 |

`Caps` 按后端给出能力，X11 全开，Wayland 全关：

```text
x11_window_ops / global_hotkey / window_icon / window_positioning /
xdnd_file_drop / x11_corners / x11_maximize_state
```

启动日志会同时打印 env hint 与 raw handle 确认：

```text
[platform] session=wayland desktop=ubuntu:GNOME wayland=wayland-0 x11_display=:0 xwayland=true ui_backend=wayland (env hint)
[platform] ui_backend=wayland (winit raw handle confirmed) ...
```

`--diagnose` 明确区分数据来源：

```text
ui_backend=wayland (env hint; 窗口未创建，按 winit env 规则推断) ...
xdg_app_id=xtervo
x11/xwayland view (DISPLAY=":0"; 不是本进程 Wayland 窗口，仅供参考):
screen=...   # XWayland 虚拟屏幕，不是 Wayland 窗口
x11/xwayland own windows (仅 X11 客户端，不含本进程 Wayland 窗口):
```

无 `DISPLAY` 时只打印 `x11: DISPLAY 未设置；跳过 X11/XWayland 探测`，不再输出
误导性的 screen/pointer/own windows。

---

## 3. 行为矩阵（本轮改动）

| 能力 | 原生 Wayland | X11 / XWayland | 实现 |
|---|---|---|---|
| 启动 / PTY / 软件渲染 | ✅ | ✅ | 不适配 |
| Skia/EGL、全屏、`xdg_toplevel.resize` | ✅（llvmpipe 可验证协议；真实 GPU 需本机驱动） | ✅ | 不改渲染路径 |
| 4K / 分数缩放 | GPU-native 按窗口 scale_factor 用物理像素渲染（字号/cell/基线一起缩放） | ✅（scale=1 不变） | CPU/Skia 兜底仍按逻辑分辨率，见下 |
| IME（text-input-v3 + fcitx5） | ✅ 实测提交“你好”；组合期按键 sink、`Ime::Enabled` 后重发 area、Palette/获焦恢复焦点 | ✅ XIM 原路径 | 保留隐藏 TextInput；不在 enable 前清 dirty |
| 剪贴板 / PRIMARY（应用内 OSC52/复制） | ⚠️ 仅 `DISPLAY` 存在时 arboard 回退 XWayland；纯 Wayland ❌ | ✅ | 失败时打印一次可读原因，短行日志保持 `[clipboard] 不可用` |
| 文件拖放 | ❌ winit 0.30.13 Wayland 后端无 DnD | ✅ XDND 原路径 | 不伪造 hover；启动只记录能力 |
| 全局热键 / Quake | ❌ 无协议；XWayland grab 对 Wayland 窗口是假阳性 | ✅ 被动 grab 原路径 | Wayland 不注册；设置里拒绝启用并 toast；菜单触发提示不支持 |
| 窗口图标 | ❌ 运行时 `_NET_WM_ICON` 无意义 | ✅ `_NET_WM_ICON` 64×64 | Wayland 跳过线程，改由 `.desktop` + app_id |
| app_id / WM_CLASS | ✅ `app_id=xtervo` | ✅ `WM_CLASS=("xtervo","xtervo")`（保持 winit 默认） | 仅 Wayland 在 show 前 `set_xdg_app_id("xtervo")`；X11 不设置，避免改 WM_CLASS |
| 窗口定位 / 激活 | ❌ `set_position`/`focus_window` 空实现 | ✅ | 不自称成功；Quake 不支持 |
| 最大化状态机 | ⚠️ 交给 winit/xdg_toplevel | ✅ `_NET_WM_STATE` 状态机 | Wayland 明确跳过，不再空转 2s |
| SHAPE 圆角 | ❌ 合成器绘制 | ✅ 原 worker | Wayland 不 spawn、不查询 |
| 拖边 resize 自绘回退 | ❌ 无 X11 全局指针 | ✅ 原逻辑 | Wayland 不建 xresize，只用 winit `drag_resize_window` |
| resize 收尾 | ⚠️ pointer-up 可能被合成器吃掉 | ✅ 真实指针查询 | 按方向把补发的 PointerReleased 落到原抓手；900ms idle watchdog 兜底；新建 Tab 前清理未结束状态 |
| Bell | ⚠️ stderr BEL 回退 | ✅ X11 Bell | Wayland 不连 XWayland 假成功 |
| 会话几何 | 只存尺寸，`position_known=false` | 存位置 | 避免 Wayland 的 `(0,0)` 污染下次 X11 恢复 |

---

## 4. 本轮实测（隔离 headless GNOME 46）

为了避免弹窗、抢焦点、覆盖真实剪贴板，全部交互测试在**独立 headless Mutter** 中完成：

- `gnome-shell --headless --wayland --virtual-monitor 1280x800`
- 私有 `HOME` / `XDG_RUNTIME_DIR` / `XDG_CONFIG_HOME` / `XDG_CACHE_HOME` / `XDG_DATA_HOME`
- 独立 `dbus-run-session`（私有 bus、私有 XWayland `:2/:3`）
- 输入用 `org.gnome.Mutter.RemoteDesktop` 注入（无需 root、不碰真实桌面）
- 截图用嵌套会话内的 `gnome-screenshot`（portal），输出 1280×800

证据目录：`.logs/wl/nested/`（本地，不提交）。

### 4.1 原生 Wayland

```text
[platform] xdg_app_id=xtervo
[platform] ... xwayland=false ui_backend=wayland (env hint)
[quake] 原生 Wayland 不支持全局热键 F12；Quake 仅能从命令面板/菜单触发...
READY TIMING ... cell=9x19
[platform] ui_backend=wayland (winit raw handle confirmed) ...
[maximize] 原生 Wayland：跳过 X11 最大化状态机，使用 winit
[ui] 原生 Wayland：跳过 X11 own_windows/遮挡检测（窗口由合成器管理）
[ui] 原生 Wayland 不设置 _NET_WM_ICON：图标由 .desktop + app_id=xtervo 决定
```

- `WAYLAND_DEBUG=1` 抓到 `xdg_toplevel.set_app_id("xtervo")`、`set_fullscreen(nil)`、
  `zwp_text_input_manager_v3` / `zwp_text_input_v3.enable/set_cursor_rectangle/commit`。
- IME：fcitx5 + pinyin，注入 `nihao Space` → `[ime] "你好"`、
  `[send] bytes=[228,189,160,229,165,189]`，截图
  `.logs/wl/nested/shots/06-nested-ime.png`。
- 合成器 tile（`Super+Left`）后窗口 640×800，内容重排，截图
  `.logs/wl/nested/shots/07-nested-tile.png`。
- 纯 Wayland 剪贴板失败路径（无 `DISPLAY`）：

```text
[clipboard] 不可用：原生 Wayland 剪贴板需要合成器提供 ext-/wlr-data-control
（当前 GNOME/Mutter 未提供），也没有 DISPLAY 可回退 XWayland
[clipboard] 不可用
[clipboard] 写入失败: clipboard unavailable
```

进程不崩溃，后续 query 仍返回 0 字节。

### 4.2 Wayland 窗口 + 私有 XWayland 剪贴板

xtervo 仍以原生 Wayland 窗口运行（`ui_backend=wayland`，`xwayland=true`），
arboard 回退到私有 `:3`：

```text
[clipboard] 复制 6 字节: "WLCLIP"
[clipboard] osc52 query ... reply bytes=6
[clipboard] PRIMARY 写入 5 字节
[clipboard] osc52 query ... reply bytes=5
```

在嵌套 `:3` 上 `xclip -selection clipboard -o` → `WLCLIP`，
`xclip -selection primary -o` → `WLPRI`。真实桌面剪贴板未被触碰。

### 4.3 X11 回归（隔离嵌套 `:3`）

```text
[platform] ... x11_display=:3 ui_backend=x11 (env hint)
[quake] 全局热键 F12 已注册
[platform] ui_backend=x11 (winit raw handle confirmed) ...
[maximize] runtime ready win=0x200003 ...
[icon] 写入窗口 0x200003，4098 个 CARDINAL
[ui] _NET_WM_ICON 已设置 (win=0x200003)
```

- `xwininfo`：`720x412+0+0`，`WM_CLASS ("xtervo", "xtervo")`（与适配前一致），`_NET_WM_ICON` 64×64；
- OSC52 CLIPBOARD/PRIMARY 在嵌套 `:3` 上写入/回读成功；
- 截图 `.logs/wl/nested/shots/05-x11-nested.png`。

> 嵌套 `:3` 没有 EWMH window manager（headless 会话未起 WM），因此
> `wmctrl`/最大化状态机/Quake 定位类行为只验证了“注册成功、日志正确、
> 图标/渲染正常”，完整 WM 行为仍由真实 X11 会话覆盖；本轮未改这些代码路径。

### 4.4 测试与内存

```text
cargo test --workspace --offline
→ 1069 passed, 0 failed, 0 ignored（含需要 /dev/ptmx 的终端测试）
```

2 分钟隔离 Wayland 采样（debug 二进制、单 pane、`/bin/cat`，期间做过 IME、
全屏/tile、截图和注入）：

```text
25 次采样（每 5s，t=0..120s）：
RSS     247756 KiB 恒定（+0 KiB）
threads 14 恒定
fds     27 恒定
进程无 panic/OOM；采样后仍存活
```

> 本机没有 Xvfb/openbox/`.perf`，`scripts/soak.sh` 的 X11 全量工作负载
> **ENV-SKIP**，不能伪 PASS；本轮改动只做启动期平台门控与诊断，不触及终端/
> 渲染循环，2m 采样作为内存门禁，15m/30m 留给发布候选。

---

## 5. 本轮明确不做

- 不升级 winit/Slint，不新增 `wayland-client`/`smithay` 依赖；
- 不实现原生 Wayland 剪贴板（`wl_data_device` 需要窗口/seat/serial 集成，
  且 GNOME 不提供 data-control）；
- 不实现 Wayland DnD（winit 0.30.13 无后端实现）；
- 不接 `org.freedesktop.portal.GlobalShortcuts`：GNOME 46 的 portal
  （xdg-desktop-portal 1.18.4）没有该接口，无法实现 Quake 全局热键；
- 不实现 layer-shell/窗口定位：GNOME 不提供 `wlr-layer-shell`；
- 不改 X11 的 EWMH/XDND/XIM/图标/圆角逻辑，只加门控。

## 6. 后续（Phase 2/3）

1. **原生剪贴板**：评估 `smithay-clipboard` / 自建 `wl_data_device`，需协调
   winit 的 seat/serial；否则继续把“Wayland + XWayland”作为正式支持组合。
2. **DnD**：等待 winit 上游实现，或单独接 `wl_data_device`；实现前 UI 不承诺。
3. **Quake 全局热键**：等 GNOME/KDE 提供 `GlobalShortcuts` portal 后再评估；
   当前 Wayland 明确禁用，不保留假开关。
4. **app_id/打包**：验证安装后的 `xtervo.desktop`（`Icon=xtervo`、
   `StartupWMClass=xtervo`）在 GNOME/KDE Overview 中正确显示图标与分组。
5. **真机矩阵**：GNOME 46 / KDE Plasma 6 / 不同 GPU + fcitx5/ibus；30m soak。

## 7. 证据与复现

本地证据（不提交）：

```text
.logs/wl/nested/gnome-shell.log          # headless Mutter / XWayland :2,:3
.logs/wl/nested/app.log                  # 原生 Wayland 启动
.logs/wl/nested/app-ime.log              # fcitx5 提交“你好”
.logs/wl/nested/app-wlclip.log           # Wayland 窗口 + XWayland 剪贴板
.logs/wl/nested/app-x11.log              # X11 客户端路径
.logs/wl/nested/soak-2m.txt              # 2m RSS/线程/FD
.logs/wl/nested/shots/                   # 嵌套截图
```

复现脚本：`scripts/e2e_wayland_phase1.sh`（headless Mutter 隔离，不弹宿主窗口）。
手工最小命令见脚本头部注释；核心环境：

```bash
export HOME=$PWD/.logs/wl/nested/home
export XDG_RUNTIME_DIR=$PWD/.logs/wl/nested/rt
export XDG_CONFIG_HOME=$PWD/.logs/wl/nested/config
export XDG_CACHE_HOME=$PWD/.logs/wl/nested/cache
gnome-shell --headless --wayland --virtual-monitor 1280x800
# 另一个 shell（同一 XDG_RUNTIME_DIR）
env -u DISPLAY WAYLAND_DISPLAY=wayland-0 target/debug/xtervo --diagnose
```

注意：`WAYLAND_DISPLAY`/`DISPLAY` 必须显式设置；headless 会话的 XWayland
`:2` 可能因 systemd X11 services 失败而不可用，测试脚本优先使用 `:3`
（Mutter 的 managed services display）。
