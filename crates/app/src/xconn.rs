// SPDX-License-Identifier: GPL-3.0-only
//! X11 连接辅助：沙箱里没有 `/tmp/.X11-unix`，只能走 **抽象命名空间** socket。
//!
//! std 的 `SocketAddr::from_abstract_name` 让我们不用 libc 就能连上；认证信息从
//! XAUTHORITY 里按 display 号匹配 MIT-MAGIC-COOKIE-1。

use std::io::Read;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

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

// ---------------------------------------------------------------------------
// X11 SHAPE 自适应窗口圆角
// ---------------------------------------------------------------------------

/// 普通窗口的最大圆角半径，单位是 **X11 物理像素**。
const MAX_CORNER_RADIUS: u32 = 12;
/// resize 期间两次 SHAPE 请求的最小间隔。Slint 每帧都会回调 `resized`，
/// 不节流会向 X server 刷大量 CreatePixmap/ShapeMask 请求。
/// 启动时等待 Slint/winit 建窗、map 的重试次数与间隔（与 set_window_icon 一致）。
const CORNER_STARTUP_RETRIES: usize = 50;
const CORNER_STARTUP_RETRY_DELAY: Duration = Duration::from_millis(100);

/// 圆角判断所需的窗口状态。最大化/全屏时半径取 0（方角），
/// 避免窗口四角露出桌面/壁纸，也避免全屏时出现一条缝。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CornerState {
    pub maximized: bool,
    pub fullscreen: bool,
}

/// 一次圆角重算请求。
///
/// `logical_width/height` 是 Slint 的**逻辑像素**（root.width/height，随 scale
/// factor 变化），`scale_factor` 来自 `ui.window().scale_factor()`；worker 用
/// [`physical_extent`] 换算物理像素，作为 X11 geometry 查询失败时的兜底。
/// 正常情况下以 `get_geometry` 返回的物理尺寸为准（X11 窗口没有逻辑像素概念）。
#[derive(Debug, Clone, Copy)]
pub struct CornerRequest {
    pub logical_width: f32,
    pub logical_height: f32,
    pub scale_factor: f32,
    /// `Some` = 调用方刚设置的状态（最大化/全屏按钮、F11），优先采用；
    /// `None` = 查询 `_NET_WM_STATE`（外部 WM 操作 / resize 回调）。
    pub state: Option<CornerState>,
}

/// 逻辑像素换算 X11 物理像素（四舍五入）。
///
/// Slint 的 `resized` 回调给的是逻辑像素；X11 Pixmap/ShapeMask 的宽高是物理
/// 像素。例如 540×400 @ scale 2.0 → 1080×800。非法 scale 回退 1.0，负尺寸回退 0。
pub fn physical_extent(logical: f32, scale_factor: f32) -> u32 {
    if !logical.is_finite() || logical <= 0.0 {
        return 0;
    }
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 { scale_factor } else { 1.0 };
    (logical * scale).round().clamp(0.0, u16::MAX as f32) as u32
}

/// 自适应圆角半径（物理像素）：普通窗口 12px，且不超过短边的一半；
/// 最大化/全屏为 0（方角）。调用方传入的 w/h 必须是物理像素。
pub fn corner_radius(width: u32, height: u32, maximized: bool, fullscreen: bool) -> u32 {
    if maximized || fullscreen {
        return 0;
    }
    MAX_CORNER_RADIUS.min(width / 2).min(height / 2)
}

/// 整数平方根（Newton 法）；用于让圆角逐行计算保持整数、确定、可单测。
fn integer_sqrt(value: u64) -> u64 {
    if value < 2 {
        return value;
    }
    // 初值取 >= sqrt(value)：2^ceil(bits/2)。
    let bits = 64 - value.leading_zeros();
    let mut x = 1u64 << ((bits + 1) / 2);
    loop {
        let next = (x + value / x) / 2;
        if next >= x {
            return x;
        }
        x = next;
    }
}

