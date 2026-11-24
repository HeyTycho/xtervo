# 终端基础行为：可听铃 / 滚动策略 / 滚动条 / Backspace・Delete

> 分支：`feat/term-behavior`（基于 master `d8ac7f8`）
> 范围：只补 GNOME Terminal 的 5 类基础行为；**不改** GPU、窗口 resize、背景图、
> Tab 布局、IME。
> 验证环境：headless Xvfb `:118`（`1280x800x24`，仓库内 `.perf/xvfb`），
> 不占用 `:1` / 不启动 Xephyr / 不 pkill 其他进程。

所有设置都在 **Settings → Terminal**，写入 `~/.config/xtervo/config.toml` 并即时生效
（例外：scrollback 容量只能对新 pane 生效，见 §3）。

## 1. 可听铃（audible bell）

| 配置 | 默认 | 含义 |
|---|---|---|
| `audible_bell` | `false` | BEL 时调用系统响铃 |

- **默认关闭**，避免升级后突然响铃；设置页可开。
- 与 `visual_bell`（视觉铃）完全独立：可以只闪、只响、都开、都关；
  可听铃不要求 pane 在活动 Tab（后台 Tab 的 BEL 也会响），与 GNOME Terminal 一致。
- 实现（`crates/app/src/bell.rs`）：
  - 优先向 X server 发 core `Bell` 请求（`percent = -100` = server 默认音量）。
    GNOME Terminal 走 GDK/X11 bell，声音由桌面声音主题/`xset b` 决定。
  - X11 连接懒建立、成功后在 UI 线程复用；失败清空，下次 BEL 重试。
  - 无 X11（无 `DISPLAY`）且 stderr 是终端时，写一个 BEL 字节，让启动 xtervo
    的终端模拟器响铃；两者都失败则静默并记日志。
- 同一轮事件处理里到达的多个 BEL 合并成一次响铃（避免 BEL 风暴刷爆 X server）；
  BEL 走 CoreEvent 事件唤醒立即处理，400ms timer 只兜底，日志：
  `[bell] ts_ms=... audible count=N backend=ok|unavailable`。
- 局限：X11 bell 是否真的出声取决于桌面设置（headless Xvfb 下只有请求成功，
  没有声音）；无音量/音色/每 Tab 静音配置；Wayland 下若没有 XWayland 只能走
  stderr 终端回退。

## 2. 滚动策略

| 配置 | 默认 | 含义 |
|---|---|---|
| `scroll_on_output` | `false` | 输出时是否强制回到底部 |
| `scroll_on_keystroke` | `true` | 按键后是否回到底部（GNOME 默认） |

### scroll_on_output=false（默认）

用户上滚查看历史时，新输出**只增长历史、不抢视图**。这不是新写的逻辑，而是
alacritty grid 已有语义：`display_offset != 0` 时 `Grid::scroll_up` 会同步递增
`display_offset`，视口保持在同一段内容上。xtervo 之前没有配置项，但行为已正确；
本轮补上开关和回归测试，避免以后改坏。

### scroll_on_output=true

core 在 `Actor::feed()` 解析完一批 PTY 字节后调用 `Term::scroll_display(Scroll::Bottom)`，
任何输出都把视口拉回最新。运行时切换通过 `PaneHandle::set_scroll_on_output(bool)`
（`Ctrl::SetScrollOnOutput`）异步投递，已存在的 pane 立即生效，不只影响新 pane。

### scroll_on_keystroke=true（默认）

按键编码成功、写入 PTY 后：

1. UI 读 `PaneHandle::scroll_position()` 的**无锁原子快照**判断 `offset > 0`；
2. 只有确实上滚过才调用 `PaneHandle::scroll_to_bottom()`（`Ctrl::ScrollTo(0)`），
   不向 actor 发同步请求，不阻塞 UI 或 PTY；
3. 只对物理按键/IME 提交生效，粘贴/拖放插入不走这条路径。

`scroll_on_keystroke=false` 时按键完全不改滚动位置。

core 新增 API（`crates/core/src/pane.rs`）：

