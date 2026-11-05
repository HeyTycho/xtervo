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

- `.logs/selfcheck/<时间戳>/summary.json`：各测试套件通过数、启动耗时、空闲 RSS、二进制大小、IME 结果
- `.logs/selfcheck/history.jsonl`：每轮追加一行，可直接画趋势
- 退出码非 0 = 有测试失败、启动预算/空闲 RSS 超阈值、或测试数量异常减少（防“删测试过关”）
- DISPLAY 不可用时启动预算与 RSS 段自动跳过，不因此判定失败

基线门禁在 [`docs/selfcheck-baseline.json`](selfcheck-baseline.json)：
`ready_ms_max` / `first_damage_ms_max` / `idle_rss_mb_max` / 各套件最少用例数。
`idle_rss_mb_max` 按字体惰性加载后的实测值留约 35% 余量（实测 46–52 MB → 70 MB），
防止 CJK 回退字体被重新变成启动时全量解析。

### 1. 度量（Measure）

| 维度 | 工具 | 频率 |
|---|---|---|
| 启动 → 首帧 | `selfcheck.sh`（3 次取中位） | 每次改动 |
| 按键 → 上屏延迟 | `--latency N` + `docs/latency-*.json` | 改渲染/输入时 |
| 全屏 FPS | `--fps SECS` / `scripts/stress_fullscreen.py` | 改渲染时 |
| 空闲 RSS / CPU | `scripts/measure_startup.py --rss-only` | 每周/改核心时 |
| VT 解析吞吐 | `cargo bench -p tervo-core --bench pty_throughput` | 改解析/帧时 |
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
cargo bench -p tervo-core --bench pty_throughput
./target/release/tervo --latency 200
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
    `[icon] 写入窗口 0x...` 取）；排查鼠标链路用 `TERVO_MOUSELOG=1`，会打印命中的格与是否上报。
- **遗留**：1003 悬停只按格去重、未做时间节流；`--title` 参数目前没真正写到窗口标题（顺手记下，下轮处理）。

### 2026-11-05 · 第 2 轮：关闭分屏后布局不恢复（幽灵分屏）

- **现象**：右键 Close Pane 关掉右侧/下方 pane 后，剩余 pane 没有铺满，被关掉的位置还留着一块
  不再刷新的画面，也不再接收输入。
- **诊断**：`close_pane_inner` 先把 id 从 `pane_rows` 删掉，`refresh_views` 再用
  `pane_rows.len() == rects.len()` 判断能否原地更新。两者此时已经相等，于是走 `set_row_data`
  分支——但 `pane_model` 还多一行，旧行永远留在模型里。
- **复现（before/after 同坐标，`--demo-splits` + 30 行窗口，右键第 3 个 pane → Close Pane）**：
  - before：`[ws] close ... panes=2 model_rows=3` —— 模型比实际多一行（幽灵分屏）。
  - after：`[ws] close ... panes=2 model_rows=2` —— 模型与布局一致。
- **附带修复**：右键菜单的夹取用了 `root.height`，但菜单坐标系是内容区（比窗口少一个 tabbar 的 32px），
  12 行窗口里「Close Pane」整行落到窗口外，右键靠下时点不到（实测点到的是 Split Down）。
  改为按内容区高度夹取后，同坐标正常命中 `action=2`。
- **测试**：新增 `closing_last_pane_must_not_update_rows_in_place`（先红后绿），把
  「模型行数必须等于布局数量」钉成不变量；app 用例 41→42，基线同步抬高。
- **门禁**：`./scripts/selfcheck.sh` PASS；ready 204.9ms / first_damage 212.8ms。
- **工程**：`git init` + 首次提交已完成；`.gitignore` 排除 `target/`、`.cargo/`、`.logs/`，
  仓库本体 1MB（不吞 16G 构建产物），后续每轮可回滚。

### 2026-11-05 · 第 3 轮：会话快照 + 一键重启

