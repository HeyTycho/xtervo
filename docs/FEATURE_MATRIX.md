# Tervo 功能矩阵（FEATURE_MATRIX）

> 审计日期：2026-11-05 ｜ 仓库：`/home/yt/AIworkspace/tervoterminal`（起始 HEAD `0ea6231`，复核到并发提交 `8327e20`） ｜ 审计方式：只读代码 + 既有文档 + 静态走查；**未执行 cargo build/test**（同仓另一 Agent 在构建）。
> 上游网络不可用（2026-11-05，GitHub/docs.tabby.sh DNS 解析到非公网地址），上游行为未能复核；见 [`TABBY_UPSTREAM_AUDIT.md`](TABBY_UPSTREAM_AUDIT.md)。
> 状态分布与闭环结论见 [`CLOSURE.md`](CLOSURE.md)；统计脚本 `python3 scripts/matrix_stats.py`。

## 0. 覆盖与状态口径

- 并发说明：审计期间另一 Agent 提交了 `b80e265`（config schema/迁移/备份、字体惰性加载等），本矩阵已按该提交复核 config 相关行；此后 `94baefb`/`99d7895` 加入行内链接识别与 OSC 8 透出、`8327e20` 加入命令注册表，均已复核对应行（TERM-012/UI-TERM-011/C9）。工作区其余未提交改动未纳入。
- 覆盖需求文档 B、C 两部分的**全部 228 个 `[ ]` 复选框**：B 部分 110 条，C 部分 118 条；另附 B7 引言 1 条、C15 平台说明 5 条（标 `extra`，不计入完成度）。
- `implemented` 仅表示：用户可见路径存在、非 mock、错误路径基本处理、有既有测试或可重复人工验收记录、文档已更新。本轮**未复跑**任何测试，Result 列引用既有记录并注明。
- `partial`：已有可用子集，但存在明确缺口。`needs_verification`：上游行为/触发方式/协议本身未核实，或本仓库行为未经查证。`not_started`：无实现。`unsupported`：明确排除（平台/外部工具）。
- Upstream evidence 列缩写：`U-AUDIT` = 本轮网络不可用，上游未核实（不代表上游没有该功能）；`U-XTERM` = Tabby 默认 xterm.js 前端（转引 `docs/tabby-themes.md`）；`U-THEME` = 已核主题 API（`tabby-core/src/services/themes.service.ts`、`api/theme.ts`，转引）；`U-PLUGIN` = 已核 npm 插件结构（keyword `tabby-plugin`，转引）；`U-CONF` = 已核 `config.yaml`/`config.service.ts`（转引）；`U-REQ` = 仅需求文档描述。

## 1. 状态计数与完成度

| 范围 | 条目 | implemented | partial | in_progress | needs_verification | not_started | unsupported | 加权完成度 |
|---|---|---|---|---|---|---|---|---|
| B 部分（UI） | 110 | 11 | 24 | 0 | 11 | 64 | 0 | 20.9% |
| C 部分（底层） | 118 | 7 | 32 | 0 | 10 | 67 | 2 | 19.5% |
| B+C 合计 | 228 | 18 | 56 | 0 | 21 | 131 | 2 | 20.2% |

> 加权公式：`(implemented + 0.5×partial + 0.5×in_progress) / 条目总数`。`needs_verification / not_started / unsupported` 记 0。该数字是“功能条目覆盖度”，不是代码行数或工时完成度。

## 2. 审计发现：现有实现/文档与需求不符（至少 3 条）

1. **SSH/Serial 依赖“已锁定”不实**：`docs/tabby-parity.md` 写“依赖已锁定 russh / serialport”，但 `Cargo.toml`、`crates/*/Cargo.toml` 与 `Cargo.lock` 均无 `russh`、`serialport`、`tokio-serial`，也无相关模块。
2. **会话恢复被写成 ✅ 过度**：`docs/tabby-parity.md` 称“重启后原样拉起”；实际只有菜单 `Restart Session` 或手动 `--restore` 才恢复布局+`cwd`，正常启动**不会**恢复，也不恢复运行中进程/滚动历史。C8-004 应为 `partial`。
3. **Tab OSC 标题并非完整跟随**：`docs/tabby-parity.md` 标 ✅；`main.rs::poll_events` 只把**活动 Tab 的首个 pane**标题写回 `tab.title`，后台 Tab、非首 pane 不更新，且无前缀/后缀/过滤。B2-008 应为 `partial`。
4. ~~**全屏没有实现**~~ 已修（2026-11-05）：F11 与 `--fullscreen` 走 `Window::set_fullscreen`，实测 2560x1440 满屏，UI-WIN-006 升为 implemented。
5. ~~**Pane 焦点没有视觉状态**~~ 已修（2026-11-05）：`main.slint` 按 `focused` 画 1px accent 边框，UI-PANE-004 升为 implemented。
6. **鼠标 1002 拖动按钮编码错误**：`main.rs::on_pane_mouse_move` 在转发拖动时硬编码 `MouseButton::Left`；按住右键/中键拖动（1002）会被当成左键上报。鼠标上报文档只验证了左键/滚轮。
7. **OSC 52 / 颜色查询 / 尺寸查询无响应**：`pane.rs` 的 `Listener::send_event` 对 `Event::ClipboardStore/Load`、`ColorRequest`、`TextAreaSizeRequest` 走 `_ => {}`；应用查询终端颜色/尺寸时拿不到回复。
8. ~~**CLI 与实际不符**~~ 已修（2026-11-05）：`-l` 与 `--latency` 等价，`--title` 通过 `window-title` 属性真正写入窗口标题（OPS-006）。
9. **稳定 ID 未全面落实**：需求 A3 要求 Tab/Pane/Session/Profile/Plugin 都用稳定 ID；`workspace::Tab` 只有 `Vec` 下标，`session::Snapshot` 无 ID，`PaneId` 是进程内自增（重启后重排），Profile/Plugin 尚无模型。
10. **Session 未独立于 Pane 生命周期**：需求 A3 要求 Session 独立；当前 `PaneHandle` 同时拥有 actor/PTY/child，关闭 Pane/Tab 即 Drop 并 kill 子进程，无法“仅关视图/保留后台 Session”。（2026-11-05 已补关闭确认，见 UI-WIN-008，仅缓解误关，未解决生命周期解耦）

## 3. B 部分：UI 页面、控件和用户操作

### B1. 主窗口与自绘标题栏（9 条）

状态：implemented=5，partial=3，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-WIN-001 | 无系统装饰窗口，不显示传统标题栏 | U-AUDIT / 需求 B1 | main.slint `no-frame: true`；main.rs `MainWindow::new` + `set_size` | implemented | 人工：启动后窗口顶部直接是 TabBar；docs/ui-shell.md §2/§7 .logs/nf.png；真机加 3 个 Tab 窗口宽度保持 540（Tab 条在 Flickable 里） | 已有记录（2026-11-05） | 依赖 winit/X11；Wayland 服务端装饰未验证 |
| UI-WIN-002 | 顶部自绘 TitleBar/TabBar：图标、Tab、新建、窗口按钮 | U-AUDIT / 需求 B1 | main.slint TabBar Rectangle：品牌 Image、tabs、+、⚙、─/□/✕ | implemented | 人工：核对 5 类控件可见可点；.logs/t4.png | 已有记录（ui-shell §2/§7） | 按钮无 tooltip/accessible label；无系统菜单 |
| UI-WIN-003 | 标题栏空白区拖动；按钮/Tab/输入区不误触发 | U-AUDIT / 需求 B1 | main.slint `WindowMoveArea` + `title-hit`；按钮/Tab 为后置 TouchArea | implemented | 人工：拖空白区移动窗口；拖按钮/Tab 不移动 | 已有记录（ui-shell §2） | 无自动化；依赖 Slint 命中顺序 |
| UI-WIN-004 | 最大化/还原/最小化/关闭；双击标题栏 | U-AUDIT / 需求 B1 | main.rs `on_toggle_maximize`/`on_minimize_window`/`on_close_window`；main.slint 双击 title-hit | implemented | 人工：─/□/❐/✕ 与双击空白区；ui-shell §3 | 已有记录（ui-shell §3） | 关闭无确认；双击只在空白区，Tab 上双击无效 |
| UI-WIN-005 | 窗口尺寸变化、DPI/Scale Factor、主题切换 | U-AUDIT / 需求 B1 | main.rs `on_resized`/`refresh_views`；main.slint 8 个 ResizeHandle；`push_theme` | partial | 人工：拖边缩放 + 切主题；DPI 未测（代码无 scale_factor 分支） | 缩放/主题已有记录；DPI/缩放因子未验证 | 物理像素假设；无 scale_factor 监听与跨屏 DPI 复测 |
| UI-WIN-006 | 全屏切换（F11/快捷键/命令） | U-AUDIT / 需求 B1 | `handle_input` F11 → `toggle_fullscreen(st)`；命令面板 `view.toggle-fullscreen` 同一实现；`--fullscreen` 启动参数 | implemented | 真机：F11 → `[ui] fullscreen=true`（2560x1440），再按 → `false`（540x472），无 panic；`--fullscreen` 启动验证过 | 2026-11-05 实测通过 | 全屏切换依赖 WM；Wayland 下由合成器决定 |
| UI-WIN-007 | TabBar 位置可配置：上/下/左/右 | U-AUDIT（上游支持方向待核） | config `tabbar_position`（top/bottom）+ `tabbar-bottom` 属性；根布局改绝对定位；TabBar 菜单可切换并持久化 | partial | 真机：菜单切 bottom → `[ui] tabbar_position=bottom`，窗口底部出现 TabBar；重启日志 `启动 tabbar_position=bottom`；右键菜单锚在 TabBar 上方（y=224） | 上/下可用且持久化 | 左/右未实现（竖排 TabBar 需另做布局）；上游支持方向待核 |
| UI-WIN-008 | 关闭窗口前对活动任务/连接确认；不误杀应保留 Session | U-AUDIT / 需求 B1 | `request_close`/`needs_close_confirm` + 确认弹窗；`confirm_close`（默认 true）；Tab/Pane 关闭支持 `close_action=background`（保留会话，见 UI-TAB-005） | partial | 单测 `close_confirm_policy`；真机关闭确认/Cancel；“关 Tab 保留会话”另有 pid 不变证据 | 关窗确认 + 会话保留策略均可用 | 退出应用仍会结束后台会话；确认弹窗未列出具体会话名；close_action 无设置页入口 |
| UI-WIN-009 | 多窗口：新建窗口、Tab 在窗口间打开/移动 | U-AUDIT / 需求 B1 | 无实现 | not_started | 无 | 未实现 | 单进程单窗口；无窗口管理器/实例间 IPC |

