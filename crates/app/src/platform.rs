// SPDX-License-Identifier: GPL-3.0-only
//! 平台能力探测与 Wayland / X11 能力门控。
//!
//! 判定分两层，避免把「XWayland 可用」误当成「本进程是 X11 窗口」：
//!
//! - **hint**：读 `WAYLAND_DISPLAY` / `WAYLAND_SOCKET` / `DISPLAY`，按 winit
//!   0.30 的后端选择规则推断。窗口创建前的调用点（Quake 热键注册、圆角
//!   worker、`--diagnose`）只能用它。
//! - **confirmed**：窗口创建后从 winit raw window handle 读取，是唯一可信的
//!   后端事实。两者不一致时以 raw handle 为准，并保留启动日志。
//!
//! Wayland 会话里 `DISPLAY` 通常仍指向 XWayland，`xconn` 也能连上；因此所有
//! 依赖 X11 窗口语义的能力（窗口图标、全局热键、窗口定位、XDND、SHAPE 圆角）
//! 都必须走本模块的 `Caps`，不能只看 `DISPLAY`。

use std::sync::atomic::{AtomicU8, Ordering};

use slint::winit_030::winit;

/// Wayland app_id；必须与打包的 `xtervo.desktop`（`StartupWMClass`、`Icon=xtervo`）
/// 一致，桌面才能关联图标与启动器。X11 不设置（保持 winit 默认 WM_CLASS）。
pub const XDG_APP_ID: &str = "xtervo";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UiBackend {
    Unknown = 0,
    X11 = 1,
    Wayland = 2,
}

impl UiBackend {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::X11,
            2 => Self::Wayland,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::X11 => "x11",
            Self::Wayland => "wayland",
            Self::Unknown => "unknown",
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

fn has_value(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|v| !v.is_empty())
}

/// 启动早期的会话事实。只在纯函数里使用，便于单测，不直接读全局 env。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFacts {
    pub session_type: Option<String>,
    pub desktop: Option<String>,
    pub wayland_display: Option<String>,
    pub wayland_socket: Option<String>,
    pub display: Option<String>,
}

impl SessionFacts {
    pub fn from_env() -> Self {
        Self {
            session_type: non_empty(std::env::var("XDG_SESSION_TYPE").ok()),
            desktop: non_empty(std::env::var("XDG_CURRENT_DESKTOP").ok()),
            wayland_display: non_empty(std::env::var("WAYLAND_DISPLAY").ok()),
            wayland_socket: non_empty(std::env::var("WAYLAND_SOCKET").ok()),
            display: non_empty(std::env::var("DISPLAY").ok()),
        }
    }

    /// 与 winit 0.30.13 `EventLoop::new` 的选择顺序一致：Wayland 优先。
    /// 空字符串按“未设置”处理（winit 同样过滤空值）；这里不依赖 `from_env`
    /// 已规范化，手工构造的 facts 也保持同一语义。
    pub fn hint(&self) -> UiBackend {
        if has_value(&self.wayland_display) || has_value(&self.wayland_socket) {
            UiBackend::Wayland
        } else if has_value(&self.display) {
            UiBackend::X11
        } else {
            UiBackend::Unknown
        }
    }

    /// Wayland 会话同时存在 `DISPLAY`：剪贴板等能力可回退 XWayland，但窗口本身
    /// 仍是 Wayland surface。
    pub fn xwayland_likely(&self) -> bool {
        self.hint() == UiBackend::Wayland && has_value(&self.display)
    }

    pub fn summary(&self) -> String {
        format!(
            "session={} desktop={} wayland={} x11_display={} xwayland={}",
            self.session_type.as_deref().unwrap_or("-"),
            self.desktop.as_deref().unwrap_or("-"),
            self.wayland_display
                .as_deref()
                .or(self.wayland_socket.as_deref())
                .unwrap_or("-"),
            self.display.as_deref().unwrap_or("-"),
            self.xwayland_likely(),
        )
    }
}

