# 日志与诊断

Tervo 默认把 stderr（包括 panic）同时写入按次启动的日志文件，排查“右键没反应 / 焦点不对 / 窗口挡住”时先看日志。

## 路径与环境变量

| 项 | 默认值 |
|---|---|
| 日志目录 | `~/.local/state/tervo/logs/`（`XDG_STATE_HOME` 存在时跟随） |
| 文件 | `tervo-<epoch 秒>.log`，保留最近 8 个 |
| `TERVO_LOG_DIR` | 覆盖日志目录（测试/多实例） |
| `TERVO_LOG=0` | 关闭文件日志（panic 仍打印到 stderr） |

## 一键诊断

```bash
tervo --diagnose
```

会打印：版本/pid、DISPLAY/Wayland/Slint backend、config/profiles 路径、日志路径、屏幕尺寸、指针位置、**当前活动窗口（pid/几何/title）**、本进程窗口、**其他 Tervo 实例窗口**，以及最近 40 行日志。

## 右键问题怎么读日志

1. 在 Tervo 窗口里右键，然后看日志里有没有：

   ```
   [trace] pane-down-right pane=1 at 200,168 win_pos=... win_size=...
   [ui] right click pane=1 at 200,168
   [trace] pane-up-right ...
   ```

   - **有 `[trace] pane-down-right`**：X11 事件到达了 Tervo；若菜单没出现，是 UI 路由/浮层状态问题（日志里会继续有 `settings open=` / `menu dismissed`）。
   - **没有 trace**：事件没到 Tervo。用 `--diagnose` 看 `active_window` 与“其他 Tervo 实例窗口”——常见原因是旧的全屏 Tervo 窗口盖住了新窗口，或点击落在别的应用（例如宿主终端/TUI）里。
2. 如果 pane 里跑着 tmux/vim/Claude Code 等**开启鼠标上报**的 TUI，旧版本会把右键转发给 TUI
   （日志只有 `[trace] pane-down-right` 而没有 `[ui] right click pane=`）。当前版本已改为
   **右键始终先开 Tervo 菜单**，Shift+右键才转发给 TUI。
3. 启动日志会主动提示：

   ```
   [ui] 警告：另一个可见的 Tervo 窗口 0x…（pid=…）2560x1413 at 0,27，可能挡住点击
   ```

   关闭那个旧实例即可。

## panic

panic 会带 `Backtrace::force_capture()` 写入同一日志（也打印到 stderr），格式：

```
[panic] thread 'main' panicked at ...:
backtrace:
   0: ...
```

## 报 bug 时提供什么

- `tervo --diagnose` 的完整输出；
- 对应时间的 `tervo-*.log`（或其中 `[trace]`/`[ui]`/`[panic]` 相关段落）；
- 是否在 tmux/vim 鼠标模式、是否使用 Wayland、是否多开 Tervo。