### B2. TabBar 操作（12 条）

状态：implemented=2，partial=9，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-TAB-001 | 新建 Tab：+、快捷键、命令面板、Profile Selector | U-AUDIT / 需求 B2 | main.rs `new_tab_inner`；Ctrl+T；`+` 按钮；Command Palette 执行 `tab.new` | partial | 真机：命令面板执行 `tab.new` → 新建 Tab | `+`/Ctrl+T/命令面板三条入口可用 | 无 Profile Selector |
| UI-TAB-002 | 新建 Tab 下拉：Profile/连接类型、搜索、键盘选择 | U-AUDIT / 需求 B2 | 无实现 | not_started | 无 | 未实现 | 无下拉/搜索/键盘选择 |
| UI-TAB-003 | 单击 Tab 切换活动 Tab | U-AUDIT / 需求 B2 | main.slint tab-hit `tab-clicked`；main.rs `set_active_tab` | implemented | 人工：多 tab 单击切换；ui-shell §7 .logs/t4.png | 已有记录 | 仅左键；无中键关闭/双击重命名 |
| UI-TAB-004 | 拖动 Tab 调整顺序；拖出窗口按上游行为处理 | U-AUDIT（拖出行为待核） | main.slint `drag-tab`/`drag-x` 拖拽状态；Rust `drag_target` + `workspace::move_tab`；顺序随会话快照持久化 | partial | 单测 `drag_target_maps_pointer_to_slot`、`move_tab_reorders_and_tracks_active`；真机拖 tab0→tab1 日志 `[ws] reorder tab 0 → 1`，点击/双击仍正常 | 拖动排序可用 | 拖出窗口未实现（上游语义亦未核实）；无拖拽视觉反馈 |
| UI-TAB-005 | 关闭 Tab；区分仅关视图/结束 Session/后台保留 | U-AUDIT / 需求 B2 | main.rs `close_action`（end/background）：`detach_runtime` 移入后台池（不 Drop、子进程保留）；**Background Sessions 面板**列标题并可点选重连（`refresh_background`/`reattach_background`），菜单与 `session.reattach` 也可 | implemented | 真机：关 Tab → `[session] detach pane=1`，pid 不变；面板 `background panel items=1`，点条目 → `reattach pane=1`，同一 pid 复用（截图 .logs/bgpanel.png） | 结束/后台两条路径 + 会话列表重连可用 | 退出应用会一并结束后台会话；列表只显示标题与 pane id，无 cwd/运行时长 |
| UI-TAB-006 | 关闭其他/指定 Tab；恢复最近关闭 Tab | U-AUDIT（上游菜单能力待核） | main.rs `close_other_tabs` + workspace `keep_only`；per-tab 菜单项 Close Others | partial | 单测 `keep_only_drops_other_tabs_and_reports_panes`；真机 `[ws] close other tabs, keep=0, 关闭 1 个 pane` | 关闭其他已可用；最近关闭未实现 | 无最近关闭栈；上游是否提供待核 |
| UI-TAB-007 | 双击 Tab 或快捷键重命名（以源码触发方式为准） | U-AUDIT（上游触发方式待核） | main.slint 重命名浮层（TextInput，Enter 提交/Esc 取消）；main.rs F2 与双击入口、`sanitize_tab_title` 清洗 | partial | 单测 `tab_title_sanitizing`；真机双击 Tab → `[ui] rename tab=0 title=...` | 双击入口与浮层可用；F2/Enter/Esc 路径已编码 | 沙箱键盘注入不可用，Enter 提交未端到端验证；无重命名撤销 |
| UI-TAB-008 | 标题跟随 Shell/SSH 远端标题；前缀/后缀/过滤 | U-AUDIT / 需求 B2 | pane.rs Listener Title→CoreEvent::Title；main.rs `poll_events` 400ms + `rebuild_tabs` | partial | `title_event_is_delivered`；人工 printf OSC 0/2；见 ui-shell §3 | 事件有测试；仅活动 tab 的首个 pane 标题写回 | 后台 tab/非首 pane 不更新；无 prefix/suffix/filter；无 SSH |
| UI-TAB-009 | Tab 显示活动/连接/后台活动/进度/完成/失败；非仅颜色 | U-AUDIT / 需求 B2 | main.slint TabView{title,active} 底色/accent 线/字重；无连接/进度字段 | partial | 人工：多 tab 观察激活态；无连接/进度场景 | 活动态有底色+accent 线+字重（非仅颜色） | 无连接状态/进度/完成失败/后台活动；无图标/无障碍标签 |
| UI-TAB-010 | Tab 右键菜单（按上游实际项目） | U-AUDIT（上游菜单项待核） | 窗口级菜单（置顶/新建/设置/重启/关窗）+ per-tab 菜单（Close/Close Others/Duplicate，`tab-menu-action`） | partial | 真机：右键 Tab → `[ui] tab action=2 index=0` → `[ws] duplicate tab=0 panes=1`；Close Others 关闭其它 pane | per-tab 菜单可用 | 无 Rename/Split/复制连接；上游完整项目待核 |
| UI-TAB-011 | Tab 键盘导航：上/下一个、序号跳转、最近 Tab | U-AUDIT / 需求 B2 | main.rs `handle_input` 拦截 Ctrl+Tab / Ctrl+Shift+Tab / Ctrl+1..9 → `switch_tab_relative`/`switch_tab_number`；`next_tab_index` 循环 | partial | 单测 `tab_navigation_wraps`；键盘注入在本沙箱不可用，待真机人工验收 | 循环切换与序号跳转已实现 | 无“最近使用 Tab”栈；键盘端到端未在沙箱验证 |
| UI-TAB-012 | 持久化 Tab 顺序、标题、Pane 布局与恢复信息 | U-AUDIT / 需求 B2 | session.rs Snapshot/TabSnapshot/Tree；main.rs `capture_snapshot`/`restore_session` | partial | session.rs 5 例；人工 Restart Session（self-iteration 第 3 轮） | 顺序/标题/树/ratio/cwd 可存；仅在手动重启时 | 无自动启动恢复；Tab 无独立稳定 ID（仅下标）；无运行中进程/历史 |

### B3. Split Pane / 分屏操作（11 条）

状态：implemented=6，partial=1，needs_verification=1，not_started=3。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-PANE-001 | 当前 Tab 新建水平/垂直分屏 | U-AUDIT / 需求 B3 | workspace.rs `Node::split`；main.rs `split_pane_inner`；右键 Split Right/Down | implemented | workspace `split_row_divides_width_with_gap`/`split_col_divides_height`；人工 .logs/m2.png | 已有记录 | 入口仅终端右键菜单；无快捷键 |
| UI-PANE-002 | 任意层级嵌套分屏，不限于 2/4 | U-AUDIT / 需求 B3 | workspace.rs 递归 `Node` + `Tab::split`；`layout`/`dividers` 递归 | implemented | `nested_split_layout_is_contained`、`nested_dividers_have_paths`；3 pane 截图 | 已有记录（3 pane 嵌套） | 无深度上限；极深布局未压测 |
| UI-PANE-003 | 分隔线拖动调比例；合理最小尺寸 | U-AUDIT / 需求 B3 | workspace.rs `layout`/`set_ratio`（0.1 夹取）；main.slint 分隔线 TouchArea；main.rs `on_divider_dragged` | implemented | `set_ratio_updates_layout_and_clamps`；人工 .logs/split_after.png | 已有记录 | 最小 10%；无双击复位/键盘调比例 |
| UI-PANE-004 | 单击 Pane 聚焦；焦点有清晰视觉状态 | U-AUDIT / 需求 B3 | main.rs `focus_pane`；main.slint pane 边框按 `p.focused && root.panes.length > 1` 画 1px accent | implemented | 人工：`--demo-splits` 点击不同 pane，焦点边框跟随；单 pane 不画边框 | 2026-11-05 实测：点击切换后边框立即跟随（截图 .logs/act003-focus.xwd） | 边框宽度固定 1px；未做焦点 pane 标题条/高亮分隔线 |
| UI-PANE-005 | 快捷键在相邻 Pane 间切换焦点 | U-AUDIT / 需求 B3 | 无实现；`commands.rs` 定义 `pane.focus-next`（Ctrl+Shift+Right），无分发 | not_started | 无 | 未实现 | 命令表占位未接线；无方向/相邻 pane 焦点算法 |
| UI-PANE-006 | 关闭当前 Pane；最后一个 Pane 的产品定义 | U-AUDIT / 需求 B3 | main.rs `close_pane_inner`；workspace `Tab::remove`/`Node::remove`；右键 Close Pane | implemented | `remove_collapses_to_sibling`、`tab_focus_moves_after_removal`、幽灵分屏回归 | 已有记录（self-iteration 第 2 轮） | 最后一个 pane 直接删 tab/退应用；无空状态/确认 |
| UI-PANE-007 | Pane 放大/还原（若上游提供），不改 Session 状态 | U-AUDIT（上游行为待核） | 无实现 | needs_verification | 待上游核实是否有 zoom | 未核实/未实现 | 无 pane zoom/maximize 布局状态 |
| UI-PANE-008 | Pane 间移动/重排/重组；上游支持则复刻拖拽 | U-AUDIT（拖拽语义待核） | 无实现 | not_started | 无 | 未实现 | 无 pane 拖拽/交换/移入其他 tab |
| UI-PANE-009 | 每 Pane 独立尺寸、Session、滚动位置、输入焦点 | U-AUDIT / 需求 B3 | pane.rs 每 pane 独立 PaneHandle/actor/PTY/Term；main.rs PaneRuntime 各自 grid/rect/renderer；Tab.focused | implemented | `pty_prints_and_ui_pulls`、`selection_copy_returns_text`；3 pane 截图 .logs/nf.png；人工独立输入 | 已有记录 | 独立滚动位置无专项断言；焦点视觉弱 |
| UI-PANE-010 | 分屏布局保存为 Profile/Workspace 并恢复 | U-AUDIT / 需求 B3 | session.rs Tree/Split(ratio)；菜单 Restart Session | partial | session.rs 树↔JSON 往返含 ratio；人工恢复记录 | 仅匿名会话快照；无命名 Profile/Workspace | 快照只由 Restart 写入，不能命名/管理/导出 |
| UI-PANE-011 | 多 Pane 广播输入：开关、范围显示、危险提示 | U-AUDIT / 需求 B3 | 无实现 | not_started | 无 | 未实现 | 无 broadcast 状态/UI/危险命令提示 |

