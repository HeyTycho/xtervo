# Changelog

Tervo 的版本与 tag 规则见 `docs/RELEASING.md`。

## [Unreleased]

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
