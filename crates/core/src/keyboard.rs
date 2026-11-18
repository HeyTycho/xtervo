// SPDX-License-Identifier: GPL-3.0-only
//! 键盘协议状态：kitty progressive enhancement flags 与 xterm XTMODKEYS。
//!
//! 为什么不用 alacritty 0.26 自带的键盘栈？
//!
//! alacritty 已经能解析 `CSI ? u` / `CSI = flags ; mode u` / `CSI > flags u` /
//! `CSI < n u` 并写进 `TermMode`，但 0.26.0 有两处与 kitty 规范不一致：
//! - `report_keyboard_mode()` 回的是**栈顶**，而 `CSI = 3 u`（set 不 push）后
//!   栈仍为空，会错误回 `CSI ? 0 u`；
//! - push 只把新 flags 压栈，不保存“push 前的当前 flags”，因此
//!   set-then-push-then-pop 会丢掉 set 的值（与规范要求的恢复语义不符）。
//!
//! 所以 actor 用本模块旁路扫描同一批字节，维护自己的 current flags + 栈；
//! 状态快照交给 `PaneHandle`，UI 用它决定编码。alacritty 的 kitty 支持关闭，
//! 避免重复回包；modifyOtherKeys 同理（它的 Handler 只有空实现）。
//!
//! 解析只接收字节切片，所有序列都有长度上限，非法输入一律忽略，绝不 panic。

use alacritty_terminal::term::TermMode;

/// kitty keyboard protocol 的 progressive enhancement flags（低 5 bit）。
///
/// 数值与 kitty 规范一致，可直接对应 `CSI = flags ; mode u` 的 flags 参数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardFlags(u8);

impl KeyboardFlags {
    /// `CSI u` 转义（Esc / ctrl+key / alt+key / ctrl+alt+key / shift+alt+key）。
    pub const DISAMBIGUATE_ESC_CODES: Self = Self(0b0000_0001);
    /// 上报 press / repeat / release 事件类型。
    pub const REPORT_EVENT_TYPES: Self = Self(0b0000_0010);
    /// 额外上报 shifted key / base layout key。
    pub const REPORT_ALTERNATE_KEYS: Self = Self(0b0000_0100);
    /// 所有按键都用转义序列上报（含可打印键）。
    pub const REPORT_ALL_KEYS_AS_ESC: Self = Self(0b0000_1000);
    /// 在 `CSI u` 里附带按键产生的文本（与 REPORT_ALL_KEYS_AS_ESC 配合）。
    pub const REPORT_ASSOCIATED_TEXT: Self = Self(0b0001_0000);
    /// 我们实际实现的全部 flags；set/query 只会出现这些位。
    pub const ALL: Self = Self(0b0001_1111);

    /// 截断未知 bit，保证永远不会向应用声明未实现的 flags。
    pub const fn from_bits_truncate(bits: u8) -> Self {
        Self(bits & Self::ALL.0)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    /// 从 alacritty 的 `TermMode` 抽取 kitty flags（兼容/测试用）。
    pub fn from_term_mode(mode: TermMode) -> Self {
        let mut bits = 0u8;
        if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
            bits |= Self::DISAMBIGUATE_ESC_CODES.0;
        }
        if mode.contains(TermMode::REPORT_EVENT_TYPES) {
            bits |= Self::REPORT_EVENT_TYPES.0;
        }
        if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
            bits |= Self::REPORT_ALTERNATE_KEYS.0;
        }
        if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
            bits |= Self::REPORT_ALL_KEYS_AS_ESC.0;
        }
        if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
            bits |= Self::REPORT_ASSOCIATED_TEXT.0;
        }
        Self(bits)
    }
}

impl std::ops::BitOr for KeyboardFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// modifyOtherKeys 等级。0 = 关闭；1 = 仅 alt/meta 等“其他修饰键”；
/// 2 = 所有修饰键；3 = 无修饰键也编码（xterm 的扩展等级）。
pub const MODIFY_OTHER_KEYS_MAX_LEVEL: u8 = 3;