/// 圆角第 `row` 行（0-based，按像素中心采样）左右两侧各需要裁掉的列数。
///
/// 半径 r 的四个角各是一个 1/4 圆。对行 y，像素中心到圆心的垂直距离
/// `dy = r - (y + 0.5)`，该行白色区域半宽 `sqrt(r² - dy²)`；
/// 四舍五入后 `inset = r - half_width`。全程整数运算，避免浮点误差。
pub fn corner_inset(row: u32, radius: u32) -> u32 {
    // X Rectangle 的 y 是 i16，半径再大也没有意义；同时避免乘法溢出。
    let radius = radius.min(u16::MAX as u32 / 2);
    if radius == 0 || row >= radius {
        return 0;
    }
    let r = radius as u64;
    // 2*dy = 2r - 2row - 1，避免浮点与半像素误差。
    let twice_dy = 2 * (r - row as u64) - 1;
    let squared = 4 * r * r - twice_dy * twice_dy;
    // 2*half_width = sqrt(squared)，向上取整后折算 inset。
    let ceil_half = {
        let floor = integer_sqrt(squared);
        if floor * floor == squared { floor } else { floor + 1 }
    };
    ((2 * r + 1 - ceil_half) / 2) as u32
}

/// 1-bit 掩码里要涂黑的一个矩形（每行左右各一块，四个角镜像）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskRect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// 生成圆角掩码的黑色矩形列表。
///
/// X11 SHAPE 只接受 1-bit 掩码，没有抗锯齿：半径 12 时边缘会有 1px 阶梯。
/// 对 `row in 0..r` 逐行生成四块 1px 高的矩形（4 个角），最多 4×12 块。
pub fn mask_rects(width: u32, height: u32, radius: u32) -> Vec<MaskRect> {
    // X11 Rectangle 的 x/y 是 i16；正常屏幕尺寸远小于上限，超出时按上限保护。
    let w = width.min(i16::MAX as u32) as u16;
    let h = height.min(i16::MAX as u32) as u16;
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let radius = radius.min(w as u32 / 2).min(h as u32 / 2);
    let mut rects = Vec::with_capacity(radius as usize * 4);
    for row in 0..radius {
        let inset = corner_inset(row, radius);
        if inset == 0 {
            continue;
        }
        let inset = inset as u16;
        let top = row as u16;
        let bottom = h - 1 - row as u16;
        rects.push(MaskRect { x: 0, y: top as i16, width: inset, height: 1 });
        rects.push(MaskRect { x: (w - inset) as i16, y: top as i16, width: inset, height: 1 });
        rects.push(MaskRect { x: 0, y: bottom as i16, width: inset, height: 1 });
        rects.push(MaskRect { x: (w - inset) as i16, y: bottom as i16, width: inset, height: 1 });
    }
    rects
}