### B4. Sidebar、Toolbar 与连接入口（8 条）

状态：partial=2，not_started=6。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-NAV-001 | Sidebar 显示/隐藏；宽度调整（若上游支持） | U-AUDIT（上游宽度行为待核） | 无实现 | not_started | 无 | 未实现 | 无 Sidebar；上游是否支持宽度调整待核 |
| UI-NAV-002 | Sidebar/Toolbar 按钮、分组、顺序遵循上游；插件可扩展 | U-AUDIT / 需求 B4 | 无实现 | not_started | 无 | 未实现 | 无 Sidebar/Toolbar/插件入口 |
| UI-NAV-003 | Connection Manager 入口始终易访问 | U-AUDIT / 需求 B4 | 无实现 | not_started | 无 | 未实现 | 无连接管理入口 |
| UI-NAV-004 | Profile Selector：快速选 Shell/SSH/Serial/Telnet Profile | U-AUDIT / 需求 B4 | TabBar 菜单 Profiles 面板：四类 Profile 点选开 Tab（Local 直接起 shell、SSH 系统 ssh、Telnet 系统 telnet、Serial socat） | partial | 真机：Local/SSH/Telnet/Serial 各开一个 Tab 均成功（见 SSH/Telnet/Serial 各行证据） | 四类 Profile 均可打开 | 无“新 Tab 直接选 Profile”入口（需先开面板）；无 Profile 搜索 |
| UI-NAV-005 | Command Palette：模糊搜索命令/Profile/设置（按上游核验） | U-AUDIT（上游能力待核） | main.slint Palette 浮层 + main.rs `open_palette`/`refresh_palette`/`palette_accept`；`commands::Registry::search` 模糊搜索；TabBar 菜单与 Ctrl+Shift+P 入口 | partial | 真机：菜单 → `[ui] palette open items=10`；点第一条 → `[ui] run command tab.new` → `[ws] new tab: tabs=2` | 命令面板可用（19/21 命令已接线） | Profile/设置项未接入搜索；无键盘输入端到端验证（沙箱键盘注入不可用） |
| UI-NAV-006 | Toolbar 插件按钮：图标、名称、提示、动作、禁用态 | U-AUDIT / 需求 B4 | 无实现 | not_started | 无 | 未实现 | 无插件系统/工具栏 |
| UI-NAV-007 | 状态/通知区：连接、活动任务、进度、完成提醒 | U-AUDIT / 需求 B4 | 无状态区；仅 CoreEvent::Bell/Exited 事件存在但 UI 未消费 | not_started | 无 | 未实现 | 无状态栏/通知中心；Bell/Exited 被 `poll_events` 忽略 |
| UI-NAV-008 | 所有图标 Tooltip/Accessible Label；禁用/加载/错误态清楚 | U-AUDIT / 需求 B4 | main.slint MiniBtn/窗口按钮只画 Text 字形，无 tooltip/accessible-label/disabled 态 | not_started | 人工：悬停按钮无提示；屏幕阅读器未测 | 未实现 | ⚙/─/□/❐/✕/+ 均无标签；无禁用/加载/错误态 |

### B5. Connection Manager / Profile 管理 UI（13 条）

状态：partial=2，needs_verification=3，not_started=8。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-PROF-001 | Profile 列表与 Profile Groups/分组展示 | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | 无 Profile 数据模型/列表/分组 |
| UI-PROF-002 | 搜索/过滤 Profile | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | 无 Profile UI |
| UI-PROF-003 | 创建、编辑、保存、取消、删除 Profile | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | 无 Profile 编辑器/持久化 |
| UI-PROF-004 | 复制 Profile、拖动排序、移动到分组、分组增删改（按原版核验） | U-AUDIT（原版行为待核） | 无实现 | needs_verification | 待上游核实分组操作 | 未核实/未实现 | 无 Profile/分组模型 |
| UI-PROF-005 | 设置默认新 Tab Profile | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | 仅 config.shell 单一默认 shell |
| UI-PROF-006 | 点击 Profile 连接；当前/新 Tab 打开（依上游） | U-AUDIT（打开位置待核） | 无实现 | needs_verification | 待上游核实打开位置 | 未核实/未实现 | 无 Profile 点击连接 |
| UI-PROF-007 | 连接失败显示原因、重试、编辑入口 | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | spawn 失败只 eprintln；无连接错误 UI |
| UI-PROF-008 | 连接中 Profile 进度/状态；支持取消连接 | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | 无连接状态机/取消 |
| UI-PROF-009 | Profile 可绑定快捷键 | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | 无快捷键系统 |
| UI-PROF-010 | Profile 创建/编辑按连接类型动态显示字段 | U-AUDIT / 需求 B5 | 无实现 | not_started | 无 | 未实现 | 无连接类型枚举/表单 |
| UI-PROF-011 | SSH Profile 字段分组 General/Ports/Advanced/Ciphers/Login Scripts（以源码为准） | U-AUDIT（字段分组待核） | 无实现 | needs_verification | 待上游源码核实字段 | 未核实/未实现 | 无 SSH Profile UI |
| UI-PROF-012 | Serial Profile：端口/波特率/数据位/校验/停止/流控/模式/换行 | U-AUDIT / 需求 B5 | Profile kind=serial：serial_port + baud；`serial_command()` 走 `socat -,raw,echo=0 <dev>,raw,echo=0,b<baud>` | partial | 真机：socat PTY 对 + 定时写 SERIAL-OK → Tab 显示连续 SERIAL-OK（.logs/serial-e2e.png） | 端口/波特率可用 | 无数据位/校验/停止位/流控/模式/换行设置；依赖系统 socat |
| UI-PROF-013 | Local Shell Profile：命令、参数、起始目录、环境变量 | U-AUDIT / 需求 B5 | config.rs `shell`；main.rs `create_pane` 传 shell/args/cwd；PaneConfig.env 但 app 传 Vec::new() | partial | 人工：`--shell`/`-- args`/config.toml shell；env 未接线 | --shell 与 args 可用；cwd 仅恢复时用；无 Profile UI | env 字段未接线；无起始目录/标题/自定义选项 UI |

### B6. Terminal View 交互（14 条）