/// 每块屏幕独立的 kitty 键盘状态（main / alternate screen）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScreenKeyboard {
    flags: u8,
    /// push 时保存的“push 前 flags”，pop 恢复；最近的在末尾。
    stack: Vec<u8>,
    /// 与 kitty 的 stack 上限对齐，防止恶意应用无限 push。
    stack_limit: usize,
}

impl ScreenKeyboard {
    const MAX_STACK: usize = 32;

    fn new() -> Self {
        Self { flags: 0, stack: Vec::new(), stack_limit: Self::MAX_STACK }
    }

    fn reset(&mut self) {
        self.flags = 0;
        self.stack.clear();
    }
}

/// 扫描器的产物：更新状态或向 PTY 回包。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeyboardAction {
    /// kitty set/push/pop/备用屏切换后的当前 flags。
    SetKeyboardFlags(u8),
    /// `CSI > 4 ; n m` 或 reset：新的 modifyOtherKeys 等级。
    SetModifyOtherKeys(u8),
    /// 需要写回 PTY 的应答字节。
    Reply(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    /// 已进入 CSI，正在收集参数/中间字节（最多 [`MAX_CSI_BYTES`]）。
    Csi,
    /// OSC：直到 BEL / ST。
    Osc,
    OscEscape,
    /// DCS / SOS / PM / APC：直到 ST。
    String,
    StringEscape,
}

/// 单条 CSI 只可能用来设置键盘模式，参数不会长；超长序列直接放弃，
/// 但仍然继续吃到 final byte，避免影响后续字节。
const MAX_CSI_BYTES: usize = 64;

/// 流式扫描键盘协议 CSI + 维护 kitty flags 栈。
///
/// `enabled == false`（配置 `keyboard_protocol = "off"`）时完全不解析、不回包。
/// UTF-8 前导字节 → 后续续字节数；非前导字节返回 None。
fn utf8_lead_len(b: u8) -> Option<u8> {
    match b {
        0xC2..=0xDF => Some(1),
        0xE0..=0xEF => Some(2),
        0xF0..=0xF4 => Some(3),
        _ => None,
    }
}

pub(crate) struct KeyboardScanner {
    enabled: bool,
    /// xterm modifyOtherKeys 等级（全局，不随备用屏切换）。
    level: u8,
    main: ScreenKeyboard,
    alt: ScreenKeyboard,
    on_alt_screen: bool,
    state: State,
    csi: Vec<u8>,
    csi_overflow: bool,
    /// 当前 UTF-8 多字节序列还剩几个续字节。终端输出是 UTF-8；0x90/0x9B 等
    /// 续字节不是 C1 控制符，不能进入 String/Csi 状态吞掉后续 kitty 序列。
    utf8_remaining: u8,
}

impl KeyboardScanner {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            level: 0,
            main: ScreenKeyboard::new(),
            alt: ScreenKeyboard::new(),
            on_alt_screen: false,
            state: State::Ground,
            csi: Vec::new(),
            csi_overflow: false,
            utf8_remaining: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn level(&self) -> u8 {
        self.level
    }

    #[cfg(test)]
    pub(crate) fn flags(&self) -> u8 {
        self.screen().flags
    }

