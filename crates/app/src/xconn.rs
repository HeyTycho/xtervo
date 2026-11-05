// SPDX-License-Identifier: GPL-3.0-only
//! X11 连接辅助：沙箱里没有 `/tmp/.X11-unix`，只能走 **抽象命名空间** socket。
//!
//! std 的 `SocketAddr::from_abstract_name` 让我们不用 libc 就能连上；认证信息从
//! XAUTHORITY 里按 display 号匹配 MIT-MAGIC-COOKIE-1。

use std::io::Read;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};

use x11rb::rust_connection::{DefaultStream, RustConnection};

fn display_number() -> Option<String> {
    let display = std::env::var("DISPLAY").ok()?;
    let after_colon = display.rsplit(':').next()?;
    Some(after_colon.split('.').next()?.to_string())
}

fn xauthority_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("XAUTHORITY") {
        return Some(p.into());
    }
    std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".Xauthority"))
}

/// 极简 Xauthority 解析：只取第一个与 display 号匹配的 MIT-MAGIC-COOKIE-1。
fn read_cookie(display: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut data = Vec::new();
    std::fs::File::open(xauthority_path()?).ok()?.read_to_end(&mut data).ok()?;

    let mut pos = 0usize;
    let take = |pos: &mut usize, len: usize| -> Option<&[u8]> {
        let out = data.get(*pos..*pos + len)?;
        *pos += len;
        Some(out)
    };
    while pos + 2 <= data.len() {
        let _family = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?);
        let addr_len = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?) as usize;
        let _addr = take(&mut pos, addr_len)?;
        let num_len = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?) as usize;
        let number = take(&mut pos, num_len)?;
        let name_len = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?) as usize;
        let name = take(&mut pos, name_len)?;
        let data_len = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?) as usize;
        let cookie = take(&mut pos, data_len)?;

        if name == b"MIT-MAGIC-COOKIE-1"
            && (number.is_empty()
                || String::from_utf8_lossy(number) == display
                || String::from_utf8_lossy(number).ends_with(&format!(":{display}")))
        {
            return Some((name.to_vec(), cookie.to_vec()));
        }
    }
    None
}

