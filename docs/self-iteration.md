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

### 2026-11-05 · Tab 活动/进度/通知（OSC 9;4 / 9 / 777 + BEL）

- **核心**：`alacritty_terminal` 0.26 的 vte `Handler` 没有 `osc_dispatch` 钩子（OSC 9/777
  在 vte 内部直接丢弃），新增 `core/osc.rs` 流式状态机，在 actor `feed` 旁路扫描：
  OSC 9;4 进度（0 清除/1 进行中/2 错误/3 不确定/4 暂停）、OSC 9 消息、OSC 777;notify、
  BEL、OSC 7 cwd；支持 BEL/ST 终止与跨 PTY chunk；非法/越界/超长（16 KiB）忽略不 panic。
  BEL 改由扫描器统一上报（避免与 alacritty `Event::Bell` 重复）。
- **应用**：`PaneRuntime` 记录 progress/attention/notification；`poll_events` 消费并按 Tab
  聚合（错误 > 进行中 > 暂停 > 不确定），TabView 显示底部进度条、百分比、错误“!”、
  完成“✓”、未读“●”；切到该 Tab 清除未读/完成。非活动 Tab 的 OSC 9/777/BEL 走
  `notify-send`（参数直传、不经 shell、清洗限长、命令缺失静默失败），配置
  `desktop_notifications`（默认 true，Settings → Terminal）。
- **测试**：core osc 12 例 + smoke `osc_progress_notification_and_bell_are_delivered`；
  `cargo test -p tervo-core` 全绿（80：38 lib + 16 mouse_report + 15 smoke + 11 VT），
  `cargo test -p tervo-app --bin tervo` 102 passed；`cargo build --release -p tervo-app` 通过。
- **未做**：桌面通知点击跳转、命令完成自动检测/静音策略、OSC 633、不确定进度动画；
  共享显示器约束下未启动 GUI，Tab 样式仅编译验证（手工步骤见报告）。

### 2026-11-05 · Vault 加密凭据 + SSH 密码 askpass（MVP）

- **问题**：SSH `auth=password` 只有名字，密码没有安全存放/自动填充路径；设置页也没有
  凭据管理入口。需求 C12/SEC-001..006、B7/UI-SET-010、B8/UI-SSH-001 都停在 not_started。
- **做法**：
  - 新增 `crates/app/src/vault.rs`：`vault.enc`（magic/version + Argon2id 参数 + salt +
    nonce + ChaCha20-Poly1305 密文，头部作 AAD）；`initialize/unlock/lock/change_master/
    set/get/remove/clear`，锁定时读写报错；文件 0600、临时文件 + rename 原子写；解析器对
    版本/字段/参数范围/长度/base64 做严格校验，坏文件原样保留。
  - 新增 `crates/app/src/askpass.rs`：0700 临时目录 + 0700 脚本，
    `SSH_ASKPASS_REQUIRE=force`，密码只进 ssh 子进程环境；guard Drop 清目录/尽力清内存。
  - Profile 增加 `save_password: bool`（默认 false，无 password 字段）；表单 `auth=password`
    显示 masked 密码 + 保存勾选；保存路径先校验 Vault 解锁，再写 Profile/Vault；取消勾选
    时解锁则同步删除旧凭据。
  - 设置左侧导航新增 Vault 页（状态/路径、Set/Change、Unlock、Lock、Clear 二次确认、
    错误/提示）；启动时默认锁定；连接已保存密码但 Vault 锁定时跳到该页提示解锁。
  - 文档：新增 [`VAULT.md`](VAULT.md)，更新 FEATURE_MATRIX 的 SEC-001..006、UI-SET-010、
    UI-SSH-001 与 B7/C12 计数。
- **测试**：`cargo test -p tervo-app --bin tervo` → **108 passed**。新增 vault 7 例
  （roundtrip/锁定读写、错密码+损坏/截断/空文件、无明文+0600、改主密码、拒覆盖+Clear、
  恶意 KDF 参数上限、TERVO_VAULT 覆盖）、askpass 5 例（密码 prompt 原样输出/非密码 prompt 回答 no/force 环境/Debug 脱敏与 0700/真实 ssh+ssh-keygen 的 known_hosts 判定）、
  Profile 序列化无明文密码 1 例。全部使用临时目录，不碰真实 `$HOME`（`TERVO_VAULT` 路径用例走纯函数，不改进程环境）。
- **踩坑（重要）**：最初脚本对任何 askpass prompt 都输出密码。本地用 paramiko 测试服务 +
  `SSH_ASKPASS_REQUIRE=force` 做无 GUI 探针时，OpenSSH 把密码当成 host key 确认的
  yes/no 答案，3 秒内调用 askpass **5044 次**并卡死。修复：
  - 脚本识别 prompt：只有含 `password`/`密码` 才输出密码，其余（host key 确认等）回答 `no`，
    ssh 快速失败（实测 rc=255，无循环）；
  - 连接前用 `ssh -G` 取实际 `stricthostkeychecking` 与 known_hosts 路径，再用
    `ssh-keygen -F` 判断 host key 是否已确认；未确认则本次不用 askpass，交给终端显示指纹/
    手输一次密码，known_hosts 命中后下次才自动填充；`accept-new`/`no` 不弹确认，可直接 askpass。
  - 仍然存在的边界：动态 `KnownHostsCommand`、host 在 known_hosts 但服务器换了 key 类型时
    可能误判；此时脚本的 `no` 兜底保证快速失败而不是死循环/泄露密码。
- **安全限制（已写入 docs/VAULT.md）**：
  - askpass 密码仍经子进程环境变量传递，同用户可经 `/proc/<ssh-pid>/environ` 观察；
    后续应改继承 fd/pipe。注意 portable-pty 的 `pre_exec` 会关闭所有 `fd > 2`，需要
    fd 白名单或 Unix socket 方案。
  - 解锁后的主密码/密码在进程内存，未 mlock/完整 zeroize；无空闲自动锁定；无 Keyring；
    锁定时取消勾选无法立即删除旧密文（Clear 或下次解锁后处理）。
  - 私钥口令/其他 secret 未接 Vault；known_hosts 仍交系统 ssh。
- **未做**：GUI 真机走查（共享显示器约束，未启动 GUI/xkey/xclick/xwin）；原生 russh；
  解锁弹窗；凭据列表 UI；Keyring。`cargo build --release -p tervo-app` 通过（见本次提交说明）。

### 2026-11-05 · Profile 选择器改为独立图标（对齐 Tabby）

- 用户指出 Tabby 的 Profile 不在 “+” 下拉里，而是单独入口。调整：
  - “+” 恢复为**直接新建默认 Tab**（不再弹菜单）；
  - 旁边新增独立 `>_` 图标，打开 Profile 菜单，菜单只包含已保存 Profile + `Manage Profiles…`；
  - `newtab-*` 内部命名改为 `profilemenu-*`，pick 下标不再有“默认 Tab”偏移；
  - 点外部/Esc 关闭、菜单跟随图标定位保持。
- 真机：+ → tabs=2、无菜单；图标 → `[ui] profile menu profiles=2`；选 local-a →
  `[ui] profile pick=0` 并进入 shell；截图 .logs/profile-menu.png / profile-menu-picked.png。
- 旧记录中“+ 下拉”的描述已被本条取代。

### 2026-11-05 · Profiles UX：搜索 / 默认 Profile / 复制（Tabby 对齐 MVP）

- **问题**：Profiles 管理面板只能全量列出，Profile 多时难找；没有“新 Tab 默认 Profile”，
  `+`/Ctrl+T/命令 `tab.new` 永远起系统 shell；复制只能手工重建；面板 420x320 加搜索/
  更多操作后空间不够。FEATURE_MATRIX UI-PROF-002/005/006 等仍标 not_started。
- **做法**：
  - 搜索/过滤：`profile.rs::filter_indices(profiles, query)` 纯函数，按 `name`/`host`/`kind`
    大小写不敏感子串匹配，空/全空白查询返回全部下标。面板加搜索框（输入即过滤、×/Esc
    清空），Rust 侧 `profile_indices` 保存“可见行 → store 下标”，点击/编辑/删除/复制/
    星标都经该映射，过滤后仍打开正确 Profile。
  - 默认 Profile：`Config` 新增 `default_profile_id: u64`（默认 0，旧配置 serde default
    自动补 0）；面板每行行首 `★/☆`，点击 `on_profile_set_default` 写 config（再点取消）；
    `new_tab_with_default` 用于 `+`、Ctrl+T、命令 `tab.new`、TabBar New Tab：ID 有效时
    调 `open_profile_tab`，0/失效 ID 回退 `new_tab_inner`（系统 shell）；删除默认 Profile
    时 `clear_default_after_remove` 回退 0 并写盘。启动首 Tab/会话恢复保持原行为，避免
    启动即连 SSH。
  - 复制：`ProfileStore::duplicate` 全量克隆字段、`duplicate_name` 生成
    `原名 (copy 1/2/…)` 去重、`add` 分配新 ID；`on_profile_duplicate` 保存 profiles.toml。
    面板行内 `Dup` 按钮。
  - 布局：面板 420x320 → 420x404，搜索框/可滚动列表（Flickable，6 行视口）/星标/
    Dup/编辑/删除/底部默认提示；472 高窗口内上下留 34px，不溢出；关闭面板时把焦点还给
    终端 IME 宿主（避免搜索框吃掉按键）。
  - 文档：更新 FEATURE_MATRIX 的 UI-PROF-001..006/010/013、UI-NAV-004、UI-TAB-001/002
    与 B/B+C 计数。
- **测试**：`CARGO_HOME=.../.cargo CARGO_TARGET_DIR=$PWD/target cargo test -p tervo-app
  --bin tervo` → **139 passed, 0 failed**（首次完整编译 1m00s）。新增/改写：
  - `filter_matches_name_host_kind_case_insensitively`：name/host/kind、大小写不敏感、
    空查询全量、无命中；
  - `default_index_resolves_and_falls_back`：0/失效 ID 回退、删除默认项后解析失败；
  - `deleting_default_profile_falls_back_to_shell`（config 回退 0）；
  - `duplicate_assigns_new_id_and_dedupes_copy_names`（新 ID、`(copy N)`、字段复制、
    profiles.toml 落盘读回）、`duplicate_name_helper_handles_existing_copies`、
    `duplicate_missing_profile_is_rejected`；
  - `config::default_profile_id_defaults_to_zero_and_survives_roundtrip` 与既有
    config roundtrip/missing-fields 断言。
- **限制/未做**：共享显示器约束下**未启动 GUI/xkey/xclick/xwin**，面板布局与新交互仅
  编译 + 单测验证，未做截图/e2e；Profile 选择器（`>_` 菜单）本身仍无搜索/键盘导航；
  无 Profile Groups/拖拽排序；启动首 Tab 不套用默认 Profile；删除仍无二次确认。
- **手工验证步骤（合并后）**：
  1. 用 `TERVO_CONFIG=/tmp/t/config.toml TERVO_PROFILES=/tmp/t/profiles.toml` 启动，建
     2 个 Local + 1 个 SSH Profile。
  2. 打开 Profiles 面板：输入 `SSH`/host 片段/名字大小写变体 → 列表即时缩小；点击结果
     打开对应 Profile 的新 Tab；点 × 或 Esc → 恢复完整列表。
  3. 点某行星标 → 行首变 ★、config.toml 出现 `default_profile_id = <id>`；关闭面板后
     点 `+`/Ctrl+T/命令面板 `tab.new` → 新 Tab 用该 Profile；删除该 Profile → config
     回退 `0`，再 `+` 起系统 shell；把 config 手改成不存在的 ID → 同样回退 shell。
  4. 对一条 Profile 点 `Dup` → 出现 `name (copy 1)`；再点 → `(copy 2)`；重启后仍在
     profiles.toml 中。
  5. 窗口 540x472 下检查面板不溢出、列表超过 6 行可滚动、搜索框关闭后终端按键恢复。

### 2026-11-05 · Tabby hotkeys/主题导入（SESS-009 / UI-SET-005）

- **问题**：`--import-tabby` 只迁移 Profiles + `terminal.fontSize`，Tabby 用户的改键与
  终端配色仍要手抄；SESS-009 / UI-SET-005 因此停在 partial。
- **上游依据（不靠猜）**：sparse clone `Eugeny/tabby` master
  commit `467ce8e`（2026-10-06，`tabby-core` 1.0.231-nightly.0，配置 `version: 1`，
  `LATEST_VERSION = 1`）：
  - `hotkeys` 为 command id → `string | string[] | (string|string[])[]`；`profile` /
    `profile-selectors` / `group-selectors` 是对象分支，运行时由
    `getHotkeysConfigRecursive` 拼成 `profile.<name>` 等动态 id
    （`tabby-core/src/services/hotkeys.service.ts`）；默认键见
    `tabby-core/src/configDefaults.{linux,macos,windows}.yaml` 与 `tabby-terminal/src/config.ts`；
    command id 来自 `tabby-core/src/hotkeys.ts`、`tabby-terminal/src/hotkeys.ts`、
    `tabby-local/src/hotkeys.ts`（`new-tab`）、`tabby-settings/src/hotkeys.ts`（`settings`）。
  - `terminal.colorScheme` / `lightColorScheme` 存**完整对象**；`customColorSchemes` 是数组；
    `TerminalColorScheme = { name, foreground, background, cursor, colors[16],
    selection?, selectionForeground?, cursorAccent? }`
    （`tabby-core/src/api/theme.ts`、`tabby-terminal/src/config.ts`）。
- **做法**：
  - `tabby_import.rs` 的 `import_yaml` 增加 `&mut Config` 与 themes_dir；`ImportReport`
    增加 hotkeys/themes 的新增、未变、未映射、跳过、重命名字段。
  - 21 条语义明确映射（`TABBY_HOTKEY_MAP`）：`new-tab`/`close-tab`/`next-tab`/`previous-tab`/
    `duplicate-tab`/`reopen-tab`、`split-right`/`split-bottom`/`close-pane`/`pane-nav-next`、
    `copy`/`paste`/`search`/`clear`/`select-all`、`command-selector`/`toggle-fullscreen`/
    `zoom-in`/`zoom-out`/`reset-zoom`/`settings`。
  - chord：兼容 `-` 与 macOS 默认里混入的 `+` 分隔；`Ctrl/Shift/Alt/Super/⌘/⌥/Win` 别名；
    `Ctrl--` 字面减号；统一走 `Chord::parse` + `canonical()`。只收可绑定单段键；
    冲突/非法/空绑定/多段键跳过并记录原因；已有自定义绑定不覆盖；一个命令多个候选键只取
    第一个可用者，其余计数忽略。
  - 主题：校验 name/fg/bg/cursor/16 色，`UiColors::derive` 生成 11 个 UI 色后写出完整
    Tervo TOML；同内容去重、同名文件加 `-2` 后缀、绝不覆盖；坏色值/缺字段/非 16 色跳过。
  - CLI 与设置页均显示新增/未映射/跳过数量及首个原因；设置页导入后重载 `ThemeRegistry`
    并刷新 hotkey 列表。
- **测试**：`cargo test -p tervo-app --bin tervo` → **139 passed**。`tabby_import.rs` 共
  11 例（新增 7 例：映射表一致性、chord 归一化、映射+未映射+非法、冲突+空绑定+多段键、
  主题落盘+重名后缀、坏色值、短 hex/深浅 variant），全部使用临时目录；生成的主题再用
  `ThemeRegistry::load` 回读验证。
- **限制**：hotkeys 只覆盖语义明确子集；每个命令只能一条单段 override，Tabby 的
  multi-chord 与 `[]`（显式取消默认键）无法表达；主题只取 16 色 + fg/bg/cursor，忽略
  selection/selectionForeground/cursorAccent，不支持 named/rgba 颜色；不导入插件配置、
  UI 主题 CSS、密码；受共享显示器约束未启动 GUI 真机走查。schema 依据上述 commit，
  Tabby 后续版本需重新核对。

### 2026-11-05 · 外部进程插件 API MVP（命令注册/执行）

- **问题**：`plugin.rs` 原来只扫描扁平 `~/.config/tervo/plugins/*.toml` 并展示，不加载/
  不执行；命令面板只有静态 `commands::builtin()`；需求 C13/PLUG-001..005、B11/UI-PLUG 停在
  not_started/partial。
- **做法**（不替换旧格式，新增 v1 目录格式可执行）：
  - `plugin.rs` 同时扫描旧扁平 `*.toml`（仍只展示、校验、统计）与
    `<plugins>/<name>/plugin.toml`（v1：name/version/api=1/entry/enabled/description/commands）。
  - v1 校验：重名插件跳过、命令 id 插件内唯一、api≠1 拒绝、entry 拒绝绝对路径/`..`/
    符号链接逃逸且必须存在；command key 必须是单个可绑定组合。
  - 命令注册扩展点 `plugin::register_plugins(&mut Registry, &PluginStore)`：启用插件的命令
    以 `plugin.<name>.<id>`、标题 `(plugin: <name>)`、分类 `Plugins` 进入命令表；Palette
    可搜/可执行，Hotkeys 页可见并支持 override；默认 key 在内置硬编码键之后分发。
  - 执行走 `run_command` 的 unknown-id 分支 → `PluginStore::spawn_command` → 后台线程
    spawn entry（cwd=插件目录，env `TERVO_PLUGIN_NAME/TERVO_PLUGIN_API`）；stdin 写一行
    JSON `{"event":"command","id":"...","args":[]}` 后关闭；并发捕获 stdout/stderr（各
    64 KiB）、10s 超时 kill，退出码/超时/输出只写 `[plugin]` 日志。UI 线程不等待。
  - `enabled=false` 不注册命令、`spawn_command` 拒绝；设置页沿用模型显示
    名称/版本/状态/api/命令数/说明，并更新底部说明为真实行为（含无沙箱警告）。
