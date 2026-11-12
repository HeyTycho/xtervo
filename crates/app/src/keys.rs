// SPDX-License-Identifier: GPL-3.0-only
//! Slint KeyEvent → PTY 字节序列 与 快捷键 chord。
//!
//! Slint 用私有区码点表示特殊键（见 i-slint-common/key_codes.rs），这里做一次映射。
//!
//! 普通 shell（应用没有请求任何键盘协议）必须与旧版本逐字节一致：
//! [`encode_mode`] 保留为唯一 legacy 路径；只有应用真的 query/set 了
//! kitty keyboard protocol 或 xterm modifyOtherKeys，才走 [`encode_key_event`]。
//!
//! 支持的协议：
//! - kitty keyboard protocol：`CSI u` 编码、progressive enhancement flags、
//!   特殊键规范序列、repeat/release 事件、shifted alternate key、associated text。
//!   flags 状态由 core 复用 alacritty 的 `TermMode`/键盘栈，见 [`KeyboardFlags`]。
//! - xterm modifyOtherKeys：`CSI > 4 ; level m`，带修饰的普通键/功能键走
//!   `CSI 27 ; mod ; code ~` 或规范功能键序列。
//!
//! 已知限制（不伪造）：
//! - Slint `KeyEvent` 只给 text + modifiers，没有 physical key；base layout key
//!   （标准 PC-101 下的键）无法可靠推导，因此只上报 shifted key。
//! - caps lock / num lock 不在 Slint modifiers 里，运行时恒为 0；编码函数支持
//!   这两个 bit（单元测试覆盖），但 UI 拿不到锁状态。

use tervo_core::KeyboardFlags;

use crate::commands::{Chord, Mods};

const UP: char = '\u{F700}';
const DOWN: char = '\u{F701}';
const LEFT: char = '\u{F702}';
const RIGHT: char = '\u{F703}';
const INSERT: char = '\u{F727}';
const HOME: char = '\u{F729}';
const END: char = '\u{F72B}';
const PAGE_UP: char = '\u{F72C}';
const PAGE_DOWN: char = '\u{F72D}';

/// Backspace 键发给 PTY 的编码（GNOME Terminal 的 backspace-binding）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackspaceBinding {
    /// 0x7f（ASCII DEL）。GNOME 默认，也是过去的行为。
    #[default]
    AsciiDelete,
    /// 0x08（Control-H）。
    ControlH,
}

impl BackspaceBinding {
    /// 解析配置字符串；大小写/空白不敏感，未知值回退默认。
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "control-h" => Self::ControlH,
            _ => Self::AsciiDelete,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AsciiDelete => "ascii-delete",
            Self::ControlH => "control-h",
        }
    }
}

/// Delete 键发给 PTY 的编码（GNOME Terminal 的 delete-binding）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeleteBinding {
    /// `ESC [ 3 ~`。GNOME 默认。
    #[default]
    DeleteSequence,
    /// 0x7f（ASCII DEL）。
    AsciiDel,
    /// 0x08（Control-H）。
    ControlH,
}

impl DeleteBinding {
    /// 解析配置字符串；大小写/空白不敏感，未知值回退默认。
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "ascii-del" => Self::AsciiDel,
            "control-h" => Self::ControlH,
            _ => Self::DeleteSequence,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeleteSequence => "delete-sequence",
            Self::AsciiDel => "ascii-del",
            Self::ControlH => "control-h",
        }
    }
}

/// 一次按键编码所需的、可配置的特殊键映射。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyBindings {
    pub backspace: BackspaceBinding,
    pub delete: DeleteBinding,
}

impl KeyBindings {
    /// Delete 的字节序列。
    #[inline]
    fn delete_seq(self) -> &'static [u8] {
        match self.delete {
            DeleteBinding::DeleteSequence => b"\x1b[3~",
            DeleteBinding::AsciiDel => &[0x7f],
            DeleteBinding::ControlH => &[0x08],
        }
    }
}

fn special(text: &str, bindings: KeyBindings) -> Option<&'static [u8]> {
    let ch = text.chars().next()?;
    let seq: &'static [u8] = match ch {
        '\u{0008}' => match bindings.backspace {
            BackspaceBinding::AsciiDelete => b"\x7f",
            BackspaceBinding::ControlH => b"\x08",
        },
        '\u{000a}' => b"\r",            // Enter
        '\u{0009}' => b"\t",            // Tab
        '\u{001b}' => b"\x1b",          // Escape
        '\u{007f}' => bindings.delete_seq(), // Delete
        INSERT => b"\x1b[2~",
        HOME => b"\x1b[H",
        END => b"\x1b[F",
        PAGE_UP => b"\x1b[5~",
        PAGE_DOWN => b"\x1b[6~",
        UP => b"\x1b[A",
        DOWN => b"\x1b[B",
        RIGHT => b"\x1b[C",
        LEFT => b"\x1b[D",
        '\u{F704}' => b"\x1bOP", // F1
        '\u{F705}' => b"\x1bOQ", // F2
        '\u{F706}' => b"\x1bOR", // F3
        '\u{F707}' => b"\x1bOS", // F4
        '\u{F708}' => b"\x1b[15~",
        '\u{F709}' => b"\x1b[17~",
        '\u{F70A}' => b"\x1b[18~",
        '\u{F70B}' => b"\x1b[19~",
        '\u{F70C}' => b"\x1b[20~",
        '\u{F70D}' => b"\x1b[21~",
        '\u{F70E}' => b"\x1b[23~",
        '\u{F70F}' => b"\x1b[24~",
        _ => return None,
    };
    Some(seq)
}

fn ctrl_byte(ch: char) -> Option<u8> {
    let c = ch.to_ascii_lowercase();
    let b = match c {
        '@' => 0x00,
        'a'..='z' => (c as u8) - b'a' + 1,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        ' ' => 0x00,
        '?' => 0x7f,
        _ => return None,
    };
    Some(b)
}

/// Slint 按键事件 → 快捷键 chord（录制与分发共用）。
///
/// 返回 None 表示这个事件不构成可绑定按键：空文本、修饰键本身（X11 会把
/// Ctrl/Shift/Alt 也作为 key-pressed 上报）、IME 多字文本等。
pub fn chord_from_key_event(text: &str, ctrl: bool, alt: bool, shift: bool) -> Option<Chord> {
    let key = key_name(text)?;
    Some(Chord {
        mods: Mods { ctrl, shift, alt, meta: false },
        key,
    })
}

