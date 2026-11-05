//! Slint KeyEvent → PTY 字节序列。
//!
//! Slint 用私有区码点表示特殊键（见 i-slint-common/key_codes.rs），这里做一次映射。

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
}