- **测试**：`cargo test -p tervo-app --bin tervo` → **146 passed**（新增 14 例：旧+新共存、
  v1 解析/字段/api/非法 entry/符号链接/重名/命令 id 重复/非法 key、命名空间注册与禁用、
  外部 shell 脚本的 env/stdin JSON/stdout/stderr/cwd/退出码/超时 kill/spawn 错误）。
- **文档**：新增 [`PLUGIN_API.md`](PLUGIN_API.md)（目录/清单/协议/权限/限制/手工验证）；
  更新 FEATURE_MATRIX UI-PLUG-001..007、PLUG-001..006、SEC-005 与 B11/C13 计数。
- **限制**：不做 Tabby/JS/WASM 兼容；无结果回传 UI、无常驻生命周期/事件/安装卸载/升级/
  热重载；无权限模型/沙箱（插件=当前用户普通子进程）；超时固定 10s；未启动 GUI
  （共享显示器约束），设置页与真机执行待人工走查。
- **手工验证**：按 `docs/PLUGIN_API.md` §6 建 `plugins/hello/{plugin.toml,run.sh}`，
  启动后看 `[plugin]` 注册日志与设置页；Palette 执行 → stdout/stderr/退出码日志；
  `exec sleep 30` → ~10s 超时 kill；`enabled=false` 重启后命令消失、设置页 disabled。

### 2026-11-05 · 修复 Vault 密码 SSH 自动登录的 known_hosts 判定

- **问题**：`auth=password` + `save_password` 时，host key 已在 known_hosts 中却仍记录
  “主机密钥未在 known_hosts 中确认：本次不启用 askpass”，每次都要手动确认/输入密码。
- **根因**：`askpass::host_key_known` 的 `ssh-keygen -F` 查找键与 OpenSSH 实际匹配规则不一致：
  1. 没有使用 `ssh -G` 的 `hostname`：`~/.ssh/config` 里 `Host alias` + `HostName 127.0.0.1`
     时，ssh 用解析后的 `127.0.0.1` 查 known_hosts，旧实现却用别名 `alias` 查；
  2. `HostKeyAlias` 被拼成 `[alias]:port`，OpenSSH 实际用别名原文查表（已用 ssh 8.9 验证）；
  3. 没有采用 `ssh -G` 的 `port`（虽然 `-p` 显式传入时通常一致）。
  实测：`ssh-keygen -F '[vaultssh-alias]:2222'` miss、`-F '[127.0.0.1]:2222'` hit；
  `-F '[myalias]:2222'` miss、`-F 'myalias'` hit。
- **修复**：`host_key_known` 解析 `ssh -G` 的 `hostname`/`port`/`hostkeyalias`：
  默认用 effective hostname + 端口构造 `host`/`[host]:port`；有 `HostKeyAlias` 时使用别名
  原文；strict/known_hosts 路径解析与“未知 host 不使用 askpass”的保守策略不变。
  未命中时日志打印 lookup 键、strict 与候选 known_hosts 路径（不含密码），便于判断条目是否
  写到了应用进程看不到的位置。
- **测试**：`cargo test -p tervo-app --bin tervo` → **136 passed**。新增 4 例：
  `host_key_known_matches_nonstandard_port_entry`（Profile 级 `[127.0.0.1]:2222` 命中，
  端口/主机不同必须 miss）、`ssh_keygen_matches_bracketed_port_spec`（真实 ssh-keygen
  匹配 `[127.0.0.1]:2222`，且端口/主机不匹配时必须 miss）、
  `host_key_known_uses_resolved_hostname_from_ssh_config`
  （临时 ssh_config 的 Host/HostName 别名）、`host_key_known_uses_host_key_alias_verbatim`
  （别名原文，不附加端口）；无 ssh/ssh-keygen 的环境自动 skip。
  无 GUI 真实验证：paramiko 测试服务（`--password s3cret`）+ known_hosts 条目，直接以
  `ssh` + `SSH_ASKPASS_REQUIRE=force` 登录成功（服务端 `password auth ok`，终端打印
  `SSH-TEST-OK`）；未知 host 只触发一次 host key 确认、rc=255，无 askpass 密码泄露/重试；
  askpass 日志与输出均无密码。
- **限制**：GUI 仍受共享显示器约束未做 password 链路走查；`KnownHostsCommand` 等动态配置、
  known_hosts 有 host 但服务器换 key 类型仍可能误判（askpass 的 `no` 兜底保证快速失败）；
  `ssh-keygen` 不在 PATH 时保守返回 false。若 Profile Host 与 known_hosts 键完全一致
  （如 `127.0.0.1` + `[127.0.0.1]:2222`），旧实现本身也能命中；本次修复覆盖的是此前漏判的
  ssh 配置别名/HostKeyAlias/有效端口形态，并新增端口键的真实 `ssh-keygen -F` 回归测试。

### 2026-11-05 · 插件事件钩子与「最近插件活动」

- **问题**：外部进程插件 API 只有命令触发，没有生命周期事件；命令结果只写日志，
  设置页看不到最近输出；需求 C13/PLUG-001、PLUG-003 与 B11/UI-PLUG-005 的
  “事件订阅/诊断信息”仍停在 partial。
- **做法**（保持旧扁平清单与 api=1 命令协议不变）：
  - `PackageManifest` 新增可选 `events: Vec<String>`；只接受
    `startup` / `tab-open` / `tab-close`，重复/未知值让整份清单进 `skipped`；
    旧扁平清单不读该字段，仍只展示。
  - `PluginEvent` 枚举统一字符串与解析；`PluginStore::event_targets` 只返回
    `enabled` 且声明了该事件的插件；`spawn_event` 每个插件一个后台线程，
    返回已提交数，spawn 后立即返回，UI 不等待。
  - stdin 协议统一为单行 JSON：命令 `{"event":"command","id","args"}`；
    事件 `{"event":"startup|tab-open|tab-close","payload":{...}}`。
  - payload 只带非敏感字段：`startup={}`；`tab-open={title,kind}`；
    `tab-close={title}`。标题去控制字符/空白并截断 200 字符，kind 只允许短 ASCII
    标识（异常回退 `local`）；不含 cwd/命令行/环境/凭据。
  - 事件超时与命令一致（10s `EVENT_TIMEOUT`），失败/超时/spawn 失败只写
    `[plugin]` 日志与活动列表，不影响命令/窗口主流程；禁用插件不触发。
  - 主程序接线：应用状态就绪后、首个 Tab 创建前发 `startup`；本地/Profile/恢复/复制
    Tab 打开后发 `tab-open`（含 kind）；关闭 Tab、关闭最后一个 Pane、Close Others 时
    发 `tab-close`。最后 Tab 关闭会退出事件循环，该次事件是尽力而为。
  - 新活动日志 `ActivityLog`（`Arc<Mutex<_>>`，容量 8，revision 递增）：
    记录时间/插件/`command <id>` 或 `event <name>`/`exit=<code>`|`timeout`|
    `signal`|`spawn-error`/输出首行；摘要取 stdout 首个非空行，否则 stderr，
    复用 `activity::sanitize_notification_text` 去控制字符并截断 80 字符。
  - Slint Plugins 页拆成 Installed + Recent plugin activity 两个列表（各 80/108px
    可滚动）；`poll_events`（400ms）只在 revision 变化时 `set_vec` 刷新，
    后台线程不直接碰 Slint model。完整 stdout/stderr 仍只写日志。
- **测试**：`cargo test -p tervo-app --bin tervo` → **172 passed**（新增 8 例）：
  `parses_event_subscriptions_and_rejects_unknown_or_duplicate`（事件清单/未知/重复）、
  `event_payloads_carry_only_title_and_kind`（payload JSON/单行/截断/无敏感字段）、
  `event_targets_respect_declaration_and_enabled`（未声明/禁用不触发）、
  `spawn_event_sends_json_payload_to_declared_plugins_only`（临时 shell 脚本落盘验证
  三种事件的 stdin JSON，未声明/禁用插件无文件）、
  `execute_event_sends_event_json_and_reports_failure`（事件 stdin/非零退出/超时 kill）、
  `event_failure_updates_activity_without_blocking`（失败只进活动）、
  `activity_log_keeps_latest_and_sanitizes_summary`（容量/顺序/控制字符/截断/
  stderr 回退/超时/signal/spawn-error）、
  `spawn_command_records_activity_and_spawn_error`（命令成功与 entry 丢失）。
  未启动 GUI。
- **文档**：更新 [`PLUGIN_API.md`](PLUGIN_API.md)（events 字段/事件协议/payload/
  最近活动/限制/手工验证）、FEATURE_MATRIX UI-PLUG-001/002/003/005/007 与
  PLUG-001/002/003/005（测试数 172）。
- **限制**：只有三个固定生命周期事件，无任意事件订阅/取消/去重/合并，也没有
  pane 输出、命令完成等事件；事件无返回值，不能修改 Tab/终端状态；活动列表仅
  最近 8 条内存摘要（重启清空、不可点击/复制，完整输出仍看日志）；关闭最后一个
  Tab/窗口时 `tab-close` 尽力而为；同一插件并发触发不设上限；无持久日志面板；
  未启动 GUI（共享显示器约束），设置页布局与事件真机走查待人工验证；未新增依赖。
- **手工验证**：按 `docs/PLUGIN_API.md` §6 建带 `events` 的 `plugins/hello/`，
  启动后看 `events.log` 的 startup/tab-open/tab-close JSON，新建/关闭 Tab，
  在设置 → Plugins 看最近活动行；`exec sleep 30` 验证 timeout；`enabled=false`
  重启后事件与命令都不再触发。

### 2026-11-05 · 窗口背景图 + 终端背景透明度（对齐 Tabby Appearance）

- **问题**：`THEME-006`、`UI-SET-002` 只有主题配色，没有背景图/透明度；`config.rs` 也没有
  对应字段。Tabby 的 Appearance 面板依赖背景图 + 终端不透明度，而 Tervo 的终端帧是软件
  光栅化的 RGBA 缓冲，具备直接写 alpha 并用 Slint 混合的条件。
- **做法**：
  - `config.rs` 新增 `background_image: String`（默认空=关；`~/` 用 `$HOME` 展开；只接受
    存在的普通文件）与 `background_opacity: f32`（默认 1.0，clamp 0.2–1.0，读入/写盘都
    四舍五入到两位小数；手写 TOML 的整数 `= 1` 也接受）。旧配置缺字段走 serde default，
    不升 schema（纯增量字段）。
  - `render.rs` 新增纯函数 `background_alpha`/`default_background`：默认背景 RGB 不变、
    alpha=opacity；`Renderer::new/set_theme/set_opacity` 都保留透明度。cell 背景改按 RGB
    比较，避免“默认背景带 alpha 后整屏 cell 背景被重填为不透明”；文字/光标/彩色 cell
    背景仍写 255，保证可读。RGBA 缓冲继续用 `Image::from_rgba8`（直通 alpha），由 Slint
    与背景图混合。
  - `main.slint` 在窗口根 `Rectangle` 最底层放 `Image { image-fit: cover }`；有有效背景图
    时内容区与 pane 背景透明（否则保持 `Palette.colors.bg`），TabBar/设置面板保持不透明。
    Appearance 页新增 Background image 路径输入（空=关闭、× 清除、非法红字）与自绘
    Opacity 滑块（20%–100%）。
  - `main.rs` 启动时校验/解码配置里的背景图（坏路径回退纯色并保留错误文案）；设置回调
    即时应用：合法路径写盘、非法路径只提示并保留上一背景；滑块拖动只预览重绘、松手才
    写配置，避免每帧原子写。
- **测试**：`cargo test -p tervo-app --bin tervo` → **171 passed**（新增 7 例：opacity
  clamp/两位小数、背景字段 roundtrip、旧配置默认、路径校验、`~` 展开、alpha 纯函数、
  默认背景 alpha 与彩色 cell 背景不透明；不启动 GUI）。`scripts/matrix_stats.py` 复核
  THEME-006 升为 partial 后为 37/99/14/71/7。
- **限制**：无模糊、无桌面透传（窗口自身仍不透明，所以 opacity 只在与背景图叠加时可见）；
  无每主题独立背景；背景固定 cover（无 contain/fill 选项）；未真机走查 GPU/软件后端下
  Slint 的 alpha 混合差异；只有默认背景透明，文字/光标/彩色 cell 背景保持不透明；大图
  同步解码可能短暂卡顿；图片经 Slint 缓存，同路径换文件需重启或改路径；相对路径按进程
  cwd 解析。
- **手工验证建议**（共享显示器约束下本轮未执行）：设置 → Appearance 输入一张 PNG 绝对
  路径看背景图铺满、文字仍清晰；拖 Opacity 看终端背景透出图片且松手后 config.toml
  出现 `background_opacity = 0.xx`；输入不存在路径看红字且背景不消失；把路径清空恢复
  纯色。

### 2026-11-05 · SFTP 传输进度与取消

- **问题**：SFTP 面板的 Upload/Download 是同步 `sftp` 调用，大文件/目录期间 UI 卡住，
  没有当前文件/进度/取消；FEATURE_MATRIX 的 UI-SSH-005、C4/SFTP-001..007 多数仍标
  not_started（与已有的面板/文件操作实现不符）。
- **做法**：
  - `sftp.rs` 新增后台 `TransferJob`：`sftp -q -b -` + batch 脚本（首行 `progress`）；
    下载统一 `get -R`（普通文件也已实测安全），上传目录 `put -R`、单文件 `put`；
    stdout/stderr 各一个读线程，按 `\r`/`\n` 切行。
  - 纯函数 `parse_progress_line` / `parse_progress_output`：从右往左找 `NN%`，
    解析 `path 42% 1.2MB 3.4MB/s [ETA]`（支持文件名含空格、0/100%、KB/MB/GB）；
    `TransferState` 状态机（Idle/Running/Done/Failed/Cancelled）负责 UI 文案/百分比。
  - 取消：`Arc<Mutex<Child>>` + `AtomicBool`；UI 线程 `cancel()` 只置标志并（Unix）
    kill，不等待；worker 每 40ms `try_wait` 回收，`Drop` 兜底 terminate+join；
    `sftp_transfer` 单槽位保证同一时间只有一个传输，传输中其余 SFTP 操作在 Rust 层拒绝。
  - UI：传输区显示当前文件 + 进度条/百分比（无进度行时“不确定进度”）+ Cancel；
    传输中整面板加 shield，完成/失败/取消后自动恢复；400ms `poll_events` 非阻塞收事件；
    上传成功后刷新列表并保留结果文案。
- **测试**：`cargo test -p tervo-app --bin tervo` → **177 passed**（新增 13 例：
  进度解析 5、`\r`/`\n` 切行 1、状态机 3、batch 脚本 1、临时 shell 脚本
  取消 kill/成功/stderr 失败 3）。无 GUI 真实验证：`scripts/ssh_test_server.py`
  （paramiko SFTP）+ `tervo --sftp-probe` → list、单文件 get/put、目录 get/put 均通过，
  1MiB 下载与源文件 `cmp` 字节一致；另用 Python pty 对照确认 OpenSSH 进度行的真实格式与
  “非 tty 不输出”的行为。
- **限制**：OpenSSH 的 `progress` 表在 stdout 不是 tty（本应用是管道）时**不输出逐文件
  进度**，实测管道下 stdout/stderr 都没有进度行，所以实际面板通常显示“不确定进度”，
  不是字节级百分比；无总大小/ETA/失败重试/断点续传；取消可能留下部分本地或远端文件；
  Windows 取消只记录标志不 kill；UI 进度刷新依赖 400ms 轮询；共享显示器约束下未启动
  GUI 做真机截图走查。
- **文档**：FEATURE_MATRIX UI-SSH-003/004/005 与 C4/SFTP-001..007 按实际实现改写，
  B8/C4 分节计数和总表（37/106/14/64/7、weighted 41.0%）同步。

### 2026-11-05 · 修复 Vault 密码 SSH 自动登录的 askpass 链路（GUI 真机验证）

- **问题**：Profile `auth=password` + `save_password`、Vault 已解锁并有密码，GUI 连接
  仍失败：服务端 `password auth ok` 次数 0，pane 显示
  `client_loop: send disconnect: Broken pipe`，应用日志只有 `[ui] profile pick=0` /
  `[profile] ssh 命令`，没有任何 askpass 启用/失败记录。
