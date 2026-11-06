# 终端菜单与查找：Match case / regex、Read Only、Reset、Save Contents

> 分支：`feat/term-menu`（基于 master `d8ac7f8`）
> 范围：只补终端查找选项、只读切换、复位与内容保存；**不改** GPU/resize/背景图/
> Tab 布局/IME。验证环境：headless Xvfb `:119` + Openbox + `winit-software`。
> 复现脚本：`scripts/e2e_term_menu.sh`；截图/日志在 `.logs/term-menu/`（gitignore）。
>
> 2026-11-06 增补（`feat/search-ux`，基于 master `9ffee39`）：查找匹配计数
> `当前/总数`、搜索框改为真 TextInput（鼠标点击/拖选/Ctrl+A/C/V）、
> Enter/Shift+Enter 从 core 计数刷新；验证脚本 `scripts/e2e_search_ux.sh`
> （独立 Xvfb `:127`），截图/日志在 `.logs/search-ux/`（gitignore）。

## 1. 查找：匹配计数、Match case / Match regex

### core 搜索 API

`crates/core/src/pane.rs`：

```rust
pub struct SearchHit {
    pub line: u16,
    pub start_col: u16,
    pub end_col: u16,
    /// 当前命中在全部匹配中的 1-based 序号；无命中时为 0。
    pub index: usize,
    /// scrollback + 视口内的匹配总数；无命中时为 0。
    pub total: usize,
}

pub struct SearchOptions {
    pub match_case: bool,
    pub regex: bool,
}

impl PaneHandle {
    /// 兼容旧调用：纯文本、大小写不敏感。
    pub fn search(&self, query: &str, forward: bool) -> Option<SearchHit>;
    /// 带选项；非法 regex 返回可展示的错误。
    pub fn search_with(
        &self,
        query: &str,
        forward: bool,
        options: SearchOptions,
    ) -> Result<Option<SearchHit>, String>;
}
```

- 纯文本模式仍先用 `regex_escape` 转义元字符；regex 模式直接把 query 交给
  alacritty 的 `RegexSearch`。
- alacritty 0.26 的 `RegexSearch::new` 按“query 是否含大写”启发式决定大小写，
  无法表达用户的显式开关。这里始终包一层内联组：
  `(?i:<query>)`（Match case 关）或 `(?-i:<query>)`（Match case 开），
  大小写行为由开关唯一决定。
- **匹配计数与命中在同一次 actor 请求里完成**：`count_search_matches` 用
  alacritty `RegexIter` 从 `grid.topmost_line()` 扫到 `bottommost_line()`，
  与 `search_next` 共用同一 `RegexSearch`/grid 语义（换行、宽字符、大小写都一致）。
  计数**不读 UI `Frame`、不扫帧文本**；无命中时 UI 显示 `0/0`。
- 下一次搜索的 origin 由命中终点/起点移动一格（正向 +1、反向 -1，
  到头/到尾按 grid 环绕）。这同时修掉了反向搜索包含当前命中、Shift+Enter
  “原地不动”的问题。
- 非法 regex 不清空整个搜索框，只清旧高亮并返回错误；UI 显示
  `invalid regex`，搜索条描边变红，完整错误写日志。
- `Ctrl::Search` 的 reply 类型为 `Result<Option<SearchHit>, String>`；
  actor 不把构建失败伪装成“没命中”。

### Slint 查找条

- 搜索条仍为 360×62，Ctrl+F 打开；查询区从只读 `Text` 换成真正的
  `TextInput`（单行），`init => self.focus()`，打开即可输入。
- `text <=> root.search-query`；`edited` 回调把 UI 文本读回 Rust 并重新搜索，
  所以键入/退格/粘贴/鼠标插入都走同一条增量搜索路径。
- 鼠标：点击聚焦并移动光标、拖选、`Ctrl+A` 全选、
  `Ctrl+C/V` 复制粘贴，全部由 Slint TextInput 内置编辑处理；这些快捷键
  **只在搜索框聚焦时生效**，不会触发终端复制/粘贴/全选。
- `Enter` 向前、`Shift+Enter` 向后，继续调用 core 搜索并刷新 `index/total`；
  没有命中时右侧状态为 `0/0`，非法 regex 为 `invalid regex`。
- `Esc` 关闭查找条并调用 `focus-terminal()` 把 IME/键盘焦点还给终端；
  焦点恢复时把 `ime_spot_dirty` 标脏，下一次轮询重发候选窗锚点。
- 第二行仍是 `Match case`、`Match regex` 两个开关（点击切换）；搜索框
  聚焦时保留 `Alt+C` / `Alt+R` 快捷切换。
- 查找条铺一层背景 `TouchArea` 吃掉空白区点击，避免穿透到终端；输入框和
  开关是后置子元素，鼠标事件优先给它们。

### 验证

core 集成测试：

- `search_supports_case_and_regex_options`：`ALPHA` + Match case 开 → 不命中；
  关 → 命中；纯文本 `a.c` 不把 `.` 当通配符，regex 模式命中 `abc`；
  非法 `[` 返回 `invalid regex` 且清高亮；同时断言 `ALPHA` 无大小写时 `total=2`。
