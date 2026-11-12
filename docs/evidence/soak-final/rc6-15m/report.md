# Tervo 稳定性 soak 报告（W-07）

- 结论：**PASS**
- 开始：`2026-11-12T13:31:10+08:00`
- worktree：`/home/user/AIworkspace/rusttabbyterminal`
- 分支 / commit：`master` / `da0a195`
- 二进制：`/home/user/AIworkspace/rusttabbyterminal/target/release/tervo`
- SHA256：`7d1dfa79826f34b165aa4739351386ef11ba298105a447ea352c89c99b072458`（295785864 bytes）
- 显示：`:143`（Xvfb 1280x800x24，无 :1、无 Xephyr、无 pkill）
- 配置：22 Tab、4 pane、scrollback=2000、winit-software、Sixel+IIP 开
- 时长：请求 900s，实际采样到 911s；完成 67 个压力 cycle
- 采样：91 次，每 10s；预热基线取 t≥180s 的前 5 个样本

> 说明：这是 headless Xvfb + 软件渲染的 **长时压测**，不是 24h 结论；本报告只覆盖本次实际运行的时长。

## 1. 场景覆盖

| 场景 | 观测值 / 证据 |
|---|---|
| 输出洪峰（cat 大文本 / seq 20 万行） | 发送受控命令 536 条；shell 未确认 0 条 |
| 20+ Tab | 日志最大 `tabs=22`（目标 22） |
| 分屏 | `[ws] split pane=` 3 次（目标 3） |
| 滚轮 / 滚动条 | 每 cycle 12 下 + 8 上，偶数 cycle 拖滚动条 |
| Sixel / IIP | 每 cycle 各 2 张（条纹 + 尺寸参数变体） |
| emoji / 宽字符 / 组合字符 | 每 cycle 120 行，含 ZWJ/旗帜/肤色/组合标记 |
| Settings 打开 / 分类切换 | 打开 34 次，分类切换 272 次 |
| 复制 | `[clipboard] 复制` 132 次 |
| 搜索 | `[ui] find open` 67 次 |
| 窗口 resize | 成功 168/168 次（900x620、1180x740、1100x700） |

## 2. 资源趋势（预热后）

| 指标 | 基线（前 5 样本中位） | 结束（后 5 样本中位） | 变化 | 线性斜率 |
|---|---|---|---|---|
| RSS | 884.3 MiB | 940.7 MiB | +56.4 MiB（+6.4%） | +2.60 MiB/min（R²=0.27） |
| PSS | 850.0 MiB | 904.6 MiB | +54.6 MiB | — |
| 峰值 RSS/PSS | 952.6 / 918.1 MiB | — | — | — |
| CPU | 平均 109.5% | 峰值 126.3% | — | — |
| 线程 | 168 | 168 | +0 | — |
| FD | 94 | 94 | +0 | — |
| 子进程（PTY shell） | 25 | 25 | +0 | — |

## 3. 采样摘要

| 时间 | RSS MiB | PSS MiB | CPU | 线程 | FD | 子进程 | 阶段 |
|---|---|---|---|---|---|---|---|
| 181s | 870.4 | 835.9 | 117.9% | 168 | 94 | 25 | settings |
| 211s | 892.0 | 857.7 | 96.8% | 168 | 94 | 25 | settings |
| 241s | 913.7 | 879.3 | 99.8% | 168 | 94 | 25 | flood |
| 272s | 918.0 | 883.6 | 114.8% | 168 | 94 | 25 | scroll |
| 302s | 939.4 | 904.8 | 117.6% | 168 | 94 | 25 | tabs |
| 332s | 939.8 | 905.4 | 108.2% | 168 | 94 | 25 | flood |
| 362s | 944.0 | 909.3 | 114.6% | 168 | 94 | 25 | scroll |
| 392s | 949.3 | 914.4 | 112.4% | 168 | 94 | 25 | tabs |
| 422s | 944.7 | 910.2 | 117.9% | 168 | 94 | 25 | settings |
| 452s | 952.6 | 918.1 | 100.8% | 168 | 94 | 25 | settings |
| 482s | 952.0 | 916.1 | 103.5% | 168 | 94 | 25 | flood |
| 512s | 950.4 | 914.4 | 110.4% | 168 | 94 | 25 | scroll |
| 543s | 947.9 | 912.7 | 120.9% | 168 | 94 | 25 | tabs |
| 573s | 942.1 | 907.1 | 107.6% | 168 | 94 | 25 | flood |
| 603s | 944.3 | 908.5 | 97.8% | 168 | 94 | 25 | scroll |
| 633s | 949.7 | 913.6 | 115.7% | 168 | 94 | 25 | tabs |
| 663s | 943.9 | 907.9 | 122.7% | 168 | 94 | 25 | settings |
| 693s | 945.3 | 909.3 | 100.1% | 168 | 94 | 25 | settings |
| 723s | 944.7 | 908.7 | 96.0% | 168 | 94 | 25 | flood |
| 753s | 943.0 | 906.9 | 97.9% | 168 | 94 | 25 | scroll |
| 783s | 942.6 | 906.3 | 116.6% | 168 | 94 | 25 | tabs |
| 814s | 937.6 | 901.6 | 106.4% | 168 | 94 | 25 | flood |
| 844s | 939.9 | 903.9 | 114.5% | 168 | 94 | 25 | scroll |
| 874s | 944.3 | 908.2 | 113.9% | 168 | 94 | 25 | tabs |
| 904s | 937.1 | 901.1 | 86.0% | 168 | 94 | 25 | settings |

## 4. 判定明细

- 硬失败：0 项
- 告警：3 项
  - WARN: RSS 增长需关注：56.4 MiB，斜率 2.60 MiB/min，R²=0.27
  - WARN: 日志出现 error/failed 1 次（不一定是致命错误，见 app.log）
  - WARN: Xvfb 日志出现 error/fatal 1 行

## 5. 限制

- 未运行 24h；不能把本次结果当作 24h/72h 稳定性结论。
- headless Xvfb + Mesa/llvmpipe + `winit-software`；未验证真实 GPU、Wayland/compositor、IME、音频。
- 单窗口 22 Tab + 4 pane；未覆盖多窗口、SSH/SFTP/Serial/Telnet、插件、Vault 解锁等外部 I/O。
- 滚动/复制/搜索通过 XTest 注入；Xvfb 没有真实输入设备/剪贴板管理器，复制只验证应用日志与堆栈，不等价于真机剪贴板体验。
- 内存判定是启发式阈值（RSS/PSS + 线性斜率 + R²），短跑波动会被标注为 WARN；泄漏最终需更长跑 + 堆分析确认。
- 整个脚本只 kill 自己记录的 Xvfb/Openbox/Tervo PID；不占用 :1、不启动 Xephyr、不使用 pkill。

## 6. 复现

```bash
cd /home/user/AIworkspace/rusttabbyterminal
DURATION=2m  ./scripts/soak.sh   # 快速门禁
DURATION=30m BIN=$PWD/target/release/tervo ./scripts/soak.sh
```

## 7. 证据文件

- `samples.csv`：逐次采样（RSS/PSS/CPU/线程/FD/存活/日志错误）
- `app.log` / `logs/tervo-*.log`：应用 stderr 与按次日志
- `xvfb.log` / `openbox.log`：X server / WM
- `shots/`：start/settings/end 截图（如环境有 ffmpeg）
- `summary.json`：机器可读判定

