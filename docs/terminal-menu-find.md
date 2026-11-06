# 终端菜单与查找：Match case / regex、Read Only、Reset、Save Contents

> 分支：`feat/term-menu`（基于 master `d8ac7f8`）
> 范围：只补终端查找选项、只读切换、复位与内容保存；**不改** GPU/resize/背景图/
> Tab 布局/IME。验证环境：headless Xvfb `:119` + Openbox + `winit-software`。
> 复现脚本：`scripts/e2e_term_menu.sh`；截图/日志在 `.logs/term-menu/`（gitignore）。

## 1. 查找：Match case / Match regex

### core 搜索 API

`crates/core/src/pane.rs` 新增：

```rust
pub struct SearchOptions {
    pub match_case: bool,
    pub regex: bool,
}

impl PaneHandle {
    /// 兼容旧调用：纯文本；非法正则返回 None。
    pub fn search(&self, query: &str, forward: bool) -> Option<SearchHit>;
    /// 带选项；非法正则返回可展示的错误。
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
- 非法 regex 不清空整个搜索框，只清旧高亮并返回错误；UI 显示
  `invalid regex`，搜索条描边变红，完整错误写日志。
- `Ctrl::Search` 的 reply 类型改为 `Result<Option<SearchHit>, String>`；
  actor 不再把构建失败伪装成“没命中”。

### Slint 查找条

- 搜索条从 300×34 扩到 360×62，仍由 Ctrl+F 打开；键盘输入/退格/Enter/
  Shift+Enter/Esc 路径保持不变。
- 第二行是两个开关：`Match case`、`Match regex`（点击切换，Rust 侧重新搜索）。
- 键盘快捷切换：搜索框打开时 `Alt+C` / `Alt+R`（GNOME Terminal 风格）；
  不影响终端输入路径，因为这些键只在 `search_open` 分支消费。
- `no match`、`invalid regex` 状态均在条内显示；错误时边框用 danger 色。
- 匹配计数仍未做；搜索框仍不是可鼠标编辑的 TextInput（本轮不改输入架构）。

### 验证

core 集成测试 `search_supports_case_and_regex_options`：

- `ALPHA` + Match case 开 → 不命中；关 → 命中（大小写不敏感由开关决定）；
- 纯文本 `a.c` 不把 `.` 当通配符；regex 模式命中 `abc`；
- 非法 `[` 返回 `invalid regex`，且高亮被清掉。

Xvfb `:119`（`scripts/e2e_term_menu.sh`，app log）：

```text
[ui] find option match_case=true regex=false
[ui] find query="ALPHA" forward=true case=true regex=false hit=none
[ui] find option match_case=false regex=true
[ui] find query="[" forward=true case=false regex=true error=invalid regex: ...
[ui] find query="a.c" forward=true case=false regex=true hit=SearchHit { line: 1, ... }
```

截图：`find_nocase.png`、`find_case_on.png`、`find_invalid.png`、`find_regex_hit.png`。

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
# app: 231 passed（基线 229 + save path 1 + modifier artifact 1）
# core: 67 lib + 19 smoke（基线 16 + 3 新集成）等，全 workspace 387 passed

CARGO_HOME=$PWD/.cargo cargo build -p tervo-app --offline \
  --bin tervo --bin xwin --bin xclick --bin xkey
./scripts/e2e_term_menu.sh   # Xvfb :119，输出 .logs/term-menu/
```

Xvfb 脚本覆盖：查找大小写/正则/非法 regex、菜单/角标/只读键盘+粘贴+恢复、
Save Contents 路径与文件、Reset/Reset and Clear、关闭搜索/弹窗后输入仍正常。
截图见脚本末尾输出；app log 关键行：

```text
[ui] read-only pane=1 on=true
[readonly] pane=1 丢弃按键 text="b"
[readonly] pane=1 丢弃粘贴 bytes=7
[ui] save-contents ok pane=1 path=... bytes=63
[ui] reset pane=1 clear-scrollback=false
[ui] reset pane=1 clear-scrollback=true
```

## 6. 已知限制

- `Reset` 与 `Reset and Clear` 当前都会清 scrollback（上游 `reset_state` 行为）；
  文档与菜单保留两者的独立语义。
- 查找仍无匹配计数；搜索框不是可鼠标编辑的 TextInput（沿用现有键盘路径）。
- Save Contents 是显式路径弹窗，不是系统文件选择器；不做覆盖二次确认。
- Read Only 是**每 pane** 状态，不随 Tab/会话持久化（新 pane 默认关闭）；
  命令作用于焦点 pane，右键菜单作用于右键的 pane。
- Xvfb 下的 e2e 用 XTest；IME 组合键路径不在本分支范围。