- **根因（三处叠加）**：
  1. **公钥认证抢在密码前**：`Profile::ssh_command()` 对 `auth=password` 没有限制认证方式，
     ssh 默认先试 agent/密钥。测试服务对 `user=test` 接受任意公钥（key 认证测试需要），
     本机 agent 又有 key，于是认证以 publickey 成功，密码 prompt 根本不出现，
     Vault/askpass 自然从未被调用；服务端也不会记录 `password auth ok`。
  2. **测试服务 PTY 回调签名错误**：`scripts/ssh_test_server.py` 的
     `check_channel_pty_request(self, term, width, height, pw, ph, modes)` 与 paramiko 2.x
     的 `(self, channel, term, width, height, pixelwidth, pixelheight, modes)` 不一致。
     任何请求 PTY 的连接（Tervo 在 PTY 里跑 ssh）即使认证成功也会在 shell 请求前抛
     `TypeError` 断链，客户端表现为 `client_loop: send disconnect: Broken pipe`。
     它把真正的认证结果掩盖了，看起来像密码失败。
  3. **成功路径没有诊断**：askpass 创建/注入成功时没有任何日志，脚本也不留调用痕迹，
     无法区分“没启用 askpass”“脚本没被调用”“prompt 没识别”“env 没传到”。
- **修复**：
  - `Profile::ssh_command()`：`auth=password` 时注入
    `-o PubkeyAuthentication=no -o PreferredAuthentications=password,keyboard-interactive`
    （kbd-int 在 password 之后兜底 PAM 服务器；agent/key 不变）。GUI 日志会打印完整 ssh 命令。
  - 诊断（只记布尔/长度，不记密码）：`[vault] 已取出 Profile <id> 的密码（len=n）`、
    `[askpass] 已启用 profile=<id> require=force debug=<bool>`、
    `[askpass] 注入 ssh 子进程环境: keys=[...] secret_set=true`。
  - 新增 opt-in `TERVO_ASKPASS_DEBUG=<file>`：askpass 脚本每次被调用追加
    `match=password|no prompt=...`（**绝不写密码**），用于确认调用与 prompt 识别。
  - `ssh_test_server.py` 修正 PTY 回调签名（补 `channel`/`pixelwidth`/`pixelheight`），
    并记录 `pty request WxH`；PTY 连接现在能进 shell。另对“未知 host/超时在握手前断开”
    容错：`start_server` 的 `EOFError/OSError` 只关闭当前 transport，不再让测试服务进程
    整体退出（否则一次未知 host 探针就会杀死后续 e2e 的服务端）。
- **测试**：`cargo test -p tervo-app --bin tervo` → **168 passed**。新增 4 例：
  `vault::tests::credential_is_keyed_by_profile_store_id`（ProfileStore 分配 id → vault.set
  → 用 store 中 Profile.id 取回；两 Profile 不串、未保存 id NotFound、重启后仍可取）、
  `profile::tests::password_auth_ssh_command_is_password_only`（password 注入两种 -o，
  agent/key 不注入）、`askpass::tests::applied_envs_reach_real_child_process`
  （apply 后真 spawn 子进程验证 3 个环境变量与脚本存在）、
  `askpass::tests::debug_file_records_prompt_without_secret`
  （诊断文件记录 prompt/匹配分支且不含密码）。
- **真机 GUI 验证**（独立 `TERVO_CONFIG/TERVO_VAULT/TERVO_PROFILES/TERVO_LOG_DIR`，
  窗口移到空白区 3400,800，60x20=540x472，只 kill 自己 PID；paramiko 服务
  `--password s3cret` @127.0.0.1:2222，known_hosts 由 Profile `extra_args` 指定）：
  GUI 设置 Vault 主密码 → Profile 表单保存密码 → 关闭设置 → Profile 图标 → `SSH · vaultssh`
  行连接。应用日志出现 `[ui] profile pick=0`、`[vault] ... len=6`、`[askpass] 已启用 ...
  require=force debug=true`、`[askpass] 注入 ... secret_set=true`；askpass 调试文件
  `match=password prompt=test@127.0.0.1's password:`；服务端
  `[ssh-test] password auth ok` + `[ssh-test] pty request 60x20 term=b'xterm-256color'`；
  pane 出现 `SSH-TEST-OK` / `remote$ `。app 日志、askpass 调试文件、profiles.toml、
  vault.enc 均无明文密码，vault.enc 0600。
- **无 GUI PTY 探针**：`script -qec` 里用与 app 相同的 ssh 参数 + askpass env，真实 ssh
  登录成功（`SSH-TEST-OK`，服务端 `password auth ok`）；未知 host 仍只回答 `no`、rc=255。
  旧文档探针用 `</dev/null`，stdin 非 tty 会“碰巧”走 askpass，覆盖不到 GUI 的 force 路径，
  已在 `docs/VAULT.md` §5 改为 PTY 探针。
- **限制**：系统 OpenSSH < 8.4 不支持 `SSH_ASKPASS_REQUIRE=force`；askpass 只识别含
  `password`/`密码` 的 prompt（其它语言/keyboard-interactive 自定义问题仍可能失败）；
  `TERVO_ASKPASS_DEBUG` 会记录服务器 prompt 原文（无密码）仅供排查；密码仍经 ssh 子进程
  环境变量传递（`/proc/<pid>/environ` 可观察）；首次连接仍回退终端确认 host key。
  `auth=password` 现在明确不回退公钥（需要公钥请改 Profile Auth）。

### 2026-11-05 · 自适应窗口圆角（X11 SHAPE）

- **目标**：`no-frame` 主窗口去掉系统装饰后四角是直角；给普通窗口加 12px 自适应圆角，
  最大化/全屏保持方角（避免桌面缝隙）；多窗口实例各自独立、互不串窗。
- **实现**：`crates/app/Cargo.toml` 的 x11rb 增加 `shape` feature（无新增 crate）。
  `xconn.rs` 新增 `CornerShaper` 后台线程 + `apply_rounded_corners`：
  复用 `find_own_window` 按 `_NET_WM_PID` 找到本进程主窗口（启动时 100ms×50 重试，
  多实例不会改到别人的窗口），建 depth=1 Pixmap 并整体填白，再按 4 个角逐行涂黑色
  矩形抠出半径 r 的 1/4 圆，`shape::mask(SO::SET, SK::BOUNDING/CLIP, ...)`，再用
  `shape::rectangles(SK::INPUT)` 把输入区域保持为整窗矩形（避免 BOUNDING 顺带改变
  四角拖拽/按钮命中），flush 后释放 pixmap/GC。任何失败只打 `[corners]` 日志，
  不影响窗口/终端（不 panic）。
- **半径**：`corner_radius(w,h,max,full) = if max||full { 0 } else { min(12, w/2, h/2) }`，
  w/h 是 **X11 物理像素**。Slint `resized` 回调给逻辑像素，`CornerRequest` 带
  `ui.window().scale_factor()`；worker 优先 `get_geometry` 取物理尺寸（X11 窗口没有
  逻辑像素），`physical_extent` 的“逻辑 × scale”只作 geometry 查询失败兜底与日志
  （1x：540→540；2x：540→1080；非法 scale 回退 1.0）。
- **状态**：resize 请求查 `_NET_WM_STATE` 的 `MAXIMIZED_HORZ`+`VERT` / `FULLSCREEN`；
  最大化/全屏按钮、F11、`--fullscreen` 额外传“刚设置的权威状态”，不等 WM 属性更新，
  避免同尺寸切换时角落留缝。
- **节流/去重**：`resized` 只做 channel send；worker 合并到最新请求，按
  `(win,w,h,radius)` 去重，最快 50ms 一次（≤20 次 SHAPE/s），拖拽 resize 不阻塞 UI。
- **测试**：`cargo test -p tervo-app --bin tervo` → **172 passed**。新增 8 例（均不依赖
  X server/GUI）：`corner_radius_is_zero_when_maximized_or_fullscreen`、
  `corner_radius_caps_and_adapts_to_small_windows`、`physical_extent_honours_scale_factor`、
  `radius_uses_physical_pixels_not_logical`、`corner_inset_matches_circle_rows`、
  `corner_inset_is_monotonic`、`mask_rects_mirror_all_four_corners`、
  `mask_rects_clamp_radius_and_skip_degenerate_windows`。
- **限制**：X11 SHAPE 只接受 1-bit 掩码，**边缘没有抗锯齿**（半径 12 时逐行 1px 阶梯）；
  Wayland/服务器无 SHAPE 扩展时只记日志、窗口保持直角；INPUT 显式保持整窗矩形，
  因此不提供“圆角外点击穿透”；resize 节流间隔内新长出的右/下边缘可能短暂被旧 shape 裁剪；
  半径上限固定 12 物理像素，未做 2x 屏视觉加倍或用户配置项。
- **手工验证（有显示器时；本轮遵守共享显示器约束未启动 GUI/xkey/xclick/xwin）**：
  1. 启动后看日志 `[corners] X11 SHAPE 1.1 可用` 与
     `[corners] win=0x... WxH radius=12 (logical ... × scale ...)`；四角透出桌面。
  2. 拖右/下边放大：日志尺寸递增、radius 保持 12；停止拖拽后角落为圆角。
  3. 点最大化：日志 `max=true` 且 `radius=0`，四角贴合屏幕；还原后 `radius=12`。
  4. F11 / `--fullscreen`：`[ui] fullscreen=true` 后 `radius=0`；退出全屏恢复 12。
  5. 2x DPI（如 `WINIT_X11_SCALE_FACTOR=2`）：日志 `logical ... × scale 2 ≈ ...
     physical`，geometry 与物理尺寸一致，radius 仍是 12 物理像素。
  6. 双实例（Ctrl+Shift+N）：两窗口日志的 win id/pid 不同，各自四角都圆、互不串窗。
  7. 极窄窗口（宽/高 ≤24px）：radius 随短边降到 `min(w,h)/2`，无越界/负尺寸矩形。

### 2026-11-06 · Tab 自动收缩 + 最左侧 Tab 搜索 + GUI 测试隔离

- **Tab 自动收缩**：按窗口宽度/Tab 数计算 `tab-width-cap`（40–180px），Tab 宽度
  `max(40px, min(cap, 标题宽度+36px))`；Tab 较少时按标题，多 Tab 时均分并逐步收窄，
  低于 68px 隐藏 close（hover/激活仍可关），120ms `animate width` 过渡。
  `+` / Profile 按钮移到 Flickable 外固定右侧，Tab 过多时中间区域可横向滚动，不再把按钮挤走。
- **Tab 搜索**：TabBar 最左侧新增 `⌕` 按钮（Chrome 风格）；浮层输入过滤当前窗口所有 Tab
  （标题 + pane 标题），↑/↓ 选择、Enter 跳转、Esc/点外部关闭；命令面板可搜 “Search Tabs”，
  注册 `tab.search`（Ctrl+Shift+A）。
- **GUI e2e 隔离**：`scripts/e2e_lib.sh` 默认启动 Xephyr 虚拟显示器（`:99`，
  `-no-host-grab`），所有 xkey/xclick/xwin/xwd 只作用于虚拟屏，不再占用用户的 :1 键鼠。
- 测试：`cargo test -p tervo-app --bin tervo` → 204 passed；Xephyr 真机：10 Tab 自动收缩、
  `[ui] tab search open items=10`、pick=4；截图 `.logs/tabs-autoshrink.png`、`.logs/tab-search.png`。
- 同时修复插件/SFTP 测试在刚写完临时脚本时的 ETXTBSY（exec 重试 5 次）。

### 2026-11-06 · Tab 关闭范围（左/右/其它）

- per-tab 菜单新增 Close Left / Close Right（保留 Close Others）；Rust 新增
  `close_tabs_left` / `close_tabs_right`，复用 `close_tab_inner` 以触发 PTY 关闭、
  TabClose 插件事件、后台保留策略与最近关闭快照。
- 修复首版 off-by-one：Close Left 现在只关 `keep` 次，当前 Tab 保持在最左；Close Right
  从尾部关到 `keep+1`，当前 Tab 保持激活。
- Xephyr 真机（5 Tab，右键第 3 个 → Close Left）：剩 3 个（当前 + 右侧）；
  Close Right 同；`.logs/tab-close-left.png` / `tab-close-right.png`。
- 测试：`cargo test -p tervo-app --bin tervo` → 204 passed。

### 2026-11-06 · 平台抽象与 GPU 路线文档

- 新增 `docs/PLATFORM_GPU.md`：盘点 X11 耦合点（xconn/quake/自定义 resize/SHAPE），
  定义 `Platform` 与 `FrameRenderer` 两个边界；GPU 分三阶段（持久 framebuffer + UI 合成、
  GPU 终端光栅、Wayland）；明确软件回退与非目标。
- 这是后续 Wayland/GPU 并行开发的接口约定，尚未在本轮改渲染实现。

### 2026-11-06 · resize 卡顿：交给 WM/compositor live resize

- **根因**：Tervo 无边框窗口用自绘 ResizeHandle，每个 pointer move 同步
  `query_pointer` + `Slint set_size/set_position`，触发整窗重排、pane grid resize、
  renderer 缓冲重建/整帧重绘，软件后端下逐事件都做一次昂贵提交。
  GNOME Terminal 走 GTK CSD + Mutter compositor live resize，应用只收 configure
  事件，尺寸推进由 compositor 帧驱动，且 VTE 渲染走 GPU，所以顺滑。
- **修复**：X11 边/角拖动优先发 EWMH `_NET_WM_MOVERESIZE`（direction 0..7 与
  Slint ResizeHandle 一致，button=1/source=1），让 Mutter/KWin 接管指针 grab 和
  live resize；应用侧 `wm_resize=true` 后不再每帧 set_size，只收 Slint resize。
  先检查 `_NET_SUPPORTED` 是否包含该 atom；不支持时保留原自绘 fallback（Xephyr 无 WM）。
  Wayland 后续对应 `xdg_toplevel.resize`，放入 `Platform` 抽象。
- **验证**：`xprop -root _NET_SUPPORTED` 有 `_NET_WM_MOVERESIZE`；204 tests 通过；
  Xephyr 无 WM 的 fallback 可启动。GNOME 实际拖拽待用户用 release 复测。
- **限制**：WM 路径依赖 EWMH；无 WM/远程 X 仍走自绘；圆角 SHAPE 仍非原生。

### 2026-11-06 · resize/SHAPE 止损：默认方角、Xvfb 无宿主窗口测试

- 现象：最大化/最小化后右侧露桌面、右上角按钮点不到。定位到 X11 SHAPE
  BOUNDING mask 在窗口尺寸/WM state 变化时可能保留旧尺寸；部分 WM 的 live
  resize/maximize 组合更明显。`clear_rounded_corners` 已补 INPUT 清理并加 250ms
  geometry 轮询重裁，但 SHAPE 仍不是可靠的原生方案。
- 决策：**SHAPE 圆角默认关闭**，`TERVO_CORNERS=1` 显式开启实验；默认窗口方角，
  不再让 mask 干扰 WM 最大化/最小化/live resize。show 后再同步一次 client 尺寸，
  规避 WM 用 preferred size 建 frame 的竞态。
- 测试隔离：`scripts/e2e_lib.sh` 改为 **headless Xvfb**（`.perf/xvfb` 或系统 Xvfb），
  不再创建 Xephyr 宿主窗口，不占用户 workspace/键鼠；Openbox/Xvfb 仅用于本地
  WM 行为验证。
- 测试：`cargo test -p tervo-app --bin tervo` → 204 passed。

### 2026-11-06 · 最大化/最小化修复：EWMH 请求 + 默认方角

- 现象：最大化/最小化无效、右侧露桌面/涂抹背景。点击回调已触发；排除按钮命中后，
  确认 SHAPE 的旧 BOUNDING mask 会裁掉 WM 放大后的区域，且 Slint/winit 的
  `set_maximized/set_minimized` 在部分 X11 WM 下未落地。
- 修复：`xconn::wm_set_maximized`（`_NET_WM_STATE` add/remove HORZ+VERT）、
  `wm_minimize`（`WM_CHANGE_STATE` IconicState），仍保留 winit 作为回退；
  `window_is_maximized` 读 EWMH 真实状态，避免 `is_maximized` 不同步导致只能放大不能还原；
  SHAPE 圆角默认关闭（`TERVO_CORNERS=1` 才开），先保证 WM 行为正确；
  show 后再同步一次 client 尺寸。
- 测试：204 passed；headless Xvfb（不再 Xephyr）验证启动/点击；Openbox 测试中
  回调可触发但 Openbox 不接受 EWMH maximize（测试 WM 限制），GNOME 待用户实测。

### 2026-11-06 · mouse-reporting TUI 右键菜单焦点修复

- 问题：在 dshtui 等开启鼠标上报的 TUI 里右键菜单关闭后 IME 焦点没有归还，
  文本选择/键盘输入失效；`right_click=off` 时右键被吞掉，TUI 收不到。
- 修复：`menu-dismissed` 与 `menu-action` 后调用 `invoke_focus_terminal()`；
  `off` 模式把右键 Press/Release 作为 SGR 鼠标事件转发给 PTY（仅 mouse reporting 时）。
- 验证：Xvfb + 自制 mouse-reporting TUI；右键开菜单→点外部关闭→输入 `abc`，
  TUI 原始字节日志收到 `abc`。`right_click=off` 的转发路径已编译。

### 2026-11-06 · 无边框窗口 live resize：winit accessor + 跨连接 Ungrab 实证

- **问题**：自绘 `ResizeHandle` 每帧 `query_pointer` + `set_size` 卡顿；之前只发 EWMH
  `_NET_WM_MOVERESIZE` 不 ungrab，拖动完全无反应。
- **关键实证**：X server 的 `XUngrabPointer` 只对“持有 grab 的那个 client”生效。用两个
  Xlib 连接 + XTest 在 `:105`/Openbox 对照：按下后不 ungrab → 540x472 不变；由另一个
  连接 ungrab → 仍不变；由**同一连接** ungrab 后再发 EWMH → 540→700。因此
  winit/Slint 的隐式 grab 必须由 winit 自己的连接释放，`xconn` 跨连接 ungrab 无效
  （这正是上次 EWMH 完全不能拖的根因）。