fn key_name(text: &str) -> Option<String> {
    match text {
        "" => return None,
        // 0x10..=0x18：Shift/Ctrl/Alt/AltGr/CapsLock/右 Shift/右 Ctrl/Meta/右 Meta
        "\u{10}" | "\u{11}" | "\u{12}" | "\u{13}" | "\u{14}" | "\u{15}" | "\u{16}" | "\u{17}"
        | "\u{18}" => return None,
        "\u{1b}" => return Some("escape".into()),
        "\u{8}" => return Some("backspace".into()),
        "\u{7f}" => return Some("delete".into()),
        "\n" | "\r" => return Some("enter".into()),
        "\t" => return Some("tab".into()),
        " " => return Some("space".into()),
        "\u{F700}" => return Some("up".into()),
        "\u{F701}" => return Some("down".into()),
        "\u{F702}" => return Some("left".into()),
        "\u{F703}" => return Some("right".into()),
        "\u{F727}" => return Some("insert".into()),
        "\u{F729}" => return Some("home".into()),
        "\u{F72B}" => return Some("end".into()),
        "\u{F72C}" => return Some("pageup".into()),
        "\u{F72D}" => return Some("pagedown".into()),
        _ => {},
    }
    // 只接受单字符；IME 多字提交不参与快捷键。
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // F1–F24 → "f1"…"f24"（Slint: U+F704 起依次排列）
    let code = c as u32;
    if (0xF704..=0xF71B).contains(&code) {
        return Some(format!("f{}", code - 0xF704 + 1));
    }
    // 可打印 ASCII 单字符：小写化（Shift 已单独记录在 mods 里）。
    if c.is_ascii_graphic() {
        return Some(c.to_ascii_lowercase().to_string());
    }
    None
}

/// 返回要写入 PTY 的字节；None 表示忽略该键。
pub fn encode(text: &str, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    encode_mode(text, ctrl, alt, false, false, KeyBindings::default())
}

/// 和 `encode` 相同，但支持 kitty keyboard protocol 的 CSI-u 编码，
/// 以及可配置的 Backspace/Delete 编码。
///
/// 这是**普通 shell 的 legacy 路径**，只由 [`encode_key_event`] 在应用没有
/// 请求任何协议时调用；修改它必须保证字节不变。
/// xterm modified navigation/function keys。
///
/// Ctrl+Left/Right = `CSI 1;5D/C`，readline 默认映射到 backward-word/
/// forward-word；Ctrl+Delete = `CSI 3;5~`，映射到 kill-word。终端只负责把
/// 标准序列交给 shell/应用，不自己删除字符。
fn modified_navigation(
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    bindings: KeyBindings,
) -> Option<Vec<u8>> {
    if !(ctrl || alt || shift) {
        return None;
    }
    // 自定义 delete-binding 优先：ascii-del/control-h 明确要求保留原字节。
    if text == "\u{7f}" && bindings.delete != DeleteBinding::DeleteSequence {
        return None;
    }
    let (number, trailer) = match text {
        "\u{F700}" => (1, 'A'), // Up
        "\u{F701}" => (1, 'B'), // Down
        "\u{F702}" => (1, 'D'), // Left
        "\u{F703}" => (1, 'C'), // Right
        "\u{F729}" => (1, 'H'), // Home
        "\u{F72B}" => (1, 'F'), // End
        "\u{F727}" => (2, '~'), // Insert
        "\u{7f}" => (3, '~'),   // Delete
        "\u{F72C}" => (5, '~'), // PageUp
        "\u{F72D}" => (6, '~'), // PageDown
        _ => return None,
    };
    let bits = (shift as u8) | ((alt as u8) << 1) | ((ctrl as u8) << 2);
    let param = u16::from(bits) + 1;
    Some(format!("\x1b[{number};{param}{trailer}").into_bytes())
}

pub fn encode_mode(
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    kitty: bool,
    bindings: KeyBindings,
) -> Option<Vec<u8>> {
    if text.is_empty() {
        return None;
    }
    // 方向/Home/End/Insert/Delete/Page 带修饰键：xterm/GNOME 风格。
    if let Some(seq) = modified_navigation(text, ctrl, alt, shift, bindings) {
        return Some(seq);
    }
    if let Some(seq) = special(text, bindings) {
        let mut v = seq.to_vec();
        if alt {
            v.insert(0, 0x1b);
        }
        return Some(v);
    }
    // kitty keyboard protocol：可打印字符带修饰键时用 CSI u，避免 Ctrl+Shift
    // 和 Ctrl 被压成同一个控制字节。无修饰的可打印文本仍保持原样。
    if kitty {
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if !c.is_control() && (ctrl || alt || shift) {
                let cp = if c.is_ascii_alphabetic() { c.to_ascii_lowercase() as u32 } else { c as u32 };
                let mods = 1 + (shift as u32) + ((alt as u32) << 1) + ((ctrl as u32) << 2);
                return Some(format!("\x1b[{cp};{mods}u").into_bytes());
            }
        }
    }
    if ctrl {
        if let Some(ch) = text.chars().next() {
            if let Some(b) = ctrl_byte(ch) {
                let mut v = Vec::with_capacity(2);
                if alt {
                    v.push(0x1b);
                }
                v.push(b);
                return Some(v);
            }
        }
    }
    let mut v = Vec::with_capacity(text.len() + 1);
    if alt {
        v.push(0x1b);
    }
    v.extend_from_slice(text.as_bytes());
    Some(v)
}

// ---------------------------------------------------------------- 键盘协议

/// 一次按键事件的类型（kitty REPORT_EVENT_TYPES）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

/// Slint 能提供的修饰键。caps/num lock 目前拿不到（见模块文档）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Windows/Command（Slint 的 meta）。
    pub meta: bool,
    /// 运行时恒 false；保留给编码与测试。
    pub caps_lock: bool,
    /// 运行时恒 false；保留给编码与测试。
    pub num_lock: bool,
}

impl KeyModifiers {
    /// kitty 规范：shift=1, alt=2, ctrl=4, super=8, caps=64, num=128。
    /// 转义序列里还要再 +1。
    fn bits(self) -> u8 {
        (self.shift as u8)
            | ((self.alt as u8) << 1)
            | ((self.ctrl as u8) << 2)
            | ((self.meta as u8) << 3)
            | ((self.caps_lock as u8) << 6)
            | ((self.num_lock as u8) << 7)
    }