/// 给窗口设置 X11 SHAPE 圆角掩码；`radius == 0` 时恢复完整矩形。
///
/// 掩码是 depth=1 Pixmap：先整体填白（保留），再用四角逐行黑色矩形抠掉圆角
/// 外部。BOUNDING 让没有合成器的 WM 也裁掉窗口内容；CLIP 覆盖合成器路径。
/// 另外显式把 INPUT 设为整窗矩形：部分服务器会用 BOUNDING 做输入命中，不重置会
/// 让四角拖拽/按钮点不到；视觉仍是圆角，但点击区域与改动前一致。
pub fn apply_rounded_corners(
    conn: &RustConnection,
    window: u32,
    radius: u32,
) -> Result<(), String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::shape::{self, SK, SO};
    use x11rb::protocol::xproto::{
        ChangeGCAux, ClipOrdering, ConnectionExt as _, CreateGCAux, Rectangle,
    };

    let geometry = conn
        .get_geometry(window)
        .map_err(|e| format!("查询窗口几何失败: {e}"))?
        .reply()
        .map_err(|e| format!("查询窗口几何失败: {e}"))?;
    let width = geometry.width;
    let height = geometry.height;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let radius = radius.min(width as u32 / 2).min(height as u32 / 2);

    let pixmap = conn.generate_id().map_err(|e| format!("申请 Pixmap id 失败: {e}"))?;
    conn.create_pixmap(1, pixmap, window, width, height)
        .map_err(|e| format!("创建 depth=1 Pixmap 请求失败: {e}"))?
        .check()
        .map_err(|e| format!("创建 depth=1 Pixmap 失败: {e}"))?;
    let gc = match conn.generate_id() {
        Ok(gc) => gc,
        Err(e) => {
            let _ = conn.free_pixmap(pixmap);
            return Err(format!("申请 GC id 失败: {e}"));
        },
    };
    let gc_cookie = match conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(1u32)) {
        Ok(c) => c,
        Err(e) => {
            let _ = conn.free_pixmap(pixmap);
            return Err(format!("创建 GC 请求失败: {e}"));
        },
    };
    if let Err(e) = gc_cookie.check() {
        let _ = conn.free_pixmap(pixmap);
        return Err(format!("创建 GC 失败: {e}"));
    }

    let result = (|| -> Result<(), String> {
        // 白 = 保留；depth=1 下 foreground 1 即白色。
        conn.poly_fill_rectangle(pixmap, gc, &[Rectangle { x: 0, y: 0, width, height }])
            .map_err(|e| format!("填充白色掩码失败: {e}"))?;
        let black: Vec<Rectangle> = mask_rects(width as u32, height as u32, radius)
            .into_iter()
            .map(|r| Rectangle { x: r.x, y: r.y, width: r.width, height: r.height })
            .collect();
        if !black.is_empty() {
            conn.change_gc(gc, &ChangeGCAux::new().foreground(0u32))
                .map_err(|e| format!("切换 GC 前景色失败: {e}"))?;
            conn.poly_fill_rectangle(pixmap, gc, &black)
                .map_err(|e| format!("绘制圆角黑色矩形失败: {e}"))?;
        }
        shape::mask(conn, SO::SET, SK::BOUNDING, window, 0, 0, pixmap)
            .map_err(|e| format!("SHAPE BOUNDING 请求失败: {e}"))?
            .check()
            .map_err(|e| format!("SHAPE BOUNDING 失败: {e}"))?;
        shape::mask(conn, SO::SET, SK::CLIP, window, 0, 0, pixmap)
            .map_err(|e| format!("SHAPE CLIP 请求失败: {e}"))?
            .check()
            .map_err(|e| format!("SHAPE CLIP 失败: {e}"))?;
        // 少数老服务器可能不支持 Shape 1.1 的 INPUT 区域；圆角（BOUNDING/CLIP）
        // 已经生效，这里失败只提示一次，不把整次应用判失败。
        static INPUT_WARNED: std::sync::Once = std::sync::Once::new();
        if let Err(e) = shape::rectangles(
            conn,
            SO::SET,
            SK::INPUT,
            ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &[Rectangle { x: 0, y: 0, width, height }],
        )
        .map_err(|e| e.to_string())
        .and_then(|cookie| cookie.check().map_err(|e| e.to_string()))
        {
            INPUT_WARNED.call_once(|| {
                eprintln!("[corners] 保持矩形 INPUT 失败（圆角仍生效）: {e}")
            });
        }
        Ok(())
    })();

    let _ = conn.free_gc(gc);
    let _ = conn.free_pixmap(pixmap);
    let _ = conn.flush();
    result
}

struct StateAtoms {
    state: u32,
    maximized_horz: u32,
    maximized_vert: u32,
    fullscreen: u32,
}