/// 按标题（WM_NAME / _NET_WM_NAME）递归查找窗口。
#[allow(dead_code)]
pub fn find_window(conn: &RustConnection, win: u32, title: &str, depth: usize) -> Option<u32> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    fn read_prop(conn: &RustConnection, win: u32, atom: u32) -> Option<String> {
        let reply = conn.get_property(false, win, atom, AtomEnum::ANY, 0, 1024).ok()?.reply().ok()?;
        if reply.value.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&reply.value).to_string())
        }
    }

    if depth > 8 {
        return None;
    }
    if let Some(name) = read_prop(conn, win, AtomEnum::WM_NAME.into()) {
        if name.contains(title) {
            return Some(win);
        }
    }
    if let Ok(cookie) = conn.intern_atom(false, b"_NET_WM_NAME") {
        if let Ok(reply) = cookie.reply() {
            if let Some(name) = read_prop(conn, win, reply.atom) {
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

/// 找到属于当前进程的窗口（用 `_NET_WM_PID` 精确区分多实例）。
pub fn find_own_window(conn: &RustConnection, root: u32, title: &str) -> Option<u32> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    fn read_pid(conn: &RustConnection, win: u32, atom: u32) -> Option<u32> {
        let reply = conn.get_property(false, win, atom, AtomEnum::CARDINAL, 0, 1).ok()?.reply().ok()?;
        reply.value32().and_then(|mut it| it.next())
    }

    let mut stack = vec![root];
    let mut visited = 0;
    // 同一个进程可能还有隐藏的辅助窗口（输入法/临时窗口），只认**可见且面积最大**的那个，
    // 否则会把图标写到 1x1 的窗口上，自动化按日志里的 win id 也会点到不存在的窗口。
    let mut best: Option<(u32, u32)> = None; // (area, window)
    while let Some(win) = stack.pop() {
        visited += 1;
        if visited > 512 {
            break;
        }
        let tree = match conn.query_tree(win).ok().and_then(|c| c.reply().ok()) {
            Some(t) => t,
            None => continue,
        };
        for child in tree.children {
            let pid_matches = conn
                .intern_atom(false, b"_NET_WM_PID")
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|a| read_pid(conn, child, a.atom))
                .is_some_and(|pid| pid == std::process::id());
            if pid_matches {
                // 空标题：只按 PID 匹配（`--title` 自定义标题时用）。
                let title_matches = if title.is_empty() {
                    true
                } else {
                    let wm_name = conn
                        .get_property(false, child, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| String::from_utf8_lossy(&r.value).to_string())
                        .unwrap_or_default();
                    wm_name.contains(title)
                };
                if title_matches {
                    let viewable = conn
                        .get_window_attributes(child)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .is_some_and(|a| a.map_state == x11rb::protocol::xproto::MapState::VIEWABLE);
                    if viewable {
                        if let Some(geo) =
                            conn.get_geometry(child).ok().and_then(|c| c.reply().ok())
                        {
                            let area = geo.width as u32 * geo.height as u32;
                            if best.is_none_or(|(a, _)| area > a) {
                                best = Some((area, child));
                            }
                        }
                    }
                }
            }
            stack.push(child);
        }
    }
    if best.is_some() {
        return best.map(|(_, w)| w);
    }
    None
}

/// 设置 `_NET_WM_ICON`（ARGB CARDINAL 数组）。Slint 内置 icon 在部分环境为空，这里兜底。
pub fn set_window_icon(rgba: &[u8], size: u16, title: &str) -> Result<u32, String> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    use x11rb::wrapper::ConnectionExt as _;

    let (conn, screen_num) = connect()?;
    #[allow(unused_imports)]
    use x11rb::connection::Connection;

    let root = conn.setup().roots[screen_num].root;

    let win = (0..50)
        .find_map(|_| {
            let w = find_own_window(&conn, root, title);
            if w.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            w
        })
        .ok_or_else(|| "找不到本进程的窗口".to_string())?;

    let mut data: Vec<u32> = Vec::with_capacity(2 + (size as usize * size as usize));
    data.push(size as u32);
    data.push(size as u32);
    for px in rgba.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        data.push((a << 24) | (r << 16) | (g << 8) | b);
    }

    let atom = conn
        .intern_atom(false, b"_NET_WM_ICON")
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .atom;
    conn.change_property32(PropMode::REPLACE, win, atom, AtomEnum::CARDINAL, &data)
        .map_err(|e| e.to_string())?;
    eprintln!("[icon] 写入窗口 0x{win:x}，{} 个 CARDINAL", data.len());
    conn.flush().map_err(|e| e.to_string())?;
    Ok(win)
}

/// 查询指针在 root 窗口上的绝对坐标（用于自绘拖边缩放）。
pub fn query_pointer(conn: &RustConnection, root: u32) -> Option<(i32, i32)> {
    use x11rb::protocol::xproto::ConnectionExt;
    let reply = conn.query_pointer(root).ok()?.reply().ok()?;
    Some((reply.root_x as i32, reply.root_y as i32))
}

/// 连接当前 DISPLAY：优先抽象 socket，失败再退回 x11rb 默认路径。
pub fn connect() -> Result<(RustConnection, usize), String> {
    let display = display_number().ok_or("DISPLAY 未设置")?;
    let name = format!("/tmp/.X11-unix/X{display}");
    if let Ok(addr) = SocketAddr::from_abstract_name(name.as_bytes()) {
        if let Ok(stream) = UnixStream::connect_addr(&addr) {
            if let Ok((stream, _peer)) = DefaultStream::from_unix_stream(stream) {
                let (auth_name, auth_data) = read_cookie(&display).unwrap_or_default();
                if let Ok(conn) = RustConnection::connect_to_stream_with_auth_info(
                    stream, 0, auth_name, auth_data,
                ) {
                    return Ok((conn, 0));
                }
            }
        }
    }
    x11rb::connect(None).map_err(|e| format!("X11 连接失败: {e}"))
}


