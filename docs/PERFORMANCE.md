# 性能与打包基线（Tervo）

本文记录**可复现**的性能/体积数据与门禁。所有数字都在本仓库的开发机上测得
（Ubuntu + X11；2026-11-06 起默认 Skia/GL，无 GPU/GL 失败回退 winit-software）。
换机器会变，但门禁与采集方式不变。

## 1. 一键门禁

```bash
export CARGO_HOME=$PWD/.cargo
./scripts/selfcheck.sh
```

`scripts/selfcheck.sh` 依次执行：

| 项目 | 门禁 | 数据来源 |
|---|---|---|
| release 构建 | 必须成功 | `cargo build --release` |
| 单元/集成测试 | 数量不低于基线 | app / core / conformance 三套 |
| 启动 ready | ≤ 450 ms | 应用日志 `ready=…`（`--startup-budget`） |
| 首帧 first_damage | ≤ 600 ms | 应用日志 `first_damage=…` |
| 空闲 RSS（单 pane） | ≤ 70 MB | `/proc/<pid>/VmRSS` |
| 启动预算 | 解析失败即 FAIL | 日志里没有 `ready=` 直接判失败 |

历史每次运行都写到 `.logs/selfcheck/history.jsonl`，最近的摘要见
`.logs/selfcheck/<时间戳>/summary.json`。

## 2. 最近基线（2026-11-05）

| 指标 | 最近一次 | 说明 |
|---|---|---|
| ready | **52.6 ms** | 窗口可见 + 首帧提交（近期 43–89，受系统负载影响） |
| first_damage | **85.7 ms** | 首帧内容上屏（近期 60–140；2026-11-05 修正测量口径，见下） |
| 空闲 RSS（1 pane） | **52.5 MB** | 含软件渲染缓冲与字体 atlas（近期 46–53） |
| 空闲 RSS（4 pane） | **44 MB** | 拆分 4 个 pane 后测（shell 子进程另计） |
| 空闲 CPU（4 pane） | **0.2–0.4 %** | 5 秒 `/proc/<pid>/stat` 增量 |
| 开发二进制 | 183–192 MB | 带 `debug=1`，供 perf/flamegraph |
| 发布产物（strip） | **32.3 MB** | `strip --strip-all`，体积缩减 82.5% |

48 次 selfcheck 历史趋势：前 10 次 ready 平均 **202.5 ms**，最近 10 次平均
**54.1 ms**——主要是把 CJK 字体回退从启动时急切解析改成首次使用时惰性加载
（一次性代价约 159 ms，只影响首次显示中文）。

## 2.1 first_damage 口径（2026-11-05 修正）

原先在 `ui.show()` + 首帧之后才建立 XDamage 监听：若首帧早于监听建立，测到的会是
**下一次** damage（光标/后续输出），数值忽高忽低；更名后这个竞态稳定地多等了
100 ms 轮询。修正为：

1. `DamageWatcher::start` 找窗口的轮询从 100 ms 缩短到 5 ms；
2. 监听建立后**强制重绘一帧**，确保测的是“窗口就绪后渲染第一帧”的耗时。

因此 47 ms → 60–140 ms 不是性能退化，而是口径修正；门槛（600 ms）保持不变。

## 2.2 2560x1413 持久帧 / 背景 / resize（2026-11-06，headless llvmpipe）

测试环境：Xvfb :104 2560x1440x24 + Openbox + `LIBGL_ALWAYS_SOFTWARE=1`；
`render_probe` 实证 `actual_renderer=skia-opengl`、`GL_RENDERER=llvmpipe`。
**llvmpipe 是 CPU 光栅，不能当成 NVIDIA/真 GPU 数据。**

| 指标 | 旧实现 | 新实现 | 说明 |
|---|---|---|---|
| `Renderer::image()` 2560x1413 | 4.8 ms | **0.00 ms** | 双缓冲持久 `SharedPixelBuffer`；`Image` 仅 refcount clone，`detach=false` |
| 整屏 draw 2560x1413 | 0.36–1.67 ms（脏行） | 0.55–1.11 ms（整屏） | 切缓冲必须整屏重建；比整屏拷贝便宜 |
| resize apply（含 grid resize） | 11.0 ms | 10.5–12.7 ms | 新实现多分配一块 14MB buffer，基本持平（随运行有波动） |
| 背景图 cover（2560x1440 持续帧） | 3.53 ms/帧 | 1.49 ms/帧 | 大窗口拖动期间临时隐藏背景，停下恢复 |