fn load_state_atoms(conn: &RustConnection) -> Option<StateAtoms> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let atom = |name: &[u8]| -> Option<u32> {
        conn.intern_atom(false, name).ok()?.reply().ok().map(|a| a.atom)
    };
    Some(StateAtoms {
        state: atom(b"_NET_WM_STATE")?,
        // 个别精简 WM 缺少某个状态 atom：记 0 让该状态永不命中，而不是整体降级。
        maximized_horz: atom(b"_NET_WM_STATE_MAXIMIZED_HORZ").unwrap_or(0),
        maximized_vert: atom(b"_NET_WM_STATE_MAXIMIZED_VERT").unwrap_or(0),
        fullscreen: atom(b"_NET_WM_STATE_FULLSCREEN").unwrap_or(0),
    })
}

fn query_window_state(
    conn: &RustConnection,
    window: u32,
    atoms: &StateAtoms,
) -> Option<CornerState> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    let reply = conn
        .get_property(false, window, atoms.state, AtomEnum::ATOM, 0, 64)
        .ok()?
        .reply()
        .ok()?;
    let mut maximized_horz = false;
    let mut maximized_vert = false;
    let mut fullscreen = false;
    for atom in reply.value32()? {
        maximized_horz |= atom == atoms.maximized_horz;
        maximized_vert |= atom == atoms.maximized_vert;
        fullscreen |= atom == atoms.fullscreen;
    }
    Some(CornerState { maximized: maximized_horz && maximized_vert, fullscreen })
}

/// 找本进程主窗口并返回物理几何；启动时窗口可能还没 map，按 100ms 重试。
/// `cached` 让后续 resize 不必每次遍历整棵窗口树（多实例按 `_NET_WM_PID` 区分）。
fn resolve_own_window(
    conn: &RustConnection,
    root: u32,
    title: &str,
    cached: &mut Option<u32>,
) -> Option<(u32, u16, u16)> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    for attempt in 0..CORNER_STARTUP_RETRIES {
        if let Some(win) = *cached {
            if let Some(reply) =
                conn.get_geometry(win).ok().and_then(|cookie| cookie.reply().ok())
            {
                if reply.width > 0 && reply.height > 0 {
                    return Some((win, reply.width, reply.height));
                }
            }
            *cached = None;
        }
        if let Some(win) = find_own_window(conn, root, title) {
            *cached = Some(win);
            if let Some(reply) =
                conn.get_geometry(win).ok().and_then(|cookie| cookie.reply().ok())
            {
                if reply.width > 0 && reply.height > 0 {
                    return Some((win, reply.width, reply.height));
                }
            }
        }
        if attempt + 1 < CORNER_STARTUP_RETRIES {
            std::thread::sleep(CORNER_STARTUP_RETRY_DELAY);
        }
    }
    None
}

/// 后台圆角 worker：只保留最新请求，节流 + 去重后调用 SHAPE。
///
/// 用独立线程而不是在 `resized` 回调里同步做 X 往返，避免拖拽 resize 时把
/// UI 线程卡在 CreatePixmap/ShapeMask 上。
#[derive(Clone, Debug)]
pub struct CornerShaper {
    tx: Sender<CornerRequest>,
}

impl CornerShaper {
    /// 创建 worker 并连接 X11；SHAPE 不可用/连接失败只影响圆角，不影响窗口启动。
    pub fn spawn(title: &str) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let title = title.to_string();
        std::thread::Builder::new()
            .name("window-corners".into())
            .spawn(move || run_corner_shaper(rx, title))
            .map_err(|e| format!("创建窗口圆角线程失败: {e}"))?;
        Ok(Self { tx })
    }

    /// 发送一次重算请求；worker 已退出（应用正在关闭）时静默忽略。
    pub fn request(&self, request: CornerRequest) {
        let _ = self.tx.send(request);
    }
}

