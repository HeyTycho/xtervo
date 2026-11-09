# Changelog

Tervo 的版本与 tag 规则见 `docs/RELEASING.md`。

## [Unreleased]

### Added

- Settings Help 页面：快捷键、配置/日志路径、诊断复制、版本信息、已知限制。
- Background image Browse 文件选择器；图片/Cwd/Shell/SSH key 支持 `~`。
- 无背景图时终端 opacity 也可用（透明窗口；需要合成器）。
- Sixel/IIP 超大图缩到 pane 内；点击打开灯箱，支持缩放/Fit/Close/Esc。
- dsh-tui 等 TUI 用 ECH/空格擦图时同步清理旧 overlay，避免图片“到处飘”。

### Fixed

- 松开左键后仍能 resize 窗口（尤其上边框）。

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