```rust
impl PaneHandle {
    /// 回到底部；异步、不阻塞。调用方先用 scroll_position() 判断是否真的上滚。
    pub fn scroll_to_bottom(&self);
    /// 运行时切换输出跟随（设置页立即生效）。
    pub fn set_scroll_on_output(&self, enabled: bool);
}

pub struct PaneConfig {
    // ...
    /// 输出时是否始终跟随底部（默认 false）。
    pub scroll_on_output: bool,
}
```

## 3. scrollback_unlimited

| 配置 | 默认 | 含义 |
|---|---|---|
| `scrollback_unlimited` | `false` | true 时忽略 `scrollback`，使用安全上限 |

- 安全上限：`UNLIMITED_SCROLLBACK_LINES = 100_000` 行（与设置页 +/- 的最大值一致）。
- **内存风险必须明确**：alacritty 的 scrollback 是**预分配**的，100,000 行 ×
  120 列 × 16 B ≈ **192 MB**，并且**每个新 pane 各占一份**。因此“无限”不是真无限，
  而是明确、有界、可解释的上限；设置页提示里直接写了这个数字。
- 只对之后新建的 pane 生效（会话中扩容历史缓冲没有安全的 alacritty API）；
  已存在 pane 的 history 不变。想改回固定值时点设置页 `+/-` 会自动关闭
  `scrollback_unlimited`（从当前 `scrollback` 数值继续加减）。
- `Config::effective_scrollback()` 统一决定新 pane 的容量；CLI `--scrollback`
  仍只修改数值字段，`scrollback_unlimited=true` 时上限优先。

## 4. scrollbar_policy

| 配置 | 默认 | 含义 |
|---|---|---|
| `scrollbar_policy` | `"auto"` | `always` / `auto` / `never` |

- `auto`：只有 `history > 0`（scrollback 真的超出视口）才显示，既有行为。
- `always`：始终显示；没有历史时 thumb 占满整条轨道（表示“有滚动条但内容全在
  视口内”），有历史时按比例缩短。
- `never`：始终隐藏；滚轮回滚、键盘滚动、选区和鼠标上报不受影响。
- 仍是右侧 9px overlay：不占终端列、不改变 pane 尺寸/PTY resize；右侧最外 5px
  继续留给窗口 resize 命中带。
- 设置改动立即刷新所有 pane 的视图（`refresh_all_pane_views`），不需要新 pane。
- 几何计算抽成纯函数 `scrollbar_geometry(policy, ScrollPosition, min_thumb, track_h)`
  并单测三个策略的可见性与 always 空历史 thumb。

## 5. Backspace / Delete 编码

| 配置 | 默认 | 可选值 |
|---|---|---|
| `backspace_binding` | `"ascii-delete"` | `ascii-delete`（0x7f）/ `control-h`（0x08） |
| `delete_binding` | `"delete-sequence"` | `delete-sequence`（`ESC [3~`）/ `ascii-del`（0x7f）/ `control-h`（0x08） |

- 默认值与 GNOME Terminal 一致：Backspace 发 `0x7f`，Delete 发 `ESC [3~`。
- 只改**发给 PTY 的字节**，不改 Slint/winit 的按键识别；带 Alt 时仍保留 ESC 前缀
  （如 `Alt+Backspace` = `ESC 0x08`）。
- 特殊键优先于 kitty keyboard CSI-u：Backspace/Delete 永远按绑定字节发送。
- 配置解析大小写/空白不敏感；未知值稳定回退默认（不会因为拼错而改变按键行为），
  写回统一成规范字符串。
- 设置页改动即时生效：每次按键都从 `Config::key_bindings()` 读取，
  不需要重启或重建 pane。

## 6. 设置页 Terminal 区

Terminal 分类改成 `Flickable`（内容超出对话框时可滚动），新增：

- `Unlimited scrollback（仅新 pane）` 开关 + 100,000 行/≈192 MB 风险提示；
- `Audible bell（BEL 响系统铃）` 开关（默认关）；
- `Scroll on output（输出时回到底部）`（默认关）；
- `Scroll on keystroke（按键时回到底部）`（默认开）；
- `Scrollbar` 三档 ChoiceBtn：Always / Auto / Never；
- `Backspace` 两档、`Delete` 三档 ChoiceBtn，显示实际字节；
- 相关提示文案（视觉铃/可听铃独立、上滚不抢视图、按键才回底、只影响 PTY 字节）。