XTest 80 步/4ms resize（1080x736→1380x736，Openbox WM live resize）：

| backend | applies | 中位 apply | 最大 apply | watchdog 回退 |
|---|---|---|---|---|
| winit-software | 11 | 3.10 ms | 4.10 ms | 0 |
| winit-skia / llvmpipe | 14 | 1.80 ms | 3.00 ms | 0 |

Skia 同轮记录到 35 帧渲染，中位 2.85ms、最大 6.84ms（llvmpipe，包含每帧纹理上传）。
大窗口（2538x1396→2238x1396）Openbox 只产生 1–2 次 configure：software 1 次
9.60ms，Skia 2 次中位 10.85ms/最大 12.70ms。

注意：Openbox 会把 2538x1396 的拖动合并成 1–2 次 configure（9.6/10.9ms），不能把
“apply 次数少”当成更快；1080 窗口的 80 步数据更能反映逐 configure 路径。
完整日志/截图：`.logs/bench-resize/`、`.logs/gpu-func/`（gitignored）。

## 2.3 事件唤醒延迟 / 空闲开销（2026-11-06，P1-07，headless Xvfb :138）

测试：`scripts/e2e_event_wake.sh`，同一 debug binary 跑两遍（wake 默认 / `TERVO_EVENT_WAKE=0`
纯 400ms 轮询），shell 在发事件前用 `date +%s%3N` 打点，应用日志 `ts_ms=` 对齐。

| 事件 | 事件唤醒 | 400ms 轮询（对照） |
|---|---|---|
| OSC 标题 ×6 | 0–1 ms | 88–390 ms |
| BEL | 0–1 ms | 287–292 ms |
| OSC52 query | 1–2 ms | 385–388 ms |
| 插件活动刷新 | 8–9 ms | 397–400 ms |
| SFTP 上传完成 | 27–33 ms | 82–102 ms |

空闲（8 秒采样，`TERVO_WAKE_STATS=1`）：两遍 `core_event/app_event` 增量均为 0，
`periodic_400ms` 每 5 秒 12–13 次；CPU ticks before 111–214 / after 103–110（100 Hz，
两次运行系统负载有波动），voluntary context switches before 225–252 / after 263——
事件唤醒没有引入高频轮询。本表是相对 A/B 数据（Xvfb + `winit-software`），不能直接
当真实 GPU/桌面空闲 CPU。日志/截图：`.logs/event-wake/`。

## 2.4 渲染 damage 恢复（2026-11-06，P1-06）

**问题**：双缓冲改造后 `FrameBuffers::rotate()` 恒返回 true，`draw_impl` 把
`buffer_switched` 并进 `full`，`frame.damage` 分支实际不可达；每次按键/单行输出
都整屏 raster/blend（200x50 实测约 1.58 ms，profile `rows=50`、每帧
`full_redraws+1`）。

**修复**：双缓冲只在轮换时把**上一帧重绘过的行**从旧 buffer 复制到新 buffer，
再 raster 本帧 damage 行；两边始终只差一帧。首帧/resize/主题/透明度、以及
`frame.full` 或 damage 行数 > `rows/2`（普通屏幕滚动会脏所有行）仍走整屏。
单色字形补单元格纵向裁剪：部分字体的重音/上半块会向邻行渗 1px，局部重绘不覆盖
邻行时会在那里留下混合像素（像素等价测试抓到的真实残影）。

**可复现基准**（同进程 legacy-full vs damage 对照，release）：

```bash
CARGO_TARGET_DIR=$PWD/.target-render ./scripts/bench_render.sh
# 场景：single_row / typing_burst / full_200x50 / scroll，每项 200–600 帧
```

200x50（cell 9x22，buffer 1800x1100，`draw_without_cursor` + `image`）：

