# xtervo 稳定性 soak 报告（W-07）

- 结论：**PASS**
- 开始：`2026-11-12T12:05:44+08:00`
- worktree：`/home/user/AIworkspace/xtervo`
- 分支 / commit：`master` / `3919021`
- 二进制：`/home/user/AIworkspace/xtervo/target/release/xtervo`
- SHA256：`b6ca22db4f937561c0e24a86c386a95f0eee6aa667426d13f36c8ff3b3b7ff7d`（290009248 bytes）
- 显示：`:143`（Xvfb 1280x800x24，无 :1、无 Xephyr、无 pkill）
- 配置：22 Tab、4 pane、scrollback=2000、winit-software、Sixel+IIP 开
- 时长：请求 1800s，实际采样到 1810s；完成 134 个压力 cycle
- 采样：181 次，每 10s；预热基线取 t≥180s 的前 5 个样本

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
| RSS | 882.9 MiB | 936.4 MiB | +53.5 MiB（+6.1%） | +0.40 MiB/min（R²=0.07） |
| PSS | 830.5 MiB | 884.3 MiB | +53.7 MiB | — |
| 峰值 RSS/PSS | 953.4 / 900.8 MiB | — | — | — |
| CPU | 平均 109.3% | 峰值 127.5% | — | — |
| 线程 | 168 | 168 | +0 | — |
| FD | 94 | 94 | +0 | — |
| 子进程（PTY shell） | 25 | 25 | +0 | — |

## 3. 采样摘要

| 时间 | RSS MiB | PSS MiB | CPU | 线程 | FD | 子进程 | 阶段 |
|---|---|---|---|---|---|---|---|
| 141s | 814.8 | 762.0 | 120.1% | 168 | 94 | 25 | tabs |
| 212s | 891.0 | 838.8 | 100.3% | 168 | 94 | 25 | settings |
| 282s | 926.8 | 874.4 | 114.3% | 168 | 94 | 25 | scroll |
| 352s | 946.9 | 894.7 | 105.5% | 168 | 94 | 25 | scroll |
| 422s | 948.5 | 896.3 | 124.8% | 168 | 94 | 25 | resize |
| 493s | 944.1 | 891.9 | 113.0% | 168 | 94 | 25 | resize |
| 563s | 942.6 | 890.2 | 98.3% | 168 | 94 | 25 | settings |
| 633s | 943.7 | 891.6 | 121.0% | 168 | 94 | 25 | copy-search |
| 703s | 946.0 | 893.8 | 127.5% | 168 | 94 | 25 | copy-search |
| 774s | 940.0 | 887.7 | 107.5% | 168 | 94 | 25 | settings |
| 844s | 938.7 | 886.5 | 113.2% | 168 | 94 | 25 | images |
| 914s | 941.2 | 889.0 | 95.0% | 168 | 94 | 25 | images |
| 984s | 945.1 | 892.5 | 118.9% | 168 | 94 | 25 | tabs |
| 1055s | 939.2 | 886.8 | 89.0% | 168 | 94 | 25 | panes |
| 1125s | 938.2 | 886.0 | 98.5% | 168 | 94 | 25 | settings |
| 1195s | 936.8 | 884.3 | 94.2% | 168 | 94 | 25 | scroll |
| 1265s | 943.1 | 890.7 | 119.8% | 168 | 94 | 25 | scroll |
| 1336s | 939.9 | 887.5 | 117.2% | 168 | 94 | 25 | settings |
| 1406s | 941.1 | 888.5 | 109.9% | 168 | 94 | 25 | flood |
| 1476s | 940.6 | 888.2 | 101.0% | 168 | 94 | 25 | flood |
| 1547s | 942.7 | 890.1 | 116.3% | 168 | 94 | 25 | tabs |
| 1617s | 939.5 | 886.7 | 120.0% | 168 | 94 | 25 | tabs |
| 1687s | 940.8 | 888.6 | 90.4% | 168 | 94 | 25 | settings |
| 1758s | 939.3 | 887.2 | 111.3% | 168 | 94 | 25 | images |
| 1808s | 930.1 | 877.7 | 107.3% | 168 | 94 | 25 | resize |

## 4. 判定明细

- 硬失败：0 项
- 告警：3 项
  - WARN: RSS 增长需关注：53.5 MiB，斜率 0.40 MiB/min，R²=0.07
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
cd /home/user/AIworkspace/xtervo
DURATION=2m  ./scripts/soak.sh   # 快速门禁
DURATION=30m BIN=$PWD/target/release/xtervo ./scripts/soak.sh
```

## 7. 证据文件

- `samples.csv`：逐次采样（RSS/PSS/CPU/线程/FD/存活/日志错误）
- `app.log` / `logs/xtervo-*.log`：应用 stderr 与按次日志
- `xvfb.log` / `openbox.log`：X server / WM
- `shots/`：start/settings/end 截图（如环境有 ffmpeg）
- `summary.json`：机器可读判定