- **实现**：workspace slint 启用 `unstable-winit-030`；`on_start_window_resize` 经
  `WinitWindowAccessor::with_winit_window` 调 `winit::Window::drag_resize_window`，
  winit 在自己的 X11 连接内先 `XUngrabPointer`、flush，再发 `_NET_WM_MOVERESIZE`
  （button=1/source=1/direction 0..7）；Wayland 由 winit 转 `xdg_toplevel.resize`。
  成功后 `wm_resize=true`，drag/end 不再每帧 `set_size`。仅当 winit **实际使用 X11
  后端**时才先查 `_NET_SUPPORTED`（Wayland+XWayland 同时有 DISPLAY 时不会误判）：
  不支持则直接走自绘（若先调 winit，它会 ungrab；无 WM 接手时窗外 move 全部丢失，
  自绘也会失效）。WM 抢走指针后 Slint 收不到真实 ButtonRelease，故收到
  第一帧 WM resize 后异步补发 `WindowEvent::PointerReleased` 清理 TouchArea
  pressed/grabbed；若 150ms 内尺寸没变且还能收到 move，则回退自绘。
  `xconn::start_wm_resize` 按需求实现（查 root pointer、ungrab、flush、EWMH），保留为
  非 winit 后端兜底与协议单测对象，源码注明跨连接 ungrab 限制。
- **真实验证**（`Xvfb :105 -screen 0 1280x800x24 -ac -nolisten tcp` + `.perf/openbox`）：
  `xclick 908 400 drag 1068 400` → 540x472 → 700x472，日志
  `[ui] WM live resize dir=3 source=winit`；右下角 700x472 → 800x572，`dir=4`；
  下边 800x572 → 800x622，`dir=5`；全程无回退日志。截图
  `.logs/resizepolish/wm-resize-final.png`（`/.logs` 已忽略）。可复现脚本：
  `scripts/e2e_wm_resize.sh`（隔离 Xvfb/Openbox、只 kill 自己的 pid），同机
  `:109` 实测输出 `PASS: WM live resize 540x472 -> 700x472 -> 800x572`。
- **无 WM 回退**（`:108` Xvfb，无窗口管理器）：日志
  `[ui] WM 未声明 _NET_WM_MOVERESIZE，回退自绘 resize` + `[ui] 自绘 resize dir=3`，
  `xclick` 540x472→700x472；右下角 dir=4，700x472→800x572，证明 fallback 真可用。
- **测试**：`cargo test -p tervo-app --bin tervo` → **207 passed**（新增方向映射 2 例 +
  fallback 选择 1 例）。
- **限制**：只在 Openbox 验证，Mutter/GNOME 与真实 GPU 合成器的流畅度仍需用户实测；
  Wayland 路径走 winit `xdg_toplevel.resize`，未实测；若 WM 在 `_NET_SUPPORTED` 里声明
  支持却实际不接管，winit 已 ungrab 后应用可能收不到窗外 move（150ms watchdog 只有在
  还能收到 move 时才生效），此时仍需换回自绘/重新拖动；`unstable-winit-030` 是 Slint
  不稳定 feature，升级时要复核；DPI 跨屏仍未复测。

### 2026-11-06 · 背景图 / 终端透明度可用性重做（feat/bgpolish）

- **用户反馈**：背景图 + 透明度“稀烂根本没法用”。先按约束在 headless Xvfb
  `:106`（`.perf/xvfb/usr/bin/Xvfb :106 -screen 0 1280x800x24 -ac -nolisten tcp`）
  跑 GUI 截图复现，再对照代码定位。
- **根因（4 个，都可独立造成“没法用”）**：
  1. **alpha 双重预乘**：`render.rs` 已把默认背景 RGB 按 opacity 预乘，`image()` 却仍用
     `Image::from_rgba8`；Slint 对该接口的约定是**直通 alpha**，内部合成前会再预乘一次。
     结果 opacity=50% 时主题底色贡献只剩 25%，画面整体发黑；字形抗锯齿又按这份被压暗的
     底色混合，文字边缘发灰。旧提交 `b43443e` 的“修复发白”方向反了。
  2. **pane 帧图没有钉在左上角**：`Image { width: p.iw; height: p.ih }` 未写 x/y，Slint
     在元素小于父级时会居中。窗口尺寸除不尽 cell 时（如 800×568 内容区、cell 9×22），
     帧图偏移 (4,9)：左侧/顶部漏出**未经终端底色混合的原图**，右侧/底部与补条重叠，
     出现一条双倍 alpha 的暗边。1080×736（cell 整除）看不出来，所以此前截图没暴露。
  3. **没有暗化遮罩**：最低 20% opacity 时纯白图片区域仍是 204 灰，浅色终端文字对比度
     <2:1；用户把 opacity 拉到能看见图片时文字就不可读。
  4. **默认/旧配置体验**：有图 + opacity=100% 时图片完全被主题底色盖住，用户会以为
     “设了没生效”；无图时 opacity 仍可拖动但不生效，且旧配置残留低 opacity 没有明确
     回退。设置面板高度也不够放 Dim/Opacitiy 两组控件与说明。
- **修复**：
  - `render.rs` 改回**直通 RGBA**（主题 RGB 不变、只写 alpha）并用 `from_rgba8`，由 Slint
    统一预乘一次；alpha 范围 0–1（0% 完全透出图片）；文字/光标/彩色 cell 背景仍恒为
    alpha=255。新增回归测试：opacity=0 时默认背景 alpha=0、字形像素 alpha=255 且保持原色。
  - 新增 `background_dim: f32`（0–0.8，默认 **0.60**，两位小数，整数 TOML 也接受）：
    Slint 在背景图之上、所有内容之下加纯黑 `opacity: background-dim` 遮罩；设置页新增
    **Dim** 滑块（0–80%）与百分比。默认 60% 把纯白压到 102 灰，0% opacity 下白字对比度
    仍有 4.94:1；用户可自行调低。
  - `Image` 显式 `x: 0px; y: 0px`；pane 右侧/底部不足一格的余量用
    `Palette.colors.bg.with-alpha(opacity)` 的**两条窄补条**补齐，且不给整块 pane 铺这个
    半透明色（否则帧图下会二次叠 alpha）。800×600 实测左右/上下边缘颜色一致，无暗边/漏图。
  - 无图时 `renderer_opacity()` 强制 1.0（旧配置残留低 opacity 也不会让纯色终端发灰）；
    首次通过设置页启用背景图且 opacity=100% 时自动降到 35%，避免“设图没反应”。
  - 路径输入：合法路径边打边加载/写盘，输入过程中不闪红字；Enter/失焦 commit 时才把
    非法路径显示为红字并保留上一张背景（坏路径不写配置）；× 清除路径并恢复不透明。
    Opacity 拖动即时重绘预览、松手写盘；Dim 预览由 Slint 直接改属性、松手写盘。
    设置面板 520 高，Opacity 下方提示按“无图/100% 遮住图/正常”三态切换。
- **测试**：`CARGO_HOME=... CARGO_TARGET_DIR=$PWD/target cargo test -p tervo-app --bin tervo`
  → **206 passed**（新增 dim clamp/roundtrip/default、alpha=0、文字不透明等；不启动 GUI）。
- **GUI 验收（headless Xvfb :106，1280×800×24，winit-software，窗口 1080×736）**：
   - 高对比测试图（纯红/绿/蓝/白四象限 + 文字）截图：`.logs/bg-readability-000/030/060/090.png`；
     同场景整屏终端输出：`.logs/bg-opacity-000/030/060/090.png`。
   - `.logs/bg-settings-opacity0.png`：0% opacity 时设置面板与 TabBar 仍不透明可读；
     `.logs/bg-cover-800x600.png`：改变窗口比例后 cover 正确（红绿分界仍在 1/2，无拉伸），
     帧图贴左上、边缘无漏图/暗边。
   - `.logs/bg-invalid-path.png`：非法路径红字 + 输入框红边 + 背景回退纯色；
     `.logs/bg-noimage-opacity20.png`：无图 + 旧 opacity 20% 时内容区仍是纯主题底色。
   - `xkey`/`xclick` 实测：输入 `.logs/bg-test.png` → config 写入 image + opacity 0.35；
     非法路径 Enter → 错误显示且 config 保持上一张图；× 清除 → `background_image=""` 且
     终端立即变为不透明；拖 Dim 到 20% → config `background_dim=0.20`；拖 Opacity → 0.03 落盘。
   - 对比度量化见 `.logs/bg-contrast.txt`：0/30/60/90% 四象限最差（纯白块）分别为
     4.94/6.94/9.79/13.19:1；白字前景恒为 (238,238,236)；彩色 cell 背景恒为 (204,0,0)，
     证明文字/光标/彩色 cell 不随 opacity 变透明。
- **改动文件**：`crates/app/src/config.rs`、`render.rs`、`main.rs`、`crates/app/ui/main.slint`。
- **限制**：无模糊、无桌面透传（窗口自身不透明，opacity 只在与背景图叠加时可见）、无每主题
  独立背景；cover 固定，无 contain/fill 选项；大图仍在 UI 线程同步解码，可能短暂卡顿；
  Slint 按路径缓存图片，同一路径替换文件需重启或改路径；设置面板在极矮窗口（内容区
  < 约 480px）仍可能裁剪，未做整页滚动；只验证了 winit-software，Skia/GPU 后端未走查；
  设置页布局/滑块命中只做了 Xvfb 自动点击，未做人工触控板走查。

### 2026-11-06 · TabBar 收缩与 Tab 搜索交互打磨

- **Tab 收缩**：下限从 40px 提到 52px（`tab-width-cap` 52–180px），宽度动画
  120ms→140ms；窄 Tab 标题字体降到 10px、左右内边距收窄、close 阈值 84px
  （更窄只在 hover 覆盖显示），状态标记在 <64px 时压缩成 4px 小点，避免
  “所有标题只剩 ...”。背景/边框/close 颜色加 120–140ms 过渡；没有常驻动画。
- **溢出滚动**：Tab 条放进 `Flickable`，左侧留 58px 固定搜索/品牌区，滚动内容
  不会钻到图标下面；支持拖拽与横向滚轮，底部加 2px 滚动条。活动 Tab 变化、
  新建/关闭、窗口 resize 时 `reveal-active-tab()` 自动把它滚进视口；活动 Tab
  是最后一个时把 `+`/Profile 一起带入视口。`+`/Profile 仍在 Tab 条内跟随 Tab，
  没有改成右侧固定。
- **Tab 搜索**：手绘放大镜按钮（圆环 + Path 手柄）替代字体字符 `⌕`；浮层
  `min(340, width-8)` 宽，贴 TabBar 下方；TextInput 自动聚焦并显示 placeholder，
  实时过滤 Tab 标题 + pane 标题；↑↓ 循环、Enter 跳转、Esc/点外部关闭；无匹配
  时显示 “No matching tabs”，Enter 不再关闭面板；结果列表内部 Flickable 滚动，
  选中项自动保持可见。关闭/跳转/空结果 Enter 后把焦点还给终端。
- **命令与快捷键**：`run_command` 补上 `"tab.search"`（命令面板之前会落到
  “未知命令”）；保留 Ctrl+Shift+A，并加 `^A` 控制字符回退分支。顺带修复重命名
  浮层提交/取消后焦点没有归还终端的问题。
- **GUI 验证**：headless Xvfb `:107`（1280x800，`-ac -nolisten tcp`），窗口
  1080×736 与 540×700。1/3/10/20 Tab 截图：
  `.logs/tabs-{1,3,10,20}-1080.png`、`.logs/tabs-{1,3,10,20}-540.png`
  （20 Tab 末个活动时另见 `.logs/tabs-20-540-active-last.png`）；
  20 Tab 下点 `+` 新建第 21 个（`.logs/tabs-21-after-plus.png`），`+`/Profile
  仍可见；滚轮左右滚动 `.logs/tabs-20-scrolled-{left,right}.png`；hover close
  `.logs/tab-hover-close.png`。Tab 搜索：20 结果、输入过滤、Down×12 选中滚入、
  Enter `[ui] tab search pick=3`、Esc/点外部关闭后终端继续收到
  `echo focus_ok` / `outside_ok`；命令面板搜 “Search Tabs” 执行
  `[ui] run command tab.search`；空态与滚动截图
  `.logs/tab-search-{20,empty,wheel-scroll,wheel-then-down,pick-focus2,click-outside}.png`。
  拖动排序在 Flickable 改造后仍正常（`[ws] reorder tab 0 → 1`）。
- **测试**：`cargo test -p tervo-app --bin tervo` → **204 passed**。
- **限制**：20 Tab/窄窗口必须横向滚动才能看全；活动 Tab 在中部时 `+`/Profile
  可能在视口外（只有末个 Tab 会自动带入），需要滚到末尾；滚动无 snap/惯性参数
  调优，reveal 后可能保留前一个 Tab 的半截；搜索只做子串匹配、无模糊排序/高亮；
  Ctrl+Shift+A 的 `^A` 回退分支未单独注入（正常文本映射和命令面板已验证）。
  未做 CPU/帧率 profiler 采样；所有过渡都是一次性属性动画，没有常驻 Timer 或
  循环动画。本轮未改窗口 resize/max/min/背景图代码。

### 2026-11-06 · resize：16ms 合并 + 实时重排（不再拉伸字符）

- 现象：winit 已接管 WM live resize，但每个 configure 都 `resize_grid + PTY + 重绘`
  会卡；先试过“拖动时拉伸旧帧、停下再重排”，但字符会变形再回弹，用户明确拒绝。
- 最终：configure 先合并到 16ms，只保留最新尺寸；每次 apply 执行完整的
  `refresh_views`（grid/PTY）与 `redraw_visible`，字符始终 1:1；不再有
  `resize-stretch`。`TERVO_RESIZE_PROFILE=1` 可打印 apply 次数/耗时。
- 验证：80 步/4ms 快速拖拽 → 约 30 次 apply，app 侧 reflow 0.5ms/次；
  Openbox 540→840；无 WM 回退 540→701；输入/截图确认字符清晰；209 tests。
- 限制：真实帧率仍取决于 Slint 合成/软件终端帧；这只是去掉重排洪泛，不等于 GPU。

### 2026-11-06 · resize 右上角图标被裁：UI 布局不能延后

- 现象：快速缩小时右上角设置/最小化/最大化/关闭按钮先消失再出现。根因是 UI 布局
  被 16ms 合并推迟，缩小时按钮仍在旧宽度的右边界外，被新窗口裁掉一帧。
- 修复：每个 configure 立即 `refresh_views_layout` + `request_redraw`，保证控件
  当帧贴到新右边界；terminal grid/PTY/字符重绘仍每 16ms 合并，避免重排灌满 UI 线程。
- 字符仍 1:1，无 resize-stretch。

### 2026-11-06 · 默认 software 渲染：Skia 在 resize 时阻塞事件循环

- 用户实测日志：Skia 下 `WM live resize dir=3 source=winit` 后 150ms 判定
  “未接管指针”回退自绘；同一台机器 `SLINT_BACKEND=winit-software` 下 WM 路径
  正常、拖动明显更跟手。
- 结论：Skia/GL 帧提交在 resize 时让 winit 事件循环排队，watchdog 误判 WM 未接管；
  不是 EWMH 协议问题。先把 **software 设为默认**，`SLINT_BACKEND=winit-skia`
  保留为 GPU 实验开关。
- 保留：WM live resize、UI 布局逐 configure 立即更新、terminal 16ms 合并重排、
  字符 1:1 不拉伸。
- 验证：software 默认 + Openbox WM resize 540→700→800 通过；209 tests。
- 限制：software 在高分屏/大窗口有上限；GPU 终端帧仍是后续方向。

### 2026-11-06 · 右侧 pane 右键菜单“不好使”：坐标夹取

- 用户日志里 `right click pane=2` 与菜单打开都正常，但随后 `menu dismissed`；
  复现发现右侧/底部右键时菜单左上角被 `min(x, width-150)` / `min(y, height-276)`
  硬夹到离鼠标约 100px 的位置，看起来像没弹在右键处，用户点外部关闭。
- 修复：靠右/下时菜单翻到鼠标左侧/上方（标准 context menu 锚定），x/y 不再
  远距离夹取；菜单 action 仍绑定被右键的 pane id。
- 验证：右侧分屏 720x472，右键右 pane (600,300)，菜单贴在鼠标左上方；
  Split Right action 仍作用于 pane=2。


### 2026-11-06 · GPU/Skia 默认开启 + resize 事件循环修复 + 持久双缓冲

- **背景**：master 在 `main()` 无条件 `SLINT_BACKEND=winit-software`，为了规避用户
  NVIDIA 机器上 Skia resize 卡顿与 `WM live resize 未接管指针，回退自绘`。本轮移除
  强制 software 默认，默认请求 Slint Skia；显式 `SLINT_BACKEND=winit-software` 保留。