状态：implemented=2，partial=9，not_started=3。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-TERM-001 | 点击终端获得输入焦点；切换不丢输入状态 | U-AUDIT / 需求 B6 | pane-hit down → `ime.focus()` + `pane-clicked`；焦点边框 | partial | 真机（XMODIFIERS=@im=none）：点击 pane 键入 `echo ACCEPT-OK` + Enter → 输出正确（.logs/acceptance-keyboard.png） | 点击聚焦 + 键盘输入 + 快捷键可用 | IME 候选跟随仍未端到端验证（默认输入法会拦截 XTest）；focus in/out 未上报 |
| UI-TERM-002 | 鼠标拖选；跨行、反向、滚动自动扩展 | U-AUDIT / 需求 B6 | pane.rs `SelectionStart/Extend`+`sel_anchor`；main.rs `on_pane_mouse_down/move/up` | partial | `selection_copy_returns_text`、`backward_selection_includes_first_column`；人工 XTest 反向拖选 | 拖选/跨行/反向已有记录 | 无滚动自动扩展；无双击选词/三击选行/矩形选择 |
| UI-TERM-003 | 复制、粘贴、全选、清屏、查找、滚到顶/底 | U-AUDIT / 需求 B6 | arboard；Ctrl+Shift+C/V；`copy_current_selection`/`paste_into_focused`；Ctrl+F；菜单与命令含 Select All/Clear | partial | 真机 Select All/Clear；`search_finds_and_scrolls`；粘贴确认真机验证 | 复制/粘贴/查找/全选/清屏可用 | 滚到顶/底未做；粘贴无危险命令检测 |
| UI-TERM-004 | 右键菜单：复制/粘贴/查找/清屏/Profile/拆分等（按上游） | U-AUDIT（完整菜单待核） | main.slint 右键菜单 8 项（Copy/Paste/Split Right/Split Down/Close Pane/Find/Select All/Clear）；`on_menu_action` 分发 0–7 | partial | 真机：Select All → Copy 得到 `alpha\nbravo\ncharlie`；Clear 后同路径得到空白（已过滤） | 复制/粘贴/分屏/关闭/查找/全选/清屏均可用 | 无 Profile/缩放菜单项；上游完整项目待核；鼠标上报模式下右键仍按设计转发给 PTY |
| UI-TERM-005 | 可配置右键粘贴（PuTTY 风格） | U-AUDIT / 需求 B6 | 无实现 | not_started | 无 | 未实现 | 无右键粘贴设置；右键固定弹菜单 |
| UI-TERM-006 | 可配置 Copy-on-Select | U-AUDIT / 需求 B6 | config `copy_on_select` + 设置面板开关；`on_pane_mouse_up` 仅在开启时写剪贴板 | implemented | 真机：关闭后拖选 0 条 clipboard 日志，开启后 1 条（复制 6 字节）；config 默认断言 | 2026-11-05 实测通过 | 无每 profile 覆盖；设置项在 Terminal 分类（无搜索） |
| UI-TERM-007 | 多行粘贴确认/警告；记住用户选择 | U-AUDIT / 需求 B6 | main.rs `needs_paste_confirm` + main.slint 粘贴确认浮层（Cancel / Always Allow / Paste）；config `confirm_multiline_paste` | implemented | 单测 `paste_confirm_only_for_multiline_when_enabled`；真机：多行粘贴 → `paste confirm lines=3`；Always Allow 后写配置且下次直发 | 2026-11-05 实测通过 | 只按“是否多行”判断，未做危险命令检测；预览只取前 3 行 |
| UI-TERM-008 | Bracketed Paste 包裹并发送给支持的 Shell/TUI | U-AUDIT / 需求 B6 | main.rs `paste_payload(normalized, bracketed)` 统一组装；检查 `BRACKETED_PASTE` 模式位 | partial | 单测 `paste_normalization_and_brackets`（包裹字节/长度）；真机 `paste bytes=13 bracketed=false`（cat 未开模式） | 包裹与规范化有单测 | 缺“应用真的收到 200/201”的端到端验收；未测内容含 201 |
| UI-TERM-009 | 滚动条/滚轮；alternate screen（vim/less）滚动正确 | U-AUDIT / 需求 B6 | main.rs `on_pane_scrolled`；pane.rs `Ctrl::Scroll`；alacritty alt screen | partial | `wheel_scrollback_shows_history`；人工 vim/htop（phase0 FPS 记录） | 回滚可用；alt screen 依赖 alacritty | 无可见滚动条；alt screen 滚轮行为未专项验收 |
| UI-TERM-010 | 查找面板：关键词、大小写/正则、上/下一个、匹配计数 | U-AUDIT（上游选项待核） | main.rs `open_search`/`run_search`/`close_search`；pane.rs `search`（regex_escape 纯文本） | partial | `search_finds_and_scrolls`；人工 Ctrl+F、Enter/Shift+Enter | 增量纯文本搜索/高亮/上下跳转可用 | 无大小写/正则/匹配计数；搜索框不可鼠标聚焦/选中 |
| UI-TERM-011 | 点击 URL/IP/文件路径执行动作；区分远端/本地路径 | U-AUDIT / 需求 B6 | core `link.rs::scan` 识别 URL/IPv4/路径；`frame.rs` OSC 8 链接表 + `CellData.link`；app 层尚未接 hover/Ctrl+点击 | partial | link.rs 7 例；`vt_conformance` OSC 8 用例；app 无链接交互测试 | core 识别/透出已有记录（94baefb/99d7895）；UI 动作缺失 | 无 hover 手型/Ctrl+点击/打开动作；远端/本地路径未区分；无插件 Linkifier |
| UI-TERM-012 | 终端字体放大/缩小/重置；显示当前缩放 | U-AUDIT / 需求 B6 | 仅设置面板 ± 改全局字号（`on_bump_font_size`）；`commands.rs` 有 zoom-in/out/reset 占位，无分发 | not_started | 人工：设置面板可改字号；无 Ctrl+=/-/0 | 未实现（仅全局设置） | 命令表占位未接线；无 per-pane/临时缩放/缩放显示 |
| UI-TERM-013 | 可配置终端上下文菜单与插件装饰器 | U-AUDIT / 需求 B6 | main.slint 菜单硬编码 5 项；无插件/装饰器接口 | not_started | 无 | 未实现 | 无菜单配置/插件扩展点 |
| UI-TERM-014 | 内容变化不导致光标跳动、选区丢失或明显闪烁 | U-AUDIT / 需求 B6 | main.rs 拉帧 + render.rs 增量重绘；selection 存 Term，frame 携带 Span；比较 last_selection/search | partial | 静态审查；无视觉回归/录屏基线；未专项验收 | 代码路径合理；未验证 | 无自动化视觉回归；局部重绘/整屏切换边界未压测 |

### B7. Settings 设置页（14 条）

状态：partial=8，not_started=6。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-SET-001 | Application：应用级行为、启动与默认行为 | U-AUDIT / 需求 B7 | 设置面板改为分类页（Appearance/Terminal/Window/Config File），当前无独立 Application 分类 | partial | 真机：四个分类可切换（.logs/settings-cat-*.png） | 分类框架可用 | Application 分类内容（启动行为、默认项）未做 |
| UI-SET-002 | Appearance：外观、窗口风格、图标/字体、背景效果 | U-AUDIT / 需求 B7 | main.slint 设置面板仅有主题选择；主题含 UI chrome 配色 | partial | 人工：12 套主题 + Auto 可切换；无窗口风格/背景效果 | 主题外观可用 | 无窗口风格/图标/字体/背景/透明度设置 |
| UI-SET-003 | Profiles & Connections：默认 Profile、列表、连接行为 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无 Profile 设置页 |
| UI-SET-004 | Terminal：行为、字体、滚动、复制粘贴、鼠标等 | U-AUDIT / 需求 B7 | main.slint 字号 ±、回滚 ±、Copy on select / Confirm close 开关（ToggleRow）；config.rs font_size/scrollback/copy_on_select/confirm_close | partial | 真机：开设置点开关 → `[ui] copy_on_select=false` 并写入配置；关闭后拖选 0 条剪贴板日志、开启后 1 条（复制 6 字节） | 字号/回滚/复制-关闭行为可调 | 无字体族/行距/鼠标/滚动行为设置；无设置搜索 |
| UI-SET-005 | Color Scheme：选择、预览、导入/导出 | U-THEME / 需求 B7 | main.slint 主题网格 + Flickable；themes.rs ThemeRegistry 6 格式导入；无预览/导出 | partial | themes.rs 12 例（含 6 格式导入）；人工 .logs/imported_settings4.png | 选择/导入可用；无预览/导出 | 无独立预览；导出未实现；热重载未实现 |
| UI-SET-006 | Config Sync：原版或插件提供时的兼容策略 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无同步机制/策略 |
| UI-SET-007 | Hotkeys：列表、搜索、录入、修改、清除、冲突提示、多段键 | U-AUDIT / 需求 B7 | 设置面板 Hotkeys 分类：命令表默认绑定列表（分类/标题/组合键）、冲突数、多段键 Display 支持 | partial | 单测 `hotkey_page_lists_bindings_and_conflicts`；真机截图 `.logs/settings-hotkeys.png`（22 commands · 16 bound · 0 conflicts） | 只读列表 + 冲突提示可用 | 无搜索/录入/修改/清除；未持久化自定义键；无键盘捕获（沙箱无法验证） |
| UI-SET-008 | Plugins：搜索、可用/已装、安装、卸载、启用、禁用、更新、详情、权限、目录 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无插件系统/管理器 UI |
| UI-SET-009 | SSH：Host Key、Agent 类型、X11 Display 等 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无 SSH/设置页 |
| UI-SET-010 | Vault：加密凭据容器、主密码、锁定/解锁、凭据管理 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无 Vault/Keyring/凭据模型 |
| UI-SET-011 | Window：窗口行为、TabBar 位置、Quake Mode | U-AUDIT / 需求 B7 | TabBar 菜单：Always on Top、Tab Bar Top/Bottom（持久化）；无独立 Window 设置页/Quake | partial | 真机：菜单切换置顶与 TabBar 位置，配置落盘 | 置顶 + TabBar 上/下可用 | 无 Window 分类设置页、无 Quake Mode、无左/右 TabBar |
| UI-SET-012 | Config File：路径、打开文件/目录、诊断（不显示 secret） | U-CONF / 需求 B7 | 设置面板 Config File 分类：显示 `Config::path()`、Open Folder（xdg-open）；文案声明不存密码/私钥 | partial | 真机：面板显示 `/tmp/set2.toml`；Open Folder 调用 xdg-open（沙箱内可能失败，会打日志） | 路径展示与打开目录可用 | 无配置诊断/语法检查；无“打开配置文件”按钮（只有目录）；secret 策略依赖 Vault（未实现） |
| UI-SET-013 | 插件可动态注册设置页/项；卸载后配置清理或保留策略 | U-AUDIT / 需求 B7 | 无实现 | not_started | 无 | 未实现 | 无插件设置 API/生命周期策略 |
| UI-SET-014 | 保存时机明确；保存失败/无效值/迁移错误提示 | U-AUDIT / 需求 B7 | config.rs 即时保存 + 原子写 + schema 迁移/备份；错误仅 eprintln；数值 clamp | partial | config.rs 7 例；代码走查：字号 8–32、回滚 500–100000 夹取；写失败仅 stderr | 即时保存/迁移/备份可用；无 UI 提示 | 保存/迁移/解析失败不弹 UI；无“恢复备份”入口；无效值只夹取不提示 |

### B8. SSH / SFTP / Port Forward UI（9 条）

