> **2026-11-19 审查提示**：本矩阵部分“限制/未实现”列已落后于代码（例如四向 TabBar、插件 Linkifier、录制导出、多窗口 session v2）。以 `docs/REVIEW_BACKLOG_2026-11-18.md` 的修复状态和当前源码/测试为准；下列已修正的冲突行随本轮更新。

# xtervo 功能矩阵（FEATURE_MATRIX）

> 当前主线快照/合并审计见 [`MAINLINE_STATUS.md`](MAINLINE_STATUS.md)（2026-11-12，master `502e490`）；本文件保留历史审计，不再单独代表当前发布状态。


> 审计日期：2026-11-05 ｜ 仓库：`/home/user/AIworkspace/xtervoterminal`（起始 HEAD `0ea6231`，复核到并发提交 `8327e20`） ｜ 审计方式：只读代码 + 既有文档 + 静态走查；**未执行 cargo build/test**（同仓另一 Agent 在构建）。
> 上游网络不可用（2026-11-05，GitHub/docs.tabby.sh DNS 解析到非公网地址），上游行为未能复核；见 [`TABBY_UPSTREAM_AUDIT.md`](TABBY_UPSTREAM_AUDIT.md)。
> 状态分布与闭环结论见 [`CLOSURE.md`](CLOSURE.md)；统计脚本 `python3 scripts/matrix_stats.py`。
> 2026-11-06 增补（`feat/term-menu`）：B6 查找大小写/正则、Read Only、Reset、Save Contents；B6 现有 18 条
> （implemented=7 / partial=10 / not_started=1），证据见 [`terminal-menu-find.md`](terminal-menu-find.md)。
> 2026-11-06 增补（`feat/search-ux`，P1）：查找匹配计数 `当前/总数`、可鼠标编辑的
> TextInput 搜索框（点击/拖选/Ctrl+A/C/V）、Enter/Shift+Enter 刷新计数；
> UI-TERM-010 升为 implemented，B6 计数改为 implemented=9 / partial=9 / not_started=1
> （本节只改该行，未重算文件顶部历史总量），证据见 [`terminal-menu-find.md`](terminal-menu-find.md)。
> 2026-11-06 增补（`feat/sixel`）：Sixel 内联图片（UI-TERM-020 / TERM-020），B/C 各 +1 条 partial；
> 证据见本文件对应行与 [`self-iteration.md`](archive/self-iteration.md)。
> 2026-11-06 增补（`feat/iip-image`）：iTerm2 IIP 内联图片（OSC 1337 File，UI-TERM-021），
> C1 增至 17 条（partial=11）；与 Sixel 独立开关、共用 overlay/滚动/resize 机制，不新增 DA1。
> 2026-11-06 增补（`feat/settings-ui`）：设置改为无 PTY 的 **Settings Tab**（可回退 legacy modal），
> 8 个分组图标分类、主题下拉、分段选择器/switch/stepper/slider、搜索跳卡片；Profiles 内嵌列表与
> Plugins 页使 UI-SET-003/008 从 not_started 升为 partial，B7 状态计数已同步。
> 2026-11-06 增补（`feat/p0-hardening`）：PTY 键盘/粘贴改 16 MiB 有界写 backlog + `pull` 100ms
> 硬超时跳帧；SFTP list/mkdir/rename/delete 改后台任务 + 10s 硬超时/kill；TERM-013、
> UI-SSH-003/004、SFTP-002/003 的证据/限制已更新（见 `self-iteration.md` 本轮记录）。
> 2026-11-06 增补（`feat/context-menu`）：UI-TERM-004 pane 右键菜单改为 14px/30px 分组菜单
> （Link/Clipboard/Pane/View/Session + 16px 线性图标 + 真实快捷键提示 + 窄窗 Flickable 滚动），
> 动作 ID/回调/hit area/outside-close 不变；证据见 [`self-iteration.md`](archive/self-iteration.md) 与
> `scripts/e2e_context_menu.sh`（Xvfb :135）。
> 2026-11-06 增补（`feat/url-paths`）：URL/OSC 8 hover 手型（按格去重、不拉帧不重绘终端）、
> 本地路径 Ctrl+click（`~`/相对路径解析 + 存在/权限检查 + 失败浮层）、远端 Profile 路径不本地打开；
> UI-TERM-011 / TERM-012 的证据与限制已更新，e2e 见 `scripts/e2e_url_paths.sh`（Xvfb :128）。
> 2026-11-06 增补（`feat/event-wake`，P1-07）：CoreEvent（标题/Bell/OSC52/通知）、插件活动、
> SFTP 结果改为容量 1 的 wake 事件唤醒，400ms timer 仅兜底/周期任务；UI-TAB-008、UI-SSH-005、
> SFTP-004、NOTIFY-003 的实现/证据已更新；e2e 见 `scripts/e2e_event_wake.sh`（Xvfb :138）。
> 2026-11-06 增补（`feat/render-damage`）：双缓冲轮换只同步上一帧脏行 + 本帧 damage，
> 单行输出/按键 `full=false rows=1`、`full_redraws` 不增；resize/主题/滚动历史/大范围
> damage 仍整屏；单色字形补单元格纵向裁剪（修复邻行 1px 残影）。证据见
> `docs/PERFORMANCE.md` §2.3、`scripts/e2e_render_damage.sh`（Xvfb :137）与 UI-TERM-014。

> 2026-11-06 增补（`feat/ssh-sftp`，W-02/P1-08）：SSH Profile 新增 ProxyJump/-R/-D/agent forwarding、
> known_hosts 应用内首次/变更预检、SSH 退出横幅+Reconnect；SFTP 队列/Retry/chmod/元数据 Cancel/
> 连接超时+keepalive；Vault Argon2 后台线程。UI-SSH-001/002/003/004/005/006/007/009、SSH-005/007/009/011、
> SFTP-002/003/004/007、SEC-001/003 的实现/证据/限制已更新；本轮未重算顶部状态总数。

## 0. 覆盖与状态口径

- 并发说明：审计期间另一 Agent 提交了 `b80e265`（config schema/迁移/备份、字体惰性加载等），本矩阵已按该提交复核 config 相关行；此后 `94baefb`/`99d7895` 加入行内链接识别与 OSC 8 透出、`8327e20` 加入命令注册表，均已复核对应行（TERM-012/UI-TERM-011/C9）。工作区其余未提交改动未纳入。
- 覆盖需求文档 B、C 两部分复选框与后续审计新增行：当前 237 条（B/UI 118 条、C/底层 119 条）；另附 7 条平台/引言说明（标 `extra`，不计入完成度）。W-01 后已把 UI-TAB-012、SESS-004 升为 implemented，新增 Profile 外观/session profile_id 证据。
- `implemented` 仅表示：用户可见路径存在、非 mock、错误路径基本处理、有既有测试或可重复人工验收记录、文档已更新。本轮**未复跑**任何测试，Result 列引用既有记录并注明。
- `partial`：已有可用子集，但存在明确缺口。`needs_verification`：上游行为/触发方式/协议本身未核实，或本仓库行为未经查证。`not_started`：无实现。`unsupported`：明确排除（平台/外部工具）。
- Upstream evidence 列缩写：`U-AUDIT` = 本轮网络不可用，上游未核实（不代表上游没有该功能）；`U-XTERM` = Tabby 默认 xterm.js 前端（转引 `docs/tabby-themes.md`）；`U-THEME` = 已核主题 API（`tabby-core/src/services/themes.service.ts`、`api/theme.ts`，转引）；`U-PLUGIN` = 已核 npm 插件结构（keyword `tabby-plugin`，转引）；`U-CONF` = 已核 `config.yaml`/`config.service.ts`（转引）；`U-REQ` = 仅需求文档描述。

## 1. 状态计数与完成度

| 范围 | 条目 | implemented | partial | in_progress | needs_verification | not_started | unsupported | 加权完成度 |
|---|---|---|---|---|---|---|---|---|
| B 部分（UI） | 118 | 32 | 60 | 0 | 5 | 20 | 1 | 52.5% |
| C 部分（底层） | 119 | 14 | 49 | 0 | 9 | 41 | 6 | 32.4% |
| B+C 合计 | 237 | 46 | 109 | 0 | 14 | 61 | 7 | 42.4% |

> 加权公式：`(implemented + 0.5×partial + 0.5×in_progress) / 条目总数`。`needs_verification / not_started / unsupported` 记 0。该数字是“功能条目覆盖度”，不是代码行数或工时完成度。2026-11-06 W-01（feat/profile-session）后，UI-TAB-012/SESS-004 升为 implemented，新增 Profile 外观/session profile_id/save-on-exit 证据；表中公式得到 42.4%。`scripts/matrix_stats.py` 的 weighted 口径把 needs_verification 记 0.25，当前输出 43.9%（237 条，appendix 7 不计）。

## 2. 审计发现：现有实现/文档与需求不符（至少 3 条）

1. **SSH/Serial 依赖“已锁定”不实**：`docs/tabby-parity.md` 写“依赖已锁定 russh / serialport”，但 `Cargo.toml`、`crates/*/Cargo.toml` 与 `Cargo.lock` 均无 `russh`、`serialport`、`tokio-serial`，也无相关模块。
2. **会话恢复被写成 ✅ 过度**：`docs/tabby-parity.md` 称“重启后原样拉起”；实际只有菜单 `Restart Session` 或手动 `--restore` 才恢复布局+`cwd`，正常启动**不会**恢复，也不恢复运行中进程/滚动历史。C8-004 应为 `partial`。
3. **Tab OSC 标题并非完整跟随**：`docs/tabby-parity.md` 标 ✅；`main.rs::poll_events` 只把**活动 Tab 的首个 pane**标题写回 `tab.title`，后台 Tab、非首 pane 不更新，且无前缀/后缀/过滤。B2-008 应为 `partial`。
4. ~~**全屏没有实现**~~ 已修（2026-11-05）：F11 与 `--fullscreen` 走 `Window::set_fullscreen`，实测 2560x1440 满屏，UI-WIN-006 升为 implemented。
5. ~~**Pane 焦点没有视觉状态**~~ 已修（2026-11-05）：`main.slint` 按 `focused` 画 1px accent 边框，UI-PANE-004 升为 implemented。
6. **鼠标 1002 拖动按钮编码错误**：`main.rs::on_pane_mouse_move` 在转发拖动时硬编码 `MouseButton::Left`；按住右键/中键拖动（1002）会被当成左键上报。鼠标上报文档只验证了左键/滚轮。
7. ~~**OSC 52 无响应**~~ 已修（2026-11-06）：core `osc.rs` 旁路解析 OSC 52（set/query/clear、c/p/s、1 MiB 上限、非法 base64 丢弃），`CoreEvent::Clipboard` 交 app 写 arboard/X11 PRIMARY；alacritty 内建 `osc52` 设为 Disabled 防重复。**颜色/尺寸查询**仍不完整：`ColorRequest` 已回写，`TextAreaSizeRequest` 仍走 `_ => {}`，颜色查询无测试。
8. ~~**CLI 与实际不符**~~ 已修（2026-11-05）：`-l` 与 `--latency` 等价，`--title` 通过 `window-title` 属性真正写入窗口标题（OPS-006）。
9. **稳定 ID 未全面落实**：需求 A3 要求 Tab/Pane/Session/Profile/Plugin 都用稳定 ID；`workspace::Tab` 只有 `Vec` 下标，`session::Snapshot` 无 ID，`PaneId` 是进程内自增（重启后重排）；Profile 已有稳定 ID（profile.rs），Plugin 尚无模型。
10. **Session 未独立于 Pane 生命周期**：需求 A3 要求 Session 独立；当前 `PaneHandle` 同时拥有 actor/PTY/child，关闭 Pane/Tab 即 Drop 并 kill 子进程，无法“仅关视图/保留后台 Session”。（2026-11-05 已补关闭确认，见 UI-WIN-008，仅缓解误关，未解决生命周期解耦）

## 3. B 部分：UI 页面、控件和用户操作

### B1. 主窗口与自绘标题栏（9 条）

状态：implemented=5，partial=4，not_started=0。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-WIN-001 | 无系统装饰窗口，不显示传统标题栏 | U-AUDIT / 需求 B1 | main.slint `no-frame: true`；main.rs `MainWindow::new` + `set_size`（show 后再同步一次）；xconn.rs `CornerShaper`/`apply_rounded_corners`（X11 SHAPE）**默认关闭**，`XTERVO_CORNERS=1` 才启用 | implemented | SHAPE 失败不再写入去重缓存，失败 invalidate + 250ms reconcile 重试；最小化不写新 mask、map 回来强制重算；INPUT 显式整窗；Xvfb+openbox 初始/最大化/恢复/minimize/60 帧 live-resize 探针一致，无 WM 下不崩；Wayland 无 SHAPE | 无装饰窗口可用；圆角为实验开关 | SHAPE 与部分 WM 的 maximize/minimize/live-resize 组合会留下过期 mask（露桌面、按钮点不到），故默认方角；SHAPE 1-bit 无抗锯齿；Wayland 不支持 |
| UI-WIN-002 | 顶部自绘 TitleBar/TabBar：图标、Tab、新建、窗口按钮 | U-AUDIT / 需求 B1 | main.slint TabBar Rectangle：品牌 Image、tabs、+、⚙、─/□/✕ | implemented | 人工：核对 5 类控件可见可点；.logs/t4.png | 已有记录（ui-shell §2/§7） | 按钮无 tooltip/accessible label；无系统菜单 |
| UI-WIN-003 | 标题栏空白区拖动；按钮/Tab/输入区不误触发 | U-AUDIT / 需求 B1 | main.slint `WindowMoveArea` + `title-hit`；按钮/Tab 为后置 TouchArea | implemented | 人工：拖空白区移动窗口；拖按钮/Tab 不移动 | 已有记录（ui-shell §2） | 无自动化；依赖 Slint 命中顺序 |
| UI-WIN-004 | 最大化/还原/最小化/关闭；双击标题栏 | U-AUDIT / 需求 B1 | main.rs `on_toggle_maximize`/`on_minimize_window`/`on_close_window`；main.slint 双击 title-hit | implemented | 人工：─/□/❐/✕ 与双击空白区；ui-shell §3 | 已有记录（ui-shell §3） | 关闭无确认；双击只在空白区，Tab 上双击无效 |
| UI-WIN-005 | 窗口尺寸变化、DPI/Scale Factor、主题切换 | U-AUDIT / 需求 B1 | main.rs `on_resized`/`refresh_views`；main.slint 8 个 ResizeHandle；slint `unstable-winit-030` + `WinitWindowAccessor` 调 `winit::Window::drag_resize_window`（winit 在自己 X11 连接内先 `XUngrabPointer` 再发 `_NET_WM_MOVERESIZE`；Wayland 转 `xdg_toplevel.resize`）；X11 且 winit 实际使用 X11 后端时预检 `_NET_SUPPORTED`（Wayland/XWayland 不误判），不支持/无 WM 回退自绘；WM watchdog 改为“是否见过 configure / 最近一次 configure”，不再用固定 150ms 误判；UI 布局每 configure 立即更新，缩小立即重绘、放大 60fps 合并；terminal grid/PTY/字符仍 16ms 合并；>1.5MP 窗口拖动时临时隐藏背景图并在 200ms 后恢复；默认 `winit-skia`（有 `/dev/dri`）/`winit-software`（无 DRM），1s 内无 GPU API 且无渲染帧自动 exec 回退；`render_probe` 记录实际 backend/GL renderer | implemented | 真机 NVIDIA RTX 4060（驱动 580.173.02，Vulkan DiscreteGpu）：actual_renderer=wgpu30，[gpu-native] adapter=\"NVIDIA GeForce RTX 4060\" backend=Vulkan；draw_p95=36.30µs、full_200x50 total_p50=1596.09µs；验证脚本 scripts/verify_gpu_host.sh --check → RESULT: PASS，日志 .logs/gpu-host-verify/{env,run,bench_render}.log | WM live resize 与自绘回退均可用；无静默回退；背景恢复、字符 1:1 | 真机 NVIDIA 流畅度未复测，llvmpipe 是 CPU 光栅；1s 内确认不到 GPU API 会重启为软件（有 1s 黑屏窗口）；超大窗口拖动期间背景图会短暂隐藏；Mutter/GNOME 与 Wayland 未实测；`unstable-winit-030` 为 Slint 不稳定 feature |
| UI-WIN-006 | 全屏切换（F11/快捷键/命令） | U-AUDIT / 需求 B1 | `handle_input` F11 → `toggle_fullscreen(st)`；命令面板 `view.toggle-fullscreen` 同一实现；`--fullscreen` 启动参数 | implemented | 真机：F11 → `[ui] fullscreen=true`（2560x1440），再按 → `false`（540x472），无 panic；`--fullscreen` 启动验证过 | 2026-11-05 实测通过 | 全屏切换依赖 WM；Wayland 下由合成器决定 |
| UI-WIN-007 | TabBar 位置可配置：上/下/左/右 | U-AUDIT（上游支持方向待核） | config `tabbar_position`（top/bottom）+ `tabbar-bottom` 属性；根布局改绝对定位；TabBar 菜单可切换并持久化 | implemented | tabbar_position 支持 top/bottom/left/right；左/右为竖排 200px（搜索/品牌固定、Tab 列纵向滚动、底部按钮固定），内容区/侧栏/设置/IME 锚点按四向重排；上游 Tabby 已核实四向存在；上/下与基线逐像素 0 差异；2m soak PASS；竖排暂不支持拖拽重排 | 四向 TabBar 可用且持久化；竖排暂不支持拖拽重排 | 上游支持方向已核实；左/右拖拽重排未实现 |
| UI-WIN-008 | 关闭窗口前对活动任务/连接确认；不误杀应保留 Session | U-AUDIT / 需求 B1 | `request_close`/`needs_close_confirm` + 确认弹窗；`confirm_close`（默认 true）；Tab/Pane 关闭支持 `close_action=background`（保留会话，见 UI-TAB-005） | implemented | 关窗确认列出具体会话名（每 Tab 一行、多窗格标注、>6 截断汇总）+ close_action 设置页分段控件与热更新同步；Xvfb 验证两行会话名 | 关窗确认 + 会话保留策略均可用；确认弹窗列出会话名 | 退出应用仍会结束后台会话；close_action 无设置页入口 |
| UI-WIN-009 | 多窗口：新建窗口、同进程独立 Tab/PTY、共享服务、session v2 | U-AUDIT / 需求 B1 | primary single-instance + WindowRegistry/WindowRuntime；second xtervo/--window 经 Unix control socket 转发；Ctrl+Shift+N 同进程新 MainWindow；每窗口独立 models/tabs/panes/focus，共享 Config/Theme/PluginHost/MCP/Vault/Session；session v2 聚合 geometry/maximized/tabs；插件/MCP 带 window_id | implemented | 560 app tests；双窗口 session/callbacks/single-instance/plugin window routing e2e 全 PASS；2m/15m 双窗口 soak 证据；kill -9 恢复多窗口 | 默认单 primary + 同进程窗口；--window 兼容；关闭单窗口不影响其它窗口；Tab 暂不能跨窗口移动 | 跨窗口拖 Tab、Wayland/硬件 GPU 多窗口、15m/30m 发布级 soak 仍待补 |

### B2. TabBar 操作（14 条）

