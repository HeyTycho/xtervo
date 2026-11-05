// SPDX-License-Identifier: GPL-3.0-only
//! 调试工具：XTest 注入按键。
//!
//! 用法：`xkey <text> [--ctrl] [--shift]`
//! 仅支持键盘映射 level 0 上的字符（小写字母、数字、常见符号）。

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest;
use x11rb::wrapper::ConnectionExt as _;

#[path = "../xconn.rs"]
mod xconn;

const PRESS: u8 = 2;
const RELEASE: u8 = 3;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut text: String = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_default();
    // 具名按键：--key backspace 等
    if let Some(pos) = args.iter().position(|a| a == "--key") {
        let name = args.get(pos + 1).map(|s| s.as_str()).unwrap_or("");
        text = match name {
            "backspace" => "\u{8}".to_string(),
            "enter" => "\n".to_string(),
            "escape" => "\u{1b}".to_string(),
            "tab" => "\t".to_string(),
            "f11" => "\u{F70E}".to_string(),
            "f2" => "\u{F705}".to_string(),
            "f12" => "\u{F70F}".to_string(),
            "up" => "\u{F700}".to_string(),
            "down" => "\u{F701}".to_string(),
            "left" => "\u{F702}".to_string(),
            "right" => "\u{F703}".to_string(),
            other => {
                eprintln!("未知按键: {other}");
                return;
            },
        };
    }
    let ctrl = args.iter().any(|a| a == "--ctrl");
    let shift = args.iter().any(|a| a == "--shift");

    let (conn, screen_num) = xconn::connect().expect("connect X");
    let setup = conn.setup();
    let root = setup.roots[screen_num].root;
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let count = max.wrapping_sub(min).wrapping_add(1);
    let reply = conn.get_keyboard_mapping(min, count).expect("mapping").reply().expect("reply");
    let per = reply.keysyms_per_keycode as usize;

    // 返回 (keycode, 是否需要 Shift)；冒号、大写字母等只在 level 1 上。
    let find = |ch: char| -> Option<(u8, bool)> {
        let sym = match ch {
            '\n' | '\r' => 0xff0d, // Return
            '\t' => 0xff09,
            '\u{8}' => 0xff08,     // BackSpace
            '\u{1b}' => 0xff1b,    // Escape
            '\u{F70E}' => 0xffc8,  // F11
            '\u{F705}' => 0xffbf,  // F2
            '\u{F70F}' => 0xffc9,  // F12
            '\u{F700}' => 0xff52,  // Up
            '\u{F701}' => 0xff54,  // Down
            '\u{F702}' => 0xff51,  // Left
            '\u{F703}' => 0xff53,  // Right
            _ => ch as u32,
        };
        reply
            .keysyms
            .iter()
            .enumerate()
            .find(|(i, s)| **s == sym && (i % per) < 4)
            .map(|(i, _)| {
                let level = (i % per) as u32;
                (min + (i / per) as u8, level > 0)
            })
    };
    let ctrl_kc = reply
        .keysyms
        .iter()
        .enumerate()
        .find(|(i, s)| i % per == 0 && **s == 0xffe3)
        .map(|(i, _)| min + (i / per) as u8);
    let shift_kc = reply
        .keysyms
        .iter()
        .enumerate()
        .find(|(i, s)| i % per == 0 && **s == 0xffe1)
        .map(|(i, _)| min + (i / per) as u8);

    let press = |kc: u8| {
        let _ = xtest::fake_input(&conn, PRESS, kc, x11rb::CURRENT_TIME, root, 0, 0, 0);
    };
    let release = |kc: u8| {
        let _ = xtest::fake_input(&conn, RELEASE, kc, x11rb::CURRENT_TIME, root, 0, 0, 0);
    };

    if ctrl {
        if let Some(kc) = ctrl_kc {
            press(kc);
        }
    }
    if shift {
        if let Some(kc) = shift_kc {
            press(kc);
        }
    }
    for ch in text.chars() {
        let found = find(ch).or_else(|| find(ch.to_ascii_lowercase()));
        let Some((kc, needs_shift)) = found else {
            eprintln!("跳过不支持的字符: {ch:?}");
            continue;
        };
        // --shift 已经按住时不再重复按。
        let press_shift = needs_shift && !shift;
        if press_shift {
            if let Some(kc) = shift_kc {
                press(kc);
                let _ = conn.flush();
                std::thread::sleep(std::time::Duration::from_millis(6));
            }
        }
        press(kc);
        let _ = conn.flush();
        std::thread::sleep(std::time::Duration::from_millis(25));
        release(kc);
        let _ = conn.flush();
        std::thread::sleep(std::time::Duration::from_millis(25));
        if press_shift {
            if let Some(kc) = shift_kc {
                release(kc);
                let _ = conn.flush();
                std::thread::sleep(std::time::Duration::from_millis(6));
            }
        }
    }
    if shift {
        if let Some(kc) = shift_kc {
            release(kc);
        }
    }
    if ctrl {
        if let Some(kc) = ctrl_kc {
            release(kc);
        }
    }
    // 关键：必须等服务器把 press/release 全部处理完再断开。
    // 只 flush 就退出会偶发丢掉最后的修饰键 release，把 Ctrl 永久卡在
    // XTEST 虚拟键盘上——整个桌面所有按键都会变成 Ctrl+xxx（“键盘失灵”）。
    let _ = conn.sync();
    println!("sent {text:?} ctrl={ctrl} shift={shift}");
}
