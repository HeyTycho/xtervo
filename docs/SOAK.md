# 稳定性 soak（W-07）

可复跑的长时稳定性压测：在独立 Xvfb 上持续制造输出/交互/图片/多 Tab/resize 压力，
按固定间隔采样 RSS/PSS/CPU/线程/FD/子进程/存活/日志错误，结束时生成 **PASS/FAIL +
CSV + Markdown**。脚本只面向 release 前的稳定性门禁，不修改 core/UI 行为。

脚本： [`scripts/soak.sh`](../scripts/soak.sh)

## 快速开始

```bash
# 2 分钟脚本门禁（先验证脚本自身能完整跑完）
DURATION=2m ./scripts/soak.sh

# 默认 30 分钟
DURATION=30m ./scripts/soak.sh

# 指定二进制 / 输出目录 / 显示器
DURATION=30m \
  BIN=$PWD/target/release/tervo \
  SOAK_OUT=$PWD/.logs/soak/manual-30m \
  ./scripts/soak.sh
```

前置：release 二进制与调试 X 工具（同一次构建）：

```bash
CARGO_HOME=/home/user/AIworkspace/rusttabbyterminal/.cargo \
  cargo build --release --offline -p tervo-app \
  --bin tervo --bin xwin --bin xclick --bin xkey
```

依赖：`Xvfb`、`openbox`、`x11-utils`（`xdpyinfo`/`xwininfo`/`xprop`）、`python3`、
`ffmpeg`（截图可选）、`xsel`（本脚本不依赖）。Xvfb/Openbox 默认从主仓
`.perf/` 查找，可用 `SOAK_XVFB_BIN`/`SOAK_OPENBOX_BIN` 覆盖。

## 隔离与安全

- 固定独立显示 **`:143`**（`SOAK_DISPLAY` 可改）；显示已被占用时**直接失败**，
  绝不 kill 占用者。不碰 `:1`，不启动 Xephyr，不使用 `pkill`。
- 只 kill 本脚本记录的 Xvfb / Openbox / Tervo PID，并在 kill 前校验
  `/proc/<pid>/cmdline`，避免 PID 复用误伤。
- `HOME`、`XDG_CACHE_HOME`、`XDG_STATE_HOME`、`TERVO_CONFIG`、`TERVO_LOG_DIR`
  全部指向输出目录，不写用户真实配置。
- 默认 `SLINT_BACKEND=winit-software`（headless 无 GPU）；截图用 `ffmpeg x11grab`。
- `DURATION` 支持 `30s` / `2m` / `15m` / `1h` 或纯秒数；默认 `30m`。

## 场景（每个 cycle 都做）

| 场景 | 实现 |
|---|---|
| 输出洪峰 | 受控 shell `cat` 约 1.9 MB 的 256 色/truecolor 大文本 ×2；`seq 1 200000`；ANSI 光标定位 12000 行 |
| 滚动 | XTest 滚轮 12 下 + 8 上；偶数 cycle 拖右侧滚动条 |
| Sixel | 每 cycle 输出 120x72 条纹 + 64x48 四色条带两张合法 DCS |
| IIP | 每 cycle 输出 512x512 RGBA PNG（inline）+ 96x64 PNG（width/height 按 cell） |
| emoji / 宽字符 | 每 cycle 120 行 ZWJ 家庭、旗帜、肤色、组合标记、CJK |
| 多 Tab | 共 22 个 Tab（21 次 `Ctrl+T`） |
| 分屏 | 活动 Tab 内 2 次 Split Right + 1 次 Split Down（4 pane） |
| 切 Tab / pane | `Ctrl+Tab`、`Ctrl+Shift+Right` 后向当前 pane 继续输出 |
| Settings | 命令面板打开 Settings Tab，点击 8 个分类，Esc 关闭（每 2 cycle） |
| 复制 / 搜索 | 拖选 + `Ctrl+Shift+C`；`Ctrl+F` 输入查询并 Esc |
| 窗口 resize | 900x620、1180x740、1100x700，经 Openbox + `xwin` 真实 configure |

