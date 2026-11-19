# Changelog

Tervo 的版本与 tag 规则见 `docs/RELEASING.md`。

## [Unreleased]

### Changed

- native SSH 不再调用 `ssh -G`/`ssh-keygen -F`：新增纯 Rust ssh_config 与
  known_hosts 解析（hashed host、通配、`!` 否定、`@revoked`）。
- native SSH 支持 ProxyJump（多跳）、本地/动态/远程端口转发、agent
  forwarding；加密私钥 passphrase 走 Vault（Profile 表单可保存）。
- Serial 设置页支持端口枚举/Refresh/1.5s 热插拔刷新，以及 DTR/RTS 开关；
  这些参数在打开串口时应用。
- 连接后端改为进程内库：SSH 默认走 `russh`（password/key/agent + PTY/shell +
  resize；known_hosts 必须已确认），串口默认走 `serialport`（5-8 数据位/
  parity/stop/flow）。ProxyJump/转发/agent-forward/extra_args/加密私钥/
  未保存 Vault 密码/未知主机自动回退系统 `ssh`；串口打不开回退 `socat`。
  可用 `transport = "system"` / `"socat"` 显式选择旧路径。详见
  `docs/CONNECTION_BACKENDS.md`。

### Fixed

- 全仓 review（2026-11-18）后的安全/数据/输入修复：
  - OSC 52：远端 pane 默认禁止查询本机剪贴板（新配置 `osc52_remote_read=false`）；
    `osc52` 未知值按 deny，不再 fail-open。剪贴板复制日志只记长度，不记内容。
  - 原子写：config 备份恢复、录制导出改临时文件 + rename；畸形 Xresources
    颜色不再在 UTF-8 字符中间切片 panic；e2e 拒绝删除已存在的 `E2E_OUT`。
  - 多窗口：WorkspaceStore 移入 `SharedServices` 单例；恢复成功后先写新快照
    再消费旧快照；重开失败把快照放回栈；桌面通知限 4 个在飞 +
    2s 超时；关闭非 primary 窗口最后一个 Tab/pane 只关窗口，不再退出整个进程。
  - 输入：keyboard/OSC 扫描器跳过 UTF-8 续字节（中文/Cyrillic 不再吞掉
    kitty/OSC 序列）；PTY chunk 携带 pgrp，键盘 flags 按 owner pgrp 复位，
    并加 100ms 前台 pgrp 监视线程。
  - GPU：图片 SourceId 按 pane 命名空间；字体切换重建 atlas；Stage 0 的
    dirty cell 行展开为 `cell_h` 像素行；glyph cache 先限额再返回字形。
  - 测试：TabBar/keyboard/pin-drag/single-instance 多窗口 e2e 适配并复跑；
    新增 UTF-8 扫描器、Xresources、shared workspace、最后 Tab 关窗等回归。
- 标题 spinner（Codex 重连、dsh 工作态）期间 Tab 切不动/误关：`tab-box.width`
  依赖 `tab-area.preferred-width`，而后者又受 width 约束，形成循环绑定；标题
  10–20Hz 刷新时宽度会从自然值一级级缩到 52px 下限，close 按钮滑到 Tab 中间，
  点 Tab 就变成关 Tab。现在用独立 `Text` 量标题自然宽度（断开循环）；窄 Tab
  （<84px）inactive 时不再 hover 显示 close，整块都是切换热区，只有 active
  才显示 close；行数不变时 `set_row_data` 原地更新 Tab 模型（避免 Repeater
  反复重建）。新增 e2e：3 Tab + 标题 spinner 下点击 Tab 必须切换
  （`crates/app/ui/main.slint`、`crates/app/src/main.rs`、
  `scripts/e2e_tabbar_plus_profile.sh`）。
- 中间层 TUI 的子进程退出后键盘 flags 未复位：codex 在 dsh 等 TUI 里运行，
  codex 退出/崩溃后前台进程组回到 dsh 而不是 shell；旧逻辑只认
  `pgid == shell_pgid`，残留的 kitty/modifyOtherKeys 会让 dsh 收到
  `9;5:3u`/`7;129u` 并显示成乱码。现在上一个前台 pgrp 已消失（ESRCH）时
  也复位；进入新命令（旧 pgrp 仍活着）保持原行为，避免和新应用首批
  `CSI > flags u` 抢顺序（`crates/core/src/pane.rs`，PTY 回归测试
  `intermediate_tui_child_exit_resets_stale_kitty_flags`）。
- TabBar 恢复旧交互：`+` 直接新建默认 Tab（不再弹 Profile 下拉）；`>_` 只做
  Profile 选择，菜单去掉搜索框，键盘 ↑↓/Enter/Esc 保留；菜单不再抢
  FocusScope，关闭后终端输入立即可用。11-17 的 LOCAL-004 把 `+` 改成下拉、
  20:20 又给菜单加搜索，用户侧表现为“+ 不新建默认 Tab、菜单多出搜索”。
  （`crates/app/ui/main.slint`、`crates/app/src/main.rs`，
  `scripts/e2e_tabbar_plus_profile.sh`）
