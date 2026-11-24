# 终端 UI 基础能力：滚动条 / 视觉铃 / 拖放

> 分支：`feat/ui-basics`（基于 master `65c2f11`）
> 范围：只补 pane 内的终端交互；**不改** GPU、窗口 resize、背景图、TabBar 布局。
> 验证环境：headless Xvfb `:115`（`1280x800x24`，仓库内 `.perf/xvfb`），
> 不占用 `:1` / 不启动 Xephyr / 不 pkill 其他进程。

## 1. 可见滚动条

### 数据路径（core，无阻塞）

此前 `PaneHandle` 只有 `scroll(lines)`，UI 无法知道当前回滚位置与总历史长度。
新增（`crates/core/src/pane.rs`）：

```rust
pub struct ScrollPosition {
    pub offset: usize,  // 0 = 底部（最新），history = 顶部（最旧）
    pub history: usize, // scrollback 行数（不含视口）
    pub rows: usize,    // 视口行数
}

impl PaneHandle {
    /// 只读 actor 发布的原子快照，绝不阻塞 UI / PTY。
    pub fn scroll_position(&self) -> ScrollPosition;
    /// 绝对位置滚动（滚动条点击/拖拽用），异步投递。
    pub fn scroll_to(&self, offset: usize);
}
```

- actor 把 `offset`/`history` 打包进一个 `AtomicU64`（一次 load 不会撕裂），
  在每轮 IO/ctrl 处理结束后刷新；`Ctrl::Pull` 在把帧交给 UI **之前**刷新，
  保证 UI 拿到帧时读到的滚动位置是同一时刻的。
- 覆盖输出（history 增长）、滚轮、搜索跳转、resize（history 可能收缩）、清屏。
- `scroll_to` 内部换算成 `Scroll::Delta`，由 alacritty 夹取到 `0..=history`。
- 纯几何计算放在 `ScrollPosition`：`thumb()`（归一化 top/height）、
  `offset_for_thumb_top()`、`offset_for_track_fraction()`，全部可在无 UI 环境单测。

### Slint overlay

`PaneView` 新增 `scrollbar-visible` / `scrollbar-thumb-y` / `scrollbar-thumb-h` /
`scrollbar-track-h`。Rust 在 `pane_view()` 里按 **track 像素高（= grid 高）** 和
`SCROLLBAR_MIN_THUMB_PX = 24` 一次算好，Slint 只负责画：

- track 高 = 终端网格像素高，透明背景；thumb 5px 宽、圆角、hover/按下变 accent。
- 只有 `history > 0` 时显示（`ScrollPosition::scrollable()`），即 scrollback 真的超出视口。
- 点击/拖动把 track 归一化位置传给 `pane-scrollbar(id, fraction)`，
  Rust 用 `offset_for_track_fraction()` 反推目标 offset，再 `scroll_to()`。
  语义是“thumb 中心跟指针”：点顶部跳到最旧，点底部回到最新。
- 右侧最外 5px 是窗口 resize 命中带，overlay 因此内缩 3px（9px 可点区域），
  不改变 pane 尺寸，也不挡终端文字布局。
- 输出/滚动/resize/铃闪烁后由 `pane_view()`（或 `refresh_pane_view()`）刷新模型。

### 验证

- core 单测：`scroll_position_pack_roundtrip`、`scroll_thumb_geometry_endpoints_and_min_size`、
  `scroll_track_fraction_maps_to_top_and_bottom`、`scroll_thumb_offset_roundtrip_is_monotonic`。
- core 集成：`wheel_scrollback_shows_history` 增加 offset/history/rows 断言与
  `scroll_to` 顶部/中间/越界/底部；新增 `scroll_position_tracks_resize_without_blocking`。
- GUI（Xvfb `:115`）：
  - `.logs/ui-basics/scrollbar_bottom.png`：300 行输出，thumb 在底部；
  - `.logs/ui-basics/scrollbar_top.png`：点击 track 上方 → `offset=264 / history=281`，
    画面顶部为 18–37 行，thumb 到顶；
  - `.logs/ui-basics/scrollbar_back_bottom.png`：点击底部回到最新；
  - `.logs/ui-basics/scrollbar_drag.png`：从轨道底部拖到上方 → `fraction 0.909→0.227`、
    `offset 17→223`，thumb 跟随；
  - `.logs/ui-basics/no_history.png`：无 scrollback 的 shell 不画滚动条
    （像素检查 0 个亮色 thumb）；
  - app log：`[ui] scrollbar pane=1 fraction=0.091 offset=264 history=281 rows=20`。