- **根因/关键实证**：
  1. `/dev/dri` 缺失时 Skia 能建窗，但 OpenGL/GL 初始化失败会落到 Skia software
     surface：`:104` 用 `GALLIUM_DRIVER=invalid` + `MESA_LOADER_DRIVER_OVERRIDE=invalid`
     复现“窗口存在、内容全黑”；`set_rendering_notifier` 不触发任何 GPU 回调。
  2. 这不是“等一帧”问题，必须自动回退。策略：无 `/dev/dri` 默认
     `winit-software`；有 DRM 时默认 `winit-skia`，建窗后 1s 内没有 GPU API 回调、
     且没有任何渲染帧（Skia software surface 在 Slint 1.18 不出帧）就 `exec` 回退
     `winit-software`（保留参数/日志，标记防循环）。
  3. 旧 watchdog 只看“按下后是否过了 150ms”，GPU 首帧/渲染排队时 configure 晚到就被
     误判成 WM 没接管。改为“最近一次 configure / 是否见过 configure”，400ms（未见过）
     或 800ms（见过但停了）才回退；有 configure 就绝不因渲染慢而抢回自绘。
  4. resize 性能热点不是 terminal reflow（release 0.5–2ms），而是每帧整屏
     `SharedPixelBuffer::new` + `copy_from_slice`：2560x1413 实测 4.8ms。双缓冲持久帧
     把 `image()` 降到 0.00ms（`detach=false`），改为一帧整屏 draw（0.55–1.11ms）。
- **实现**：
  - `Cargo.toml`：Slint features 增加 `renderer-skia-opengl`（优先 OpenGL，而不是仅
    wgpu/softbuffer），保留 `renderer-software`；`log` 打开 Slint 日志通道。
  - 新增 `render_probe.rs`：`Window::set_rendering_notifier` 判实际 renderer；
    `NativeOpenGL` 回调里经文档化的 `get_proc_address` 查 `glGetString`，打印
    `GL_VENDOR/GL_RENDERER/GL_VERSION`；1.2s 无 GL 回调明确打印
    `skia-software-surface` 且注明 headless 不能验证 GPU；`Unsupported` 打印
    `winit-software`。GPU 确认状态用于 1s 自动回退。
  - `main.rs`：默认 backend、无 DRM 降级、debug-only 注入 UI 初始化失败、exec 回退、
    resize watchdog/redraw 合并/背景隐藏、profile 日志。
  - `render.rs`：`FrameBuffers` 双缓冲；`draw()` 切换 buffer 后整屏重绘，`image()` 只
    refcount clone；记录 `last_draw_us/last_image_us/last_buffer_detached`。
  - `main.slint`：`background-suspended`；窗口面积 >1.5MP 的拖动期间隐藏背景图/暗化层，
    停止 200ms 恢复。小窗口不做，避免闪一下。
- **验证（全部 headless Xvfb :104，未碰 :1、未用 Xephyr、未 pkill）**：
  - 实际后端：默认无 DRM → `winit-software`；显式 `SLINT_BACKEND=winit-skia` +
    `LIBGL_ALWAYS_SOFTWARE=1` → `actual_renderer=skia-opengl`、
    `GL_RENDERER="llvmpipe (LLVM 15.0.7, 256 bits)"`（CPU 光栅，明确不是 NVIDIA）。
  - 黑屏回退：强制 GL 失败日志 `exec 自动回退 winit-software`；新进程
    `actual_renderer=winit-software`，`.logs/fallback-gl/after.png` 非黑（content 可见）；
    debug 注入 UI 初始化失败也走同一回退。
  - 功能：`.logs/gpu-func/` 覆盖 Tab/搜索（items=2、pick=0）、分屏右 pane 右键
    （`right click pane=3`、`menu action pane=3 action=0`）、mouse reporting
    （TUI 原始字节收到 `ESC[<0;31;17M`/`m`）、最大化 2560x1440、还原 1080x736、
    最小化 `_NET_WM_STATE_HIDDEN` 唤醒、背景/暗化/resize 后恢复。
  - resize：Skia/llvmpipe 1080x736→1380x736，80 步/4ms，14 次 apply，中位 1.80ms，
    最大 3.00ms，35 帧渲染中位 2.85ms/最大 6.84ms，`watchdog 回退 0`；software
    11 次 apply 中位 3.10ms/最大 4.10ms。大窗口 2538x1396→2238x1396：software 1 次
    9.60ms，Skia 2 次中位 10.85ms/最大 12.70ms（Openbox 合并了 configure）。
  - 无 WM：`-ac -nolisten tcp` 的 Xvfb 无 Openbox，日志
    `WM 未声明 _NET_WM_MOVERESIZE` + `自绘 resize dir=3`，1080→1380，背景恢复。
  - 缩小中途截图 `.logs/shrink-clip/mid.png`：窗口 1017x736 时右上角四个按钮完整，
    证明“缩小立即重绘”路径没有晚一帧裁切。
  - 背景 A/B：2560x1440 持续帧有背景 render 中位 3.53ms，隐藏后 1.49ms/帧。
  - 测试：`cargo test` 全绿；`cargo test -p tervo-app --bin tervo` → **213 passed**
    （新增 3 个 watchdog/redraw 单测 + 1 个双缓冲脏帧回归）。
- **限制/残余风险**：
  - **没有真 NVIDIA GPU**：所有 Skia 数字来自 llvmpipe，不能声称等同用户机器；用户
    580.173.02 上仍需 release 复测 resize 流畅度、首帧、长时间稳定性。
  - Skia software surface 在无 GL 时必须等 1s watchdog 才回退，用户会看到约 1s 黑窗；
    无 DRM 的机器已提前走 software，只有“有 /dev/dri 但 GL 坏”才会遇到。
  - exec 回退会重启进程，当前 Tab/会话重新创建；PTY 子进程随旧进程关闭。
  - `renderer-skia-opengl` 会优先 OpenGL（而不是 wgpu/Vulkan）；若 NVIDIA GLX/EGL
    有驱动特定问题，仍可能回退到 software surface 并触发 1s 超时。
  - 双缓冲每次切 buffer 整屏重绘；4K/多 pane 下的 CPU 上限未压测。`Image` 仍需 Skia
    每帧上传纹理（没有安全的持久 GPU texture API），这是下一步。
  - 背景图仅在 >1.5MP 窗口拖动时隐藏；拖动中会看到背景短暂消失，停止 200ms 恢复。
  - Mutter/GNOME live resize 的“能收到 move 但 configure 晚到”场景无法在 Openbox
    精确复现；watchdog 逻辑已用单测覆盖，但真机仍需看 `TERVO_RESIZE_PROFILE=1` 日志。
  - `xwd -id` 与 live resize 有竞态：窗口在 geometry 查询与 XGetImage 之间继续缩放时，
    会抓到“右侧黑边/错行条纹”的伪帧（软件后端同样有黑边，只是没有背景条纹）。按钮
    验证只用 geometries 前后一致的干净帧，不把伪帧当应用缺陷；真机肉眼复测仍需做。

### 2026-11-06 · dsh-tui 分屏兼容：右键 contextmenu 优先给 TUI

- dsh-tui 的“分屏”不是 Tervo 分 pane，而是单 PTY 内 Ink 渲染的右侧栏：
  `sidePanel.splitEnabled` + `canSplit(columns>=93)` + ratio 0.68 + 1 列 divider。
- 兼容缺口：dsh-tui 用 SGR button 2（右键 press）实现 ContextMenuEvent；Tervo 原先
  在 mouse-reporting pane 里把普通右键拦成本地菜单，侧栏右键失效。
- 修复：mouse-reporting 下普通右键转发给 TUI；Tervo 菜单改为 Shift+右键。
  非 mouse-reporting 的普通 shell 仍是普通右键开 Tervo 菜单。
- 测试：伪 mouse-reporting TUI 收到 `<2;col;rowM/m`；非上报右键菜单不受影响。

### 2026-11-06 · TUI 基础兼容：1004 focus + kitty CSI-u

- 补 DECSET 1004：100ms 轮询 winit 窗口焦点，pane 焦点/窗口焦点变化时向启用
  1004 的 PTY 发 `CSI I`/`CSI O`；模式刚启用也会补发当前状态。
- 补 kitty keyboard protocol 的 CSI-u：alacritty TermMode 检测 DISAMBIGUATE/
  REPORT_ALL_KEYS 后，带修饰的可打印键编码为 `CSI codepoint;mods u`；普通 shell
  仍走旧控制字节，不改变日常行为。
- 测试：合成 TUI 收到 `ESC[O`/`ESC[I`；214 app tests（新增 kitty 单测）。
- 未做：modifyOtherKeys 全量、kitty event-types/release、OSC 52、URL 点击等。

### 2026-11-06 · IME 候选点跟随聚焦 pane

- 根因：Slint 只在 TextInput 文本变化时发 IME Update；移动隐藏 TextInput 不会更新
  preedit spot，切右侧 pane/光标移动后候选窗仍停在旧位置。
- 修复：`update_ime_spot` 用聚焦 pane 的 `(cursor.line, cursor.col)` 换算窗口坐标
  （内容区 + tabbar 偏移），直接调 winit `set_ime_cursor_area`；焦点切换、Tab 切换、
  resize、每次终端帧重绘都会同步，并按坐标去重。
- 验证：右侧分屏聚焦后日志 `[ime] spot pane=2 ... win=407,32`，不再从 0 开始。
- 限制：fcitx/ibus 是否遵守 XNSpotLocation 取决于输入法；若仍不跟随，需要输入法侧日志。

### 2026-11-06 · IME spot 跟随“只移动光标不重绘”的 TUI

- dsh 侧栏通过声明光标把终端光标停到右侧输入框，但可能不产生内容 damage；
  Tervo 原来只在 redraw_visible 后更新 IME spot，候选窗就停在左/旧位置。
- PaneHandle 增加原子 cursor（actor 每次解析/控制事件后刷新，不依赖 UI 拉帧）；
  IME 用 50ms 定时器读取当前光标并调 winit `set_ime_cursor_area`，按坐标去重。
- 验证：合成 TUI 发 `CSI 5;80H` 后不再输出，日志 50ms 内出现
  `[ime] spot pane=1 cell=4,79 win=711,120`。
- 限制：输入法是否遵守 spot 仍取决于 fcitx/ibus；dsh 真机需用户复测。

### 2026-11-06 · dsh Btw 侧栏 IME spot：点击锚点兜底

- 实证：dsh `/btw` 的 BtwComposer 只画反色 caret（Text inverse），**不调用
  useDeclaredCursor、不移动硬件终端光标**；所以硬件 cursor 始终在左侧 chat 输入框。
  任何跟踪硬件 cursor 的终端都无法知道右侧 composer 的位置。
- 兼容：mouse-reporting TUI 内左键点击时，记录点击格作为 IME 候选锚点；50ms IME
  同步优先用该锚点，直到切换 pane/Tab 或点击别处。实际 dsh：点击右侧 composer 后
  `[ime] spot` 从 x=54 变 x=846。
- 普通 shell/非 mouse-reporting 不受影响；不改变 TUI 自动 focus 逻辑。

### 2026-11-06 · OSC 52 剪贴板 + X11 PRIMARY

- **OSC 52（core）**：`osc.rs` 在旁路扫描器里补 `OSC 52;Pc;Pd`：
  - `Pd` 为 base64 文本 = set；`?` = query；空 = clear；`Pc` 支持 `c`（剪贴板）、
    `p`/`s`（都映射 X11 PRIMARY），缺省按 xterm 取 `s`；
  - 自带严格 base64 编解码（不新增依赖），支持带/不带 `=`；非法字符、错误长度、
    中间补位、非 UTF-8、解码后超过 **1 MiB** 都整条忽略，不 panic；
  - 扫描器对 `52;` 前缀单独放宽到 ~1.4 MB（1 MiB 原文的 base64），其它 OSC 仍限
    16 KiB；超限后继续吃到 terminator，不污染后续序列；
  - 新事件 `CoreEvent::Clipboard(Store/Load/Clear)`；`PaneHandle::clipboard_reply`
    负责把查询结果编码成 `OSC 52;Pc;BASE64 ST` 写回 PTY。alacritty `TermConfig`
    的 `osc52` 设为 `Disabled`，避免同一条序列被处理两次。
- **app**：`osc52 = "allow" | "deny"`（默认 allow，未知值按 allow）。allow 时
  Store 写 arboard 剪贴板 / `SetExtLinux` PRIMARY，Load 读对应选区并回写，Clear
  调 arboard clear；deny 两个方向都拒绝（query 也能泄漏本机剪贴板）。日志以
  `[clipboard] osc52 ...` 前缀输出。
- **copy-on-select / 中键**：拖选、双击选行、Ctrl+Shift+C 现在同时写系统剪贴板与
  X11 PRIMARY；中键粘贴优先读 PRIMARY，为空/读取失败回退普通剪贴板（日志
  `middle paste source=primary|clipboard`）。不需要 `wayland-data-control` feature。
- **单测**：core 新增 `osc::tests::osc52_*` / `base64_*` / `clipboard_reply_*`
  （set、c/p/s、缺省 selection、query、clear、非法 base64/UTF-8、1 MiB 边界、
  跨 chunk、回包截断）；`tests/osc52.rs` 用真实 PTY 验证 OSC52 set 事件与
  `clipboard_reply` 回写。core 全量 91 tests；app 215 tests。
