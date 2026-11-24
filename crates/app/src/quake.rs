// SPDX-License-Identifier: GPL-3.0-only
//! Quake Mode 的 X11 全局热键（被动 grab）。
//!
//! 只支持 X11：Wayland 没有等价的全局热键协议，由调用方记录回退日志。
//! 热键在独立线程里阻塞等待 KeyPress，再通过 `slint::invoke_from_event_loop`
//! 回到 UI 线程。自动重复由 [`ToggleGate`] 过滤。

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as XprotoExt, EventMask, GrabMode, ModMask,
};
use x11rb::rust_connection::RustConnection;

use crate::xconn;

/// 同一热键两次 toggle 的最小间隔（防手抖/极快重复）。
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(250);

/// X11 自动重复的状态机。
///
/// 关闭 detectable auto-repeat 时，X11 对按住键发的是
/// `KeyRelease + KeyPress` 对；只看“当前是否按住”会被这对假事件骗过，
/// 在最初的 repeat delay（通常 ~660ms）之后再次 toggle。这里用
/// release→press 的极短间隔识别自动重复对，只有真正的 release 之后
/// 才接受下一次热键。
#[derive(Debug)]
struct ToggleGate {
    held: bool,
    last_toggle: Instant,
    last_release: Option<Instant>,
}

impl ToggleGate {
    fn new(now: Instant) -> Self {
        Self {
            held: false,
            last_toggle: now
                .checked_sub(Duration::from_secs(2))
                .unwrap_or(now),
            last_release: None,
        }
    }

    /// 返回 true 表示这次 KeyPress 应该 toggle。
    fn on_press(&mut self, now: Instant) -> bool {
        if self.held {
            return false;
        }
        self.held = true;
        if let Some(release) = self.last_release {
            if now.saturating_duration_since(release) < REPEAT_PAIR_GAP {
                // X11 自动重复的假 release 后紧跟假 press：仍然算“按住”。
                return false;
            }
        }
        if now.saturating_duration_since(self.last_toggle) < TOGGLE_DEBOUNCE {
            return false;
        }
        self.last_toggle = now;
        true
    }

    fn on_release(&mut self, now: Instant) {
        self.held = false;
        self.last_release = Some(now);
    }
}

/// 自动重复对里 release 与下一个 press 的最大间隔。真实用户按键不会
/// 在 release 后 <30ms 内又按下；X11 repeat 对几乎是连续到达的。
const REPEAT_PAIR_GAP: Duration = Duration::from_millis(30);

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
        &ChangeWindowAttributesAux::new()
            .event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
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
        let mut gate = ToggleGate::new(Instant::now());
        loop {
            match conn.wait_for_event() {
                Ok(Event::KeyPress(ev)) if ev.detail == keycode => {
                    eprintln!("[quake] KeyPress state={:?}", ev.state);
                    if gate.on_press(Instant::now()) {
                        on_toggle();
                    }
                },
                Ok(Event::KeyRelease(ev)) if ev.detail == keycode => {
                    gate.on_release(Instant::now());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn auto_repeat_release_press_pairs_do_not_retoggle() {
        let base = Instant::now();
        let mut gate = ToggleGate::new(base - Duration::from_secs(3));

        // 第一次真实按下：toggle。
        assert!(gate.on_press(at(base, 0)));

        // X11 自动重复：~660ms 后开始发 fake release + fake press 对。
        for i in 0..10 {
            let release = at(base, 660 + i * 33);
            gate.on_release(release);
            assert!(
                !gate.on_press(release + Duration::from_millis(1)),
                "auto-repeat pair {i} must not toggle"
            );
        }

        // 真实松开后再次按下（间隔超过 repeat pair 阈值/去抖）才 toggle。
        gate.on_release(at(base, 2_000));
        assert!(gate.on_press(at(base, 2_400)));
    }

    #[test]
    fn quick_double_press_is_debounced_until_release() {
        let base = Instant::now();
        let mut gate = ToggleGate::new(base - Duration::from_secs(3));
        assert!(gate.on_press(at(base, 0)));
        gate.on_release(at(base, 40));
        // release→press 的 80ms 间隔不是自动重复对，但仍在去抖窗口内。
        assert!(!gate.on_press(at(base, 120)));
        gate.on_release(at(base, 150));
        assert!(gate.on_press(at(base, 400)));
    }
}