状态：partial=5，not_started=4。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-SSH-001 | SSH Profile 编辑窗口分组清晰、密码/密钥安全提示 | U-AUDIT / 需求 B8 | Profile 表单 SSH 分组：Host/Port/User/Auth/Key/Extra；`ssh_command()` 用系统 ssh 客户端跑在 PTY；密码认证仅存 `auth=password`（Vault 未实现） | partial | 单测 `ssh_command_builds_openssh_args`；端到端：paramiko 服务端 + key 认证 → 面板点开后终端显示 `PARAMIKO-SSH-OK` 与远端提示符（.logs/ssh-e2e2.png） | SSH 连接（key/agent）可用 | 无内建 known_hosts 管理（依赖系统 ssh）；密码认证未接 Vault；原生 russh 引擎未做；未做跳板机/端口转发设置 |
| UI-SSH-002 | 首次主机密钥确认、变更告警、known_hosts 管理入口 | U-AUDIT / 需求 B8 | 交给系统 ssh：主机密钥确认/告警直接显示在终端里；Extra 参数可传 UserKnownHostsFile | partial | 端到端走 `-o StrictHostKeyChecking=yes` + 自定义 known_hosts 成功连接 | 借用系统 ssh 的完整校验流程 | 无应用内 known_hosts 管理 UI；无密钥变更专门告警面板 |
| UI-SSH-003 | SFTP 面板/Tab：目录导航、上级、刷新、路径输入、列表、属性 | U-AUDIT / 需求 B8 | **SFTP 面板**（TabBar 菜单 → SFTP）：SSH Profile 选择、Remote 路径输入、List、条目列表点选；`sftp.rs` 走系统 `sftp -q -b -` | partial | 端到端：`scripts/ssh_test_server.py`（SFTP）→ 面板 List 显示 `1 个条目` / `/probe.txt`（.logs/sftp-panel.png） | 面板列目录/选择可用 | 无“上级/刷新/属性列”；同步阻塞调用（大目录会卡 UI）；无传输进度 |
| UI-SSH-004 | SFTP 上传/下载/建目录/重命名/删除；危险确认 | U-AUDIT / 需求 B8 | 面板 Download selected / Upload：`get`/`put` batch；Local 路径输入；操作结果写状态行 | partial | 端到端：下载 → 本地内容 `sftp-panel-ok`；上传 → 远端出现 `tervo-sftp` | 上传/下载可用 | 无进度/取消/危险确认；mkdir/rename/rm 未封装；同步阻塞 |
| UI-SSH-005 | 传输进度、速度、已传/总大小、取消、失败重试 | U-AUDIT / 需求 B8 | 无实现 | not_started | 无 | 未实现 | 无传输任务模型/进度 UI |
| UI-SSH-006 | Port Forward 管理：查看/添加/启停/删除，显示监听与目标 | U-AUDIT / 需求 B8 | Profile `local_forwards` → `ssh -L`；表单 Forwards 行；转发由 ssh 子进程持有（关 Tab 即停） | partial | 单测 `ssh_forwards_become_dash_l_args`；**端到端**：`scripts/ssh_test_server.py`（paramiko，含 direct-tcpip）→ `ss` 显示 19000 监听，`printf ping \| socat TCP:127.0.0.1:19000` 收到 `ECHO:ping` | 本地转发可用且验证通过 | 无应用内管理面板（增删改查/启停/监听与目标展示）；仅 local 转发（-L），无 -R/-D |
| UI-SSH-007 | Jump Host 配置入口与链路可视化/可读描述 | U-AUDIT / 需求 B8 | 无实现 | not_started | 无 | 未实现 | 无跳板机 |
| UI-SSH-008 | Login Scripts 编辑器：命令/顺序/延时/等待提示符（按上游核验） | U-AUDIT（字段待核） | 无实现 | not_started | 无 | 未实现 | 无登录脚本 |
| UI-SSH-009 | X11/Agent Forwarding 状态显示与失败提示 | U-AUDIT / 需求 B8 | 无实现 | not_started | 无 | 未实现 | 无 forwarding 状态 |

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
| UI-QUAKE-001 | 全局快捷键显示/隐藏终端窗口 | U-AUDIT / 需求 B10 | 无实现 | not_started | 无 | 未实现 | 无全局快捷键/Quake 状态机 |
| UI-QUAKE-002 | 窗口按设置停靠屏幕边缘/指定位置（受 OS 限制） | U-AUDIT / 需求 B10 | 无实现 | not_started | 无 | 未实现 | 无窗口定位设置；Wayland 定位受限 |
| UI-QUAKE-003 | 调整窗口高宽、自动隐藏/失焦隐藏（按上游核验） | U-AUDIT（行为待核） | 无实现 | not_started | 无 | 未实现 | 无 Quake 尺寸/失焦隐藏 |
| UI-QUAKE-004 | Quake 下 Tab/Pane/Session 操作与普通窗口一致 | U-AUDIT / 需求 B10 | 无实现 | not_started | 无 | 未实现 | 无 Quake 模式 |
| UI-QUAKE-005 | 快捷键冲突提示；Wayland 全局快捷键能力检测与回退 | U-AUDIT / 需求 B10 | 无实现 | not_started | 无 | 未实现 | 无全局快捷键/能力检测/回退说明 |

### B11. 插件管理 UI（7 条）

状态：partial=3，not_started=4。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-PLUG-001 | Available/Installed 插件列表 | U-AUDIT / 需求 B11 | `plugin.rs` 扫描 `plugins/*.toml`；设置 Plugins 页列出已安装清单（enabled/disabled）；跳过坏文件并计数 | partial | 4 个单测（加载/跳过/重复/未知字段保留）；真机 `[plugin] 发现 2 个清单，跳过 1`，页面显示 `2 plugins · 1 enabled · 1 skipped`（.logs/settings-plugins.png） | 已安装（清单）列表可用 | 无“可用/商店”列表；无安装/卸载/升级 |
| UI-PLUG-002 | 插件名称、说明、版本、作者、兼容 API、权限要求 | U-AUDIT / 需求 B11 | `PluginManifest{id,name,version,description,enabled,extra}`；未知字段原样保留 | partial | 单测 `unknown_fields_are_preserved`；页面显示 名称/版本/状态/说明 | 基础元数据可用 | 无 author/apiVersion/permissions 强类型字段与展示 |
| UI-PLUG-003 | 搜索、安装、卸载、启用、禁用、升级、重启提示 | U-AUDIT / 需求 B11 | 仅读取 `enabled` 字段展示，不可切换 | not_started | 无 | 未实现 | 生命周期/搜索/升级全缺；启用状态只读 |
| UI-PLUG-004 | 安装来源与签名/校验；安装前展示权限 | U-AUDIT / 需求 B11 | 无实现 | not_started | 无 | 未实现 | 无安装器/签名/权限 UI |
| UI-PLUG-005 | 插件崩溃/超时/API 不兼容诊断信息 | U-AUDIT / 需求 B11 | 无实现 | not_started | 无 | 未实现 | 无插件运行时/诊断 |
| UI-PLUG-006 | Open Plugins Directory | U-AUDIT / 需求 B11 | 设置页显示插件目录路径；未做打开按钮（可复制路径） | partial | 真机截图显示 `/tmp/pl` | 路径可见 | 无一键打开目录按钮 |
| UI-PLUG-007 | 插件扩展 Toolbar/命令面板/Tab 菜单/终端菜单/设置/Provider/Middleware | U-AUDIT / 需求 B11 | 无实现 | not_started | 无 | 未实现 | 无扩展点 |

## 4. C 部分：底层功能点

### C1. Terminal Emulator 核心（16 条）