fn run_corner_shaper(rx: Receiver<CornerRequest>, title: String) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::shape;

    let (conn, screen_num) = match connect() {
        Ok(value) => value,
        Err(e) => {
            eprintln!("[corners] X11 连接失败，跳过圆角: {e}");
            return;
        },
    };
    match shape::query_version(&conn)
        .map_err(|e| e.to_string())
        .and_then(|cookie| cookie.reply().map_err(|e| e.to_string()))
    {
        Ok(reply) => {
            eprintln!("[corners] X11 SHAPE {}.{} 可用", reply.major_version, reply.minor_version)
        },
        Err(e) => {
            eprintln!("[corners] X11 SHAPE 不可用（{e}），保持直角窗口");
            return;
        },
    }

    let root = conn.setup().roots[screen_num].root;
    let root_width = conn.setup().roots[screen_num].width_in_pixels as u32;
    let root_height = conn.setup().roots[screen_num].height_in_pixels as u32;
    let atoms = load_state_atoms(&conn);
    if atoms.is_none() {
        eprintln!("[corners] 读取 _NET_WM_STATE 失败；按窗口是否铺满 root 判断最大化");
    }

    let mut window: Option<u32> = None;
    let mut pending: Option<CornerRequest> = None;
    let mut last_request: Option<CornerRequest> = None;
    let mut last_applied: Option<(u32, u16, u16, u32)> = None;
    let mut last_log: Option<(u16, u16, u32, Instant)> = None;
    let mut find_failure_logged = false;
    // resize 期间保持方角，停下 SETTLE 后再补圆角：避免拖拽时每帧改 SHAPE。
    let settle = Duration::from_millis(180);
    let mut pending_round: Option<(u32, u16, u16, u32)> = None;
    let mut last_resize_at = Instant::now();
    let mut shape_cleared = false;
    let mut last_clear_log = Instant::now() - Duration::from_secs(10);

    loop {
        // 1) 有等待恢复的圆角：先方角拖拽，静止 180ms 后再应用掩码。
        if let Some((win, width, height, radius)) = pending_round {
            let elapsed = last_resize_at.elapsed();
            if elapsed < settle {
                match rx.recv_timeout(settle - elapsed) {
                    Ok(newer) => {
                        // 新尺寸到了：回到普通处理路径，重新 resolve/清方角。
                        pending = Some(newer);
                        pending_round = None;
                        last_resize_at = Instant::now();
                        continue;
                    },
                    Err(RecvTimeoutError::Timeout) => {},
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            let key = (win, width, height, radius);
            match apply_rounded_corners(&conn, win, radius) {
                Ok(()) => {
                    last_applied = Some(key);
                    shape_cleared = false;
                    let should_log = last_log.is_none_or(|(lw, lh, lr, at)| {
                        lw != width
                            || lh != height
                            || lr != radius
                            || at.elapsed() >= Duration::from_secs(1)
                    });
                    if should_log {
                        eprintln!(
                            "[corners] win=0x{win:x} {width}x{height} radius={radius}（静止后恢复）"
                        );
                        last_log = Some((width, height, radius, Instant::now()));
                    }
                },
                Err(e) => {
                    eprintln!("[corners] 应用圆角失败 win=0x{win:x} {width}x{height} radius={radius}: {e}");
                    last_applied = Some(key);
                },
            }
            pending_round = None;
            continue;
        }

        // 2) 取请求（启动阶段窗口可能晚 map，500ms 超时重试）。
        let mut request = match pending.take() {
            Some(request) => request,
            None => match rx.recv_timeout(if window.is_none() {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(3600)
            }) {
                Ok(request) => request,
                Err(RecvTimeoutError::Timeout) if window.is_none() => match last_request {
                    Some(request) => request,
                    None => continue,
                },
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        while let Ok(newer) = rx.try_recv() {
            request = newer;
        }
        last_request = Some(request);

        let Some((win, width, height)) = resolve_own_window(&conn, root, &title, &mut window)
        else {
            if !find_failure_logged {
                eprintln!("[corners] 5s 内未找到本进程主窗口（X11/Wayland 后端？），圆角跳过");
                find_failure_logged = true;
            }
            continue;
        };
        find_failure_logged = false;
        while let Ok(newer) = rx.try_recv() {
            request = newer;
            last_request = Some(newer);
        }

        let state = match request.state {
            Some(state) => state,
            None => atoms
                .as_ref()
                .and_then(|atoms| query_window_state(&conn, win, atoms))
                .unwrap_or_else(|| CornerState {
                    maximized: width as u32 >= root_width && height as u32 >= root_height,
                    fullscreen: false,
                }),
        };
        let radius = corner_radius(width as u32, height as u32, state.maximized, state.fullscreen);
        let key = (win, width, height, radius);
        if last_applied == Some(key) {
            continue;
        }

        if radius == 0 {
            // 最大化/全屏：立即清掉圆角掩码，窗口贴边。
            if !shape_cleared {
                if let Err(e) = clear_rounded_corners(&conn, win) {
                    eprintln!("[corners] 清除圆角失败 win=0x{win:x}: {e}");
                }
                shape_cleared = true;
            }
            last_applied = Some(key);
            continue;
        }

        // 普通 resize：第一帧立即去掉掩码，拖拽期间保持方角；停止后补圆角。
        if !shape_cleared {
            if let Err(e) = clear_rounded_corners(&conn, win) {
                eprintln!("[corners] 清除圆角失败 win=0x{win:x}: {e}");
            }
            shape_cleared = true;
            if last_clear_log.elapsed() >= Duration::from_secs(1) {
                eprintln!("[corners] resize 中：暂时取消圆角裁剪（停下后恢复）");
                last_clear_log = Instant::now();
            }
        }
        pending_round = Some((win, width, height, radius));
        last_resize_at = Instant::now();
    }
}

/// 去掉 BOUNDING/CLIP 掩码（窗口恢复矩形）；INPUT 不在这里改。
fn clear_rounded_corners(conn: &RustConnection, window: u32) -> Result<(), String> {
    use x11rb::protocol::shape::{self, SK, SO};
    shape::mask(conn, SO::SET, SK::BOUNDING, window, 0, 0, 0u32)
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| format!("SHAPE BOUNDING clear: {e}"))?;
    shape::mask(conn, SO::SET, SK::CLIP, window, 0, 0, 0u32)
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| format!("SHAPE CLIP clear: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_radius_is_zero_when_maximized_or_fullscreen() {
        assert_eq!(corner_radius(1080, 800, true, false), 0);
        assert_eq!(corner_radius(1080, 800, false, true), 0);
        assert_eq!(corner_radius(1080, 800, true, true), 0);
        // 方角不依赖尺寸：极小窗口也不 panic
        assert_eq!(corner_radius(0, 0, true, true), 0);
    }

    #[test]
    fn corner_radius_caps_and_adapts_to_small_windows() {
        assert_eq!(corner_radius(1080, 800, false, false), 12);
        assert_eq!(corner_radius(24, 24, false, false), 12);
        assert_eq!(corner_radius(23, 80, false, false), 11);
        assert_eq!(corner_radius(80, 21, false, false), 10);
        assert_eq!(corner_radius(20, 8, false, false), 4);
        assert_eq!(corner_radius(3, 100, false, false), 1);
        assert_eq!(corner_radius(2, 100, false, false), 1);
        assert_eq!(corner_radius(1, 100, false, false), 0);
        assert_eq!(corner_radius(0, 100, false, false), 0);
    }

    #[test]
    fn physical_extent_honours_scale_factor() {
        assert_eq!(physical_extent(540.0, 1.0), 540);
        assert_eq!(physical_extent(540.0, 2.0), 1080);
        assert_eq!(physical_extent(270.0, 2.0), 540);
        assert_eq!(physical_extent(10.4, 1.5), 16); // 15.6 四舍五入
        assert_eq!(physical_extent(0.0, 2.0), 0);
        assert_eq!(physical_extent(-1.0, 2.0), 0);
        assert_eq!(physical_extent(f32::NAN, 2.0), 0);
        assert_eq!(physical_extent(f32::INFINITY, 2.0), 0);
        // scale 非法时按 1.0，不因 DPI 查询失败把半径算成 0
        assert_eq!(physical_extent(100.0, 0.0), 100);
        assert_eq!(physical_extent(100.0, f32::NAN), 100);
        assert_eq!(physical_extent(40000.0, 2.0), u16::MAX as u32);
    }

    #[test]
    fn radius_uses_physical_pixels_not_logical() {
        // 5x5 逻辑 @2x = 10x10 物理 → 半径 5；若误用逻辑尺寸只会得到 2。
        let w = physical_extent(5.0, 2.0);
        let h = physical_extent(5.0, 2.0);
        assert_eq!((w, h), (10, 10));
        assert_eq!(corner_radius(w, h, false, false), 5);
    }

    #[test]
    fn corner_inset_matches_circle_rows() {
        assert_eq!(corner_inset(0, 0), 0);
        assert_eq!(corner_inset(99, 12), 0);
        assert_eq!(corner_inset(0, 1), 0); // 1px 半径在 1-bit 掩码里抠不出整像素
        assert_eq!(corner_inset(0, 2), 1);
        let rows: Vec<u32> = (0..12).map(|row| corner_inset(row, 12)).collect();
        assert_eq!(rows, vec![9, 6, 5, 4, 3, 2, 1, 1, 1, 0, 0, 0]);
        // 极大半径被限制在 i16 安全范围，不溢出、不 panic
        let _ = corner_inset(0, u32::MAX);
    }

    #[test]
    fn corner_inset_is_monotonic() {
        for radius in 1..=32u32 {
            let mut previous = u32::MAX;
            for row in 0..radius {
                let inset = corner_inset(row, radius);
                assert!(inset <= previous, "r={radius} row={row} inset={inset} prev={previous}");
                assert!(inset <= radius);
                previous = inset;
            }
        }
    }

    #[test]
    fn mask_rects_mirror_all_four_corners() {
        let rects = mask_rects(100, 80, 12);
        // 半径 12 的圆角只有前 9 行有黑色区域，每行 4 块。
        assert_eq!(rects.len(), 36);
        assert_eq!(rects[0], MaskRect { x: 0, y: 0, width: 9, height: 1 });
        assert_eq!(rects[1], MaskRect { x: 91, y: 0, width: 9, height: 1 });
        assert_eq!(rects[2], MaskRect { x: 0, y: 79, width: 9, height: 1 });
        assert_eq!(rects[3], MaskRect { x: 91, y: 79, width: 9, height: 1 });
        assert!(rects.contains(&MaskRect { x: 0, y: 1, width: 6, height: 1 }));
        assert!(rects.contains(&MaskRect { x: 94, y: 1, width: 6, height: 1 }));
        assert!(rects.contains(&MaskRect { x: 0, y: 78, width: 6, height: 1 }));
        assert!(rects.contains(&MaskRect { x: 94, y: 78, width: 6, height: 1 }));
        for rect in &rects {
            assert!(rect.x >= 0 && rect.y >= 0);
            assert!((rect.x as i32 + rect.width as i32) <= 100);
            assert!((rect.y as i32) < 80);
            assert!(rect.width > 0 && rect.height == 1);
        }
    }

    #[test]
    fn mask_rects_clamp_radius_and_skip_degenerate_windows() {
        assert!(mask_rects(0, 100, 12).is_empty());
        assert!(mask_rects(100, 0, 12).is_empty());
        assert!(mask_rects(100, 80, 0).is_empty());
        assert!(mask_rects(1, 1, 12).is_empty());
        assert!(mask_rects(3, 3, 12).is_empty());
        // 20x10 的窗口最多 5px 半径，即使请求 100 也不能画出窗口外/越界的矩形。
        let rects = mask_rects(20, 10, 100);
        assert!(!rects.is_empty());
        for rect in &rects {
            assert!(rect.x >= 0 && rect.y >= 0);
            assert!((rect.x as i32 + rect.width as i32) <= 20);
            assert!((rect.y as i32) < 10);
        }
    }
}
