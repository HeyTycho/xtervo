# Phase 0 报告：可行性验证

> 状态：**已完成（含 2 项环境阻塞，需决策）**
> 环境：Linux / X11 `:1` / 2560×1440@59.95 Hz / 28 核
> 构建：release（`lto=thin`, `debug=1`），全部 15 个测试通过

---

## 1. 完成了什么

| # | 交付物 | 位置 |
|---|---|---|
| P0-1 | Cargo workspace（core / app），依赖锁定 alacritty_terminal 0.26 / Slint 1.18 / portable-pty 0.9 | `Cargo.toml`, `crates/` |
| P0-2 | VT 核心接入：**每 pane 一个 `Term` + 专属 actor 线程**；全部跨线程通道有界；UI 侧拉取式脏行快照 | `crates/core/src/pane.rs` |
| P0-2 | alacritty 0.26 API 探测结论（见 §2） | 本文件 |
| P0-3 | criterion 基准：解析吞吐 / 整屏与脏行取帧 / `cat 10MB` 端到端 | `crates/core/benches/pty_throughput.rs` |
| P0-3 | 洪峰压测：`yes` 持续输出下拉帧 P99 = 1.89 ms（debug），不阻塞 UI | `crates/core/tests/smoke.rs` |
| P0-4 | 延迟测量装置：XTest 注入真实按键 → **X11 Damage 上屏打点**；另含应用内管线打点 | `crates/app/src/latency.rs` |
| P0-5 | 最小 Slint 单窗格切片：JetBrains Mono 字形 atlas、脏行重绘、光标形状、颜色表；**不含**分屏/标签/菜单 | `crates/app/`, `crates/app/ui/main.slint` |
| P0-6 | 资源基线脚本：冷启动、空闲 RSS、空闲 CPU | `scripts/measure_startup.py` |
| — | 一键回归脚本 / 真机 GPU 验证脚本 | `scripts/{bench,phase0,run_gpu_validation}.sh` |

渲染切片已人工目视验证（截图见 `.logs/win2.png`）：主题背景 `#1e1e2e`、等宽字形、ANSI 颜色、块状光标均正确。

---

## 2. alacritty_terminal 0.26 API 探测结论

1. **damage 满足拉取式模型**
   `Term::damage() -> TermDamage::{Full, Partial(LineDamageBounds{line,left,right})}` + `Term::reset_damage()`。
   行级 + 列区间粒度，足够做「脏行优先」的局部拷贝。
   ⚠️ 当 `grid().display_offset() != 0`（滚动历史可见）时行号与视口不再一一对应，必须整屏重取——已实现并在代码注释中标明。

2. **`Term` 自身不产生 `Wakeup`**
   `Event::Wakeup` 是 alacritty 的 `EventLoop` 发出的，`Term` 只发 Title / Bell / PtyWrite / Clipboard 等。
   ⇒ actor 在每批 `processor.advance()` 之后自行触发**容量 1 的合并唤醒**（实测 1000 次 notify 只留 1 次）。

3. **`renderable_content()` 不适合每帧调用**（会整屏遍历）；直接 `grid()` + damage 自建快照，实测整屏 200×50 = **54.7 µs**。

4. **历史缓冲是预分配的**：RSS ≈ f(scrollback)。2000 行 × 120 列 × 16 B ≈ 3.8 MB；10000 行则 ~19 MB。这是 §3 RSS 数字的直接决定项。

5. **EOF 陷阱（已修复）**：PTY 读线程在 EOF 后把 `PtyClosed` 发到控制通道，但数据通道里可能仍有积压；若在收到 `PtyClosed` 时直接退出 actor，会丢掉最后 ~64 KB（实测 `cat 10MB` 卡死在 10 423 104 / 10 485 796 字节）。修复：收到 `PtyClosed` 时先用 `try_recv` **排空数据通道**，再收尾。

---

## 3. 性能数据

> ⚠️ 后端说明：开发沙箱**没有 `/dev/dri`**，Slint 的 Skia(wgpu/GL) 后端可创建窗口但拿不到可呈现的 surface（进程无报错、窗口内容为空）。
> 因此下表用 `SLINT_BACKEND=winit-software` 采集——它能完整跑通「拉帧 → 光栅化 → Image → 上屏」链路，但**不能代表 Skia 的 GPU 上传/合成开销**。
> 真机验证脚本：`./scripts/run_gpu_validation.sh`。