状态：implemented=5，partial=10，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| TERM-001 | VT220 基础兼容及 Tabby 实际使用的 xterm/ANSI 扩展；兼容测试集验证 | U-XTERM / 需求 C1 | alacritty_terminal 0.26 `Term` + `vte::ansi::Processor`；crates/core/tests/vt_conformance.rs 10 例 | partial | `cargo test -p tervo-core`（已有记录 10 条 conformance）；无 vttest/esctest 全套 | 基础 SGR/光标/宽字符/备用屏有契约测试 | 未跑完整 VT220/xterm 套件；与 Tabby 实际序列集未差分 |
| TERM-002 | CSI/ESC/OSC/DCS 解析；不完整序列、分包、非法、超长序列 | U-XTERM / 需求 C1 | vte Processor 处理分包/不完整；pane.rs `feed` 按 32 KiB 读块喂入；无 DCS/OSC 业务分发 | partial | vt_conformance 覆盖常见 CSI/OSC 标题；无 fuzz/超长/非法输入回归 | 常见序列可解析；异常输入未系统验证 | 无 fuzz 目标；DCS 只被解析器吞掉，无业务处理；超长序列未测 |
| TERM-003 | 光标移动、擦除、插入/删除字符行、滚屏、滚动区域、制表位、自动换行 | U-XTERM / 需求 C1 | alacritty_terminal 实现；`cursor_move_and_erase_line`、`autowrap_moves_to_next_line`、`clear_screen_and_home` | partial | 3 条专项契约 + alacritty 上游实现；IL/DL/滚动区域/制表位无本项目测试 | 光标/擦除/自动换行已验证 | 插入删除行、滚动区域、制表位未专项验收；依赖 alacritty 行为 |
| TERM-004 | 主/备用屏幕切换；vim/less/tmux 等全屏 TUI | U-XTERM / 需求 C1 | alacritty `TermMode::ALT_SCREEN`；`alternate_screen_mode_roundtrip`；人工 vim/htop | implemented | `alternate_screen_mode_roundtrip`；phase0 vim/htop 55–58 FPS 记录 | 已有记录 | tmux/less 未逐项验收；备用屏滚轮行为未专项测 |
| TERM-005 | 16 色、256 色、24-bit True Color；Base16 颜色基础设施 | U-XTERM / 需求 C1 | core palette.rs `Palette::resolve`（16/256/truecolor）；`sgr_basic_colors_resolve_to_palette`、`sgr_256_and_truecolor` | implemented | 两条 SGR 单测；256 色 196=#ff0000、truecolor 原样透传 | 已有记录 | Base16 无独立模型/校验，仅通过导入格式间接受支持 |
| TERM-006 | Unicode：UTF-8 分包、组合字符、双宽 CJK、emoji、VS、ZWJ grapheme | U-XTERM / 需求 C1 | vte/alacritty UTF-8；`wide_cjk_spans_two_cells`；glyph.rs `has_glyph` 缺字形留空 | partial | CJK 宽字符测试 + 人工 .logs/cjk2.png；emoji/VS/ZWJ 无测试 | UTF-8 分包与双宽 CJK 可用 | emoji 无彩色/常留空；组合字符/VS/ZWJ 未验证；无 grapheme 级测试 |
| TERM-007 | 字体 fallback、Powerline/Nerd Fonts、box drawing、连字 | U-XTERM / 需求 C1 | glyph.rs 主字体 JetBrains Mono + CJK 回退（WQY/Droid/Noto）；无 Nerd/连字逻辑 | partial | glyph.rs 3 例（CJK 回退/惰性）；人工中文截图 | CJK 回退可用；box drawing 依赖主字体 | 无 Powerline/Nerd Fonts/连字；fallback 仅 CJK 两类 |
| TERM-008 | 字体族、字号、字重、行距、字间距、渲染缩放 | U-XTERM / 需求 C1 | glyph.rs `FontSet::load(size)` 固定候选字体与 cell 尺寸；设置仅 font_size | partial | 人工：设置面板字号 ± 即时生效（ui-shell §4） | 字号可调 | 字体族/字重/行距/字间距/缩放不可配；固定物理 cell 尺寸 |
| TERM-009 | Scrollback buffer 容量可配；默认有界内存 | U-XTERM / 需求 C1 | pane.rs `TermConfig{scrolling_history}`；config.rs scrollback；`wheel_scrollback_shows_history` | implemented | 回滚单测 + 设置面板；phase0 记录 2000 行≈3.8 MB 预分配 | 已有记录 | 对新 pane 生效；无 per-pane 独立容量 UI；100k 上限未压测 |
| TERM-010 | Selection、剪贴板、鼠标报告、焦点报告、Bracketed Paste | U-XTERM / 需求 C1 | pane.rs selection/clipboard；mouse.rs 1000/1002/1003/1005/1006；Bracketed Paste 模式位与粘贴包裹 | partial | mouse_report.rs 16 例；smoke selection/backward/search；缺焦点报告测试 | 鼠标/选区/剪贴板/括号粘贴主体可用 | Focus in/out（1004）未上报；粘贴包裹无端到端验收；1002 右键拖动被编码成左键 |
| TERM-011 | OSC 终端标题、颜色查询/设置等上游实际使用扩展 | U-XTERM / 需求 C1 | Listener 只处理 Title/ResetTitle/PtyWrite 等；`Event::ColorRequest`、`ClipboardStore/Load`、`TextAreaSizeRequest` 落到 `_ => {}` | partial | `title_event_and_grid_coexist`、`title_event_is_delivered`；颜色查询无响应 | OSC 0/2 标题可用 | OSC 4/10/11 颜色查询与 OSC 52 剪贴板无响应；无 OSC 7/133 |
| TERM-012 | 可点击 URL/IP/路径；链接识别器支持插件扩展 | U-XTERM / 需求 C1 | `link.rs` 行内识别（URL/IPv4[:port]/路径）；`frame.rs` OSC 8 hyperlink 表与 `hyperlink_at`；无插件 Linkifier 接口 | partial | link.rs 单测；`vt_conformance` OSC 8 用例；无点击动作测试 | 识别/数据层已有记录（94baefb/99d7895） | 仅字符列、宽字符混排需调用方校正；无点击/打开/插件扩展 |
| TERM-013 | 高输出不丢字节、UI 可合帧、输入不被输出阻塞 | U-XTERM / 需求 C1 | pane.rs 有界通道 + 输入优先 + wake 合并；`pull_latency_stays_bounded_under_flood`；10 MB cat 基准 | implemented | smoke 洪峰测试；phase0 10 MB 89 ms、解析 195–202 MiB/s | 已有记录 | 50 MiB/s 有界内存/零丢字节未做正式压测 |
| TERM-014 | 终端 resize 正确向 PTY/SSH PTY 发送行列 | U-XTERM / 需求 C1 | pane.rs `Ctrl::Resize` → `Term::resize` + `master.resize(PtySize)`；main.rs `refresh_views` 检测 grid 变化 | implemented | 人工：拖窗口/分隔线后 shell `stty size` 变化（ui-shell §2）；无自动断言 | 已有手工验收路径 | 无自动化 resize 断言；SSH PTY 未实现 |
| TERM-015 | 终端录制/输出保存核心 API（UI 可由插件提供） | U-XTERM / 需求 C1 | 无实现 | not_started | 无 | 未实现 | 无录制/导出 API；save-output 插件兼容无从谈起 |
| TERM-016 | 完整键盘映射：Ctrl/Alt/Shift、功能键、组合键、IME、平台键码 | U-XTERM / 需求 C1 | keys.rs 特殊键/F1–F12/Ctrl 字节/Alt 前缀；main.rs X11+XIM IME 路径 | partial | keys.rs 3 例；scripts/check_ime.sh；XTest 延迟探针 | 常用键与 X11 XIM 中文输入可用 | 无 Wayland IME；无 modifyOtherKeys/kitty 键盘协议；Shift+Tab/组合键覆盖不全 |

### C2. Local Shell / PTY（7 条）

状态：implemented=2，partial=3，not_started=1，unsupported=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| LOCAL-001 | Linux PTY 创建、启动、读写、resize、退出状态回收 | U-XTERM / 需求 C2 | pane.rs `spawn`（portable-pty openpty/spawn/take_writer/try_clone_reader）；Drop kill；EOF 排空 | implemented | smoke `pty_prints_and_ui_pulls`、`backspace_erases_last_char`；phase0 EOF 修复记录 | 已有记录 | 退出状态码未呈现；异常退出无 UI |
| LOCAL-002 | 自动识别默认 Shell；指定 executable 与参数 | U-XTERM / 需求 C2 | pane.rs shell=config.shell/$SHELL/`/bin/bash`；main.rs `--shell`、`--` args | implemented | 人工 `--shell /bin/sh`；`--help` 与 parse_args；代码走查 | 已有手工路径 | `-l` 帮助有但 parse_args 不识别；无登录 shell 选项 |
| LOCAL-003 | Profile 支持起始目录、环境变量、命令、参数、标题、自定义选项 | U-XTERM / 需求 C2 | PaneConfig{cwd,env,args}；main.rs create_pane 传 cwd（仅恢复）/args，env 传空；无 Profile | partial | session.rs cwd 恢复测试；人工 `-- args` | cwd/args 部分可用 | env 未接线；无起始目录/标题/自定义选项 UI；无 Profile 模型 |
| LOCAL-004 | 多个本地 Shell Profile 并在同一窗口混用 | U-XTERM / 需求 C2 | 无实现 | not_started | 无 | 未实现 | 全局单一 shell 配置；无 Profile 选择 |
| LOCAL-005 | Shell 启动失败、进程退出、PTY 关闭、异常退出处理 | U-XTERM / 需求 C2 | spawn 失败 eprintln；CoreEvent::Exited 产生但 `poll_events` 只处理 Title；最后一帧保留 | partial | 代码走查；无退出提示测试 | 异常不会 panic；无用户可见提示/自动重启 | 启动失败仅 stderr；退出无状态/重试/关闭建议 |
| LOCAL-006 | Shell Integration / 当前目录检测 / 命令完成检测 | U-XTERM / 需求 C2 | session.rs `cwd_of` 读 /proc/<pid>/cwd；无 OSC 7/133/命令完成解析 | partial | `cwd_of_self_matches_current_dir`、`cwd_of_dead_pid_is_none` | 可通过 /proc 取 cwd 供快照 | 无 shell 侧集成脚本；无命令完成/退出码/提示符检测 |
| LOCAL-007 | Windows 专属 PowerShell/WSL/Git Bash/Cygwin/MSYS2/Cmder/CMD/Clink 能力 | U-XTERM / 需求 C2 | 无 Windows 代码路径 | unsupported | 无 | 明确不在 Ubuntu 阶段实现 | 列入 Windows 路线，不在本阶段伪造支持 |

### C3. SSH2 Client（16 条）

状态：needs_verification=1，not_started=14，unsupported=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| SSH-001 | SSH2 建连、断开、超时、keepalive、重连 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | Cargo.toml/Cargo.lock 无 russh 依赖 |
| SSH-002 | Connection Manager 与保存的 SSH Profiles | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无连接管理/Profile |
| SSH-003 | 用户名/主机/端口/认证方式 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无 SSH 配置/UI |
| SSH-004 | 密码、私钥、加密私钥口令、SSH Agent 认证 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无认证/Keyring/Agent |
| SSH-005 | known_hosts 验证；首次确认；密钥变化告警 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无主机密钥校验；必须实现且不得默认跳过 |
| SSH-006 | SSH Config 导入/兼容（Host/HostName/Port/User/IdentityFile/ProxyJump/ProxyCommand） | U-AUDIT（上游支持范围待核） | 无实现 | needs_verification | 待上游核实支持字段 | 未核实/未实现 | 无 config 解析；上游映射范围未知 |
| SSH-007 | Jump Hosts / ProxyJump / 多级跳板机 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无跳板机链路 |
| SSH-008 | Proxy Command（平台能力与安全策略核验） | U-AUDIT（能力/策略待核） | 无实现 | not_started | 无 | 未实现 | 无 ProxyCommand；安全策略未定 |
| SSH-009 | Agent Forwarding；Linux OpenSSH Agent；后续 Pageant/Windows Agent | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无 Agent forwarding |
| SSH-010 | X11 Forwarding 与自定义 X11 Display | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无 X11 forwarding |
| SSH-011 | Local/Remote/Dynamic SOCKS 端口转发 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无端口转发 |
| SSH-012 | Persistent Port Forwards：预配置并随连接恢复 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无转发规则/恢复 |
| SSH-013 | Login Scripts：连接后自动执行，顺序/等待/延时/错误处理 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无登录脚本引擎 |
| SSH-014 | Ciphers/算法配置入口；默认遵循系统/库安全值 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无 SSH 算法配置 |
| SSH-015 | SSH 主机别名、连接历史、最近连接 | U-AUDIT / 需求 C3 | 无实现 | not_started | 无 | 未实现 | 无历史/最近连接 |
| SSH-016 | WinSCP 集成（平台/外部工具） | U-AUDIT / 需求 C3 | 无实现 | unsupported | 无 | Ubuntu 不可用 | 外部工具集成，非当前平台目标；需给出替代入口 |

### C4. SFTP 与文件传输（7 条）