    fn any(self) -> bool {
        self.bits() != 0
    }
}

/// 特殊键的稳定身份，用于 press/release 配对（不依赖文本大小写）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpecialKey {
    Escape,
    Enter,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    /// F1–F24
    F(u8),
}

/// 按键身份：release 只回放 press 真的转发进 PTY 的键，避免快捷键的 release
/// 被误写进 shell。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyId {
    Special(SpecialKey),
    Char(u32),
}

/// 从 Slint 的 `text` 推导特殊键；只接受单个码点。
pub fn special_key(text: &str) -> Option<SpecialKey> {
    let mut chars = text.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(match ch {
        '\u{1b}' => SpecialKey::Escape,
        '\n' | '\r' => SpecialKey::Enter,
        '\t' => SpecialKey::Tab,
        '\u{0008}' => SpecialKey::Backspace,
        '\u{007f}' => SpecialKey::Delete,
        '\u{F727}' => SpecialKey::Insert,
        '\u{F729}' => SpecialKey::Home,
        '\u{F72B}' => SpecialKey::End,
        '\u{F72C}' => SpecialKey::PageUp,
        '\u{F72D}' => SpecialKey::PageDown,
        '\u{F700}' => SpecialKey::Up,
        '\u{F701}' => SpecialKey::Down,
        '\u{F702}' => SpecialKey::Left,
        '\u{F703}' => SpecialKey::Right,
        '\u{F704}'..='\u{F71B}' => SpecialKey::F((ch as u32 - 0xF704 + 1) as u8),
        _ => return None,
    })
}

/// press/release 配对用的按键身份。
///
/// ASCII 字母统一小写（Shift 只改文本，不换键）；特殊键按枚举；多字符/控制
/// 字符返回 None（这类事件不参与 release 回放）。
pub fn key_id(text: &str) -> Option<KeyId> {
    if let Some(sp) = special_key(text) {
        return Some(KeyId::Special(sp));
    }
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() || c.is_control() {
        return None;
    }
    let code = if c.is_ascii_alphabetic() { c.to_ascii_lowercase() as u32 } else { c as u32 };
    Some(KeyId::Char(code))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParsedKey {
    Special(SpecialKey),
    Char {
        /// unshifted / lower-case key code（kitty 要求）
        code: u32,
        /// shift 生效时的 shifted key
        shifted: Option<u32>,
        /// 本事件产生的可打印文本（associated text 用）
        text: Option<u32>,
    },
}

/// US 布局下 shift 后的符号 → 未按 shift 的键。
///
/// Slint 不给 physical key，这是能拿到 shifted alternate key 的唯一近似；
/// 非 US 布局/非 ASCII 只能省略 alternate。
fn unshifted_symbol(c: char) -> Option<char> {
    Some(match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        _ => return None,
    })
}

fn parse_key(text: &str, mods: KeyModifiers) -> Option<ParsedKey> {
    if let Some(sp) = special_key(text) {
        return Some(ParsedKey::Special(sp));
    }
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() || c.is_control() {
        return None;
    }
    let (code, shifted) = if c.is_ascii_alphabetic() {
        let code = c.to_ascii_lowercase() as u32;
        let shifted = mods.shift.then(|| c.to_ascii_uppercase() as u32);
        (code, shifted)
    } else if mods.shift {
        match unshifted_symbol(c) {
            Some(unshifted) => (unshifted as u32, Some(c as u32)),
            None => (c as u32, None),
        }
    } else {
        (c as u32, None)
    };
    let text = (!mods.ctrl && !mods.alt).then_some(c as u32);
    Some(ParsedKey::Char { code, shifted, text })
}

fn legacy_encode(
    text: &str,
    mods: KeyModifiers,
    bindings: KeyBindings,
) -> Option<Vec<u8>> {
    encode_mode(text, mods.ctrl, mods.alt, mods.shift, false, bindings)
}

/// 统一入口：按应用当前请求的协议编码一次按键。
///
/// - 协议完全没开启（flags 为空且 modifyOtherKeys=0）：与旧版逐字节一致，
///   release 直接丢弃；
/// - kitty flags 非空：kitty 编码优先，即使 modifyOtherKeys 同时开启；
/// - 只有 modifyOtherKeys：走 `CSI 27;mod;code~` / 带修饰功能键。
pub fn encode_key_event(
    text: &str,
    mods: KeyModifiers,
    kind: KeyEventKind,
    flags: KeyboardFlags,
    modify_other_keys: u8,
    bindings: KeyBindings,
) -> Option<Vec<u8>> {
    if text.is_empty() {
        return None;
    }
    if flags.is_empty() && modify_other_keys == 0 {
        // 普通 shell / 未请求协议：保持旧行为（repeat 等同 press）。
        if kind == KeyEventKind::Release {
            return None;
        }
        return legacy_encode(text, mods, bindings);
    }
    if !flags.is_empty() {
        return encode_kitty(text, mods, kind, flags, bindings);
    }
    // modifyOtherKeys 没有 release 语义。
    if kind == KeyEventKind::Release {
        return None;
    }
    encode_modify_other_keys(text, mods, modify_other_keys, bindings)
}

fn kitty_event_type(kind: KeyEventKind, flags: KeyboardFlags) -> Option<u8> {
    if !flags.contains(KeyboardFlags::REPORT_EVENT_TYPES) || kind == KeyEventKind::Press {
        return None;
    }
    Some(match kind {
        KeyEventKind::Press => 1,
        KeyEventKind::Repeat => 2,
        KeyEventKind::Release => 3,
    })
}

/// `CSI unicode-key-code:alternate ; modifiers:event ; text u`
fn build_csi_u(code: &str, mods_bits: u8, event: Option<u8>, text: Option<u32>) -> Vec<u8> {
    let mut out = String::with_capacity(24);
    out.push_str("\x1b[");
    out.push_str(code);
    let need_mods_field = mods_bits != 0 || event.is_some() || text.is_some();
    if need_mods_field {
        out.push(';');
        // 即使没有修饰键，event/text 也需要占位；kitty 规范默认值是 1。
        out.push_str(&(u16::from(mods_bits) + 1).to_string());
        if let Some(event) = event {
            out.push(':');
            out.push_str(&event.to_string());
        }
    }
    if let Some(text) = text {
        out.push(';');
        out.push_str(&text.to_string());
    }
    out.push('u');
    out.into_bytes()
}

