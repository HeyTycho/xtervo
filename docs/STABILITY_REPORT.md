# 稳定性验收报告（feat/stability）

- 日期：2026-11-06（截止 18:00）
- worktree：`.worktrees/stability`
- 分支：`feat/stability`，基线 `a42a108`（master，未合并、未改动 master）
- 本报告随提交 `test(stability): 基础功能回归验收` 一起提交。

## 环境与隔离

| 项 | 值 |
|---|---|
| rustc / cargo | 1.99.0 / 1.99.0 |
| 离线依赖 | `CARGO_HOME=$PWD/.cargo`（worktree 内指向主仓缓存，未写 master） |
| 显示 | 每个脚本独立 Xvfb；`osc52=:113`、`term_menu=:119`、`behavior=:118`、`ui=:115`、`wm_resize=:105`；手动项 `:121/:122/:123/:124/:125` |
| Xvfb | `/home/user/AIworkspace/xtervo/.perf/xvfb/usr/bin/Xvfb` |
| WM / 资源 | 主仓 `.perf/openbox`、`.perf/xvfb`（e2e 脚本按 git common dir 定位） |
| GPU | 本机 `/dev/dri` 不存在；headless 下仅 Mesa llvmpipe |
| 约束 | 未使用 `:1`、未用 Xephyr、未 `pkill`；脚本/手动只 kill 自己记录的 PID |

## 1. 单元/集成测试

命令：

```bash
CARGO_HOME=$PWD/.cargo cargo test --workspace --offline
```

结果：**全部通过**，`EXIT=0`，共 412 个测试（另 hello/doc 0 个）：

| target | 通过 |
|---|---|
| `xtervo`（app 主二进制） | **249** |
| `xclick` / `xdrop` / `xkey` / `xwin` | 各 10 |
| `xtervo_core` lib | 70 |
| `tests/mouse_report.rs` | 16 |
| `tests/osc52.rs` | 2 |
| `tests/smoke.rs` | 23 |
| `tests/vt_conformance.rs` | 12 |
| `hello` / doc-tests | 0 |

日志：`.logs/cargo-test-final.log`。初次与最终各跑一次，均 0 失败。

## 2. GUI e2e（各自独立 Xvfb）

统一前置（debug 二进制）：

```bash
CARGO_HOME=$PWD/.cargo cargo build --offline -p xtervo-app \
  --bin xtervo --bin xwin --bin xclick --bin xkey --bin xdrop
export E2E_BIN=$PWD/target/debug/xtervo
export E2E_XVFB_BIN=/home/user/AIworkspace/xtervo/.perf/xvfb/usr/bin/Xvfb
```

| 脚本 | display | 命令要点 | 结果 |
|---|---|---|---|
| `scripts/e2e_osc52.sh` | `:113` | `E2E_OUT=.logs/osc52` | **PASS**（ALL PASS） |
| `scripts/e2e_term_menu.sh` | `:119` | `XTERVO_PERF_DIR=<主仓>/.perf` | **PASS**（exit 0） |
| `scripts/e2e_terminal_behavior.sh` | `:118` | `XTERVO_PERF_DIR=<主仓>/.perf` | **PASS**（exit 0；修 xclick 后复跑仍 PASS） |
| `scripts/e2e_terminal_ui.sh` | `:115` | `XTERVO_PERF_DIR=<主仓>/.perf` | **PASS**（exit 0） |
| `scripts/e2e_wm_resize.sh` | `:105` | `E2E_XVFB_BIN`、`E2E_OPENBOX_BIN/LIB/DATA`、`E2E_BIN/E2E_XCLICK` 指向 debug | **PASS**（exit 0） |

`e2e_wm_resize.sh` 实测：初始 540x472 → 右边缘 700x472 → 右下角 800x572；
日志含 `WM live resize dir=3/dir=4 source=winit`，无自绘回退。日志：`.logs/wm-resize.out`。

**首次运行失败项：无。** 过程中出现的失败均为验收命令写错（worktree 无 `.perf`、
Xvfb 路径重复、未显式 `env -u SLINT_BACKEND`），修正命令后通过，不计为产品失败。

## 3. 重点项人工验收

### 3.1 README / 默认配置

- 发现 master 继承的 **README 未解决合并冲突标记**（`<<<<<<< HEAD` /
  `|||||||` / `=======` / `>>>>>>> feat/term-behavior`），位于「设置」表格行；
  同表还有重复的「复制」行。已修复为合并后的一行设置说明并去重。
- 默认值与 `Config::default`、README 描述一致（并已有单测覆盖：
  `appearance_defaults_match_gnome`、`interactive_defaults` 等）：
  `theme=auto`、`osc52=allow`、`audible_bell=false`、`visual_bell=true`、
  `scroll_on_output=false`、`scroll_on_keystroke=true`、`scrollback_unlimited=false`、
  `scrollbar_policy=auto`、`backspace_binding=ascii-delete`、
  `delete_binding=delete-sequence`、`cursor_shape=block`、`cursor_blink=system`、
  `background_dim=0.60`。

### 3.2 启动

- 全新 `XTERVO_CONFIG`/`XTERVO_LOG_DIR` + `SLINT_BACKEND=winit-software` 启动：
  窗口 720x560 出现，`[ws] new tab: tabs=1`，`READY TIMING`/`FIRST_DAMAGE_US` 正常，
  **无 panic**。