static CONFIRMED_BACKEND: AtomicU8 = AtomicU8::new(UiBackend::Unknown as u8);

/// 当前后端：已从 raw handle 确认则用确认值，否则退回启动期 hint。
pub fn backend() -> UiBackend {
    let confirmed = UiBackend::from_u8(CONFIRMED_BACKEND.load(Ordering::Relaxed));
    if confirmed == UiBackend::Unknown {
        SessionFacts::from_env().hint()
    } else {
        confirmed
    }
}

/// 是否已从 raw window handle 得到可信后端。
pub fn backend_confirmed() -> bool {
    UiBackend::from_u8(CONFIRMED_BACKEND.load(Ordering::Relaxed)) != UiBackend::Unknown
}

/// 记录 raw handle 得到的事实；返回记录后的当前后端。重复调用幂等。
pub fn confirm(detected: UiBackend) -> UiBackend {
    if detected != UiBackend::Unknown {
        CONFIRMED_BACKEND.store(detected as u8, Ordering::Relaxed);
    }
    backend()
}

/// 从 winit 窗口读取真实后端。`window_handle()` 在窗口刚 show 时可能还不可用，
/// 返回 `Unknown`，调用方应重试（见 main 的 window-ready 轮询）。
pub fn detect(window: &winit::window::Window) -> UiBackend {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else {
        return UiBackend::Unknown;
    };
    match handle.as_raw() {
        RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_) => UiBackend::X11,
        RawWindowHandle::Wayland(_) => UiBackend::Wayland,
        _ => UiBackend::Unknown,
    }
}

/// 每种后端下可用的 X11/桌面能力。X11 后端必须与适配前完全一致（全开）；
/// Wayland 后端只保留协议允许、且 winit 0.30 已实现的能力。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// `xconn` 看到的窗口/屏幕/指针是否属于本进程（X11 窗口语义）。
    pub x11_window_ops: bool,
    /// 全局热键（X11 passive grab）。Wayland 没有等价协议；XWayland grab 对
    /// 原生 Wayland 窗口聚焦时收不到键，注册成功是假阳性。
    pub global_hotkey: bool,
    /// `_NET_WM_ICON`。Wayland 由 `.desktop` + app_id 决定，不能写窗口图标。
    pub window_icon: bool,
    /// 客户端设置窗口全局位置（Quake 的 `x,0` 定位）。
    pub window_positioning: bool,
    /// 文件拖放（XDND）。winit 0.30.13 的 Wayland 后端没有 DnD 实现。
    pub xdnd_file_drop: bool,
    /// X11 SHAPE 自绘圆角。
    pub x11_corners: bool,
    /// X11 最大化状态机（读取 `_NET_WM_STATE`）。
    pub x11_maximize_state: bool,
}

impl Caps {
    pub const fn for_backend(backend: UiBackend) -> Self {
        let x11 = matches!(backend, UiBackend::X11);
        Self {
            x11_window_ops: x11,
            global_hotkey: x11,
            window_icon: x11,
            window_positioning: x11,
            xdnd_file_drop: x11,
            x11_corners: x11,
            x11_maximize_state: x11,
        }
    }
}

pub fn caps() -> Caps {
    Caps::for_backend(backend())
}

/// 剪贴板不可用时的可读解释。只在当前后端是 Wayland 时调用；X11 保持原日志。
pub fn clipboard_unavailable_hint() -> &'static str {
    if has_value(&SessionFacts::from_env().display) {
        "原生 Wayland 剪贴板需要合成器提供 ext-/wlr-data-control（当前 GNOME/Mutter 未提供），\
         且 XWayland 回退失败"
    } else {
        "原生 Wayland 剪贴板需要合成器提供 ext-/wlr-data-control（当前 GNOME/Mutter 未提供），\
         也没有 DISPLAY 可回退 XWayland"
    }
}