/// 方向/Home/End/Insert/Delete/PageUp/PageDown/F1–F12 的 kitty 规范序列。
/// 数字/终止符与 kitty 官方 `key_encoding.c` 一致（F3 是 `CSI 13~`）。
fn build_function(number: u32, trailer: char, mods_bits: u8, event: Option<u8>) -> Vec<u8> {
    let mut out = String::with_capacity(16);
    out.push_str("\x1b[");
    if mods_bits == 0 && event.is_none() {
        // `CSI 1 A` 的 1 可以省略；其余数字必须保留。
        if number != 1 {
            out.push_str(&number.to_string());
        }
        out.push(trailer);
        return out.into_bytes();
    }
    out.push_str(&number.to_string());
    out.push(';');
    out.push_str(&(u16::from(mods_bits) + 1).to_string());
    if let Some(event) = event {
        out.push(':');
        out.push_str(&event.to_string());
    }
    out.push(trailer);
    out.into_bytes()
}

fn kitty_function(sp: SpecialKey) -> Option<(u32, char)> {
    Some(match sp {
        SpecialKey::Up => (1, 'A'),
        SpecialKey::Down => (1, 'B'),
        SpecialKey::Right => (1, 'C'),
        SpecialKey::Left => (1, 'D'),
        SpecialKey::Home => (1, 'H'),
        SpecialKey::End => (1, 'F'),
        SpecialKey::Insert => (2, '~'),
        SpecialKey::Delete => (3, '~'),
        SpecialKey::PageUp => (5, '~'),
        SpecialKey::PageDown => (6, '~'),
        SpecialKey::F(1) => (1, 'P'),
        SpecialKey::F(2) => (1, 'Q'),
        // kitty 规范：F3 用 CSI 13~（CSI R 与光标位置报告冲突）。
        SpecialKey::F(3) => (13, '~'),
        SpecialKey::F(4) => (1, 'S'),
        SpecialKey::F(5) => (15, '~'),
        SpecialKey::F(6) => (17, '~'),
        SpecialKey::F(7) => (18, '~'),
        SpecialKey::F(8) => (19, '~'),
        SpecialKey::F(9) => (20, '~'),
        SpecialKey::F(10) => (21, '~'),
        SpecialKey::F(11) => (23, '~'),
        SpecialKey::F(12) => (24, '~'),
        _ => return None,
    })
}

/// F13–F24 在 kitty 协议里用 PUA 码点（57376..57387）+ `CSI u`。
fn kitty_pua_fkey(n: u8) -> u32 {
    57376 + u32::from(n - 13)
}

fn encode_kitty(
    text: &str,
    mods: KeyModifiers,
    kind: KeyEventKind,
    flags: KeyboardFlags,
    bindings: KeyBindings,
) -> Option<Vec<u8>> {
    let Some(parsed) = parse_key(text, mods) else {
        // IME 多字符/未知控制字符：退回 legacy，不硬造协议编码。
        if kind == KeyEventKind::Release {
            return None;
        }
        return legacy_encode(text, mods, bindings);
    };
    let bits = mods.bits();
    let event = kitty_event_type(kind, flags);
    match parsed {
        ParsedKey::Special(sp) => {
            if let Some((number, trailer)) = kitty_function(sp) {
                return Some(build_function(number, trailer, bits, event));
            }
            match sp {
                SpecialKey::Escape => Some(build_csi_u("27", bits, event, None)),
                SpecialKey::Enter | SpecialKey::Tab | SpecialKey::Backspace => {
                    let report_all = flags.contains(KeyboardFlags::REPORT_ALL_KEYS_AS_ESC);
                    // Enter/Tab/Backspace：无修饰且未开 report-all 时保持 legacy
                    // 字节（kitty 规范让用户还能敲 reset），release 不发送。
                    if !report_all && bits == 0 {
                        if kind == KeyEventKind::Release {
                            return None;
                        }
                        return legacy_encode(text, mods, bindings);
                    }
                    let code = match sp {
                        SpecialKey::Enter => "13",
                        SpecialKey::Tab => "9",
                        SpecialKey::Backspace => "127",
                        _ => unreachable!(),
                    };
                    Some(build_csi_u(code, bits, event, None))
                },
                SpecialKey::F(n) if (13..=24).contains(&n) => {
                    Some(build_csi_u(&kitty_pua_fkey(n).to_string(), bits, event, None))
                },
                _ => None,
            }
        },
        ParsedKey::Char { code, shifted, text: produced } => {
            let report_all = flags.contains(KeyboardFlags::REPORT_ALL_KEYS_AS_ESC);
            let disambiguate = flags.contains(KeyboardFlags::DISAMBIGUATE_ESC_CODES);
            // kitty: 只有 disambiguate 且带 ctrl/alt（或 report-all）才把可打印键
            // 变成 CSI u；单独 Shift 仍直接发文本（否则 TUI 收不到大写字母）。
            let escape = report_all || (disambiguate && (mods.ctrl || mods.alt));
            if !escape {
                if kind == KeyEventKind::Release {
                    return None;
                }
                return legacy_encode(text, mods, bindings);
            }
            let mut code_field = code.to_string();
            if flags.contains(KeyboardFlags::REPORT_ALTERNATE_KEYS) && mods.shift {
                if let Some(shifted) = shifted {
                    code_field.push(':');
                    code_field.push_str(&shifted.to_string());
                }
            }
            let associated = if flags.contains(KeyboardFlags::REPORT_ASSOCIATED_TEXT) {
                produced
            } else {
                None
            };
            Some(build_csi_u(&code_field, bits, event, associated))
        },
    }
}

/// xterm modifyOtherKeys 的等级语义：
/// 1 = 只处理 alt/meta 等“其他修饰键”，ctrl/shift 保持旧行为；
/// 2 = 所有修饰键；3 = 无修饰键也编码。
fn modify_applies(level: u8, mods: KeyModifiers) -> bool {
    match level {
        1 => mods.alt || mods.meta,
        2 => mods.any(),
        3 => true,
        _ => false,
    }
}