- **问题**：改完代码想验证只能整个进程重开，tab / 分屏 / cwd 全丢；`CoreEvent::Exited` 也没人处理，
  shell 退出后 pane 就僵在那里。
- **做法**：
  - `crates/app/src/session.rs`：`Snapshot`（版本 / 网格 / active / tabs）→ 每个 tab 一棵
    `Tree`（Leaf 带 cwd，Split 带 dir+ratio）。纯数据 + 纯函数，JSON 落盘用「临时文件 + rename」，
    版本不符或半截文件一律当作没有快照。
  - cwd 从子进程 `/proc/<pid>/cwd` 取，用户 cd 到哪就恢复到哪。
  - 菜单「Restart Session」：存快照 → `setsid` 拉起自身（带 `--restore` 与当前
    cols/rows/font-size/scrollback/shell/args）→ 退出旧进程；拉起成功才退出，失败保留窗口。
  - 启动时 `--restore` 走 `restore_session`：清掉默认 pane，按树分配 id、逐个带 cwd 创建，
    恢复 active/focused 后 refresh + redraw，最后消费掉快照。
- **测试**：`session.rs` 5 例（树 ↔ JSON 往返含 ratio、id 数量校验、版本门禁 + 半截文件、
  `/proc/self/cwd`、死 pid 不 panic）。
- **真机验证**：
  - 菜单重启：`已保存 … cwd=["/home/user", …]` → `已拉起新实例 pid=127` → 旧进程退出 →
    新实例 `已按快照恢复：1 个标签 / 3 个 pane`，快照被消费，新实例 3 个 shell 全在。
  - 手工快照（row 分屏，cwd=/usr 与 /etc）+ `--restore`：两个 pane 的 shell cwd 分别落到
    /usr、/etc —— 证明恢复真的用了快照里的 cwd，而不是碰巧继承。
  - 双标签快照（上下分屏 + 单 pane，active=1）：`已按快照恢复：2 个标签 / 3 个 pane`，
    cwd 依次 /usr、/etc、/var。
- **门禁**：PASS，ready 205.7ms / first_damage 207.1ms，app 47 / core 16 / conformance 10，
  基线 42→47。
- **经验**：GUI 自动化前先用 `xwin 0x<id> x y` 把窗口钉住，坐标从日志
  `[ui] tabbar menu at x,y` 反推——窗口管理器会挪窗口，写死坐标会随机点空。
- **遗留**：只存布局与 cwd，不存滚动历史/运行中程序；`Exited` 仍无 UI 提示，pane 退出后不会自动重启。

### 2026-11-05 · 第 4 轮：反向拖选选不到最左边字母

- **现象**：从右往左左键拖选，行首字母选不上；拖得越靠左，丢的字符越多。
- **诊断**：`alacritty_terminal::selection::to_range` 对 Simple 选区有两条内缩规则——
  起点是 `Side::Right` 时 `start.column += 1`，终点是 `Side::Left` 时 `end.column -= 1`。
  我们按下时固定 `Side::Left`、扩展时固定 `Side::Right`：正向拖选两头恰好正确，
  **反向拖选**点序交换后，左端变成 Right（+1，丢掉第 0 列）、右端变成 Left（−1，丢掉锚点格），
  于是 `start(5) → extend(0)` 复制出 `"bcde"` 而不是 `"abcdef"`。
- **修复**：actor 记住选区锚点，扩展时把两点排序后**重建**选区：起点固定 `Side::Left`、
  终点固定 `Side::Right`，与拖动方向无关（`crates/core/src/pane.rs` 的 `sel_anchor`）。
- **测试**：`backward_selection_includes_first_column`（先红后绿）——`start(0,5) → extend(0,0)`
  断言 `"abcdef"`，再 `→ extend(0,2)` 断言 `"cdef"`。
- **真机验证**：XTest 拖拽（9×22 格，内容 `abcdef`）：右→左拖到第 0 列，日志
  `[clipboard] 复制 6 字节: "abcdef"`；拖到第 3 列 → `"def"`。