状态：implemented=3，partial=11，not_started=0。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-TAB-001 | 新建 Tab：+、快捷键、命令面板、Profile Selector | U-AUDIT / 需求 B2 | main.rs `new_tab_with_default`（config `default_profile_id` 有效则用该 Profile，否则 `new_tab_inner`）；Ctrl+T、`+`、Command Palette `tab.new`、TabBar New Tab 共用 | implemented | 会话恢复对未记录 Profile 的 pane 回退 default_profile（ID 优先/名字解析，显式记录不变）；resolve_default_profile_index + 单测 | 三条入口均支持默认 Profile，失效时回退 shell | 启动首 Tab/会话恢复不走默认 Profile；无 per-Profile 快捷键 |
| UI-TAB-002 | 新建 Tab 下拉：Profile/连接类型、搜索、键盘选择 | U-AUDIT / 需求 B2 | TabBar “+” 新建 Tab（默认 Profile 有效时用它）；旁边独立 **Profile 图标（`>_`）** 打开菜单，只列已保存 Profile（Local/SSH/Serial/Telnet）+ Manage Profiles…；管理面板内有搜索框；点外部/Esc 关闭 | implemented | Profile 菜单搜索（大小写不敏感）+ ↑↓ 循环高亮 + Enter/Esc + 高度夹取与 Flickable 滚动；Xvfb :177 用 30 Profile 验证过滤与滚动后 Enter | + 与 Profile 选择器分离；搜索在管理面板内 | Profile 菜单本身无搜索/键盘上下选择；菜单过长时裁剪不滚动 |
| UI-TAB-003 | 单击 Tab 切换活动 Tab | U-AUDIT / 需求 B2 | main.slint tab-hit `tab-clicked`；main.rs `set_active_tab` | implemented | 人工：多 tab 单击切换；ui-shell §7 .logs/t4.png | 已有记录 | 仅左键；无中键关闭/双击重命名 |
| UI-TAB-004 | 拖动 Tab 调整顺序；拖出窗口按上游行为处理 | U-AUDIT（拖出行为待核） | main.slint 6px 阈值/`drag-active`/浮动 ghost/2px accent 插入线/8px 让位动画；Rust `drag_target_in_group`/`drag_gap_index` + `workspace::move_tab`（同组校验）；顺序随会话快照持久化 | implemented | 单测 `drag_target_maps_pointer_to_slot`/`drag_target_respects_pinned_groups`/`drag_gap_index_maps_insertion_boundary`/`move_tab_reorders_and_tracks_active`/`normal_reorder_never_crosses_pinned_group`；Xvfb :158 `scripts/e2e_tab_pin_drag.sh` PASS（ghost/插入线/让位/排序/跨组 blocked/Esc 取消） | 浮动 ghost 跟随鼠标；插入线只在合法槽位出现；后续 Tab 用 `animate` 平滑让位；Esc/窗口失焦复位；不重建 Tab/TouchArea 模型 | 拖出窗口未实现（上游语义亦未核实）；ghost 是简化 Tab 副本；pinned 跨组拖拽直接非法（不提供跨组自动改 pin） |
| UI-TAB-005 | 关闭 Tab；区分仅关视图/结束 Session/后台保留 | U-AUDIT / 需求 B2 | main.rs `close_action`（end/background）：`detach_runtime` 移入后台池（不 Drop、子进程保留）；**Background Sessions 面板**列标题并可点选重连（`refresh_background`/`reattach_background`），菜单与 `session.reattach` 也可 | implemented | 真机：关 Tab → `[session] detach pane=1`，pid 不变；面板 `background panel items=1`，点条目 → `reattach pane=1`，同一 pid 复用（截图 .logs/bgpanel.png） | 结束/后台两条路径 + 会话列表重连可用 | 退出应用会一并结束后台会话；列表只显示标题与 pane id，无 cwd/运行时长 |
| UI-TAB-006 | 关闭其他/指定 Tab；恢复最近关闭 Tab | U-AUDIT（上游菜单能力待核） | main.rs `close_other_tabs` / `close_tabs_left` / `close_tabs_right`；per-tab 菜单：Close、Close Others、Close Left、Close Right；`keep_only` 保留当前 | implemented | 新增 tab.close-others/left/right 命令与默认键 ctrl+alt+w/left/right；修复 Ctrl+Alt 被硬编码 Ctrl 分支吞掉的回归；Xvfb 三条快捷键实测 | 关闭左/右/其它已可用 | 仅右键菜单，无快捷键；关闭范围不做二次确认（沿用配置 close_action） |
| UI-TAB-007 | 双击 Tab 或快捷键重命名（以源码触发方式为准） | U-AUDIT（上游触发方式待核） | main.slint 重命名浮层（TextInput，Enter 提交/Esc 取消）；main.rs F2 与双击入口、`sanitize_tab_title` 清洗；关闭浮层后 `focus-terminal()` 归还键盘焦点 | implemented | Tab 增加仅内存 prev_title + tab.rename-undo（Ctrl+Shift+Z），重命名浮层清空标题再 Enter 也可恢复；Xvfb 实测 rename undo tab=1 | 双击入口、Enter 提交、Esc 取消可用；本轮修复提交后焦点丢失 | 无重命名撤销；F2 入口仍只编码未单独注入验证 |
| UI-TAB-008 | 标题跟随 Shell/SSH 远端标题；前缀/后缀/过滤 | U-AUDIT / 需求 B2 | pane.rs Listener Title→CoreEvent::Title；`EventSink` 事件 wake → main.rs `drain_core_events` 立即处理 + 400ms 兜底 + `rebuild_tabs` | implemented | config title_prefix/title_suffix/title_filter（filter 支持 from=>to）；顺序 filter→prefix→suffix，仅非 remote pane 应用；远端标题留给连接插件 | 事件有测试；事件唤醒后标题 <50ms；仅活动 tab 的首个 pane 标题写回 | 后台 tab/非首 pane 不更新；无 prefix/suffix/filter；无 SSH |
| UI-TAB-009 | Tab 显示活动/连接/后台活动/进度/完成/失败；非仅颜色 | U-AUDIT / 需求 B2 | main.slint TabView{title,active,progress-state,progress-ratio,progress-label,attention,completed}：Tab 底部进度条（错误红/暂停灰）、百分比/错误“!”/完成“✓”/未读“●”；窄 Tab（<64px）状态压缩为右侧 4px 小点，不挤占标题；切到该 Tab 清除 attention/完成 | implemented | PaneActivity/TabActivity 增加 background/failed；非活动 Tab 的标题/进度事件算后台活动，非零退出/信号置失败，切 Tab 清除；Slint 字形 ●/✕/▸/✓ + tooltip | 进度/完成/失败/未读活动已上 Tab；激活态仍有底色+accent 线+字重 | 无连接状态；不确定进度无动画；标记为字符/色点而非无障碍标签 |
| UI-TAB-010 | Tab 右键菜单（按上游实际项目） | U-AUDIT（上游菜单项待核） | 窗口级菜单（置顶/新建/设置/重启/SFTP/转发/Profiles/关窗）+ per-tab 菜单（Close/Close Others/Close Left/Right/Duplicate/Rename/Pin Tab/Unpin Tab）；**点击菜单外任意位置或 Esc 自动关闭** | implemented | 真机：右键 Tab 菜单出现；点内容区 → `[ui] menu dismissed` 且菜单消失（.logs/tab-ctx-open.png / tab-ctx-dismiss.png）；Xvfb :158 右键 Pin/Unpin 日志 `[ws] pin tab 1`/`[ws] unpin tab 0`，旧 action 0..5 未改 | per-tab 与窗口菜单可用且可关闭；Pin 是新增 action 6，标签按当前状态切换 | 菜单项仍未覆盖上游全部（无 Split/复制连接等）；上游完整项目待核 |
| UI-TAB-011 | Tab 键盘导航：上/下一个、序号跳转、最近 Tab | U-AUDIT / 需求 B2 | main.rs `handle_input` 拦截 Ctrl+Tab / Ctrl+Shift+Tab / Ctrl+1..9 → `switch_tab_relative`/`switch_tab_number`；`next_tab_index` 循环 | implemented | AppState.tab_lru（MRU，最小 pane id 为稳定 key，点击/快捷键/新建/恢复/关闭全覆盖）+ tab.last（Ctrl+Shift+L）；Xvfb 实测 tab.last → 1 | 循环切换与序号跳转已实现 | 无“最近使用 Tab”栈；Ctrl+1..9 端到端未单独注入验证 |
| UI-TAB-012 | 持久化 Tab 顺序、标题、Pane 布局与恢复信息 | U-AUDIT / 需求 B2 | session.rs Snapshot/TabSnapshot/Tree（Leaf 可选 profile_id；TabSnapshot 可选 pinned；均 additive+serde default）；main.rs `capture_snapshot`/`save_session_on_exit`/`restore_session`/`normalize_pinned`；`--restore` + config `restore_session_on_startup` | implemented | session.rs 旧快照无 `pinned` 读为 false 且不写 false 键；Xvfb :136 profile-session e2e；Xvfb :158 tab-pin-drag e2e：2 pinned + 2 unpinned 正常退出 → session.json pinned/order 断言 → 重启日志 `restore tab "T3" pinned=true index=0` 与截图 | 顺序/标题/树/ratio/焦点/active/窗口网格/cwd/Profile 关联/Tab pinned 可存；手动与启动自动恢复均可用，失败回退新 Tab | Tab 无独立稳定 ID（仍按下标/快照顺序）；不恢复运行中进程/滚动历史/屏幕内容；自动保存只在正常退出 |
| UI-TAB-013 | Tab 自动收缩、标题省略、close 显隐与溢出滚动；`+`/Profile 始终跟随 Tab | U-AUDIT / 用户交互打磨 | main.slint `tab-width-cap` 52–180px；Tab width/背景/close 120–140ms 过渡；Flickable 横向滚动（拖拽/滚轮）+ 2px 滚动条；活动 Tab 变化/窗口 resize 时自动 reveal，末个 Tab 连 `+`/Profile 一起带入视口；窄 Tab 用 10px 字体、状态点，close 仅在 ≥84px 或 hover 显示 | implemented | + 与 Profile 按钮固定 TabBar 右侧（tab-flick/tab-width-cap 同步收窄）；1080x736 21 Tab 与 540x736 20 Tab 下按钮可见可点 | Tab 收缩后标题仍可读（至少数个字符），20 Tab 不再全部只剩 “...”；`+`/Profile 随 Tab 移动，未改成右侧固定 | 20 Tab 必须横向滚动；活动 Tab 在中部时 `+`/Profile 可能在视口外（末个 Tab 自动带入）；滚动无吸附/惯性调优；首/末 Tab 滚动后可能半露 |
| UI-TAB-014 | Tab 搜索浮层：过滤、键盘选择、跳转、关闭与焦点归还 | U-AUDIT / 用户交互打磨 | 最左侧手绘放大镜按钮打开 340px 面板；TextInput 自动聚焦、实时过滤标题+pane 标题；↑↓ 循环、Enter 跳转、Esc/点外部关闭、空态 “No matching tabs”、结果列表内部滚动并自动露出选中项；Rust `run_command("tab.search")` + Ctrl+Shift+A（另加 ^A 控制字符回退）；关闭/跳转后 `focus-terminal()` | implemented | Xvfb :107：Ctrl+Shift+A → `[ui] tab search open items=20`；输入 `terminal 4` 过滤 → Enter `pick=3`；Down×12 选中滚入视口；Esc/点外部后终端继续收到 `echo focus_ok`/`outside_ok`；命令面板搜 “Search Tabs” 执行 `tab.search` 并打开同一浮层；空态/滚动截图；204 tests | 输入过滤、↑↓/Enter/Esc、点外部关闭、空态、焦点归还均可用；快捷键与命令面板两条入口都验证 | 只做子串匹配，无模糊排序/高亮；结果只显示 Tab 标题，不细分 pane；Ctrl+Shift+A 的 ^A 回退分支未单独注入（正常文本映射与命令面板已验证）；面板暂无搜索按钮/清空按钮 |
| UI-TAB-015 | Tab 级 Pin：固定左侧 pinned 区、角标、Pin/Unpin 菜单、排序不跨组 | U-AUDIT / 用户交互打磨 | workspace.rs `Tab.pinned`、pinned 前缀不变量、`set_tab_pinned`/`insert_tab`/`normalize_pinned`；main.slint `PinIcon` 角标 + per-tab 菜单 Pin/Unpin（action 6）+ pinned 分组分隔线；session.rs `TabSnapshot.pinned` additive | implemented | 单测 `pin_moves_to_last_pinned_and_unpin_to_first_unpinned`/`pin_keeps_active_tab_following_its_position`/`insert_tab_and_normalize_keep_pinned_prefix`；app 375 passed；Xvfb :158 e2e：Pin T2/T3、Unpin T2、Pin T4、重启恢复、pinned 角标截图 | pinned 固定在左侧；Pin 追加到最后 pinned 后，Unpin 回第一个 unpinned 前；关闭/重命名/复制仍按原下标工作；复制 pinned Tab 保留 pinned；会话恢复保留 pinned | 不提供多选 pin/快捷键/命令面板入口；拖拽跨组直接非法；未做 pinned 区折叠/独立滚动 |

### B3. Split Pane / 分屏操作（11 条）

状态：implemented=6，partial=1，needs_verification=1，not_started=3。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-PANE-001 | 当前 Tab 新建水平/垂直分屏 | U-AUDIT / 需求 B3 | workspace.rs `Node::split`；main.rs `split_pane_inner`；右键 Split Right/Down | implemented | workspace `split_row_divides_width_with_gap`/`split_col_divides_height`；人工 .logs/m2.png | 已有记录 | 入口仅终端右键菜单；无快捷键 |
| UI-PANE-002 | 任意层级嵌套分屏，不限于 2/4 | U-AUDIT / 需求 B3 | workspace.rs 递归 `Node` + `Tab::split`；`layout`/`dividers` 递归 | implemented | `nested_split_layout_is_contained`、`nested_dividers_have_paths`；3 pane 截图 | 已有记录（3 pane 嵌套） | 无深度上限；极深布局未压测 |
| UI-PANE-003 | 分隔线拖动调比例；合理最小尺寸 | U-AUDIT / 需求 B3 | workspace.rs `layout`/`set_ratio`（0.1 夹取）；main.slint 分隔线 TouchArea；main.rs `on_divider_dragged` | implemented | `set_ratio_updates_layout_and_clamps`；人工 .logs/split_after.png | 已有记录 | 最小 10%；无双击复位/键盘调比例 |
| UI-PANE-004 | 单击 Pane 聚焦；焦点有清晰视觉状态 | U-AUDIT / 需求 B3 | main.rs `focus_pane`；main.slint pane 边框按 `p.focused && root.panes.length > 1` 画 1px accent | implemented | 人工：`--demo-splits` 点击不同 pane，焦点边框跟随；单 pane 不画边框 | 2026-11-05 实测：点击切换后边框立即跟随（截图 .logs/act003-focus.xwd） | 边框宽度固定 1px；未做焦点 pane 标题条/高亮分隔线 |
| UI-PANE-005 | 快捷键在相邻 Pane 间切换焦点 | U-AUDIT / 需求 B3 | `focus_pane_dir`：按 pane 中心点的方向投影选最近（主距离 + 2×侧向距离）；Ctrl+Shift+方向键（F700–F703） | implemented | 真机 3 pane：Ctrl+Shift+Left/Down → `[ws] focus pane=1/3 dir=(-1,0)/(0,1)`；xkey 补方向键注入 | 方向键切焦点可用 | 仅四向最近，不做环绕；`pane.focus-next` 命令仍为顺序循环 |
| UI-PANE-006 | 关闭当前 Pane；最后一个 Pane 的产品定义 | U-AUDIT / 需求 B3 | main.rs `close_pane_inner`；workspace `Tab::remove`/`Node::remove`；右键 Close Pane | implemented | `remove_collapses_to_sibling`、`tab_focus_moves_after_removal`、幽灵分屏回归 | 已有记录（self-iteration 第 2 轮） | 最后一个 pane 直接删 tab/退应用；无空状态/确认 |
| UI-PANE-007 | Pane 放大/还原（若上游提供），不改 Session 状态 | U-AUDIT（上游行为待核） | 无实现 | implemented | zoomed 纯视图状态 + workspace::zoom_layout 折叠布局 + pane.toggle-zoom（Ctrl+Shift+M）+ **双击 pane 右上角拖拽手柄最大化/还原** + 最大化提示条；最大化时 pane 模型只剩一个，手柄仍保留以便再次双击还原；切焦点/切 Tab/关 pane/split 自动还原；1 条单测 + 隔离 X11 实测 dblclick on/off | 已核实：Ctrl+Shift+M 与双击手柄均可最大化/还原；最大化视图不改布局树/比例/快照 | 双击手柄需要多 pane 或已最大化；单 pane 双击忽略；无独立“最小化到标题”动画 |
| UI-PANE-008 | Pane 间移动/重排/重组；上游支持则复刻拖拽 | U-AUDIT（拖拽语义待核） | 无实现 | implemented | Node::swapped/Tab::swap_panes 只换叶子 id（Split 结构与 ratio 不变）+ 右上拖拽手柄/落点高亮/ghost + Ctrl+Alt+Z 撤销（pane.undo-swap）；3 条单测；Xvfb 未做像素级拖拽验收 | 未实现 | 无 pane 拖拽/交换/移入其他 tab |
| UI-PANE-009 | 每 Pane 独立尺寸、Session、滚动位置、输入焦点 | U-AUDIT / 需求 B3 | pane.rs 每 pane 独立 PaneHandle/actor/PTY/Term；main.rs PaneRuntime 各自 grid/rect/renderer；Tab.focused | implemented | `pty_prints_and_ui_pulls`、`selection_copy_returns_text`；3 pane 截图 .logs/nf.png；人工独立输入 | 已有记录 | 独立滚动位置无专项断言；焦点视觉弱 |
| UI-PANE-010 | 分屏布局保存为 Profile/Workspace 并恢复 | U-AUDIT / 需求 B3 | session.rs Tree/Split(ratio)；菜单 Restart Session；启动 `restore_session_on_startup`；分屏恢复复用 `restore_tab_snapshot` | implemented | 设置页工作区列表（名字/pane 数/创建时间/打开/重命名/删除二次确认/导出 JSON）+ 命令面板动态条目；6 条单测 + scripts/e2e_workspace_ui.sh Xvfb :145 PASS（含删除在用工作区后 Tab 存活） | 匿名会话快照可恢复 split/ratio/cwd，手动与自动启动均可 | 命名 Profile/Workspace、保存/管理/导出布局未做；快照只由 Restart/正常退出写入 |
| UI-PANE-011 | 多 Pane 广播输入：开关、范围显示、危险提示 | U-AUDIT / 需求 B3 | `broadcast` 状态 + 右键菜单 “Broadcast Input: ON/OFF”（action 8）；开启后键入发给当前 Tab 所有 pane；菜单标签显示状态 | implemented | broadcast.rs 命令位识别（rm -rf/mkfs/dd/fdisk/shutdown 等，echo 不误报）+ 红条预警 + 二次确认（Send to all/This pane only/Cancel）+ 切 Tab/剩 1 pane 自动关闭；6 条单测 | 广播输入可用且可切换 | 无“危险命令”拦截/确认；状态是全局的（切 Tab 不重置）；只剩 1 个 pane 时仅提示仍算开启 |

### B4. Sidebar、Toolbar 与连接入口（8 条）

状态：partial=2，not_started=6。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-NAV-001 | Sidebar 显示/隐藏；宽度调整（若上游支持） | U-AUDIT（上游宽度行为待核） | 无实现 | not_started | 无 | 未实现 | 无 Sidebar；上游是否支持宽度调整待核 |
| UI-NAV-002 | Sidebar/Toolbar 按钮、分组、顺序遵循上游；插件可扩展 | U-AUDIT / 需求 B4 | 无实现 | not_started | 无 | 未实现 | 无 Sidebar/Toolbar/插件入口 |
| UI-NAV-003 | Connection Manager 入口始终易访问 | U-AUDIT / 需求 B4 | 无实现 | not_started | 无 | 未实现 | 无连接管理入口 |
| UI-NAV-004 | Profile Selector：快速选 Shell/SSH/Serial/Telnet Profile | U-AUDIT / 需求 B4 | TabBar 独立 `>_` Profile 菜单（快速选 Profile 开新 Tab）+ Profiles 管理面板（搜索/星标默认/复制/编辑/删除）；Local/SSH/Telnet/Serial 四类可打开 | partial | 既有四类 Profile 真机打开记录；本轮搜索/默认/复制仅单测 + 编译，GUI 未启动 | 四类 Profile 可从菜单/面板打开 | Profile 菜单无搜索/键盘导航；启动首 Tab 不套用默认 Profile |
| UI-NAV-005 | Command Palette：模糊搜索命令/Profile/设置（按上游核验） | U-AUDIT（上游能力待核） | main.slint Palette 浮层 + main.rs `open_palette`/`refresh_palette`/`palette_accept`；`commands::Registry::search` 模糊搜索；TabBar 菜单与 Ctrl+Shift+P 入口 | partial | 真机：菜单 → `[ui] palette open items=10`；点第一条 → `[ui] run command tab.new` → `[ws] new tab: tabs=2` | 命令面板可用（30/30 命令已接线；本轮补 Xvfb 键盘执行验证） | Profile/设置项未接入搜索；无键盘输入端到端验证（沙箱键盘注入不可用） |
| UI-NAV-006 | Toolbar 插件按钮：图标、名称、提示、动作、禁用态 | U-AUDIT / 需求 B4 | 无实现 | not_started | 无 | 未实现 | 无插件系统/工具栏 |
| UI-NAV-007 | 状态/通知区：连接、活动任务、进度、完成提醒 | U-AUDIT / 需求 B4 | 无状态区；仅 CoreEvent::Bell/Exited 事件存在但 UI 未消费 | not_started | 无 | 未实现 | 无状态栏/通知中心；Bell/Exited 被 `poll_events` 忽略 |
| UI-NAV-008 | 所有图标 Tooltip/Accessible Label；禁用/加载/错误态清楚 | U-AUDIT / 需求 B4 | main.slint MiniBtn/窗口按钮只画 Text 字形，无 tooltip/accessible-label/disabled 态 | not_started | 人工：悬停按钮无提示；屏幕阅读器未测 | 未实现 | ⚙/─/□/❐/✕/+ 均无标签；无禁用/加载/错误态 |

### B5. Connection Manager / Profile 管理 UI（13 条）

