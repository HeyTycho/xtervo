// SPDX-License-Identifier: GPL-3.0-only
//! 调试工具：XTest 注入鼠标事件。
//!
//! 用法：
//!   `xclick <x> <y> [button] [ctrl] [shift]`  单击（可带修饰键）
//!   `xclick <x> <y> drag <x2> <y2>`           按下->移动->松开（拖选）

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest;
use x11rb::wrapper::ConnectionExt as _;

#[path = "../xconn.rs"]
mod xconn;

const MOTION: u8 = 6;
const PRESS: u8 = 4;
const RELEASE: u8 = 5;
// XTest 的键盘事件类型与鼠标按键不同（原 shift 实现误用了 4/5，实际按的是滚轮键）。
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let x: i16 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    let y: i16 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
    let button: u8 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(1);

    let (conn, screen_num) = xconn::connect().expect("connect X");
    let root = conn.setup().roots[screen_num].root;

    let shift = args.iter().any(|a| a == "shift");
    let ctrl = args.iter().any(|a| a == "ctrl");
    let find_kc = |keysym: u32| -> Option<u8> {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let count = setup.max_keycode.saturating_sub(min).saturating_add(1);
        let reply = conn.get_keyboard_mapping(min, count).unwrap().reply().unwrap();
        let per = reply.keysyms_per_keycode as usize;
        reply
            .keysyms
            .iter()
            .position(|s| *s == keysym)
            .map(|i| min + (i / per) as u8)
    };
    let shift_kc = shift.then(|| find_kc(0xffe1)).flatten(); // Shift_L
    let ctrl_kc = ctrl.then(|| find_kc(0xffe3)).flatten(); // Control_L
    eprintln!("xclick: shift={shift} shift_kc={shift_kc:?} ctrl={ctrl} ctrl_kc={ctrl_kc:?}");
    let press_mod = |kc: Option<u8>, on: bool| {
        if let Some(kc) = kc {
            let _ = xtest::fake_input(
                &conn,
                if on { KEY_PRESS } else { KEY_RELEASE },
                kc,
                x11rb::CURRENT_TIME,
                root,
                x,
                y,
                0,
            );
            let _ = conn.flush();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };

    let drag = args.get(3).map(|s| s == "drag").unwrap_or(false);

    xtest::fake_input(&conn, MOTION, 0, x11rb::CURRENT_TIME, root, x, y, 0).expect("motion");
    conn.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));

    press_mod(ctrl_kc, true);
    press_mod(shift_kc, true);
    xtest::fake_input(&conn, PRESS, button, x11rb::CURRENT_TIME, root, x, y, 0).expect("press");
    conn.flush().unwrap();
    let hold = args.iter().any(|a| a == "hold");
    std::thread::sleep(std::time::Duration::from_millis(if hold { 400 } else { 60 }));

    if drag {
        let x2: i16 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(x);
        let y2: i16 = args.get(5).and_then(|v| v.parse().ok()).unwrap_or(y);
        let steps: i16 = std::env::var("XCLICK_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
        let interval: u64 = std::env::var("XCLICK_INTERVAL_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(16);
        for i in 1..=steps {
            let mx = x + (x2 - x) * i / steps;
            let my = y + (y2 - y) * i / steps;
            xtest::fake_input(&conn, MOTION, 0, x11rb::CURRENT_TIME, root, mx, my, 0).expect("motion");
            conn.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(interval));
        }
        xtest::fake_input(&conn, RELEASE, button, x11rb::CURRENT_TIME, root, x2, y2, 0)
            .expect("release");
    } else {
        xtest::fake_input(&conn, RELEASE, button, x11rb::CURRENT_TIME, root, x, y, 0)
            .expect("release");
    }
    // 等服务器处理完 release 再退出，避免鼠标按键卡在按下状态。
    let _ = conn.sync();
    press_mod(ctrl_kc, false);
    press_mod(shift_kc, false);
    std::thread::sleep(std::time::Duration::from_millis(20));
    println!("click {x},{y} button={button} drag={drag} root=0x{root:x}");
}