fn modify_function(sp: SpecialKey) -> Option<(u32, char)> {
    Some(match sp {
        SpecialKey::Up => (1, 'A'),
        SpecialKey::Down => (1, 'B'),
        SpecialKey::Right => (1, 'C'),
        SpecialKey::Left => (1, 'D'),
        SpecialKey::Home => (1, 'H'),
        SpecialKey::End => (1, 'F'),
        SpecialKey::Insert => (2, '~'),
        SpecialKey::Delete => (3, '~'),
        SpecialKey::PageUp => (5, '~'),
        SpecialKey::PageDown => (6, '~'),
        // xterm 传统功能键编码（F3 是 CSI 1;mod R）。
        SpecialKey::F(1) => (1, 'P'),
        SpecialKey::F(2) => (1, 'Q'),
        SpecialKey::F(3) => (1, 'R'),
        SpecialKey::F(4) => (1, 'S'),
        SpecialKey::F(5) => (15, '~'),
        SpecialKey::F(6) => (17, '~'),
        SpecialKey::F(7) => (18, '~'),
        SpecialKey::F(8) => (19, '~'),
        SpecialKey::F(9) => (20, '~'),
        SpecialKey::F(10) => (21, '~'),
        SpecialKey::F(11) => (23, '~'),
        SpecialKey::F(12) => (24, '~'),
        SpecialKey::F(13) => (25, '~'),
        SpecialKey::F(14) => (26, '~'),
        SpecialKey::F(15) => (28, '~'),
        SpecialKey::F(16) => (29, '~'),
        SpecialKey::F(17) => (31, '~'),
        SpecialKey::F(18) => (32, '~'),
        SpecialKey::F(19) => (33, '~'),
        SpecialKey::F(20) => (34, '~'),
        SpecialKey::F(21) => (42, '~'),
        SpecialKey::F(22) => (43, '~'),
        SpecialKey::F(23) => (44, '~'),
        SpecialKey::F(24) => (45, '~'),
        _ => return None,
    })
}