状态：implemented=5，partial=4，needs_verification=1，not_started=3。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-PROF-001 | Profile 列表与 Profile Groups/分组展示 | U-AUDIT / 需求 B5 | profile.rs `ProfileStore`（profiles.toml 原子写、v1→v2 迁移、稳定 ID）；main.slint Profiles 面板列表（`[kind] name`、编辑/删除、搜索、复制、星标） | partial | profile.rs 单测 add/update/remove/roundtrip/ID 修复；GUI 未启动（共享显示器约束） | 列表与增删改可用；无分组 | 无 Profile Groups/分组、无拖拽排序；搜索/复制/默认见 002/004/005 |
| UI-PROF-002 | 搜索/过滤 Profile | U-AUDIT / 需求 B5 | profile.rs `filter_indices`：按 name/host/kind 大小写不敏感子串匹配，空查询返回全部；main.slint 搜索框 + ×/Esc 清空；main.rs `refresh_profiles` 维护“过滤行 → store 下标”映射 | implemented | 单测 `filter_matches_name_host_kind_case_insensitively`；GUI 未启动，手工步骤见 self-iteration 2026-11-05 | 输入即过滤；点击打开正确 Profile；清空恢复 | 无模糊/拼音搜索；Profile 选择器菜单未接搜索（仅管理面板内） |
| UI-PROF-003 | 创建、编辑、保存、取消、删除 Profile | U-AUDIT / 需求 B5 | Profile 表单（Local/SSH/Serial/Telnet 动态字段 + 共用 Appearance 区）+ `open_profile_form`/`edit_profile_form`/`save_profile_form`/`on_profile_delete`；保存先做 Vault/外观校验，再 `ProfileStore::add/update` + 原子落盘 | implemented | profile.rs 单测 add/update/remove/roundtrip/外观校验；Xvfb :136 e2e 打开 Edit 表单并回填 Font/History/Cursor/Blink/Theme（`02-profile-form.png`） | 创建/编辑/保存/取消/删除闭环；外观字段可回填/保存/清空继承 | 删除无二次确认；非法值在保存时提示，无逐字段实时校验 |
| UI-PROF-004 | 复制 Profile、拖动排序、移动到分组、分组增删改（按原版核验） | U-AUDIT（原版行为待核） | 复制：`ProfileStore::duplicate` + `duplicate_name`（`(copy N)` 去重）+ `on_profile_duplicate`（新 ID、落盘）；面板行内 Dup 按钮 | partial | 单测 `duplicate_assigns_new_id_and_dedupes_copy_names`、`duplicate_name_helper_handles_existing_copies`；GUI 未启动 | 复制 Profile 可用 | 拖动排序/移动到分组/分组增删改未做；上游分组操作待核 |
| UI-PROF-005 | 设置默认新 Tab Profile | U-AUDIT / 需求 B5 | config `default_profile_id: u64`（默认 0）；Profiles 面板行首星标 `on_profile_set_default` 写 config；`new_tab_with_default` 用于 + / Ctrl+T / 命令 `tab.new` / TabBar New Tab；ID 无效回退默认 shell，删除默认项回退 0 | implemented | 单测 `default_index_resolves_and_falls_back`、`default_profile_id_defaults_to_zero_and_survives_roundtrip`、`deleting_default_profile_falls_back_to_shell`；GUI 未启动 | 默认 Profile 设置与新 Tab 行为已接通 | 启动首 Tab/会话恢复不套用默认 Profile（避免启动即连 SSH）；无 GUI e2e |
| UI-PROF-006 | 点击 Profile 连接；当前/新 Tab 打开（依上游） | U-AUDIT（打开位置待核） | 面板点行 → `on_profile_open` → 过滤映射 → `open_profile_tab(index)` 在**新 Tab** 打开；Local/SSH/Telnet/Serial 命令构造沿用既有实现 | implemented | 既有四类 Profile 真机打开记录（见 UI-NAV-004）；本轮映射层静态走查 + GUI 未启动 | 点击 Profile 在新 Tab 打开可用 | 打开位置固定新 Tab（上游语义待核）；连接失败仅日志（见 007） |
| UI-PROF-007 | 连接失败显示原因、重试、编辑入口 | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | spawn 失败只 eprintln；无连接错误 UI/重试 |
| UI-PROF-008 | 连接中 Profile 进度/状态；支持取消连接 | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | 无连接状态机/取消 |
| UI-PROF-009 | Profile 可绑定快捷键 | U-AUDIT / 需求 B5 | 无实现 | implemented | 表单 Hotkey 行（打开/编辑/保存全通）+ `ProfileStore::add` 校验拒绝 shift+x；列表显示 (ctrl+alt+p) | 未实现 | 快捷键表只覆盖命令，无 per-Profile 绑定 |
| UI-PROF-010 | Profile 创建/编辑按连接类型动态显示字段 | U-AUDIT / 需求 B5 | main.slint 表单按 `form-kind` 显示 Local（Shell/Args/Cwd）、SSH（Host/Port/User/Auth/Key/Password/Extra/Forwards）、Serial、Telnet 字段；所有类型共用固定位置 Appearance 区（Font/History/Cursor/Blink/Theme），SSH password 展开时表单加高到 620px | implemented | GUI：Xvfb :136 local 表单截图（`02-profile-form.png`）；SSH 字段/密码区域静态走查与动态高度表达式 | 按类型动态显示字段可用；外观区不随类型位移 | SSH 字段未按 General/Ports/Advanced 分组（见 011）；小窗口下 SSH 表单可能超出底部 |
| UI-PROF-011 | SSH Profile 字段分组 General/Ports/Advanced/Ciphers/Login Scripts（以源码为准） | U-AUDIT（字段分组待核） | 无实现 | needs_verification | 待上游源码核实字段 | 未核实/未实现 | 当前 SSH 字段为平铺表单；Ciphers/Login Scripts 未做 |
| UI-PROF-012 | Serial Profile：端口/波特率/数据位/校验/停止/流控/模式/换行 | U-AUDIT / 需求 B5 | Profile kind=serial：serial_port/baud/data_bits/parity/stop_bits/flow_control/dtr/rts；默认 `serialport` 进程内后端，设置页有字段、端口枚举/Refresh、1.5s 热插拔刷新；`transport=socat` 或 native 打开失败回退 socat | partial | 单测：PTY 上 `SerialTransport` 收发、idle TimedOut 重试；Profile 校验/roundtrip；枚举/UI 静态 | 端口/波特率/5-8 数据位/校验/停止/流控/DTR/RTS 可用；端口列表热刷新 | 无 CTS/DSR/DCD 状态回读；模式/换行仍无 UI；socat 回退只支持 8N1 |
| UI-PROF-013 | Local Shell Profile：命令、参数、起始目录、环境变量 | U-AUDIT / 需求 B5 | profile.rs Profile 支持 shell/args/cwd/env + 可选 font_size/theme/cursor_shape/cursor_blink/scrollback；`create_pane` 以 Profile 优先（显式 cwd 覆盖 Profile cwd）；表单暴露 Shell/Args/Cwd 与共用外观区 | partial | 单测 roundtrip（shell/args/cwd/env/外观持久化、继承/迁移/校验）；Xvfb :136 e2e：打开 pretty Profile 后 `pwd=/home/user`，字号 20/cell 12x28、Latte 主题、underline/off 光标（`03-profile-applied.png`） | 命令/参数/起始目录/外观可用；env 可从 profiles.toml 手写 | 表单无 env/自定义标题字段；无逐字段实时校验；无独立 UI 级 env e2e |

### B6. Terminal View 交互（19 条）

状态：implemented=9，partial=9，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-TERM-001 | 点击终端获得输入焦点；切换不丢输入状态 | U-AUDIT / 需求 B6 | pane-hit down → `ime.focus()` + `pane-clicked`；焦点边框；mouse-reporting TUI 左键点击记录 IME 候选锚点（`ime_click_anchor`，Shift/右键/普通 shell 不锚定） | partial | 真机（XMODIFIERS=@im=none）：点击 pane 键入 `echo ACCEPT-OK` + Enter → 输出正确（.logs/acceptance-keyboard.png）；**Xvfb :122 真实 dsh-tui 0.14.0** `/btw test`：点击右侧 BtwComposer 后 `[mouse] down pane=1 cell=(37,94) ... reporting=true`、`[ime] spot pane=1 cell=37,94 win=846.0,846.0 phys=846,846 scale=1.000 anchor=true`（点击前硬件光标 win=54,802），右侧 composer 输入 `abcxyz` 不被 Shift 清空（.logs/ime-hardening/dsh-btw-after-click.png、dsh-after-shifts.png） | 点击聚焦 + 键盘输入 + 快捷键可用；dsh 侧栏候选点贴右 composer | 无 fcitx/ibus 时无法断言真实候选窗像素位置（只验证 spot 与 composer 行为）；Wayland IME 未验证 |
| UI-TERM-002 | 鼠标拖选；跨行、反向、滚动自动扩展 | U-AUDIT / 需求 B6 | pane.rs `SelectionStart/Extend`+`sel_anchor`；main.rs 拖选复制；**双击选行**：400ms 内同 pane 同格的第二次左键 → `Ctrl::SelectLine`（行尾空白不进选区，强制全量重绘高亮） | implemented | core `PaneHandle::select_word`（unicode-segmentation UAX#29，宽字符双列、零宽组合并基字符）；app 400ms 同格双击选词/三击选行；Xvfb 实测 double-click select word / triple-click select line=7 | 拖选/双击选行可用 | 双击选词、三击选行未做；选行按视口行（回滚到历史需先滚动） |
| UI-TERM-003 | 复制、粘贴、全选、清屏、查找、复位、保存、滚到顶/底 | U-AUDIT / 需求 B6 | arboard；Ctrl+Shift+C/V；`copy_current_selection`/`paste_into_focused`；Ctrl+F；菜单与命令含 Select All/Clear/Reset/Save Contents；滚动条 `scroll_to(0/history)` 可跳顶/底 | implemented | commands 注册 view.scroll-top / view.scroll-bottom（Ctrl+Shift+Home/End），复用 scroll_position/scroll_to 且滚动前清选区；Xvfb 实测 scroll-to-top offset=178 history=178 | 复制/粘贴/查找/全选/清屏/复位/保存可用；滚动条可跳顶/底 | 命令面板/快捷键仍未提供“滚到顶/底”；粘贴无危险命令检测 |
| UI-TERM-004 | 右键菜单：复制/粘贴/查找/清屏/保存/只读/复位/拆分等（按上游） | U-AUDIT（完整菜单待核） | main.slint 13 项（命中链接 15 项）按 Link/Clipboard/Pane/View/Session 分组：14px 标签 / 30px 行高、16px 手绘线性图标、Copy/Paste/Split/Close/Find 快捷键提示、280px 自适应宽、窄窗 Flickable 滚动；**右键在按下时打开并 accept**；菜单已开时右键在外区移动到新位置重新打开；鼠标上报模式下右键仍开 xtervo 菜单，Shift+右键才转发给 TUI；分隔线右键走 nearest_pane；动作 ID 0–14 与回调不变 | implemented | 菜单 Up/Down（跳分隔线/边界不循环/自动滚入视口）、Enter 执行、Esc 关闭，菜单打开时按键不进 PTY；默认菜单补 profile.open 与 zoom-in/out/reset | 菜单召唤稳定（含 TUI 鼠标模式）；分组/字号/图标/快捷键提示可用；只读/复位/保存/链接入口可用 | 鼠标上报下 Shift+右键转发依赖修饰键识别；无 Profile/缩放菜单项；无菜单内键盘导航；短窗口菜单需滚动；Copy 等无选区禁用态未实现 |
| UI-TERM-005 | 可配置右键粘贴（PuTTY 风格） | U-AUDIT / 需求 B6 | Tabby 式 `right_click`：menu（默认）/ paste / clipboard / off；paste 与 clipboard 短按 <250ms 执行、长按 ≥250ms 弹菜单；clipboard 有选区时复制并清除，否则粘贴；中键粘贴开关；旧 `right_click_paste` 自动折算为 paste | implemented | 单测 `effective_right_click` 迁移；真机：paste 短按 `paste bytes=11`、长按 `right click pane=1`；clipboard 双击选行后右键 `right-click clipboard copy 5 chars`；off 无菜单无粘贴；中键 `middle-click paste` | 四种模式与长按行为对齐 Tabby | Shift+右键转发是 xtervo 额外逃生口；无可视化 hotkey |
| UI-TERM-006 | 可配置 Copy-on-Select | U-AUDIT / 需求 B6 | config `copy_on_select` + 设置面板开关；`on_pane_mouse_up` 仅在开启时写剪贴板；**同时写 X11 PRIMARY**（`set_selection_clipboards`） | implemented | 真机：关闭后拖选 0 条 clipboard 日志，开启后 1 条；本轮 Xvfb :113 拖选后 `xsel -b/-p` 均为 `COPYONSELECTPRIMARY`；config 默认断言 | 2026-11-06 补 PRIMARY 验证 | 无每 profile 覆盖；设置项在 Terminal 分类（无搜索） |
| UI-TERM-007 | 多行粘贴确认/警告；记住用户选择 | U-AUDIT / 需求 B6 | main.rs `needs_paste_confirm` + main.slint 粘贴确认浮层（Cancel / Always Allow / Paste）；config `confirm_multiline_paste` | implemented | 单测 `paste_confirm_only_for_multiline_when_enabled`；真机：多行粘贴 → `paste confirm lines=3`；Always Allow 后写配置且下次直发 | 2026-11-05 实测通过 | 只按“是否多行”判断，未做危险命令检测；预览只取前 3 行 |
| UI-TERM-008 | Bracketed Paste 包裹并发送给支持的 Shell/TUI | U-AUDIT / 需求 B6 | main.rs `paste_payload(normalized, bracketed)` 统一组装；检查 `BRACKETED_PASTE` 模式位 | implemented | paste_payload 含 201/200~/201~ 逐字节用例 + 真实 PTY e2e（bash DECSET 2004，子进程收到包裹字节且正文 201 保留） | 包裹与规范化有单测 | 缺“应用真的收到 200/201”的端到端验收；未测内容含 201 |
| UI-TERM-009 | 滚动条/滚轮；alternate screen（vim/less）滚动正确 | U-AUDIT / 需求 B6 | main.rs `on_pane_scrolled`；pane.rs `Ctrl::Scroll`；**可见滚动条**：core `ScrollPosition` 原子快照 + `PaneHandle::scroll_position()/scroll_to()/scroll_to_bottom()`；Slint 右侧 overlay，click/drag → `pane-scrollbar`；`scrollbar_policy`（always/auto/never，默认 auto）：auto 仅 history>0，always 无历史时 thumb 占满轨道，never 隐藏但滚轮/键盘/选区不受影响，改动即时刷新所有 pane；alacritty alt screen | implemented | ALT_SCREEN 下滚轮折算 Up/Down（尊重 DECCKM，单次≤10 步），vim/less 可滚；滚动条 overlay 从 w-12 改 w-14 让开 resize 带 | 回滚/位置显示/点击跳转/拖拽可用；滚动条策略可配且即时生效；非阻塞查询不挡 PTY | alt screen 滚轮行为未专项验收；滚动条 9px overlay（右侧最外 5px 仍是窗口 resize 带）；always 无历史时不画禁用态轨道背景；无滚动位置百分比/快捷键跳顶底 |
| UI-TERM-010 | 查找面板：关键词、大小写/正则、上/下一个、匹配计数 | U-AUDIT（上游选项待核） | main.rs `open_search`/`run_search`/`close_search`/`search_edited`；pane.rs `search` + `count_search_matches`（alacritty `RegexSearch`/`RegexIter` 扫 scrollback+视口，`SearchHit.index/total`）；main.slint 搜索框为 TextInput，`init.focus()`，Enter/Shift+Enter、Esc、鼠标点击/拖选、Ctrl+A/C/V 原生编辑；`SearchOptions` 控制大小写/正则 | implemented | core `search_supports_case_and_regex_options`、`search_reports_match_index_and_total_across_scrollback`（1/3→2/3→3/3→1/3，反向 3/3）、`search_finds_and_scrolls`（1/2）；app `search_count_label_formats_index_total_and_empty_results`；workspace 476 passed；Xvfb :127 `scripts/e2e_search_ux.sh`：计数 2/3、3/3、0/0、invalid regex、鼠标插入/拖选/Ctrl+A/C/V、Esc 后 `echo FOCUS_OK` 到达 shell 且查询不泄漏 | 当前命中/总数可见；Enter/Shift+Enter 刷新；搜索框可鼠标编辑；非法 regex 不崩溃；匹配数来自 core，不扫 UI 帧 | 每次搜索全量扫 grid，超大 scrollback 会增加 UI 线程耗时；只高亮当前命中；搜索框内 IME 组合未专项测试；打开时点击终端仍会切走焦点 |
| UI-TERM-011 | 点击 URL/IP/文件路径执行动作；区分远端/本地路径 | U-AUDIT / 需求 B6 | core `Frame::hyperlink_at`（OSC 8 优先）+ `Frame::link_at`（URL/IPv4[:port]/路径，按显示列映射宽字符/组合字符；路径起点要求空白/开括号/引号，避免 `--x=/usr`、`PATH=…`、提示符误判）；app 无按键 hover 按格去重识别链接，Slint `mouse-cursor` + winit `set_cursor` 切手型，仅在悬停状态变化时更新模型（不拉帧/不重绘终端）；Ctrl+左键（非鼠标上报）打开：URL 走 http/https/file 白名单，本地路径先 `~`/相对路径解析（pane shell cwd）并检查存在/权限，再在后台 `xdg-open`（单参数、无 shell）；不存在/无权限不 spawn，只写日志 + 底部浮层；远端 Profile 的路径不本地打开；右键 Open/Copy Link 同时支持 URL/路径 | implemented | link 识别补全：引号内空格路径、~user、file:line:col、折行 URL 拼接（link_at_wrapped）；点击/hover 走 wrapped 并在打开前 strip 行号；含 link.rs/frame.rs 新增测试 | URL/OSC 8/本地路径可点击打开；hover 手型/离开恢复；失败有 UI/日志；TUI 鼠标上报/右键菜单行为未变 | 无插件 Linkifier；长 URL 折行不拼接；路径必须位于行首/空白/开括号后，带空格路径、`~user`、grep 式 `file:line` 不识别；远端判定按 pane 创建时的 Profile（split/duplicate 出来的本地 pane 按本地处理）；裸 IP 设计上不打开；TUI 鼠标上报模式仍把 Ctrl+click 转发给 TUI（hover 手型仅作提示） |
| UI-TERM-012 | 终端字体放大/缩小/重置；显示当前缩放 | U-AUDIT / 需求 B6 | 键盘 Ctrl+= / Ctrl+- / Ctrl+0 → `zoom_font`/`reset_font`；命令面板 zoom-in/out/reset 共用同一实现；字号持久化到 config | implemented | 屏内 zoom HUD（15px=100%，1.2s 后清空，连按用全局 token 防旧 timer），缩放/重置/设置页字号共用；广播红条时下移 | 快捷键/命令缩放可用且持久化；屏内 zoom HUD 已有 | 行高按字体自动重排 |
| UI-TERM-013 | 可配置终端上下文菜单与插件装饰器 | U-AUDIT / 需求 B6 | main.slint 固定 13 项菜单；无插件/装饰器接口 | implemented | 菜单改表驱动（DEFAULT_TERMINAL_MENU + config [[terminal_menu]] 整表替换 + 插件 [[commands]] menu=true 装饰点）；Xvfb :141 实测默认 13 项/链接 15 项/config 替换；截图 .logs/recmenu/ | 未实现 | 菜单项硬编码；无配置/插件扩展点 |
| UI-TERM-014 | 内容变化不导致光标跳动、选区丢失或明显闪烁 | U-AUDIT / 需求 B6 | main.rs 拉帧 + render.rs 增量重绘：双缓冲轮换只复制上一帧脏行、本帧只栅格化 damage 行；`full` = frame.full/force_full/damage>rows/2（首帧/resize/主题/滚动历史）；选区/搜索比较新旧 Span 补行；app 光标走独立 overlay 不碰帧；单色字形加单元格纵向裁剪防邻行渗漏；profile 打印 full/rows/copy/full_redraws | implemented | FrameBuffers 改 2–4 块自有像素池 + damage 行提交；失败测试先行（旧实现 24/24 帧整块 CoW → 新稳态 0，扩池 1 次），140 步伪随机对拍逐像素一致；headless 2m/15m soak（150 万帧）通过；真机 GPU 持有深度待复测 | 单行/按键/选区/搜索局部重绘；滚动/resize/主题整屏；增量结果与整屏参考逐像素一致；无残影 | 双 buffer 被 Slint 持有 >1 帧时 CoW 整块拷贝（正确但非 damage 成本）；Skia 仍整张纹理上传；滚动历史由 core 强制 full；Sixel 独立 overlay 不参与帧 damage；只验证 Xvfb/llvmpipe/软件后端，真机 GPU 未测 |
| UI-TERM-015 | 拖文件/目录到 pane，插入 shell 转义后的路径 | U-AUDIT / 需求 B6 | main.slint DropArea（官方 API）+ Rust `drop_paths_text`（单引号转义、末尾空格、按模式包 bracketed paste）；**winit 1.18.1 不投递外部 DataTransfer**，由 `on_winit_window_event` 的 HoveredFile/DroppedFile + X11 QueryPointer 原生桥接补齐（同一插入函数）；见 `docs/terminal-ui-basics.md` | implemented | 单测 `drop_paths_are_shell_escaped_literally`、`drop_payload_is_bracketed_with_trailing_space`；Xvfb :115 + `xdrop` XDND 注入：hover 高亮 `.logs/ui-basics/drop_hover.png`、插入 `.logs/ui-basics/drop_after.png`（含空格/单引号路径），日志 `native drop pane=1`、`bracketed=true` | X11 上真实 XDND 文件拖放可用；路径单引号转义、末尾空格、bracketed paste 正确 | Wayland 未接；跨 pane 拖动 hover 不实时跟随（落点按 DroppedFile 指针重新命中）；多文件逐条插入；不做危险命令过滤（bracketed paste 不执行） |
| UI-TERM-016 | Read Only：菜单/命令切换，键盘/粘贴不写 PTY，鼠标选择/滚动仍可用 | U-AUDIT / 需求 B6 | core `PaneHandle::set_read_only/read_only`（`Arc<AtomicBool>`）；`input`/`try_input` 提前返回，actor `write_input` 二次检查；只读时 `wants_mouse/wants_motion=false`、`mouse()` 丢弃，本地选区/滚动不受影响；app 层在按键编码/粘贴/拖放前再挡一次；菜单 `Read Only: ON/OFF`、pane 右上角 `READ ONLY` 角标、底部浮层、命令 `terminal.toggle-read-only` | implemented | core `read_only_blocks_input_and_restores`；Xvfb :119 菜单切换 → `[ui] read-only pane=1 on=true/false`，键入 logged `[readonly] ... 丢弃按键`、Ctrl+Shift+V logged `丢弃粘贴 bytes=7`，输入文件无 blocked/pasted；关闭后键盘与粘贴恢复；截图 `readonly_on.png` | 只读门禁非阻塞；键盘/粘贴/鼠标上报均不写 PTY；鼠标选择/滚动可用；状态可见 | 每 pane 状态，不持久化/不随 Tab 保存；只读时 TUI 鼠标模式点击改为本地选区（预期行为） |
| UI-TERM-017 | Reset / Reset and Clear：复位终端状态，不关闭 pane | U-AUDIT / 需求 B6 | core `Ctrl::Reset/ResetAndClear` + `PaneHandle::reset/reset_and_clear`；actor 调 alacritty `Handler::reset_state()`（RIS：模式/光标/字符集/滚动区/grid/清屏），清选区/搜索高亮、强制整帧、刷新 mode/cursor/scroll 快照；UI 菜单与命令 `terminal.reset`/`terminal.reset-and-clear`，底部浮层提示 | implemented | core `reset_clears_state_and_contents_include_scrollback`（屏幕空、history=0、搜索高亮清除，之后仍可输入）；Xvfb :119 命令面板 `[ui] reset pane=1 clear-scrollback=false/true`，截图 `reset_plain.png`/`reset_and_clear.png`，Reset 后 `again`、Reset and Clear 后 `alive` 均到达 PTY | 复位不关闭会话；菜单/命令路径一致；非阻塞投递 | alacritty 0.26 `reset_state` 同时清 scrollback，当前 Reset 与 Reset and Clear 可观察行为等价；保留两个 Ctrl 消息以便上游分化 |
| UI-TERM-018 | Save Contents：保存回滚+视口全部文本到明确路径 | U-AUDIT / 需求 B6 | core `Ctrl::Contents` + `PaneHandle::contents()`（`bounds_to_string` 覆盖 scrollback+视口，末尾补换行）；Slint 无原生文件对话框，新增显式路径弹窗（默认 `$HOME/xtervo-save-<epoch>.txt`，支持 `~`/相对路径，路径预览 + 写入错误红字）；菜单 `Save Contents…`、命令 `terminal.save-contents` | implemented | app 单测 `save_contents_resolves_paths_and_writes_files`（~、相对/绝对、空路径、无 HOME、父目录不存在）；Xvfb :119 菜单 action 14 → `[ui] save-contents open/ok path=... bytes=...`，`save_dialog.png`，保存文件含 `abc123`/`OUT:allowed`（scrollback） | 保存全部内容、显式路径、错误反馈可用；成功底部提示文件路径 | 非系统文件选择器；不做覆盖二次确认；同步写入（scrollback 量级） |
| UI-TERM-019 | 光标形状/闪烁/颜色 + 粗体亮色（GNOME Terminal 基础外观） | U-AUDIT / 需求 B6/B7 | core `CursorShape::from_name`（block/underline/ibeam，别名容错）；`Palette::resolve_fg` 实现 bold-is-bright（ANSI 0–7→亮色，默认前景不换色）与 bold-color-same-as-fg/bold-color；render.rs `draw_cursor` 支持形状/颜色/文字色/闪烁相位，并新增 `draw_without_cursor` + `cursor_overlay`（cell 小图，underline/beam 透明背景）；app 以 UI Timer 切 overlay visible，不重绘终端帧 | implemented | 241 app tests + core 70 lib/46 integration；Xvfb :117 截图与空闲 0 次 `[render]`；见 UI-SET-002 | 形状/闪烁/颜色/粗体亮色可用且闪烁不触发整帧重绘 | `system` 等同 on；应用 DECSCUSR 的 underline/beam/hollow 优先；frame=Block 时用配置形状（无法区分显式 block）；无 focus 自动暂停闪烁；仅 headless 软件/llvmpipe 验证 |

