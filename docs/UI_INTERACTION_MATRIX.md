# xtervo UI 交互矩阵（UI_INTERACTION_MATRIX）

> 审计日期：2026-11-05 ｜ 起始 HEAD `0ea6231`，复核到并发提交 `8327e20` ｜ 依据：`crates/app/ui/main.slint`、`crates/app/src/main.rs`、`workspace.rs`、`pane.rs` 的实际代码。
> 本轮只读审计，未启动 GUI、未执行构建。表中“当前行为”均可在上述文件定位；“期望行为”来自需求 B 部分，上游未核实处标 `待核`。
> 说明：当前 UI 只有「窗口壳 + TabBar + 终端分屏 + 13 项右键菜单 + 设置 + 搜索条」；B4/B5/B8/B9/B10/B11 的大部分控件尚不存在，缺失控件集中列在 §8。
> 2026-11-06 增补（`feat/term-menu`）：§5 菜单项、§6 查找 Match case/regex 与非法 regex、Read Only/Reset/Save Contents；
> 细节与 Xvfb :119 证据见 [`terminal-menu-find.md`](terminal-menu-find.md)。

## 1. 窗口壳 / TitleBar

| 控件 | 触发方式（鼠标/键盘） | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 窗口外框 | — | `main.slint:226 no-frame: true`；无系统标题栏/边框 | 无边框、自绘标题栏 | 已实现；Wayland 服务端装饰未验 | 启动截图顶部直接是 TabBar |
| TitleBar 空白区 | 左键按住拖动 | `WindowMoveArea` 覆盖整条 TabBar，空白处拖动移动窗口 | 空白区可拖窗；控件/Tab 不误触发 | 已实现；无自动化 | 拖空白区窗口移动；拖 Tab/按钮窗口不动 |
| TitleBar 空白区 | 左键双击 | `title-hit.double-clicked → toggle-maximize()` | 双击最大化/还原（平台规范） | 已实现；仅空白区，Tab 上双击无效 | 双击空白区在最大化/还原间切换 |
| TitleBar 空白区 | 右键 | `title-hit` 右键 → `tabbar-context(mouse-x, tabbar-height)` 打开窗口菜单 | 按上游提供窗口/TabBar 菜单 | 仅有窗口级 5 项菜单；Tab 上右键无 per-tab 菜单 | 空白右键出现菜单；Tab 上右键记录实际行为 |
| 品牌图标 | 无 | 18×18 `Image`，不可点 | 可点/可拖/菜单（按上游） | 无交互；无 tooltip | 点击/右键图标记录无反应 |
| 设置按钮 ⚙ | 左键单击 | `settings-open = !settings-open`；打开无 PTY 的 Settings Tab（或 `XTERVO_SETTINGS_LEGACY=1` 的居中 modal） | 打开可搜索/导航的设置页 | 已实现：8 分类/分组/图标/搜索跳转/下拉/分段/开关 | 点击 ⚙ → 打开 Settings Tab，Esc/Tab ×/终端 Tab 关闭 |
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
| Tab 新建 | Ctrl+Shift+T | `handle_input` 捕获 `t` → `new_tab_inner` | 快捷键新建 | 已实现 | Ctrl+T 新建 |
| Tab 关闭 | Ctrl+Shift+W | 关闭当前 `st.ws.active` | 快捷键关闭（可配置） | 已实现；硬编码 | Ctrl+Shift+W 关闭当前；Ctrl+W 原样发给 shell |
| Tab 右键 | 右键单击 Tab | **无 per-tab 处理**；不弹 Tab 菜单 | 关闭/重命名/分屏/复制等（按上游） | 缺失；只有 TabBar 空白右键 | 在 Tab 上右键，记录无菜单或落到空白菜单 |
| Tab 双击 | 左键双击 | 无处理（`tab-hit` 只有 clicked） | 重命名（触发方式待核） | 缺失 | 双击 Tab 无重命名输入框 |
| Tab 拖拽 | 左键拖动 | 无拖拽状态；Tab 是静态模型 | 排序；拖出窗口按上游 | 缺失 | 尝试拖动 Tab，顺序不变 |
| Tab 键盘导航 | Ctrl+PageUp/Down、Alt+1..0 等 | 无处理 | 上/下一个、序号、最近 Tab | 缺失 | 逐键测试无反应 |
| Tab 中键 | 中键单击 | 无处理 | 按上游（常见为关闭） | 待核/缺失 | 中键点击 Tab 无反应 |

