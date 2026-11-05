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

### 2026-11-05 · Tabby config.yaml 导入/导出

- **实现**：新增 `tabby_import.rs`（serde_yaml）：解析 `profiles`（local/ssh/serial/telnet）
  与 `terminal.fontSize`，重名自动加 `(tabby n)`；未知类型进报告不静默丢；导出为 Tabby 兼容 YAML。
- **入口**：CLI `--import-tabby PATH` / `--export-tabby PATH`（无 UI，迁移脚本可用）；
  设置 → Config 页新增 Import Tabby / Export Profiles 按钮 + 状态行。
- **真机**：CLI 导入 3 个（跳过 kubernetes，字号 16）；设置页导入 2 个 + 字号 15；
  导出 2 个 → `/tmp/ui2/tabby-profiles.yaml`。
- **依赖**：只增加 `serde_yaml 0.9`（3 个传递 crate），符合“少依赖”原则。
- **限制**：不导入 hotkeys/themes/插件配置；密码/私钥口令不进 Vault；报告只有计数 + 首个跳过原因。

### 2026-11-05 · SFTP 建目录/重命名/删除

- 面板新增 Target 输入 + New Dir / Rename / Delete；Delete 采用“再点一次确认”，
  避免误删；目录删除先 `rm` 失败再 `rmdir`（不递归，安全）。
- 修 `ls -1 /abs` 回显绝对路径导致拼成 `//probe.txt`：新增 `remote_entry_path`，
  绝对路径直接用。
- 测试服务脚本补 mkdir/rmdir/remove/rename（paramiko `SFTPServerInterface`）；
  端到端：建目录 → newdir 出现；Rename 目录 → renamed.txt；Delete 文件两次确认后消失。
- 注意：Slint 里 `sftp-target` 曾同时用作 Profile 下标和 Target 字符串，已拆成
  `sftp-profile`（int）与 `sftp-target`（string），避免属性冲突。

### 2026-11-05 · 右键菜单“有时候唤不出” + 转发管理面板

- **现象**：右键有时不弹菜单；空闲时可复现率低，连续右键/高输出时更明显。
- **根因**：菜单原先在 **mouse-up** 才打开，而 pane 的 right-down 返回 `reject`（Slint 不 grab），
  后续 up 可能被其它元素/分隔线 4px 命中带吃掉；连续右键时第二次仅关掉旧菜单。
- **修复**：右键 **按下即打开并 accept**，up 保留兜底；菜单已开时在外部右键会“关掉并在新位置重开”；
  菜单夹取改用真实 150×246。
- **验证**：空闲 8/8；100fps 输出下 12/12；连续右键 5/5 均重新显示（.logs/rightclick-consecutive.png）。
  结论：刷新本身不会关掉菜单，之前是“依赖 up 事件”的设计在刷新/移动下不稳。
- **转发管理面板**：Port Forwards 菜单 → 按 SSH Profile 列出/添加/删除 `-L` 规则并落盘，
  提示“下次连接生效”。真机：加 `18080:127.0.0.1:9000` → profiles.toml 写入；删除后清空。
- **测试工具**：xkey 之前只发 level-0 键，冒号/大写会丢；改为按 keysym level 自动按 Shift。

### 2026-11-05 · TabBar/Tab 右键菜单无法自动关闭

- **现象**：右键 TabBar 或某个 Tab 打开菜单后，只能点菜单项关闭；点外部一直留着。
- **根因**：两个菜单（`tabmenu-open` / `tabctx-open`）只有菜单项里的 `= false`，
  没有全窗口的“点外面关闭”层；pane 菜单有 backdrop，Tab 菜单漏了。
- **修复**：新增覆盖全窗口的透明 TouchArea（放在菜单之前、菜单之下），任意键按下即关闭并
  调用 `menu-dismissed`；Esc 也会关闭所有菜单（pane/TabBar/per-tab）。