### B7. Settings 设置页（15 条）

状态：partial=12，not_started=3（2026-11-09：新增 UI-SET-015 Help 并升 partial）。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-SET-001 | Application：应用级行为、启动与默认行为 | U-AUDIT / 需求 B7 | **Settings Tab**（默认）：不创建 PTY、不进 `workspace::Tab`；TabBar 显示可关闭齿轮 Tab，内容区整页；`Esc` / Tab × / 页头 × / 点终端 Tab 关闭，关闭后 IME 焦点归还。左侧 3 组 9 分类（Appearance/Terminal/Window/Keys/Profiles/Plugins/Vault/Advanced/Help）+ 手绘线性图标；顶部搜索按 `SETTINGS_CATALOG` 索引，点击结果切分类并用 jump token 滚到/高亮卡片。`XTERVO_SETTINGS_LEGACY=1` 回退居中 modal（同一套控件）。 | partial | 263 app tests；Xvfb :124 `scripts/settings_ui_shots.sh` 覆盖 open/分类/下拉/搜索跳转/关闭/700×500/480×360/legacy；read-shell 焦点泄漏测试（设置内输入未写 PTY，关闭后可输入）；底部 TabBar 与 Yaru Light 截图走查 | 设置 Tab 导航/搜索/关闭/回退/小窗口均可用，且不污染 workspace/PTY | 非真正 `workspace::Tab`（刻意避免改 PTY/会话模型）；无字体族/行距；搜索到卡片而非单控件；无完整 Tab 键焦点遍历 |
| UI-SET-002 | Appearance：外观、窗口风格、图标/字体、背景效果、光标/粗体 | U-AUDIT / 需求 B7 | 卡片式 Appearance：Theme 下拉框（`theme-display` 显示 Auto 解析结果）+ Auto 开关；Background image 路径（合法预览、Enter/失焦提交、× 清除、非法红字）+ Dim/Opacity 滑杆；Cursor Shape/Blink 分段选择器 + Color/Text color 输入；Bold 开关 + Bold color。回调/配置字段与旧 modal 完全一致，全部 hot apply。 | partial | 263 app tests；Xvfb :124 截图 settings_open/dropdown；切 Yaru Light 即时换色；e2e 设置写入断言沿用旧回调 | 主题/背景/光标/粗体全部保留并改为下拉/分段/开关/滑杆 | 无窗口风格/图标/字体族；无每主题独立背景；背景固定 cover；颜色仅 #RGB/#RRGGBB |
| UI-SET-003 | Profiles & Connections：默认 Profile、列表、连接行为 | U-AUDIT / 需求 B7 | Settings → Profiles：内嵌 `profile-items` 列表（点击打开、★ 默认、Edit、× 删除、New Profile），并在设置页上方叠加现有 Profiles 全面板（搜索/Dup/Close）。点列表项开 Tab 时自动退出设置页。 | partial | 263 app tests；Xvfb :124 截图 settings_profiles；Manage 模态/关闭后 Esc 重聚焦设置页走查 | 设置内可查看/新建/编辑/删除/默认/打开 Profile；完整面板保留 | 内嵌列表无搜索/复制（仍走 Manage 全面板）；无分组/拖拽排序 |
| UI-SET-004 | Terminal：行为、字体、滚动、复制粘贴、鼠标等 | U-AUDIT / 需求 B7 | 卡片式 Terminal：字号/回滚 Stepper、Unlimited scrollback switch、Scrolling（output/keystroke switch + Scrollbar 分段）、Behavior（Copy/Confirm/Right click 分段/中键/通知）、Bell（Visual/Audible）、Images（Sixel/滚动）、Key encoding（Backspace/Delete 分段）。所有旧回调与 config 字段不变，搜索索引扩到 8 个 section。 | partial | `cargo test -p xtervo-app` **263 passed**；Xvfb :124 e2e 断言 Unlimited/Scrollbar Always/Scroll on output/Audible/Backspace/Delete 的 config 与 `[ui]` 日志；截图 settings_terminal_*；搜索 sixel → Images 跳转 | 旧 Terminal 设置全部保留且即时生效；Sixel Images 段纳入分组卡片与新 e2e 坐标 | 无字体族/行距/鼠标设置；Unlimited 只对新 pane；小窗口需滚动 |
| UI-SET-005 | Color Scheme：选择、预览、导入/导出 | U-THEME / 需求 B7 | Theme 改为下拉框（12 内置 + Auto），选中即时写 config 并重取所有 pane 调色板；themes.rs ThemeRegistry 6 格式导入；`--import-tabby` 生成 tabby 主题。 | partial | themes.rs 12 例；263 app tests；Xvfb :124 dropdown 截图 + Yaru Light 实时切换 | 选择/导入可用并即时应用 | 无独立预览/导出；selection/cursorAccent 等忽略；无文件监听热重载 |
| UI-SET-006 | Config Sync：原版或插件提供时的兼容策略 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无同步机制/策略 |
| UI-SET-007 | Hotkeys：列表、搜索、录入、修改、清除、冲突提示、多段键 | U-AUDIT / 需求 B7 | Settings → Keys：30 命令列表 + 生效 chord/CUSTOM/CONFLICT，Edit 录制（Ctrl/Shift/Alt，Esc 取消，Backspace 默认），单项/全部重置，冲突拒绝；全局设置搜索可命中 Hotkey bindings。 | partial | 263 app tests；Xvfb :124 Keys 页截图；旧 hotkey 单测全通过 | 列表/录制/持久化/冲突提示可用，并归入 Keys 分类 | 无多段键；录制无 Meta；固定行宽在小窗口可能压缩 chord 文本 |
| UI-SET-008 | Plugins：搜索、可用/已装、安装、卸载、启用、禁用、更新、详情、权限、目录 | U-AUDIT / 需求 B7 | Settings → Plugins：summary、Installed 列表（enabled/disabled、id、api、runtime、命令/事件、status/theme、running/restarting/disabled 原因、权限声明）、最近 8 条活动、插件目录；V1 外部进程插件发现/命令/状态/主题在 Rust 侧。 | partial | 341 app tests；Xvfb :142 `scripts/e2e_plugin_v1.sh` 截图 command_result/status/plugins；崩溃/超时/坏 JSON/禁用单测 | 已装列表/运行状态/错误原因/活动/目录/权限声明可查看 | 无安装/卸载/UI 启用切换/更新/搜索/详情页；改 enabled 仍需重启 |
| UI-SET-009 | SSH：Host Key、Agent 类型、X11 Display 等 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无 SSH/设置页 |
| UI-SET-010 | Vault：加密凭据容器、主密码、锁定/解锁、凭据管理 | U-AUDIT / 需求 B7 | Settings → Vault 卡片：状态/路径、Master 密码 masked 输入、Set/Change、Unlock、Lock、Clear（二次确认）、错误/提示；Profile 表单 auth=password 可保存/读取。 | partial | vault.rs 8 例 + askpass 11 例；263 app tests；Xvfb :124 Vault 页截图 | Vault 管理 UI 保留并适配卡片布局 | 无空闲自动锁定/弹窗；无凭据逐条列表；密码仍经 askpass 环境变量 |
| UI-SET-011 | Window：窗口行为、TabBar 位置、Quake Mode | U-AUDIT / 需求 B7 | Settings → Window：Tab Bar at bottom、Quake mode (F12) switch + 说明；TabBar 菜单保留置顶/位置。底部 TabBar 下设置页仍占满上方内容区。 | partial | 263 app tests；Xvfb :124 底部 TabBar 截图走查；Window 页截图 | Window/Quake 开关保留；TabBar 上/下布局都不盖内容 | 无 Quake 尺寸/热键可视化编辑 |
| UI-SET-012 | Config File：路径、打开文件/目录、诊断（不显示 secret） | U-CONF / 需求 B7 | Settings → Advanced：path 展示、Open Folder、Import Tabby、Export Profiles、config-status、secret 策略说明；另展示 Settings UI 当前模式与 legacy 开关提示。 | partial | 263 app tests；Xvfb :124 Advanced 页截图；Open Folder 仍调 xdg-open（失败只打日志） | 原 Config 功能全部迁入 Advanced 卡片 | 无配置诊断/语法检查；只有目录没有“打开文件”；无恢复备份入口 |
| UI-SET-013 | 插件可动态注册设置页/项；卸载后配置清理或保留策略 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无插件设置 API/生命周期策略 |
| UI-SET-014 | 保存时机明确；保存失败/无效值/迁移错误提示 | U-AUDIT / 需求 B7 | config.rs 即时保存 + 原子写 + schema 迁移/备份；错误仅 eprintln；数值 clamp | partial | config.rs 7 例；代码走查：字号 8–32、回滚 500–100000 夹取；写失败仅 stderr | 即时保存/迁移/备份可用；无 UI 提示 | 保存/迁移/解析失败不弹 UI；无“恢复备份”入口；无效值只夹取不提示 |
| UI-SET-015 | Help：快速开始/快捷键、config/log 路径、诊断复制、版本、已知限制 | U-AUDIT / 需求 B7 + 帮助页补充 | Settings SYSTEM 组最后新增 Help（分类 8，手绘 16px 圆形问号）；9 条快捷键行；config/log 路径 + Open Config/Logs Folder + Copy Diagnostics（arboard，成功/失败写 `help-status`）；版本/commit/构建时间/target 全部来自 build.rs compile-time 常量；IME/Wayland/插件沙箱/24h soak 已知限制 + `docs/` 路径；4 条搜索索引（help/帮助 各命中 4）。打开目录用 `xdg-open` 单参数 helper，缺 xdg-open 时可见错误不 panic。 | partial | `cargo check/test -p xtervo-app --bin xtervo`（383 passed / 1 ignored，基线 382）；`scripts/e2e_settings_help.sh`（Xvfb :147，en/zh 8+ 张截图、版本/commit、按钮状态、搜索 help/帮助、760×520 滚动、1400×900 铺满、无 xdg-open 错误路径） | Help 页随窗口缩放/窄窗滚动；旧 8 分类与搜索回调不变；不联网、不发遥测 | 24h soak 尚未执行（文案标注）；插件权限仍是声明而非沙箱；Wayland IME 未实现；复制失败只在有剪贴板服务时可自然触发 |

### B8. SSH / SFTP / Port Forward UI（9 条）

状态：partial=6，not_started=3。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-SSH-001 | SSH Profile 编辑窗口分组清晰、密码/密钥安全提示 | U-AUDIT / 需求 B8 | Profile SSH 分组：Host/Port/User/Auth/Key/Password/Extra/Local(R)/Remote(-R)/Dynamic(-D) forwards/ProxyJump(-J)/Agent forwarding(-A)；`auth=password` 显示 masked 密码 + Save to Vault；Vault+临时 askpass（force）自动认证；`auth=password` 强制 `PubkeyAuthentication=no -o PreferredAuthentications=password,keyboard-interactive -o KbdInteractiveAuthentication=yes`；askpass 识别本地化 password prompt 并排除 OTP/验证码；W-02 起连接前 known_hosts 预检（见 UI-SSH-002） | partial | 单测 ssh_command/新字段校验/旧 profiles.toml 兼容/密码认证/askpass 9 例（含真实 ssh -G、ssh-keygen -F）；**W-02** app 351 tests；Xvfb :139 表单截图显示新增字段回填 | agent/key/password 均可用；跳板/转发/agent forwarding 生成正确 OpenSSH 参数；密码 Profile 可经 Vault 自动登录 | askpass 密码经 ssh 子进程环境变量传递（/proc 可见）；首连/变更仍需终端确认；无原生 russh；`XTERVO_ASKPASS_DEBUG` 仅排查 |
| UI-SSH-002 | 首次主机密钥确认、变更告警、known_hosts 管理入口 | U-AUDIT / 需求 B8 | **W-02 应用内预检**：连接前 `HostKeyJob` 后台执行 `ssh -G`→`ssh-keygen -F`→`ssh-keyscan`（10s 超时/可取消）；Known 直连；Unknown 显示 SHA256 指纹；Changed 并列旧/新指纹 + WARNING；只有用户点 Trust/Update 才写第一个 user known_hosts（0600，先 ssh-keygen -R）；Unverifiable 回退终端人工确认；`StrictHostKeyChecking=no/off/accept-new` 由用户配置接管 | implemented | known_hosts 14 例：ssh -G 解析/HostKeyAlias/端口键/首次与变更分类/keyscan 超时/取消/trust 权限/假 keyscan；Xvfb :139 首次+变更弹窗截图；日志断言 Trust 写入、Cancel 不改文件 | 首次/变更均有应用内明确提示，无静默信任；Trust 后自动继续连接 | 无 applied known_hosts 条目查看/删除管理 UI；ProxyJump/keyscan 失败只能回退终端；动态 KnownHostsCommand 可能与预检不一致 |
| UI-SSH-003 | SFTP 面板/Tab：目录导航、上级、刷新、路径输入、列表、属性 | U-AUDIT / 需求 B8 | **SFTP 面板**（TabBar 菜单 → SFTP）：SSH Profile 选择、Remote 路径、List、条目点选；`sftp.rs` 走系统 `sftp -q -b -`；list 后台 `SftpOpJob` + 10s 硬超时/kill + **W-02 Cancel**（kill 进程组）；所有 sftp 参数带 ConnectTimeout=10/keepalive | partial | 既有 e2e + W-02 Xvfb :139 列表/元数据 Cancel 截图；`--sftp-probe` 真机（无 GUI） | 列目录/选择/元数据 Cancel 可用且 UI 不冻结 | 无“上级/刷新/属性列”；慢链路上大目录可能被 10s 截断；路径含空格等特殊字符未做 batch 转义 |
| UI-SSH-004 | SFTP 上传/下载/建目录/重命名/删除；危险确认 | U-AUDIT / 需求 B8 | 面板 Download/Upload/Queue/New Dir/Rename/Delete/Chmod/Retry/Clear queue；传输后台 `TransferJob`（progress/get -R/put -R）；mkdir/rename/rm/chmod 后台 `SftpOpJob` + 10s 硬超时；Delete 两次确认；**W-02** 队列 FIFO、失败保留 Retry、chmod 八进制校验 | partial | 单测 queue/chmod/batch/取消/失败 27 例；W-02 e2e 队列/取消/chmod/失败+Retry 截图；既有 paramiko probe | 上传/下载（含目录）/建目录/重命名/删除/chmod/队列/重试可用且不冻结 UI | 无覆盖确认/断点续传/递归删除；取消可能留半成品；SFTP 不注入 Vault 密码；路径特殊字符未转义 |
| UI-SSH-005 | 传输进度、速度、已传/总大小、取消、失败重试 | U-AUDIT / 需求 B8 | `sftp.rs` 后台任务 + 双读线程按 CR/LF 切行；解析 `path 42% 1.2MB 3.4MB/s`；UI 当前文件/进度条/不确定进度/Cancel；**W-02** 传输队列 + 失败提示 + Retry；WakeSignal 事件唤醒，400ms 兜底 | partial | 单测进度解析/状态机/取消/失败/唤醒/queue 等 27 例；W-02 e2e 队列进度/取消/失败/Retry 截图；既有 event-wake e2e | 开始/当前文件/完成/失败/取消/队列/重试可用；有进度行时显示百分比/速率 | OpenSSH progress 在非 tty 管道不输出，通常只能显示不确定进度；无总大小/ETA/自动重试/断点续传；取消可能留部分文件 |
| UI-SSH-006 | Port Forward 管理：查看/添加/启停/删除，显示监听与目标 | U-AUDIT / 需求 B8 | **W-02 + 本次**：当前 native SSH pane 显示 L/R/D 监听/目标/状态；L/D 可运行时启停，remote 显示服务器分配端口；新增/删除写回 Profile；系统 ssh/插件后端只编辑 Profile | implemented | ssh_forward 状态单测；local/remote in-process e2e；面板 Slint 编译 | 当前会话状态/启停与 Profile 规则管理可用，不伪造未实现能力 | 系统 ssh/插件后端无法查询状态；remote 不支持运行时取消 |
| UI-SSH-007 | Jump Host 配置入口与链路可视化/可读描述 | U-AUDIT / 需求 B8 | **W-02** Profile 表单 Jump 字段 → `ssh -J`（支持 OpenSSH 逗号多跳）；SFTP 复用 `-J`；预检遇 ProxyJump 标记 Unverifiable 并回退终端确认 | partial | profile 单测 `ssh_remote_dynamic_forwards_and_proxy_jump_become_args`；Xvfb :139 表单截图；SFTP 参数单测 | Jump Host 可配置、可连接（系统 ssh） | 无链路可视化/跳板机单独认证 UI；预检无法穿透跳板机 |
| UI-SSH-008 | Login Scripts 编辑器：命令/顺序/延时/等待提示符（按上游核验） | U-AUDIT（字段待核） | Profile SSH 表单多行编辑器；每行 `<命令>[; delay=ms][; wait=文本][; timeout=ms][; on_error=stop/continue]`；保存校验行号 | implemented | 解析单测；in-process server 命令顺序 e2e | 可编辑并随 native 连接按序发送到远端 shell | 单行分号格式；命令内字面 `;` 需转义 `\;`；无交互式确认脚本 |
| UI-SSH-009 | X11/Agent Forwarding 状态显示与失败提示 | U-AUDIT / 需求 B8 | **W-02** Profile 增加 `agent_forward` 开关 → `ssh -A`；表单可编辑；无 X11 forwarding | partial | profile argv 单测；Xvfb :139 表单截图 | Agent forwarding 可配置/生效（系统 ssh/agent） | 无 X11 forwarding；无转发状态/失败 UI；Pageant/Windows Agent 未做 |

### B9. Serial / Telnet UI（8 条）

状态：partial=2，needs_verification=3，not_started=2，unsupported=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-SER-001 | Serial Profile 列表与保存配置 | U-AUDIT / 需求 B9 | Profiles 面板列出 Serial Profile（`[serial] 名称`）；表单 Serial 分组（设备/波特率）；落盘 profiles.toml | partial | 真机：面板显示 `[serial] test-serial` 并点开成功 | 列表/保存/打开可用 | 无连接状态/自动重连；无数据位等参数 |
| UI-SER-002 | 串口参数编辑与设备列表刷新 | U-AUDIT / 需求 B9 | 无实现 | not_started | 无 | 未实现 | 无串口枚举/参数 UI |
| UI-SER-003 | 输入模式：Text、Readline、逐字节/Hex（按上游定义核验） | U-AUDIT（上游定义待核） | 无实现 | needs_verification | 待上游核实输入模式定义 | 未核实/未实现 | 无 Serial 输入模式 |
| UI-SER-004 | 输出模式：Text、Hexdump（按上游定义核验） | U-AUDIT（上游定义待核） | 无实现 | needs_verification | 待上游核实输出模式定义 | 未核实/未实现 | 无 Serial 输出模式 |
| UI-SER-005 | Newline 转换策略选择 | U-AUDIT（上游策略待核） | 无实现 | needs_verification | 待上游核实换行策略 | 未核实/未实现 | 无 Serial 换行转换 |
| UI-SER-006 | 自动重连开关、间隔、当前状态 | U-AUDIT / 需求 B9 | 无实现 | not_started | 无 | 未实现 | 无 Serial 连接/重连 |
| UI-SER-007 | Telnet Profile、主机/端口、连接/断开、状态 | U-AUDIT / 需求 B9 | Profile kind=telnet：host/port；`telnet_command()` 用系统 telnet 跑在 PTY；表单 Telnet 分组 | partial | 真机：本地 TCP 服务 → Tab 显示 `TELNET-OK` 与 `welcome$`（.logs/telnet-e2e.png） | Telnet 连接可用 | 无连接状态/断开按钮（关 Tab 即断开）；依赖系统 telnet；无 telnet 选项协商设置 |
| UI-SER-008 | Zmodem 确认、进度、取消、结果通知 | U-AUDIT（上游支持范围待核） | 无实现（本轮确认系统无 lrzsz/sz/rz，无法做 PTY 桥接验证） | unsupported | 环境探测：`which sz rz lrz lsz` 全空 | 明确不支持 | 需要内建 Zmodem 协议实现才能补；当前无计划 |