- 应用崩溃/被 kill 后不会再出现键盘乱码：子命令把 kitty flags 留在 7
  （disambiguate + event types + alternate keys），shell 收到 `CSI-u` 后回显，
  终端吃掉 `ESC[` 只剩 `9;5:3u`/`7;129u` 这类尾巴。现在读线程用
  `tcgetpgrp` 观察前台进程组，回到 shell 时复位 kitty/modifyOtherKeys
  （`crates/core/src/pane.rs`，真实 PTY 回归测试
  `foreground_command_exit_resets_stale_kitty_flags`）。
- 多窗口标题不再把内部 window id 拼进 `WM_NAME`：新建窗口的标题曾是
  `Tervo — 2`，且每个新窗口从“当前活动窗口”克隆 opts，导致继续累积成
  `Tervo — 2 — 3`。现在所有窗口统一为基础标题（默认 `Tervo`），只裁掉纯数字的
  ` — N` 后缀，用户自定义标题不受影响（`crates/app/src/secondary.rs`，
  单测 `window_title_drops_accumulated_id_suffixes`）。

### Added

- Wayland Phase 1：新增 `platform` 能力探测（env hint + winit raw handle 二次确认），
  `--diagnose` 区分“本进程 Wayland 窗口”与 XWayland 数据；新增隔离 headless
  Mutter e2e（`scripts/e2e_wayland_phase1.sh`）。GNOME 46 + fcitx5 下
  text-input-v3 已实测提交中文；Wayland 下 `slint::set_xdg_app_id("tervo")` 让
  `.desktop`/图标/窗口分组可关联（X11 不设置，WM_CLASS 保持原值）。

### Fixed

- Wayland 拖边/拖角 resize 后，合成器会吃掉 pointer-up，Slint `ResizeHandle`
  一直保持 pressed，后续鼠标点击被边缘抓手吞掉（典型表现：Ctrl+T 新建 Tab 后
  鼠标不正常）。现在按 resize 方向把补发的 PointerReleased 落到原抓手命中区，
  并在 Wayland 下用 900ms idle watchdog 兜底收尾；新建 Tab 前也会清理未结束的
  resize/hover/drag 状态。
- 原生 Wayland 不再假报 Quake 全局热键已注册；Wayland 下明确禁用 Quake，
  设置页拒绝启用并提示。
- 原生 Wayland 不再尝试 `_NET_WM_ICON`、X11 SHAPE 圆角、X11 最大化状态机、
  X11 `own_windows`/遮挡检测，也不再为自绘 resize 连接 XWayland 指针。
- 剪贴板不可用时打印一次可读原因（GNOME/Mutter 无 data-control、是否有
  XWayland 回退），短行 `[clipboard] 不可用` 保持兼容。
- Wayland 会话快照只保存尺寸（`geometry.position_known=false`），避免 `(0,0)`
  污染共享 session 后下次 X11 恢复到左上角。
- X11/XWayland 路径保持原行为（Quake grab、`_NET_WM_ICON`、XDND、XIM、EWMH
  resize、PRIMARY）。

## [0.5.0] - 2026-11-14

Stable release: single-instance primary + default in-process multi-window, WindowRegistry/SharedServices, multi-window session v2, full shared UI callbacks, plugin/MCP window_id routing, WindowId-scoped GPU state. Clean 30m dual-window soak PASS.

### Added

- 单进程多窗口：单实例 primary + Unix control socket；第二次 `tervo`/`--window` 转发为
  primary 内新 `MainWindow`，不再启动第二套 Tervo/插件/MCP。`WindowRegistry`/
  `WindowRuntime` 每窗口独立 tabs/panes/focus/models，共享 Config/Theme/PluginHost/
  MCP/Vault/Session。179 个 UI 回调抽成共享注册；插件/MCP 增加 window_id 路由；
  GPU 按 WindowId 引用计数，关闭一个窗口不影响其它窗口。session v2 保存/恢复全部
  窗口的 geometry/maximized/active_tab/tabs，kill -9 可恢复多窗口布局。
- 会话崩溃恢复：`restore_session_on_startup=true` 时每 2s autosave 布局，异常退出
  通过 `session.running` marker 检测并在下次启动恢复；session 写盘 fsync，保留上一代
  `.json.bak` 并在 primary 损坏时回退。仍然只恢复布局/cwd/Profile 关联，不恢复
  scrollback、屏幕内容或正在运行的进程。

## [0.4.0-rc.2] - 2026-11-12

RC candidate: GPU damage scissor, embedded MCP live terminal control, plugin API v3, WASM output-filter hot path, local install/update/rollback/signature, feedback bundle.