- **验证**：右键 TabBar → 菜单；点内容区 → `[ui] menu dismissed`，菜单消失；
  per-tab 菜单同样（.logs/tabmenu-open.png / tabmenu-dismiss.png / tab-ctx-*.png）；
  Esc 关闭计数也验证。

### 2026-11-05 · 重开最近关闭的 Tab + 字号缩放快捷键

- **重开 Tab**：关闭时抓 `TabSnapshot`（布局树 + 每个 pane 的 cwd），压入最多 10 个的
  `closed_tabs`；Ctrl+Shift+T / 菜单 Reopen Closed Tab / 命令面板走同一个 `reopen_closed_tab`。
  恢复逻辑与 `--restore` 共用 `restore_tab_snapshot`（新 shell、原布局与 cwd）。
  真机：tab2 `cd /tmp` 关闭 → 重开 → `pwd` 输出 `/tmp`（.logs/reopen-tab.png）。
- **字号缩放**：Ctrl+= / Ctrl+- / Ctrl+0 → `zoom_font`/`reset_font`，复用原命令面板实现；
  真机字号 15→16→17→16→15，日志 `[ui] font_size=`。
- **注意**：`restore_session` 改为复用 `restore_tab_snapshot`，避免两套恢复逻辑漂移。
- 命令表新增 `tab.reopen-closed`（ctrl+shift+t），app 用例 95 全绿。

### 2026-11-05 · 双击选行 + 右键粘贴 + 广播输入 + 方向键焦点

- **双击选行**：Slint 1.18 的 TouchArea 没有 double-clicked，改为 Rust 侧 400ms/同格检测；
  core 新增 `Ctrl::SelectLine`（行尾空白裁剪 + 强制全量重绘），core 测试 `select_line_selects_single_visible_line`；
  真机双击第 1/2 行 → `double-click select line=1/2`，剪贴板 `bbb`/`ccc`，单击不复制。
- **右键粘贴**：`right_click_paste` 配置 + 设置页 Terminal 开关；开启后 pane 右键聚焦并粘贴，
  divider 右键仍开菜单；真机 `[ui] right-click paste` + `paste bytes=3`，cat 回显。
- **方向键焦点**：`focus_pane_dir` 按方向投影取最近 pane；xkey 补 Up/Down/Left/Right 注入；
  真机 3 pane 切换日志正确。
- **广播输入**：右键菜单 Broadcast ON/OFF，键入发给当前 Tab 所有 pane；真机两 pane 同时执行
  `echo BCAST` 并显示输出。限制：无危险命令确认，状态全局。
- **X11 窗口定位**：`find_own_window` 改为在 PID 匹配窗口里取“可见且面积最大”，避免辅助窗口
  （1x1）抢图标；测试环境里曾因此把图标写到短命窗口，自动化按日志 id 会 BadWindow。
- 清理 `main` 的 unreachable 警告与 xconn 未用 import；app 用例 95 全绿。

### 2026-11-05 · 设置改成 Tabby 式左右分列

- **布局**：窗口级设置面板由“两行分类按钮 + 下方内容”改为左侧垂直导航 + 右侧内容 + 顶部搜索；
  面板 520×440（随窗口收缩），居中在 TabBar 下方，不再盖住 TabBar/窗口按钮。
- **导航**：新增 NavItem 组件（左对齐、active 高亮），6 个分类；分类切换会打
  `[ui] settings category=N` 便于自动化。
- **页面适配**：Appearance 主题网格 4 列降为 66px；Terminal 保留字号/回滚/4 个开关；
  Config 按钮缩到 96/108/112；Hotkeys/Plugins 列表高度按右栏调整；搜索覆盖右栏。
- **真机验证**：Appearance/Terminal/Config 截图；Terminal 页右键粘贴开关点击 →
  `[ui] right_click_paste=true`；搜索 `font` → “Terminal · Font size”。
  截图：.logs/settings-2col.png / -terminal.png / -config.png / -search.png。