## 2. 视觉铃（visual bell）

- 配置 `config.toml`：`visual_bell = true`（默认；旧配置缺字段也回退 true）。
  设置 → Terminal 增加同名开关，写入配置；关闭时立即清掉进行中的闪烁并停 timer。
- `poll_events` 收到 `CoreEvent::Bell` 且该 pane 属于**活动 Tab** 时，
  置 `bell_until = now + 180ms`；30ms 重复 timer 到期清除，没有闪烁时自动 `stop()`。
  非活动 Tab 的行为不变：未读点 + `desktop_notifications` 开启时 `notify-send`。
- Slint 在 pane 上叠一个 `background: transparent` 的 2px accent 边框，
  `opacity` 160ms / `border-width` 140ms ease-out 动画；不覆盖终端内容、不拦截输入。
- 单帧 bell 到达不影响滚动位置；滚动条/选区/鼠标上报逻辑不受影响。
- reduced motion：本轮未做（按需求“可后续”），后续可从系统设置读 `gtk-enable-animations`
  或用 `visual_bell` 的时长配置降级。

### 验证

- 配置单测：`interactive_defaults`、`missing_fields_use_defaults` 均断言 `visual_bell` 默认 true。
- GUI（Xvfb `:115`，shell 每 400ms 发 BEL）：
  - `.logs/ui-basics/bell_flash.png`（从 30fps x11grab 帧中挑出的 accent 边框帧；
    检测像素 `#7aa2f7` 在 pane 顶边/左边）；
  - app log 记录多次 `bell=true`，帧序列检测到 20+ 帧带 accent 边框（含淡出）；
  - 反向验证：`visual_bell = false` 时同样每 400ms 发 BEL，1.5 秒抓帧中 accent 边框为 0 帧。

## 3. 拖放（文件 / 目录 → pane）

### Slint 1.18 支持情况（重要）

- **语言层支持**：Slint 1.18.1 有 `DropArea`（`can-drop` / `dropped` / `has-drag`）、
  `data-transfer` 类型与 `WinitWindowAccessor::on_winit_window_event`。本仓库的
  `main.slint` 已按官方 API 接入 DropArea，编译/运行通过。
- **但当前生产后端不投递外部拖放**：`i-slint-backend-winit 1.18.1` 没有把 winit 的
  `HoveredFile` / `DroppedFile` 转成 Slint `DragMove`/`Drop`/`DataTransfer`
  （只有 `i-slint-backend-testing` 能模拟 native drag）。因此仅靠 DropArea，
  从文件管理器拖文件到窗口在 winit 下**不会触发**。
- 结论：**不是 Slint 没有 DropArea，而是 winit 后端没有接 OS 拖放**。为避免假实现，
  Rust 侧增加了一个小的 X11 原生桥接：
  - `WinitWindowAccessor::on_winit_window_event` 消费 `HoveredFile` /
    `HoveredFileCancelled` / `DroppedFile`（winit 已完成 XDND 协商并给出本地路径）；
  - `HoveredFile` 用 X11 `QueryPointer` + 窗口 `inner_position()` 换算到 Slint 逻辑坐标，
    命中当前 Tab 的 pane，写入 `MainWindow.drop-hover-pane` 做 hover 高亮；
  - `DroppedFile` 重新按指针位置命中 pane（拿不到 XdndPosition 坐标，所以以落点为准），
    复用与 DropArea 相同的 `drop_files_into_pane()`，随后聚焦该 pane；
  - 非 X11（Wayland）返回 None，`DroppedFile` 回退到当前聚焦 pane；Wayland 的
    winit 版本本身不保证投递文件拖放，作为已知限制。
- 若未来升级到 winit 后端支持 native drop 的 Slint 版本，DropArea 路径可直接生效；
  两者同时生效时需要去重（当前 pinned 1.18.1 不会）。

