// SPDX-License-Identifier: GPL-3.0-only
//! 调试小工具：按标题找到窗口，请求 WM 激活（_NET_ACTIVE_WINDOW），并可选移动位置。
//!
//! 用法：`xwin "xtervo" [x y]`
//!
//! 注意：这里**只做激活/移动**，绝不 `XKillClient`——按标题匹配可能命中 WM 自己的
//! frame 窗口，断掉它会连带杀死窗口管理器（曾把 gnome-shell 干掉过一次）。

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as XprotoExt, EventMask, InputFocus, Window,
};
use x11rb::rust_connection::RustConnection;

#[path = "../xconn.rs"]
mod xconn;

fn read_prop(conn: &RustConnection, win: Window, atom: u32) -> Option<String> {
    let reply = conn
        .get_property(false, win, atom, AtomEnum::ANY, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&reply.value).to_string())
    }
}

fn find_window(conn: &RustConnection, win: Window, title: &str, depth: usize) -> Option<Window> {
    if depth > 8 {
        return None;
    }
    if let Some(name) = read_prop(conn, win, AtomEnum::WM_NAME.into()) {
        if name.contains(title) {
            return Some(win);
        }
    }
    if let Ok(cookie) = conn.intern_atom(false, b"_NET_WM_NAME") {
        if let Ok(atom) = cookie.reply() {
            if let Some(name) = read_prop(conn, win, atom.atom) {
                if name.contains(title) {
                    return Some(win);
                }
            }
        }
    }
    let tree = conn.query_tree(win).ok()?.reply().ok()?;
    for child in tree.children {
        if let Some(found) = find_window(conn, child, title, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let title = args.get(1).cloned().unwrap_or_else(|| "xtervo".into());
    let (conn, screen_num) = xconn::connect().expect("connect X");
    let root = conn.setup().roots[screen_num].root;

    let mut win = None;
    // 显式 id（`xwin 0x8600005 3000 200`）：同时存在多个同名窗口时，
    // 按标题匹配会命中错的那个，自动化脚本必须能锁定具体窗口。
    if let Some(id) = title
        .strip_prefix("0x")
        .and_then(|h| u32::from_str_radix(h, 16).ok())
    {
        win = Some(id);
    }
    for _ in 0..50 {
        if win.is_some() {
            break;
        }
        win = find_window(&conn, root, &title, 0);
        if win.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some(win) = win else {
        eprintln!("找不到窗口: {title}");
        std::process::exit(1);
    };
    println!("window = 0x{win:x}");

    // --unmap：直接 unmap Xephyr 宿主窗口（不依赖 WM，测试期间完全不可见）。
    if args.iter().any(|a| a == "--unmap") {
        let _ = conn.unmap_window(win);
        let _ = conn.flush();
        return;
    }

    // --minimize：把 Xephyr 宿主窗口最小化，避免测试弹窗占用当前 workspace。
    if args.iter().any(|a| a == "--minimize") {
        let wm_change = conn
            .intern_atom(false, b"WM_CHANGE_STATE")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let net_state = conn
            .intern_atom(false, b"_NET_WM_STATE")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let hidden = conn
            .intern_atom(false, b"_NET_WM_STATE_HIDDEN")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        // 两种都发：GNOME/Mutter 对 EWMH 的 hidden 更可靠，WM_CHANGE_STATE 兼容老 WM。
        let iconify = ClientMessageEvent::new(32, win, wm_change, [3, 0, 0, 0, 0]);
        let _ = conn.send_event(false, root, mask, iconify);
        let hide = ClientMessageEvent::new(32, win, net_state, [1, hidden, 0, 1, 0]);
        let _ = conn.send_event(false, root, mask, hide);
        let _ = conn.flush();
        return;
    }

    // --maximize / --unmaximize：按 EWMH 向 root 发 _NET_WM_STATE ClientMessage。
    // e2e 用它模拟用户点最大化（无 WM 时不会生效，脚本会检测 _NET_SUPPORTING_WM_CHECK）。
    let maximize = args.iter().any(|a| a == "--maximize");
    let unmaximize = args.iter().any(|a| a == "--unmaximize");
    if maximize || unmaximize {
        let net_state = conn
            .intern_atom(false, b"_NET_WM_STATE")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let max_vert = conn
            .intern_atom(false, b"_NET_WM_STATE_MAXIMIZED_VERT")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let max_horz = conn
            .intern_atom(false, b"_NET_WM_STATE_MAXIMIZED_HORZ")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let action = if maximize { 1 } else { 0 };
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        let event = ClientMessageEvent::new(32, win, net_state, [action, max_vert, max_horz, 1, 0]);
        let _ = conn.send_event(false, root, mask, event);
        let _ = conn.flush();
        std::thread::sleep(Duration::from_millis(150));
        return;
    }

    // --close：发送 WM_DELETE_WINDOW，等价用户点窗口关闭按钮（不杀进程）。
    if args.iter().any(|a| a == "--close") {
        let protocols = conn
            .intern_atom(false, b"WM_PROTOCOLS")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let delete = conn
            .intern_atom(false, b"WM_DELETE_WINDOW")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let event =
            ClientMessageEvent::new(32, win, protocols, [delete, x11rb::CURRENT_TIME, 0, 0, 0]);
        let _ = conn.send_event(false, win, EventMask::NO_EVENT, event);
        let _ = conn.flush();
        return;
    }

    // --focus：无 WM/Openbox 未接管时显式设 X 输入焦点，自动化按键才会落到 app。
    if args.iter().any(|a| a == "--focus") {
        let _ = conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME);
        let _ = conn.flush();
    }

    if let (Some(x), Some(y)) = (
        args.get(2).and_then(|v| v.parse::<i32>().ok()),
        args.get(3).and_then(|v| v.parse::<i32>().ok()),
    ) {
        let mut aux = x11rb::protocol::xproto::ConfigureWindowAux::new().x(x).y(y);
        // 可选第 5/6 个参数：宽高（e2e 里模拟拖拽 resize）。
        if let (Some(w), Some(h)) = (
            args.get(4).and_then(|v| v.parse::<u32>().ok()),
            args.get(5).and_then(|v| v.parse::<u32>().ok()),
        ) {
            aux = aux.width(w).height(h);
        }
        let _ = conn.configure_window(win, &aux);
    }

    let active = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let event = ClientMessageEvent::new(32, win, active, [1, x11rb::CURRENT_TIME, 0, 0, 0]);
    let _ = conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    );
    let _ = conn.set_input_focus(
        x11rb::protocol::xproto::InputFocus::PARENT,
        win,
        x11rb::CURRENT_TIME,
    );
    let _ = conn.map_window(win);
    let _ = conn.configure_window(
        win,
        &x11rb::protocol::xproto::ConfigureWindowAux::new()
            .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
    );
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
}