    /// 复位（Ctrl::Reset / RIS）：两块屏幕的 flags 栈与 modifyOtherKeys 都归零。
    pub(crate) fn reset(&mut self) {
        self.level = 0;
        self.main.reset();
        self.alt.reset();
        self.on_alt_screen = false;
        self.state = State::Ground;
        self.csi.clear();
        self.csi_overflow = false;
        self.utf8_remaining = 0;
    }

    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<KeyboardAction>) {
        if !self.enabled {
            return;
        }
        for &b in bytes {
            if self.utf8_remaining > 0 {
                if (0x80..=0xBF).contains(&b) {
                    self.utf8_remaining -= 1;
                    continue;
                }
                // 非法 UTF-8：放弃这个序列，按普通字节继续。
                self.utf8_remaining = 0;
            }
            if let Some(n) = utf8_lead_len(b) {
                self.utf8_remaining = n;
                continue;
            }
            self.state = match self.state {
                State::Ground => match b {
                    0x1b => State::Escape,
                    0x9d => State::Osc,
                    0x9b => {
                        self.begin_csi();
                        State::Csi
                    },
                    0x90 | 0x98 | 0x9e | 0x9f => State::String,
                    0x9c => State::Ground,
                    _ => State::Ground,
                },
                State::Escape => match b {
                    b'[' => {
                        self.begin_csi();
                        State::Csi
                    },
                    b']' => State::Osc,
                    b'P' | b'X' | b'^' | b'_' => State::String,
                    // RIS：终端复位。
                    b'c' => {
                        self.reset_all(out);
                        State::Ground
                    },
                    0x1b => State::Escape,
                    _ => State::Ground,
                },
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        match b {
                            b'm' => self.finish_csi_m(out),
                            b'u' => self.finish_csi_u(out),
                            b'h' | b'l' => self.finish_csi_mode(b, out),
                            _ => {},
                        }
                        State::Ground
                    } else {
                        if !self.csi_overflow {
                            if self.csi.len() < MAX_CSI_BYTES {
                                self.csi.push(b);
                            } else {
                                self.csi_overflow = true;
                                self.csi.clear();
                            }
                        }
                        State::Csi
                    }
                },
                State::Osc => match b {
                    0x07 => State::Ground,
                    0x1b => State::OscEscape,
                    0x9c => State::Ground,
                    _ => State::Osc,
                },
                State::OscEscape => match b {
                    b'\\' => State::Ground,
                    0x1b => State::OscEscape,
                    _ => State::Osc,
                },
                State::String => match b {
                    0x1b => State::StringEscape,
                    0x9c => State::Ground,
                    _ => State::String,
                },
                State::StringEscape => match b {
                    b'\\' => State::Ground,
                    0x1b => State::StringEscape,
                    _ => State::String,
                },
            };
        }
    }

    fn begin_csi(&mut self) {
        self.csi.clear();
        self.csi_overflow = false;
    }

    fn take_csi(&mut self) -> Option<Vec<u8>> {
        if self.csi_overflow {
            self.csi_overflow = false;
            self.csi.clear();
            return None;
        }
        // xterm 允许参数中间有空格；我们只关心十进制参数。
        let raw = std::mem::take(&mut self.csi);
        Some(raw.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect())
    }

    fn reset_all(&mut self, out: &mut Vec<KeyboardAction>) {
        let old = self.screen().flags;
        let old_level = self.level;
        self.main.reset();
        self.alt.reset();
        self.on_alt_screen = false;
        self.level = 0;
        if old != 0 {
            out.push(KeyboardAction::SetKeyboardFlags(0));
        }
        if old_level != 0 {
            out.push(KeyboardAction::SetModifyOtherKeys(0));
        }
    }

    fn screen(&self) -> &ScreenKeyboard {
        if self.on_alt_screen {
            &self.alt
        } else {
            &self.main
        }
    }

    fn screen_mut(&mut self) -> &mut ScreenKeyboard {
        if self.on_alt_screen {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    /// 更新当前屏幕 flags；只在数值变化时产生 action。
    fn set_current_flags(&mut self, flags: u8, out: &mut Vec<KeyboardAction>) {
        let flags = KeyboardFlags::from_bits_truncate(flags).bits();
        let old = self.screen().flags;
        if old != flags {
            self.screen_mut().flags = flags;
            out.push(KeyboardAction::SetKeyboardFlags(flags));
        }
    }

    /// `CSI = flags ; mode u`（mode: 1 replace / 2 union / 3 difference）。
    fn finish_csi_u(&mut self, out: &mut Vec<KeyboardAction>) {
        let Some(raw) = self.take_csi() else { return };
        let Some((&marker, rest)) = raw.split_first() else { return };
        match marker {
            b'?' => {
                // kitty query：回当前屏幕的真实 flags（不是栈顶）。
                let flags = self.screen().flags;
                out.push(KeyboardAction::Reply(format!("\x1b[?{flags}u").into_bytes()));
            },
            b'=' => {
                let Some((flags, mode)) = parse_flags_and_mode(rest) else { return };
                let old = self.screen().flags;
                let new = match mode {
                    2 => old | flags,
                    3 => old & !flags,
                    _ => flags,
                };
                self.set_current_flags(new, out);
            },
            b'>' => {
                let Some(flags) = parse_flags(rest) else { return };
                let current = self.screen().flags;
                {
                    let screen = self.screen_mut();
                    if screen.stack.len() >= screen.stack_limit {
                        screen.stack.remove(0);
                    }
                    screen.stack.push(current);
                    screen.flags = flags;
                }
                if current != flags {
                    out.push(KeyboardAction::SetKeyboardFlags(flags));
                }
            },
            b'<' => {
                let count = match rest {
                    b"" => 1,
                    raw => match parse_decimal(raw) {
                        Some(v) => v,
                        None => return,
                    },
                };
                let mut flags = self.screen().flags;
                {
                    let screen = self.screen_mut();
                    for _ in 0..count {
                        flags = screen.stack.pop().unwrap_or(0);
                    }
                }
                self.set_current_flags(flags, out);
            },
            _ => {},
        }
    }

    /// `CSI > 4 ; n m` / `CSI ? 4 m`（modifyOtherKeys）。
    fn finish_csi_m(&mut self, out: &mut Vec<KeyboardAction>) {
        let Some(raw) = self.take_csi() else { return };
        let Some((&marker, rest)) = raw.split_first() else { return };
        match marker {
            b'>' => {
                // XTMODKEYS: Pp=4 是 modifyOtherKeys。
                let mut params = rest.split(|&b| b == b';');
                let Some(pp) = params.next().and_then(parse_decimal) else { return };
                if pp != 4 {
                    return;
                }
                // 未给 Pv 时 xterm 语义是 reset；不少应用把 `CSI > 4 m` 当作
                // “启用默认等级”。这里按后者处理（level 2），`; 0` 明确关闭。
                let level = match params.next() {
                    None | Some(b"") => 2,
                    Some(raw) => match parse_decimal(raw) {
                        Some(v @ 0..=MODIFY_OTHER_KEYS_MAX_LEVEL) => v,
                        // 非法/不支持等级：忽略，保持旧状态（不崩溃）。
                        _ => return,
                    },
                };
                if level != self.level {
                    self.level = level;
                    out.push(KeyboardAction::SetModifyOtherKeys(level));
                }
            },
            b'?' => {
                // XTQMODKEYS: `CSI ? 4 m` 查询当前等级。
                let pp = rest.split(|&b| b == b';').next().and_then(parse_decimal);
                if pp == Some(4) {
                    out.push(KeyboardAction::Reply(
                        format!("\x1b[>4;{}m", self.level).into_bytes(),
                    ));
                }
            },
            _ => {},
        }
    }

    /// `CSI ? 1049 h/l`（以及 1047/47）切换备用屏；两块屏幕的键盘栈独立。
    fn finish_csi_mode(&mut self, final_byte: u8, out: &mut Vec<KeyboardAction>) {
        let Some(raw) = self.take_csi() else { return };
        let Some((&marker, rest)) = raw.split_first() else { return };
        if marker != b'?' {
            return;
        }
        let Some(first) = rest.split(|&b| b == b';').next().and_then(parse_mode_number) else {
            return;
        };
        if !matches!(first, 47 | 1047 | 1049) {
            return;
        }
        let on_alt = final_byte == b'h';
        if self.on_alt_screen != on_alt {
            self.on_alt_screen = on_alt;
            // 切屏后生效 flags 可能不同，必须让 UI 立刻看到新快照。
            out.push(KeyboardAction::SetKeyboardFlags(self.screen().flags));
        }
    }
}

fn parse_decimal(raw: &[u8]) -> Option<u8> {
    if raw.is_empty() {
        return None;
    }
    let mut value: u16 = 0;
    for &b in raw {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as u16)?;
        if value > u8::MAX as u16 {
            return None;
        }
    }
    Some(value as u8)
}

