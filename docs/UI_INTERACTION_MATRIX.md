# Tervo UI 交互矩阵（UI_INTERACTION_MATRIX）

> 审计日期：2026-11-05 ｜ 起始 HEAD `0ea6231`，复核到并发提交 `8327e20` ｜ 依据：`crates/app/ui/main.slint`、`crates/app/src/main.rs`、`workspace.rs`、`pane.rs` 的实际代码。
> 本轮只读审计，未启动 GUI、未执行构建。表中“当前行为”均可在上述文件定位；“期望行为”来自需求 B 部分，上游未核实处标 `待核`。
> 说明：当前 UI 只有「窗口壳 + TabBar + 终端分屏 + 5 项右键菜单 + 单卡片设置 + 搜索条」；B4/B5/B8/B9/B10/B11 的大部分控件尚不存在，缺失控件集中列在 §8。

## 1. 窗口壳 / TitleBar

| 控件 | 触发方式（鼠标/键盘） | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 窗口外框 | — | `main.slint:226 no-frame: true`；无系统标题栏/边框 | 无边框、自绘标题栏 | 已实现；Wayland 服务端装饰未验 | 启动截图顶部直接是 TabBar |
| TitleBar 空白区 | 左键按住拖动 | `WindowMoveArea` 覆盖整条 TabBar，空白处拖动移动窗口 | 空白区可拖窗；控件/Tab 不误触发 | 已实现；无自动化 | 拖空白区窗口移动；拖 Tab/按钮窗口不动 |
| TitleBar 空白区 | 左键双击 | `title-hit.double-clicked → toggle-maximize()` | 双击最大化/还原（平台规范） | 已实现；仅空白区，Tab 上双击无效 | 双击空白区在最大化/还原间切换 |
| TitleBar 空白区 | 右键 | `title-hit` 右键 → `tabbar-context(mouse-x, tabbar-height)` 打开窗口菜单 | 按上游提供窗口/TabBar 菜单 | 仅有窗口级 5 项菜单；Tab 上右键无 per-tab 菜单 | 空白右键出现菜单；Tab 上右键记录实际行为 |
| 品牌图标 | 无 | 18×18 `Image`，不可点 | 可点/可拖/菜单（按上游） | 无交互；无 tooltip | 点击/右键图标记录无反应 |
| 设置按钮 ⚙ | 左键单击 | `settings-open = !settings-open`；打开单卡片设置 | 打开可搜索/导航的设置页 | 只有 3 组设置，无导航/搜索 | 点击 ⚙ → 仅见主题/字号/回滚 |
| 最小化 ─ | 左键单击 | `Window.set_minimized(true)` | 最小化，可从任务栏恢复 | 已实现 | 点击后窗口最小化 |
| 最大化/还原 □/❐ | 左键单击 | `toggle-maximize()`；图标随 `Window.maximized` 切换 | 最大化/还原，图标同步 | 已实现 | 点击两次观察状态/图标 |
| 关闭 ✕ | 左键单击 | `on_close_window` → `quit_event_loop` | 关闭；有活动任务时确认 | 直接退出，无确认，Drop 全部 PaneHandle 杀 PTY | 开 2 pane 点 ✕，确认无对话框且子进程退出 |
| 窗口左/右边缘 | 左键拖动 | `ResizeHandle direction 3/7`，X11 query_pointer 后 set_size/position | 可缩放；有最小尺寸 | 已实现（最小 320×200） | 拖左右边观察宽度与最小夹取 |
| 窗口上/下边缘 | 左键拖动 | `ResizeHandle direction 1/5` | 可缩放 | 已实现 | 拖上下边观察高度 |
| 窗口四角 | 左键拖动 | `ResizeHandle direction 0/2/4/6` | 四角等比缩放 | 已实现 | 拖四角观察位置+尺寸 |
| 全屏 | F11/菜单 | 无实现 | 全屏/退出全屏 | 缺失；最大化≠全屏 | 按 F11 无反应；检查 `is_fullscreen` 无代码 |

## 2. TabBar / Tab