- **GUI（headless Xvfb :113，`scripts/e2e_osc52.sh`）**：16 项 PASS，覆盖
  set c/p（`xsel -b/-p` 读到 `hello-osc52`/`primary-osc52`）、query 回包
  （`ESC]52;c;aGVsbG8tb3NjNTI=ESC\`）、clear、copy-on-select 双写、中键优先
  PRIMARY、清空 PRIMARY 后回退剪贴板、deny 不处理且不写剪贴板。未使用 Xephyr、
  未碰 :1、未 pkill；只 kill 脚本自己启动的 Xvfb/app。
- **限制**：只支持单个 selection 字符（`OSC 52;cp;...` 这类组合未实现）；缺省
  selection 按 xterm 是 PRIMARY，部分工具可能预期 `c`；查询回包截断到 1 MiB；
  Wayland 原生 data-control 未启用，只有 X11/XWayland 路径；没有设置页开关，
  需要手改 `config.toml`；多行中键粘贴仍走同一确认弹窗。
- **安全**：allow 时任何能向终端写转义序列的程序（SSH 远端、`cat` 不可信文件、
  tmux 内远端进程）都能覆盖或读取剪贴板/PRIMARY；README 已写明，可设 deny。

### 2026-11-06 · 右 Shift 清空 dsh 输入：修饰键伪字符

- 现象：左侧 Shift 正常，右侧 Shift 会把 dsh 输入框文字删空。
- 根因：Slint 把 Shift_R 映射为 `\u{0015}`，而 `is_modifier_artifact` 只过滤了
  Shift_L 的 `\u{0010}`；`\u{0015}` 写入 PTY 就是 **Ctrl+U**，终端行编辑器清空当前输入。
- 修复：`\u{15}` 在 shift=true 且无 ctrl/alt 时按修饰键伪事件忽略；Ctrl+U
  （ctrl=true）仍然保留。新增回归测试。
- 测试：app 221 passed。

### 2026-11-06 · 字体 fallback：中文/emoji/宽字符/组合音标

- 字体链按用途分组惰性加载：CJK（wqy-microhei → Droid → Noto CJK → zenhei）、
  SYMBOL（Noto Sans Symbols 2 → DejaVu Sans）、EMOJI（Symbola → Noto Emoji）、
  ICON（Nerd Font 常见路径 + `TERVO_FALLBACK_FONTS`）、COLOR（Noto Color Emoji）。
  中文只碰 CJK 组；emoji 不会为了找字形解析 18 MB 的 Noto CJK。
- 渲染层：缺字逐字体回退，字形步进超过 `span*cell_w` 时等比缩小，并横向裁剪到
  该字符占的格子；组合音标（alacritty `Cell::zerowidth`）叠加到基字形墨迹中心，
  ZWJ/VS 跳过不画。`CellData` 仍是 16 字节，零宽字符走 Frame 的 side table。
- 修复后到的组合音标：alacritty 的零宽输入不产生 damage；检测到零宽码位时强制
  整帧重取并扫描零宽表。零宽字符不常见，正常整屏拷贝保持无分支热路径。
- fontdue 只有轮廓光栅化，Noto Color Emoji（CBDT/CBLC）与 Twemoji（COLR）画不出：
  命中彩色字体后停止扫描，渲染明确占位框（emoji 中心点 / Nerd 图标竖条），
  不崩溃、不画 tofu；彩色 emoji 支持列为已知限制。
- 测试：`cargo test -p tervo-core` 47 lib / 89 total；app 222 passed（新增
  core 宽字符/组合/ZWJ/截断 7 例、app 占位/叠加/裁剪 8 例）。
- 截图：`.logs/font-emoji/before.png`、`after-default.png`、`after-nerd.png`；
  重新生成用 `scripts/font_glyph_shot.sh`（Xvfb :116，仅 kill 自己的进程）。
- 性能：200x50 同机 A/B：dirty 单行约 21µs→18µs，整屏约 101µs→85µs；10MB 解析
  约 49–51ms，与基线持平（热路径未加逐格零宽检查）。
- 限制：ZWJ 家族仍是 3 个 emoji / 6 格（alacritty 按码位计宽，不做 grapheme
  cluster 合并）；组合音标无 GPOS，用墨迹居中近似；内置字体路径偏 Debian/Ubuntu。

### 2026-11-06 · 光标形状/闪烁/颜色 + bold-is-bright（feat/cursor-style）

- **目标**：补齐 GNOME Terminal 基础外观项，不改 GPU/resize/背景/Tab 布局/IME。
- **配置（app/config.rs）**：新增 `cursor_shape`（block/underline/ibeam）、
  `cursor_blink`（system/on/off）、`cursor_color`/`cursor_text_color`（空 = 主题）、
  `bold_is_bright`（默认 true）、`bold_color`、`bold_color_same_as_fg`（默认 true）。
  全部走 serde default，旧 v0/v1 配置缺字段自动补齐；读入时归一形状/闪烁/颜色，
  非法颜色清空并打日志。字段是 additive，不 bump schema（避免每次启动重写用户
  配置）；v0→v1 迁移写回时自动带出 GNOME 默认值，已有测试覆盖。
- **粗体（core/palette.rs + frame.rs）**：`Palette` 增加粗体策略；
  `CellData::from_cell` 按 BOLD 解析：`bold-is-bright` 把 ANSI 0–7/Named
  Black..White 映射到 8–15（默认前景不换色，避免 Catppuccin 的 `bright_fg`
  反而更暗）；`bold-color-same-as-fg=false` 且配置 `bold_color` 时，默认前景色
  的粗体文字使用自定义色，显式 ANSI 色不受影响。`CursorShape::from_name`
  接受 block/underline/ibeam 及 beam/bar 别名。
- **渲染（app/render.rs）**：`draw_cursor` 支持形状/颜色/光标文字色/闪烁相位；
  block 的字符改用不透明主题背景（不再复用可能带 opacity alpha 的 default_bg）。
  新增 `draw_without_cursor` + `cursor_overlay`：光标不再画进终端帧，改为生成
  cell 大小的小图（block 整格；underline/beam 透明背景 + 线），由 Slint 叠在
  `pane.image` 上。app 的 530ms UI Timer 只切 `cursor-visible`，不拉帧、不重绘、
  不重传整屏纹理；`cursor_shape/color` 改动也只重生成这张小图。
- **设置页（main.slint）**：Appearance 增加 Cursor（形状/闪烁/颜色/文字色）与
  Bold（bold-is-bright、same-as-fg、bold color）。Appearance 内容改成页内
  Flickable（`content-height: appearance-pane.preferred-height`），小窗口下可滚动；
  回调即时写配置并应用。粗体改动通过 `PaneHandle::set_theme` 重建调色板并重取整帧。
- **测试**：基线 `cargo test -p tervo-app --bin tervo` 229 → **241 passed**
  （新增 config 外观默认/旧配置/颜色解析/形状闪烁归一/roundtrip 6 例，render
  光标形状区域/颜色覆盖/闪烁 off/overlay 透明/DECSCUSR 优先 6 例）；`cargo test -p tervo-core`
  70 lib + 46 integration（新增 bold-is-bright/bold-color/CursorShape 名 3 例）。
- **截图（headless Xvfb :117，`scripts/cursor_style_shot.sh`）**：
  `.logs/cursor-style/block-blink-on.png`、`block-blink-off.png`、
  `block-config-off.png`、`underline-blink-on.png`、`ibeam-blink-on.png`、
  `bold-is-bright-on.png`、`bold-is-bright-off.png`；设置页
  `.logs/cursor-style/settings.png` 与滚动后的 `settings-scrolled.png`。
  点击设置页 Block / Blink Off 的日志 `[appearance] cursor_shape=block`、
  `cursor_blink=off` 并写回 config（`.logs/cursor-style/live.toml`）。
- **闪烁不整帧重绘的证据**：`TERVO_RENDER_PROFILE=1` 下空闲 3s，
  `[render]` 行增量 = 0；同时 0.28s 间隔连拍 6 张，光标红色像素在
  0/166/0/0/166/0 之间切换，说明只切 overlay 可见性。测试钩子
  `TERVO_CURSOR_BLINK_PHASE=on|off` 可冻结相位用于截图。
- **限制**：`system` 闪烁当前等同 `on`（未读 gsettings `cursor-blink`）；
  DECSCUSR 的 underline/beam/hollow 优先于配置；frame=Block 时用配置形状（无法
  区分“显式 block”和默认 block），与 GNOME 的“配置只是默认值”仍有细微差别；颜色只接受 #RGB/#RRGGBB；无窗口失焦暂停闪烁；只验证 Xvfb/llvmpipe，
  未验证真 GPU；设置页其它分类仍是固定高度。

### 2026-11-06 · 彩色 emoji 字形（feat/emoji-color）

- **目标**：把 emoji 占位框换成真正彩色字形，**默认开启**；只动字体/渲染层，
  不碰 Sixel/图片协议/window resize/GPU/Tab 布局/IME。
- **依赖**：app 增加 `owned_ttf_parser = "0.25"`（封装 ttf-parser 的 `Face` 与文件
  所有权，避免自引用 unsafe；与 slint 传递依赖同版本）和 `png = "0.18"`（已在
  Cargo.lock，避免新增两套解码器）。fontdue 继续负责轮廓，彩色位图完全独立。
- **实现（glyph.rs）**：
  - `FontSet` 回退链新增 `color_candidates`：内置 NotoColorEmoji 的 Debian/Ubuntu/
    Fedora 常见路径 + `TERVO_FALLBACK_FONTS` 追加项；**CJK 永不进入彩色候选**。
  - `ColorFace` 用 `owned_ttf_parser::OwnedFace` + ttf-parser 的
    `glyph_raster_image` 读 CBDT/CBLC、EBDT/EBLC、sbix；只支持 PNG 与预乘
    BGRA32。文件字节与解析结果由 `OwnedFace` 一起持有，无 unsafe。
  - PNG 用 `png` 解码并统一到 8-bit；预乘 BGRA 反预乘为直通 alpha RGBA。
    `fit_color_dims` 按 `span*cell_w x cell_h` 等比缩小，`scale_color` 做预乘
    面积平均，避免透明边缘黑边；水平/垂直居中。
  - `GlyphCache.color_map` 以 `(字符, 样式, 占格数)` 缓存 `Rc<Vec<u32>>`；
    命中只复制指针，不重读字体/不重解 PNG；失败结果也缓存。
  - `TERVO_COLOR_EMOJI=0/off/false/no` 可关闭；默认开启，配置 schema 不变。
  - 只有彩色位图的字体不再进入 fontdue 轮廓链（旧逻辑会为 emoji 解析 10 MB
    字体再放弃），`glyph_face` 返回类型简化为 `Option<&Font>`。
- **实现（render.rs）**：emoji 码位先查彩色缓存，命中后用独立 RGBA 混合
  （不套文字 coverage 的 gamma、不乘 fg）画在 `span` 格内；横向裁剪到
  `[x0, x0+span*cell_w)`、纵向裁剪到本行。彩色字体缺失/COLR 不支持时自动回落到
  原单色 `blit`，再回落到占位框。块状光标下同样优先彩色。
- **emoji cluster**：VS16/ZWJ 仍是 alacritty 零宽标记，不额外占格；`❤+VS16`
  单格彩色，`👨‍👩‍👧` 按 3 个宽字符逐个彩色绘制（6 格），`👩‍💻` 2 个组件
  （4 格），不做 grapheme cluster 合并/合字位图。
- **测试**：基线 app 249 → **258 passed, 0 failed**（新增 9 例）：CBDT 解码/彩色
  像素/尺寸/缓存命中 `Rc::ptr_eq`/emoji 不解析 CJK、`❤` 单格、ASCII/CJK 不建立
  彩色链、损坏/缺失/非彩色字体安全跳过、预乘 BGRA 反预乘、缩放边界、真实渲染的
  😀/❤+VS16/ZWJ 家族彩色像素与网格裁剪。
- **截图（headless Xvfb :121，`scripts/font_glyph_shot.sh`，font-size 16）**：
  `.logs/font-emoji/color-after.png`（默认彩色：😀🚀❤️✔️👍⚡🎉 与 👨‍👩‍👧 /
  👩‍💻 / 🏳️‍🌈 组件均为彩色）、`.logs/font-emoji/color-off.png`
  （`TERVO_COLOR_EMOJI=0` 单色/占位对照）。未使用 Xephyr、未碰 :1、未 pkill。
- **性能/日志证据**：`TERVO_FONTLOG=1` 下彩色字体只探测一次：
  `[font] 彩色字体 NotoColorEmoji.ttf#0（CBDT/CBLC/sbix, 3976 字形, 4.75ms）`；
  同一次会话中 CJK 只因中文行解析一次（wqy-microhei），没有 Noto CJK 18 MB
  解析记录；emoji 行渲染前也没有额外的 fontdue 回退加载。
- **限制**：COLR/CPAL 与 SVG 彩色字形不绘制（Twemoji 等会回落单色/占位）；
  sbix 仅 PNG（JPEG/TIFF 不支持）；不做 ZWJ 合字；彩色位图按逻辑字号解码，
  HiDPI 由合成器放大；彩色 emoji 忽略终端 fg/INVERSE；RGBA 缓存随不同 emoji
  数量线性增长。

### 2026-11-06 · IME 稳定性加固（feat/ime-hardening）

- **范围**：只动候选点同步与修饰键/点击锚点，不碰 Sixel/emoji/GPU/resize 行为/Tab/字体，不新增依赖。
- **候选点去重键**（`ImeSpotKey`）：从 `(i32,i32)` 扩为
  `pane + 逻辑毫点坐标/尺寸 + 物理像素 + DPR 千分比 + 是否点击锚点`。
  纯函数 `ime_spot_key` 负责换算（`rect + cell*cell_size`，纵向再加
  tabbar 偏移），单测覆盖 dsh 实测格 `(37,94)` / cell 9x22 / tabbar 32 →
  `win=846,846`，tabbar 在底部 → y=814，DPR 1.25 → 物理 1058/11x28，
  pane/锚点/光标格变化都会产生不同键。
- **DPR/窗口坐标**：继续向 winit 传 `LogicalPosition/LogicalSize`（winit 在
  X11 内部按 `scale_factor` 转物理像素），去重键同时记录物理值与 DPR：
  只改缩放不改逻辑坐标时也会重发，不会被误判为“没动”。
- **强制重发**（`ime_spot_dirty`）：pane 切换、Tab 切换、布局刷新
  （`refresh_views`）、resize、窗口重新获得焦点、DPR 变化、点击锚点更新时置脏；
  即使坐标与上次完全相同也重新 `set_ime_cursor_area`。只有真的交给 winit
  （窗口已创建）才清脏，启动早期不会吞掉首次下发。
- **窗口事件**：复用已有的 `on_winit_window_event`（Slint 只允许一个 filter，
  因此并入文件拖放桥接，改名 `install_native_window_events`），处理
  `Focused(true)` 与 `ScaleFactorChanged`；50ms 轮询仍作为后备。
- **点击锚点**：抽出 `ime_click_anchor(button, wants_mouse, shift)`，仅
  mouse-reporting TUI 的普通左键记录锚点；Shift+左键本地选择、右键、中键、
  普通 shell 都不锚定。TUI 关闭鼠标上报时清锚点并重发硬件光标位置。
- **Shift 过滤**：`is_modifier_artifact` 已覆盖 Shift_L `U+0010` / Shift_R
  `U+0015`（含 ctrl=true 的组合前缀）；新增左右 Shift 回归测试，确认真实
  Ctrl+U（text="u"）不被误杀。日志可看到 `[key] "\u{10}"/"\u{15}"` 后紧跟
  `[key] 忽略修饰键伪事件`，PTY 无 0x10/0x15。
- **测试**：`cargo test -p tervo-app --bin tervo` 249 → **253 passed**
  （新增 IME 坐标/DPR/pane 去重 2 例、点击锚点 1 例、左右 Shift 1 例）；
  `cargo test -p tervo-core` **70 lib + 53 integration** 全通过（未改 core）。
- **真实 dsh 验收（Xvfb :122，1080x912，cell 9x22）**：把 `~/.dsh` 复制到
  `/tmp` 可写 HOME，`DSH_HOME` 指向副本、`onboarding.json` 置
  `completed=true`、假 API key 仅用于进入会话；`dsh --profile dsh-tui` 在
  Tervo 内启动，先提交 `hello` 建立会话，再 `/btw test` 打开右侧 BtwComposer。
  点击右侧 composer 前后：
  - 点击前硬件光标 `[ime] spot pane=1 cell=35,6 win=54.0,802.0 ... anchor=false`
    （左侧 chat 输入框）；
  - 点击后 `[mouse] down pane=1 cell=(37, 94) button=0 mods=0 wants=true reporting=true`
    与 `[ime] spot pane=1 cell=37,94 win=846.0,846.0 phys=846,846 scale=1.000 anchor=true`；
  - 在右侧 composer 输入 `abc`，再注入 Shift_L/Shift_R 后输入 `xyz`，
    截图仍为 `abcxyz`（若 Shift_R 漏进 PTY 会变成 Ctrl+U 清空）；
  - 仅改窗口宽度（1080→1000→1080，候选坐标不变）后 spot 日志行数 24→26；
    焦点切到 root 再回窗口后 26→28：证明 resize/focus 的 dirty 重发生效，
    不会因坐标去重而停在旧位置。
- **普通 shell 验收**：独立 Tervo 实例 `--shell /bin/sh`，注入 `echo A` +
  Shift_L/Shift_R + `CCEPT` + Enter，XTest 发送的字节为 `[65][67][67][69][80][84][13]`，
  Shift 伪字符均被忽略，PTY 无 0x10/0x15；点击普通终端后
  `[ime] spot ... anchor=false`，不会误用点击锚点。
- **截图/证据**：`.logs/ime-hardening/dsh-btw-panel.png`（右侧 btw 面板）、
  `dsh-btw-after-click.png`（点击后）、`dsh-after-shifts.png`（`abcxyz`
  保留）、`shell-shift.png`（`/bin/sh` 输出 ACCEPT）。验收脚本为临时的
  `.ime-test/run_dsh_btw.sh`（未提交、已删除；依赖本机 dsh/Node 与 CP 配置）。
- **残余限制**：Xvfb 中没有 fcitx5/ibus 守护进程，无法断言真实中文候选窗的
  像素位置，只能验证 spot/键序列/composer 行为；DPR 路径只有纯函数单测，
  未在缩放 Xvfb 上跑 dsh；pane 切换是通用 `focus_pane`/`refresh_views` 路径，
  dsh 单 pane 场景未做分屏 e2e；**没有**引入 fcitx5 DBus 方案（本机无
  fcitx5，且会引入异步 DBus 依赖/新故障面）；若将来要做，必须独立
  feature flag + 默认 fallback + 单独 commit，不能默认开启，不能 unsafe。

### 2026-11-06 · Settings UI：Tab 化设置页 + 图标分类 + 下拉/分段

- **问题**：设置是居中 520×520 小 modal，所有控件平铺；主题/光标/滚动条/按键编码用
  一排 chip，分类无分组无图标，搜索只能切分类。Sixel Images 段加入后旧 e2e 坐标
  也点不到 Scrollbar/Backspace。
- **选择**：优先做**主窗口内的 Settings Tab**，而不是拖拽浮层。实现上不进入
  `workspace::Tab`/PTY 模型：标签栏渲染一个只在 `settings-open` 时出现的齿轮 Tab，
  内容区整页替换为设置页；关闭后不创建/杀死任何 PTY，也不污染会话快照。
  比真正改 Tab/PTY 模型的侵入小得多，同时满足“像 Tab、可关闭、Esc、不建 PTY”。
- **UI**：
  - 左侧 PERSONALIZE / WORKSPACE / SYSTEM 三组、8 个分类（Appearance/Terminal/
    Window/Keys/Profiles/Plugins/Vault/Advanced），每类 16×16 线性图标（Path/Rectangle
    手绘，无 emoji），选中态 accent 边条 + 图标/文字。
  - 右侧卡片化：Theme 下拉框、Cursor/Scrollbar/Right click/Backspace/Delete 分段选择器、
    布尔 switch、字号/回滚 Stepper、透明度/遮罩 Slider；Themes 切换即时生效。
  - Profiles 分类新增内嵌列表（打开/默认星标/编辑/删除/新建），完整管理面板仍可从
    “Manage Profiles…”叠在上方打开；Advanced = 原 Config（路径/Open Folder/Import/
    Export/诊断文案）。
  - 搜索：Rust 索引带 `section` 锚点，结果点击后切分类 + 80ms 后二次 bump token，
    Slint 在布局完成后滚到对应卡片并 accent 高亮/标题变色。
  - 关闭：Esc / 标签 × / 页头 × / 点终端 Tab；键盘焦点在设置 FocusScope，
    打开期间按键不会写 PTY（用 `read` shell 做 leak 测试）。
- **回退**：`TERVO_SETTINGS_LEGACY=1` 启动时设置页回到居中 modal（同一套控件，
  顶部 LEGACY MODAL 标记，标签栏不显示 Settings Tab）。默认新 UI；启动日志
  `[ui] settings ui=tab|legacy-modal`。