fn encode_modify_other_keys(
    text: &str,
    mods: KeyModifiers,
    level: u8,
    bindings: KeyBindings,
) -> Option<Vec<u8>> {
    let Some(parsed) = parse_key(text, mods) else {
        return legacy_encode(text, mods, bindings);
    };
    let bits = mods.bits();
    match parsed {
        ParsedKey::Special(sp) => {
            if matches!(
                sp,
                SpecialKey::Escape
                    | SpecialKey::Enter
                    | SpecialKey::Tab
                    | SpecialKey::Backspace
            ) {
                if !modify_applies(level, mods) {
                    return legacy_encode(text, mods, bindings);
                }
                let code = match sp {
                    SpecialKey::Escape => 27,
                    SpecialKey::Enter => 13,
                    SpecialKey::Tab => 9,
                    SpecialKey::Backspace => 8,
                    _ => unreachable!(),
                };
                return Some(format!("\x1b[27;{};{code}~", u16::from(bits) + 1).into_bytes());
            }
            let Some((number, trailer)) = modify_function(sp) else {
                return legacy_encode(text, mods, bindings);
            };
            if bits == 0 {
                // 无修饰时功能键本来就是转义序列，保持 legacy（SS3/CSI ~）。
                return legacy_encode(text, mods, bindings);
            }
            return Some(build_function(number, trailer, bits, None));
        },
        ParsedKey::Char { code, shifted, .. } => {
            if !modify_applies(level, mods) {
                return legacy_encode(text, mods, bindings);
            }
            let mut out_bits = bits;
            let code = if mods.shift {
                // xterm 在 escaped 形式里用 shifted 字符码（Shift+a → 65）。
                let shifted = shifted.unwrap_or(code);
                if level <= 1 && !mods.ctrl {
                    // level 1 的可打印键“已经带 shift”，modifier 里不再重复。
                    out_bits &= !1;
                }
                shifted
            } else {
                code
            };
            if out_bits == 0 && level < 3 {
                return legacy_encode(text, mods, bindings);
            }
            Some(format!("\x1b[27;{};{code}~", u16::from(out_bits) + 1).into_bytes())
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool) -> KeyModifiers {
        KeyModifiers { ctrl, alt, shift, ..KeyModifiers::default() }
    }

    fn kitty(bits: u8) -> KeyboardFlags {
        KeyboardFlags::from_bits_truncate(bits)
    }

    const DISAMBIGUATE: u8 = 0b00001;
    const EVENT_TYPES: u8 = 0b00010;
    const ALTERNATE: u8 = 0b00100;
    const REPORT_ALL: u8 = 0b01000;
    const ASSOCIATED: u8 = 0b10000;

    #[test]
    fn printable_passthrough() {
        assert_eq!(encode("a", false, false).unwrap(), b"a");
        assert_eq!(encode("中", false, false).unwrap(), "中".as_bytes());
    }

    #[test]
    fn control_codes() {
        assert_eq!(encode("c", true, false).unwrap(), vec![0x03]);
        assert_eq!(encode("d", true, false).unwrap(), vec![0x04]);
    }

    #[test]
    fn specials() {
        assert_eq!(encode("\n", false, false).unwrap(), b"\r");
        assert_eq!(encode("\u{0008}", false, false).unwrap(), b"\x7f");
        // Alt 前缀不改变大小写：Slint 已给出带 shift 的文本
        assert_eq!(encode("A", false, true).unwrap(), b"\x1bA");
        assert_eq!(encode("\u{F700}", false, false).unwrap(), b"\x1b[A");
    }

    #[test]
    fn kitty_keyboard_csi_u() {
        let b = KeyBindings::default();
        assert_eq!(encode_mode("A", false, false, true, true, b).unwrap(), b"\x1b[97;2u");
        assert_eq!(encode_mode("c", true, false, false, true, b).unwrap(), b"\x1b[99;5u");
        assert_eq!(encode_mode("c", true, true, true, true, b).unwrap(), b"\x1b[99;8u");
        assert_eq!(encode_mode("a", false, false, false, true, b).unwrap(), b"a");
        // 非 kitty 模式保持旧控制字节
        assert_eq!(encode_mode("c", true, false, false, false, b).unwrap(), vec![0x03]);
    }

    #[test]
    fn backspace_binding_controls_encoding() {
        let ascii = KeyBindings::default();
        assert_eq!(
            encode_mode("\u{8}", false, false, false, false, ascii).unwrap(),
            vec![0x7f],
            "默认 ascii-delete → 0x7f"
        );
        let control_h =
            KeyBindings { backspace: BackspaceBinding::ControlH, ..KeyBindings::default() };
        assert_eq!(
            encode_mode("\u{8}", false, false, false, false, control_h).unwrap(),
            vec![0x08],
            "control-h → 0x08"
        );
        // Alt 前缀保留，特殊键编码不被 kitty 模式改写。
        assert_eq!(
            encode_mode("\u{8}", false, true, false, false, control_h).unwrap(),
            vec![0x1b, 0x08]
        );
        assert_eq!(
            encode_mode("\u{8}", false, false, false, true, control_h).unwrap(),
            vec![0x08]
        );
        // 配置字符串解析：大小写/空白不敏感，未知值回退默认。
        assert_eq!(BackspaceBinding::from_config(" CONTROL-H "), BackspaceBinding::ControlH);
        assert_eq!(BackspaceBinding::from_config("ascii-delete"), BackspaceBinding::AsciiDelete);
        assert_eq!(BackspaceBinding::from_config("bogus"), BackspaceBinding::AsciiDelete);
        assert_eq!(BackspaceBinding::from_config("control-h").as_str(), "control-h");
    }

    #[test]
    fn delete_binding_controls_encoding() {
        let default = KeyBindings::default();
        assert_eq!(
            encode_mode("\u{7f}", false, false, false, false, default).unwrap(),
            b"\x1b[3~",
            "默认 delete-sequence"
        );
        assert_eq!(
            encode_mode("\u{7f}", true, false, false, false, default).unwrap(),
            b"\x1b[3;5~",
            "Ctrl+Delete 是 modified Delete；readline 默认 kill-word"
        );
        assert_eq!(
            encode_mode("\u{7f}", false, true, false, false, default).unwrap(),
            b"\x1b[3;3~",
            "Alt+Delete 用 CSI 修饰符，不再拼 ESC 前缀"
        );
        assert_eq!(
            encode_mode("\u{7f}", true, false, true, false, default).unwrap(),
            b"\x1b[3;6~",
            "Ctrl+Shift+Delete"
        );
        assert_eq!(
            encode_mode("\u{F702}", true, false, false, false, default).unwrap(),
            b"\x1b[1;5D",
            "Ctrl+Left = backward-word"
        );
        assert_eq!(
            encode_mode("\u{F703}", true, false, false, false, default).unwrap(),
            b"\x1b[1;5C",
            "Ctrl+Right = forward-word"
        );
        assert_eq!(
            encode_mode("\u{F702}", false, true, false, false, default).unwrap(),
            b"\x1b[1;3D",
            "Alt+Left = backward-word"
        );
        assert_eq!(
            encode_mode("\u{F702}", false, false, true, false, default).unwrap(),
            b"\x1b[1;2D",
            "Shift+Left modified sequence"
        );
        assert_eq!(
            encode_mode("\u{F729}", true, false, false, false, default).unwrap(),
            b"\x1b[1;5H",
            "Ctrl+Home"
        );
        let ascii_del =
            KeyBindings { delete: DeleteBinding::AsciiDel, ..KeyBindings::default() };
        assert_eq!(encode_mode("\u{7f}", false, false, false, false, ascii_del).unwrap(), vec![0x7f]);
        let control_h =
            KeyBindings { delete: DeleteBinding::ControlH, ..KeyBindings::default() };
        assert_eq!(encode_mode("\u{7f}", false, false, false, false, control_h).unwrap(), vec![0x08]);
        assert_eq!(
            encode_mode("\u{7f}", false, true, false, false, ascii_del).unwrap(),
            vec![0x1b, 0x7f],
            "Alt+Delete 仍带 ESC 前缀"
        );
        assert_eq!(DeleteBinding::from_config(" ASCII-DEL "), DeleteBinding::AsciiDel);
        assert_eq!(DeleteBinding::from_config("control-h"), DeleteBinding::ControlH);
        assert_eq!(DeleteBinding::from_config("bogus"), DeleteBinding::DeleteSequence);
        assert_eq!(DeleteBinding::from_config("delete-sequence").as_str(), "delete-sequence");
    }

    #[test]
    fn key_event_to_chord() {
        let c = chord_from_key_event("t", true, false, true).unwrap();
        assert_eq!(c.canonical(), "ctrl+shift+t");
        let c = chord_from_key_event("T", true, false, true).unwrap();
        assert_eq!(c.canonical(), "ctrl+shift+t", "Shift 文本大小写应归一");

        let c = chord_from_key_event("\u{F70E}", false, false, false).unwrap();
        assert_eq!(c.canonical(), "f11");
        let c = chord_from_key_event("\u{F703}", true, false, true).unwrap();
        assert_eq!(c.canonical(), "ctrl+shift+right");
        let c = chord_from_key_event("\t", true, false, false).unwrap();
        assert_eq!(c.canonical(), "ctrl+tab");
        let c = chord_from_key_event(" ", false, true, false).unwrap();
        assert_eq!(c.canonical(), "alt+space");
        assert_eq!(chord_from_key_event("\u{1b}", false, false, false).unwrap().key, "escape");
        assert_eq!(chord_from_key_event("\u{8}", false, false, false).unwrap().key, "backspace");
        assert_eq!(chord_from_key_event("\u{7f}", false, false, false).unwrap().key, "delete");

        // "+" 的规范格式是 `ctrl++`，和 parser 约定一致
        let c = chord_from_key_event("+", true, false, true).unwrap();
        assert_eq!(c.canonical(), "ctrl+shift++");
        assert_eq!(crate::commands::Chord::parse(&c.canonical()).unwrap(), c);
    }

    #[test]
    fn key_event_rejects_modifiers_and_unsupported_text() {
        for artifact in ["\u{10}", "\u{11}", "\u{12}", "\u{13}", "\u{17}"] {
            assert_eq!(chord_from_key_event(artifact, true, false, false), None);
        }
        assert_eq!(chord_from_key_event("", false, false, false), None);
        // IME 多字提交 / 非 ASCII 暂不作为快捷键
        assert_eq!(chord_from_key_event("你好", false, false, false), None);
        assert_eq!(chord_from_key_event("ab", false, false, false), None);
        assert_eq!(chord_from_key_event("你", false, false, false), None);
        // F25 不在支持范围
        assert_eq!(chord_from_key_event("\u{F71C}", false, false, false), None);
    }

    // ------------------------------------------------------------ protocol

    #[test]
    fn no_protocol_is_byte_identical_to_legacy() {
        let b = KeyBindings::default();
        let cases: &[(&str, bool, bool, bool)] = &[
            ("a", false, false, false),
            ("A", false, false, true),
            ("c", true, false, false),
            ("c", true, false, true),
            ("1", false, false, false),
            ("!", false, false, true),
            ("\u{1b}", false, false, false),
            ("\n", false, false, false),
            ("\t", false, false, false),
            ("\u{8}", false, false, false),
            ("\u{7f}", false, false, false),
            ("\u{F700}", false, false, false),
            ("\u{F703}", true, false, true),
            ("\u{F704}", false, false, false),
            ("\u{F70F}", false, false, false),
            ("中", false, false, false),
        ];
        for &(text, ctrl, alt, shift) in cases {
            let old = encode_mode(text, ctrl, alt, shift, false, b);
            let new = encode_key_event(
                text,
                mods(ctrl, alt, shift),
                KeyEventKind::Press,
                KeyboardFlags::default(),
                0,
                b,
            );
            assert_eq!(old, new, "legacy bytes changed for {text:?}");
            let repeat = encode_key_event(
                text,
                mods(ctrl, alt, shift),
                KeyEventKind::Repeat,
                KeyboardFlags::default(),
                0,
                b,
            );
            assert_eq!(old, repeat, "repeat must be same bytes when no protocol");
        }
        // 未请求协议时 release 永远不产生字节。
        assert_eq!(
            encode_key_event(
                "a",
                mods(false, false, false),
                KeyEventKind::Release,
                KeyboardFlags::default(),
                0,
                b
            ),
            None
        );
    }

    #[test]
    fn kitty_disambiguate_printable_modifiers() {
        let b = KeyBindings::default();
        let f = kitty(DISAMBIGUATE);
        let enc = |text: &str, m: KeyModifiers| {
            encode_key_event(text, m, KeyEventKind::Press, f, 0, b).unwrap()
        };
        assert_eq!(enc("a", mods(false, false, false)), b"a", "plain text unchanged");
        assert_eq!(enc("A", mods(false, false, true)), b"A", "shift-only is still text");
        assert_eq!(enc("c", mods(true, false, false)), b"\x1b[99;5u");
        assert_eq!(enc("c", mods(true, false, true)), b"\x1b[99;6u");
        assert_eq!(enc("a", mods(false, true, false)), b"\x1b[97;3u");
        assert_eq!(enc("A", mods(false, true, true)), b"\x1b[97;4u");
        assert_eq!(enc("c", mods(true, true, false)), b"\x1b[99;7u");
        // Ctrl+Space：kitty 要求用 32 而不是 NUL。
        assert_eq!(enc(" ", mods(true, false, false)), b"\x1b[32;5u");
        // 多字符文本仍是 legacy，不做协议编码。
        assert_eq!(enc("你好", mods(false, false, false)), "你好".as_bytes());
    }

    #[test]
    fn kitty_report_all_and_associated_text() {
        let b = KeyBindings::default();
        let f = kitty(REPORT_ALL | ALTERNATE | ASSOCIATED);
        let enc = |text: &str, m: KeyModifiers| {
            encode_key_event(text, m, KeyEventKind::Press, f, 0, b).unwrap()
        };
        assert_eq!(enc("a", mods(false, false, false)), b"\x1b[97;1;97u");
        assert_eq!(enc("A", mods(false, false, true)), b"\x1b[97:65;2;65u");
        assert_eq!(enc("c", mods(true, false, false)), b"\x1b[99;5u", "ctrl 不附文本");
        assert_eq!(enc("!", mods(false, false, true)), b"\x1b[49:33;2;33u");
        // report-all 时 Enter/Tab/Backspace 也变成 CSI u。
        assert_eq!(enc("\n", mods(false, false, false)), b"\x1b[13u");
        assert_eq!(enc("\t", mods(false, false, false)), b"\x1b[9u");
        assert_eq!(enc("\u{8}", mods(false, false, false)), b"\x1b[127u");
    }

    #[test]
    fn kitty_event_types_repeat_release() {
        let b = KeyBindings::default();
        let f = kitty(DISAMBIGUATE | EVENT_TYPES);
        let enc = |text: &str, m: KeyModifiers, kind| {
            encode_key_event(text, m, kind, f, 0, b).unwrap()
        };
        // press 省略事件类型（kitty 默认 1）。
        assert_eq!(enc("c", mods(true, false, false), KeyEventKind::Press), b"\x1b[99;5u");
        assert_eq!(
            enc("c", mods(true, false, false), KeyEventKind::Repeat),
            b"\x1b[99;5:2u"
        );
        assert_eq!(
            enc("c", mods(true, false, false), KeyEventKind::Release),
            b"\x1b[99;5:3u"
        );
        // Enter/Tab/Backspace 在未开 report-all 时没有 release，press 仍 legacy。
        assert_eq!(
            encode_key_event(
                "\n",
                mods(false, false, false),
                KeyEventKind::Release,
                f,
                0,
                b
            ),
            None
        );
        assert_eq!(
            encode_key_event("\n", mods(false, false, false), KeyEventKind::Press, f, 0, b)
                .unwrap(),
            b"\r"
        );
        // 普通文本键不产生 CSI-u release（避免与 press 文本不配对）。
        assert_eq!(
            encode_key_event("a", mods(false, false, false), KeyEventKind::Release, f, 0, b),
            None
        );
    }

    #[test]
    fn kitty_special_keys_use_canonical_sequences() {
        let b = KeyBindings::default();
        let f = kitty(DISAMBIGUATE);
        let enc = |text: &str, m: KeyModifiers| {
            encode_key_event(text, m, KeyEventKind::Press, f, 0, b).unwrap()
        };
        assert_eq!(enc("\u{F700}", mods(false, false, false)), b"\x1b[A");
        assert_eq!(enc("\u{F700}", mods(false, true, false)), b"\x1b[1;3A");
        assert_eq!(enc("\u{F700}", mods(true, false, false)), b"\x1b[1;5A");
        assert_eq!(enc("\u{F703}", mods(true, false, true)), b"\x1b[1;6C");
        assert_eq!(enc("\u{F729}", mods(false, false, true)), b"\x1b[1;2H");
        assert_eq!(enc("\u{F72B}", mods(false, false, false)), b"\x1b[F");
        assert_eq!(enc("\u{F727}", mods(false, false, false)), b"\x1b[2~");
        assert_eq!(enc("\u{F72C}", mods(false, false, true)), b"\x1b[5;2~");
        // Delete 在 kitty 模式必须用规范 CSI 3~，不受 DeleteBinding 影响。
        let ascii_del = KeyBindings { delete: DeleteBinding::AsciiDel, ..b };
        assert_eq!(
            encode_key_event(
                "\u{7f}",
                mods(false, false, false),
                KeyEventKind::Press,
                f,
                0,
                ascii_del
            )
            .unwrap(),
            b"\x1b[3~"
        );
        // F1–F4 / F5 / F13–F24
        assert_eq!(enc("\u{F704}", mods(false, false, false)), b"\x1b[P");
        assert_eq!(enc("\u{F704}", mods(false, true, false)), b"\x1b[1;3P");
        assert_eq!(enc("\u{F706}", mods(false, false, false)), b"\x1b[13~");
        assert_eq!(enc("\u{F708}", mods(false, false, false)), b"\x1b[15~");
        assert_eq!(enc("\u{F710}", mods(false, false, false)), b"\x1b[57376u");
        assert_eq!(enc("\u{F71B}", mods(true, false, true)), b"\x1b[57387;6u");
        // 方向键 release 带事件类型。
        let fe = kitty(DISAMBIGUATE | EVENT_TYPES);
        assert_eq!(
            encode_key_event(
                "\u{F700}",
                mods(false, false, false),
                KeyEventKind::Release,
                fe,
                0,
                b
            )
            .unwrap(),
            b"\x1b[1;1:3A"
        );
    }

    #[test]
    fn kitty_alternate_keys() {
        let b = KeyBindings::default();
        let f = kitty(DISAMBIGUATE | ALTERNATE);
        assert_eq!(
            encode_key_event(
                "A",
                mods(true, false, true),
                KeyEventKind::Press,
                f,
                0,
                b
            )
            .unwrap(),
            b"\x1b[97:65;6u"
        );
        // Shift+1 的 shifted key 是 '!'（US 布局近似）。
        assert_eq!(
            encode_key_event(
                "!",
                mods(true, false, true),
                KeyEventKind::Press,
                f,
                0,
                b
            )
            .unwrap(),
            b"\x1b[49:33;6u"
        );
    }

    #[test]
    fn kitty_has_priority_over_modify_other_keys() {
        let b = KeyBindings::default();
        let out = encode_key_event(
            "a",
            mods(true, false, false),
            KeyEventKind::Press,
            kitty(DISAMBIGUATE),
            2,
            b,
        )
        .unwrap();
        assert_eq!(out, b"\x1b[97;5u");
        assert!(!out.starts_with(b"\x1b[27;"));
    }

    #[test]
    fn modify_other_keys_levels() {
        let b = KeyBindings::default();
        let enc = |text: &str, m: KeyModifiers, level: u8| {
            encode_key_event(text, m, KeyEventKind::Press, KeyboardFlags::default(), level, b)
                .unwrap()
        };
        // level 2：所有修饰键。
        assert_eq!(enc("a", mods(true, false, false), 2), b"\x1b[27;5;97~");
        assert_eq!(enc("a", mods(false, true, false), 2), b"\x1b[27;3;97~");
        // xterm escaped 形式用 shifted 字符码。
        assert_eq!(enc("A", mods(false, false, true), 2), b"\x1b[27;2;65~");
        assert_eq!(enc("A", mods(true, false, true), 2), b"\x1b[27;6;65~");
        // level 1：ctrl/shift 保持旧行为，alt 才编码。
        assert_eq!(enc("a", mods(true, false, false), 1), vec![0x01]);
        assert_eq!(enc("A", mods(false, false, true), 1), b"A");
        assert_eq!(enc("a", mods(false, true, false), 1), b"\x1b[27;3;97~");
        // level 0/未知：legacy。
        assert_eq!(enc("a", mods(true, false, false), 0), vec![0x01]);
        assert_eq!(enc("a", mods(true, false, false), 9), vec![0x01]);
    }

    #[test]
    fn modify_other_keys_special_keys() {
        let b = KeyBindings::default();
        let enc = |text: &str, m: KeyModifiers, level: u8| {
            encode_key_event(text, m, KeyEventKind::Press, KeyboardFlags::default(), level, b)
                .unwrap()
        };
        // xterm level 2：alt/shift Tab 都走 CSI 27。
        assert_eq!(enc("\t", mods(false, true, false), 2), b"\x1b[27;3;9~");
        assert_eq!(enc("\t", mods(false, false, true), 2), b"\x1b[27;2;9~");
        assert_eq!(enc("\u{1b}", mods(false, true, false), 2), b"\x1b[27;3;27~");
        // 功能键：带修饰用规范序列，无修饰保持 legacy。
        assert_eq!(enc("\u{F700}", mods(false, false, true), 2), b"\x1b[1;2A");
        assert_eq!(enc("\u{F708}", mods(true, false, false), 2), b"\x1b[15;5~");
        assert_eq!(enc("\u{F704}", mods(false, true, false), 2), b"\x1b[1;3P");
        assert_eq!(enc("\u{F700}", mods(false, false, false), 2), b"\x1b[A");
        // level 1 的 shift+Tab 不编码（“usual shift works”）。
        assert_eq!(enc("\t", mods(false, false, true), 1), b"\t");
    }

    #[test]
    fn key_identity_matches_press_and_release() {
        assert_eq!(key_id("a"), Some(KeyId::Char(97)));
        assert_eq!(key_id("A"), Some(KeyId::Char(97)));
        assert_eq!(key_id("中"), Some(KeyId::Char('中' as u32)));
        assert_eq!(key_id("\u{F700}"), Some(KeyId::Special(SpecialKey::Up)));
        assert_eq!(key_id("\u{F71B}"), Some(KeyId::Special(SpecialKey::F(24))));
        assert_eq!(key_id("ab"), None);
        assert_eq!(key_id("\u{1}"), None);
    }

    #[test]
    fn modifier_and_lock_bits() {
        let m = KeyModifiers {
            ctrl: true,
            alt: true,
            shift: true,
            meta: true,
            caps_lock: true,
            num_lock: true,
        };
        assert_eq!(m.bits(), 1 | 2 | 4 | 8 | 64 | 128);
        // kitty 要求 +1 编码。
        let b = KeyBindings::default();
        let out = encode_key_event(
            "a",
            m,
            KeyEventKind::Press,
            kitty(DISAMBIGUATE),
            0,
            b,
        )
        .unwrap();
        assert_eq!(out, b"\x1b[97;208u");
    }
}