/// private mode 编号（1049 等）可以超过 u8。
fn parse_mode_number(raw: &[u8]) -> Option<u16> {
    if raw.is_empty() {
        return None;
    }
    let mut value: u16 = 0;
    for &b in raw {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as u16)?;
    }
    Some(value)
}

/// flags 参数：缺省/空 = 0；非法 = None（忽略整条序列）。
fn parse_flags(rest: &[u8]) -> Option<u8> {
    let raw = rest.split(|&b| b == b';').next().unwrap_or(b"");
    let flags = if raw.is_empty() { 0 } else { parse_decimal(raw)? };
    Some(KeyboardFlags::from_bits_truncate(flags).bits())
}

/// `flags ; mode`：flags 缺省 0，mode 缺省 1。
fn parse_flags_and_mode(rest: &[u8]) -> Option<(u8, u8)> {
    let mut params = rest.split(|&b| b == b';');
    let flags = match params.next().unwrap_or(b"") {
        b"" => 0,
        raw => KeyboardFlags::from_bits_truncate(parse_decimal(raw)?).bits(),
    };
    let mode = match params.next().unwrap_or(b"") {
        b"" => 1,
        raw => parse_decimal(raw)?,
    };
    Some((flags, mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::TermMode;

    fn scan(bytes: &[u8]) -> Vec<KeyboardAction> {
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        scanner.feed(bytes, &mut out);
        out
    }

    fn set(flags: u8) -> KeyboardAction {
        KeyboardAction::SetKeyboardFlags(flags)
    }

    fn reply_kitty(flags: u8) -> KeyboardAction {
        KeyboardAction::Reply(format!("\x1b[?{flags}u").into_bytes())
    }

    #[test]
    fn utf8_continuation_does_not_swallow_kitty_set() {
        // А = D0 90；旧扫描器把 0x90 当 DCS 进入 String，吞掉后面的 kitty set。
        assert_eq!(scan(b"\xD0\x90\x1b[>7u"), vec![set(7)]);
        // 你 = E4 BD A0；逐字节分块同样不能吞。
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        for b in b"\xE4\xBD\xA0\x1b[>3u" {
            scanner.feed(&[*b], &mut out);
        }
        assert_eq!(out, vec![set(3)]);
    }

    #[test]
    fn flags_from_term_mode_are_mapped_not_shifted() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_EVENT_TYPES
            | TermMode::REPORT_ASSOCIATED_TEXT;
        let flags = KeyboardFlags::from_term_mode(mode);
        assert_eq!(flags.bits(), 0b10011);
        assert!(flags.contains(KeyboardFlags::DISAMBIGUATE_ESC_CODES));
        assert!(flags.contains(KeyboardFlags::REPORT_EVENT_TYPES));
        assert!(!flags.contains(KeyboardFlags::REPORT_ALL_KEYS_AS_ESC));
        assert!(flags.contains(KeyboardFlags::REPORT_ASSOCIATED_TEXT));
        assert_eq!(KeyboardFlags::from_bits_truncate(0xff).bits(), KeyboardFlags::ALL.bits());
        assert!(KeyboardFlags::default().is_empty());
    }

    #[test]
    fn kitty_set_and_query_uses_current_flags_not_stack_top() {
        // set 不 push：query 必须回 3（alacritty 0.26 会错回 0）。
        assert_eq!(
            scan(b"\x1b[=3;1u\x1b[?u"),
            vec![set(3), reply_kitty(3)]
        );
        // flags 参数截断到实际支持的 5 bit。
        assert_eq!(
            scan(b"\x1b[=255u\x1b[?u"),
            vec![set(31), reply_kitty(31)]
        );
    }

    #[test]
    fn kitty_set_union_and_difference() {
        assert_eq!(
            scan(b"\x1b[=1;1u\x1b[=2;2u\x1b[=1;3u"),
            vec![set(1), set(3), set(2)]
        );
        // 未知 mode 按 replace（与 kitty 默认一致）。
        assert_eq!(scan(b"\x1b[=7;9u"), vec![set(7)]);
    }

    #[test]
    fn kitty_push_pop_restores_previous_flags() {
        // set 3 → push 7 → pop：规范要求恢复 3（不是 0）。
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        scanner.feed(b"\x1b[=3;1u\x1b[>7u\x1b[?u", &mut out);
        assert_eq!(out, vec![set(3), set(7), reply_kitty(7)]);
        assert_eq!(scanner.flags(), 7);
        out.clear();
        scanner.feed(b"\x1b[<1u\x1b[?u", &mut out);
        assert_eq!(out, vec![set(3), reply_kitty(3)]);
        assert_eq!(scanner.flags(), 3);
        // 空栈 pop 会重置为 0。
        out.clear();
        scanner.feed(b"\x1b[<5u\x1b[?u", &mut out);
        assert_eq!(out, vec![set(0), reply_kitty(0)]);
    }

    #[test]
    fn kitty_push_stack_is_bounded_and_evicts_oldest() {
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        scanner.feed(b"\x1b[>1u", &mut out);
        for _ in 0..(ScreenKeyboard::MAX_STACK + 4) {
            scanner.feed(b"\x1b[>2u", &mut out);
        }
        out.clear();
        // 连续 pop 到底：栈里最多 MAX_STACK 项，不影响解析/不 panic。
        scanner.feed(b"\x1b[<255u", &mut out);
        assert!(matches!(out.last(), Some(KeyboardAction::SetKeyboardFlags(_))));
        assert_eq!(scanner.flags(), 0);
    }

    #[test]
    fn kitty_alt_screen_has_independent_state() {
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        // 主屏 set 1 → 进备用屏（flags 变 0）→ 备用屏 set 3 → query → 离开 → query。
        scanner.feed(b"\x1b[=1;1u\x1b[?1049h\x1b[=3;1u\x1b[?u", &mut out);
        assert_eq!(out, vec![set(1), set(0), set(3), reply_kitty(3)]);
        out.clear();
        scanner.feed(b"\x1b[?1049l\x1b[?u", &mut out);
        assert_eq!(out, vec![set(1), reply_kitty(1)]);
    }

    #[test]
    fn modify_other_keys_levels() {
        assert_eq!(
            scan(b"\x1b[>4;2m"),
            vec![KeyboardAction::SetModifyOtherKeys(2)],
            "标准 level 2"
        );
        assert_eq!(scan(b"\x1b[>4m"), vec![KeyboardAction::SetModifyOtherKeys(2)]);
        assert_eq!(scan(b"\x1b[>4;1m"), vec![KeyboardAction::SetModifyOtherKeys(1)]);
        assert!(scan(b"\x1b[>4;0m").is_empty());
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        scanner.feed(b"\x1b[>4;2m\x1b[>4;0m", &mut out);
        assert_eq!(
            out,
            vec![
                KeyboardAction::SetModifyOtherKeys(2),
                KeyboardAction::SetModifyOtherKeys(0)
            ]
        );
        assert_eq!(scan(b"\x1b[>4;3m"), vec![KeyboardAction::SetModifyOtherKeys(3)]);
        // 非法等级 / 别的资源 / 别的 final：不改变状态。
        assert!(scan(b"\x1b[>4;9m").is_empty());
        assert!(scan(b"\x1b[>4;xm").is_empty());
        assert!(scan(b"\x1b[>2;2m").is_empty());
        assert!(scan(b"\x1b[>4;2H").is_empty());
        assert_eq!(scan(b"\x1b[> 4 ; 2 m"), vec![KeyboardAction::SetModifyOtherKeys(2)]);
    }

    #[test]
    fn modify_other_keys_query_replies_with_current_level() {
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        scanner.feed(b"\x1b[>4;1m\x1b[?4m", &mut out);
        assert_eq!(
            out,
            vec![
                KeyboardAction::SetModifyOtherKeys(1),
                KeyboardAction::Reply(b"\x1b[>4;1m".to_vec())
            ]
        );
    }

    #[test]
    fn sequences_split_across_chunks() {
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        scanner.feed(b"\x1b[=3", &mut out);
        scanner.feed(b";1u\x1b[?u", &mut out);
        assert_eq!(out, vec![set(3), reply_kitty(3)]);

        out.clear();
        scanner.feed(b"\x1b[>4", &mut out);
        scanner.feed(b";2", &mut out);
        scanner.feed(b"m\x1b[?4", &mut out);
        scanner.feed(b"m", &mut out);
        assert_eq!(
            out,
            vec![
                KeyboardAction::SetModifyOtherKeys(2),
                KeyboardAction::Reply(b"\x1b[>4;2m".to_vec())
            ]
        );
    }

    #[test]
    fn ignores_sequences_nested_in_osc_and_strings() {
        assert!(scan(b"\x1b]0;title\x1b[>4;2m\x07").is_empty());
        assert!(scan(b"\x1b]0;t\x1b[=3u\x1b[?u\x07").is_empty());
        assert!(scan(b"\x1bPq\x1b[>4;2m\x1b\\").is_empty());
        assert_eq!(
            scan(b"\x1b]0;x\x07\x1b[>4;2m"),
            vec![KeyboardAction::SetModifyOtherKeys(2)]
        );
    }

    #[test]
    fn ris_resets_and_disabled_scanner_is_inert() {
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        scanner.feed(b"\x1b[>4;2m\x1b[=3u", &mut out);
        assert_eq!(scanner.level(), 2);
        assert_eq!(scanner.flags(), 3);
        out.clear();
        scanner.feed(b"\x1bc", &mut out);
        assert_eq!(scanner.level(), 0);
        assert_eq!(scanner.flags(), 0);
        assert!(out.contains(&KeyboardAction::SetKeyboardFlags(0)));
        assert!(out.contains(&KeyboardAction::SetModifyOtherKeys(0)));

        let mut off = KeyboardScanner::new(false);
        let mut out = Vec::new();
        off.feed(b"\x1b[>4;2m\x1b[?4m\x1b[=7u\x1b[?u", &mut out);
        assert!(out.is_empty());
        assert_eq!(off.level(), 0);
        assert_eq!(off.flags(), 0);
    }

    #[test]
    fn oversized_csi_does_not_leak_state() {
        let mut scanner = KeyboardScanner::new(true);
        let mut out = Vec::new();
        scanner.feed(b"\x1b[>4;", &mut out);
        scanner.feed(&[b'1'; MAX_CSI_BYTES + 8], &mut out);
        scanner.feed(b"m", &mut out);
        assert!(out.is_empty());
        scanner.feed(b"\x1b[>4;2m", &mut out);
        assert_eq!(out, vec![KeyboardAction::SetModifyOtherKeys(2)]);
    }

    #[test]
    fn invalid_kitty_sequences_are_ignored() {
        assert!(scan(b"\x1b[=xu").is_empty());
        assert!(scan(b"\x1b[=3;xu").is_empty());
        assert!(scan(b"\x1b[>xu").is_empty());
        assert!(scan(b"\x1b[<xu").is_empty());
        // restore-cursor 的 CSI u 不是键盘协议。
        assert!(scan(b"\x1b[u").is_empty());
    }
}
