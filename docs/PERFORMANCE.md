# 性能与打包基线（Tervo）

本文记录**可复现**的性能/体积数据与门禁。所有数字都在本仓库的开发机上测得
（Ubuntu + X11 + winit-software 渲染），换机器会变，但门禁与采集方式不变。

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
- **软件渲染**：沙箱/无 GPU 环境走 `winit-software`，60fps 与高吞吐未做正式验收。
- **未验收**：P95 帧延迟 ≤50 ms、`50 MiB/s` 吞吐、4 pane 长时间 CPU 稳定性
  （见 `docs/ACTION_ITEMS.md` ACT-032）。当前只验收了上面的启动/RSS/空闲 CPU/体积。

## 6. 回归策略

1. 任何切片合入前必须 `./scripts/selfcheck.sh` 通过（测试数量只增不减，基线在
   `docs/selfcheck-baseline.json`）。
2. 改动启动路径、字体、渲染、事件循环时，对比 `history.jsonl` 最近 5 次的
   ready / first_damage / RSS，明显回退要给出原因或修复。
3. 发布前跑 `scripts/package.sh`，体积超 40 MB 直接失败；`scripts/install.sh`
   在临时前缀演练一次。