/// 屏幕（root）尺寸，用于 Quake 定位。
pub fn screen_size() -> Option<(u32, u32)> {
    use x11rb::connection::Connection;

    let (conn, screen_num) = connect().ok()?;
    let root = &conn.setup().roots[screen_num];
    Some((root.width_in_pixels as u32, root.height_in_pixels as u32))
}

/// 把本进程的主窗口置顶（_NET_WM_STATE_ABOVE + STICKY）、激活并聚焦。
/// Quake 显示时调用；找不到窗口时返回 None。
pub fn activate_own_window(title: &str) -> Option<u32> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, InputFocus,
        StackMode,
    };

    let (conn, screen_num) = connect().ok()?;
    let root = conn.setup().roots[screen_num].root;
    let win = find_own_window(&conn, root, title)?;

    let state_atom = conn.intern_atom(false, b"_NET_WM_STATE").ok()?.reply().ok()?.atom;
    for name in [b"_NET_WM_STATE_ABOVE".as_slice(), b"_NET_WM_STATE_STICKY".as_slice()] {
        if let Some(atom) = conn.intern_atom(false, name).ok().and_then(|c| c.reply().ok()) {
            let event = ClientMessageEvent::new(32, win, state_atom, [1, atom.atom, 0, 1, 0]);
            let _ = conn.send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            );
        }
    }

    if let Ok(cookie) = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW") {
        if let Ok(atom) = cookie.reply() {
            let event =
                ClientMessageEvent::new(32, win, atom.atom, [1, x11rb::CURRENT_TIME, 0, 0, 0]);
            let _ = conn.send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            );
        }
    }
    let _ = conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME);
    let _ = conn.configure_window(
        win,
        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
    );
    let _ = conn.flush();
    Some(win)
}

/// 当前输入焦点窗口（_NET_ACTIVE_WINDOW），诊断用。
pub fn active_window() -> Option<u32> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, screen_num) = connect().ok()?;
    let root = conn.setup().roots[screen_num].root;
    let atom = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW").ok()?.reply().ok()?.atom;
    let reply = conn
        .get_property(false, root, atom, x11rb::protocol::xproto::AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    reply.value32().and_then(|mut it| it.next())
}

/// 指针在 root 上的坐标，诊断用。
pub fn pointer_position() -> Option<(i32, i32)> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, screen_num) = connect().ok()?;
    let root = conn.setup().roots[screen_num].root;
    let reply = conn.query_pointer(root).ok()?.reply().ok()?;
    Some((reply.root_x as i32, reply.root_y as i32))
}

/// 本进程窗口的快照（诊断/日志用）。
#[derive(Debug, Clone)]
pub struct OwnWindow {
    pub id: u32,
    pub pid: u32,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub viewable: bool,
}

pub fn own_windows() -> Vec<OwnWindow> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    let Ok((conn, screen_num)) = connect() else { return Vec::new() };
    let root = conn.setup().roots[screen_num].root;
    let pid = std::process::id();
    let Some(pid_atom) = conn.intern_atom(false, b"_NET_WM_PID").ok().and_then(|c| c.reply().ok())
    else {
        return Vec::new();
    };

    fn title(conn: &RustConnection, win: u32) -> String {
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
        let net = conn
            .intern_atom(false, b"_NET_WM_NAME")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|a| a.atom);
        for atom in [net, Some(AtomEnum::WM_NAME.into())].into_iter().flatten() {
            if let Ok(reply) = conn.get_property(false, win, atom, AtomEnum::ANY, 0, 1024) {
                if let Ok(r) = reply.reply() {
                    if !r.value.is_empty() {
                        return String::from_utf8_lossy(&r.value).to_string();
                    }
                }
            }
        }
        String::new()
    }

    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut visited = 0;
    while let Some(win) = stack.pop() {
        visited += 1;
        if visited > 512 {
            break;
        }
        let Ok(tree) = conn.query_tree(win).ok().and_then(|c| c.reply().ok()).ok_or(()) else {
            continue;
        };
        for child in tree.children {
            let matches = conn
                .get_property(false, child, pid_atom.atom, AtomEnum::CARDINAL, 0, 1)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().and_then(|mut it| it.next()))
                .is_some_and(|p| p == pid);
            if matches {
                let viewable = conn
                    .get_window_attributes(child)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|a| {
                        a.map_state == x11rb::protocol::xproto::MapState::VIEWABLE
                    });
                if let Some(g) = conn.get_geometry(child).ok().and_then(|c| c.reply().ok()) {
                    let pos = conn
                        .translate_coordinates(child, root, 0, 0)
                        .ok()
                        .and_then(|c| c.reply().ok());
                    out.push(OwnWindow {
                        id: child,
                        pid,
                        title: title(&conn, child),
                        x: pos.as_ref().map(|p| p.dst_x as i32).unwrap_or(0),
                        y: pos.as_ref().map(|p| p.dst_y as i32).unwrap_or(0),
                        w: g.width as u32,
                        h: g.height as u32,
                        viewable,
                    });
                }
            }
            stack.push(child);
        }
    }
    out
}


