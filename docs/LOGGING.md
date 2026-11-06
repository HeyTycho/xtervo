# 日志与诊断

Tervo 默认把 stderr（包括 panic）同时写入按次启动的日志文件，排查“右键没反应 / 焦点不对 / 窗口挡住”时先看日志。

## 路径与环境变量

| 项 | 默认值 |
|---|---|
| 日志目录 | `~/.local/state/tervo/logs/`（`XDG_STATE_HOME` 存在时跟随） |
| 文件 | `tervo-<epoch 秒>.log`，保留最近 8 个 |
| `TERVO_LOG_DIR` | 覆盖日志目录（测试/多实例） |
| `TERVO_LOG=0` | 关闭文件日志（panic 仍打印到 stderr） |
| `SLINT_BACKEND` | 显式指定后端：`winit-skia` / `winit-software`；不设时默认 GPU（无 `/dev/dri` 自动软件）。`winit-skia-wgpu` 可对比 Vulkan/wgpu 路径，`winit-skia-opengl` 强制 GL |
| `TERVO_RENDER_PROFILE=1` | 打印每帧「pane 绘制 / Image 交接 / Skia 合成」耗时 |
| `TERVO_RESIZE_PROFILE=1` | 打印 resize apply 次数/耗时、背景隐藏/恢复、resize 汇总 |
| `TERVO_RESIZE_KEEP_BG=1` | 对比实验：resize 期间不隐藏大背景图 |
| `TERVO_FORCE_GPU=1` | 测试用：无 `/dev/dri` 时也走默认 GPU/Skia 分支 |
| `TERVO_FALLBACK_FONTS` | `:` 分隔的额外回退字体（可带 `#face_index`），如用户目录的 Nerd Font；详见 [FONT_FALLBACK.md](FONT_FALLBACK.md) |
| `TERVO_FONTLOG=1` | 打印每个回退字体的解析耗时/失败，排查缺字与首次出现中文/emoji 的卡顿 |

## 字体/emoji 怎么读日志

- 出现中文/emoji 的那一刻才会解析回退字体；`TERVO_FONTLOG=1` 会打印
  `[font] 回退加载 <path> (<耗时>)` 或 `[font] 回退不可用 ...`。
- 彩色 emoji（Noto Color Emoji）fontdue 取不到轮廓，画面显示**空框占位**而不是
  tofu；这是已知限制，详见 [FONT_FALLBACK.md](FONT_FALLBACK.md)。
- 纯 ASCII 会话不会加载任何回退字体；如果 `[font] 回退链` 在启动时就出现，
  说明启动配置里有非 ASCII 字符或字体路径探测失败，需要附日志排查。

## 一键诊断

```bash
tervo --diagnose
```

会打印：版本/pid、DISPLAY/Wayland/Slint backend、config/profiles 路径、日志路径、屏幕尺寸、指针位置、**当前活动窗口（pid/几何/title）**、本进程窗口、**其他 Tervo 实例窗口**，以及最近 40 行日志。

## 渲染后端怎么读日志

启动时会打印**请求的 backend** 和**实际 renderer**：

```
[render] requested_backend=winit-skia（默认 GPU/Skia；可用 SLINT_BACKEND 覆盖）
[render] requested_backend=winit-skia；notifier=已安装（Skia），等待 GL 上下文自检
[render] actual_renderer=skia-opengl
[render] GL_VENDOR="NVIDIA Corporation"
[render] GL_RENDERER="NVIDIA GeForce ..."
```

- `actual_renderer=skia-opengl` + 真实 `GL_RENDERER`：GPU/GL 路径（如果 `GL_RENDERER`
  含 `llvmpipe/softpipe`，那是 CPU 光栅，不能当 GPU 性能）。
- `actual_renderer=skia-software-surface`：Skia 没拿到 GL 且没有渲染帧，可能黑屏；
  启动 1s 后仍无 GPU API/帧会自动 `exec` 回退 `winit-software`，日志出现
  `exec 自动回退 winit-software`，随后是新进程的 `actual_renderer=winit-software`。
- `actual_renderer=winit-software`：软件后端（用户显式指定、无 `/dev/dri`，或 GPU
  初始化失败）。
- 没有 `/dev/dri` 的 headless/CI 环境**不能验证 GPU**；测试脚本应显式设
  `SLINT_BACKEND=winit-skia`，并且不要把 llvmpipe 数字写成 NVIDIA 结论。

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