| 场景 | legacy full p50 | damage p50 | speedup | rows | full_redraws delta |
|---|---|---|---|---|---|
| 单行输出 | 1581.4 µs | **34.7 µs** | **45.6×** | 1 | 0 |
| 连续按键（400 帧） | 1584.0 µs | **34.1 µs** | **46.4×** | 1 | 0 |
| full 200x50 | 1612.8 µs | 同路径 | 1.00× | 50 | 200/200 |
| 滚动（全行 damage） | 1605.9 µs | 1607.4 µs | 1.00× | 50 | 200/200 |

`image()` p50 0.05–0.08 µs（refcount clone），benchmark 内 `detach=0`。
跨提交 before（基线 `1a55f09` 首次干净运行）单行/按键/滚动都约 1.56–1.61 ms、
`rows=50`、每帧 `full_redraws+1`（1a55f09 构建后多次运行因机器负载可到
2.3–5.4 ms）；因此**同进程 A/B 比值**才是主要证据，绝对值只作参考。

**Xvfb :137 e2e**（`scripts/e2e_render_damage.sh`，release 二进制）：

```
[render] pane=1 grid=100x30 full=false rows=1 copy=0 full_redraws=1 damage_rows=1 ...
[render] pane=1 grid=100x30 full=true  rows=30 copy=0 full_redraws=2 ...   # 滚动
[render] pane=1 grid=105x30 full=true  rows=30 ...                         # resize
[render] pane=1 grid=105x30 full=false rows=1 copy=29 ...                  # resize 后单行
```

断言：单行输出 `full=false rows=1` 且 `full_redraws` 不增长；搜索高亮只重绘命中行；
滚动/resize/主题切换出现整屏帧；截图 5 张（单行输出、搜索高亮、滚动、resize、
主题切换）在 `.logs/render-damage/`，主题背景像素从 tokyo-night 切到 catppuccin-mocha
的计数为 235057→0 / 0→235057。

**已知限制**：

- 仍是两块 CPU 持久帧。若 Slint 连续两帧以上未消费旧 `Image`，写入目标 buffer 会触发
  `SharedPixelBuffer` 整块 CoW（release 100x30 约 0.6 ms；2560x1413 会更大）。此时只保证
  正确性，不保证 damage 成本；正常单帧节奏下 `detach=false`。
- Skia 仍整张纹理上传（Slint 1.18 没有安全持久 GPU texture API）；本次只省 CPU
  raster/blend/行复制，不改变纹理上传。
- Sixel 是独立 overlay，不写入终端帧，因此不参与 damage；滚动历史（`display_offset≠0`）
  仍由 core 强制 full。
- 只在 Xvfb/llvmpipe + `winit-software` 验证；真机 GPU 的 detach 频率/帧时间未验收。
## 2.5 v0.5.0 压力 soak 基线（2026-11-16，headless Xvfb :143 + Openbox）

命令（沙箱里 `/run/user/1000` 只读时需给可写目录）：

```bash
DURATION=2m BIN=$PWD/target/release/tervo SOAK_NO_SHOTS=1 \
  XDG_RUNTIME_DIR=/tmp/soak-rt TERVO_INSTANCE_DIR=/tmp/tervo-inst ./scripts/soak.sh
```

结果（commit `ff607db`，22 tabs + 3 splits + 设置/搜索/复制/resize 场景）：

```text
verdict   PASS
duration  120s（warmup 40s，实际 134s）
RSS       354.7 → 392.4 MiB（+37.7 MiB，49 采样区间 R²=0.99）
PSS       344.0 → 382.1 MiB（峰值 399.2）
CPU       avg 7.6% / max 9.6%
threads   112 → 112      fds 97 → 97
coverage  tabs=22 splits=3 settings=5/40 find=9 copy=16 resize=23/23
log       fatal=0 errors=1 xvfb_errors=1
```

解读与后续：这 2 分钟含预热（字体 atlas、22 个 Tab 模型、首次合成都在这段），
**不能**当作泄漏结论；门禁判定 PASS，且线程/FD 均已稳定。证据：`.logs/soak/20261117-125130/`。

### 2.5.1 15m soak（同场景，`20261117-125403`）

```text
RSS       460.5 → 526.6 MiB（+66.1 MiB，+3.15 MiB/min，R²=0.29）
PSS       450.3 → 516.3 MiB（峰值 534.8）
CPU       avg 7.7% / max 10.6%      threads/fds  112/97（全程不变）
coverage  tabs=22 splits=3 settings=34/272 find=67 copy=132 resize=168/168
commands  sent=536 shell_lag=0     log  fatal=0
```