设置搜索索引新增 6 条（Audible bell / Scroll on output / Scroll on keystroke /
Unlimited scrollback / Scrollbar policy / Backspace / Delete binding），
搜索命中后跳到 Terminal 分类。

## 7. 验证

### 单元/集成测试

```bash
CARGO_HOME=/home/user/AIworkspace/rusttabbyterminal/.cargo \
CARGO_TARGET_DIR=$PWD/target cargo test --workspace
```

- `cargo test --workspace`：**392 passed / 0 failed**（12 个 test binary；基线为 382）。
- app 单测 **235 passed**（基线 **229**，新增 6）：
  - `terminal_behavior_config_parsing_and_roundtrip`（默认值/解析/落盘/上限）；
  - `backspace_binding_controls_encoding`、`delete_binding_controls_encoding`；
  - `settings_search_finds_terminal_behavior_controls`；
  - `key_scroll_policy_only_fires_when_scrolled_up_and_enabled`；
  - `scrollbar_policy_controls_visibility_and_always_thumb`。
- core 新增 4 个 smoke 集成：
  - `scroll_on_output_false_keeps_user_history_view`；
  - `scroll_on_output_true_follows_new_output`；
  - `scroll_to_bottom_resets_offset_after_keyboard_navigation`；
  - `set_scroll_on_output_takes_effect_on_existing_pane`（运行时开关）。
- 已有 `wheel_scrollback_shows_history` / `scroll_position_tracks_resize_without_blocking`
  继续通过（滚动快照仍是无锁原子读）。

### GUI e2e（Xvfb :118）

```bash
cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey
./scripts/e2e_terminal_behavior.sh
```

脚本只操作 `:118`，并只 kill 自己启动的 Xvfb/OpenBox/app。覆盖：

1. 默认编码：真实 XTest Backspace/Delete → keylog 看到 `bytes=[127]` 与
   `bytes=[27, 91, 51, 126]`；
2. `backspace_binding="control-h"` / `delete_binding="ascii-del"` →
   `bytes=[8]` / `bytes=[127]`；
3. `audible_bell=true` + 每 250ms BEL → `[bell] audible count=… backend=ok`
   （`visual_bell=false` 时也响，证明独立）；
4. 无历史 shell（`sleep`）下 `always` 截图检测到 213/213 行 thumb 像素；
   `auto` / `never` 均为 0；`never` 在 `seq 1 400` 有历史时也为 0；
5. 设置页：切 Terminal、开关 Unlimited scrollback、Audible bell、
   Scroll on output、Scrollbar Always、Backspace Control-H、Delete ASCII DEL，
   每一步都断言 config.toml 写入 + `[ui]` 回调日志；截图
   `.logs/term-behavior/settings_terminal_top.png` / `_unlimited.png` / `_mid.png` /
   `_scroll.png`（∞ 表示 unlimited 生效，100,000 行安全上限）。

### 已知限制

- 可听铃没有音量/音色/每 Tab 静音；X11 bell 是否出声由桌面声音主题决定，
  headless 下只能验证请求成功（`backend=ok`）。
- `scroll_on_output=true` 时任何 PTY 输出（包括不产生新行的 BEL/OSC）都会回底；
  这是“输出即跟随”的字面语义，默认关闭。
- `scrollback_unlimited` 只能对新 pane 生效；切换后已有 pane 仍用旧容量。
- 滚动条 `always` 在无历史时画满高 thumb，不画禁用态轨道背景（overlay 不做背景，
  避免半透明遮住右侧字符）。
- 按键回底不覆盖粘贴：`Ctrl+Shift+V`/右键粘贴/拖放走独立路径，不会因为
  `scroll_on_keystroke=true` 被拉回底部。
- 未改 GPU / resize / 背景图 / Tab 布局 / IME；没有新增布局或 resize 路径。
