# 稳定交互基线（窗口 / TabBar）

日期：2026-11-06（polish 合并后）。以下行为已经验证过，后续改动**不要直接在 master 上实验**：

## 冻结项

- 窗口边/角拖拽：X11 主路径经 `WinitWindowAccessor` → winit `drag_resize_window`（同连接 ungrab + EWMH）；无 WM 时回退自绘，最小 320×200。
- 最大化/最小化：使用 Slint `Window::set_maximized` / `set_minimized`，不加 EWMH override。
- TabBar `+` / Profile 按钮：位于 Tab 条内部、跟随 Tab 移动；不做右侧固定。
- 窗口控制按钮（设置/最小化/最大化/关闭）：绝对定位于窗口右上角，位置不变。
- 圆角：X11 SHAPE 默认关闭（`TERVO_CORNERS=1` 才实验开启），避免 mask 影响 WM 行为。
- 背景图：最底层 cover + dim 遮罩；opacity 直通 RGBA（不双重预乘）；TabBar/设置不透明。
- Tab：下限 52px、Flickable 横向滚动、`+`/Profile 跟随 Tab；Tab 搜索浮层关闭后归还终端焦点。

## 改动规则

1. 窗口/resize/TabBar 布局改动必须在独立分支，不直接改 master。
2. 改动前后各留一张 Xvfb 截图（`.logs/`），并跑：
   ```bash
   cargo test -p tervo-app --bin tervo
   ```
3. 不允许把 Xephyr/测试窗口弹到用户 workspace；GUI 测试用 headless Xvfb。

## 已知 tag

- `stable-window-baseline-2026-11-06`：窗口/TabBar 基线。
- `stable-polish-2026-11-06`：resize/背景/Tab 打磨合并后的 release 基线。