## 3. TabBar 空白右键菜单（窗口菜单）

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 菜单容器 | TabBar 空白右键 | `tabmenu-open`，x 夹取到窗口内，y=32；宽 170 高 156 | 可键盘导航、点外关闭、边界夹取 | y 未夹取，窗口矮时可能溢出；无键盘导航；点外不关闭 | 小窗口右键靠下，检查菜单是否超出 |
| Always on Top | 左键单击 | `pinned = !pinned`，绑定 `Window.always-on-top`；带 ✓ | 置顶开关 | 已实现 | 点两次观察窗口层级/✓ |
| New Tab | 左键单击 | `tabbar-menu-action(1)` → `new_tab_inner` | 新建 Tab | 已实现 | 点击后 Tab 数 +1 |
| Settings | 左键单击 | `settings-open = true`；齿轮 Tab 进入 TabBar，内容区整页显示 | 打开设置 | 已实现 | 点击打开设置 Tab，不创建 PTY |
| Restart Session | 左键单击 | `restart_session`：存快照→setsid 起新进程→退出旧进程 | 恢复工作区 | 仅手动重启；不恢复进程/历史 | 分屏+cd 后点击，新实例 tab/pane/cwd 一致 |
| Close Window | 左键单击 | `quit_event_loop` | 关闭窗口（有确认） | 直接退出，无确认 | 点击后进程退出 |
| 菜单键盘 | 方向键/Enter/Esc | 无键盘处理；Esc 仅对终端菜单 `menu-open` 生效 | 键盘可选/确认/关闭 | 缺失 | 打开菜单按方向键/Enter 无反应 |

## 4. 分屏 / Pane

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| Pane 区域 | 左键单击 | `ime.focus()` + `pane-clicked` → `focus_pane`；键盘归属该 pane | 聚焦；清晰视觉状态 | 聚焦逻辑可用；`PaneView.focused` 未被 Slint 使用，无视觉态 | 点击不同 pane 输入；截图无焦点标记 |
| Pane 区域 | 左键拖选 | 按下 `selection_start`；移动 `selection_extend`；抬起自动复制 | 跨行/反向/滚动扩展；可配置 copy-on-select | 跨行/反向可用；无滚动扩展；copy-on-select 强制开 | 反向拖选验证行首；滚动时选区不扩展 |
| Pane 区域 | 左键双击/三击 | 无特殊处理（按单击处理） | 选词/选行（按上游） | 缺失 | 双击单词，仅移动光标/无选区 |
| Pane 区域 | Ctrl+左键点击链接 | core `Frame::hyperlink_at` 优先 OSC 8，`Frame::link_at` 识别普通 http/https/file、IPv4[:port] 与本地路径，并按显示列处理宽字符/组合字符；app 无按键 hover 切手型（按格去重，不重绘终端），非鼠标上报时 Ctrl+点击打开：URL 走白名单，本地路径 `~`/cwd 解析 + 存在/权限检查后 `xdg-open`（无 shell），失败只提示；远端 Profile 路径不本地打开；右键 Open/Copy Link 支持 URL/路径 | 悬停手型；Ctrl+点击打开 URL/路径；区分本地/远端 | URL/OSC 8/本地路径已接 UI；hover 手型/离开恢复；裸 IP 只识别不打开；远端路径不本地打开 | Xvfb :128 `scripts/e2e_url_paths.sh`：URL hover 手型（XFixes 光标图 16x16 X→24x24 pointer→恢复箭头）、OSC 8、裸 IP 不切手型、Ctrl+点击真实文件/缺失/无权限、右键路径 Open Link/URL Open+Copy Link、底部提示截图 |
| Pane 区域 | 右键（非鼠标上报） | `pane-right-clicked` → 13 项终端菜单（带链接 15 项），含 Find/Clear/Save Contents/Read Only/Reset | 复制/粘贴/查找/清屏/复位/保存/Profile/拆分（按上游） | 13 项已含主要终端动作；无 Profile/缩放项 | 右键观察菜单；Xvfb :119 `menu_open.png` |
| Pane 区域 | 右键/中键（鼠标上报模式） | `p.mouse-reporting` 时转发给 PTY，不弹菜单 | TUI 收到右键/中键；Shift 绕过 | 已实现；1002 拖动按钮错误（见主矩阵） | 在 vim `:set mouse=a` 下右键，检查 PTY 字节 |
| Pane 区域 | 滚轮 | `scroll-event`：鼠标上报模式发 64/65；否则本地回滚 | 上报或回滚；Shift 绕过 | 已实现 | `vim` 中滚轮翻页；普通 shell 滚历史 |
| Pane 区域 | Shift+左键 | 强制本地选词（绕过上报） | 逃生门 | 已实现 | 鼠标模式下 Shift 拖选可复制 |
| 分屏分隔线（左右） | 左键拖动 | 4px 命中区（1px 线）；`divider-dragged` → `set_ratio`；比例 0.1–0.9 | 拖动调比例；最小尺寸 | 已实现；无双击复位/键盘调整 | 拖线观察比例与夹取 |
| 分屏分隔线（上下） | 左键拖动 | 同上，ns-resize | 同上 | 同上 | 同上 |
| Pane 焦点视觉 | — | 无 | 当前 Pane 有清晰边框/标记 | 缺失 | 4 pane 下无法从画面判断焦点 |
| Pane 键盘焦点切换 | 快捷键 | 无 | 相邻 Pane 切换 | 缺失 | 按 Alt+方向等无反应 |
| Pane 最大化/还原 | 双击 pane 右上拖拽手柄 / Ctrl+Shift+M | `handle-hit.double-clicked → pane-toggle-zoom(p.id)`；`toggle_pane_zoom_for` 校验 pane 属于活动 Tab；最大化后手柄保留 | 双击最大化，再次双击还原 | 已实现 | 分屏后双击右 pane 手柄 → `[pane] zoom on pane=2`；再双击 → `[pane] zoom off pane=2` |
| Pane 拖拽重排 | 左键拖 Pane | 无 | 移动/重排/重组 | 缺失 | 拖 Pane 无布局变化 |
| 广播输入 | 开关/UI | 无 | 可开关、显示范围、危险提示 | 缺失 | 无广播按钮/状态 |

