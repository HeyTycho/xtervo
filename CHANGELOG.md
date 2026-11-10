# Changelog

Tervo 的版本与 tag 规则见 `docs/RELEASING.md`。

## [0.2.0] - 2026-11-09

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

- 松开左键后仍能 resize 窗口（尤其上边框）。
- 内联图片改为 framebuffer 合成（文本覆盖图片、滚动随 cell），移除独立视觉 overlay。
- XTWINOPS/PTY 像素尺寸、ECH 按光标行清图、块元素几何绘制。

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