### Added

- Plugin API v3 本地安装/更新/原子回滚/签名校验：目录或 zip/tar/tar.gz → staging →
  manifest/host_abi/entry/路径/大小限制校验 → RENAME_EXCHANGE（不支持时两段 rename
  回退）→ `.backup/<id>/<epoch-ms>-<version>`（默认保留 3 个）。可选 Ed25519
  `plugin.toml.sig` + `TERVO_PLUGIN_PUBKEY`/默认公钥/设置开关；坏签名始终拒绝。
  设置页 Install/Update/Rollback + source/signature 展示，CLI
  `tervo plugins install|update|rollback|verify|list|backups|keygen|sign`。
  不执行插件代码、不做市场/远程更新/在线 key 分发。文档 `docs/PLUGIN_INSTALL.md`。
- Plugin API v3 完整宿主（`crates/app/src/plugin_v3/`）：只加载 `api = 3` / `host_abi = 3`；
  v1/v2 清单 unsupported 且不执行，无兼容层。独立子进程 + JSON-RPC 2.0 line-delimited
  stdio；initialize/initialized、command.invoke、event.deliver、status.get、ping、
  shutdown；插件->宿主 ui.notify/ui.status/ui.render/context.version/
  context.config_metadata/terminal.read/write/clipboard/credentials（终端/剪贴板走
  capability gate + 每次 Allow/Deny）。每插件 supervisor：握手/命令/status/ping 超时、
  坏 JSON/崩溃退避重启、重启上限、256 KiB 行/16 MiB 输出/stderr 尾部上限；Linux
  PR_SET_PDEATHSIG + process group。设置页 Enable/Disable/Reload/Restart/Open folder
  不重启 Tervo；首次启用展示权限并确认，权限变化要求重新确认。声明式 panel/settings
  最小渲染（text/markdown/list/input/button/select/progress）与 startup/tab-open/
  tab-close/pane-focus 事件。示例 `docs/examples/plugin-v3-echo/`，文档
  `docs/PLUGIN_V3.md`。MCP 仍为宿主内嵌功能，不实现插件 MCP。
- WASM 热路径：`runtime = "wasm"` 的 terminal.output_filter 在 Sixel/IIP/Kitty/vte 之前执行；
  wasmtime 34、无 WASI/import，memory/alloc/filter ABI，16 MiB 内存、fuel/epoch、256 KiB
  输入/输出/行上限；trap/超时/越界/超限原样放行并熔断，绝不丢 PTY 字节。示例
  `docs/examples/plugin-wasm-filter/`，文档 `docs/PLUGIN_V3.md` 第 6 章。
- 默认 GPU-native：不设环境变量即走 Slint WGPU30 + Scene glyph atlas/图片；`TERVO_CPU=1` 强制 CPU，`TERVO_GPU_NATIVE=0` 仅兼容 stage 0；无 adapter/失败自动 CPU。
- Sixel/Kitty 图片进入 GPU Scene（source registry、clip、below/above-text z-order）；修复 Sixel `2J` 事件顺序导致刚放置图片被清除。
- `TERVO_RENDER_HUD=1` / Settings → Images → Render HUD：FPS、frame time、实际 renderer、GPU-native frames/draw calls、atlas/stage0 上传、网格尺寸。
- `tervo diag doctor [--issue]`：确定性 Doctor 命令入口。
- `tervo diag serve` MCP 扩展：`tervo.doctor_check`、`tervo.issue_draft`、`tervo.metrics_read`、`resources/list|read`，保留只读边界。
- `tervo diag serve` live terminal control：list/output/send_text/send_key/run/focus/new_tab/split/scroll；close_pane/close_tab/kill 需 `TERVO_DIAG_ALLOW_DESTRUCTIVE=1`，写操作需 `TERVO_DIAG_ALLOW_WRITE=1` 与 token。
- Settings → Help → Feedback：生成脱敏本地 tar.gz，打开配置的 issue 新建页；不自动提交 issue/MR、不 post 凭据。
- 工业级 MaximizeController：desired/observed/pending + PropertyNotify、显式 X11 xid、超时只报错不改 target、无 WM 回退。

### Fixed

- Kitty placement ID 保持在正 i32 范围，避免 Slint presenter 点击命中断裂。
- desktop action id 改为 `new-window`，Ubuntu Dock 不再同时添加默认项和自定义项。
- 无 terminal damage 的 redraw 复用上一帧 GPU target；有 damage 时 text/image pass 走 damage scissor，不再整 target 重绘。
- dsh 等全屏 TUI 中滚轮/滚动条滚动时清除原生选区，避免重绘后高亮粘在错误文本上。
- 图片图层顺序：cell 背景 → 图片 → 选区/搜索 → 字形；TUI 黑色 backing cell 不再把图片盖黑。
- 灯箱按 image id 从原始 RGBA 取图，先设 source 再 open；Fit 允许放大且 >100% pixelated。
- 新增 `scripts/e2e_image_display.sh`：自生成 Sixel，断言图片图层/黑底/滚动/灯箱首次点击/Fit/100%。
- dsh 侧栏 ◀/▶ 按文本几何图形单色渲染；U+2922 ⤢ 增加 DejaVuSerif 符号 fallback。