- **经验**：alacritty 的 `Side` 描述的是「格子边界」而不是拖动方向；跨端点的选区
  不要按方向固定传 side，按点的先后决定更稳。
- **门禁**：PASS，ready 201.3ms / first_damage 202.2ms；app 47 / core 16 / conformance 10。

### 2026-11-05 · 第 26 轮：打包/安装演练 + 4 pane 性能基线

- **打包**：`SKIP_BUILD=1 ./scripts/package.sh /tmp/dist` → 183.4 MB（带符号）strip 到
  **32.2 MB**（−82.4%），体积门禁 ≤40 MB 通过，strip 后 `--help` 冒烟通过。
- **安装**：`PREFIX=/tmp/prefix ./scripts/install.sh` → `bin/tervo`（32.2 MB）+
  `.desktop` + 图标；安装后的二进制 `--help` 正常。
- **4 pane**：右键菜单做 2×2（3 条 `[ws] split`），应用 RSS **44 MB**、5 秒空闲 CPU
  **0.2%**、子进程 5 个（4 shell + gsettings）。门禁要求 ≤220 MiB / ≤1%，均通过。
- **历史趋势**：48 次 selfcheck，ready 前 10 次均值 202.5 ms → 最近 10 次 54.1 ms，
  主要来自 CJK 字体回退惰性化。
- **教训**：右键菜单行位置是 `click_y + 18 + 30*i`，且菜单 x 会被夹到
  `min(x, 宽-160)`；按固定坐标点分屏会点错行，前两次只拆出 2 pane。
- **门禁**：PASS，ready 45.7 ms / first_damage 46.8 ms；app 89 / core 66 / conformance 11。

### 2026-11-05 · 第 28 轮：设置搜索

- **实现**：设置面板标题栏加搜索框；`SETTINGS_CATALOG`（10 条，含关键词）+
  大小写不敏感子串匹配；结果浮层覆盖内容、点条目跳转对应分类并清空查询。
- **真机**：`XMODIFIERS=@im=none` 启动后，点搜索框输入 `copy` →
  `[ui] settings filter q="cop" hits=1` / `q="copy" hits=1`；点结果 →
  `[ui] settings goto cat=1`，Terminal 分类展示（.logs/settings-search-jump.png）。
- **教训 1**：默认环境有中文输入法，XTest 键入会被合成候选词（截图里出现候选窗口），
  自动化脚本必须 `XMODIFIERS=@im=none` 才能输入 ASCII。
- **教训 2**：Slint 里浮层要声明在内容**之后**才会画在上面；声明在布局前面会被
  分类页盖住（搜索浮层曾半透明透出主题网格）。
- **教训 3**：`TextInput.text` 必须双向绑定 `<=>`，否则 Rust 清空查询后输入框仍留旧文字，
  与占位符重叠。

### 2026-11-05 · 第 30 轮：最终验收与交付

- **验收**：selfcheck PASS（ready 46.7 ms / first 47.4 ms / RSS 52.5 MB）；app 90 /
  core 66 / conformance 11；打包 193 MB → 32.3 MB stripped（−82.5%，≤40 MB 门禁）；
  安装到 `/tmp/final-prefix` 后 `--help` 正常。
- **矩阵**：228 条，implemented 23 / partial 76 / needs_verification 17 /
  not_started 105 / unsupported 7，加权 28.6%。
- **交付文档**：新增 `docs/FINAL_REPORT.md`（能做什么、证据、限制、如何继续），
  更新 `CLOSURE.md` / `PERFORMANCE.md` 的最终数字。
- **结论**：能日常使用的 Slint/X11 终端工作区，但不是官方 Tabby 的完整替代；
  剩余大头（内建 SSH/SFTP、Vault、插件运行时、Quake、连接管理器 UI）未闭环。

### 2026-11-05 · 更名 Tervo + 许可证合规

- **更名**：`rusttabby-*` → `tervo-*`（crate），二进制 `tervo`，配置 `~/.config/tervo`，
  环境变量 `TERVO_*`，桌面项 `tervo.desktop`；文档/脚本同步。