- **排查过程**：测试环境里有旧的 2560×1413 Tervo 窗口（宿主 PID 968901，非本会话）盖住屏幕
  导致点击被吞；改为按 `_NET_WM_PID` 选本进程窗口并 `xwin` 移到空白区，不杀用户窗口。

### 2026-11-05 · “+” 下拉（Tabby 式新建 Tab 菜单）

- TabBar 的 “+” 不再直接建默认 Tab，而是打开下拉：`New Tab（默认 shell）` +
  已保存 Profile（按 kind 标注 Local/SSH/Serial/Telnet）+ `Manage Profiles…`。
- 菜单锚定 “+” 的 `absolute-position`，点外部/Esc 关闭；选中后自动关菜单。
- 真机（2 个 local Profile）：菜单项正确；点 local-a → `[ui] newtab pick=1`，
  新 Tab 标题 `local-a`、shell 正常。截图 .logs/newtab-menu.png / newtab-profile-tab.png。
- 说明：按用户要求只做 Tabby 主线的功能；此前试做的 `~/.ssh/config` 导入已撤回
  （不纳入本项目主线）。
- 限制：没有搜索/键盘上下选择；Profile 很多时菜单会裁剪（后续可加 Flickable）。

### 2026-11-05 · Quake Mode + 可落盘日志/诊断 + 模态面板右键关闭

- **Quake Mode（Tabby B10）**：X11 被动 grab `F12`（config `quake_hotkey`），热键线程
  经 `invoke_from_event_loop` 调 `toggle_quake_inner`；顶部居中、屏宽 70%/屏高 50%，
  `_NET_WM_STATE_ABOVE/STICKY` + 激活聚焦；设置 Window 页开关，命令
  `view.toggle-quake`。
  - 真机踩坑：`window.hide()` 会让 Slint 在“最后一个窗口隐藏”时退出事件循环，第二次
    F12 后进程直接消失（日志停在 `[quake] hide`）。改为 `set_minimized(true)`，
    F12 恢复，进程/PTY/热键都活着。
  - 真机：5120x1440 屏 → 3584x720 at 768,0；Quake 内右键菜单、hide/show 后再右键都正常。
- **日志/诊断**：新增 `logging.rs`，用 pipe+dup2 把 stderr（含 panic/backtrace）tee 到
  `~/.local/state/tervo/logs/tervo-<epoch>.log`（保留 8 个；`TERVO_LOG=0` 关闭）；
  新增 `tervo --diagnose`：环境、屏幕、指针、活动窗口、本进程/其他 Tervo 实例窗口、
  最近 40 行日志。
  - 右键追踪：pane 的 right down/up 加 `[trace]`；日志里没有 trace = 事件没到 Tervo。
  - 实测发现宿主上有一个旧的 2560x1413 Tervo 窗口（pid 1062405）盖住新窗口；启动日志
    现在会警告“另一个可见的 Tervo 窗口…可能挡住点击”。
- **右键关闭模态面板**：设置/Profiles/SFTP/Forwards/Background 面板的遮罩改为**任意键**
  按下即关闭；面板内右键也会关闭（面板内没有右键菜单）。注意：右键落在面板内的按钮
  （ChoiceBtn 等）仍会被按钮的 TouchArea 吃掉，这是 Slint 命中顺序的预期行为。
- 文档：`docs/LOGGING.md`、README 日志段、UI-QUAKE-001..005、UI-SET-011；app 95 测试。

### 2026-11-05 · 右键在 Agent TUI pane 里失效（鼠标上报）

- **现场日志**（用户 17:33 实例 pid 1270604，`~/.local/state/tervo/logs/tervo-1793871518.log`）：
  `pane-down-right` 12 次，只有 1 次 `right click pane=`（新开的 pane 2），pane 1 的 11 次
  只有 trace、没有菜单。截图显示 pane 1 跑的是 deepseek-flash/Claude Code TUI。