## 5. 终端右键菜单（13 项；命中链接时 15 项）

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 菜单容器 | Pane 右键 | `menu-open`，内容区坐标；无链接 13 行、带链接 15 行；点外部/Esc 关闭 | 边界内、点外/Esc 关闭、键盘导航 | 点外/Esc 已实现；无键盘导航 | 右键后点空白/Esc 关闭 |
| Copy / Paste | 左键 | action 3/4；粘贴遵守多行确认与 bracketed paste | 复制/粘贴 | 已实现 | 选文本→Copy；多行→Paste 确认 |
| Split Right / Split Down | 左键 | action 0/1 → `split_pane` | 左右/上下分屏 | 已实现 | 菜单后 pane 数 +1 |
| Close Pane | 左键 | action 2 → `close_pane_inner` | 关闭当前 pane | 已实现 | 关闭后 pane 消失 |
| Find | 左键 | action 5 → `terminal.find` | 打开查找条 | 已实现（Ctrl+Shift+F 同路径） | 菜单 Find 后出现查找条 |
| Select All / Clear | 左键 | action 6/7 → core select-all/clear-screen | 全选/清屏 | 已实现 | 全选后复制；清屏后屏幕空 |
| Save Contents… | 左键 | action 14 → 显式路径弹窗；core `contents()` 保存 scrollback+视口；失败红字 | 保存内容到明确路径 | 已实现最小版；非系统文件对话框 | 菜单打开弹窗→Enter→文件存在且含历史 |
| Read Only | 左键 | action 11 → 切换当前 pane 只读；pane 角标 `READ ONLY` + 菜单标签 + 浮层 | 可切换、状态可见、关闭恢复 | 已实现；每 pane 不持久化 | 开启后键入/粘贴不写 PTY，关闭恢复 |
| Reset / Reset and Clear | 左键 | action 12/13 → core `Ctrl::Reset` / `ResetAndClear`（RIS + 清回滚） | 复位终端状态，不关 pane | 已实现；Reset 也清 scrollback（上游限制） | 复位后屏幕空、会话仍可输入 |
| Broadcast Input | 左键 | action 8 切换 broadcast | 多 pane 同步输入 | 已实现 | 两 pane 下键入同时到达 |
| Open / Copy Link | 左键 | 命中链接时 action 9/10 | 打开/复制链接 | 已实现 | 右键链接→Open/Copy |
| Esc / 点外部关闭 | 键盘/鼠标 | 隐藏 IME TextInput 捕获 Esc；`menu-open` 时关闭；外部 TouchArea 关闭 | 关闭菜单 | 已实现 | 两种方式都能关 |