| 控件 | 触发方式（鼠标/键盘） | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| TabBar 容器 | 左键拖动空白 | `WindowMoveArea` 移动窗口 | 可拖窗；可配置位置（上/下/左/右，待核） | 位置固定顶部；无侧边/底部 | 拖动空白区；检查无位置设置 |
| Tab（非活动） | 左键单击 | `tab-hit.clicked → tab-clicked(i)` → `set_active_tab` | 切换活动 Tab | 已实现 | 多 Tab 单击切换 |
| Tab（活动） | — | 底色 `Palette.bg` + 边框 + 2px accent 底 + 字重 600 | 活动态清晰、非仅颜色 | 已实现（颜色+线+字重）；无连接/进度状态 | 截图对比活动/非活动 |
| Tab 标题 | — | `TabView{title, active}`；标题来自 `poll_events` OSC 0/2，仅活动 Tab 首个 pane | 跟随 Shell/SSH 标题；前缀/后缀/过滤 | 后台 Tab/非首 pane 不更新；无前缀/过滤 | 后台 tab 执行 `printf '\033]0;X\007'`，观察标题是否变化 |
| Tab 关闭 × | 左键单击 | `tab-closed(i)` → `close_tab_inner`，Drop PaneHandle 杀 PTY | 关闭；区分仅关视图/结束会话/后台保留 | 只有结束会话；无确认/生命周期选择 | 关闭后 `pgrep` 子 shell 应消失；检查无对话框 |
| Tab 新建 + | 左键单击 | `new-tab()` → `new_tab_inner` | 新建 Tab；可下拉选 Profile | 仅默认 shell，无下拉 | 点击 + 新 Tab 启动默认 shell |
| Tab 新建 | Ctrl+T | `handle_input` 捕获 `t` → `new_tab_inner` | 快捷键新建 | 已实现 | Ctrl+T 新建 |
| Tab 关闭 | Ctrl+W | 关闭当前 `st.ws.active` | 快捷键关闭（可配置） | 已实现；硬编码 | Ctrl+W 关闭当前 |
| Tab 右键 | 右键单击 Tab | **无 per-tab 处理**；不弹 Tab 菜单 | 关闭/重命名/分屏/复制等（按上游） | 缺失；只有 TabBar 空白右键 | 在 Tab 上右键，记录无菜单或落到空白菜单 |
| Tab 双击 | 左键双击 | 无处理（`tab-hit` 只有 clicked） | 重命名（触发方式待核） | 缺失 | 双击 Tab 无重命名输入框 |
| Tab 拖拽 | 左键拖动 | 无拖拽状态；Tab 是静态模型 | 排序；拖出窗口按上游 | 缺失 | 尝试拖动 Tab，顺序不变 |
| Tab 键盘导航 | Ctrl+Tab/Ctrl+1..9 等 | 无处理 | 上/下一个、序号、最近 Tab | 缺失 | 逐键测试无反应 |
| Tab 中键 | 中键单击 | 无处理 | 按上游（常见为关闭） | 待核/缺失 | 中键点击 Tab 无反应 |

## 3. TabBar 空白右键菜单（窗口菜单）

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 菜单容器 | TabBar 空白右键 | `tabmenu-open`，x 夹取到窗口内，y=32；宽 170 高 156 | 可键盘导航、点外关闭、边界夹取 | y 未夹取，窗口矮时可能溢出；无键盘导航；点外不关闭 | 小窗口右键靠下，检查菜单是否超出 |
| Always on Top | 左键单击 | `pinned = !pinned`，绑定 `Window.always-on-top`；带 ✓ | 置顶开关 | 已实现 | 点两次观察窗口层级/✓ |
| New Tab | 左键单击 | `tabbar-menu-action(1)` → `new_tab_inner` | 新建 Tab | 已实现 | 点击后 Tab 数 +1 |
| Settings | 左键单击 | `settings-open = true` | 打开设置 | 已实现 | 点击打开设置卡片 |
| Restart Session | 左键单击 | `restart_session`：存快照→setsid 起新进程→退出旧进程 | 恢复工作区 | 仅手动重启；不恢复进程/历史 | 分屏+cd 后点击，新实例 tab/pane/cwd 一致 |
| Close Window | 左键单击 | `quit_event_loop` | 关闭窗口（有确认） | 直接退出，无确认 | 点击后进程退出 |
| 菜单键盘 | 方向键/Enter/Esc | 无键盘处理；Esc 仅对终端菜单 `menu-open` 生效 | 键盘可选/确认/关闭 | 缺失 | 打开菜单按方向键/Enter 无反应 |