- **原因**：该 TUI 发送 `ESC[?1000h ESC[?1006h` 开启 SGR 鼠标上报；旧逻辑在 mouse-reporting
  时把右键转发给 PTY，所以 Tervo 不弹菜单。不是 dshtui 冲突，也不是窗口被挡。
- **修复**：改为 Tabby 语义——右键**始终**先开 Tervo 菜单（不再被 TUI 吞掉），Shift+右键才
  转发给 TUI；中键仍转发。trace 保留 `pane-right-forward` 仅用于 Shift 转发路径。
- 真机：mouse-reporting pane 内普通右键 → `[ui] right click pane=1`，菜单出现
  （.logs/mouse-mode-right-menu.png）。

### 2026-11-05 · 右键对齐 Tabby：四模式 + 长按 + 中键粘贴

- 查 Tabby 源码（`tabby-terminal/src/api/baseTerminalTab.component.ts` / `config.ts`）：
  默认 `rightClick: 'menu'`，另有 paste / clipboard / off；在 xterm 外层宿主拦截
  mousedown/mouseup，paste/clipboard 以 250ms 区分短按/长按；contextmenu 永远 preventDefault；
  鼠标上报的 TUI 也不会拿到右键。
- 实现：config `right_click` + `paste_on_middle_click`；旧 `right_click_paste` 迁移为 paste；
  Rust 记录 right-down 时间/坐标，抬起时短按执行、长按弹菜单；clipboard 有选区复制并
  `Ctrl::ClearSelection` 清除，否则粘贴；中键按配置粘贴；Slint 在 pane 内始终本地处理
  右键/中键（仅 Shift+右键转发给 TUI）。
- 真机：paste 短按 → `paste bytes=11`，长按 → `right click pane=1`；clipboard 选区 →
  `right-click clipboard copy 5 chars`；off → 无动作；middle → `middle-click paste`
  （.logs/right-click-modes.png、settings-right-click-modes.png）。

### 2026-11-05 · 多窗口 MVP（Ctrl+Shift+N 新建独立窗口）

- **动机**：需求 B1 UI-WIN-009 要求多窗口。单进程多窗口需要重写 Tab 树/渲染/事件循环所有权，
  本轮先交付可用的进程级 MVP（明确不引入单实例锁/共享内存/IPC）。
- **实现**：新增 `crates/app/src/window.rs`；`window.new`（Category::Window，默认
  `Ctrl+Shift+N`）→ `std::env::current_exe()` + `--window` 拉起新 Tervo 进程。
  - `setsid()` 脱离父进程会话，父窗口关闭/Ctrl+C 不连带新窗口；
  - stdin/stdout/stderr 接 `/dev/null`：新进程写自己的日志文件，避免混进父进程的
    log tee 管道；
  - `spawn()` 立即返回、不阻塞 UI；后台 reaper 线程 `wait`，避免父进程长期运行时留下僵尸；
  - 只传 `--window`，不继承父进程的 `--restore/--shell/--cols` 等参数，避免子进程递归恢复/建窗。
- **日志**：父进程 `[ui] new window pid=<pid>`，失败 `[ui] new window failed: <err>`（不 panic）；
  子进程启动时 `[ui] new window process pid=<pid> (--window)`，仅用于区分。
- **入口**：TabBar 右键菜单新增 New Window（第 13 项，菜单 396px = 13×28 + 12×2 + 8 正好）；
  命令面板可搜到 New Window；`--help`/README 同步。
- **测试**：`cargo test -p tervo-app --bin tervo` 全绿（新增 4 个不依赖 GUI 的用例：
  命令注册/快捷键、spawn 命令行只带 `--window`、`--window` 不继承父参数）；
  `cargo build --release -p tervo-app` 通过。未启动 GUI（共享显示器约束）。
- **限制**：独立进程，Tab/Pane 不能跨窗口移动/拖拽；配置并发为原子写“最后写入者生效”；
  关闭确认只覆盖当前窗口；每个窗口独立日志，`--diagnose` 默认只报告当前进程。