## 6. 搜索

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 搜索条 | Ctrl+Shift+F | `open_search`；右上角 360×62 浮层；查询区是 `TextInput`，打开时 `init.focus()` | 打开查找面板并聚焦 | 已实现；有真实文本光标 | Ctrl+F 出现搜索条并直接可输入（`count_2of3.png`、`reopen_focus.png`） |
| 搜索输入 | 键盘字符/退格 | TextInput 原生编辑，`edited` → Rust 增量 `run_search`；空查询清高亮并显示 `0/0` | 输入关键词 | 已实现 | 输入 `alpha` 看高亮与计数（Xvfb :127） |
| Match case | 点击开关 / Alt+C | `search-match-case` → core `SearchOptions.match_case`；`(?-i:)` 强制区分大小写 | 大小写开关 | 已实现 | 搜 `ALPHA`：开=不命中，关=命中 |
| Match regex | 点击开关 / Alt+R | `search-regex` 同排开关；regex 直接交给 alacritty `RegexSearch` | 正则开关 | 已实现 | 搜 `a.c` 命中 `abc`；`[` 报错 |
| 非法 regex | 输入 `[` 等 | `search_with` 返回 Err；条内显示 `invalid regex`、边框 danger 色，完整错误进日志，旧高亮清掉 | 明确错误 | 已实现（英文短文本） | `invalid_regex.png`；日志 `error=invalid regex` |
| 上一个/下一个 | Enter / Shift+Enter | Enter 向前、Shift+Enter 向后（origin 前进/后退一格并环绕）；命中滚动到视口并高亮 | 上/下一个 | 已实现；计数同步刷新 | `1/3 → 2/3 → 3/3 → 1/3`，反向回 `3/3`（core 测试 + Xvfb 日志） |
| 无命中提示 | — | `search-status = "0/0"`（无命中时） | 明确空状态 | 已实现 | 搜 `zzz` 显示 `0/0`（`count_0of0.png`） |
| 关闭 | Esc | `close_search` 清所有 pane 高亮/错误，`focus-terminal()` 归还 IME/键盘焦点，spot 标脏 | Esc/关闭按钮 | 已实现 Esc + 焦点归还；无关闭按钮 | Esc 后 `echo FOCUS_OK` 到达 shell（`terminal_focus.png`） |
| 搜索框交互 | 鼠标 | TextInput 点击聚焦/移动光标、拖选、Ctrl+A/C/V；空白区由背景 TouchArea 吃掉，不穿透终端 | 可编辑 | 已实现 | `mouse_caret.png`、`mouse_select.png`；拖选+C 剪贴板 = 当前查询；Esc 后查询不泄漏到终端 |
| 匹配计数 | — | core `SearchHit.index/total`（alacritty `RegexIter` 扫 scrollback+视口）；UI 显示 `当前/总数`，非法 regex 显示 `invalid regex` | 如 `3/17` | 已实现 | `count_2of3.png`、`count_3of3.png`、`count_0of0.png`；app/core 单测 |

## 7. 设置（Settings Tab）