- **许可证**：Slint 1.18 是 `GPL-3.0-only`/商业/royalty-free 三选一，本项目选 GPL，
  因此整体定为 **GPL-3.0-only**；新增 `LICENSE`、`THIRD_PARTY_NOTICES.md`
  （369 个运行时 crate / 198 份许可证文本）、`docs/LICENSING.md`。
- **商标风险**：原图标为“猫+终端窗口红绿灯”，与 Tabby 吉祥物高度相似，存在混淆风险；
  已替换为由 `scripts/gen_icon.py` 生成的原创几何 “T” 标记（无猫/无红绿灯），
  并在 README 加独立项目声明与更名说明。
- **可复现**：`python3 scripts/gen_third_party_notices.py` 重新生成依赖声明；
  `install.sh` 把 LICENSE/THIRD_PARTY_NOTICES/LICENSING 安装到 `share/doc/tervo/`。
- **验证**：selfcheck PASS；打包 32.1 MB（≤40 MB）；安装后 `tervo --help` 正常；
  标题栏显示新图标（`.logs/tervo-brand.png`）。

### 2026-11-05 · 修正 first_damage 测量口径

- **现象**：更名后 selfcheck 的 first_damage 从 ~47 ms 稳定变成 ~155 ms，ready 不变。
- **诊断**：`DamageWatcher` 在首帧之后才建立；找窗口用 100 ms 轮询，首帧早于监听时
  会漏掉，只能等下一次 damage，数值被高估成 100 ms 的整数倍 + 下一帧。
- **修复**：轮询改 5 ms；监听建立后强制 `redraw_all()` 再等 damage。
- **结果**：first_damage 85.7 ms（近期 60–140），属真实“窗口就绪→首帧”耗时；
  之前的 47 ms 是漏测后的偶然值。

### 2026-11-05 · F11 全屏崩溃（RefCell 二次借用）

- **现象**：运行中按 F11（或命令面板 `view.toggle-fullscreen`）直接 panic：
  `thread 'main' panicked at crates/app/src/main.rs:151:22: RefCell already borrowed`，退出码 101；
  `--fullscreen` 启动参数却正常。
- **根因**：`on_key_input` 已经用 `with_app(|st| handle_input(st, …))` 持有 `AppState` 的可变借用，
  `handle_input` 的 F11 分支又调用 `toggle_fullscreen()`，其内部再次 `with_app(...)` →
  同一 `RefCell` 二次 `borrow_mut` → panic。命令面板路径同理（`palette_accept` 的
  `with_app` 里调 `run_command`，`run_command` 再进 `toggle_fullscreen` 的 `with_app`）。
  启动参数在 `ui.show()` 前后直接 `window().set_fullscreen(true)`，不经过 `with_app`，所以没炸。
- **修复**：`toggle_fullscreen(st: &AppState)` 改为接收状态、内部不再 `with_app`；
  `handle_input` 与 `run_command` 两个调用点传 `st`。
- **验证**：F11 两次 → `[ui] fullscreen=true`（2560x1440）/`false`（540x472），无 panic；
  xkey 新增 `--key f11`（keysym 0xffc8）供自动化。
- **同类排查（本轮一并修复）**：
  - `switch_tab_relative` / `switch_tab_number`：Ctrl+Tab、Ctrl+1–9、Palette 的 Next/Previous Tab
    都会二次借用（实测 Ctrl+Tab 直接 panic），改为接收 `&mut AppState`；
  - `handle_input` 的 F2 分支：`with_app(|st| open_rename(...))` 同样二次借用，改为 `open_rename(st, ...)`；
  - `with_app` 本身加防御：`try_borrow_mut` 失败时打印 `[app] with_app 重入` 并返回 `None`，
    避免未来再因一个回调把整个终端打崩。
- **教训**：`with_app` 是“入口层”工具，任何已经拿到 `&mut AppState` 的函数都不得再调用它。