## [0.3.1] - 2026-11-10

### Fixed

- 窗口 resize 收尾不再在 `with_app` 借用期间同步 dispatch PointerReleased，消除 `with_app 重入` 和松开鼠标后状态卡住。

## [0.3.0] - 2026-11-10

### Added

- 内置 `tervo diag env/snapshot/replay/trace --json`、MCP stdio、显式 opt-in live control socket（0600/token，只读，`scripts/e2e_diag_live.sh`）；`tervo.diag.v1` schema；不含 API key/图片 RGBA。
- Settings Help 页面：快捷键、配置/日志路径、诊断复制、版本信息、已知限制。
- Background image Browse 文件选择器；图片/Cwd/Shell/SSH key 支持 `~`。
- 无背景图时终端 opacity 也可用（透明窗口；需要合成器）。
- Sixel/IIP 超大图缩到 pane 内；点击打开灯箱，支持缩放/Fit/Close/Esc。
- dsh-tui 等 TUI 用 ECH/空格擦图时同步清理旧 overlay，避免图片“到处飘”。
- 回应 XTWINOPS `CSI 16t/14t` 并设置 PTY 像素 winsize，修复 dsh 图片按默认 8×16 低清。
- Unicode 块元素（U+2580–259F）按 cell 几何绘制，不再依赖字体半块字形。
- 插件 API v2：v1 清单继续兼容，新 `[contributes.ai]` 声明式 AI 能力，`ai.chat` 走同一 process supervisor；v2 样例 `docs/examples/ai-echo`。
- Settings → Plugins 的 AI Chat 面板：插件选择、prompt、Send/Cancel/Close、错误/空状态；宿主不保存/转发 API Key，不内置网络客户端。
- AI Chat e2e：`scripts/e2e_ai_panel.sh`，离线 echo 插件验证命令路由与日志脱敏。

### Changed

- 内联图片从独立 Slint overlay 改为 framebuffer 内合成：文本覆盖图片、滚动随 cell、灯箱仍用原图。
- `PLUGIN_API_VERSION` 提升到 2，最低兼容 1；新增 `PLUGIN_AI_API_VERSION`。

### Fixed

- 多显示器：Quake 使用当前 monitor 几何 + EWMH workarea，不再按整个 root 居中。
- 鼠标上报 TUI（dsh-tui/vim/tmux）中，`right_click = "menu"` 不再被无条件转发给 TUI。
- GNOME dock/任务栏右键增加 New Window action（`StartupWMClass=tervo`）。
- 松开左键后仍能 resize 窗口（尤其上边框）。
- 内联图片改为 framebuffer 合成（文本覆盖图片、滚动随 cell），移除独立视觉 overlay。
- XTWINOPS/PTY 像素尺寸、ECH 按光标行清图、块元素几何绘制。

## [0.2.0] - 2026-11-09

### Added

- 图片合成 v2：framebuffer 内合成、ECH 行级清理、XTWIN/PTY 像素尺寸、块元素几何绘制。
- 插件 API v2 + process plugin AI Chat（宿主不存 API Key）、`docs/examples/ai-echo`。
- Settings AI Chat 面板、Help 页面、i18n、Tab pin/拖拽、背景图/灯箱。
- 打包 tar/deb/校验和，30m soak PASS。

### Fixed

- 图片低清/漂移、Settings 齿轮/右键菜单、窗口 resize 松手、内存收敛。

## [0.1.0] - 2026-11-09

### Added

- Rust + Slint 终端：多 Tab、分屏、GPU/软件渲染、自定义窗口缩放。
- Kitty keyboard / modifyOtherKeys、鼠标上报、bracketed paste、OSC52、焦点事件。
- Sixel / iTerm2 IIP 图片、彩色 emoji、字体回退。
- Profile、SSH/SFTP/Vault、会话恢复、外部进程插件 V1。
- Settings Tab、中英文语言选项、主题、右键菜单、搜索、URL/path hover。
- Tab Pin、拖拽 ghost/插入线/让位动画。
- 打包：tar.gz / deb / install / rollback。

### Fixed

- IIP/Sixel 在 scrollback 封顶后永久保留导致的长时内存增长（30m +64MiB，平台）。
- Settings 齿轮退化成太阳、右键菜单装饰图标、Dim/Opacity 滑块悬停跟随。
- 首次启动不读 `default_profile_id`。
