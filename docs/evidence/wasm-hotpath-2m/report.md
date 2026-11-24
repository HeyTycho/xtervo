# xtervo 稳定性 soak 报告（W-07）

- 结论：**PASS**
- 开始：`2026-11-12T21:20:02+08:00`
- worktree：`/home/user/AIworkspace/xtervo/.worktrees/agent-wasm-hotpath`
- 分支 / commit：`agent/wasm-hotpath` / `fab1f2a`
- 二进制：`/home/user/AIworkspace/xtervo/.worktrees/agent-wasm-hotpath/target/release/xtervo`
- SHA256：`65aef37738e9ddb17da053380b2156f7553c94dbeeecf4415d2a24abf82158a8`（361947088 bytes）
- 显示：`:148`（Xvfb 1280x800x24，无 :1、无 Xephyr、无 pkill）
- 配置：22 Tab、4 pane、scrollback=2000、winit-software、Sixel+IIP 开
- 时长：请求 120s，实际采样到 136s；完成 9 个压力 cycle
- 采样：14 次，每 10s；预热基线取 t≥40s 的前 5 个样本

> 说明：这是 headless Xvfb + 软件渲染的 **长时压测**，不是 24h 结论；本报告只覆盖本次实际运行的时长。

## 1. 场景覆盖

| 场景 | 观测值 / 证据 |
|---|---|
| 输出洪峰（cat 大文本 / seq 20 万行） | 发送受控命令 72 条；shell 未确认 0 条 |
| 20+ Tab | 日志最大 `tabs=22`（目标 22） |
| 分屏 | `[ws] split pane=` 3 次（目标 3） |
| 滚轮 / 滚动条 | 每 cycle 12 下 + 8 上，偶数 cycle 拖滚动条 |
| Sixel / IIP | 每 cycle 各 2 张（条纹 + 尺寸参数变体） |
| emoji / 宽字符 / 组合字符 | 每 cycle 120 行，含 ZWJ/旗帜/肤色/组合标记 |
| Settings 打开 / 分类切换 | 打开 5 次，分类切换 40 次 |
| 复制 | `[clipboard] 复制` 16 次 |
| 搜索 | `[ui] find open` 9 次 |
| 窗口 resize | 成功 23/23 次（900x620、1180x740、1100x700） |

## 2. 资源趋势（预热后）

| 指标 | 基线（前 5 样本中位） | 结束（后 5 样本中位） | 变化 | 线性斜率 |
|---|---|---|---|---|
| RSS | 360.7 MiB | 396.4 MiB | +35.7 MiB（+9.9%） | +45.50 MiB/min（R²=0.99） |
| PSS | 352.4 MiB | 388.3 MiB | +36.0 MiB | — |
| 峰值 RSS/PSS | 413.7 / 405.6 MiB | — | — | — |
| CPU | 平均 9.4% | 峰值 12.4% | — | — |
| 线程 | 111 | 111 | +0 | — |
| FD | 95 | 95 | +0 | — |
| 子进程（PTY shell） | 25 | 25 | +0 | — |

## 3. 采样摘要

| 时间 | RSS MiB | PSS MiB | CPU | 线程 | FD | 子进程 | 阶段 |
|---|---|---|---|---|---|---|---|
| 1s | 57.9 | 49.4 | 0.0% | 15 | 23 | 1 | setup |
| 11s | 193.0 | 184.5 | 6.2% | 111 | 95 | 25 | seq |
| 21s | 324.5 | 315.6 | 7.7% | 111 | 95 | 25 | settings |
| 31s | 330.9 | 321.3 | 8.2% | 111 | 95 | 25 | scroll |
| 41s | 347.2 | 337.6 | 10.0% | 111 | 95 | 25 | scroll |
| 51s | 348.3 | 339.8 | 6.4% | 111 | 95 | 25 | settings |
| 61s | 360.7 | 352.4 | 10.2% | 111 | 95 | 25 | tabs |
| 71s | 371.8 | 363.5 | 12.4% | 111 | 95 | 25 | tabs |
| 81s | 375.9 | 367.8 | 7.7% | 111 | 95 | 25 | flood |
| 91s | 380.0 | 371.9 | 10.6% | 111 | 95 | 25 | flood |
| 101s | 394.7 | 386.7 | 11.0% | 111 | 95 | 25 | settings |
| 111s | 396.4 | 388.3 | 8.2% | 111 | 95 | 25 | scroll |
| 122s | 407.4 | 399.3 | 10.8% | 111 | 95 | 25 | scroll |
| 132s | 413.7 | 405.6 | 6.8% | 111 | 95 | 25 | settings |

## 4. 判定明细

- 硬失败：0 项
- 告警：4 项
  - WARN: RSS 增长需关注：35.7 MiB，斜率 45.50 MiB/min，R²=0.99
  - WARN: 日志出现 error/failed 1 次（不一定是致命错误，见 app.log）
  - WARN: Xvfb 日志出现 error/fatal 1 行
  - WARN: 本次仅 136s 的短跑；内存斜率只作门禁参考，不能外推 24h

## 5. 限制

- 未运行 24h；不能把本次结果当作 24h/72h 稳定性结论。
- headless Xvfb + Mesa/llvmpipe + `winit-software`；未验证真实 GPU、Wayland/compositor、IME、音频。
- 单窗口 22 Tab + 4 pane；未覆盖多窗口、SSH/SFTP/Serial/Telnet、插件、Vault 解锁等外部 I/O。
- 滚动/复制/搜索通过 XTest 注入；Xvfb 没有真实输入设备/剪贴板管理器，复制只验证应用日志与堆栈，不等价于真机剪贴板体验。
- 内存判定是启发式阈值（RSS/PSS + 线性斜率 + R²），短跑波动会被标注为 WARN；泄漏最终需更长跑 + 堆分析确认。
- 整个脚本只 kill 自己记录的 Xvfb/Openbox/xtervo PID；不占用 :1、不启动 Xephyr、不使用 pkill。

## 6. 复现

```bash
cd /home/user/AIworkspace/xtervo/.worktrees/agent-wasm-hotpath
DURATION=2m  ./scripts/soak.sh   # 快速门禁
DURATION=30m BIN=$PWD/target/release/xtervo ./scripts/soak.sh
```

## 7. 证据文件

- `samples.csv`：逐次采样（RSS/PSS/CPU/线程/FD/存活/日志错误）
- `app.log` / `logs/xtervo-*.log`：应用 stderr 与按次日志
- `xvfb.log` / `openbox.log`：X server / WM
- `shots/`：start/settings/end 截图（如环境有 ffmpeg）
- `summary.json`：机器可读判定

