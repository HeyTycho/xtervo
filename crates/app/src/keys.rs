// SPDX-License-Identifier: GPL-3.0-only
//! Slint KeyEvent → PTY 字节序列 与 快捷键 chord。
//!
//! Slint 用私有区码点表示特殊键（见 i-slint-common/key_codes.rs），这里做一次映射。

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

fn special(text: &str) -> Option<&'static [u8]> {
    let ch = text.chars().next()?;
    let seq: &'static [u8] = match ch {
        '\u{0008}' => b"\x7f",          // Backspace
        '\u{000a}' => b"\r",            // Enter
        '\u{0009}' => b"\t",            // Tab
        '\u{001b}' => b"\x1b",          // Escape
        '\u{007f}' => b"\x1b[3~",       // Delete
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
    if text.is_empty() {
        return None;
    }
    if let Some(seq) = special(text) {
        let mut v = seq.to_vec();
        if alt {
            v.insert(0, 0x1b);
        }
        return Some(v);
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
