# 键盘 / IME 已知缺口（TERM-016）

> 记录原则：拿不到的状态**显式降级并留日志**，不伪造成“已支持”。
> 对应实现：`crates/app/src/keys.rs`、`crates/app/src/xconn.rs`、
> `crates/app/src/main.rs`（`handle_input` / IME 宿主）。

## 1. Caps Lock / Num Lock（已实现，X11）

- Slint 的 `KeyEvent.modifiers` 只有 ctrl/alt/shift/meta，没有锁键。
- X11 后端：`xconn::LockProbe` 用 `QueryPointer` 的 keymask 读 Caps（LockMask）
  与 Num Lock。Num Lock 的修饰位不写死 Mod2，而是从
  `GetModifierMapping` + `GetKeyboardMapping` 里找 `Num_Lock` keysym 的 mask。
- 每次按键做 X round-trip 会拖慢输入，因此 120ms 缓存；Caps Lock 切换后
  第一个键通常已能看到新状态。
- 内核位数按 kitty 规范进入 CSI-u（caps=64、num=128）。
- Wayland 原生 / 无 `DISPLAY` / 无 Xauthority：探针构造失败，caps/num 恒 false，
  启动/首次按键日志记录一次 `[keyboard] caps/num lock 状态不可用`。
  这是**显式降级**，不是“已知 bug 但假装有值”。

## 2. Base layout key（不可用，显式降级）

- kitty `REPORT_ALTERNATE_KEYS` 规范要求同时上报 base layout key 与 shifted key；
  Slint 只给文本与修饰键，没有 physical keycode，base layout key 无法推导。
- 现状：只上报 shifted key（`CSI code:shifted;mods u`），base layout 字段省略。
- `keys::BASE_LAYOUT_KEY_SUPPORTED = false` 是唯一真相；应用请求该 flag 时
  `note_base_layout_degraded()` 只记录一次日志，避免刷屏。
- 影响：非 US 布局下 TUI 若依赖 base key 做键位判断，只能退回 shifted 语义。

## 3. IME 缺口

已做的（X11/XIM）：

- Slint `TextInput` 隐藏宿主（2×2px、transparent）承接 XIM/fcitx/ibus 输入，
  `edited` 提交整段文本进 PTY；preedit 非空时键盘事件 `reject` 给输入法，
  避免候选拼音漏进终端。
- 隐藏宿主锚到光标格（`ime_anchor_cell`），候选窗不会固定在左上角。

未做/不可验（不伪造成已支持）：

- **Wayland text-input-v3**：无原生路径；Wayland 会话里靠 XWayland/XIM，
  候选窗位置、提交时序未验收（见矩阵 UI-TERM-001/UI-WIN-001 同一环境依赖）。
- **候选窗像素位置**：只能通过隐藏宿主位置近似，无法读取输入法窗口几何；
  多屏/缩放下的偏移不做保证。
- **preedit 内容**：不渲染到终端画面（交给输入法候选窗），因此终端里没有
  下划线/组合态预览。
- **多字符 commit 与快捷键**：IME 提交（中文、emoji 等）恒按普通文本进 PTY，
  `chord_from_key_event` 拒绝多字符/非 ASCII，不会误触快捷键。
- **死键/Compose**：由 XIM/输入法处理；终端侧看到的是最终文本，无法区分
  dead key 组合。
- **按键释放**：IME 路径没有 press/release 配对，kitty `REPORT_EVENT_TYPES`
  下 IME 提交不发送 release（这是有意的：提交不是物理按键）。