## 4. 分屏 / Pane

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| Pane 区域 | 左键单击 | `ime.focus()` + `pane-clicked` → `focus_pane`；键盘归属该 pane | 聚焦；清晰视觉状态 | 聚焦逻辑可用；`PaneView.focused` 未被 Slint 使用，无视觉态 | 点击不同 pane 输入；截图无焦点标记 |
| Pane 区域 | 左键拖选 | 按下 `selection_start`；移动 `selection_extend`；抬起自动复制 | 跨行/反向/滚动扩展；可配置 copy-on-select | 跨行/反向可用；无滚动扩展；copy-on-select 强制开 | 反向拖选验证行首；滚动时选区不扩展 |
| Pane 区域 | 左键双击/三击 | 无特殊处理（按单击处理） | 选词/选行（按上游） | 缺失 | 双击单词，仅移动光标/无选区 |
| Pane 区域 | Ctrl+左键点击链接 | core `Frame::hyperlink_at` 优先 OSC 8，`Frame::url_at` 识别普通 http/https/file 并按显示列处理宽字符/组合字符；app 在非鼠标上报时 Ctrl+点击打开，失败显示底部浮层并写日志；右键菜单出现 Open Link / Copy Link | 悬停手型；Ctrl+点击打开 URL/路径；区分本地/远端 | URL/OSC 8 已接 UI；IP/路径与 hover 手型缺失；远端/本地未区分 | headless Xvfb :114 + fake xdg-open：普通 URL/OSC 8/OSC 8 优先/失败提示/菜单 Open Link 均记录命中 |
| Pane 区域 | 右键（非鼠标上报） | `pane-right-clicked` → 5 项终端菜单 | 复制/粘贴/查找/清屏/Profile/拆分（按上游） | 只有 5 项；无 Find/Clear/Profile | 右键观察菜单项 |
| Pane 区域 | 右键/中键（鼠标上报模式） | `p.mouse-reporting` 时转发给 PTY，不弹菜单 | TUI 收到右键/中键；Shift 绕过 | 已实现；1002 拖动按钮错误（见主矩阵） | 在 vim `:set mouse=a` 下右键，检查 PTY 字节 |
| Pane 区域 | 滚轮 | `scroll-event`：鼠标上报模式发 64/65；否则本地回滚 | 上报或回滚；Shift 绕过 | 已实现 | `vim` 中滚轮翻页；普通 shell 滚历史 |
| Pane 区域 | Shift+左键 | 强制本地选词（绕过上报） | 逃生门 | 已实现 | 鼠标模式下 Shift 拖选可复制 |
| 分屏分隔线（左右） | 左键拖动 | 4px 命中区（1px 线）；`divider-dragged` → `set_ratio`；比例 0.1–0.9 | 拖动调比例；最小尺寸 | 已实现；无双击复位/键盘调整 | 拖线观察比例与夹取 |
| 分屏分隔线（上下） | 左键拖动 | 同上，ns-resize | 同上 | 同上 | 同上 |
| Pane 焦点视觉 | — | 无 | 当前 Pane 有清晰边框/标记 | 缺失 | 4 pane 下无法从画面判断焦点 |
| Pane 键盘焦点切换 | 快捷键 | 无 | 相邻 Pane 切换 | 缺失 | 按 Alt+方向等无反应 |
| Pane 最大化/还原 | 菜单/快捷键 | 无 | 若上游有则实现 | 待核/缺失 | 检查无 zoom 菜单 |
| Pane 拖拽重排 | 左键拖 Pane | 无 | 移动/重排/重组 | 缺失 | 拖 Pane 无布局变化 |
| 广播输入 | 开关/UI | 无 | 可开关、显示范围、危险提示 | 缺失 | 无广播按钮/状态 |