| 控件 | 触发方式 | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| 打开 | 齿轮 ⚙ / 命令 `settings.open` | `settings-open = true`；TabBar 追加齿轮 Tab，内容区显示全页设置 | 打开设置且不创建 PTY | 已实现；命令面板/Vault 提示同样可打开 | `[ui] settings open=true`；`workspace` tabs 数不变 |
| 关闭 | Esc / 页头 ✕ / 设置 Tab × / 点终端 Tab | `close-settings()` → `settings-toggled(false)`，归还 IME 焦点 | 四种入口都能关 | 已实现 | 四种入口后终端可继续输入（read-shell leak 测试） |
| 分类 | 左侧 NavItem | 8 类分 3 组（PERSONALIZE/WORKSPACE/SYSTEM），线性图标，选中 accent 边条 | 分组 + 图标 + 二级分组 | 已实现 | 截图 `settings_open/terminal/profiles` |
| 搜索 | 顶部输入 | 索引名称/关键词；点击结果切分类 + 滚动并高亮卡片 | 搜索并跳转分类/设置 | 已实现（跳到卡片级） | `settings filter q="sixel" hits=1` → `jump section=images` |
| 主题 | 下拉框 | 12 内置 + Auto；选中即时换色；`theme-display` 显示 Auto 解析结果 | 下拉/分段而非一排 chip | 已实现；无独立预览 | 截图 `settings_dropdown`；切 Yaru Light 即时生效 |
| 光标 / 滚动条 / 按键编码 | 分段选择器 | Block/Underline/I-beam、System/On/Off、Always/Auto/Never、Backspace/Delete 编码 | 分段选择器 | 已实现 | e2e 写 config + `[ui]` 回调日志 |
| 布尔项 | switch | Bold is bright、bell、copy、scroll、sixel、TabBar、Quake 等全部保留 | switch | 已实现 | e2e Unlimited/Audible/Scroll output |
| 数值项 | Stepper/Slider | 字号/回滚步进；背景 dim/opacity 滑杆 | slider/stepper | 已实现 | 截图 + 手动拖动；配置即时保存 |
| Profiles | 侧栏 Profiles | 内嵌列表（打开/星标默认/编辑/删除/新建）+ Manage 全面板 | 设置内可管理 Profile | partial：完整搜索/复制仍在全面板 | 截图 `settings_profiles`；Manage 模态叠在设置页上 |
| 设置 Tab 与终端 Tab | 点击终端 Tab | 关闭设置并切到该终端 Tab | 不污染 workspace | 已实现 | `[ui] settings open=false` + 继续输入 |
| 回退 | 启动 `XTERVO_SETTINGS_LEGACY=1` | 居中 820×620 modal，同一套控件，TabBar 不显示 Settings Tab | 新 UI 坏了仍能设置 | 已实现 | 截图 `settings_legacy`；日志 `settings ui=legacy-modal` |
| 小窗口 | 拖角缩到 700×500 / 480×360 | 侧栏 156–164px、内容 Flickable、搜索框保留 | 小窗口可用 | 已实现（480×360 不裁剪分类名） | 截图 `settings_small/tiny` |
| 键盘导航 | Tab/方向键 | 仅 Esc 处理；控件靠鼠标/搜索 | 完整键盘导航 | 缺失 | Tab 无遍历 |

## 8. 缺失控件（需求 B 部分要求，但 UI 中不存在）

| 控件 | 触发方式（期望） | 当前行为 | 期望行为 | 差异 | 验收方式 |
|---|---|---|---|---|---|
| Sidebar | 点击/快捷键 | 无 | 显示/隐藏、宽度调整、连接入口 | 缺失 | 界面无侧栏 |
| Toolbar | 点击 | 无 | 按钮/分组/插件按钮/Tooltip | 缺失 | 界面无工具栏 |
| Connection Manager | 菜单/快捷键 | 无 | 始终易访问 | 缺失 | 无入口 |
| Profile Selector | 新 Tab 下拉 | 无 | Shell/SSH/Serial/Telnet Profile 选择 | 缺失 | 新建 Tab 无下拉 |
| Command Palette | Ctrl+Shift+P | `commands.rs` 30 命令 + 模糊搜索；Slint 浮层 + `run_command` 分发；支持键盘输入/↑↓/Enter/Esc | 模糊搜索命令/Profile/设置并执行 | 命令路径已接 30/30；Profile/设置项未接入搜索 | Xvfb :119 键盘搜索 `reset terminal` / `reset and clear` 并执行成功；日志 `[ui] run command ...` |
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
5. **设置/主题**：⚙→Settings Tab→切分类/主题下拉→搜索 sixel 跳转→Esc 关闭→终端继续输入；`XTERVO_SETTINGS_LEGACY=1` 重启验证居中 modal 回退。
6. **回归记录**：每项保留 before/after 截图到 `.logs/ui-audit/`，与 `docs/FEATURE_MATRIX.md` 的 Test/Result 对应。