状态：not_started=7。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| SFTP-001 | 基于 SSH Channel 的 SFTP 会话 | U-AUDIT / 需求 C4 | 无实现 | not_started | 无 | 未实现 | 无 SSH/SFTP |
| SFTP-002 | 远端文件列表、目录导航、文件元数据 | U-AUDIT / 需求 C4 | 无实现 | not_started | 无 | 未实现 | 无 SFTP UI/协议 |
| SFTP-003 | 上传/下载、建目录、重命名、删除 | U-AUDIT / 需求 C4 | 无实现 | not_started | 无 | 未实现 | 无文件操作 |
| SFTP-004 | 传输进度、速率、取消、失败重试 | U-AUDIT / 需求 C4 | 无实现 | not_started | 无 | 未实现 | 无传输任务模型 |
| SFTP-005 | 大文件流式传输，不一次性读入内存 | U-AUDIT / 需求 C4 | 无实现 | not_started | 无 | 未实现 | 无 SFTP 流式通道 |
| SFTP-006 | 独立 SFTP Tab/面板；插件实现也需原生等价能力 | U-AUDIT / 需求 C4 | 无实现 | not_started | 无 | 未实现 | 无 SFTP Tab/插件 |
| SFTP-007 | 权限、覆盖确认、路径编码、断线中断处理 | U-AUDIT / 需求 C4 | 无实现 | not_started | 无 | 未实现 | 无权限/覆盖/编码/断线策略 |

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
| SERIAL-001 | 串口枚举、端口打开/关闭 | U-AUDIT / 需求 C6 | 无实现 | not_started | 无 | 未实现 | Cargo 无 serialport/tokio-serial 依赖 |
| SERIAL-002 | 保存多个 Serial Profiles | U-AUDIT / 需求 C6 | 无实现 | not_started | 无 | 未实现 | 无 Serial Profile |
| SERIAL-003 | Baud Rate（含平台非标准）、Data Bits、Parity、Stop Bits、Flow Control | U-AUDIT / 需求 C6 | 无实现 | not_started | 无 | 未实现 | 无非标准波特率能力评估 |
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
| SESS-001 | Shell/SSH/Serial/Telnet Profile 存储、编辑、分组 | U-AUDIT / 需求 C8 | `profile.rs` v2：四类 kind + 字段 + `validate()` + v1→v2 迁移；表单支持**新建/编辑**（行内 ✎ 回填、`)`update` 保留 ID）；行内 × 删除 | partial | 11 个单测；真机：新建（预填）→ 保存 id=1；点 ✎ → `编辑表单 id=1`、标题 Edit Profile、字段回填；保存 → `保存 id=1（共 1）`（update 未新增） | Profile 增/删/改/打开可用 | 无分组/导入导出/搜索；转发字段无专门管理 UI |
| SESS-002 | Profile 绑定快捷键；支持默认 Profile | U-AUDIT / 需求 C8 | 无实现 | not_started | 无 | 未实现 | 无快捷键/默认 Profile |
| SESS-003 | Split Tab 布局保存为 Profile/Workspace | U-AUDIT / 需求 C8 | 无实现 | not_started | 无 | 未实现 | 只有匿名 Restart 快照 |
| SESS-004 | 记住打开的 Tabs/Panes；下次启动恢复工作区 | U-AUDIT / 需求 C8 | session.rs Snapshot；main.rs `capture_snapshot`/`restore_session`（仅 `--restore`）；菜单 Restart Session | partial | session.rs 5 例；人工 Restart Session（self-iteration 第 3 轮） | 手动重启可还原布局/cwd | 启动默认不恢复；无崩溃恢复；无滚动历史/运行中进程 |
| SESS-005 | 恢复最近关闭的 Tab | U-AUDIT / 需求 C8 | 无实现 | not_started | 无 | 未实现 | 关闭即丢；无最近关闭栈 |
| SESS-006 | 区分恢复 UI 布局、连接配置、真实运行进程；不混淆 | U-AUDIT / 需求 C8 | session.rs 只存树/ratio/cwd；docs/self-iteration 明示不存历史/进程 | partial | 代码走查；文档已声明边界 | 边界声明清楚，但只有布局+cwd | 无连接配置/进程恢复；重启会新起 shell |
| SESS-007 | 断开连接后的重连策略与 Session 状态提示 | U-AUDIT / 需求 C8 | 无实现 | not_started | 无 | 未实现 | 无连接/Session 状态模型 |
| SESS-008 | 配置格式版本化、迁移、导入、导出 | U-CONF / 需求 C8 | config.rs `SCHEMA_VERSION=1`、`load_from` 迁移前备份、`migrate`、原子写、`extra` 保留未知键；session.rs VERSION=1 + 原子写；无导入/导出 UI | partial | config.rs 7 例（迁移/备份/未知键/坏文件不覆盖/原子写）；session.rs 版本门禁 | 版本化/迁移/备份/原子写已有记录（b80e265） | 无导入/导出/恢复 UI；备份只在迁移时生成；无用户可见迁移报告 |
| SESS-009 | 导入原 Tabby 配置：Profiles/快捷键/主题/插件配置，生成不兼容报告 | U-CONF / 需求 C8 | 无实现 | not_started | 无 | 未实现 | 无 Tabby config.yaml 解析/迁移报告 |

### C9. Hotkeys / Commands（7 条）

状态：implemented=1，partial=4，not_started=2。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| KEY-001 | 所有核心操作都有可配置命令 ID | U-AUDIT / 需求 C9 | `commands.rs::builtin` 22 条稳定 ID + 默认快捷键；键盘分发：Ctrl+T/W/F、Ctrl+Shift+C/V/P/E/D/W、Ctrl+Tab/Shift+Tab、Ctrl+1–9、F11、F2；命令面板 21 条 | partial | 真机（XMODIFIERS=@im=none）：Ctrl+T、Ctrl+Shift+P/E/D、Ctrl+Tab、Ctrl+Shift+Tab、Ctrl+1/Ctrl+3、F11、F2 全部通过且无 panic；键入 `echo ACCEPT-OK` 正常 | 常用快捷键与命令面板可用（含本轮修复的 F11/F2/Tab 切换二次借用崩溃） | 分发仍硬编码，未从 Registry 统一；缩放（Ctrl+±/0）与部分命令无键盘入口；无用户改键 |
| KEY-002 | 自定义快捷键，支持单键与 multi-chord | U-AUDIT / 需求 C9 | `commands.rs::Shortcut::parse` 支持 1–2 段、ctrl/alt/shift/meta；无配置读写/录制 UI | partial | `shortcut_parse_and_format_roundtrip`；无自定义绑定测试 | 解析/格式化数据层已有记录 | 无用户配置/录制/持久化；快捷键仍取内置默认 |
| KEY-003 | 冲突检测、重复快捷键处理、平台保留键处理 | U-AUDIT / 需求 C9 | `Registry::conflicts` + `bindings`；内置绑定断言无冲突；无平台保留键表 | partial | `conflict_detection`、`builtin_registry_is_consistent` | 冲突检测已有记录；无 UI/保留键处理 | 无冲突提示 UI；未处理桌面环境保留键；重复绑定策略仅报告 |
| KEY-004 | Profile 绑定快捷键 | U-AUDIT / 需求 C9 | 无实现 | not_started | 无 | 未实现 | 无 Profile/快捷键 |
| KEY-005 | 命令面板可搜索并执行命令 | U-AUDIT / 需求 C9 | `Registry::search` + Palette UI；`run_command` 分发 21/21 条（terminal.clear/select-all 已接 core） | implemented | 单测 `fuzzy_search_prefers_title_prefix`；真机点击执行 `tab.new` 成功；菜单 Select All/Clear 生效 | 2026-11-05 实测通过 | 键盘路径未在沙箱验证（注入不可用） |
| KEY-006 | 插件可注册命令、快捷键、命令上下文 | U-AUDIT / 需求 C9 | 无实现 | not_started | 无 | 未实现 | 无插件 API |
| KEY-007 | 快捷键配置可迁移并提供默认配置 | U-AUDIT / 需求 C9 | `builtin()` 提供默认快捷键；`Shortcut` 可序列化；无配置项/迁移逻辑 | partial | `builtin_registry_is_consistent`；无迁移测试 | 默认绑定已有记录；无迁移 | 无用户自定义配置/版本迁移；默认键未与 UI 分发统一 |

### C10. Themes / Color Schemes / Appearance（7 条）

状态：partial=5，needs_verification=1，not_started=1。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| THEME-001 | 应用 UI Theme：明暗、强调色、控件样式 | U-THEME / 需求 C10 | themes.rs 12 套 ThemeData；palette.rs UiColors::derive；main.rs `push_theme` 写 Slint Palette | partial | palette.rs 7 例；themes.rs 12 例；人工 12 套截图 | 12 套内置 + Auto + UI chrome 同步 | 无自定义强调色/控件样式；UI 主题非 CSS 可扩展 |
| THEME-002 | Terminal Color Scheme：16/256/True Color、前景/背景/光标/选区颜色 | U-THEME / 需求 C10 | palette.rs PaletteSpec/Palette；themes.rs 导入格式；render.rs 选区用反色，无独立 selection 色 | partial | `sgr_256_and_truecolor`；主题导入 6 例 | 16/256/truecolor + fg/bg/cursor 可用 | 无独立 selection/selectionForeground 配置；选区固定反色 |
| THEME-003 | Base16 兼容 | U-THEME / 需求 C10 | themes.rs 支持 Xresources/kitty 等 16 色导入（Base16 常见载体）；无 base16 语义/元数据 | partial | `imports_xresources`、`imports_kitty` | 可消费 Base16 风格 16 色文件 | 无 Base16 命名/校验/调色板映射保证 |
| THEME-004 | 内置 + 导入社区色彩方案；核实上游 150+ 数量 | U-THEME / 需求 C10 | 12 套内置；ThemeRegistry 扫描目录支持 iTerm2/WT/Alacritty/kitty/Xresources/YAML | partial | themes.rs 12 例；人工导入 .logs/imported_settings4.png | 内置 12 + 导入不限量 | 未内置 150+；上游当前数量未核实；改文件需重启 |
| THEME-005 | 字体、fallback、ligatures、Powerline、Nerd Fonts、emoji | U-THEME / 需求 C10 | glyph.rs 固定 JetBrains Mono/DejaVu + CJK 回退；emoji 缺字形留空 | partial | glyph.rs 3 例；人工中文/粗体 | CJK fallback 可用 | 无自选字体/连字/Powerline/Nerd/彩色 emoji |
| THEME-006 | 窗口背景/透明/模糊仅在平台支持时启用；不支持时回退 | U-THEME / 需求 C10 | 无透明/模糊代码；窗口背景为 Palette.colors.surface 纯色 | not_started | 无 | 未实现 | 无平台能力检测/回退；当前纯色背景 |
| THEME-007 | 旧 Tabby CSS Theme 转 Slint 的兼容分级或 WebView 适配 | U-THEME / 需求 C10 | 无 CSS 解析/WebView；docs/tabby-themes.md 已说明 CSS 依赖 Angular DOM/Bootstrap | needs_verification | 待定兼容策略；无实现 | 未核实/未实现 | CSS 不能直接映射 Slint；可选 WebView 层未做，需分级 |

### C11. Progress / Notification / Smart Tabs（6 条）

状态：partial=1，needs_verification=1，not_started=4。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| NOTIFY-001 | 检测终端进度信号（按上游实际协议审计） | U-AUDIT（上游协议待核） | 无实现 | needs_verification | 待审计上游进度协议/来源 | 未核实/未实现 | 无 OSC 9/777/633 等进度解析 |
| NOTIFY-002 | Tab 显示进度或活动状态 | U-AUDIT / 需求 C11 | TabView 只有 title/active；无 progress 字段 | not_started | 无 | 未实现 | 无进度/活动状态 UI |
| NOTIFY-003 | 进程/命令完成通知；通知策略与静音 | U-AUDIT / 需求 C11 | pane.rs 发 CoreEvent::Bell/Exited；main.rs `poll_events` 只消费 Title；无桌面通知 | partial | event.rs 单测；无 UI 消费 | 事件通道存在；用户不可见 | Bell/Exited 被忽略；无策略/静音/命令完成检测 |
| NOTIFY-004 | 后台 Tab 有活动/完成时标记并可跳转 | U-AUDIT / 需求 C11 | 无实现 | not_started | 无 | 未实现 | 无后台活动标记/跳转 |
| NOTIFY-005 | 桌面通知权限、失败、平台差异处理 | U-AUDIT / 需求 C11 | 无实现 | not_started | 无 | 未实现 | 无桌面通知集成 |
| NOTIFY-006 | 插件可注册进度/通知提供者，不能伪造核心会话状态 | U-AUDIT / 需求 C11 | 无实现 | not_started | 无 | 未实现 | 无插件 API/通知总线 |

### C12. Vault / Secrets / Security（6 条）

状态：partial=1，not_started=5。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| SEC-001 | 加密保存 SSH 密码、私钥口令及其他敏感配置 | U-AUDIT / 需求 C12 | 无实现 | not_started | 无 | 未实现 | 无 SSH/凭据存储 |
| SEC-002 | Master Passphrase / Vault 解锁与锁定（按原版核验） | U-AUDIT / 需求 C12 | 无实现 | not_started | 无 | 未实现 | 无 Vault/主密码 |
| SEC-003 | 优先 Linux Secret Service/Keyring；加密文件回退说明 KDF 风险 | U-AUDIT / 需求 C12 | 无实现 | not_started | 无 | 未实现 | 无 Keyring/加密文件后端 |
| SEC-004 | 普通配置、日志、崩溃报告不得出现明文密码/私钥/token | U-AUDIT / 需求 C12 | 当前无 secret 字段；config.rs 只存主题/字号/回滚/shell；无结构化脱敏/崩溃报告 | partial | 代码走查：无凭据字段 | 当前配置不含凭据 | 无脱敏框架；未来接 SSH/Vault 时必须补；配置权限未显式 0600 |
| SEC-005 | 插件默认不能读凭据；使用需 capability | U-AUDIT / 需求 C12 | 无实现 | not_started | 无 | 未实现 | 无插件系统/权限模型 |
| SEC-006 | 主机密钥安全、文件权限、本地 IPC 权限检查 | U-AUDIT / 需求 C12 | 无实现 | not_started | 无 | 未实现 | 无 SSH/IPC；配置文件权限未收紧 |

### C13. Plugin System 与旧插件兼容（13 条）

状态：needs_verification=1，not_started=12。

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| PLUG-001 | 稳定、版本化、具备生命周期的 Plugin API | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无插件宿主/API；Cargo 无 wasmtime/abi 依赖 |
| PLUG-002 | 扩展点：Commands/Hotkeys/Toolbar/Settings/Profile Providers/Tab 菜单/终端菜单/Decorator/Stream Processor/Middleware/Color Scheme/Linkifier | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 12 类扩展点均无接口 |
| PLUG-003 | 注册/注销时管理资源、事件订阅、异步任务 | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无插件生命周期 |
| PLUG-004 | 权限声明：terminal/file/network/clipboard/secret/process 分权 | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无权限模型 |
| PLUG-005 | 插件崩溃隔离、超时、日志、升级与版本兼容检查 | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无插件运行时/诊断 |
| PLUG-006 | 新插件优先 WASM/WIT（先定义 Host API 再引 Runtime） | U-PLUGIN / 需求 C13 | 无实现 | not_started | 无 | 未实现 | 无 WIT/Host API/runtime 评估记录 |
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
| OPS-003 | 插件诊断：加载失败、API 版本冲突、权限拒绝、超时、崩溃 | U-AUDIT / 需求 C14 | 无实现 | not_started | 无 | 未实现 | 无插件系统/诊断 |
| OPS-004 | 配置导入、导出、备份、恢复 | U-AUDIT / 需求 C14 | config.rs 迁移前 `config.toml.v<old>.bak` 备份 + 原子写；无导入/导出/恢复入口 | partial | config.rs `old_config_is_backed_up_and_migrated` | 迁移备份可用；导入导出/恢复缺失 | 只有迁移前自动备份；无用户主动备份/恢复/导出 |
| OPS-005 | 版本信息、更新检查、更新失败反馈；Linux 包管理适配 | U-AUDIT / 需求 C14 | `--help` 打印 CARGO_PKG_VERSION；scripts/package.sh/install.sh；无更新检查 | partial | 人工 `--help`；打包脚本记录 | 版本信息/打包可用 | 无更新检查/失败反馈/自动更新策略 |
| OPS-006 | CLI 参数与 URI/命令入口（如启动 Profile/新 Tab，按上游核验） | U-AUDIT / 需求 C14 | main.rs `parse_args`：--cols/rows/scrollback/font-size/shell/latency(-l)/fps/title/restore 等；`window-title` 属性透传；无 URI/Profile | partial | 真机：`--title SELFTEST` → `xprop _NET_WM_NAME` 输出一致；`-l` 与 `--latency` 等价 | 2026-11-05 实测通过 | 无 URI/Profile 启动；无 --help 自动生成 |

### C15. 平台差异与明确排除项（0 条）

C15 无 `[ ]` 复选框条目；平台差异说明见 §5（不计入 228 条）。

## 5. 附：补充条目（非复选框，不计入 228 条）

| ID | Feature | Upstream evidence | Rust implementation | Status | Test | Result | Limitation |
|---|---|---|---|---|---|---|---|
| UI-SET-000 | Settings 可搜索、可导航的设置布局（B7 引言） | U-AUDIT / 需求 B7 | 设置面板：6 分类导航 + 搜索框（`settings_search` 目录匹配 + 结果浮层点击跳转分类） | partial | 单测 `settings_search_matches_labels_and_keywords`；真机：输入 copy → `settings filter hits=1`，点结果 → `settings goto cat=1`（.logs/settings-search-jump.png） | 分类导航 + 搜索可用 | 搜索基于 10 条精选目录而非全部控件；无高亮/无键盘上下选择 |
| PLAT-001 | Ubuntu X11/Wayland：全局快捷键、窗口定位、边缘停靠能力检测与降级 | U-AUDIT / 需求 C15 | X11 路径已验证（XTest/XDamage/X11 图标）；Wayland 无能力检测/回退代码；无 GPU 自动 winit-software | partial | phase0 X11 记录；无 Wayland 验收 | X11 可用；Wayland 未验证 | Wayland 全局快捷键/定位/停靠均未实现；无运行时能力探测 |
| PLAT-002 | Windows 后续：PowerShell/WSL/Git Bash/Cygwin/MSYS2/Cmder/CMD/Clink 等 | U-AUDIT / 需求 C15 | 无实现 | unsupported | 无 | 明确排除（Ubuntu 第一阶段） | 不在当前交付范围；不得伪造支持 |
| PLAT-003 | macOS 后续：Vibrancy、Keychain、平台 Shell/键盘 | U-AUDIT / 需求 C15 | 无实现 | unsupported | 无 | 明确排除（Ubuntu 第一阶段） | 不在当前交付范围 |
| PLAT-004 | Tabby Web App/self-hosted 不混入桌面版完成度 | U-AUDIT / 需求 C15 | 无实现 | unsupported | 无 | 明确排除 | 无 Web 版目标/代码 |
| PLAT-005 | Tabby 不是 Shell 本身；通过 PTY 启动系统 Shell | U-AUDIT / 需求 C15 | pane.rs `spawn` 用 portable-pty 启动 $SHELL/config.shell；不实现 Bash/Zsh | implemented | smoke PTY 测试；人工启动 bash/zsh | 已按此边界实现 | 无内置 shell/补全；依赖系统 shell |

## 6. 维护规则

- 新增/修改功能时先改本矩阵，再改代码；`implemented` 必须附测试命令或可重复人工步骤。
- 上游结论必须回填 `TABBY_UPSTREAM_AUDIT.md`；未复核前保持 `U-AUDIT`，不得改写成“Tabby 已有”。
- 每条 `needs_verification` 在完成上游审计后必须收敛为 `implemented/partial/not_started/unsupported` 之一。