- **测试**：
  - `cargo test -p tervo-app` **263 passed**（基线 262，+1：搜索命中带 section、
    8 分类均有索引项）。
  - `scripts/e2e_terminal_behavior.sh` 在 Xvfb :124 全通过；设置段改为 120×32
    （1080×736）稳定窗口 + 新坐标，覆盖 Unlimited / Scrollbar Always / Scroll on
    output / Audible bell / Backspace Control-H / Delete ASCII DEL，并新增
    “搜索 sixel → 跳 Images”和 Esc 关闭断言。
  - `scripts/settings_ui_shots.sh` 在 Xvfb :124 生成 open/dropdown/terminal/search/
    search-jump/profiles/closed/small(700×500)/tiny(480×360)/legacy 截图；
    小窗口侧栏改为 156–164px，分类名不再被截断。
  - 焦点/PTY 隔离：`/bin/sh` 循环 `read` 写文件；打开设置输入 `LEAK` 未落盘，
    Esc/标签 ×/终端 Tab 关闭后 `AFTER` 落盘，证明焦点归还终端。
  - 浅色主题（Yaru Light）与底部 TabBar 也各截图走查。
- **限制**：没有真正把 Settings 作为 `workspace::Tab`（这是刻意选择，避免改 PTY/
  会话模型）；设置页无字体族/行距项；搜索跳到卡片而不是单个控件；键盘 Tab 焦点
  遍历仍有限；legacy 是同一套控件换 modal 呈现，不是保留旧代码逐像素复刻。

### 2026-11-06 · P0 UI 阻塞加固：PTY 写 backlog + SFTP 10s 超时（feat/p0-hardening）

- **范围**：只修 Codex 只读审计的 P0 两项（PTY 输入/拉帧、SFTP 元数据同步），顺带把
  `copy_selection/search_with/contents` 的 500ms 同步等待压到 200ms。不改默认配置/UI 布局，
  不碰 Sixel/emoji/GPU/resize；未合入 master，提交在 `feat/p0-hardening`。
- **问题 1（PTY）**：旧 `PaneHandle::input()` 用有界(64) `send` 阻塞；actor 的
  `write_input()` 在写队列满时退化为阻塞 `send`；`pull()` 无超时 `recv()`。慢 reader
  时 pty-writer 阻塞在 `write_all` → actor 阻塞 → input 通道满 → UI 线程无限等。
- **修复 1**：
  - 键盘/粘贴改为 UI 直接推 **按字节限界的写 backlog**（16 MiB / 8192 块，
    `crates/core/src/pane.rs::WriteQueue`），pty-writer 用 Condvar 消费；
    生产者只做 `try push`，actor 不再做键盘中转。超限才丢弃新块并计数
    （`PaneStats::write_dropped_*`），日志限频（首块/每 64 块）。
  - `pull()` 100ms `recv_timeout`；超时返回 `Frame::skipped` 空占位，app 跳过重绘；
    下一次 pull 带 `force_full` 整屏补画，迟到响应按请求 ID 丢弃（actor 侧收到
    `Ctrl::Pull{id,force_full}`，迟到帧 `try_send` 满时自排空，绝不在 actor 阻塞）。
  - 所有 ctrl 投递改为 50ms `send_timeout`（超时计数）；copy/search/contents
    回复上限 500ms → 200ms。
- **问题 2（SFTP）**：`SftpTarget::run()` 用 `wait_with_output()` 无超时；list/mkdir/
  rename/delete 在 Slint 回调里同步执行，远端卡死 = 窗口冻结。
- **修复 2**：
  - `run_command_with_timeout`：spawn 后 `try_wait` 轮询 + 截止时间；超时 kill
    **整个进程组**（`process_group(0)` + `libc::kill(-pid)`，连带 sftp 拉起的 ssh），
    reader 线程走通道 + 有限宽限，不让重孙进程把 join 挂死。
  - 默认 `SFTP_TIMEOUT=10s`；错误文案 `sftp 超时（10s），已终止子进程`。
  - 新增 `SftpCommand`/`SftpOutcome`/`SftpOpJob`：Slint 回调只 `start` 后台线程，
    100ms timer + 400ms poll 非阻塞收结果；list/mkdir/rename/rm 全部后台化；
    `rm`→`rmdir` 回退共享同一 deadline，不会 20s。
  - 操作期间 `sftp_busy` 阻止并发子进程；传输完成后的列表刷新仍走同一后台任务。
- **前/后证据（临时 probe，现已固化为 `crates/core/examples/p0_blocking_probe.rs`）**：
  慢 reader（每 10ms 读 1 字节）+ 2000×1KiB 灌入：
  - master `a82ac52` 同源探针：`pull_blocked=true (no frame within 2000ms)`、
    `input_blocked=true (2000 chunks did not return within 2100ms)`；
  - 修复后：`pull_done elapsed_ms=0.4`、`input_done elapsed_ms=2.0 worst_ms=0.134`，
    无丢弃（2 MiB < 16 MiB）；10 MiB 单次粘贴由压力测试逐字节校验。
  - SFTP 旧实现（master 快照 + 假 `sftp`=`exec sleep 30`）：`sftp_run_blocked=true
    (no result within 2000ms)`；修复后同一场景 300ms 测试超时并返回 `sftp 超时
    （300ms），已终止子进程`，默认 10s 由 :133 GUI e2e 验证。
  - 压力测试（`crates/core/tests/p0_backpressure.rs`）：
    - 慢 shell + 8 MiB 输出洪峰 + 10 MiB 粘贴：`worst_input≈0.8ms`、
      `worst_pull≈0.15ms`，文件与粘贴内容**逐字节相等**（顺序/不丢/无 PTY 转换）；
    - 完全不读的 shell 灌 20 MiB：16 MiB 后明确丢弃 3 MiB（计数+首条日志），
      input/pull 均 <250ms，进程不卡；
    - backlog 饱和时 copy/search/contents：copy≈0.24ms、search≈1.5ms、
      contents≈0.22ms（上限 200ms）；
    - actor 退出后 `pull()` 在 50ms ctrl 投递超时后返回 `skipped`（<300ms）。
- **SFTP GUI 证据（Xvfb :133，`scripts/e2e_p0_blocking.sh`）**：假 `sftp` 写 pid 后
  `exec sleep 30`；点击面板 List 后 10034ms 返回 `sftp 超时（10s），已终止子进程`，
  app 进程存活，假 sftp pid 已被 kill；期间再次打开 TabBar 菜单并执行 New Tab
  成功，证明窗口事件循环未冻结。日志摘要
  `[sftp] 后台操作完成: list / 失败: sftp 超时（10s），已终止子进程`。
- **测试**：
  - `cargo test -p tervo-core`：**90 lib + 63 integration**（新增 p0_backpressure 4、
    WriteQueue 2）；`cargo test -p tervo-app --bin tervo`：**281 passed**（基线 277，
    新增元数据超时 4 例）。
  - `cargo test --workspace --locked`：**474 passed, 0 failed**（app 281 + core 153 +
    xclick/xdrop/xkey/xwin 40）；Xvfb :133 的 SFTP e2e 全 PASS。
  - 启动冒烟（Xvfb :133，winit-software）：`READY TIMING total_us=217295`、
    `FIRST_DAMAGE_US 252680`，无 panic。
  - 既有键盘/粘贴/拉帧/搜索/复制/选区/OSC52/Sixel/mouse-report 回归全绿。
- **限制/风险**：
  - 16 MiB 写 backlog 是明确上限：持续灌入超过上限会丢弃**新块**（计数+限频日志），
    极端慢 shell 下不保证全部字节送达；10 MiB 正常粘贴有覆盖。
  - ctrl（resize/theme/selection 等）在 actor 极端忙时可能 50ms 投递超时被丢弃；
    下一次 UI 操作/重绘会恢复，但 resize 极端情况下可能与 actor 网格短暂不一致。
  - SFTP 元数据命令没有独立 Cancel 按钮，只能等 10s 硬超时；大目录/慢链路可能在
    10s 被截断，需要用户重试。`rm`→`rmdir` 回退共享 10s，不会双倍等待。
  - Vault Argon2 仍同步在 UI 回调里（本轮未动）：`Vault::unlock(&mut self)` 接口和
    密钥状态迁移需要单独设计 worker/状态机，属于可选项，未为赶 P0 冒险改接口。
  - 未改默认值/UI 布局/P1（keyboard/search/url/settings/Sixel/emoji/GPU/resize）；
    真实 50 MiB/s 零丢字节压测与远端大目录（>10s）实测未做。

### 2026-11-06 · P1-10 终端右键菜单美化（feat/context-menu）

- **范围**：只改 `crates/app/ui/main.slint` 的 pane 右键菜单视觉/信息架构，外加 e2e 脚本
  与本文档；动作 ID、`menu-action` 回调、菜单打开/`menu-open` 状态、hit area、
  outside-close、默认配置均未动。PTY/SFTP、Sixel/emoji、GPU/resize、IME、键盘协议未碰；
  TabBar/Profile/Tab 菜单仍用原来的紧凑 `MenuRow`。
- **信息架构**：13 项（命中链接时 15 项）分 5 组：Link（仅链接时）、Clipboard、Pane、
  View、Session。Link 有链接时置顶，保证右键链接的第一动作仍是 Open/Copy Link；其余顺序
  为 Copy/Paste → Split Right/Split Down/Close Pane/Broadcast → Find/Select All/Clear →
  Read Only/Reset/Reset and Clear/Save Contents。所有 action id 0–14 保持原映射。
- **视觉**：菜单项 14px/30px 行高（≥需求 13px/30px），分组标题 13px/22px semi-bold
  次级色；菜单宽 280px 且随窗口夹取；16×16 手绘线性图标（复制/粘贴/分屏/关闭/广播/查找/
  全选/清除/保存/只读/复位/链接），不依赖 emoji 字体。Copy/Paste/Split/Close/Find 显示
  真实快捷键提示；Read Only ON / Broadcast ON 用 accent 淡底与图标高亮；Close Pane 悬停
  用 danger 色。面板 raise 底 + border + 10px 圆角 + 阴影，全部取 `Palette` 当前主题色。
- **定位/溢出**：优先右/下；放不下翻到左/上；再放不下才夹进内容区（右/下 8/6px 边距）。
  菜单高 490px（链接 572px），内容区不够时 `menu-view-h = 内容区 - 12px`，用
  `Flickable`（`interactive: false`，滚轮仍可滚动，点击立即交给菜单行）滚动，并在右侧
  画 3px thumb；不再有整块菜单掉到窗口外。
- **验证**（Xvfb :135，`scripts/e2e_context_menu.sh`）：
  - `cargo test -p tervo-app --bin tervo`：**281 passed**（基线 281）。
  - 暗色普通菜单面板 bbox 278×488 @ (301,45)，锚点=右键处；菜单外左键关闭日志
    `[ui] menu dismissed`。
  - 有选区：拖选前后区域像素 diff **2750**，选区高亮保留，菜单正常弹出。
  - 链接菜单 278×570，Link 组置顶；点 Copy Link 日志 `[link] copy uri=...`，
    `xsel -b` 剪贴板 = `http://example.com/ctx-menu-demo`（action 10 未变）。
  - 480×360：菜单 278×314 夹在 (193,39)，右/下留边；滚轮 3 格后菜单区域像素 diff
    **26118**，thumb 随内容移动。
  - 864×700 靠右下 (804,608)：菜单翻到 (525,151)，不溢出。
  - Solarized Light：菜单 278×488，组标题/快捷键提示/图标对比度可读。
  - `scripts/e2e_term_menu.sh`：窗口改 24 行（内容区 528px），行坐标改为
    `padding 6 + 分组标题 22×组数 + 30×index + 15`，Read Only index 9、Save index 12；
    Read Only 切换/保存/Reset 全 PASS，`menu_open.png` 完整显示 13 项。
- **截图**（`.logs/context-menu/`，gitignore）：`menu_normal.png`、`menu_selection.png`、
  `menu_link.png`、`menu_narrow_480x360.png`、`menu_narrow_scrolled.png`、
  `menu_bottom_right.png`、`menu_light.png`；`.logs/term-menu/menu_open.png` 为
  720×560 的全菜单截图。
- **限制**：
  - 短窗口（内容区 < 502px）时菜单必须滚动；490/572px 高是为了同时保留全部动作，
    没有做折叠分组或二级菜单。
  - 菜单内没有键盘导航（与改动前一致）；本轮只保证现有 hit area/outside-close 不变。
  - 现有 pane 菜单没有 per-item disabled 状态，UI 也拿不到“选区是否为空”；Copy 在
    无选区时仍可点击（点击为无操作），未新增禁用逻辑。
  - 菜单宽是 280px + 窗口夹取，不是逐字测量 preferred-width；极窄窗口用 elide。

### 2026-11-06 · P1 链接体验：URL/OSC 8 hover 手型 + 本地路径 Ctrl+click（feat/url-paths）

- **范围**：只做 P1-03（URL hover 手型、本地文件路径 Ctrl+click、失败提示），保留 OSC 8 优先/
  http(s)/file 白名单/右键 Open·Copy Link/普通 Ctrl+click 选词与 TUI 鼠标上报语义。不碰设置 UI、
  搜索、键盘协议、Sixel、emoji、GPU、resize；worktree `.worktrees/url-paths`（`feat/url-paths`，
  基于 master `9ffee39`），按要求**不合 master**。提交信息：`feat(link): hover 手型 + 本地路径`。
- **识别层（core）**：
  - `link.rs` 保留 URL / IPv4[:port] / 路径三类识别；路径起点边界收紧为**行首/空白/开括号/引号**，
    避免把 `--prefix=/usr`、`PATH=/bin:/usr/bin`、`user@host:/tmp$` 提示符里的斜杠当成可点路径；
    `key=https://…` 这类 URL 仍可命中。`http://IP:port` 作为 URL 正常识别；裸 IP:port 仍只标记
    `LinkKind::Ipv4`，UI 不打开（防误判）。
  - `frame.rs` 新增 `Frame::link_at(row,col) -> Option<(LinkKind,String)>`，与 `url_at` 共用
    “显示列 → 文本字符下标”映射：宽字符右半 spacer 回退到左半边，组合字符在 `zerowidth` 表里
    不占列，因此路径/URL 后接中文或重音不会错位；`url_at` 仍只返回 URL，旧调用/测试不变。
- **hover 手型（不重绘终端）**：
  - Slint `pane-hit` 的 move 事件调用新 `pane-hover(p.id,x,y)`；Rust 用 `link_hover_cell` 按格去重，
    只有换格才重建行文本/扫描链接，且只有“悬停链接 ↔ 非链接”状态变化时才 `set_row_data` 更新
    `PaneView.link-hover` 并 `winit::Window::set_cursor(Pointer/Default)`。同格移动、空白间移动
    完全不改模型/不拉终端帧；Slint 侧 `mouse-cursor: p.link-hover ? pointer : default` 让指针状态
    与元素 hover 生命周期一致。`changed has-hover` → `pane-unhover` 在指针离开 pane 时清手型，
    避免残留到 TabBar/设置页。
  - 手型覆盖：OSC 8（白名单 scheme）、行内 http/https/file URL、本地路径；裸 IP 不显示。
    远端 Profile（ssh/telnet/serial）的**路径**不显示手型也不本地打开；URL/OSC 8 不受影响。
  - 日志：`[link] hover pane=1 cell=r,c kind=uri|path text=...`、`[link] hover end ...`、
    `[link] cursor=pointer|default`，便于 e2e/排障。
- **本地路径打开**：
  - `~` 只展开 `$HOME`（不执行 shell、不支持 `~user`）；相对路径以 pane shell 的
    `/proc/<pid>/cwd` 为基准（取不到回退 Tervo 进程 cwd），避免 `cd` 后点 `./a` 打开错目录。
  - 打开前 `metadata` + `access(2)`：不存在/父目录无权限/文件不可读/目录不可读或不可搜索
    都只写日志 + 底部浮层，**不 spawn xdg-open**；打开时 `Command::new("xdg-open").arg(path)`
    单参数传绝对路径，绝不拼 shell。`file://` URL 仍走原白名单 URI 路径，不做本地存在性检查。
  - 右键菜单的 Open Link / Copy Link 现在同时支持 URL 与路径（Copy 复制展示文本；Open 走同一
    校验/远端策略）；TUI 鼠标上报下 Ctrl+click 仍原样转发给应用，未改变 xterm 语义。
- **测试**：
  - `cargo test -p tervo-app --bin tervo`：**284 passed**（基线 281；新增裸 IP 不打开/路径解析/
    不存在与权限 3 例，并更新 OSC 8 优先/不安全 scheme 测试走 `frame_target_at`）。
  - `cargo test -p tervo-core`：**94 lib + 63 integration** 全绿（新增 link 路径边界/`http://IP:port`
    2 例、frame `link_at` 宽字符/组合字符列映射 2 例）。
  - `scripts/e2e_url_paths.sh`（Xvfb **:128**，140x24，winit-software，fake xdg-open）PASS：
    URL hover 日志 + XFixes 光标图 **16x16 X（hash 8d4475…）→ 24x24 手型（hash ae9be6…）→
    移开恢复 24x24 箭头（hash 85f60f…）**；hover/离开前后 `[render] pane=` 日志 **0→0**
    （未触发终端帧重绘）；OSC 8 命中 `https://osc.example/x`；裸 IP:port
    不切手型；Ctrl+click 真实文件 → fake xdg-open 收到绝对路径；不存在/无权限路径分别提示
    `路径不存在` / `无权限读取路径` 且 xdg-open 调用数不变；右键路径 Open Link 再打开一次、
    右键 URL 仍出现 Open/Copy Link。截图 `.logs/url-paths/`：`hover_url.png`（x11grab 叠加手型）、
    `path_missing_toast.png`、`path_deny_toast.png`、`menu_path.png`、`menu_url.png`、
    `cursor_pointer.png`/`cursor_default.png`、`app.log`、`xdg-open.log`。
