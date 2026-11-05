//! 鼠标上报编码（xterm 1000 / 1002 / 1003 + 1005 / 1006）。
//!
//! `alacritty_terminal` 只跟踪鼠标模式位（见 [`TermMode::MOUSE_MODE`]），
//! 不生成转义序列（那部分在 alacritty 的 app 层）。所以编码器归我们自己：
//! 纯函数、无 IO、逐字节可测——vim / htop / tmux 的点击是否落在正确格子全看这里。
//!
//! 协议速查：
//! - SGR 1006：`CSI < b ; x ; y M|m`（`M` 按下/移动，`m` 抬起；坐标 1-based）
//! - legacy X10：`CSI M` + `32+b` `32+x` `32+y`（单字节，坐标上限 223）
//! - UTF-8 1005：同 legacy 布局，但 x/y 用 UTF-8 编码（突破 223 限制）
//! - `b` 的位：低 2 位按键（0 左 / 1 中 / 2 右 / 3 无键），+32 表示移动，
//!   滚轮 64/65，修饰键 Shift=4、Alt=8、Ctrl=16

use alacritty_terminal::term::TermMode;

/// 修饰键。xterm 用位掩码 4/8/16 上报。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Modifiers {
    fn bits(self) -> u8 {
        u8::from(self.shift) * 4 + u8::from(self.alt) * 8 + u8::from(self.ctrl) * 16
    }
}

/// 参与上报的按键。`None` 只用于「无按键的悬停移动」（1003）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    None,
}

impl MouseButton {
    fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
            Self::None => 3,
            Self::WheelUp => 64,
            Self::WheelDown => 65,
        }
    }

    fn is_wheel(self) -> bool {
        matches!(self, Self::WheelUp | Self::WheelDown)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// 一次鼠标事件。`line` / `col` 是**视口内 0-based** 单元格坐标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: MouseButton,
    pub action: MouseAction,
    pub line: u16,
    pub col: u16,
    pub modifiers: Modifiers,
}

/// 终端是否要求接收鼠标事件（1000/1002/1003 任一开启）。
#[inline]
pub fn reporting(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_MODE)
}

/// 编码一次鼠标事件；`None` 表示该事件在当前模式下不应发给 PTY。
pub fn encode(ev: MouseEvent, mode: TermMode) -> Option<Vec<u8>> {
    if !reporting(mode) {
        return None;
    }

    // 滚轮只有「按下」语义，没有 release/motion。
    if ev.button.is_wheel() && ev.action != MouseAction::Press {
        return None;
    }

    // 1000 只报按下/抬起；1002 只在按住时报移动；1003 报一切移动。
    if ev.action == MouseAction::Motion
        && !mode.contains(TermMode::MOUSE_MOTION)
        && !(mode.contains(TermMode::MOUSE_DRAG) && ev.button != MouseButton::None)
    {
        return None;
    }

    let sgr = mode.contains(TermMode::SGR_MOUSE);
    let mut code = ev.button.code();
    if ev.action == MouseAction::Motion {
        code += 32;
    } else if ev.action == MouseAction::Release && !sgr {
        // legacy 协议没有 release 的按键号，统一用 3。
        code = 3;
    }
    code = code.saturating_add(ev.modifiers.bits());

    let x = u32::from(ev.col) + 1;
    let y = u32::from(ev.line) + 1;

    if sgr {
        let end = if ev.action == MouseAction::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes());
    }

    let mut out = Vec::with_capacity(6);
    out.extend_from_slice(b"\x1b[M");
    out.push(32u8.saturating_add(code));
    if mode.contains(TermMode::UTF8_MOUSE) {
        // 1005：坐标用 UTF-8 编码，可超过 223。
        let mut buf = [0u8; 4];
        out.extend_from_slice(char_from(32 + x).encode_utf8(&mut buf).as_bytes());
        out.extend_from_slice(char_from(32 + y).encode_utf8(&mut buf).as_bytes());
    } else {
        // X10：单字节，坐标上限 223（32 + 223 = 255）。
        out.push(32u8.saturating_add(x.min(223) as u8));
        out.push(32u8.saturating_add(y.min(223) as u8));
    }
    Some(out)
}

/// 把码点夹到合法 Unicode 标量范围内（避免极端坐标 panic）。
#[inline]
fn char_from(codepoint: u32) -> char {
    char::from_u32(codepoint).unwrap_or('\u{10ffff}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_bits_match_xterm() {
        assert_eq!(Modifiers::default().bits(), 0);
        assert_eq!(Modifiers { shift: true, alt: false, ctrl: false }.bits(), 4);
        assert_eq!(Modifiers { shift: false, alt: true, ctrl: false }.bits(), 8);
        assert_eq!(Modifiers { shift: false, alt: false, ctrl: true }.bits(), 16);
        assert_eq!(Modifiers { shift: true, alt: true, ctrl: true }.bits(), 28);
    }
}