| 指标 | 目标 | 实测 | 判定 |
|---|---|---|---|
| 冷启动 → 首帧上屏 | < 400 ms | **184.2 ms**（6 次：175.8 / 202.1，中位 184.2） | ✅ |
| 空闲 RSS（单窗格，120×32） | < 50 MB | **42.2 MB**（+ 子 shell 7.7 MB） | ✅ |
| 空闲 CPU | — | **0.00 %** | ✅ |
| 按键 → 屏幕可见（端到端） | 中位 < 8 ms，P99 < 20 ms | **中位 16.16 ms，P99 18.26 ms** | ❌ 中位数超标，P99 达标 |
| 同上：应用内管线（key 回调 → 帧交给 Slint） | — | **中位 39.8 µs，P99 202 µs**（598 样本） | ✅ 瓶颈不在自研部分 |
| vim/htop 全屏刷新 | 稳定 60 FPS | **55–58 FPS**，帧间隔中位 16.68 ms，P99 34.1 ms，CPU 13 % | ⚠️ 偶发并帧 |
| `cat` 10 MB 端到端 | < 1 s | **89 ms**（73–104 ms，20 次） | ✅ 11× 余量 |
| VT 解析吞吐 | — | **195–202 MiB/s**（10 MB 纯文本 / SGR） | — |
| 整屏取帧 200×50（10 000 格） | — | **54.7 µs** | ✅ |
| 单行脏行取帧 | — | **15.9 µs** | ✅ |
| 洪峰下拉帧延迟（`yes` 满速输出） | 不阻塞 UI | 平均 0.43 ms / 最差 1.89 ms（debug 构建） | ✅ |

原始数据：`docs/bench-output.txt`、`docs/{startup,latency,fps}-winit-software.json`。

### 延迟构成拆解（为什么是 16 ms）

```
XTest 注入 ──X server──► winit ──► Slint key 回调 ──► actor ──► Term 回显
    │                                                              │
    │                       39.8 µs（实测应用内管线）               │
    │                                                              ▼
    └──────────── 16.16 ms（实测端到端）──────────── Slint 帧节流（15.9 ms）──► 光栅化 ──► X Damage
```

根因：Slint winit 后端的 `TimerBasedFrameThrottle`（`i-slint-backend-winit/frame_throttle.rs`）
对**每次**重绘都新起一个 `1000/刷新率` 的定时器（重绘后若无 pending 就停表），
且 `slint::Window::request_redraw()` 也走同一节流。
⇒ 60 Hz 屏上，端到端延迟 ≈ 1 个完整刷新周期，与实现无关；中位 < 8 ms 在 60 Hz + vsync 下不可达（理论下界 ~8.3 ms）。

---

## 4. 未解决问题

1. **【阻塞】Skia GPU 路径未验证**：沙箱无 GPU，无法回答「Skia 纹理上传 + 合成能否 60 FPS / 延迟如何」。需要真机跑 `scripts/run_gpu_validation.sh`。
2. **【需决策】延迟硬指标与刷新率冲突**：中位 < 8 ms 需要 ≥120 Hz 或绕过帧节流，或改测「帧提交」而非「上屏」。
3. **全屏 60 FPS 偶发并帧**（P99 = 2 帧）：与第 2 点同源（节流器相位漂移），非算力问题（CPU 仅 13 %）。
4. 每帧一次整屏 `SharedPixelBuffer` 拷贝 + 纹理上传；后续应做**脏矩形上传**或自定义 Skia 集成。
5. 字体仅有 JetBrains Mono，无 CJK/emoji 回退；无 IME、鼠标选择、滚轮、剪贴板、超链接、搜索。
6. 只有单窗格；分屏树 / 标签页 / 会话管理尚未开始（按约束留给 Phase 1+）。

---

## 5. 下一阶段建议（Phase 1）

**优先级 1（先把阻塞项清零）**
- 真机 Skia 验证：跑 `scripts/run_gpu_validation.sh`，回填 `docs/gpu-*.json`。
- 帧时钟重构：自建与 vsync 对齐的 pull/paint 节奏，替换或绕过 Slint 的按需节流；目标「中位 ≤ 1 帧、P99 ≤ 2 帧」。
- 脏矩形上传：只把变化区域交给 `Image`（或按脏带切多个 Image），削减 2.7 MB/帧 的拷贝与上传。

**优先级 2（功能面）**
- 输入层补全：修饰键组合、鼠标拖选/滚轮、粘贴/复制（OSC52）、搜索、超链接。
- 多 pane：应用层分屏树 + 标签页（每 pane 一个 actor，复用 `PaneHandle`）。
- 会话管理：SSH(russh) / Serial(serialport) 作为 `Pane` 的传输层后端，接口对齐现有的 reader/writer 线程。

**优先级 3（工程化）**
- 性能看板：把 criterion + 延迟/帧率探针接入 CI（对比基线，回归即报警）。
- 插件宿主（fidius）只做接口占位，Phase 1 不实现。

---

## 6. 复现方式

```bash
export CARGO_HOME=$PWD/.cargo          # 沙箱内 ~/.cargo 只读
cargo test -p rusttabby-core           # 15 个测试
./scripts/bench.sh                     # criterion
./scripts/phase0.sh                    # 启动/RSS/延迟/FPS 全套
SLINT_BACKEND=winit-skia ./scripts/run_gpu_validation.sh   # 真机 GPU
```

运行交互式终端：

```bash
SLINT_BACKEND=winit-software ./target/release/rusttabby-app --cols 120 --rows 32
```