## 5. 终端右键菜单（5 项）

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 菜单容器 | Pane 右键 | `menu-open`，坐标内容区，x/y 均夹取；150×156 | 边界内、点外/Esc 关闭、键盘导航 | 点外不关闭；无键盘导航；Esc 只在 `menu-open` 且搜索未开时 | 菜单打开后点终端空白，菜单仍在 |
| Copy | 左键单击 | `menu-action(3)` → `copy_current_selection` | 复制选区 | 已实现（无选区则无动作） | 选文本→右键 Copy→剪贴板一致 |
| Paste | 左键单击 | `menu-action(4)` → `paste_into_focused` | 粘贴；多行确认 | 直接粘贴；无多行确认 | 复制多行→Paste，检查是否直接执行 |
| Split Right | 左键单击 | `split_pane(id, Row)`，新 pane 聚焦 | 左右分屏 | 已实现 | 菜单后 pane 数 +1 |
| Split Down | 左键单击 | `split_pane(id, Col)` | 上下分屏 | 已实现 | 同上，方向为上下 |
| Close Pane | 左键单击 | `close_pane_inner`；最后一个 pane 删 Tab/退应用 | 关闭；最后一个按产品定义 | 已实现；无空状态/确认 | 关到最后一个，应用退出 |
| Esc 关闭 | Esc | 隐藏的 IME TextInput 捕获；`menu-open` 时关菜单 | 关闭菜单 | 只处理终端菜单；TabBar 菜单不响应 Esc | 打开两种菜单分别按 Esc |

## 6. 搜索

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 搜索条 | Ctrl+F | `open_search`；右上角 300×34 显示条；搜索框不是 TextInput | 打开查找面板并聚焦 | 显示条可用；无真实输入焦点 | Ctrl+F 出现搜索条 |
| 搜索输入 | 键盘字符/退格 | `handle_input` 在 `search_open` 时把字符追加到 `search_query`，退格 pop，增量 `run_search` | 输入关键词、可鼠标选中/编辑 | 可用；但搜索框本身不可点击/选区/光标 | 输入 `abc` 观察高亮；点击搜索条无光标 |
| 上一个/下一个 | Enter / Shift+Enter | Enter 向前、Shift+Enter 向后；命中滚动到视口并高亮 | 上/下一个、匹配计数 | 已实现上下跳；无计数 | 多次 Enter 观察命中位置 |
| 大小写/正则 | 设置/开关 | 无；`regex_escape` 强制纯文本 | 可配置大小写/正则（按上游） | 缺失 | 搜 `ABC` 与 `abc` 行为相同；无开关 |
| 无命中提示 | — | `search-status = "no match"` | 明确空状态 | 已实现（英文） | 搜不存在字符串显示 no match |
| 关闭 | Esc | `close_search`，清所有 pane 高亮 | Esc/关闭按钮 | 已实现 Esc；无关闭按钮 | Esc 后高亮消失 |
| 搜索框交互 | 鼠标 | 无 TouchArea/TextInput | 可点击定位、选择、复制 | 缺失 | 点击搜索条无反应 |
| 匹配计数 | — | 无 | 如 3/17 | 缺失 | 无计数显示 |

## 7. 设置面板

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 遮罩 | 左键单击 | `settings-open = false` | 点外关闭 | 已实现 | 点遮罩设置关闭 |
| 卡片 | — | 420px 宽，高度按主题数计算（最多 4 行主题） | 可滚动/导航的设置页 | 单卡片、无分类导航/搜索 | 检查仅 3 组设置 |
| Auto 主题 | 左键单击 | `set-theme("auto")`；显示 `Auto → <resolved>` | 跟随系统，可指定深浅主题 | 已实现；非 GNOME 回退有限 | 切换系统深浅色观察 |
| 主题网格 | 左键单击 | `for choice in theme-choices`，4 列，Flickable 滚动 | 选择、预览、导入/导出 | 12 内置+导入可用；无预览/导出 | 点击主题即时换色；滚动列表 |
| 字号 −/+ | 左键单击 | `bump-font-size(-1/+1)`，8–32，重建字体/渲染器，即时保存 | 字号调整并显示 | 已实现 | 点击 ± 观察字号/配置 |
| 回滚 −/+ | 左键单击 | `bump-scrollback(-500/+500)`，500–100000，保存；只对新 pane 生效 | 回滚行数可配 | 已实现（对新 pane） | 改后新开 pane 的缓冲变化 |
| Unlimited scrollback | 左键单击 ToggleRow | 写 `scrollback_unlimited`；true 时新 pane 用 100,000 行安全上限；点 +/- 自动关回固定值 | 无限/有界可配并提示内存 | 已实现（对新 pane） | 设置页点击后 config + 日志；Xvfb :118 e2e |
| 可听铃 / 视觉铃 | 左键单击 ToggleRow | 独立写 `audible_bell`/`visual_bell`；可听铃默认关，BEL 时 X11 Bell | 两个独立开关 | 已实现 | Xvfb :118 BEL → `[bell] audible backend=ok` |
| Scroll on output / keystroke | 左键单击 ToggleRow | 写配置；output 立即更新所有 pane 的 actor，keystroke 每次按键读取 | 上滚不抢/按键回底 | 已实现 | core smoke + 设置页写盘断言 |
| Scrollbar policy | 左键单击 ChoiceBtn | `always`/`auto`/`never`，立即刷新所有 pane overlay | 滚动条三档 | 已实现 | Xvfb :118 像素检测 213/213 vs 0 |
| Backspace / Delete binding | 左键单击 ChoiceBtn | 写 `backspace_binding`/`delete_binding`；每次按键读取编码 | 编码可配、默认 GNOME | 已实现 | XTest keylog 字节断言 |
| 设置关闭 | Esc | 无处理 | Esc 关闭 | 缺失 | 设置打开按 Esc，仍打开 |
| 设置键盘导航 | Tab/方向键 | 无 | 可键盘操作 | 缺失 | Tab 键无焦点移动 |
| 设置搜索 | 输入 | 无 | 可搜索设置项 | 缺失 | 无搜索框 |