### B10. Quake Mode 与窗口操作（5 条）

状态：not_started=5。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-QUAKE-001 | 全局快捷键显示/隐藏终端窗口 | U-AUDIT / 需求 B10 | X11 被动 grab（`quake::spawn_listener`，config `quake_hotkey`，支持 `Ctrl+Shift+F12` 语法）；热键线程 → `invoke_from_event_loop` → `toggle_quake_inner`；设置 Window 页开关 + 命令 `view.toggle-quake` | implemented | 真机：F12 → `[quake] show 3584x720 at 768,0`；再按 → `[quake] hide (minimized)`，进程存活；再按恢复（X11） | 全局热键可显示/隐藏，状态机正确 | 仅 X11；Wayland 无全局热键；失焦自动隐藏未做 |
| UI-QUAKE-002 | 窗口按设置停靠屏幕边缘/指定位置（受 OS 限制） | U-AUDIT / 需求 B10 | `xconn::screen_size()` + `quake_width`/`quake_height`（默认 0.7/0.5）→ 顶部居中；`_NET_WM_STATE_ABOVE/STICKY` + `_NET_ACTIVE_WINDOW` 置顶聚焦 | implemented | 真机 5120x1440：Quake 3584x720 at 768,0，置顶后可正常右键/输入 | 顶部居中停靠可用 | 只有顶部居中；不记忆多显示器/指定 edge |
| UI-QUAKE-003 | 调整窗口高宽、自动隐藏/失焦隐藏（按上游核验） | U-AUDIT（行为待核） | 高宽按屏幕比例配置；隐藏用 `set_minimized(true)`（**不能用 `hide()`**：Slint 最后一个窗口隐藏会退出事件循环，进程直接结束）；F12 恢复 | partial | 真机：F12 show → hide（WM_STATE=Iconic）→ show，进程 PID 不变；隐藏原因写进日志 | 尺寸/手动显隐可用 | 失焦自动隐藏、动画未做；比例只读配置（无设置项） |
| UI-QUAKE-004 | Quake 下 Tab/Pane/Session 操作与普通窗口一致 | U-AUDIT / 需求 B10 | Quake 复用同一窗口/AppState，不切换模式；Tab、Pane、右键菜单、输入路径完全一致 | implemented | 真机：Quake 内右键 → `[trace] pane-down-right` + `[ui] right click pane=1`，菜单正常；hide/show 后再次右键同样有效 | Quake 内交互与普通窗口一致 | 无独立 Quake 会话/工作区 |
| UI-QUAKE-005 | 快捷键冲突提示；Wayland 全局快捷键能力检测与回退 | U-AUDIT / 需求 B10 | `grab_key().check()` 失败（BadAccess）→ 日志“已被其他程序占用”；无 DISPLAY/Wayland 时 `connect()` 失败 → 日志；应用其它功能不受影响 | partial | 真机：正常注册 `[quake] 全局热键 F12 已注册`；错误路径为日志提示（无对话框） | 冲突/能力不足有日志可查 | 无 UI 冲突提示/自动改键；Wayland 无原生实现 |

### B11. 插件管理 UI（7 条）

状态：partial=6，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-PLUG-001 | Available/Installed 插件列表 | U-AUDIT / 需求 B11 | `plugin.rs` 兼容两种清单：旧扁平 `plugins/*.toml`（只展示、不执行）+ v1 目录 `plugins/<id>/plugin.toml`（oneshot/process 均可执行）；设置 Plugins 页列出已安装清单（名称/版本/状态/API/runtime/命令数/事件数/status/theme/运行状态），统计 enabled/命令数/skipped，并逐条日志跳过原因 | partial | plugin 单测：旧清单加载/跳过/重复/未知字段、v1 加载/新字段/非法 entry/API/runtime/permissions/contributes/重名/禁用/命令注册/事件解析；341 app tests；Xvfb :142 Plugins 页截图 | 旧+新已安装清单与运行状态可用 | 无“可用/商店”列表；无安装/卸载/升级/UI 搜索 |
| UI-PLUG-002 | 插件名称、说明、版本、作者、兼容 API、权限要求 | U-AUDIT / 需求 B11 | 旧 `PluginManifest` 原样保留；`PackageManifest` 新增 id/runtime/permissions/contributes（status/theme/commands 交叉校验）与命令静态 args；设置页行显示名称/版本/状态/id/API/runtime/命令数/事件数/status/theme/运行状态/权限声明/说明 | partial | v1 字段校验单测（runtime/permissions/contributes/args/theme 路径）；`display_line` 断言；Xvfb :142 Plugins 页截图 | 名称/版本/兼容 API/状态/命令数/事件数/runtime/权限声明可展示 | 无 author 字段；权限仅声明不强制；主题/状态贡献只做最小展示 |
| UI-PLUG-003 | 搜索、安装、卸载、启用、禁用、升级、重启提示 | U-AUDIT / 需求 B11 | manifest `enabled`（默认 true）控制是否注册命令/执行事件/启动常驻进程；禁用插件仍展示但不出现在 Palette，`spawn_command`/`spawn_event` 拒绝、`start_process_plugins` 跳过；运行时崩溃超限显示 disabled 原因；修改后需重启 | partial | `disabled_package_is_not_registered_or_executed`、`disabled_process_plugin_is_not_started`、`crashing_process_plugin_restarts_then_disables`、`registers_namespaced_commands_and_skips_disabled` | 清单级启用/禁用 + 运行时禁用原因可用 | 无 UI 开关/搜索/安装/卸载/升级；enabled 变更需重启 |
| UI-PLUG-004 | 安装来源与签名/校验；安装前展示权限 | U-AUDIT / 需求 B11 | `[permissions]`（filesystem/network/process/clipboard/credentials）参与清单校验并在设置页行内展示“仅声明/未授予”；无安装来源校验/签名/权限授予流程 | partial | 权限字段校验/展示单测（`process_manifest_parses_permissions_contributions_and_theme`、`invalid_process_manifest_fields_are_skipped`） | 权限声明可查看 | 仅声明，无沙箱/授予/撤销/安装前确认；无安装器/签名 |
| UI-PLUG-005 | 插件崩溃/超时/API 不兼容诊断信息 | U-AUDIT / 需求 B11 | 常驻进程 supervisor 隔离：2s 握手、10s 命令超时 kill、崩溃自动重启上限 2、坏 JSON/超长行 5 次禁用、api 不匹配禁用；环境/权限/路径错误在加载期或状态里显示原因；活动列表 + 日志 + 设置页 disabled 原因；活动 push 后 `WakeSignal` 刷新 | partial | 崩溃/超时/坏 JSON/握手超时/api 不匹配/禁用进程单测；e2e `crash` 插件重启上限禁用并截图 | 崩溃/超时/API 检查/重启/禁用原因可诊断 | 无升级/迁移/手动重启/完整结构化日志面板；活动仅 8 条内存摘要；命令/握手超时固定（10s/2s） |
| UI-PLUG-006 | Open Plugins Directory | U-AUDIT / 需求 B11 | 设置页显示插件目录路径；未做打开按钮（可复制路径） | partial | 真机截图显示 `/tmp/pl` | 路径可见 | 无一键打开目录按钮 |
| UI-PLUG-007 | 插件扩展 Toolbar/命令面板/Tab 菜单/终端菜单/设置/Provider/Middleware | U-AUDIT / 需求 B11 | Command Provider（Palette/快捷键/静态 args/返回文本浮层）+ Status Provider（底部最小状态栏 text/tooltip/command，可点击）+ Theme Pack（声明式 TOML 进主题下拉）；`events` 三种生命周期事件；process runtime JSON Lines | partial | 命令注册/参数/结果/状态轮询/Theme Pack/状态栏点击 e2e；341 app tests | 命令/状态/生命周期事件/主题扩展点可用 | Toolbar/Tab 菜单/终端菜单/设置 schema/任意事件订阅/自定义 UI 等未实现 |

## 4. C 部分：底层功能点

### C1. Terminal Emulator 核心（17 条）

状态：implemented=5，partial=11，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-TERM-020 | Sixel 内联图片默认开启、可配置关闭、滚动/resize 锚定、不参与文本/选区 | U-AUDIT / 需求 C1 | core `sixel.rs` 流式 DCS 过滤 + Sixel→RGBA 解码（RGB/HLS/透明/重复/raster/25 MiB/16 MP 上限）；`PaneHandle::set_sixel/set_sixel_scrolling`；app `sync_inline_images` 把可见图片按 cell 锚点同步到 Slint `PaneImage` overlay（按 core id 缓存 RGBA，仅图片/锚点变化时重建）；设置页 Terminal → Images 两个开关与上限提示；关闭立即清图且 DA1 去参数 4 | implemented | Sixel ECH 擦图矩形从光标列向右算（原误算左侧）；ech_erase_range 单测 + 双图只擦相交者的真实 PTY 用例 | Sixel 默认开、设置页即时开关、DA1 宣告、滚动/resize/切 Tab 位置正确；图片不进选区；dsh-tui 可启动 | dsh 启动到 onboarding，未验证真实图片消息；TUI 局部 ECH 擦除不追踪；scrollback 淘汰最旧行时锚点可能偏移；Sixel 无部分透明；仅 Xvfb/llvmpipe |
| UI-TERM-021 | iTerm2 IIP 内联图片（OSC 1337 File）默认开启、可配置关闭、滚动/resize/切 Tab 锚定、不参与文本/选区 | U-AUDIT / 需求 C1 | core `iip.rs` 流式过滤 `ESC ] 1337;File=...:BASE64`（BEL/ST/C1 ST 终止、跨 chunk），严格 base64（20 MiB）+ PNG/JPEG 解码（`png` + 轻量 `zune-jpeg`，16 MP / 单边 16384），支持 `inline=1`/`name`/`width`/`height`（cell、`px`、`auto`）/`preserveAspectRatio`；actor 复用 Sixel 的 `ImageStore`/`InlineImage` overlay/滚动/resize 预算，`PaneHandle::set_iip` 只清 IIP 图片（Sixel/DA1 不变）；app 复用 `sync_inline_images` 与按 core id 的 RGBA 缓存；设置页 Terminal → Images 增加 IIP 开关与 20 MB/16 MP 提示 | implemented | IIP inline=0/缺省 → 仍吞进不出图，但发 IipIgnored → 状态栏 toast + [iip] 日志（5s 节流，说明不下载文件）；core 2 例 + app 1 例 + Xvfb 真实 OSC 1337 证据 | 两种格式与 `width/height` 三态可用；滚动/resize/切 Tab 锚点正确；关闭立即清图且不再解码；图片不进文本/选区；DA1 与 Sixel 图片不受 IIP 开关影响 | 不做动画/文件下载（`inline=0` 或缺少 `inline=1` 被忽略）；IIP 与 iTerm2 一致不占用光标行，客户端需自行换行留位；scrollback 淘汰最旧行时锚点可能偏移；大图解码在 actor 线程内完成（未做后台解码/内存峰值压测）；仅 Xvfb/llvmpipe 验证 |
| TERM-001 | VT220 基础兼容及 Tabby 实际使用的 xterm/ANSI 扩展；兼容测试集验证 | U-XTERM / 需求 C1 | alacritty_terminal 0.26 `Term` + `vte::ansi::Processor`；crates/core/tests/vt_conformance.rs 10 例 | implemented | 自研 VT 兼容套件 crates/core/tests/vt_compat_suite.rs（32 PASS / 0 FAIL / 9 XFAIL，XFAIL 均为上游 vte/alacritty 限制且写期望）+ scripts/vt_compat_suite.sh；同时修正 HT 语义（移动光标不打印控制字符） | 基础 SGR/光标/宽字符/备用屏有契约测试 | 未跑完整 VT220/xterm 套件；与 Tabby 实际序列集未差分 |
| TERM-002 | CSI/ESC/OSC/DCS 解析；不完整序列、分包、非法、超长序列 | U-XTERM / 需求 C1 | vte Processor 处理分包/不完整；pane.rs `feed` 按 32 KiB 读块喂入；无 DCS/OSC 业务分发 | implemented | 确定性 fuzz crates/core/tests/vt_fuzz_target.rs（自研 xorshift64* + 语料变异，无 cargo-fuzz）：不 panic / Frame 尺寸恒定 / 光标不越界 / 历史≤scrollback / live heap<32MiB；默认 3×600 轮 + 4×5000 轮 56MiB 输入通过；scripts/fuzz_vt.sh；DCS/OSC 仍无业务分发 | 常见序列可解析；异常输入未系统验证 | 无 fuzz 目标；DCS 只被解析器吞掉，无业务处理；超长序列未测 |
| TERM-003 | 光标移动、擦除、插入/删除字符行、滚屏、滚动区域、制表位、自动换行 | U-XTERM / 需求 C1 | alacritty_terminal 实现；`cursor_move_and_erase_line`、`autowrap_moves_to_next_line`、`clear_screen_and_home` | implemented | tests/vt_editing.rs 16 例：IL/DL、DECSTBM 区内滚动、SU/SD、ICH/DCH/ECH 边界、HTS/TBC/CHT/CBT、DECAWM、IND/RI/NEL；TermSim 直喂无 PTY | 光标/擦除/自动换行已验证 | 插入删除行、滚动区域、制表位未专项验收；依赖 alacritty 行为 |
| TERM-004 | 主/备用屏幕切换；vim/less/tmux 等全屏 TUI | U-XTERM / 需求 C1 | alacritty `TermMode::ALT_SCREEN`；`alternate_screen_mode_roundtrip`；人工 vim/htop | implemented | `alternate_screen_mode_roundtrip`；phase0 vim/htop 55–58 FPS 记录 | 已有记录 | tmux/less 未逐项验收；备用屏滚轮行为未专项测 |
| TERM-005 | 16 色、256 色、24-bit True Color；Base16 颜色基础设施 | U-XTERM / 需求 C1 | core palette.rs `Palette::resolve`（16/256/truecolor）；`sgr_basic_colors_resolve_to_palette`、`sgr_256_and_truecolor` | implemented | 两条 SGR 单测；256 色 196=#ff0000、truecolor 原样透传 | 已有记录 | Base16 无独立模型/校验，仅通过导入格式间接受支持 |
| TERM-006 | Unicode：UTF-8 分包、组合字符、双宽 CJK、emoji、VS、ZWJ grapheme | U-XTERM / 需求 C1 | vte/alacritty UTF-8；宽字符/组合/ZWJ 网格测试；glyph.rs 彩色 emoji（ttf-parser 读 CBDT/CBLC/sbix + png 解码 + RGBA 缓存，默认开启，`XTERVO_COLOR_EMOJI=0` 可关）与单色回退 | implemented | vendored alacritty 增加 zwj_anchor：U+200D 并入基准格、光标不前进；基准格仍 WIDE_CHAR+SPACER 保证选区/折行/链接坐标不变；更新旧断言（旧 6 格 → 新 2 格）并新增折行测试 | UTF-8/双宽/组合音标/彩色 emoji 可用；VS16 单格彩色；ZWJ 组件不越界 | 无 grapheme cluster 合并（ZWJ 家庭仍按码位 6 格）；COLR/SVG 彩色字形不支持；肤色/旗帜等序列未逐项验收 |
| TERM-007 | 字体 fallback、Powerline/Nerd Fonts、box drawing、连字 | U-XTERM / 需求 C1 | glyph.rs 主字体 JetBrains Mono + CJK 回退（WQY/Droid/Noto）；无 Nerd/连字逻辑 | implemented | glyph::discover_icon_fonts 扫描系统字体目录（nerd/powerline/symbols，深度3/≤32 候选）+ Powerline U+E0B0–E0B7 缺字时按 cell 几何 4×4 超采样合成进 atlas；cargo check 通过；本机未装 Nerd Font，仅编译与离线 coverage 几何验证 | CJK 回退可用；box drawing 依赖主字体 | 无 Powerline/Nerd Fonts/连字；fallback 仅 CJK 两类 |
| TERM-008 | 字体族、字号、字重、行距、字间距、渲染缩放 | U-XTERM / 需求 C1 | glyph.rs `FontSet::load(size)` 固定候选字体与 cell 尺寸；设置仅 font_size | implemented | config 5 字段（font_family/font_weight/line_height/letter_spacing/render_scale）+ FontSet::load_with(FontOptions) + 设置页控件即时生效 + 热重载失败整体回滚；含 cell 尺寸/族名解析测试 | 字号可调 | 字体族/字重/行距/字间距/缩放不可配；固定物理 cell 尺寸 |
| TERM-009 | Scrollback buffer 容量可配；默认有界内存 | U-XTERM / 需求 C1 | pane.rs `TermConfig{scrolling_history}`；config.rs `scrollback` + `scrollback_unlimited`：`Config::effective_scrollback()` 在 unlimited 时使用 100,000 行安全上限；pane.rs `scroll_on_output` 在 actor feed 后回底/运行时 `Ctrl::SetScrollOnOutput` | implemented | 回滚单测 + 设置面板；phase0 记录 2000 行≈3.8 MB 预分配；`terminal_behavior_config_parsing_and_roundtrip`、smoke `scroll_on_output_false/true`；Xvfb :118 scrollbar/设置页 e2e | 固定容量与“无限”（100k 安全上限）都可配；100k 上限明确标注 ≈192 MB/新 pane | 只对新 pane 生效；100k 未做长时间压测；不提供 per-pane 独立容量 UI |
| TERM-010 | Selection、剪贴板、鼠标报告、焦点报告、Bracketed Paste | U-XTERM / 需求 C1 | pane.rs selection/clipboard；mouse.rs 1000/1002/1003/1005/1006；Bracketed Paste 模式位与粘贴包裹；**OSC 52** 由 core `osc.rs` 解析（set/query/clear、`c`/`p`/`s` 选择区、1 MiB 上限），app 写 arboard 剪贴板与 `SetExtLinux` PRIMARY；copy-on-select 双写，中键优先 PRIMARY、空则回退剪贴板 | implemented | mouse_report.rs 追加 6 例：1002 右键=2/34/2、1003 悬停=35、真 PTY 1002+1006 与 1003+1006 逐字节；app 侧 bracketed paste 200~/201~ 端到端（含内容含 201） | 选区/剪贴板/PRIMARY/鼠标/括号粘贴主体可用；OSC52 双向可用 | 1004 已另补（本行未重验）；粘贴包裹无端到端验收；1002 右键拖动被编码成左键（旧记录）；OSC52 多 selection 组合（如 `cp`）未支持 |
| TERM-011 | OSC 终端标题、颜色查询/设置等上游实际使用扩展 | U-XTERM / 需求 C1 | Listener 处理 Title/ResetTitle/PtyWrite/ColorRequest/Exit；core `osc.rs` 旁路扫描 OSC 7/9/52/777（alacritty `osc52 = Disabled` 防重复），OSC 52 事件由 app 写剪贴板/PRIMARY 并回写查询 | implemented | OSC 4/10/11/12 查询走 Listener 回包（Term.colors() 运行时色优先）+ fill_frame 叠加 Palette::set_rgb；104/110/111/112 复位回主题；tests/osc_palette.rs 7 例 + CSI 14t/18t/OSC 133 端到端 | OSC 0/2 标题、OSC 7、OSC 9 通知/进度、OSC 52 双向可用 | `TextAreaSizeRequest` 仍无响应；OSC 4/10/11 颜色查询无测试；无 OSC 133；OSC 52 查询回包截到 1 MiB |
| TERM-012 | 可点击 URL/IP/路径；链接识别器支持插件扩展 | U-XTERM / 需求 C1 | `link.rs` 行内识别（URL/IPv4[:port]/路径；路径边界防参数误判）；`frame.rs` OSC 8 表/`hyperlink_at` + `link_at`（显示列映射宽字符/组合字符）；app Ctrl+点击 URL/本地路径 + hover 手型 + 右键 Open/Copy Link；裸 IP 只识别不打开；无插件 Linkifier 接口 | implemented | 插件 Linkifier trait + register_linkifier/scan_custom，Frame::link_at 内置无命中时兜底；版本/时间（v1.2.3:4、12:34:56）排除 | URL/OSC 8/本地路径可点击打开；插件 Linkifier 已接入；路径失败有 UI/日志；IP 检测保留但按设计不打开 | 折行 URL 不拼接；路径含空格/`~user`/`file:line` 不识别；远端路径不本地打开；TUI 鼠标上报下 Ctrl+click 仍转发 |
| TERM-013 | 高输出不丢字节、UI 可合帧、输入不被输出阻塞 | U-XTERM / 需求 C1 | pane.rs：键盘/粘贴直接进 **16 MiB 有界写 backlog**（UI 永不阻塞），PTY 读有界，wake 合并；`pull` 100ms 硬超时跳帧 + force_full 补画；copy/search/contents 200ms 上限；新增 `tests/p0_backpressure.rs`（慢 reader + 10 MiB + 输出洪峰、>16 MiB 丢弃计数、ctrl 回复上限） | implemented | core 90 lib + 63 integration；P0 4 例全过：10 MiB 逐字节一致 worst_input 0.8ms / worst_pull 0.15ms；backlog 溢出 16 MiB 后明确丢弃 3 MiB；修复前 master probe：input/pull 均 >2s 不返回（见 self-iteration 记录） | UI 输入/拉帧有界且顺序保持；正常按键/10 MiB 粘贴不丢 | 持续灌入 >16 MiB 会丢弃新块（计数+日志）；极端负载下 ctrl（resize/theme）可能 50ms 投递失败；50 MiB/s 零丢字节正式压测仍未做 |
| TERM-014 | 终端 resize 正确向 PTY/SSH PTY 发送行列 | U-XTERM / 需求 C1 | pane.rs `Ctrl::Resize` → `Term::resize` + `master.resize(PtySize)`；main.rs `refresh_views` 检测 grid 变化 | implemented | 人工：拖窗口/分隔线后 shell `stty size` 变化（ui-shell §2）；无自动断言 | 已有手工验收路径 | 无自动化 resize 断言；SSH PTY 未实现 |
| TERM-015 | 终端录制/输出保存核心 API（UI 可由插件提供） | U-XTERM / 需求 C1 | 无实现 | implemented | core recording.rs：Recorder/RecordingStatus/Export（64MiB 上限、超限计数、关闭时零锁快路径）；PaneHandle start/stop/export/snapshot；app terminal.recording-* 命令 + Save 弹窗导出；含真实 PTY 集成测试 | implemented | 已补录制/导出 API 与 UI 命令；save-output 插件兼容仍待接 |
| TERM-016 | 完整键盘映射：Ctrl/Alt/Shift/Super、功能键、组合键、IME、平台键码 | U-XTERM / 需求 C1 | keys.rs 保留 legacy 字节路径（`encode_mode`），应用请求协议后走 `encode_key_event`：kitty CSI-u / xterm modifyOtherKeys；core/keyboard.rs 维护 kitty flags + main/alternate 两套栈（query/set/push/pop 自解析，规避 alacritty 0.26 的栈顶 query / set-push-pop 语义偏差）与 XTMODKEYS；app 接 Slint `key-released` + `event.repeat`，按 REPORT_EVENT_TYPES 回放 press/release；main.rs 保留 X11+XIM IME 路径与修饰键伪字符过滤；core 读线程用 `tcgetpgrp` 观察前台进程组，子命令退出/回到 shell 时复位 kitty flags 与 modifyOtherKeys（崩溃/被 kill 的应用不会把 shell 输入变成 CSI-u 回显乱码） | partial | app 297 passed（keys 19 例 + config/旧路径零变化）；core 107 lib + 7 keyboard_protocol integration（伪 TUI：query/set/push/pop/alt/modify/off + `foreground_command_exit_resets_stale_kitty_flags`）；Xvfb :123 `scripts/e2e_keyboard_protocol.sh` 4 场景 PASS；真实 dsh-tui（TERM=xterm-kitty）flags=1，Ctrl+Shift+J → `CSI 106;6u`、Shift+Enter → `CSI 13;2u` | 常用键、kitty/modifyOtherKeys、repeat/release 可用；普通 shell 字节不变；dsh 真实链路可用；前台命令崩溃后不残留增强键盘状态 | 无 Wayland IME；base layout key 无法从 Slint 取得；caps/num lock 运行时不可见（编码位已支持）；未请求协议时 Shift+Tab 仍保持旧 TAB；kitty 编码受 app 硬编码快捷键拦截（Ctrl+Shift+字母等） |

