# POSTMORTEM：2026-11-21 输入法/键盘连续回归

> 性质：事故复盘 + 强制规则。  
> 规则入口：[`AGENTS.md`](../AGENTS.md#输入--ime-专项2026-11-21-事故后强制)。  
> 相关：渲染残留复盘 [`POSTMORTEM_2026-11-11_RENDER_RESIDUE.md`](archive/POSTMORTEM_2026-11-11_RENDER_RESIDUE.md)。

## 1. 现象

用户升级后连续反馈：

- 终端里打字、Backspace/Delete、Ctrl+Z 全部无效；
- dsh-tui 里尤其严重，普通 shell 有时看似正常；
- Tab 会“自己改名字”；
- 多 Tab 无法左右滚动；
- 中键/关闭按钮无法关闭 Tab；
- 候选窗跟随鼠标而不是终端光标。

最终确认是 **多个独立缺陷 + 一次错误验收流程** 叠加，而不是一个“输入法 bug”。

## 2. 根因

### 2.1 preedit 残留吞掉所有按键（P0）

`2cf5ffc` 在隐藏 `TextInput.key-pressed` 里用：

```slint
if (self.preedit-text != "") {
    return EventResult.accept;
}
```

把“组合中”当成永久状态。fcitx/ibus 在焦点切换、text-input disable/enable 后可能留下非空
`preedit-text`，又不再发 `Preedit("")`/`Commit`。之后每个键——包括 Backspace、Delete、
Ctrl+Z——都在进入 `root.key-input` 之前被吞掉。

关键证据（agent `key-path`）：

- `TextInputVTable::key_event` 被调用 583 次，应用日志 `[key]` 为 0；
- 在真实 `MainWindow` 上构造 `preedit-text="ni"` 的失败测试：2701b12 红，e8deed8 绿。

修复：

- 组合态改由 winit `Ime::Preedit/Commit/Disabled` 权威维护（`ime-composing`），
  `preedit-text` 只作辅助；
- 8s watchdog 强制清组合态并重建焦点；
- 组合中 Ctrl/Alt/Meta 永不吞；
- 点终端、切/关 Tab、改名、`Ime::Enabled` 都复位组合态。

### 2.2 主窗口首轮 `Ime::Enabled` 丢失（P0）

主窗口在 `ui.show()` 之后才 `install_native_window_events`。Wayland 的第一轮
text-input enable 在 show/获焦阶段发出，handler 尚未注册，`ime_enabled` 永远为 false。

修复：

- 主窗口在 `show()` 前注册；
- 隐藏 TextInput 的 `focus-terminal()` 先切到辅助 `FocusScope` 强制 FocusOut，
  再切回 `ime` 触发 FocusIn/Enable（Slint 对同一已聚焦 item 的 `focus()` 是 no-op）。

### 2.3 Num/Caps Lock 被当成修饰键（P1）

`KeyModifiers::any()` 使用包含 lock 位的 `bits()`。Num Lock 打开时，
`modifyOtherKeys=2` 把普通字母/退格编码成：

```text
ESC[27;129;98~   # 'b'
ESC[27;129;8~    # Backspace
```

dsh-tui 不识别该序列，表现为“dsh-tui 输入/退格全坏”，而普通 shell 不请求该协议，
所以很难复现。

修复：`any()` 只看 Ctrl/Alt/Shift/Meta；lock 位仍保留在真正修饰键的转义字段里。
回归测试 `lock_keys_alone_never_escape_plain_text`。

### 2.4 快捷键抢 shell 键（P1）

- `Ctrl+W` 被硬编码为关闭 Tab，shell 的 unix-word-rubout 永远收不到；
- `Ctrl+T` 新建 Tab，shell transpose 收不到；
- `Ctrl+F` 查找，shell forward-char 收不到；
- 键位与 GNOME Terminal 默认不一致。

修复：按本机 `gsettings` 的 GNOME Terminal 默认值对齐（新 Tab `Ctrl+Shift+T`、
查找 `Ctrl+Shift+F`、切 Tab `Ctrl+PageUp/Down`、`Alt+1..0` 等）；`Ctrl+W/T/F`
留给 shell。

### 2.5 其他同批问题

- Tab 改名被 OSC 标题覆盖：新增 `title_locked`；
- 多 Tab 被压到十几像素：布局槽位 min/max 约束 + 32px 下限 + 横向滚动；
- 滚轮方向反、Bottom/HiDPI 命中错、陈旧 cursor 吞终端滚动；
- 中键关闭 Tab（Chrome 行为）。

## 3. 流程错误（比代码更值得记）

1. **没有失败测试就改代码**：违反 `AGENTS.md` 排障纪律第 3 条。
2. **一次改多层**：候选位置、焦点、preedit、快捷键、TabBar 混在一轮，症状互相掩盖。
3. **用 X11/Xephyr 验收 IME**：winit X11/XIM 不产生 text-input preedit，
   无法覆盖真实 Wayland 故障。
4. **没确认运行中的二进制 commit**：在旧 `29de0ec` 上排查了一轮。
5. **凭印象改平台快捷键**：没有先读 `gsettings`。
6. **连续两个修复无效仍继续猜**：没有及时转插桩/二分。

## 4. 以后怎么做

### 必跑回归

- stale preedit 不吞普通键 / 组合中 Ctrl/Alt/Meta 放行；
- Num Lock / Caps Lock 下普通字母、Backspace 编码不被转义；
- 快捷键与 `gsettings org.gnome.Terminal.Legacy.Keybindings` 逐项对齐；
- 真实 Wayland + fcitx5/RIME：ASCII、Backspace/Delete、Ctrl+Z、Ctrl+W、
  NumLock、CapsLock、中文上屏、切/关 Tab 后继续输入。

### 设计不变量

- 输入必须 **fail-open**：任何状态锁都不能永久阻断普通按键；
- 组合态只有一个权威源（winit IME 事件）；
- Ctrl/Alt/Meta 永远可达应用/shell；
- 任何“组合中”锁必须有超时自愈。

### 调试顺序

```text
1. 确认二进制 commit + 启动时间。
2. 开 XTERVO_KEYLOG=1 / XTERVO_IME_LOG=1，抓第一处分歧：
   winit IME 事件 -> Slint key-pressed -> root.key-input -> handle_input -> PTY bytes。
3. 没有失败测试不改；连续两次无效就停，插桩/二分。
4. 真实 Wayland IME 证据优先于 X11。
```

## 5. 本地证据（不提交）

- `.logs/wl-live-repro/`、`.logs/wl-key-test2/`：`[ime] spot` 刷屏但无 `[key]`；
- `/tmp/ime-audit/gdb*.log`：583 次 key_event、0 条 `[key]`；
- `.logs/dshtui-repro*/`：`CSI 27;129;8~` -> `[127]` 前后对比；
- `.logs/tabbar-audit/`、`.logs/middle-click-test/`、`.logs/title-lock-test/`。
