//! 调试小工具：按标题找到窗口，请求 WM 激活（_NET_ACTIVE_WINDOW），并可选移动位置。
//!
//! 用法：`xwin "Rust Tabby" [x y]`
//!
//! 注意：这里**只做激活/移动**，绝不 `XKillClient`——按标题匹配可能命中 WM 自己的
//! frame 窗口，断掉它会连带杀死窗口管理器（曾把 gnome-shell 干掉过一次）。

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as XprotoExt, EventMask, Window,
};
use x11rb::rust_connection::RustConnection;

#[path = "../xconn.rs"]
mod xconn;

fn read_prop(conn: &RustConnection, win: Window, atom: u32) -> Option<String> {
    let reply = conn.get_property(false, win, atom, AtomEnum::ANY, 0, 1024).ok()?.reply().ok()?;
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
    let title = args.get(1).cloned().unwrap_or_else(|| "Rust Tabby".into());
    let (conn, screen_num) = xconn::connect().expect("connect X");
    let root = conn.setup().roots[screen_num].root;

    let mut win = None;
    // 显式 id（`xwin 0x8600005 3000 200`）：同时存在多个同名窗口时，
    // 按标题匹配会命中错的那个，自动化脚本必须能锁定具体窗口。
    if let Some(id) = title.strip_prefix("0x").and_then(|h| u32::from_str_radix(h, 16).ok()) {
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

    if let (Some(x), Some(y)) = (
        args.get(2).and_then(|v| v.parse::<i32>().ok()),
        args.get(3).and_then(|v| v.parse::<i32>().ok()),
    ) {
        let _ = conn.configure_window(
            win,
            &x11rb::protocol::xproto::ConfigureWindowAux::new().x(x).y(y),
        );
    }

    let active = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW").unwrap().reply().unwrap().atom;
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