- **限制/风险**：
  - 路径识别要求前置空白/开括号/引号：带空格路径、`~user`、grep 式 `/tmp/a:12` 不识别；
    行尾标点会被 `trim_end` 去掉（路径本身以 `.`/`,` 结尾时可能被截）。
  - 裸 IP:port 仍被识别为 `Ipv4` 但设计上不打开；只有带 scheme 的 `http://IP:port` 可点。
  - 远端判定按 pane 创建时的 Profile：SSH/telnet/serial pane 不本地开路径；split/duplicate 出来的
    pane 是本地 shell，按本地处理。无插件 Linkifier、长 URL 折行不拼接。
  - TUI 鼠标上报模式仍把 Ctrl+click 转发给 TUI（保持既有行为）；hover 手型仅作提示，
    实际打开仍走 Shift+右键菜单 Open Link。Wayland 下 `set_cursor` 未做专项验收。
  - 悬停状态在鼠标移动时更新；若终端内容在指针静止时变化，手型可能保持到下一次移动/离开。
    未做 URL 下划线/高亮，只切系统指针。

### 2026-11-06 · 完整 Kitty / modifyOtherKeys 键盘协议（feat/keyboard-protocol）

- **协议状态**：`core/keyboard.rs` 在把字节喂给 vte 的同时旁路解析
  `CSI ? u` / `CSI = flags;mode u` / `CSI > flags u` / `CSI < n u`，维护 current
  flags 与 main/alternate 两套独立栈；kitty query 只回本终端实际实现的 5 个
  bit（disambiguate/event/alternate/report-all/associated-text）。同时保留
  xterm XTMODKEYS：`CSI > 4 ; 1|2|3 m`、`CSI > 4 m`、`CSI ? 4 m`。
- **为什么自己维护栈**：alacritty 0.26 的 `report_keyboard_mode()` 回的是键盘
  栈顶，`CSI = 3 u`（set 不 push）后答 `?0u`；其 push 也不保存“push 前当前
  flags”，set→push→pop 会丢状态。两处都与 kitty 规范不符，故关闭
  `TermConfig.kitty_keyboard`，由 core 扫描器接管；主/备用屏仍按 47/1047/1049
  切换各自栈。`keyboard_protocol = "off"` 时扫描器完全 inert（不回包、不编码）。
- **编码（keys.rs）**：
  - kitty `CSI unicode:alternate ; mods:event ; text u`；DISAMBIGUATE 只把
    Esc / Ctrl+可打印 / Alt+可打印转成 CSI-u，单 Shift 仍发文本；REPORT_ALL
    把包括 Enter/Tab/Backspace 在内所有键转 CSI-u；REPORT_EVENT_TYPES 的
    press 省略事件类型，repeat=`:2`、release=`:3`；REPORT_ALTERNATE_KEYS 带
    shifted key，REPORT_ASSOCIATED_TEXT 带文本。
  - 特殊键用 kitty 规范序列：方向 `CSI 1;mod A/B/C/D`、Home/End `CSI 1;mod H/F`、
    Insert/Delete/PageUp/PageDown `CSI 2/3/5/6;mod~`、F1–F12 按 kitty 表
    （F3 为 `CSI 13;mod~`）、F13–F24 为 `CSI 57376..57387;mod u`。
  - 修饰位：shift=1/alt=2/ctrl=4/super=8/caps=64/num=128，序列里 +1；Slint
    的 meta 映射 super。caps/num lock 不在 Slint `KeyboardModifiers` 里，运行时
    恒 0（编码与单测已覆盖这两个 bit）。
  - modifyOtherKeys：可打印键 `CSI 27;mod;code~`；level 1 只处理 alt/meta，
    level 2 所有修饰键，level 3 无修饰键也编码；功能键用 xterm 带修饰序列；
    kitty flags 非空时 kitty 优先。`CSI > 4 m`（无 Pv）按常见应用意图当 level 2
    （xterm 原语义是 reset，差异已写入矩阵）。
- **release/repeat**：Slint 侧新增 `key-released(event)` 回调、`key-input` 透传
  `event.repeat`。Rust 只在应用请求 REPORT_EVENT_TYPES 时回放 release，并用
  per-pane held-key 登记表只发送“press 真的进过 PTY 且以转义序列发送”的键；
  普通 shell、off、搜索框、快捷键录制中不产生任何 release 字节。纯文本键在
  未开 REPORT_ALL_KEYS 时仍按 kitty 语义只发文本（不配对 release）。
- **零行为变化**：`encode_key_event` 在 flags 为空且 modifyOtherKeys=0 时直接
  调用旧 `encode_mode`；单测 `no_protocol_is_byte_identical_to_legacy` 对
  字母/大写/控制字节/特殊键/中文/backspace/delete 逐字节比对。配置
  `keyboard_protocol = "auto"|"off"`（默认 auto，未知值回 auto，旧配置缺字段
  兼容），off 只在新 pane 生效。
- **验证**：
  - `cargo test -p tervo-app --bin tervo`：**297 passed**（master 基线 285 + keys 12 例）；
    `cargo test -p tervo-core`：**177 passed**（107 lib + 6 keyboard_protocol
    集成 + 既有 64），Xvfb 无关。
  - `cargo test --workspace --locked`：**514 passed, 0 failed**。
  - 伪 TUI（真实 PTY，`crates/core/tests/keyboard_protocol.rs`）：query 收
    `ESC[?0u`；set/push/pop 收 `?3u/?7u/?3u`；备用屏独立 `?3u`，回主屏 `?1u`；
    `CSI > 4;2m` + `?4m` 收 `ESC[>4;2m`；`keyboard_protocol=off` 一个字节都不回。
  - Xvfb :123（`scripts/e2e_keyboard_protocol.sh`，不碰 :1/Xephyr/pkill）：
    kitty 场景 Ctrl+A → `1b5b39373b3575`（`CSI 97;5u`）、release → `CSI 97;5:3u`，
    Shift+Up → `CSI 1;2A`、release → `CSI 1;2:3A`；modifyOtherKeys 场景
    Ctrl+A → `CSI 27;5;97~`；普通 shell/off 场景 Ctrl+A/b/Up 与旧字节
    `01 62 1b5b41` 完全一致，且 query/modify 无回包。
  - 真实 dsh-tui（Xvfb :123，`TERM=xterm-kitty`、DSH_HOME 指向 workspace 副本）：
    app 日志 `flags=1`，Ctrl+Shift+J → `CSI 106;6u`、Shift+Enter → `CSI 13;2u`、
    普通 `x` → `x`；dsh welcome guide 保持运行（截图
    `.logs/dsh-kbd-final/shot.png`），说明真实 TUI 链路可用。
- **未完成/限制**：
  - base layout key（标准 PC-101 物理键）无法从 Slint `KeyEvent` 推导，只上报
    shifted key；非 US 布局的 shifted 映射为 US 近似。
  - caps/num lock 状态拿不到（winit/Slint 不暴露），运行时 bit 恒 0。
  - 纯文本键 release 只在 REPORT_ALL_KEYS 下发送；未请求协议时 Shift+Tab 仍是
    旧 `TAB` 字节（保持零行为变化）。
  - `CSI > 4 m` 无值形式按 level 2 兼容处理（与 xterm reset 语义不同，已文档化）；
    未实现 formatOtherKeys=1；modifyOtherKeys 的 F21–F24 用 CSI 42–45。
  - 不支持 Wayland IME；键盘协议与硬编码 app 快捷键的优先级仍由现有分发决定
    （Ctrl+Shift+字母会被 app 抢占）。

### 2026-11-06 · P1-07 事件唤醒：400ms CoreEvent 轮询 → wake（feat/event-wake）

- **基线**：`poll_events` 每 400ms 跑一次，OSC 标题/Bell/OSC52/通知/插件活动/SFTP
  完成都最多等 400ms。输入与拉帧已有独立 wake，不受影响。
- **实现（commit `16b5135`）**：
  - core `EventSink`：新增 `new_with_wake(capacity)`。事件仍走原来的有界队列
    （容量 256，满则计数丢弃），成功 `try_send` 后额外 `WakeSignal::notify()`
    （容量 1，只表示“去看”）。唤醒可丢/可合并，但事件本身不会被唤醒消费；
    `pane::spawn` 改用它并暴露 `PaneHandle::event_wake_receiver()`。
  - app 每个 pane 的桥接线程改为 `select!` 同时等两条通道：内容 wake →
    `redraw_pane`（原逻辑不变）；事件 wake → `invoke_from_event_loop` 后在 UI
    线程 `drain_core_events`，一次 drain 到空。pending 标志只合并“排 UI 回调”，
    回调先清 pending 再 drain，处理期间新事件还能再唤醒一次。
  - 从 `poll_events` 拆出 `drain_core_events`（标题/进度/通知/Bell/Cwd/OSC52），
    `poll_events` 保留为 **400ms 兜底 + 周期任务**：仍调用语义 drain（防唤醒
    丢失/合并窗口），并刷新插件活动 revision、SFTP 传输/元数据、超时逻辑。
  - 插件：`ActivityLog::push` 后 `WakeSignal` 通知；SFTP：`TransferJob` 的进度/
    结束事件、`SftpOpJob` 的结果写通道后通知。两类共用一个 app 级
    `app-wake` 桥接线程，UI 回调只做非阻塞 `try_recv` + revision 去重。
  - 诊断：`TERVO_EVENT_WAKE=0` 关闭事件唤醒回到纯 400ms 轮询（默认开启）；
    `TERVO_WAKE_STATS=1` 每 5s 打印 `core_event/app_event/periodic_400ms`
    计数；事件/完成日志统一加 `ts_ms=<epoch ms>`，可与 shell 的
    `date +%s%3N` 直接对差。
- **验证（Xvfb :138，`scripts/e2e_event_wake.sh`，同 binary A/B）**：
  - wake：6 个 OSC 标题 0–1ms、BEL 0–1ms、OSC52 query 1–2ms、插件活动 8–9ms、
    SFTP 上传完成 27–33ms；每个事件在日志里恰好一次（不丢/不重复），OSC52
    回包写回 PTY 且 base64 为 `wake-clip`。
  - before（`TERVO_EVENT_WAKE=0`）：标题 88–390ms、BEL 287–292ms、OSC52
    385–388ms、插件 397–400ms、SFTP 82–102ms，确认旧路径确实是 400ms 轮询。
  - 空闲 5s：两遍 `core_delta=0 app_delta=0`，`periodic_400ms=12–13`（只在
    400ms timer 上跑）；空闲 8s CPU before 111–214 / after 103–110 ticks
    （两次运行，系统负载有波动），voluntary context switches before 225–252 /
    after 263，无高频轮询/CPU 退化。
  - `cargo test -p tervo-app --bin tervo`：**300 passed**（基线 297 + 事件
    唤醒/插件/SFTP 3 例）；`cargo test -p tervo-core --lib`：**108 passed**
    （基线 107 + EventSink wake 合并/不丢事件 1 例）；workspace **518 passed**。
  - 截图 `.logs/event-wake/event-wake.png`：Tab 标题为 `wake-title-6`，SFTP
    面板可见 fake-ssh 的 `probe-one/probe-two`；日志在 `.logs/event-wake/`。
- **行为/UI**：默认路径只改变事件处理时机，不改变事件语义与界面；400ms
  timer 仍在，插件/SFTP 的 100ms 元数据 timer 也未移除。关闭/退出时桥接线程
  随通道断开自行退出，无额外清理流程。
- **限制**：`EventSink` 事件队列满仍按既有策略丢弃并计数（`dropped()` 可读），
  唤醒不等于反压；app 级 wake 合并插件/SFTP 通知，回调会同时尝试两类
  `try_recv`（幂等）；`TERVO_EVENT_WAKE` 只是 A/B 诊断开关，不写配置；
  SFTP/插件延迟实测基于假 sftp/本地插件，真实网络时延另计。

### 2026-11-06 · 渲染 damage 恢复：单行更新不再整屏重绘（feat/render-damage）

- **问题（Codex P1）**：`crates/app/src/render.rs` 的 `FrameBuffers::rotate()` 恒
  返回 true，`draw_impl` 把 `buffer_switched` 合并进 `full`，导致 `frame.damage`
  分支实际不可达。`TERVO_RENDER_PROFILE=1` 下每次按键/单行输出都是
  `grid=200x50 rows=50`，`full_redraws` 每帧 +1；双缓冲虽然省掉了 image 拷贝，
  但每帧仍整屏 raster/blend。
- **修复（render.rs）**：
  - 双缓冲行同步：`prev_changed_rows` 记录上一帧实际重绘的行；轮换到旧 buffer 时
    只把这些行从当前 buffer 复制过去，再栅格化本帧 damage 行（本帧要重绘的行跳过
    复制）。两块 buffer 始终保持“只差一帧”，旧行不丢、不会叠出花屏。
  - `full` 最终判定 = `frame.full || force_full || frame.needs_full_redraw()`
    （damage 行数 > rows/2）。首帧/resize/主题/透明度/整屏 damage/普通屏幕滚动
    走整屏；整屏后把“全部行”记为下一帧要同步的集合。
  - 选区/搜索高亮：比较新旧 `Span`，把两者覆盖的行都标脏；没有变化时不重绘。
  - 光标：app 的 `draw_without_cursor` 路径不再因为光标移动/闪烁重绘任意终端行；
    `draw()` 兼容路径记录 `drawn_cursor`，移动/改形状时擦旧画新。
  - 日志：profile 行改为打印渲染器实际 `full`、`rows`、`copy`、`full_redraws`、
    `damage_rows`，不再用 `frame.full` 冒充最终判定。
  - **字形纵向裁剪**：新增像素等价测试发现 `Ö`/`▀` 等重音/半块字形会向邻行渗
    1px；局部重绘不覆盖邻行时会留下上一帧的混合像素。`blit_bitmap_buf` 增加
    单元格纵向 clip（光标 overlay/frame 光标共用），水平裁剪逻辑不变，未改
    Sixel/彩色 emoji 路径。
- **不变量 / 安全检查**：
  - 轮换后新 buffer 必须补齐上一帧脏行；`partial_redraw_does_not_detach_shared_buffers`
    覆盖 app 持有上一帧 `Image` 时的双缓冲写入。
  - 增量结果与“同一帧全新 Renderer 整屏重绘”逐像素一致：单行文本、宽字符、
    选区出现/扩大/清除、搜索高亮、彩色背景、下划线/删除线、光标 overlay 移动、
    整屏后立刻单行、跨行字形渗漏共 12 步场景。
  - `frame.skipped`（actor 超时）仍直接保留上一帧、不轮换不重绘；下一次 core
    force_full 补全。
- **测试**：
  - `cargo test -p tervo-app --bin tervo`：**310 passed, 0 failed, 1 ignored**
    （基线 297 + 12 damage 单测 + 1 字形渗漏像素等价；ignored 是手动基准）。
  - `cargo test --workspace`：**527 passed, 1 ignored**（core 177 不变）。
  - 新增 `crates/app/src/render_damage_tests.rs`：单行只重绘 1 行、连续按键
    `full_redraws` 不增、全屏路径行数、全行 damage 走 full、主题/resize 强制
    full、选区/搜索旧新 span 补行、光标 overlay 不动帧、`draw()` 光标换行擦除、
    双缓冲不 detach、字形跨行不渗漏。
- **性能（release，同进程 A/B，200x50 / 1800x1100）**：单行输出
  **1581.4 µs → 34.7 µs（45.6×）**；连续按键 **1584.0 µs → 34.1 µs（46.4×）**；
  full 200x50 1612.8 µs、滚动 1605.9→1607.4 µs（同路径，1.00×，不退化）。
  复现：`CARGO_TARGET_DIR=$PWD/.target-render ./scripts/bench_render.sh`，
  详见 `docs/PERFORMANCE.md` §2.3。
- **GUI e2e（Xvfb :137，`scripts/e2e_render_damage.sh`，release 二进制）**：
  - 单行输出：`full=false rows=1 copy=0 full_redraws=1`，截图
    `.logs/render-damage/01-single-line.png`；
  - 搜索高亮：命中行局部重绘 `full=false rows=1`，截图中 #f9e2af 高亮 876 px，
    `02-search-highlight.png`；
  - 滚动 100 行：`full=true rows=30`，截图 `03-scroll.png`（scroll-072..100 对齐）；
  - resize 900→1090：整屏后 `full=false rows=1 copy=29`（0.15ms），
    `04-resize.png`；
  - 主题热更新到 catppuccin-mocha：整屏重绘，背景像素 tokyo 235057→0、
    mocha 0→235057，`05-theme.png`；
  - 日志断言无 panic，应用存活。
- **限制**：
  - 两块 buffer 仍在 Slint 连续两帧以上未消费时触发整块 CoW（release 100x30
    约 0.6 ms，2560x1413 更大）；正确性由 CoW 保证，但该帧不是 damage 成本。
    正常单帧节奏 `detach=false`。
  - Skia 仍整张纹理上传；本次只省 CPU raster/blend/行复制，Slint 1.18 无持久
    GPU texture API。
  - Sixel 是 Slint overlay，不写终端帧，不参与 damage；滚动历史
    （`display_offset≠0`）仍由 core 强制 full。
  - 只验证 Xvfb/llvmpipe + winit-software；真机 GPU 的 detach 频率/帧时间未验收。