### C2. Local Shell / PTY（7 条）

状态：implemented=2，partial=3，not_started=1，unsupported=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| LOCAL-001 | Linux PTY 创建、启动、读写、resize、退出状态回收 | U-XTERM / 需求 C2 | pane.rs `spawn`（portable-pty openpty/spawn/take_writer/try_clone_reader）；Drop kill；EOF 排空 | implemented | smoke `pty_prints_and_ui_pulls`、`backspace_erases_last_char`；phase0 EOF 修复记录 | 已有记录 | 退出状态码未呈现；异常退出无 UI |
| LOCAL-002 | 自动识别默认 Shell；指定 executable 与参数 | U-XTERM / 需求 C2 | pane.rs shell=config.shell/$SHELL/`/bin/bash`；main.rs `--shell`、`--` args | implemented | 人工 `--shell /bin/sh`；`--help` 与 parse_args；代码走查 | 已有手工路径 | `-l` 帮助有但 parse_args 不识别；无登录 shell 选项 |
| LOCAL-003 | Profile 支持起始目录、环境变量、命令、参数、标题、自定义选项 | U-XTERM / 需求 C2 | profile.rs `Profile{shell,args,cwd,env}` + 外观字段；main.rs `create_pane` Profile 优先；Profile 表单 Shell/Args/Cwd + 共用 Appearance 区 | implemented | Profile 新增 title（None=name，effective_title 用于新 Tab）+ 表单 Name/Title 双列 + Cwd 行 Test 按钮（profile::test_start_dir 展开 ~/校验）；2 条单测 | 命令/参数/起始目录/env（手写 profiles.toml）/外观可用 | 表单无 env/自定义标题；无“起始目录”单独测试按钮；无 UI 级 env e2e |
| LOCAL-004 | 多个本地 Shell Profile 并在同一窗口混用 | U-XTERM / 需求 C2 | `+` 直接新建默认 Tab；`>_` Profile 菜单按 Profile 打开同窗 Tab（Profile 自带 shell/args/env/cwd，表单含 Env 行）；菜单为纯列表，无搜索框 | implemented | `scripts/e2e_tabbar_plus_profile.sh` PASS（Xvfb :152 + openbox）：`+` → `[ws] new tab: tabs=2` 且无 profile menu；`>_` → `[ui] profile menu profiles=2`；↓+Enter → `[profile] open "beta"`；截图 `.logs/tabbar-plus-profile/e2e/shots/` | + 开默认 Tab、`>_` 选 Profile 同窗混用 | Profile 菜单无搜索、多 Profile 需滚动；仅 local 类型自带 shell/env/cwd |
| LOCAL-005 | Shell 启动失败、进程退出、PTY 关闭、异常退出处理 | U-XTERM / 需求 C2 | spawn 失败 eprintln；CoreEvent::Exited 产生但 `poll_events` 只处理 Title；最后一帧保留 | implemented | 会话横幅扩到所有 pane（进程退出/信号/PTY 关闭，带退出码与 Restart/关闭建议），启动失败也弹 notice；Xvfb e2e shell=/nonexistent 与 sh -c true | 异常不会 panic；无用户可见提示/自动重启 | 启动失败仅 stderr；退出无状态/重试/关闭建议 |
| LOCAL-006 | Shell Integration / 当前目录检测 / 命令完成检测 | U-XTERM / 需求 C2 | session.rs `cwd_of` 读 /proc/<pid>/cwd；无 OSC 7/133/命令完成解析 | implemented | core ShellMark/OSC 133;A|B|C|D[;code] 流式解析 + CoreEvent::Shell；app 记录提示符阶段/退出码/完成数并打 [shell] 日志、非活动 Tab 完成标记；OSC 7 cwd 优先用于本地链接解析 | 可通过 /proc 取 cwd 供快照 | 无 shell 侧集成脚本；无命令完成/退出码/提示符检测 |
| LOCAL-007 | Windows 专属 PowerShell/WSL/Git Bash/Cygwin/MSYS2/Cmder/CMD/Clink 能力 | U-XTERM / 需求 C2 | 无 Windows 代码路径 | unsupported | 无 | 明确不在 Ubuntu 阶段实现 | 列入 Windows 路线，不在本阶段伪造支持 |

### C3. SSH2 Client（16 条）

状态：needs_verification=1，not_started=14，unsupported=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| SSH-001 | SSH2 建连、断开、超时、keepalive、重连 | U-AUDIT / 需求 C3 | 默认 `transport=native`：russh 进程内连接/认证/PTY/shell、ProxyJump 多跳、本地/动态/远程转发、agent forwarding；纯 Rust ssh_config/known_hosts；20s 连接超时、60s inactivity；ProxyCommand/Match/IdentityFile/extra_args 自动回退系统 ssh | partial | 进程内 russh test server：password + known_hosts + PTY/shell + echo；ProxyJump 双 server；local forward 到 echo；remote/agent 代码路径 | 简单连接、ProxyJump、本地转发、动态 SOCKS、远程转发、agent forwarding、加密私钥口令可用 | 无 keepalive 配置项；无真实外部 sshd e2e；远程转发未做端到端测试 |
| SSH-002 | Connection Manager 与保存的 SSH Profiles | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无连接管理/Profile |
| SSH-003 | 用户名/主机/端口/认证方式 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无 SSH 配置/UI |
| SSH-004 | 密码、私钥、加密私钥口令、SSH Agent 认证 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无认证/Keyring/Agent |
| SSH-005 | known_hosts 验证；首次确认；密钥变化告警 | U-AUDIT / 需求 C3 | 系统 ssh 完成握手；**W-02** 连接前应用内预检（ssh-keyscan）给出首次/变更提示并显式 Trust/Update；askpass 对 host-key prompt 回答 no | partial | known_hosts 14 例；Xvfb :139 首次/变更截图；真实 ssh -G/ssh-keygen 参与测试 | 首次/变更提示与 known_hosts 写入可用，无静默信任 | 非原生 SSH2 校验；ProxyJump/keyscan 失败回退终端；无管理 UI |
| SSH-006 | SSH Config 导入/兼容（Host/HostName/Port/User/IdentityFile/ProxyJump/ProxyCommand） | U-AUDIT（上游支持范围待核） | 无实现 | needs_verification | 待上游核实支持字段 | 未核实/未实现 | 无 config 解析；上游映射范围未知 |
| SSH-007 | Jump Hosts / ProxyJump / 多级跳板机 | U-AUDIT / 需求 C3 | **W-02** Profile `proxy_jump` → `ssh -J`；OpenSSH 支持逗号分隔多级跳板；SFTP 同步使用 | implemented | profile 单测 + e2e 表单；系统 ssh 语义 | Jump Host（含多级）可配置连接 | 依赖系统 ssh；无跳板链路可视化/单独凭据 |
| SSH-008 | Proxy Command（平台能力与安全策略核验） | U-AUDIT（能力/策略待核） | 无实现 | not_started | 无 | 未实现 | 无 ProxyCommand；安全策略未定 |
| SSH-009 | Agent Forwarding；Linux OpenSSH Agent；后续 Pageant/Windows Agent | U-AUDIT / 需求 C3 | **W-02** `agent_forward=true` → `ssh -A`；Profile 表单开关 | partial | profile argv 单测 + e2e 表单 | Linux OpenSSH Agent forwarding 可配置 | Pageant/Windows Agent 未做；无状态显示 |
| SSH-010 | X11 Forwarding 与自定义 X11 Display | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无 X11 forwarding |
| SSH-011 | Local/Remote/Dynamic SOCKS 端口转发 | U-AUDIT / 需求 C3 | Profile 支持 L/R/D；native 全部实现，远程 port 0 已支持；`ForwardController` 暴露规则/状态，Port Forwards 面板显示监听/目标/状态并启停 local+dynamic | implemented | profile 校验/argv；ssh_forward 状态单测；local/remote in-process e2e | native 当前会话可查询/控制；remote 显示服务器分配端口；规则写回 Profile | 系统 ssh/插件后端只显示 Profile 规则；remote 不支持运行时取消 |
| SSH-012 | Persistent Port Forwards：预配置并随连接恢复 | U-AUDIT / 需求 C3 | 调研完成：Profile 规则持久化，自动重连会重建会话并按 Profile 重启转发 | partial | 现有重连/转发测试覆盖新连接恢复 | 预配置随新连接/重连恢复 | 无独立于 pane 的常驻转发进程；断线期间 listener 不保持 |
| SSH-013 | Login Scripts：连接后自动执行，顺序/等待/延时/错误处理 | U-AUDIT / 需求 C3 | Profile `login_scripts` 原始行；native shell 后按序发送；`command/delay/wait/timeout/on_error` 解析；多行设置编辑器 | implemented | 解析单测；in-process server 收到命令顺序 e2e | 命令只发远端 PTY，不执行本地 shell；超时默认停止后续脚本并保留会话 | 单行分号格式，命令内字面 `;` 需写 `\;`；无交互式密码/确认脚本 |
| SSH-014 | Ciphers/算法配置入口；默认遵循系统/库安全值 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无 SSH 算法配置 |
| SSH-015 | SSH 主机别名、连接历史、最近连接 | U-AUDIT / 需求 C3 | 成功 SSH 连接写 `~/.config/xtervo/history.toml`（Profile/名/host/user/port/时间，无凭据）；`>_` 菜单顶部 ↺ 最近项 | partial | history 去重/上限/损坏文件/相对时间单测；菜单映射复用 Profile 下标 | 最近成功连接可见并可一键重开 | 无主机别名管理；无历史清理/固定项 UI |
| SSH-016 | WinSCP 集成（平台/外部工具） | U-AUDIT / 需求 C3 | 无实现 | unsupported | 无 | Ubuntu 不可用 | 外部工具集成，非当前平台目标；需给出替代入口 |

### C4. SFTP 与文件传输（7 条）

状态：partial=7。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| SFTP-001 | 基于 SSH Channel 的 SFTP 会话 | U-AUDIT / 需求 C4 | 系统 `sftp -q -b -` 子进程复用 SSH Profile（key/port/extra args）；非内建 SSH Channel | partial | `--sftp-probe list/get/put` 对 `scripts/ssh_test_server.py`（paramiko）通过；无 GUI | 经系统 sftp 可建立会话并传文件 | 无内建 SFTP 协议栈；依赖系统 `sftp`；连接/认证行为沿用系统 ssh |
| SFTP-002 | 远端文件列表、目录导航、文件元数据 | U-AUDIT / 需求 C4 | 面板 Remote 路径 + `ls -1` + 点选；list 后台 + 10s 超时 + **W-02 Cancel**；ConnectTimeout/keepalive | partial | e2e 列表截图；假 sftp 超时/取消单测；paramiko probe（既有） | 列表与路径输入可用，UI 不冻结 | 无上级/刷新/权限/mtime/大小列；路径特殊字符未转义 |
| SFTP-003 | 上传/下载、建目录、重命名、删除 | U-AUDIT / 需求 C4 | Download/Upload 后台 TransferJob；mkdir/rename/rm/chmod SftpOpJob + 10s 硬超时；Delete 二次确认；**W-02** 队列/失败 Retry/chmod | partial | queue/chmod/取消/失败单测；Xvfb :139 队列/chmod/retry 截图；probe（既有） | 上传/下载（含目录）/建目录/重命名/删除/chmod/队列/重试可用 | 无覆盖确认/断点续传；目录删除非递归；取消可能留半成品 |
| SFTP-004 | 传输进度、速率、取消、失败重试 | U-AUDIT / 需求 C4 | TransferEvent Progress/Finished；解析进度行；UI 进度/Cancel/队列/Retry；WakeSignal 立即唤醒 | partial | 进度/状态机/取消/失败/唤醒/queue 单测；W-02 e2e 队列/取消/失败/Retry 截图 | 开始/当前文件/完成/失败/取消/队列/重试可用 | 非 tty 下通常不确定进度；无总大小/ETA/自动重试；取消留部分文件 |
| SFTP-005 | 大文件流式传输，不一次性读入内存 | U-AUDIT / 需求 C4 | 文件内容由系统 sftp 流式读写，应用只读 stdout/stderr 文本；目录 `get -R`/`put -R` | partial | probe 1MiB 文件（字节比对一致）与目录树 get/put 通过 | 应用进程不把文件读入内存 | 不是应用内流式通道；无缓冲区/限速/续传；未做 >1GiB 长稳实测 |
| SFTP-006 | 独立 SFTP Tab/面板；插件实现也需原生等价能力 | U-AUDIT / 需求 C4 | 独立 SFTP 面板（TabBar 菜单 → SFTP），不占 Tab；含 profile 选择/路径/列表/文件操作/进度区 | partial | e2e 面板 `.logs/sftp-panel.png`；代码走查面板组件 | 面板可用 | 不是独立 Tab；无插件 API/原生等价能力 |
| SFTP-007 | 权限、覆盖确认、路径编码、断线中断处理 | U-AUDIT / 需求 C4 | **W-02** chmod（八进制 0–7777）UI；Delete 二次确认；错误经 stderr 回显；ConnectTimeout/keepalive 让断线约 10s 失败 | partial | chmod/parse_mode/失败单测；Xvfb :139 chmod/失败截图 | 权限修改、错误展示、删除确认可用 | 无覆盖确认；路径编码只做 UTF-8 lossy，特殊字符未转义；断线=失败，无续传 |

### C5. Zmodem（4 条）

状态：unsupported=4。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| ZMODEM-001 | SSH/Telnet/Serial 会话中的 Zmodem 发送/接收 | U-AUDIT / 需求 C5 | 无实现；系统无 `lrzsz`（sz/rz），也没有内建协议栈 | unsupported | 环境探测 `which sz rz lrz lsz` 均为空（2026-11-05） | 明确不支持 | 需内建 Zmodem 状态机 + 文件 UI；本轮记 unsupported 而不是假实现 |
| ZMODEM-002 | 识别 Zmodem 协议序列并切换传输，不误显示为终端文本 | U-AUDIT / 需求 C5 | 无实现（无协议识别） | unsupported | 无 | 明确不支持 | 需在输出流里识别 `**\x18B00` 等序列，属内建 Zmodem 一部分 |
| ZMODEM-003 | 文件选择、保存位置、冲突处理、进度、取消、失败提示 | U-AUDIT / 需求 C5 | 无实现 | unsupported | 无 | 明确不支持 | 依赖 ZMODEM-001 |
| ZMODEM-004 | 大文件与断线异常处理 | U-AUDIT / 需求 C5 | 无实现 | unsupported | 无 | 明确不支持 | 依赖 ZMODEM-001 |

### C6. Serial Terminal（10 条）

状态：needs_verification=5，not_started=5。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| SERIAL-001 | 串口枚举、端口打开/关闭 | U-AUDIT / 需求 C6 | 默认 `serialport` 进程内打开/关闭/读写；PTY 回环测试；打不开时回退 socat | partial | `SerialTransport` PTY 收发测试、缺失设备错误测试 | 端口打开/关闭/读写可用 | 端口枚举/热插拔未做（需 udev/串口枚举） |
| SERIAL-002 | 保存多个 Serial Profiles | U-AUDIT / 需求 C6 | Profile kind=serial + 多 Profile 存储/表单 | implemented | Profile serialization/roundtrip | 可保存多个 Serial Profile | 无端口枚举选择器 |
| SERIAL-003 | Baud Rate（含平台非标准）、Data Bits、Parity、Stop Bits、Flow Control | U-AUDIT / 需求 C6 | `serialport` 配置 baud/data_bits/parity/stop_bits/flow_control；设置页字段 | implemented | PTY 回环 + Profile 校验/roundtrip | 5-8 数据位、none/even/odd、1/2 停止位、none/software/hardware 流控 | 非标准波特率交给 serialport/驱动；未逐项真机验证 |
| SERIAL-004 | 输入：文本、Readline、逐字节/Hex（按上游定义核验） | U-AUDIT（上游定义待核） | 无实现 | needs_verification | 待上游核实输入模式 | 未核实/未实现 | 无 Serial 输入处理 |
| SERIAL-005 | 输出：文本、Hexdump（按上游定义核验） | U-AUDIT（上游定义待核） | 无实现 | needs_verification | 待上游核实输出模式 | 未核实/未实现 | 无 Serial 输出处理 |
| SERIAL-006 | CR/LF/CRLF 等 Newline Conversion | U-AUDIT（上游策略待核） | 无实现 | needs_verification | 待上游核实换行策略 | 未核实/未实现 | 无换行转换 |
| SERIAL-007 | 自动重连、设备拔插识别、重连失败提示 | U-AUDIT / 需求 C6 | 无实现 | not_started | 无 | 未实现 | 无串口热插拔/重连 |
| SERIAL-008 | 串口读写缓冲与高频数据处理 | U-AUDIT / 需求 C6 | 无实现 | not_started | 无 | 未实现 | 无串口后端/缓冲 |
| SERIAL-009 | Zmodem 支持（若原版 Serial 支持，纳入兼容测试） | U-AUDIT（上游支持待核） | 无实现 | needs_verification | 待上游核实 Serial Zmodem | 未核实/未实现 | 无 Serial/Zmodem |
| SERIAL-010 | 可选时间戳输出（原生或 serial-timestamp 插件兼容） | U-AUDIT（插件现状待核） | 无实现 | needs_verification | 待核实插件来源/可用性 | 未核实/未实现 | 无时间戳/插件桥 |

### C7. Telnet（4 条）

状态：needs_verification=1，not_started=3。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| TELNET-001 | Telnet Client：主机、端口、连接、断开、重连 | U-AUDIT / 需求 C7 | 无实现 | not_started | 无 | 未实现 | 无 Telnet 客户端 |
| TELNET-002 | Telnet IAC 协商和转义，不能当普通 TCP 文本 | U-AUDIT / 需求 C7 | 无实现 | not_started | 无 | 未实现 | 无 IAC 解析 |
| TELNET-003 | 终端输入输出与 Terminal Engine 对接 | U-AUDIT / 需求 C7 | 无实现 | not_started | 无 | 未实现 | 无 Telnet 传输层 |
| TELNET-004 | Zmodem 收发（若上游当前支持） | U-AUDIT（上游支持待核） | 无实现 | needs_verification | 待上游核实 Telnet Zmodem | 未核实/未实现 | 无 Telnet/Zmodem |

### C8. Profiles / Workspace / Session 持久化（9 条）

