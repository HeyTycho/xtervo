# 自我迭代方案：让终端模拟器每轮都变得更好

> 目标不是“让 AI 随便改代码”，而是建立一条**可度量、可回滚、有人类门禁**的改进流水线：
> 每一轮都必须带着「失败用例 / 基线数据 / 截图」进来，带着「全部通过 + 指标不退化」出去。

## 1. 总闭环

```
        ┌─────────────────────────────────────────────────────────┐
        │  0. 基线：./scripts/selfcheck.sh                        │
        │     build + 单测 + VT 契约 + 启动预算 (+ IME) → JSON    │
        └───────────────┬─────────────────────────────────────────┘
                        ▼
   ┌────────────┐    ┌────────────┐    ┌────────────┐    ┌────────────┐
   │ 1. 度量    │ →  │ 2. 诊断    │ →  │ 3. 改进    │ →  │ 4. 门禁    │
   │ 性能/资源  │    │ 契约/差分  │    │ agent 补丁 │    │ 自检+评审  │
   │ 启动/延迟  │    │ fuzz/issue │    │ 先写测试   │    │ 回滚/记录  │
   └────────────┘    └────────────┘    └────────────┘    └─────┬──────┘
         ▲                                                      │
         └──────────────── 5. 沉淀基线 / 文档 / 下一轮 ◄────────┘
```

### 0. 一条命令的基线

```bash
./scripts/selfcheck.sh            # 常规
RUN_IME=1 ./scripts/selfcheck.sh  # 加中文 IME 冒烟（需要 X11 + fcitx/ibus）
```

产出：

- `.logs/selfcheck/<时间戳>/summary.json`：各测试套件通过数、启动耗时、二进制大小、IME 结果
- `.logs/selfcheck/history.jsonl`：每轮追加一行，可直接画趋势
- 退出码非 0 = 有测试失败、启动预算超阈值、或测试数量异常减少（防“删测试过关”）

基线门禁在 [`docs/selfcheck-baseline.json`](selfcheck-baseline.json)：
`ready_ms_max` / `first_damage_ms_max` / 各套件最少用例数。

### 1. 度量（Measure）

| 维度 | 工具 | 频率 |
|---|---|---|
| 启动 → 首帧 | `selfcheck.sh`（3 次取中位） | 每次改动 |
| 按键 → 上屏延迟 | `--latency N` + `docs/latency-*.json` | 改渲染/输入时 |
| 全屏 FPS | `--fps SECS` / `scripts/stress_fullscreen.py` | 改渲染时 |
| 空闲 RSS / CPU | `scripts/measure_startup.py --rss-only` | 每周/改核心时 |
| VT 解析吞吐 | `cargo bench -p rusttabby-core --bench pty_throughput` | 改解析/帧时 |
| 二进制大小 | `selfcheck.sh` 自动记录 | 每轮 |

原则：**没有基线数据的“优化”不算优化**；没有 before/after 的 PR 不进。

### 2. 诊断（Diagnose）

- **行为契约**：`crates/core/tests/vt_conformance.rs` 已覆盖 SGR/256/truecolor、光标移动/擦除、
  自动换行、宽字符、备用屏、bracketed paste、OSC 标题。新增终端特性必须先加一条契约用例。
- **差分测试**：同一段字节流分别喂给 `Term` 和参考实现（alacritty / xterm / tmux），比较整屏文本、
  光标、模式位；先用 `vttest` / `esctest` 的测试脚本生成输入，再逐帧比对。
- **Fuzz**：对 `vte::Processor` 输入做随机/变异 fuzz（`cargo-fuzz`），目标是不 panic、不越界、
  UTF-8 边界安全；崩溃样本固化成回归用例。
- **真实使用反馈**：从 issue / 日志里收集“哪个 TUI 不正常”（vim、htop、fzf、tmux、neovim、dsh 等），
  每个问题先写成可复现脚本，再谈修复。
- **视觉回归**：主题/字体/中文渲染用 `xwd → PNG` 截图对比（当前 `.logs/` 已有基线图），
  改 UI 时至少留 before/after 两张。

### 3. 改进（Improve）—— agent 分工

一轮迭代固定四个角色（可同一模型多开，也可人工）：

1. **Scout**：从 selfcheck/history、失败用例、issue、性能探针里挑**一个**最高价值问题，
   产出「复现步骤 + 期望行为 + 验收指标」。
2. **Implementer**：先写失败测试，再改最小代码；只动必要文件；给出 before/after 数据。
3. **Verifier**：独立跑 `selfcheck.sh` + 相关基准 + 截图；检查 diff 是否越界、是否引入新依赖、
   是否偷偷放宽测试；不通过直接打回。
4. **Historian**：更新基线、文档、`docs/tabby-parity.md` 状态；把经验写进本文件或注释。

约束：

- 一轮只解决一个主题，diff 尽量 < 400 行；大重构拆多轮。
- 必须先有失败用例（或明确的性能 before 数据）；不接受“看起来更好”。
- 不自动合并、不自动发布；Verifier 通过后由人 review。
- 失败/超时 3 次即回滚本轮，记录原因，换下一个问题。

### 4. 门禁（Guard）