- `search_reports_match_index_and_total_across_scrollback`：`foo foo\nbar foo`
  连续向前搜索得到 `1/3 → 2/3 → 3/3 → 1/3`，反向回到 `3/3`；
  证明计数来自 scrollback+视口，且 Shift+Enter 真的会后退。
- `search_finds_and_scrolls`：`seq 1 200` 搜 `77` 得到 `1/2`
  （77/177），滚动到命中、清除高亮。

app 单测 `search_count_label_formats_index_total_and_empty_results`：
`3/12`、无命中 `0/0`、`index=0,total>0` 的防御性 `1/5`。

Xvfb `:127`（`scripts/e2e_search_ux.sh`，app log）：

```text
[ui] find query="alpha" forward=true case=false regex=false hit=SearchHit { ... index: 2, total: 3 } count=2/3
[ui] find query="alpha" forward=true ... count=3/3
[ui] find query="alpha" forward=false ... count=2/3
[ui] find query="zzz" forward=true ... hit=none count=0/0
[ui] find option match_case=false regex=true
[ui] find query="[" forward=true case=false regex=true error=invalid regex: ...
```

截图：`count_2of3.png`、`count_3of3.png`、`count_0of0.png`、
`invalid_regex.png`、`mouse_caret.png`、`mouse_select.png`、
`terminal_focus.png`、`reopen_focus.png`。

脚本还验证：左端/右端鼠标点击分别插入字符、拖选后 `Ctrl+C` 的剪贴板内容、
退格后 `Ctrl+V` 恢复、`Ctrl+A` 后输入完全替换；Esc 后输入 `echo FOCUS_OK`
确实到达 shell，且搜索框里的 `alpha` 没有泄漏到终端输入。

## 2. Read Only

### 门禁（core，非阻塞）

`PaneHandle` 增加 `Arc<AtomicBool>`：

```rust
pub fn set_read_only(&self, on: bool);
pub fn read_only(&self) -> bool;
```

- `PaneHandle::input` / `try_input` 在读标志为 true 时直接返回，**不进入有界队列**，
  因此不会阻塞 UI。
- actor 的 `write_input` 也检查同一原子标志：切换为只读时，已经入队但尚未写的
  输入也会被丢弃。
- `wants_mouse()` / `wants_motion()` 在只读时返回 false，`mouse()` 直接丢弃；
  这样鼠标点击回到本地选区/滚轮路径，**鼠标选择/滚动仍可用**，但不会向 TUI 写
  鼠标上报。焦点事件经 `input()` 也被挡住。

### UI

- 右键菜单新增 `Read Only: ON/OFF`；命令面板新增 `terminal.toggle-read-only`。
- 状态可见：pane 右上角 `READ ONLY` 角标 + 菜单项标签 + 底部 4 秒浮层。
- app 层在编码按键前、粘贴/拖放前各挡一次（日志 + 提示），core 仍是最终
  门禁；两层都不阻塞。
- 关闭后键盘/粘贴立即恢复。

### 验证

- core `read_only_blocks_input_and_restores`：只读时 `input`/`try_input` 均不能
  写入 PTY，关闭后输入恢复。
- Xvfb `:119`：
  - 菜单 action 11 → `[ui] read-only pane=1 on=true`，截图 `readonly_on.png`；
  - 键入 `blocked` + Enter：每条按键日志 `[readonly] pane=1 丢弃按键`，
    shell 输入文件没有 blocked；
  - Ctrl+Shift+V：`[readonly] pane=1 丢弃粘贴 bytes=7`，输入文件没有 pasted；
  - 菜单切回 OFF 后键盘 `allowed` 与粘贴 `pasted` 都到达 PTY；
  - 顺带修复：Ctrl+Shift+P/C/V 的 Shift 伪字符 `\u{10}` 带 `ctrl=true` 时
    之前会漏过滤并往 PTY 写 0x10（Ctrl+P），现在无条件过滤。

## 3. Reset / Reset and Clear

### core

新增 Ctrl 消息 + 公共方法：

```rust
pub fn reset(&self);            // Ctrl::Reset
pub fn reset_and_clear(&self);  // Ctrl::ResetAndClear
```

actor 调用 alacritty `Handler::reset_state()`（RIS 语义：重置模式/光标/字符集/
滚动区/标题栈/两个 grid，并清屏），随后清选区/搜索高亮、强制整帧、刷新模式位、
光标与滚动快照。`Reset and Clear` 再显式发 `clear_screen(ClearMode::Saved)`。

**限制**：alacritty 0.26 的 `reset_state()` 本身会 `clear_history()`，所以当前
`Reset` 也会清掉 scrollback，与 `Reset and Clear` 在可观察行为上等价；两个入口
保留为独立 Ctrl 消息/命令语义，未来上游提供保留 history 的 reset 时可直接分化。
没有用关闭 pane 代替复位。

### UI / 命令

