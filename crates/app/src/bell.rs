// SPDX-License-Identifier: GPL-3.0-only
//! 系统响铃（audible bell）。
//!
//! BEL 到达且 `audible_bell = true` 时：
//! - 原生 X11 窗口优先向 X server 发 `Bell` 请求（GNOME Terminal 同样走
//!   GDK/X11 bell，是否真的出声由桌面声音主题/`xset b` 决定）；
//! - 原生 Wayland 或没有 X11 连接时，如果 stderr 是终端就写一个 BEL 字节，
//!   让启动 xtervo 的终端模拟器响铃。
//!
//! 两条路径都是尽力而为：失败只返回 false，不影响终端输入输出，也不阻塞
//! 解析线程（调用点在 UI 事件轮询里）。

use std::cell::RefCell;
use std::io::Write;

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::ConnectionExt as _;

thread_local! {
    /// 响铃不频繁，X11 连接懒建立并复用；请求失败后清空，下次重新建。
    static X11: RefCell<Option<x11rb::rust_connection::RustConnection>> =
        const { RefCell::new(None) };
}

/// 响一次系统铃；返回两个后端里是否有任意一个成功。
pub fn ring() -> bool {
    if ring_x11() {
        return true;
    }
    if stderr_is_tty() {
        let mut err = std::io::stderr();
        // BEL 不是可打印文本；写失败也不影响终端状态。
        return err.write_all(b"\x07").and_then(|_| err.flush()).is_ok();
    }
    false
}

fn ring_x11() -> bool {
    // 原生 Wayland 窗口不是 X11 client：连 XWayland 发 Bell 不会有声音，还会
    // 让调用方误以为成功而不走 stderr BEL 回退。
    if !crate::platform::caps().x11_window_ops {
        return false;
    }
    X11.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = crate::xconn::connect().ok().map(|(conn, _)| conn);
        }
        // percent = -100 表示使用 X server 默认音量；Bell 是 void 请求，
        // flush 只把请求交给 socket，不等回复。
        let sent = match slot.as_ref() {
            Some(conn) => match conn.bell(-100) {
                Ok(_) => conn.flush().is_ok(),
                Err(_) => false,
            },
            None => false,
        };
        if !sent {
            // 连接/请求失败：清空，下次响铃时重试（无 DISPLAY 时不会永久卡在坏连接）。
            *slot = None;
        }
        sent
    })
}

fn stderr_is_tty() -> bool {
    // SAFETY: isatty 只读 fd 状态，不解引用任何指针。
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}