## 8. 缺失控件（需求 B 部分要求，但 UI 中不存在）

| 控件 | 触发方式（期望） | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| Sidebar | 点击/快捷键 | 无 | 显示/隐藏、宽度调整、连接入口 | 缺失 | 界面无侧栏 |
| Toolbar | 点击 | 无 | 按钮/分组/插件按钮/Tooltip | 缺失 | 界面无工具栏 |
| Connection Manager | 菜单/快捷键 | 无 | 始终易访问 | 缺失 | 无入口 |
| Profile Selector | 新 Tab 下拉 | 无 | Shell/SSH/Serial/Telnet Profile 选择 | 缺失 | 新建 Tab 无下拉 |
| Command Palette | Ctrl+Shift+P 等（待核） | `commands.rs` 已有 21 命令 + 模糊搜索；无面板 UI/执行接线 | 模糊搜索命令/Profile/设置并执行 | 数据层已有，UI 缺失 | Ctrl+Shift+P 无反应；`Registry::search` 单测通过 |
| 状态/通知区 | — | 无 | 连接/任务/进度/完成 | 缺失 | 无状态栏 |
| Tab 重命名 | 双击/快捷键 | 无 | 输入框改名 | 缺失 | 双击无反应 |
| Tab 拖拽排序 | 拖动 | 无 | 排序/拖出 | 缺失 | 拖动无效 |
| 全屏 | F11 | 无 | 全屏/退出 | 缺失 | F11 无反应 |
| 快捷键设置页 | 设置导航 | 无 | 录制/冲突/多段键 | 缺失 | 设置无 Hotkeys |
| 插件管理页 | 设置导航 | 无 | 列表/安装/权限/目录 | 缺失 | 设置无 Plugins |
| 连接状态/进度条 | — | 无 | Tab/Pane 上可见 | 缺失 | 无连接场景 |
| 桌面通知 | — | 无 | 命令完成/后台活动 | 缺失 | Bell/Exited 无 UI |
| Tooltip/Accessible Label | 悬停/读屏 | 只有字形按钮，无 label | 所有图标有提示/无障碍名 | 缺失 | 悬停无 tooltip；读屏无名称 |

## 9. 验收建议（可直接执行）

1. **窗口壳**：启动→拖空白→双击→最小化→任务栏恢复→最大化→拖边/角→关闭；每一步截图并记录子进程数。
2. **Tab**：开 3 Tab→点击切换→Ctrl+T/W→后台 Tab 发 OSC 标题→右键 Tab/空白→尝试拖动/双击重命名。
3. **分屏**：右键 Split Right→Split Down→拖分隔线到 10%/90%→点击各 pane 输入→Close Pane 到最后一个。
4. **终端交互**：拖选/反向拖选→自动复制→Ctrl+Shift+C/V→Ctrl+F 搜索/Enter/Shift+Enter/Esc→滚轮回滚→vim 鼠标上报→Shift+点击绕过。
5. **设置/主题**：⚙→切 Auto/主题→±字号/回滚→点遮罩关闭→Esc 验证（当前应不关闭）→改配置后重启验证持久化。
6. **回归记录**：每项保留 before/after 截图到 `.logs/ui-audit/`，与 `docs/FEATURE_MATRIX.md` 的 Test/Result 对应。
