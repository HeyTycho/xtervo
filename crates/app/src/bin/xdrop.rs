// SPDX-License-Identifier: GPL-3.0-only
//! 调试工具：用 XDND 协议把本地文件"拖"到指定窗口（headless e2e 用）。
//!
//! 用法：`xdrop <window-id> <local-x> <local-y> <path>...`
//! 只实现 Xdnd version 5 + text/uri-list 的最小 source 端，供 Slint/winit
//! 的 DropArea / `DroppedFile` 路径做端到端验证；不执行任何真实拖拽。

use std::thread::sleep;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SelectionNotifyEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

#[path = "../xconn.rs"]
mod xconn;

fn intern(conn: &RustConnection, name: &[u8]) -> u32 {
    conn.intern_atom(false, name).expect("intern_atom").reply().expect("atom reply").atom
}

fn send_msg(conn: &RustConnection, target: Window, kind: u32, data: [u32; 5]) {
    let ev = ClientMessageEvent::new(32, target, kind, data);
    conn.send_event(false, target, EventMask::NO_EVENT, ev).expect("send XDND message");
    conn.flush().expect("flush");
}

fn serve_selection(
    conn: &RustConnection,
    req: x11rb::protocol::xproto::SelectionRequestEvent,
    text_uri_list: u32,
    fallback_property: u32,
    data: &str,
) {
    let property = if req.property == 0 { fallback_property } else { req.property };
    conn.change_property(
        PropMode::REPLACE,
        req.requestor,
        property,
        text_uri_list,
        8,
        data.len() as u32,
        data.as_bytes(),
    )
    .expect("set selection property");
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property,
    };
    conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify)
        .expect("send SelectionNotify");
    conn.flush().expect("flush");
}

/// 等待并处理 SelectionRequest；返回响应次数。
fn pump(conn: &RustConnection, text_uri_list: u32, xdnd_selection: u32, data: &str, timeout: Duration) -> usize {
    let deadline = Instant::now() + timeout;
    let mut served = 0;
    while Instant::now() < deadline {
        match conn.poll_for_event() {
            Ok(Some(x11rb::protocol::Event::SelectionRequest(req))) => {
                serve_selection(conn, req, text_uri_list, xdnd_selection, data);
                served += 1;
            },
            Ok(Some(x11rb::protocol::Event::ClientMessage(msg))) => {
                // 目标可能回 XdndStatus / XdndFinished；打印便于诊断。
                eprintln!("[xdrop] client-message type=0x{:x} data={:?}", msg.type_, msg.data);
            },
            Ok(Some(_)) => {},
            Ok(None) => sleep(Duration::from_millis(5)),
            Err(e) => {
                eprintln!("[xdrop] poll error: {e}");
                break;
            },
        }
    }
    served
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("用法: xdrop <window-id> <local-x> <local-y> <path>...");
        std::process::exit(2);
    }
    let target: Window = {
        let s = args[0].trim();
        if let Some(hex) = s.strip_prefix("0x") {
            u32::from_str_radix(hex, 16).expect("window id")
        } else {
            s.parse().expect("window id")
        }
    };
    let local_x: i16 = args[1].parse().expect("x");
    let local_y: i16 = args[2].parse().expect("y");
    let paths = &args[3..];

    let (conn, screen_num) = xconn::connect().expect("connect X");
    let root = conn.setup().roots[screen_num].root;

    let xdnd_enter = intern(&conn, b"XdndEnter");
    let xdnd_position = intern(&conn, b"XdndPosition");
    let xdnd_drop = intern(&conn, b"XdndDrop");
    let xdnd_selection = intern(&conn, b"XdndSelection");
    let text_uri_list = intern(&conn, b"text/uri-list");
    let action_copy = intern(&conn, b"XdndActionCopy");

    let source: Window = conn.generate_id().expect("generate_id");
    conn.create_window(
        0,
        source,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(EventMask::NO_EVENT),
    )
    .expect("create source window");
    conn.set_selection_owner(source, xdnd_selection, x11rb::CURRENT_TIME)
        .expect("own XdndSelection");
    conn.flush().expect("flush");

    // 目标窗口内的局部坐标 -> root 坐标（XdndPosition 使用 root 坐标）。
    let translated = conn
        .translate_coordinates(target, root, local_x, local_y)
        .expect("translate_coordinates")
        .reply()
        .expect("translate reply");
    let (root_x, root_y) = (translated.dst_x, translated.dst_y);
    eprintln!("[xdrop] target=0x{target:x} root=({root_x},{root_y}) source=0x{source:x}");

    let mut uri_list = String::new();
    for p in paths {
        let p = std::path::Path::new(p).canonicalize().unwrap_or_else(|_| p.into());
        uri_list.push_str("file://");
        uri_list.push_str(&p.to_string_lossy());
        uri_list.push_str("\r\n");
    }
    eprintln!("[xdrop] uri-list={uri_list:?}");

    // XdndEnter: 只带 text/uri-list（不使用 type list 属性）。
    send_msg(&conn, target, xdnd_enter, [source, 5 << 24, text_uri_list, 0, 0]);
    conn.flush().expect("flush");
    let packed = ((root_x as u32 & 0xffff) << 16) | (root_y as u32 & 0xffff);
    send_msg(
        &conn,
        target,
        xdnd_position,
        [source, 0, packed, x11rb::CURRENT_TIME, action_copy],
    );
    // winit 会在 XdndPosition 时 XConvertSelection；先服务完再发 Drop。
    let served = pump(&conn, text_uri_list, xdnd_selection, &uri_list, Duration::from_millis(800));
    eprintln!("[xdrop] served selection requests={served}");
    if served == 0 {
        eprintln!("[xdrop] 警告：目标没有请求数据（可能未接受拖放）");
    }
    send_msg(&conn, target, xdnd_drop, [source, 0, x11rb::CURRENT_TIME, 0, 0]);
    let served2 = pump(&conn, text_uri_list, xdnd_selection, &uri_list, Duration::from_millis(800));
    eprintln!("[xdrop] served after drop={served2}");
    // 释放 selection owner，避免目标后续读取悬空数据。
    let _ = conn.set_selection_owner(x11rb::NONE, xdnd_selection, x11rb::CURRENT_TIME);
    let _ = conn.destroy_window(source);
    let _ = conn.flush();
}