- `--exit-after-first`：**exit=0**。
- 日志位置与 `docs/LOGGING.md` 描述一致。

### 3.3 GPU / software fallback

- `/dev/dri` 不存在，`SLINT_BACKEND` 未设置：自动选 `winit-software`，日志明确写
  「默认 GPU/Skia，但未检测到 /dev/dri……自动选软件」，随后
  `actual_renderer=winit-software`。
- `SLINT_BACKEND=winit-skia` 在 Xvfb 下走 Mesa **llvmpipe**（`skia-opengl`），
  日志正确提示「CPU 光栅，不是 NVIDIA/真 GPU 的性能证据」；未把它当 GPU 结论。
- 用 debug 钩子 `XTERVO_TEST_FORCE_UI_INIT_FAIL=1` 注入首次 UI 初始化失败：
  自动回退 `winit-software` 并成功开窗，日志包含
  「初始化失败……自动回退 SLINT_BACKEND=winit-software」。
- 结论：无 DRM 自动软件路径 + 初始化失败回退路径均通过；**真实 GPU 性能不可验**。

### 3.4 背景 / dim

- 256x256 纯灰 `#808080` 背景图，`background_opacity=0.35`；
  同一像素 `(350,400)`：`background_dim=0.80` → `(25,25,29)`，
  `background_dim=0.20` → `(75,75,79)`。
- dim 值越大画面越暗，背景图叠加、暗化遮罩均生效；无崩溃/无黑屏。
- 验证日志/截图：`.logs/manual/runtime/bg-0.80/`、`.logs/manual/runtime/bg-0.20/`。

### 3.5 Tab 20

- 空 pane 中依次 `xwin xtervo`（设置 X 焦点）+ 19 次 `xkey t --ctrl`：
  日志出现 `[ws] new tab: tabs=20`，app 存活且无 panic；
  `pgrep -P` 显示 20 个 shell 子进程；截图 `.logs/manual/tabs20/tabs20.png`。
- 注：裸 Xvfb 无 WM 时若不做 `xwin` 聚焦，合成按键可能不进入应用；
  这是测试环境前置条件，不是应用 bug（e2e 脚本已有同等处理）。

### 3.6 右键 / 只读 / Save Contents

由 `scripts/e2e_term_menu.sh` 覆盖并通过：

- Find：Match case / regex / 非法 regex 提示；
- Read Only：右键菜单切换、`READ ONLY` 角标、键盘/Paste 不写 PTY，关闭恢复；
- Save Contents：菜单入口、显式默认路径、实际写入 63 bytes、错误弹窗；
- Reset / Reset and Clear；关闭搜索/弹窗后终端输入仍正常。

### 3.7 光标闪烁与空闲重绘

- 聚焦状态逐帧采样：光标区域 `(45..53, 32..53)` 在 9x22 的 198 个像素上
  在亮/暗之间切换，说明闪烁实际可见。
- 空闲约 3.2s（含 `530ms` 闪烁定时器）：`[render] pane=` 计数保持 3 → 3，
  **没有新增整 pane 栅格化**；进程 CPU 13 ticks（约 4% 单核，软件后端合成开销）。
- 结论：空闲闪烁只切 overlay，不重绘终端帧；`cursor_blink=off` 的路径也有单测覆盖。
- 注：裸 Xvfb 无 WM 且未聚焦时，光标会隐藏/静止；聚焦后闪烁正常。

### 3.8 Sixel / emoji

按指示**跳过**：对应分支尚未合并到本基线，不纳入本次验收，也不声称兼容。

## 4. 本次修复（仅低风险）

1. `README.md`：删除 master 合并遗留的冲突标记与重复「复制」行，恢复完整设置行。
2. `crates/app/src/bin/xclick.rs`：调试行只在显式注入 `ctrl`/`shift` 时打印，
   普通点击/拖拽保持安静；避免 e2e 输出被 `xclick: shift=false ...` 淹没。
   功能不变，修后 `cargo test` 与 `term_menu`/`behavior` e2e 复跑通过。

未改架构、默认值、UI 布局、键位策略。

## 5. 已知限制 / 观察项

- **无真 GPU**：`/dev/dri` 缺失，所有渲染结论仅限 llvmpipe/software，
  不能证明 NVIDIA/真 GPU 的帧率、延迟或 fallback 表现。
- **Sixel/emoji 未合并**：未测试、未等待。
- `e2e_wm_resize.sh` 用的是 debug 二进制（worktree 未构建 release），
  只验证 live resize 行为，不作为性能数据。
- worktree 内没有 `.perf`；运行 e2e 需显式传入主仓 `.perf` 的
  `E2E_XVFB_BIN`/`E2E_OPENBOX_*`（脚本本身的默认路径只适用于主仓）。
- 观察项（未修，低风险）：`xtervo --diagnose` 的 `requested_backend` 提示只按
  `/dev/dri` 判断，未考虑 `XTERVO_FORCE_GPU=1`；正常启动逻辑是正确的，
  但 diagnose 输出在强制 GPU 场景下可能与实际请求不一致。
- 本次未复测 Wayland、IME、真实桌面 WM 与长时间压力；这些不在本轮验收范围。

## 6. 失败清单

- 阻塞/高风险失败：**无**。
- 非阻塞、未修观察项：`--diagnose` 后端提示与 `XTERVO_FORCE_GPU` 的边界不一致（见上）。
- 测试脚本首轮人为调用错误已修正，不计入失败。