### 插入规则

`drop_files_into_pane()`（`crates/app/src/main.rs`）：

- 每个路径用 shell 单引号转义：`'` → `'\''`，其余字符按字面保留
  （空格、`$`、`;`、反引号、`*`、`~`、双引号等都不展开）；空串 → `''`。
- 多个路径空格分隔，**末尾保留一个空格**，方便用户继续输入参数。
- 按终端当前模式决定是否包 `ESC[200~` / `ESC[201~`（bracketed paste）；
  未启用时退化为纯文本。**不做危险命令过滤**：bracketed paste 只插入文本，
  不会自动回车执行（这与需求一致）。
- winit 对多文件会逐个发 `DroppedFile`，因此每个路径各插一次；
  最终命令行仍是 `'a b' 'c' ` 形式。

### 验证

- 单测：`drop_paths_are_shell_escaped_literally`（空格/中文/单引号/`$HOME`/`;`/反引号/`*`/
  `~`/反斜杠/双引号/空列表）、`drop_payload_is_bracketed_with_trailing_space`
  （bracketed 包裹字节、末尾空格、无换行、非 bracketed 退化）。
- GUI（Xvfb `:115`）：`crates/app/src/bin/xdrop.rs` 是最小 XDND source，
  通过 `XdndEnter/Position/Drop` + `text/uri-list` 把真实本地路径交给 winit：
  - `.logs/ui-basics/drop_hover.png`：拖入时 pane 出现 accent hover 边框；
  - `.logs/ui-basics/drop_after.png`：bash prompt 中出现
    `'/tmp/xtervo-e2e-drop/file one.txt' '/tmp/xtervo-e2e-drop/dir with space' '/tmp/xtervo-e2e-drop/it'\''s.txt' `；
  - app log：`native drop pane=1`、`drop pane=1 files=1 bracketed=true bytes=43/45/42`。

## 4. 测试与回归

```bash
# 仅本轮相关（工作树内共享 target）
CARGO_HOME=/home/user/AIworkspace/rusttabbyterminal/.cargo \
CARGO_TARGET_DIR=$PWD/target cargo test --workspace
```

- 本轮 `cargo test --workspace`：**342 passed / 0 failed**（11 个 test binary）。
  其中 app 单测 **216**（基线 214，新增 2 个拖放测试；visual_bell 断言并入现有配置测试），
  core lib 43、smoke 16（新增 1）、mouse_report 16、vt_conformance 11、
  xwin/xkey/xclick/xdrop 各 10（共享 `xconn` 测试）。
- GUI 回归脚本（已提交）：[`scripts/e2e_terminal_ui.sh`](../scripts/e2e_terminal_ui.sh)
  ```bash
  cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xdrop
  ./scripts/e2e_terminal_ui.sh     # 截图/日志 → .logs/ui-basics/
  ```
  依次验证：滚动条点击/拖动/回底、视觉铃（30fps 抓帧 + accent 像素检测）、
  XDND 文件拖放（hover + 插入截图）、反向用例（`visual_bell=false` 无闪烁、
  无历史不画滚动条）。Xvfb/OpenBox 来自 `.perf/`，只操作 `:115`，
  脚本内只 kill 自己启动的 PID；不依赖 `:1`/Xephyr/pkill。

## 5. 已知限制

- 视觉铃未跟随系统 reduced motion（按需求可后续）；`visual_bell` 只有开关，无时长/颜色配置。
- 滚动条是 9px overlay，右侧最外 5px 仍是窗口 resize 命中带；极窄 pane 下 thumb 会占满 track。
- 原生文件拖放桥接目前只实现 X11；Wayland 下 winit 不保证 `DroppedFile`，
  DropArea 在 1.18.1 winit 后端也不接收外部 DataTransfer。
- 拖动跨多个 pane 时无法实时跟随 XdndPosition（winit 不暴露坐标），hover 高亮停在
  首次进入的 pane，最终落点会在 `DroppedFile` 时按指针重新命中；落点插入始终正确。
- 未改 GPU / resize / 背景图 / TabBar 布局；滚动条不会触发新的 layout/PTY resize。