fn window_pid(conn: &RustConnection, win: u32) -> u32 {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    conn.intern_atom(false, b"_NET_WM_PID")
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|a| {
            conn.get_property(false, win, a.atom, AtomEnum::CARDINAL, 0, 1)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().and_then(|mut it| it.next()))
        })
        .unwrap_or(0)
}

fn window_title(conn: &RustConnection, win: u32) -> String {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    let net = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|a| a.atom);
    for atom in [net, Some(AtomEnum::WM_NAME.into())].into_iter().flatten() {
        if let Ok(reply) = conn.get_property(false, win, atom, AtomEnum::ANY, 0, 1024) {
            if let Ok(r) = reply.reply() {
                if !r.value.is_empty() {
                    return String::from_utf8_lossy(&r.value).to_string();
                }
            }
        }
    }
    String::new()
}

fn window_class(conn: &RustConnection, win: u32) -> String {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    conn.get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| String::from_utf8_lossy(&r.value).replace('\0', " "))
        .unwrap_or_default()
}

/// 按 id 取一个窗口的快照（不要求属于本进程）。
pub fn window_snapshot(id: u32) -> Option<OwnWindow> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, _screen_num) = connect().ok()?;
    let viewable = conn
        .get_window_attributes(id)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|a| a.map_state == x11rb::protocol::xproto::MapState::VIEWABLE);
    let g = conn.get_geometry(id).ok()?.reply().ok()?;
    let pos = conn.translate_coordinates(id, conn.setup().roots[0].root, 0, 0).ok()?.reply().ok();
    Some(OwnWindow {
        id,
        pid: window_pid(&conn, id),
        title: window_title(&conn, id),
        x: pos.as_ref().map(|p| p.dst_x as i32).unwrap_or(0),
        y: pos.as_ref().map(|p| p.dst_y as i32).unwrap_or(0),
        w: g.width as u32,
        h: g.height as u32,
        viewable,
    })
}

/// 扫描所有 Tervo 类窗口（包括**不是本进程**的旧实例），用于诊断“谁挡在前面”。
pub fn tervo_windows() -> Vec<OwnWindow> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let Ok((conn, screen_num)) = connect() else { return Vec::new() };
    let root = conn.setup().roots[screen_num].root;
    let mut out = Vec::new();
    let Ok(tree) = conn.query_tree(root).ok().and_then(|c| c.reply().ok()).ok_or(()) else {
        return out;
    };
    for child in tree.children {
        let class = window_class(&conn, child);
        if !class.to_ascii_lowercase().contains("tervo") {
            continue;
        }
        let viewable = conn
            .get_window_attributes(child)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|a| a.map_state == x11rb::protocol::xproto::MapState::VIEWABLE);
        if let Some(g) = conn.get_geometry(child).ok().and_then(|c| c.reply().ok()) {
            let pos = conn
                .translate_coordinates(child, root, 0, 0)
                .ok()
                .and_then(|c| c.reply().ok());
            out.push(OwnWindow {
                id: child,
                pid: window_pid(&conn, child),
                title: window_title(&conn, child),
                x: pos.as_ref().map(|p| p.dst_x as i32).unwrap_or(0),
                y: pos.as_ref().map(|p| p.dst_y as i32).unwrap_or(0),
                w: g.width as u32,
                h: g.height as u32,
                viewable,
            });
        }
    }
    out
}