/// 首次剪贴板失败时打印一次可读原因；后续失败仍打印短行 `[clipboard] 不可用`，
/// 保持原有 soak/e2e 日志断言不变。
pub fn log_clipboard_unavailable_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if caps().x11_window_ops {
        return;
    }
    ONCE.call_once(|| {
        eprintln!("[clipboard] 不可用：{}", clipboard_unavailable_hint());
    });
}

/// `--diagnose` 用的一行状态：明确区分 hint 与已确认后端，以及 XWayland 数据来源。
pub fn diagnose_summary() -> String {
    let facts = SessionFacts::from_env();
    let confirmed = UiBackend::from_u8(CONFIRMED_BACKEND.load(Ordering::Relaxed));
    format!(
        "ui_backend={} ({}; {}) {}",
        backend().label(),
        if confirmed == UiBackend::Unknown {
            "env hint"
        } else {
            "winit raw handle"
        },
        if confirmed == UiBackend::Unknown {
            "窗口未创建，按 winit env 规则推断"
        } else {
            "已确认"
        },
        facts.summary()
    )
}

/// 文件拖放是否可用；Wayland 下 winit 0.30 不会产生 `HoveredFile`/`DroppedFile`。
pub fn file_drop_supported() -> bool {
    caps().xdnd_file_drop
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(wayland: Option<&str>, display: Option<&str>) -> SessionFacts {
        SessionFacts {
            session_type: Some("wayland".into()),
            desktop: Some("ubuntu:GNOME".into()),
            wayland_display: wayland.map(str::to_string),
            wayland_socket: None,
            display: display.map(str::to_string),
        }
    }

    #[test]
    fn wayland_wins_over_xwayland_display_like_winit() {
        // 真实 Wayland 会话里 DISPLAY 也指向 XWayland；winit 仍然选 Wayland。
        let f = facts(Some("wayland-0"), Some(":0"));
        assert_eq!(f.hint(), UiBackend::Wayland);
        assert!(f.xwayland_likely());
    }

    #[test]
    fn x11_only_when_no_wayland_display() {
        let f = facts(None, Some(":0"));
        assert_eq!(f.hint(), UiBackend::X11);
        assert!(!f.xwayland_likely());
    }

    #[test]
    fn empty_wayland_display_is_ignored() {
        // winit 把空字符串当作未设置；这里必须一致，否则 X11 会被误判成 Wayland。
        let f = facts(Some(""), Some(":0"));
        assert_eq!(f.hint(), UiBackend::X11);
    }

    #[test]
    fn no_display_is_unknown_not_x11() {
        let f = facts(None, None);
        assert_eq!(f.hint(), UiBackend::Unknown);
    }

    #[test]
    fn x11_caps_keep_every_legacy_capability() {
        let caps = Caps::for_backend(UiBackend::X11);
        assert!(caps.x11_window_ops);
        assert!(caps.global_hotkey);
        assert!(caps.window_icon);
        assert!(caps.window_positioning);
        assert!(caps.xdnd_file_drop);
        assert!(caps.x11_corners);
        assert!(caps.x11_maximize_state);
    }

    #[test]
    fn wayland_caps_do_not_claim_x11_only_features() {
        let caps = Caps::for_backend(UiBackend::Wayland);
        assert!(!caps.x11_window_ops);
        assert!(!caps.global_hotkey);
        assert!(!caps.window_icon);
        assert!(!caps.window_positioning);
        assert!(!caps.xdnd_file_drop);
        assert!(!caps.x11_corners);
        assert!(!caps.x11_maximize_state);
    }

    #[test]
    fn app_id_matches_desktop_entry() {
        // packaging/xtervo.desktop: StartupWMClass=xtervo / Icon=xtervo
        assert_eq!(XDG_APP_ID, "xtervo");
    }

    #[test]
    fn clipboard_hint_is_actionable() {
        let hint = clipboard_unavailable_hint();
        assert!(hint.contains("data-control"));
        assert!(hint.contains("XWayland"));
    }
}