所有输入都通过 XTest（`xclick`/`xkey`）或 PTY 标准输入进入产品，不直接调用 core API。

## 采样与报告

每 `SOAK_SAMPLE_INTERVAL`（默认 10s）记录：

`elapsed_s, rss_kb, pss_kb, cpu_pct, threads, fds, children, app_alive,
xvfb_alive, win_alive, log_fatal, log_errors, mem_available_kb, phase`

- RSS 来自 `/proc/<pid>/status` 或 `smaps_rollup`；PSS 优先 `smaps_rollup`；
- CPU 用 `/proc/<pid>/stat` 的 utime+stime 差分；
- fatal 标记：`[panic]`/`panicked`/`out of memory`/`allocation failed`/`fatal`/SIGSEGV；
- 报告包含预热后基线/结束中位数、线性斜率与 R²、CPU 均值/峰值、线程/FD/子进程变化、
  均匀抽取的采样表、场景覆盖计数、告警、限制和复现命令。

输出目录：

```
.logs/soak/<stamp>/
├── samples.csv        # 逐次采样
├── report.md          # 人读报告（PASS/FAIL）
├── summary.json       # 机器可读判定
├── app.log            # Tervo stderr（含 panic）
├── logs/tervo-*.log   # 按次日志
├── xvfb.log / openbox.log
├── shots/             # start/settings/end 截图（有 ffmpeg 时）
└── config.toml        # 本次配置（22 Tab 场景由脚本驱动）
```

## PASS/FAIL 判定

**硬失败（FAIL）**

- 运行中任意采样 `app_alive=0` 或 `xvfb_alive=0`，或结束时进程已死；
- 日志出现 panic / OOM / fatal 标记；
- 实际运行时长明显短于 `DURATION`，或运行被中断；
- 场景覆盖不足：`tabs<22`、`splits<3`、Settings 未打开/分类切换<4、搜索未打开、
  resize 成功 0 次；
- RSS 持续增长：预热后增长 >96 MiB 且斜率 >2 MiB/min 且 R²>0.55，或绝对增长 >192 MiB。

**告警（仍 PASS，但报告记录）**：RSS 增长 >32 MiB 或斜率 >1 MiB/min、线程/FD 增长、
日志 `error/failed`、Xvfb 日志错误、复制未确认、shell 输入确认滞后、短跑警告。

## 实测结果（基线 `08ef1aa`，app 327 tests）

环境：Linux x86_64，31 GiB 内存，28 线程；headless Xvfb 1280x800x24 +
Openbox + Mesa llvmpipe + `winit-software`；release 二进制
`target/release/tervo`（SHA256 `bfb72218…4120a2`）。

### 2m 快速门禁（2026-11-07 00:07–00:10，最终脚本）

- 命令：`DURATION=2m SOAK_DISPLAY=:144 SOAK_OUT=$PWD/.logs/soak/quick-2m-final ./scripts/soak.sh`
- 结果：**PASS**，8 个 cycle，13 个采样；tabs=22、splits=3、
  Settings 打开 4 次/分类切换 32 次、find=8、copy=14、resize 20/20、
  受控命令 64 条全部确认；无 panic/OOM/日志 error；Xvfb/app 全程存活。
- RSS/PSS：整体峰值 **1506.4 / 1420.0 MiB**；预热后基线 1407.7 MiB →
  结束 1488.5 MiB（**+80.8 MiB，+5.7%**），斜率 +97.3 MiB/min，R²=0.99；
  CPU 平均 80.9%、峰值 88.4%；线程 166、FD 92、子进程 25 全程稳定。
- 证据：`.logs/soak/quick-2m-final/`（report.md / samples.csv / summary.json / shots）。

**2m 结论**：脚本端到端可运行，无崩溃、无 OOM、无输入确认丢失；但 2m 太短，
RSS 斜率只能作门禁参考，不能外推长时稳定性。

### 15m 长跑（2026-11-07 00:01–00:16，:143）