- 右键菜单：`Reset`、`Reset and Clear`；命令：`terminal.reset`、
  `terminal.reset-and-clear`（无默认快捷键，避免抢占终端键）。
- 底部浮层显示 `Terminal reset` / `Terminal reset and cleared`。
- 会话与 PTY 不关闭；复位后继续输入可正常工作（core 与 e2e 都验证）。

### 验证

- core `reset_clears_state_and_contents_include_scrollback`：Reset 后屏幕空、
  搜索高亮清空、history=0；随后仍可输入；Reset and Clear 后同样为空。
- Xvfb `:119`（命令面板）：`[ui] reset pane=1 clear-scrollback=false/true`；
  `reset_plain.png` / `reset_and_clear.png` 为空白屏幕；Reset 后键入 `again`、
  Reset and Clear 后键入 `alive` 都到达 shell。

## 4. Save Contents

Slint 1.18 没有原生文件选择器（winit 后端也没有文件对话框桥接）。按需求做
**显式路径 + 错误提示**的最小实现，不放假按钮：

- 右键菜单 `Save Contents…` / 命令 `terminal.save-contents` 打开居中弹窗；
  默认路径 `$HOME/tervo-save-<epoch>.txt`（无 HOME 时 `/tmp/tervo-save-<epoch>.txt`）。
- 路径可编辑；支持 `~` 与相对路径（相对路径按进程 cwd 解析），下方显示完整
  路径预览；写入失败在弹窗内用红色文字显示 `写入 <path> 失败：<io error>`，
  成功则关闭弹窗并底部提示 `Saved N bytes to <path>`。
- 内容来自 core `PaneHandle::contents()`：`Term::bounds_to_string` 覆盖
  **scrollback + 视口**，末尾补换行；不再只保存当前可见帧。
- 写入是同步用户动作（量级为 scrollback），core 读取最多 500ms；只在点击
  Save/Enter 时发生，不影响正常输出路径。
- 没有覆盖确认：显式路径即用户意图；这是最小实现的一部分。

### 验证

- app 单测 `save_contents_resolves_paths_and_writes_files`：`~` 展开、
  相对路径、绝对路径、空路径/无 HOME/父目录不存在错误。
- Xvfb `:119`：菜单 action 14 打开弹窗（`save_dialog.png`），Enter 保存；
  日志 `[ui] save-contents ok pane=1 path=... bytes=...`；文件包含
  `abc123`、`OUT:allowed` 等 scrollback/屏幕内容。

## 5. 测试与命令

```bash
CARGO_HOME=$PWD/.cargo cargo test --workspace --offline
# P1（feat/search-ux）实测：
# app: 282 passed（基线 281 + search count label 1）
# core: 90 lib + 64 integration（smoke 24，含新增计数顺序 1）；workspace 476 passed

CARGO_HOME=$PWD/.cargo cargo build -p tervo-app --offline \
  --bin tervo --bin xwin --bin xclick --bin xkey
# P1 查找 UX：独立 Xvfb :127，输出 .logs/search-ux/
CARGO_TARGET_DIR=$PWD/target ./scripts/e2e_search_ux.sh
```

P1 脚本覆盖：`Ctrl+F` 打开即聚焦、输入增量计数、Enter/Shift+Enter、
鼠标点击插入位置、拖选 + Ctrl+A/C/V、0 命中 `0/0`、非法 regex、Esc 后
终端输入与焦点恢复、重开搜索仍聚焦。app log 关键行见 §1 验证。

旧的 term-menu 脚本/记录保留在下方 §2–§4；其 Xvfb `:119` 截图不随 P1 重跑。

## 6. 已知限制

- `Reset` 与 `Reset and Clear` 当前都会清 scrollback（上游 `reset_state` 行为）；
  文档与菜单保留两者的独立语义。
- 查找计数是每次搜索对 scrollback + 视口的一次完整 `RegexIter` 扫描；默认
  2000 行量级开销很小，但超大 scrollback 或高频输入时会增加 UI 线程耗时
  （core ctrl 回复上限 200ms，超时 UI 按 0/0/无命中处理）。
- 只高亮当前命中，不绘制“全部匹配”高亮；计数中的其余命中不可直接点击。
- 搜索框的 IME 上屏走 Slint TextInput 自身；本分支只验证了关闭后终端
  IME/键盘焦点恢复（`focus-terminal` + spot 标脏），未在 fcitx/ibus 下
  专项测试搜索框内中文组合输入。
- 搜索框打开时点击终端仍会切走焦点；Rust 侧保留原键盘分支作为兜底，
  正常路径应以搜索框 TextInput 为准。查找条空白区点击会被背景 TouchArea
  吃掉，不会触发终端选区。
- Save Contents 是显式路径弹窗，不是系统文件选择器；不做覆盖二次确认。
- Read Only 是**每 pane** 状态，不随 Tab/会话持久化（新 pane 默认关闭）；
  命令作用于焦点 pane，右键菜单作用于右键的 pane。
- Xvfb 下的 e2e 用 XTest；输入法组合键路径不在本分支范围。
