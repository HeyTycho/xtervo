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
                let title_matches = {
                    let wm_name = conn
                        .get_property(false, child, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| String::from_utf8_lossy(&r.value).to_string())
                        .unwrap_or_default();
                    wm_name.contains(title)
                };
                if title_matches {
                    return Some(child);
                }
            }
            stack.push(child);
        }
    }
    None
}

/// 设置 `_NET_WM_ICON`（ARGB CARDINAL 数组）。Slint 内置 icon 在部分环境为空，这里兜底。
pub fn set_window_icon(rgba: &[u8], size: u16) -> Result<(), String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    use x11rb::wrapper::ConnectionExt as _;

    let (conn, screen_num) = connect()?;
    let root = conn.setup().roots[screen_num].root;

    let win = (0..50)
        .find_map(|_| {
            let w = find_own_window(&conn, root, "Rust Tabby");
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
    Ok(())
}

/// 查询指针在 root 窗口上的绝对坐标（用于自绘拖边缩放）。
pub fn query_pointer(conn: &RustConnection, root: u32) -> Option<(i32, i32)> {
    use x11rb::connection::Connection;
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