- `selfcheck.sh` 全绿 + 阈值不退化 + 用例数不减少。
- 改 VT/渲染必须附带 conformance/截图；改性能必须附 before/after。
- 新增依赖需说明理由与许可证；核心依赖（alacritty_terminal / Slint）升级单独一轮。
- 每轮结束保留 `.logs/selfcheck/<stamp>/` 与截图，可回滚、可复盘。

## 3. 接下来 3 轮的候选

按“用户每天能感知 + 风险可控”排序：

1. ~~**鼠标上报**~~ ✅ 2026-11-05 完成（见下方「迭代记录」）；仅剩 host 侧用 `vim`/`htop`
   做一次人工端到端确认。
2. **主题热重载**：`notify` 监听 `themes/` + 配置文件，200ms debounce；非法文件保留 last-good；
   selfcheck 增加“改文件后 UI 自动换色”的截图用例。
3. **差分/Fuzz 基础设施**：把 `vttest`/`esctest` 的固定脚本接进 conformance；加一个最小 fuzz
   目标与 30 秒 smoke fuzz；把崩溃样本固化成测试。

再往后：会话恢复、Profile、分屏拖动、SSH/Serial、Wayland IME、emoji/连字、脏矩形上传。
完整功能盘点见 [`tabby-parity.md`](tabby-parity.md)。

## 4. 一句话使用方式

```bash
# 每轮开始前/后各跑一次，比较 history.jsonl
./scripts/selfcheck.sh
# 有明确优化目标时再跑重基准
cargo bench -p rusttabby-core --bench pty_throughput
./target/release/rusttabby-app --latency 200
```

只要这条闭环在，终端模拟器就可以持续“自我迭代”，且每次改进都有证据、有回归保护。

## 5. 迭代记录

### 2026-11-05 · 第 1 轮：鼠标上报（vim / htop / tmux）

- **问题**：TUI 里点击、滚轮全部失效——鼠标事件被本地选词吞掉，PTY 收不到任何序列。
- **诊断**：`alacritty_terminal` 只跟踪 `TermMode` 的 1000/1002/1003/1005/1006 位，
  **不生成**转义序列（那部分在 alacritty 的 app 层）；而 `pane.rs` 也只有 Selection 路径，
  没有任何「鼠标 → PTY」的通道。
- **改动**：
  - 新增 `crates/core/src/mouse.rs`：`encode()` 纯函数，覆盖 SGR 1006 / legacy X10 / UTF-8 1005；
    修饰键 4/8/16；坐标 1-based；legacy 坐标钳到 223；1000 丢弃移动、1002 只在按住时报、1003 悬停上报。
  - `Ctrl::Mouse` + actor 侧门禁（以解析器的实时模式为准，UI 多转发一次也不会误写 PTY）；
    `PaneHandle::mode()/wants_mouse()/wants_motion()` 用 `AtomicU32` 做无阻塞快照，
    UI 不必为「转发还是选词」付一次同步往返。
  - UI 接线：`PointerEvent.modifiers` → 位掩码；Shift+点击绕过上报回到本地选词；
    鼠标模式下右键不再弹菜单；滚轮转成 64/65 上报。
- **测试**：
  - `crates/core/tests/mouse_report.rs` 16 例，含真 PTY 端到端——shell 打开 1000+1006 后
    把 stdin 收到的字节落盘，断言恰好是 `ESC [ < 0 ; 3 ; 2 M`（门禁在 actor 里、字节确实进了 PTY）。
  - **真机 GUI 端到端**（XTest 注入 + winit-software）：点 (3,11) 格收到 `ESC[<0;12;4M` 与
    `ESC[<0;12;4m`；点 (7,44) 格收到 `ESC[<0;45;8M`/`m`；滚轮（XTest button 4）收到 `ESC[<64;23;5M`。
    坐标 = 格 + 1，按下 `M` / 抬起 `m`，与 xterm 语义一致。
- **门禁**：`./scripts/selfcheck.sh` PASS；ready 204.7ms（基线 450）、first_damage 211.2ms（基线 600）、
  app 41 / core 16 / conformance 10，用例数基线同步抬高（39→41、15→16）。
- **经验**：
  - Slint 1.18 的回调实参里 `int` 的按位或 `|` 会解析失败（“expected ')'”），用 `+` 拼互斥位即可。
  - 修饰键从 `PointerEvent.modifiers` 取，`scroll-event` 里也有 `modifiers`。
  - 沙箱里 `/tmp` 每次 bash 调用都会重建：GUI 验证脚本必须和启动写在**同一条命令**里，否则
    `--shell /bin/sh -- /tmp/xxx.sh` 指向不存在的文件，pane 静默退出、模式位当然不生效。
  - 同名窗口会误导 `xwin`：现已支持 `xwin 0x<id> x y` 按 id 激活/移动（id 从 app 日志的
    `[icon] 写入窗口 0x...` 取）；排查鼠标链路用 `RUSTTABBY_MOUSELOG=1`，会打印命中的格与是否上报。
- **遗留**：1003 悬停只按格去重、未做时间节流；`--title` 参数目前没真正写到窗口标题（顺手记下，下轮处理）。
