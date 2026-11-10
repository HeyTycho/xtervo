# 多显示器 / dock 支持

状态：基础支持（2026-11-10）。

## 已支持

- XRandR monitor 列表：`xconn::monitors()`，不把整个 root（例如 5120×1440）
  当成一块屏。
- Quake Mode（F12）在**当前窗口所在 monitor**内居中，不再落在双屏接缝。
- 使用 EWMH `_NET_WORKAREA` 避开顶部/底部 panel（GNOME top bar 等）。
- `--diagnose` 输出：
  ```text
  screen=5120x1440
  monitor=2560x1440+0+0 primary=true
  monitor=2560x1440+2560+0 primary=false
  workarea=5120x1413+0+27
  ```
- `.desktop`：
  ```ini
  StartupWMClass=tervo
  Actions=NewWindow;

  [Desktop Action NewWindow]
  Name=New Window
  Exec=tervo --window
  ```
  GNOME dock/任务栏右键 Tervo 图标应出现 “New Window / 新建窗口”。

## 限制

- `_NET_WORKAREA` 是全局矩形，不是每块显示器独立；如果两块屏的 dock 高度
  不同，Tervo 只能按全局 workarea 近似避开。
- 没有 monitor 热插拔监听；拔掉显示器后已保存的窗口位置可能需要手动移回。
- 自绘窗口 resize 仍按 root 坐标拖拽，可能覆盖 dock；WM 原生 resize 由 WM 约束。
- 任务栏 action 是否显示取决于桌面环境；GNOME 需要重新安装/刷新 desktop entry。