- **手工验证（有显示器时）**：启动 release 版 → Ctrl+Shift+N（或右键 TabBar → New Window）
  → 出现第二个窗口；`ps -o pid,ppid,sid,cmd -C tervo` 可见两个不同 pid、不同 sid；
  两个窗口分别 `echo $$` 得到不同 shell pid；在 A 窗口 Ctrl+T 加 Tab，B 窗口 Tab 数不变；
  关掉 A，B 仍可输入；父进程日志有 `[ui] new window pid=`，新进程日志首部有
  `args=[..., "--window"]` 与 `[ui] new window process pid=`。

### 2026-11-05 · 可编辑快捷键（录制/冲突/重置）

- **目标**：Hotkeys 从只读列表升级为可编辑，对齐 Tabby 的单段键录制与冲突提示；
  同时把快捷键分发从“只认硬编码”扩展到“先看用户 override”。
- **配置**：`config.rs` 新增 `hotkeys: Vec<HotkeyOverride>`（`command` / `chord`），
  `#[serde(default)]` 且空表不写盘，旧 `config.toml` 无需迁移；chord 统一存小写规范格式
  （`ctrl+shift+t`、`f12`、`ctrl+=`；`+` 键写作 `ctrl++`）。
- **数据层**：`commands.rs` 提供 `Chord::parse/canonical/is_bindable`、`Shortcut::canonical`
  （保留 1–2 段解析）、`effective_shortcut`、`effective_conflicts`、`validate_override`、
  `override_command_for_chord`、`suppressed_default`、`set_override/clear_override`；
  非法键名/多主键/未知命令都返回可展示错误；普通字符与 Shift-only 组合被拒绝，避免抢终端输入。
- **设置页**：右侧列表每行 = 命令标题 + 生效 chord + `CUSTOM` 标记 + Edit/Reset；
  Edit 进入录制（`hotkey-recording-row` 高亮，状态栏提示 Esc 取消 / Backspace 恢复默认）；
  冲突时保留录制并显示红色错误，右上角显示 `✓ no conflicts` / `⚠ N conflict(s)`，
  另有 Reset All。录制走 `handle_input` 最前面的分支，所以录制中不会触发任何原快捷键或写入 PTY。
- **分发**：`handle_input` 在 F11/搜索框/硬编码快捷键之前，把当前事件转成 chord：
  命中 override → `run_command` 并 return；若按键是某命令已被改走的旧默认键 → 吞掉，
  避免“改了 New Tab，Ctrl+T 仍然开 Tab”。没有 override 时完全走原路径。
- **测试**：`cargo test -p tervo-app --bin tervo` → 108 passed / 0 failed。新增覆盖
  chord 解析/规范化（含 `+`、别名、非法输入）、key event→chord、override 查找/生效绑定/
  冲突/保存校验、旧默认键抑制、config hotkeys roundtrip/旧配置兼容、Hotkeys 行模型。
- **限制**：无 Hotkeys 搜索；多段键（`Ctrl+K Ctrl+T`）未支持录制/override；录制只有
  Ctrl/Shift/Alt（Slint `key-input` 回调未传 Meta）；小窗口下固定列宽可能被裁剪；
  未启动 GUI（与其他 agent 共用显示器，本轮约束只跑 cargo test/build），真机录屏待人工验证。
- **手工验证**：设置 → Hotkeys → New Tab 行点 Edit → 按 `Ctrl+Shift+N` → 行显示
  `Ctrl+Shift+N` + `CUSTOM`，config.toml 出现 `[[hotkeys]]`；回终端按 `Ctrl+Shift+N`
  新开 Tab、按 `Ctrl+T` 不再开 Tab；再 Edit 一个已占用组合（如 `Ctrl+W`）应显示冲突错误；
  Reset 单项/Reset All 恢复默认；Esc 取消录制后终端输入正常。