状态：partial=4，not_started=5。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| SESS-001 | Shell/SSH/Serial/Telnet Profile 存储、编辑、分组 | U-AUDIT / 需求 C8 | `profile.rs` v2：四类 kind + 字段 + `validate()` + v1→v2 迁移；表单支持**新建/编辑**（行内 ✎ 回填、`)`update` 保留 ID）；行内 × 删除 | implemented | Profile.group（additive）+ 设置页分组展示/折叠/搜索 + 自有格式导入导出 xtervo.profiles v1（无密码，Vault 引用单列）；5 条单测 + --profile-rows 探针证据；转发字段管理 UI 不在此项 | Profile 增/删/改/打开可用 | 无分组/导入导出/搜索；转发字段无专门管理 UI |
| SESS-002 | Profile 绑定快捷键；支持默认 Profile | U-AUDIT / 需求 C8 | 无实现 | implemented | config 新增 default_profile（按名字不区分大小写，id 优先）+ 星标跟随解析 + 热更新分类；Xvfb 首 Tab 日志“使用默认 Profile id=7” | 未实现 | 无快捷键/默认 Profile |
| SESS-003 | Split Tab 布局保存为 Profile/Workspace | U-AUDIT / 需求 C8 | 无实现 | implemented | workspace.save 改为命名弹窗（预填 Tab 标题并全选、实时空名/重名提示、Enter 保存 Esc 取消）；Xvfb palette→workspace.save→写盘 workspaces.toml | 未实现 | 只有匿名 Restart 快照 |
| SESS-004 | 记住打开的 Tabs/Panes；下次启动恢复工作区 | U-AUDIT / 需求 C8 | session.rs Snapshot（Tree::Leaf 可选 profile_id）；main.rs `capture_snapshot`/`save_session_on_exit`/`restore_session`；`--restore` + `restore_session_on_startup`（默认 false，Settings→Behavior） | implemented | session.rs 8 例；Xvfb :136 e2e：正常退出写快照，重启恢复 3 Tab/pretty split/cwd=/var、/etc、/usr/窗口 120x32；损坏快照回退新 Tab（`09-fallback-newtab.png`） | 手动 Restart 与可选自动启动恢复均可还原布局/cwd/Profile 关联；失败回退新 Tab | 默认仍不自动恢复；无崩溃恢复；无滚动历史/运行中进程；非 local Profile 不自动重连 |
| SESS-005 | 恢复最近关闭的 Tab | U-AUDIT / 需求 C8 | 关闭时压入 `closed_tabs`（最多 10 个，存布局树 + 每个 pane 的 cwd）；Ctrl+Shift+T、TabBar 菜单 Reopen Closed Tab、命令面板共用 `reopen_closed_tab` | implemented | 真机：tab2 `cd /tmp` 后关闭 → Ctrl+Shift+T → `[ws] reopen closed tab`，新 tab `pwd` 显示 `/tmp`；菜单项同样验证（.logs/reopen-tab.png） | 最近关闭 Tab 可恢复布局与 cwd | 只恢复布局/cwd，不起回原进程；不恢复滚动历史；栈上限 10 |
| SESS-006 | 区分恢复 UI 布局、连接配置、真实运行进程；不混淆 | U-AUDIT / 需求 C8 | session.rs 只存树/ratio/cwd/可选 profile_id，不存凭据/进程；`restore_pane_launch` 只对 local Profile 重建启动信息，SSH/Serial/Telnet 回退普通 shell；docs/self-iteration 明示边界 | implemented | session::restore_boundaries/text 明确“恢复布局+cwd+本地 Profile，不恢复进程/历史”，设置页常驻说明行 + 恢复日志打印边界；1 条单测 | 边界清晰：恢复布局+cwd+local Profile 启动配置，不恢复进程/历史；非 local 明确不自动建连 | 无连接状态/重连模型；重启仍是新 shell；SSH Profile 只保留 Tab 标题/布局/cwd（回退 shell） |
| SESS-007 | 断开连接后的重连策略与 Session 状态提示 | U-AUDIT / 需求 C8 | connection.rs 通用连接状态（Connecting/Connected/Disconnected/Failed + 重试计数与策略，config 可配）+ Tab 标记与状态 pill；本地/插件共用，去掉“仅 ssh”限制 | implemented | connection 4 条单测；`scripts/e2e_workspace_ui.sh` Connected/Disconnected/Failed 日志与截图 | 连接状态/重试策略/状态 pill 可用 | 不恢复 scrollback/运行进程；状态 pill 是底部 overlay，可能盖住最后一行 TUI |
| SESS-008 | 配置格式版本化、迁移、导入、导出 | U-CONF / 需求 C8 | config.rs `SCHEMA_VERSION=1`、`load_from` 迁移前备份、`migrate`、原子写、`extra` 保留未知键；session.rs VERSION=1 + 原子写；无导入/导出 UI | implemented | ConfigInspection::inspect_file + export_to/import_from/restore_latest_backup（拒绝未来版本与坏备份）+ Advanced 页三按钮与报告行；3 条单测 | 版本化/迁移/备份/原子写已有记录（b80e265） | 无导入/导出/恢复 UI；备份只在迁移时生成；无用户可见迁移报告 |
| SESS-009 | 导入原 Tabby 配置：Profiles/快捷键/主题/插件配置，生成不兼容报告 | U-CONF / 需求 C8 | `tabby_import.rs`：按 Tabby master `467ce8e`（2026-10-06，tabby-core 1.0.231-nightly.0，config `version: 1`）解析 `config.yaml`，导入 local/ssh/serial/telnet Profiles + `terminal.fontSize`；21 条语义明确命令的 hotkeys（`Ctrl-Shift-P`/`Ctrl--`/`Super`/`⌘` 归一化）；colorScheme/lightColorScheme/customColorSchemes → `themes/tabby-<slug>.toml`；未映射/冲突/非法/多段键/坏色值进报告；CLI `--import-tabby/--export-tabby` + 设置页显示数量与首个跳过原因 | implemented | ImportReport.not_imported（插件配置/CSS/凭据，只报路径不回显值）+ 中英文摘要接入设置页导入状态行；1 条单测 | Profiles/字号/映射快捷键/终端配色可迁移，Profile 导出可回灌 | 不导入插件配置、UI 主题 CSS、密码；hotkeys 只映射子集（`profile.*`/`group-selectors.*`/tab 序号/方向导航/滚动/MRU 未映射），每命令只保留一条单段 chord，Tabby 的 `[]`（取消默认键）无法表达；主题忽略 selection/cursorAccent；schema 依据 commit `467ce8e`，旧/新版本可能漂移 |

### C9. Hotkeys / Commands / Keyboard protocol（12 条）

状态：implemented=7，partial=3，not_started=2。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| KEY-001 | 所有核心操作都有可配置命令 ID | U-AUDIT / 需求 C9 | `commands.rs::builtin` 24 条稳定 ID + 默认快捷键；`handle_input` 在硬编码默认键前先按当前按键构造 chord 并匹配用户 override（只认表内命令），命中后 `run_command`；改键后旧默认键被吞掉；无 override 时硬编码路径行为不变 | implemented | F2/F11/Ctrl+Tab/Ctrl+1..9 与 Ctrl/Ctrl+Shift 硬编码 match 全部迁入 Registry（global/extra_shortcuts + run_command 的 tab.select-N/pane.focus-*）；新增全局分发与别名测试 | 24 条命令均可用 override 改键并立即生效；默认路径无回归（逻辑走查 + 单测） | F2/Ctrl+1–9 等入口仍硬编码、无命令 ID；分发尚未完全由 Registry 驱动；平台保留键未处理 |
| KEY-002 | 自定义快捷键，支持单键与 multi-chord | U-AUDIT / 需求 C9 | `config.hotkeys: Vec<HotkeyOverride>` + `Chord::parse/canonical/is_bindable`；快捷键页录制单段组合键（Ctrl/Shift/Alt + 键或 F1–F24），`set_override/clear_override` 持久化；`Shortcut::parse` 仍支持 1–2 段（默认值/显示），override 只接受单段 | implemented | chord_from_key_event 接收 meta + X11 Ctrl+字母折叠；Hotkeys 页两段录制开关（第一段暂存/Enter 存单段）；分发支持 2s 两段序列；config 归一化含 “Ctrl + K Ctrl + T” | 单段自定义键可录制/持久化/生效；旧配置无 `hotkeys` 字段直接兼容 | multi-chord 只能解析/展示，不能录制或配置；Meta 不在 key 回调中；普通字符与 Shift-only 组合被拒绝以免抢占终端输入 |
| KEY-003 | 冲突检测、重复快捷键处理、平台保留键处理 | U-AUDIT / 需求 C9 | `Registry::effective_conflicts` 把 override 计入生效绑定；保存前 `validate_override` 拒绝同 chord 绑不同命令；Hotkeys 页行内标红 + 右上角冲突数；无平台保留键表 | implemented | 启动 repair_hotkey_overrides 自动清理未知命令/非法 chord/不可绑定/重复/前缀冲突（后写优先）并写回；PLATFORM_RESERVED 保留键清单在录制时警告 | 冲突检测覆盖默认键与 override，保存时拒绝并提示，页头汇总冲突数 | 未处理桌面环境保留键；手改 config 的重复绑定只报告/分发取最后一条，不自动修复 |
| KEY-004 | Profile 绑定快捷键 | U-AUDIT / 需求 C9 | 无实现 | implemented | Profile 热键：单段 chord 解析、必须带 Ctrl/Alt、磁盘往返、冲突提示（`default_command_for_chord`）；Xvfb XTEST 实测 Profile 热键开 Tab | 未实现 | 无 Profile/快捷键 |
| KEY-005 | 命令面板可搜索并执行命令 | U-AUDIT / 需求 C9 | `Registry::search` + Palette UI；`run_command` 分发 30/30 条（terminal.find/case-regex 选项、read-only、reset、save-contents 已接 core） | implemented | 单测 `fuzzy_search_prefers_title_prefix`；真机点击执行 `tab.new` 成功；菜单 Select All/Clear 生效 | 2026-11-05 实测通过 | 键盘路径未在沙箱验证（注入不可用） |
| KEY-006 | 插件可注册命令、快捷键、命令上下文 | U-AUDIT / 需求 C9 | 无实现 | implemented | v3 manifest [[commands]] 新增 keybinding/when；plugin_v3/context.rs 表达式求值；PluginStore::command_refs 现算（enabled+approved）并过滤 palette/按键；Hotkeys 页合并插件命令并标冲突；625 行级测试通过 | 未实现 | 无插件 API |
| KEY-007 | 快捷键配置可迁移并提供默认配置 | U-AUDIT / 需求 C9 | `builtin()` 提供默认快捷键；`Config.hotkeys` 为可选 `[[hotkeys]]` 表（`#[serde(default)]`，空表不写盘），旧配置无需迁移；未知命令/非法 chord 在生效与分发时忽略 | implemented | `builtin_registry_is_consistent`、`hotkeys_default_to_empty_and_old_config_loads`、`hotkeys_roundtrip_and_are_written_as_tables`、`empty_hotkeys_are_omitted_from_toml` | 默认配置和自定义配置都能稳定 roundtrip；旧版 config.toml 直接加载 | 手改配置中的未知 id / 非法 chord 只跳过、无 UI 诊断；无配置导出/导入快捷键 |
| KEY-008 | Backspace/Delete 发送编码可配置（GNOME Terminal 兼容） | U-XTERM / 需求 C1 | keys.rs `BackspaceBinding`/`DeleteBinding`/`KeyBindings`；`encode_mode` 对 `\u{0008}`/`\u{007f}` 按配置映射（0x7f / 0x08 / ESC[3~），Alt 保留 ESC 前缀，kitty 模式不覆盖特殊键；config `backspace_binding`/`delete_binding` 解析大小写/空白不敏感、未知值回退默认；设置页即时生效 | implemented | app `backspace_binding_controls_encoding`、`delete_binding_controls_encoding`；Xvfb :118 真实 XTest 按键：默认 `bytes=[127]`/`bytes=[27, 91, 51, 126]`，control-h/ascii-del 配置 `bytes=[8]`/`bytes=[127]` | 默认 GNOME 值；两键编码独立可配；改设置立即生效，不影响快捷键/IME | 只支持题面三种 Delete/两种 Backspace 值；Ctrl+Backspace 未做特殊映射（仍按当前绑定） |
| KEY-009 | Kitty keyboard protocol query/set/push/pop + 主/备用屏独立栈 | U-XTERM / 需求 C1 | `core/keyboard.rs` 解析 `CSI ? u` / `CSI = flags;mode u` / `CSI > flags u` / `CSI < n u`；自维护 current flags 与 main/alternate 两套栈（上限 32，满时淘汰最旧），query 只回实际支持的 5 bit；alacritty 0.26 的 set-without-push query 与 set-push-pop 恢复语义不符规范，故关闭其 kitty 实现由 core 接管；`keyboard_protocol="off"` 时扫描器完全 inert | implemented | core 13 lib + `keyboard_protocol.rs` 6 例：初始 `?0u`、set `?3u`、push `?7u`、pop 回 `?3u`、备用屏 `?3u` → 回主屏 `?1u`、off 无回包；Xvfb :123 真实 app 收到 `ESC[?0u`/`ESC[?3u` | query/set/push/pop 与双屏栈按规范工作 | 栈上限 32；备用屏只识别 47/1047/1049；只支持实际实现的 5 个 flags |
| KEY-010 | Kitty CSI-u 编码：modifiers / flags / 特殊键 / alternate / associated text | U-XTERM / 需求 C1 | `keys.rs::encode_key_event`：DISAMBIGUATE 只对 ctrl/alt/Esc 生效，REPORT_ALL 所有键 CSI-u；Enter/Tab/Backspace 未开 report-all 时保持 legacy；方向/Home/End/Insert/Delete/Page/F1–F24 用 kitty 规范序列（F3=`CSI 13;mod~`，F13–F24 PUA）；修饰位 shift=1/alt=2/ctrl=4/super=8/caps=64/num=128 +1；REPORT_ALTERNATE_KEYS 带 shifted key，REPORT_ASSOCIATED_TEXT 带文本；Slint meta→super | implemented | app `kitty_disambiguate_printable_modifiers`、`kitty_report_all_and_associated_text`、`kitty_special_keys_use_canonical_sequences`、`kitty_alternate_keys`、`modifier_and_lock_bits` 等；Xvfb :123：Ctrl+A→`CSI 97;5u`、Shift+Up→`CSI 1;2A`；dsh flags=1 下 Ctrl+Shift+J→`CSI 106;6u`、Shift+Enter→`CSI 13;2u` | 协议 flags 驱动的编码与特殊键 canonical 序列可用；普通 shell 逐字节不变（`no_protocol_is_byte_identical_to_legacy`） | base layout key 无法从 Slint 取得（只报 shifted）；caps/num lock 运行时恒 0；非 US 布局的 shifted 近似；多字符 IME 文本仍走 legacy |
| KEY-011 | xterm modifyOtherKeys（`CSI > 4 ; level m`） | U-XTERM / 需求 C1 | `core/keyboard.rs` 旁路扫描 `CSI > 4 ; n m` / `CSI > 4 m` / `CSI ? 4 m`，level 0–3；app 对可打印键编码 `CSI 27;mod;code~`（level 1 只对 alt/meta，level 2 所有修饰，level 3 无修饰也编码），功能键用 xterm 带修饰序列（F1–F20），kitty flags 非空时 kitty 优先；非法/不支持 level 忽略并回退 legacy | implemented | app `modify_other_keys_levels`、`modify_other_keys_special_keys`、`kitty_has_priority_over_modify_other_keys`；core 5 例 + integration `?4m` 应答；Xvfb :123：level 2 下 Ctrl+A→`CSI 27;5;97~`、Shift+Up→`CSI 1;2A`，普通 shell/off 不变 | 应用发 `CSI > 4;2m` 后带修饰键按 xterm 语义上报，同时请求 kitty 时以 kitty 为准 | `CSI > 4 m`（无值）按常见应用意图映射 level 2（xterm 原语义为 reset，已文档化）；未实现 formatOtherKeys=1 的 `CSI code;mod u` 变体；F21–F24 用 CSI 42–45 |
| KEY-012 | kitty repeat/release 事件（press=1 / repeat=2 / release=3） | U-XTERM / 需求 C1 | Slint 增加 `key-release` 回调、`key-input` 透传 `event.repeat`；`handle_input`/`handle_key_release` 用 held-key 登记表（按 pane + KeyId）只回放“press 真的进过 PTY 且以转义序列发送”的键；仅 REPORT_EVENT_TYPES 开启时发送；普通 shell、`off`、搜索框、快捷键录制中一律不发 | implemented | app `kitty_event_types_repeat_release`、`key_identity_matches_press_and_release`；Xvfb :123 kitty 场景实测 Ctrl+A press+release=`CSI 97;5u` + `CSI 97;5:3u`、Shift+Up press+release=`CSI 1;2A` + `CSI 1;2:3A`；plain/off 无多余字节 | press/repeat/release 事件类型正确，且不会把快捷键/搜索框的 release 写入 PTY | 纯文本键的 release 只在 REPORT_ALL_KEYS 下发送（避免与文本 press 不配对）；修饰键本身仍按伪字符过滤，不上报 modifier release；caps/num 不可见 |

### C10. Themes / Color Schemes / Appearance（7 条）

状态：partial=6，needs_verification=1，not_started=0。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| THEME-001 | 应用 UI Theme：明暗、强调色、控件样式 | U-THEME / 需求 C10 | themes.rs 12 套 ThemeData；palette.rs UiColors::derive；main.rs `push_theme` 写 Slint Palette | partial | palette.rs 7 例；themes.rs 12 例；人工 12 套截图 | 12 套内置 + Auto + UI chrome 同步 | 无自定义强调色/控件样式；UI 主题非 CSS 可扩展 |
| THEME-002 | Terminal Color Scheme：16/256/True Color、前景/背景/光标/选区颜色 | U-THEME / 需求 C10 | palette.rs PaletteSpec/Palette；themes.rs 导入格式；render.rs 选区用反色，无独立 selection 色 | partial | `sgr_256_and_truecolor`；主题导入 6 例 | 16/256/truecolor + fg/bg/cursor 可用 | 无独立 selection/selectionForeground 配置；选区固定反色 |
| THEME-003 | Base16 兼容 | U-THEME / 需求 C10 | themes.rs 支持 Xresources/kitty 等 16 色导入（Base16 常见载体）；无 base16 语义/元数据 | partial | `imports_xresources`、`imports_kitty` | 可消费 Base16 风格 16 色文件 | 无 Base16 命名/校验/调色板映射保证 |
| THEME-004 | 内置 + 导入社区色彩方案；核实上游 150+ 数量 | U-THEME / 需求 C10 | 12 套内置；ThemeRegistry 扫描目录支持 iTerm2/WT/Alacritty/kitty/Xresources/YAML | partial | themes.rs 12 例；人工导入 .logs/imported_settings4.png | 内置 12 + 导入不限量 | 未内置 150+；上游当前数量未核实；改文件需重启 |
| THEME-005 | 字体、fallback、ligatures、Powerline、Nerd Fonts、emoji | U-THEME / 需求 C10 | glyph.rs 固定 JetBrains Mono/DejaVu + CJK 回退；彩色 emoji 默认开启（Noto Color Emoji CBDT/CBLC/sbix + `XTERVO_FALLBACK_FONTS`，独立 RGBA 缓存）；单色回退/占位兜底 | partial | glyph.rs 6 例（彩色解码/缓存/缩放/越界/CJK 惰性）+ render 3 例；人工中文/粗体；Xvfb 截图 `.logs/font-emoji/color-after.png` | CJK fallback 与彩色 emoji 可用 | 无自选字体/连字/Powerline；Nerd 仅常见路径；COLR/SVG emoji 仍不支持 |
| THEME-006 | 窗口背景/透明/模糊仅在平台支持时启用；不支持时回退 | U-THEME / 需求 C10 | config `background_image`（空=关；`~` 展开；只接受存在普通文件）+ `background_opacity`（0–1）+ `background_dim`（0–0.8，默认 0.6，写盘两位小数）；main.slint 最底层 Image（cover）+ 黑色 dim 遮罩 + 有图时内容区/pane 透明；render.rs 默认背景**直通 RGBA** alpha=opacity（`from_rgba8`，避免二次预乘），文字/光标/彩色 cell 恒 alpha=255；pane 帧图显式 x/y=0，右侧/底部余量用 `bg.with-alpha(opacity)` 补条；无图强制 alpha=1；主题切换/字号重建保留透明度；设置页即时生效 | partial | 213 passed（config roundtrip/clamp/旧配置/路径/`~`/dim + render alpha/直通/文字不透明 + 双缓冲脏帧）；Xvfb :104 Skia/llvmpipe：背景 cover/暗化/透明度截图 `.logs/gpu-func/01-start.png`、最大化/恢复后 `.logs/gpu-func/12-maximized.png`、resize 后 `.logs/gpu-func/15-after-resize.png`；大面积窗口拖动期间背景临时隐藏（A/B 实测每帧省约 2ms），停下恢复 | 纯色或背景图 + 终端默认背景透明可用；暗化保证白底文字可读；坏路径回退并提示；TabBar/设置不透明；GPU 路径已走查 | 无模糊；窗口本身不透明（无桌面透传）；无每主题独立背景；大窗口拖动期间背景短暂隐藏；只有默认背景透明（彩色 cell、文字/光标保持不透明）；大图同步解码可能短暂卡顿；设置页极矮窗口可能裁剪 |
| THEME-007 | 旧 Tabby CSS Theme 转 Slint 的兼容分级或 WebView 适配 | U-THEME / 需求 C10 | 无 CSS 解析/WebView；docs/tabby-themes.md 已说明 CSS 依赖 Angular DOM/Bootstrap | needs_verification | 待定兼容策略；无实现 | 未核实/未实现 | CSS 不能直接映射 Slint；可选 WebView 层未做，需分级 |

### C11. Progress / Notification / Smart Tabs（6 条）

状态：implemented=2，partial=3，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| NOTIFY-001 | 检测终端进度信号（按上游实际协议审计） | U-AUDIT（上游协议待核） | `core/osc.rs` 流式 OscScanner（alacritty/vte Handler 无 osc_dispatch，actor feed 旁路扫描）：OSC 9;4 进度 0..4、OSC 9 消息、OSC 777;notify、BEL、OSC 7 cwd；跨 chunk/ST/BEL 终止 | implemented | core osc 模块 12 例（状态/缺百分比/坏值/超长/分片）；smoke `osc_progress_notification_and_bell_are_delivered` | 上述协议均可解析并转 CoreEvent；非法/越界/超长忽略，不 panic | 未支持 OSC 633；上游 Tabby 具体进度协议未核实；BEL 在 DCS/CSI 内不响 |
| NOTIFY-002 | Tab 显示进度或活动状态 | U-AUDIT / 需求 C11 | `app/activity.rs` 按 Pane 聚合（错误>进行中>暂停>不确定）+ `main.slint` TabView 进度条/百分比/“!”/“✓”/“●” | implemented | `activity::aggregate_*`、`progress_ratio_and_label`；app 102 测试通过 | Tab 可见进度条、百分比、错误/完成/未读标记 | 不确定进度无动画；无连接状态/无障碍标签 |
| NOTIFY-003 | 进程/命令完成通知；通知策略与静音 | U-AUDIT / 需求 C11 | pane.rs 发 Progress/Notification/Bell；main.rs `EventSink` 事件 wake → `drain_core_events` 立即消费，400ms `poll_events` 兜底；非活动 Tab 调 `notify-send`；`desktop_notifications` 默认 true（Settings→Terminal）；视觉铃 `visual_bell` 默认 true（2px 边框/180ms）；可听铃 `audible_bell` 默认 false，X11 `Bell` 请求 + stderr tty BEL 回退，与视觉铃独立、后台 Tab 的 BEL 也响 | partial | `sanitize_notification_text`、`notification_content`、config 默认值/旧配置单测；smoke 事件测试；Xvfb :115 30fps 抓帧检测视觉铃；Xvfb :118 每 250ms BEL → `[bell] audible backend=ok`（visual_bell=false 场景）；`scripts/e2e_event_wake.sh` BEL 0ms（400ms 轮询 291ms） | OSC 9/777/BEL 桌面通知与 Tab 未读点可用；语义事件 <50ms；视觉铃/可听铃独立且可关 | 无命令结束自动检测；无按事件静音/限流；X11 bell 是否出声依赖桌面声音主题，无音量/音色/每 Tab 静音；同一轮 BEL 合并一次响铃；视觉铃未跟随 reduced motion |
| NOTIFY-004 | 后台 Tab 有活动/完成时标记并可跳转 | U-AUDIT / 需求 C11 | `set_active_tab_inner` 切 Tab 清除 attention/完成；未读点/完成标记由 TabView 显示；点 Tab 即可跳转 | partial | `aggregate_ors_attention_and_completion`；app 102 测试通过 | 后台 Tab 有未读/完成标记，切过去清除 | 无 smart-tab 列表/自动跳转；无“最近活动”排序 |
| NOTIFY-005 | 桌面通知权限、失败、平台差异处理 | U-AUDIT / 需求 C11 | `notify-send` 参数直传（不经 shell、`--app-name=xtervo`、stdio 置空、后台线程回收）；命令不存在静默失败 | partial | `notification_content` 清洗/限长单测 | 有桌面通知集成与静默降级，配置可关 | 未检测 DBus/通知权限，无平台回退；无点击跳转 |
| NOTIFY-006 | 插件可注册进度/通知提供者，不能伪造核心会话状态 | U-AUDIT / 需求 C11 | 无实现 | not_started | 无 | 未实现 | 无插件 API/通知总线 |

### C12. Vault / Secrets / Security（6 条）

状态：partial=5，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| SEC-001 | 加密保存 SSH 密码、私钥口令及其他敏感配置 | U-AUDIT / 需求 C12 | `vault.rs` Argon2id + ChaCha20-Poly1305；Profile `auth=password`+save_password；**W-02** initialize/unlock/change_master 后台线程；worker 主密码/AEAD 明文/Unlocked Drop 尽力清零 | partial | vault 13 例（含 5 例后台任务）；GUI busy/完成截图；日志/vault.enc 不含主密码 | SSH 登录密码可加密保存并用于 askpass，KDF 不阻塞 UI | 仅 SSH 登录密码；无 keyring/私钥口令；未 mlock，残留内存副本风险仍在 |
| SEC-002 | Master Passphrase / Vault 解锁与锁定（按原版核验） | U-AUDIT / 需求 C12 | `vault.rs` 提供 initialize/unlock/lock/change_master；设置 → Vault 页可操作；启动默认锁定，锁定时 get/set/remove 返回错误；Clear 需二次确认 | partial | vault 单测覆盖锁定读写/改主密码/重开解锁；`cargo build --release` | 主密码解锁/锁定/改密可用 | UI 未真机验证；无空闲自动锁定、无解锁弹窗、无失败次数退避 |
| SEC-003 | 优先 Linux Secret Service/Keyring；加密文件回退说明 KDF 风险 | U-AUDIT / 需求 C12 | 未接 Secret Service/Keyring；Argon2id 19 MiB/t=2/p=1、0600、原子写；**W-02** KDF 后台线程 + busy 状态；docs/VAULT.md 写明风险 | partial | vault 单测（权限/无明文/后台任务）；文档走查 | 无 Keyring 时加密文件后端可用且解锁不冻结 UI | 未优先/未实现 Keyring；无后端口抽象 |
| SEC-004 | 普通配置、日志、崩溃报告不得出现明文密码/私钥/token | U-AUDIT / 需求 C12 | config.rs/profiles.toml 均无 secret 字段；Profile 只存 `save_password` 标志；Vault 文件只含 base64 密文；日志只记 Profile id/kind、不打印密码；askpass 密码仅存在于 ssh 子进程环境 | partial | `file_never_contains_plaintext_and_permissions_are_private`、`profile_serialization_has_no_plaintext_password`；代码走查 | 普通配置/日志无明文凭据 | askpass 环境变量可被同用户经 `/proc/<pid>/environ` 观察；无结构化脱敏框架/崩溃报告；config/profiles 文件权限未统一 0600 |
| SEC-005 | 插件默认不能读凭据；使用需 capability | U-AUDIT / 需求 C12 | 无实现；`docs/PLUGIN_API.md` 明确插件是普通子进程、拥有当前用户全部权限、无沙箱/无 capability，同用户可读 `~/.ssh`、config、日志与 `vault.enc` 密文 | not_started | 无 | 未实现 | 无插件运行时隔离/权限模型；MVP 插件 API 不是安全边界 |
| SEC-006 | 主机密钥安全、文件权限、本地 IPC 权限检查 | U-AUDIT / 需求 C12 | `vault.enc` 以 0600 创建并在原子替换前 chmod；临时 askpass 目录/脚本 0700 且 Drop 清理；askpass 密码只进 ssh 子进程环境、脚本/日志/文件均不含密码；`XTERVO_ASKPASS_DEBUG` 仅记录 prompt 与匹配分支（无密码）；主机密钥仍完全交系统 ssh；known_hosts 判定用 `ssh -G` 解析实际 `hostname`/`port`/`HostKeyAlias`，未确认 host 不使用 askpass；无本地 IPC | partial | vault 文件权限测试、askpass 目录/脚本权限测试、`applied_envs_reach_real_child_process`、`debug_file_records_prompt_without_secret`、`credential_is_keyed_by_profile_store_id`、`host_key_known_matches_nonstandard_port_entry`、`ssh_keygen_matches_bracketed_port_spec`、`host_key_known_uses_resolved_hostname_from_ssh_config`、`host_key_known_uses_host_key_alias_verbatim`；未知 host 强制 askpass 只回答一次 `no` 后 rc=255；GUI 真机后 grep app 日志/askpass 调试文件/profiles.toml/vault.enc 均无明文密码，vault.enc 0600 | vault 凭据文件与 askpass 临时文件权限已收紧；known_hosts 查找键与 ssh 一致；GUI 密码链路无密码落日志 | 配置文件/profile 文件未显式 0600；无内建 known_hosts 管理/本地 IPC 权限检查；密码仍经子进程环境变量传递（`/proc/<pid>/environ` 可见，后续改 fd/pipe）；`XTERVO_ASKPASS_DEBUG` 会记录 prompt 原文，需用户显式开启并自行清理 |

### C13. Plugin System 与旧插件兼容（13 条）

状态：partial=5，not_started=7，needs_verification=1（PLUG-006 于
2026-11-12 只落地窄 WASM `terminal.output_filter`，其余 WASM Host API 未做）。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| PLUG-001 | 稳定、版本化、具备生命周期的 Plugin API | U-PLUGIN / 需求 C13 | 两种 api=1 运行时：oneshot（旧：每命令/事件一行 JSON 子进程）+ process（V1：2s 握手、JSON Lines、命令结果、状态、事件通知、10s 超时 kill、崩溃重启上限 2、坏 JSON 禁用、宿主退出 shutdown+PDEATHSIG）；设置页显示 running/restarting/disabled 原因 | partial | `process_plugin_handshakes_runs_commands_and_events`、`node_process_plugin_command_result`、`crashing_process_plugin_restarts_then_disables`、`command_timeout_kills_process_and_reports_timeout`、`bad_json_disables_plugin_without_hanging_host`、`handshake_timeout_is_reported_and_retried`、`process_plugin_api_mismatch_is_disabled`、oneshot 既有测试；341 app tests；Xvfb :142 e2e | 版本化外部进程 API（oneshot + 常驻 process）可用 | 不兼容 Tabby/JS；无任意事件订阅/卸载/热重载；api 只支持 1；无 WASM/WIT |
| PLUG-002 | 扩展点：Commands/Hotkeys/Toolbar/Settings/Profile Providers/Tab 菜单/终端菜单/Decorator/Stream Processor/Middleware/Color Scheme/Linkifier | U-PLUGIN / 需求 C13 | Commands + Hotkeys（Palette/默认键/override/静态 args/返回文本）+ Status Provider（底部状态栏）+ Theme Pack（TOML）；生命周期事件 `startup`/`tab-open`/`tab-close`；其余扩展点无接口 | partial | 命令注册/参数/结果/状态轮询/主题/状态栏点击 e2e；事件 payload/触发过滤单测 | 命令、快捷键、状态、主题、生命周期事件扩展点可用 | Toolbar/Settings/Profile Provider/Tab/终端菜单/Middleware/Linkifier 等未实现；无任意事件订阅 |
| PLUG-003 | 注册/注销时管理资源、事件订阅、异步任务 | U-PLUGIN / 需求 C13 | process runtime：每插件 supervisor 线程管理常驻子进程、pending 请求、状态轮询、重启/禁用与 shutdown；oneshot 每次后台线程 + 并发 stdout/stderr reader；宿主 Drop 发 shutdown/kill；无热注销/重载 API | partial | 进程命令/事件/状态/崩溃/超时/坏 JSON/禁用测试；`shutdown_processes` 清理；无泄漏断言（线程退出由 EOF/shutdown 驱动） | 常驻生命周期、单次执行资源回收、三种事件分发可用 | 无热注销/热重载/自定义事件订阅/单插件并发上限；运行中不能卸载 |
| PLUG-004 | 权限声明：terminal/file/network/clipboard/secret/process 分权 | U-PLUGIN / 需求 C13 | `[permissions]` filesystem/network/process/clipboard/credentials 参与解析/校验并在设置页展示“仅声明/未授予”；默认拒绝未实现能力（不提供 API），但插件仍是普通子进程、无沙箱/无强制撤销；不传 Vault 明文，事件 payload 不含凭据 | partial | 权限解析/展示/非法值跳过单测；PLUGIN_API §9 安全边界 | 权限声明与展示可用，默认不授予未实现能力 | 无沙箱/seccomp/capability 强制；同用户仍可读文件/网络/进程；不应把插件 API 当安全边界 |
| PLUG-005 | 插件崩溃隔离、超时、日志、升级与版本兼容检查 | U-PLUGIN / 需求 C13 | 常驻进程独立 supervisor/子进程：2s 握手、10s 命令超时 kill、崩溃重启上限 2 后禁用、坏 JSON/超长行 5 次禁用、api 不匹配/非法 manifest 跳过；stdout/stderr/协议错误写 `[plugin]` 日志；活动列表 + 设置页 disabled 原因 + 底部浮层；版本兼容精确 `api=1` | partial | `crashing_process_plugin_restarts_then_disables`、`command_timeout_kills_process_and_reports_timeout`、`bad_json_disables_plugin_without_hanging_host`、`handshake_timeout_is_reported_and_retried`、`process_plugin_api_mismatch_is_disabled`、`read_line_capped_*`、oneshot 既有超时/退出码测试；Xvfb :142 e2e | 崩溃隔离/自动重启/超时/坏 JSON/API 检查/诊断列表可用 | 无升级/迁移/手动重启/完整结构化诊断 UI；活动仅 8 条；超时固定、无单插件并发上限 |
| PLUG-006 | 新插件优先 WASM/WIT（先定义 Host API 再引 Runtime） | U-PLUGIN / 需求 C13 | `runtime = "wasm"` + Wasmtime 34：无 import、窄 ABI（memory/alloc/filter）、fuel/epoch/内存/输入输出上限、trap 原样回退；只支持 `terminal.output_filter`，真实 PTY e2e | partial | `plugin_wasm`、`plugin_v3::tests::wasm`、`output_filter`；2m soak | 窄 WASM 输出过滤器可用 | 无 WIT/组件模型/任意 WASM 命令/UI/WASI；无市场/签名/AOT |
| PLUG-007 | 扫描原插件包与 API 使用情况；不承诺全量兼容 | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无扫描器；未建立兼容分级 |
| PLUG-008 | 识别 package.json `tabby-plugin` keyword、入口、依赖、版本 | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无 package.json 解析；keyword 事实见 docs/tabby-themes.md |
| PLUG-009 | Node.js sidecar 加载旧 JS/TS 插件，RPC 与 Rust Host 通信 | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无 Node 依赖/sidecar/RPC |
| PLUG-010 | 兼容包/代理对象：映射 tabby-core、tabby-terminal、tabby-local 常用接口 | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无 shim/代理层 |
| PLUG-011 | 兼容核心非 UI：Commands/Hotkeys/Config/Color Schemes/Middleware/Profile Providers/Notifications | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 宿主侧这些能力本身多数未实现 |
| PLUG-012 | Angular Component/Template/DI 不可直接执行；仅按需可选 WebView UI Host | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无 WebView；默认不启浏览器运行时的边界已明确 |
| PLUG-013 | 依赖 Electron 私有 API/原生 Node 模块/未公开 API 的插件逐个评估并标记不支持 | U-PLUGIN / 需求 C13 | 无实现 | needs_verification | 未逐个评估 | 未核实/未实现 | 无评估清单；见 COMPATIBILITY_MATRIX.md 分级 |

### C14. Logging / Diagnostics / Update / Import-Export（6 条）

状态：partial=4，not_started=2。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| OPS-001 | 结构化日志，按模块/Session 关联；凭据自动脱敏 | U-AUDIT / 需求 C14 | main.rs `env_logger::init`；无 log:: 调用；大量 eprintln 带 pane id；无脱敏 | partial | 代码走查；无日志格式/字段测试 | 仅 stderr 文本日志 | 非结构化、无 session 关联、无脱敏、无轮转 |
| OPS-002 | 连接诊断：DNS/TCP/SSH 握手/认证/Host Key/Channel 阶段定位 | U-AUDIT / 需求 C14 | 无实现 | not_started | 无 | 未实现 | 无 SSH/连接诊断 |
| OPS-003 | 插件诊断：加载失败、API 版本冲突、权限拒绝、超时、崩溃 | U-AUDIT / 需求 C14 | 加载失败/非法字段写 `[plugin] 跳过` + 设置页 skipped；api 不匹配/坏 JSON/超时/崩溃/重启上限显示 disabled 原因；活动列表与日志；权限拒绝无实现（无强制） | partial | api mismatch/坏 JSON/崩溃/超时/非法清单单测；Xvfb :142 crash 插件 disabled 截图 | 加载/API/超时/崩溃诊断可用 | 无权限拒绝诊断（无权限强制）、无升级/迁移、无结构化诊断面板 |
| OPS-004 | 配置导入、导出、备份、恢复 | U-AUDIT / 需求 C14 | config.rs 迁移前 `config.toml.v<old>.bak` 备份 + 原子写；无导入/导出/恢复入口 | partial | config.rs `old_config_is_backed_up_and_migrated` | 迁移备份可用；导入导出/恢复缺失 | 只有迁移前自动备份；无用户主动备份/恢复/导出 |
| OPS-005 | 版本信息、更新检查、更新失败反馈；Linux 包管理适配 | U-AUDIT / 需求 C14 | `--help` 打印 CARGO_PKG_VERSION；scripts/package.sh/install.sh；无更新检查 | partial | 人工 `--help`；打包脚本记录 | 版本信息/打包可用 | 无更新检查/失败反馈/自动更新策略 |
| OPS-006 | CLI 参数与 URI/命令入口（如启动 Profile/新 Tab，按上游核验） | U-AUDIT / 需求 C14 | main.rs `parse_args`：--cols/rows/scrollback/font-size/shell/latency(-l)/fps/title/restore 等；`window-title` 属性透传；无 URI/Profile | partial | 真机：`--title SELFTEST` → `xprop _NET_WM_NAME` 输出一致；`-l` 与 `--latency` 等价 | 2026-11-05 实测通过 | 无 URI/Profile 启动；无 --help 自动生成 |

### C15. 平台差异与明确排除项（0 条）

C15 无 `[ ]` 复选框条目；平台差异说明见 §5（不计入 244 条）。

## 5. 附：补充条目（非复选框，不计入 244 条）

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-SET-000 | Settings 可搜索、可导航的设置布局（B7 引言） | U-AUDIT / 需求 B7 | 设置面板：6 分类导航 + 搜索框（`settings_search` 目录匹配 + 结果浮层点击跳转分类） | partial | 单测 `settings_search_matches_labels_and_keywords`；真机：输入 copy → `settings filter hits=1`，点结果 → `settings goto cat=1`（.logs/settings-search-jump.png） | 分类导航 + 搜索可用 | 搜索基于 10 条精选目录而非全部控件；无高亮/无键盘上下选择 |
| UI-SET-001 | Settings / 菜单界面语言（English / 中文）与运行中热切换 | 需求 B7 + i18n 补充 | Slint 全局 `Lang.t(en, zh)`（pure function，绑定自动重算）；Appearance 顶部 Language 分段；`Config.language = "en"|"zh"`（默认 en，未知回退 en，additive）；Settings 静态文案、搜索索引/结果、终端右键菜单/TabBar 菜单、Profiles 面板/表单双语；Rust 动态状态（hotkey/vault/plugin/config/profile default/theme display）跟随语言 | implemented | `cargo test -p xtervo-app --bin xtervo`（376 passed / 1 ignored，基线 369）；`scripts/e2e_settings_i18n.sh`（Xvfb :126，源码断言 + 23 张截图；live en↔zh、config 写回、外部热更新、右键/TabBar、小窗口、重启、legacy） | en/zh 双向切换与持久化 PASS；截图见 `.logs/settings-i18n/shots/` | Rust 校验错误/known_hosts 弹窗等少数动态错误文案仍可能中文；插件自身名称/描述不翻译；语言选项必须同时展示 English/中文 |
| PLAT-001 | Ubuntu X11/Wayland：全局快捷键、窗口定位、边缘停靠能力检测与降级 | U-AUDIT / 需求 C15 | X11 路径已验证（XTest/XDamage/X11 图标）；Wayland 无能力检测/回退代码；GPU 后端已做自动降级（无 DRM→software；Skia 1s 无 GPU API→exec software） | partial | phase0 X11 记录；Xvfb :104 无 DRM 默认 software；强制 GL 失败自动 exec 回退；无 Wayland 验收 | X11 可用；GPU 回退链路可用；Wayland 未验证 | Wayland 全局快捷键/定位/停靠均未实现；真机 NVIDIA 未验证；无 Wayland 运行时能力探测 |
| PLAT-002 | Windows 后续：PowerShell/WSL/Git Bash/Cygwin/MSYS2/Cmder/CMD/Clink 等 | U-AUDIT / 需求 C15 | 无实现 | unsupported | 无 | 明确排除（Ubuntu 第一阶段） | 不在当前交付范围；不得伪造支持 |
| PLAT-003 | macOS 后续：Vibrancy、Keychain、平台 Shell/键盘 | U-AUDIT / 需求 C15 | 无实现 | unsupported | 无 | 明确排除（Ubuntu 第一阶段） | 不在当前交付范围 |
| PLAT-004 | Tabby Web App/self-hosted 不混入桌面版完成度 | U-AUDIT / 需求 C15 | 无实现 | unsupported | 无 | 明确排除 | 无 Web 版目标/代码 |
| PLAT-005 | Tabby 不是 Shell 本身；通过 PTY 启动系统 Shell | U-AUDIT / 需求 C15 | pane.rs `spawn` 用 portable-pty 启动 $SHELL/config.shell；不实现 Bash/Zsh | implemented | smoke PTY 测试；人工启动 bash/zsh | 已按此边界实现 | 无内置 shell/补全；依赖系统 shell |
| TERM-020 | DCS/Sixel 业务解析：跨 chunk、BEL 终止、非法/超限丢图、DA1 能力宣告 | U-AUDIT / 需求 C1 | `core/sixel.rs` 独立状态机过滤 `ESC P ... q ... ST/BEL/C1 ST`，跨 chunk 保留 ESC/DCS 头；非 Sixel DCS 原样交还 vte；解码支持 RGB/HLS、透明、`!Pn`、raster attributes，原始 25 MiB / 16 MP / 单边 16384 上限；DA1 回应在 Listener 精确替换为 `CSI ?62;4;9;22c`，关闭/二级 DA 不改写 | partial | core 单测 18 例 + PTY 集成 6 例；DA1 真机截图 on/off；dsh-tui 启动截图 + 用 dsh 自己的 `selectTerminalImageProtocol([62,4,9,22])` 输出 `sixel` | 常见 Sixel 编码器输出可解码；BEL 终止不会污染解析器；超限/畸形只丢图；dsh 的 DA1 选择路径成立 | 未实现 HLS 以外的其它色彩空间差异/调色板重映射；未做 fuzz；未验证超大真图内存峰值（IIP 见 UI-TERM-021） |

## 6. 维护规则

- 新增/修改功能时先改本矩阵，再改代码；`implemented` 必须附测试命令或可重复人工步骤。
- 上游结论必须回填 `TABBY_UPSTREAM_AUDIT.md`；未复核前保持 `U-AUDIT`，不得改写成“Tabby 已有”。
- 每条 `needs_verification` 在完成上游审计后必须收敛为 `implemented/partial/not_started/unsupported` 之一。