结论：斜率从 2m 的 47 MiB/min 掉到 3.15 MiB/min，且线性拟合 R²=0.29
（更像预热后的台阶而不是稳定泄漏）；线程/FD 不变。**24h 仍需单独验收**。

### 2.5.2 吞吐与帧延迟（同日，`cargo bench` / `bench_render.sh`）

```text
PTY→可见 10 MiB 端到端   178.35 ms  ≈ 56.1 MiB/s   （目标 ≥50 MiB/s ✅）
criterion 提示相对历史基线 +78%（p<0.01），但同日并行/单独两次都是 ~178ms，
说明历史基线来自更早的构建/环境，需要重新基线化（不是 0.5.0 引入）

200x50 网格，CPU 渲染路径：
  single_row   damage  ~35.9 µs p95 39.9 µs（vs legacy full 1626 µs，45×）
  typing_burst damage  ~34.9 µs p95 37.1 µs（45×）
  full_200x50                    1610 µs p95 1663 µs
  scroll                         1621 µs（整行重绘，无加速）
```

结论：P95 帧延迟目标（≤50 ms）远未触顶（0.04–1.7 ms 量级）；滚动是唯一
“整屏重绘”路径，后续若优化优先看它。

## 3. 4 pane 测量步骤（可复现）

```bash
./target/release/tervo --cols 60 --rows 20 --title PERF &
# 右键 pane → Split Right / Split Down，做成 2x2（日志会出现 3 条 [ws] split）
APP=$(pgrep -x tervo | head -1)
awk '/VmRSS/{print $2/1024 " MB"}' /proc/$APP/status
# 空闲 CPU：5 秒内 utime+stime 增量 / 500
```

注意：菜单行位置 = `click_y + 18 + 30*行号`，`click_x` 会被夹到
`min(x, 窗口宽-160)`；窗口管理器可能挪窗口，点击前用 `xwininfo` 重新取坐标。

## 4. 打包与安装

```bash
SKIP_BUILD=1 ./scripts/package.sh /tmp/dist    # 体积门禁 ≤40 MB + strip 后 --help 冒烟
PREFIX=/tmp/prefix ./scripts/install.sh        # bin + .desktop + 图标
/tmp/prefix/bin/tervo --help
```

实测：`/tmp/dist/tervo` 32.2 MB、`file` 显示 stripped、`--help` 正常；
安装到 `/tmp/prefix` 后 `bin/tervo` 可执行，桌面项写入
`share/applications/tervo.desktop`。

## 5. 已知成本与未验收项

- **CJK 回退字体**：首次显示中文会触发一次字体解析（约 159 ms），之后常驻；
  启动路径已惰性化，不影响英文启动时间。
- **渲染后端**：有 `/dev/dri` 默认 Skia/GL；无 DRM 默认 `winit-software`；Skia 建窗
  后 1s 内确认不到 GPU/GL API 会 exec 回退软件（避免 Skia software surface 黑屏）。
- **真机 GPU 未验收**：上述 Skia 数据全部来自 llvmpipe；NVIDIA 580 上的帧率/延迟/
  长时间稳定性仍需用户用 release 复测。
- **持久帧**：已去掉每帧整屏 CPU 拷贝；Skia 仍每帧上传纹理（Slint 1.18 无安全持久
  GPU texture API），这是下一步 GPU 化的主战场。
- **未验收**：P95 帧延迟 ≤50 ms、`50 MiB/s` 吞吐、4 pane 长时间 CPU 稳定性
  （见 `docs/ACTION_ITEMS.md` ACT-032）。当前只验收了上面的启动/RSS/空闲 CPU/体积。

## 6. 回归策略

1. 任何切片合入前必须 `./scripts/selfcheck.sh` 通过（测试数量只增不减，基线在
   `docs/selfcheck-baseline.json`）。
2. 改动启动路径、字体、渲染、事件循环时，对比 `history.jsonl` 最近 5 次的
   ready / first_damage / RSS，明显回退要给出原因或修复。
3. 发布前跑 `scripts/package.sh`，体积超 40 MB 直接失败；`scripts/install.sh`
   在临时前缀演练一次。
