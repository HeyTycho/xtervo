# xtervo 稳定性 soak 报告（W-07）

- 结论：**PASS**
- 开始：`2026-11-12T21:35:51+08:00`
- worktree：`/home/user/AIworkspace/rusttabbyterminal`
- 分支 / commit：`master` / `9799f03`
- 二进制：`/home/user/AIworkspace/rusttabbyterminal/target/release/xtervo`
- SHA256：`6b833573a83cbbbbcdc9a14c26c475782ad4d7c47136258bb5951829c4ace13a`（366194960 bytes）
- 显示：`:143`（Xvfb 1280x800x24，无 :1、无 Xephyr、无 pkill）
- 配置：22 Tab、4 pane、scrollback=2000、winit-software、Sixel+IIP 开
- 时长：请求 1800s，实际采样到 1806s；完成 134 个压力 cycle
- 采样：180 次，每 10s；预热基线取 t≥180s 的前 5 个样本

> 说明：这是 headless Xvfb + 软件渲染的 **长时压测**，不是 24h 结论；本报告只覆盖本次实际运行的时长。

## 1. 场景覆盖

| 场景 | 观测值 / 证据 |
|---|---|
| 输出洪峰（cat 大文本 / seq 20 万行） | 发送受控命令 1072 条；shell 未确认 0 条 |
| 20+ Tab | 日志最大 `tabs=22`（目标 22） |
| 分屏 | `[ws] split pane=` 3 次（目标 3） |
| 滚轮 / 滚动条 | 每 cycle 12 下 + 8 上，偶数 cycle 拖滚动条 |
| Sixel / IIP | 每 cycle 各 2 张（条纹 + 尺寸参数变体） |
| emoji / 宽字符 / 组合字符 | 每 cycle 120 行，含 ZWJ/旗帜/肤色/组合标记 |
| Settings 打开 / 分类切换 | 打开 67 次，分类切换 536 次 |
| 复制 | `[clipboard] 复制` 266 次 |
| 搜索 | `[ui] find open` 134 次 |
| 窗口 resize | 成功 335/335 次（900x620、1180x740、1100x700） |

## 2. 资源趋势（预热后）

| 指标 | 基线（前 5 样本中位） | 结束（后 5 样本中位） | 变化 | 线性斜率 |
|---|---|---|---|---|
| RSS | 884.6 MiB | 940.6 MiB | +56.0 MiB（+6.3%） | +0.50 MiB/min（R²=0.11） |
| PSS | 854.5 MiB | 910.7 MiB | +56.2 MiB | — |
| 峰值 RSS/PSS | 953.6 / 923.5 MiB | — | — | — |
| CPU | 平均 102.5% | 峰值 124.2% | — | — |
| 线程 | 168 | 168 | +0 | — |
| FD | 94 | 94 | +0 | — |
| 子进程（PTY shell） | 25 | 25 | +0 | — |

## 3. 采样摘要

| 时间 | RSS MiB | PSS MiB | CPU | 线程 | FD | 子进程 | 阶段 |
|---|---|---|---|---|---|---|---|
| 141s | 818.7 | 792.8 | 115.3% | 168 | 94 | 25 | tabs |
| 212s | 893.4 | 863.8 | 92.6% | 168 | 94 | 25 | settings |
| 282s | 927.3 | 897.6 | 104.2% | 168 | 94 | 25 | scroll |
| 352s | 948.2 | 918.1 | 104.2% | 168 | 94 | 25 | scroll |
| 422s | 948.4 | 918.7 | 113.6% | 168 | 94 | 25 | resize |
| 493s | 940.7 | 911.0 | 104.2% | 168 | 94 | 25 | resize |
| 563s | 941.0 | 910.9 | 92.6% | 168 | 94 | 25 | settings |
| 633s | 942.2 | 912.5 | 110.9% | 168 | 94 | 25 | copy-search |
| 703s | 945.7 | 915.9 | 121.4% | 168 | 94 | 25 | copy-search |
| 774s | 939.5 | 909.4 | 91.3% | 168 | 94 | 25 | settings |
| 844s | 938.6 | 908.9 | 103.3% | 168 | 94 | 25 | emoji |
| 914s | 941.6 | 911.9 | 93.7% | 168 | 94 | 25 | images |
| 984s | 949.1 | 918.7 | 112.6% | 168 | 94 | 25 | resize |
| 1055s | 934.4 | 904.4 | 78.8% | 168 | 94 | 25 | resize |
| 1125s | 942.2 | 912.3 | 94.3% | 168 | 94 | 25 | settings |
| 1195s | 939.3 | 909.0 | 78.1% | 168 | 94 | 25 | scroll |
| 1265s | 941.9 | 912.0 | 122.1% | 168 | 94 | 25 | copy-search |
| 1336s | 940.8 | 910.9 | 100.8% | 168 | 94 | 25 | settings |
| 1406s | 941.7 | 911.8 | 102.9% | 168 | 94 | 25 | images |
| 1476s | 945.2 | 914.9 | 94.7% | 168 | 94 | 25 | images |
| 1546s | 949.5 | 919.5 | 110.5% | 168 | 94 | 25 | panes |
| 1617s | 934.0 | 904.0 | 105.8% | 168 | 94 | 25 | resize |
| 1687s | 940.5 | 910.2 | 92.7% | 168 | 94 | 25 | settings |
| 1757s | 940.6 | 910.7 | 107.5% | 168 | 94 | 25 | copy-search |
| 1797s | 945.1 | 915.2 | 92.2% | 168 | 94 | 25 | emoji |

## 4. 判定明细

- 硬失败：0 项
- 告警：3 项
  - WARN: RSS 增长需关注：56.0 MiB，斜率 0.50 MiB/min，R²=0.11
  - WARN: 日志出现 error/failed 1 次（不一定是致命错误，见 app.log）
  - WARN: Xvfb 日志出现 error/fatal 1 行

## 5. 限制

- 未运行 24h；不能把本次结果当作 24h/72h 稳定性结论。
- headless Xvfb + Mesa/llvmpipe + `winit-software`；未验证真实 GPU、Wayland/compositor、IME、音频。
- 单窗口 22 Tab + 4 pane；未覆盖多窗口、SSH/SFTP/Serial/Telnet、插件、Vault 解锁等外部 I/O。
- 滚动/复制/搜索通过 XTest 注入；Xvfb 没有真实输入设备/剪贴板管理器，复制只验证应用日志与堆栈，不等价于真机剪贴板体验。
- 内存判定是启发式阈值（RSS/PSS + 线性斜率 + R²），短跑波动会被标注为 WARN；泄漏最终需更长跑 + 堆分析确认。
- 整个脚本只 kill 自己记录的 Xvfb/Openbox/xtervo PID；不占用 :1、不启动 Xephyr、不使用 pkill。

## 6. 复现

```bash
cd /home/user/AIworkspace/rusttabbyterminal
DURATION=2m  ./scripts/soak.sh   # 快速门禁
DURATION=30m BIN=$PWD/target/release/xtervo ./scripts/soak.sh
```

## 7. 证据文件

- `samples.csv`：逐次采样（RSS/PSS/CPU/线程/FD/存活/日志错误）
- `app.log` / `logs/xtervo-*.log`：应用 stderr 与按次日志
- `xvfb.log` / `openbox.log`：X server / WM
- `shots/`：start/settings/end 截图（如环境有 ffmpeg）
- `summary.json`：机器可读判定

