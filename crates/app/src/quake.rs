// SPDX-License-Identifier: GPL-3.0-only
//! Quake Mode 的 X11 全局热键（被动 grab）。
//!
//! 只支持 X11：Wayland 没有等价的全局热键协议，由调用方记录回退日志。
//! 热键在独立线程里阻塞等待 KeyPress，再通过 `slint::invoke_from_event_loop`
//! 回到 UI 线程。自动重复用 250ms 去抖过滤。

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as XprotoExt, EventMask, GrabMode, ModMask,
};
use x11rb::rust_connection::RustConnection;

use crate::xconn;

/// 注册全局热键；成功后后台线程一直监听。热键格式：`F12` / `Ctrl+Shift+F12`。
pub fn spawn_listener(
    hotkey: &str,
    on_toggle: impl Fn() + Send + 'static,
) -> Result<(), String> {
    let (mask, keysym) = parse_hotkey(hotkey)?;
    let (conn, screen_num) = xconn::connect()?;
    let root = conn.setup().roots[screen_num].root;
    let keycode = keysym_to_keycode(&conn, keysym)
        .ok_or_else(|| format!("键盘映射里找不到 {hotkey}"))?;

    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::KEY_PRESS),
    )
    .map_err(|e| format!("选择 root KeyPress 失败: {e}"))?
    .check()
    .map_err(|e| format!("选择 root KeyPress 失败: {e}"))?;

    // CapsLock/NumLock 打开时修饰位会变，把常见组合都抓一遍。
    let mut grabbed = false;
    for extra in [ModMask::default(), ModMask::LOCK, ModMask::M2, ModMask::LOCK | ModMask::M2] {
        if let Ok(cookie) = conn.grab_key(
            false,
            root,
            mask | extra,
            keycode,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        ) {
            if cookie.check().is_ok() {
                grabbed = true;
            }
        }
    }
    if !grabbed {
        return Err(format!("{hotkey} 已被其他程序占用（X11 grab 失败）"));
    }
    conn.flush().map_err(|e| format!("flush 失败: {e}"))?;
    eprintln!("[quake] 全局热键 {hotkey} 已注册");

    std::thread::spawn(move || {
        let mut last = Instant::now() - Duration::from_secs(2);
        loop {
            match conn.wait_for_event() {
                Ok(Event::KeyPress(ev)) if ev.detail == keycode => {
                    eprintln!("[quake] KeyPress state={:?}", ev.state);
                    let now = Instant::now();
                    if now.duration_since(last) < Duration::from_millis(250) {
                        continue;
                    }
                    last = now;
                    on_toggle();
                },
                Ok(_) => {},
                Err(e) => {
                    eprintln!("[quake] 监听线程退出: {e}");
                    break;
                },
            }
        }
    });
    Ok(())
}

fn parse_hotkey(spec: &str) -> Result<(ModMask, u32), String> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let key = parts.last().ok_or_else(|| format!("热键为空: {spec:?}"))?;
    let mut mask = ModMask::default();
    for m in &parts[..parts.len() - 1] {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mask |= ModMask::CONTROL,
            "shift" => mask |= ModMask::SHIFT,
            "alt" => mask |= ModMask::M1,
            "super" | "meta" | "win" => mask |= ModMask::M4,
            other => return Err(format!("不支持的修饰键: {other}")),
        }
    }
    let keysym = keysym_for(key).ok_or_else(|| format!("不支持的键: {key}"))?;
    Ok((mask, keysym))
}

fn keysym_for(key: &str) -> Option<u32> {
    let upper = key.to_ascii_uppercase();
    if let Some(n) = upper.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0xffbe + n - 1);
        }
    }
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(c as u32)
}

fn keysym_to_keycode(conn: &RustConnection, keysym: u32) -> Option<u8> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(min).saturating_add(1);
    let reply = conn.get_keyboard_mapping(min, count).ok()?.reply().ok()?;
    let per = reply.keysyms_per_keycode as usize;
    if per == 0 {
        return None;
    }
    reply
        .keysyms
        .iter()
        .position(|s| *s == keysym)
        .map(|i| min + (i / per) as u8)
}