- 命令：`DURATION=15m SOAK_OUT=$PWD/.logs/soak/long-15m ./scripts/soak.sh`
- 实际采样 **905s / 60 个 cycle / 91 次采样**；22 Tab、3 split、Settings 打开 32 次/
  分类切换 256 次、find=61、copy=112、受控命令 488 条全部确认、resize 150/150（按
  cycle 计数，运行输出无失败告警）；**无 panic/OOM/日志 error，app/Xvfb 到最后采样
  仍存活**；线程 166、FD 92、子进程 25 全程不变；CPU 平均 81.0% / 峰值 103.7%。
- RSS/PSS：1s 启动 212.1 MiB；第一个 cycle 后 ~1.31 GiB；预热后基线 1610.7 MiB →
  结束 1838.9 MiB（**+228.2 MiB，+14.2%**），斜率 **+16.7 MiB/min（R²=0.86）**；
  整体峰值 RSS/PSS **1844.1 / 1815.4 MiB**。
- **判定：FAIL**（触发内存硬门禁：增长 >96 MiB 且斜率 >2 MiB/min 且 R²>0.55）。
  这不是崩溃/OOM，而是**持续内存增长**；24h 会更严重，需另开任务定位后再谈
  “无持续内存增长”。
- 证据：`.logs/soak/long-15m/`；报告由 905s CSV 离线补生成（收尾时 agent 热编辑
  `scripts/soak.sh` 被 bash 增量解析打断，未执行 SIGTERM；工作负载/采样完整，
  详见报告顶部归档说明）。

### 资源高峰与已知观察

- 第一个 cycle 的 `seq 1 200000` + `cat` 大文本会让 RSS 从 ~370 MiB 短时冲到
  ~1.31 GiB；之后 15m 内继续缓升（总峰值 1844.1 MiB，斜率 16.7 MiB/min）。
  高度怀疑是**大 burst 在滚动缓冲/帧交换上的瞬时高水位 + 后续缓存/未回收**；
  15m 仍未收敛，需要更长跑 + 堆分析确认。本任务不做代码修复，只记录复现方式和证据。
- 15m 长跑最终判定 **FAIL（memory growth）**；2m 门禁 PASS 只说明脚本/场景可跑，
  不能作为长时稳定性通过依据。
- 22 Tab + 4 pane 本身约 370 MiB（25 个 PTY shell、166 线程）；这是本场景基线，
  不代表单 Tab 日常内存。
- 2m 样本里 Xvfb 日志有 1 行匹配 `error/fatal` 的告警（X server 无害 warning），
  未导致存活/采样异常。

## 限制

- 本任务只做压测脚本和证据，**未改 core/UI**，也未修复任何观察到的内存高水位。
- 未跑 24h/72h；headless Xvfb + 软件渲染，不代表真机 GPU/Wayland/IME/音频。
- 单窗口 22 Tab + 4 pane；未覆盖多窗口、SSH/SFTP/Serial/Telnet、插件、Vault 解锁、
  真实剪贴板管理器、真实输入设备。
- 复制通过应用日志 `[clipboard] 复制` 确认；Xvfb 无剪贴板管理器。
- 内存 PASS 阈值是启发式（RSS/PSS + 斜率 + R²），短跑波动会标 WARN；最终泄漏判定
  仍需更长跑（如 2h/24h）+ heap profile。
- `TERVO_CONFIG`/日志/home 均在本次输出目录，不污染用户配置。

## 复现长跑

```bash
cd /home/user/AIworkspace/rusttabbyterminal/.worktrees/soak
DURATION=15m SOAK_OUT=$PWD/.logs/soak/long-15m BIN=$PWD/target/release/tervo \
  ./scripts/soak.sh 2>&1 | tee .logs/soak-long-15m.out
```

跑完后看：

```bash
cat .logs/soak/long-15m/report.md
cat .logs/soak/long-15m/summary.json
column -s, -t < .logs/soak/long-15m/samples.csv | less
```

## 分段隔离（feat/memory-fix）

为定位 15m 的 RSS 增长，用同一份 `scripts/soak.sh` 的临时副本把 cycle 拆成
单场景（`DURATION=2m/3m`、`SOAK_SAMPLE_INTERVAL=5`、22 Tab 或 1 Tab + 4 pane），
其余环境与正式门禁一致；**未修改任何 PASS/FAIL 阈值**。release `f6ae438` 上结果：

| 分段 | 时长 | RSS 变化 | 结论 |
|---|---|---|---|
| idle（22 Tab + 4 pane，无操作） | 3m | 389.6 → 389.9 MiB | 稳定 |
| flood+seq（1 Tab + 4 pane） | 3m | 232.1 → 232.4 MiB | 稳定，history 封顶 2000 |
| 仅 Sixel+IIP | 3m | 231.3 → 231.3 MiB | 稳定 |
| 仅 resize（210/210 成功） | 3m | 238.3 → 248.7 MiB（R²=0.15） | 抖动，无线性增长 |
| 仅 copy/search | 3m | 271.2 → 272.4 MiB | +1.2 MiB |
| 仅 Settings（31 开/248 类） | 3m | 279.8 → 280.4 MiB | 稳定 |
| **仅 emoji** | 2m | **232 → 1062 MiB，之后 +0.1 MiB** | **首帧 emoji 一次性 +800 MiB** |

### emoji 首帧 800 MiB 根因（已定位）

用受控 shell 逐条输出单个字符/组合（真实 UTF-8，经 PTY）后逐段采样 RSS：

| 输入 | RSS 变化 |
|---|---|
| `A` / `😀` / `❤️` / ZWJ 家庭 / 国旗 | 稳定（225–237 MiB） |
| `漢字` | +165 MiB |
| 组合附加符 `Z̸̧̛̪̎` | +18 MiB |
| keycap `1️⃣` | +341 MiB |
| 彩虹旗 `🏳️‍🌈` | +301 MiB |
| 之后 120 行 emoji.txt | 稳定在 ~1062 MiB |

逐段合计约 +800 MiB，且**一次性、不随重复 emoji 再增长**。同时：
- Renderer 指针稳定、彩色 glyph 缓存 mono=31/color=16/decodes=16/hits 数千，
  排除逐帧重建与彩色缓存未命中；
- resize 旧 buffer、ImageStore/inline cache 均未增长；
- `font_probe` 单独解析 7 个回退字体（合计约 100 MB 文件）进程峰值 RSS
  **340 MiB**（fontdue 解析 CJK/TTC 的常驻放大），说明增量来自**首次按需解析
  回退字体**，而不是终端帧/图像缓冲。

修复（最小、低风险）：
1. 组合标记/零宽附加符走新的 `GlyphCache::get_mark`：只在主字体取字形，
   **不再为单个 keycap/U+20E3 等标记扫描并解析整套 CJK/emoji 回退字体**；
2. CJK 回退候选按文件大小升序（优先 3.8 MB Droid，避免为个别生僻字直接解析
   18 MB NotoSansCJK TTC）；
3. `GlyphCache` 单色/彩色缓存加显式上限（8192/1024，超限整体丢弃并复用 atlas），
   防止异常码位种类使每个 pane 缓存无界增长。
单测：`combining_mark_never_builds_fallback_chain`、
`glyph_cache_is_bounded_by_explicit_limits`。

**修复后实测**（同一 quickbisect，1 Tab）：从 212 MiB 起步，`漢字` → 332、
keycap → 334（+2）、彩虹旗 → 344（+11）、随后整行 emoji.txt → 345（+1），
结束 347 MiB。首帧 emoji 总增量从 ~850 MiB 降到 **~135 MiB**，keycap/彩虹旗的
两段大跳变消失；剩余主要是首个 CJK 字符仍会按需解析一个 CJK 回退字体。

**未完成/限制**：尚未在修复版上复跑完整 15m；15m 的 +228 MiB 慢斜率与该一次性
跳变的关系仍需后续确认。普通 CJK 首字仍会解析一个 CJK 回退字体；若要求首帧
<100 MiB，需要进一步换用按字形惰性解析的字体后端或去掉大 CJK 回退。门禁阈值
未改。
