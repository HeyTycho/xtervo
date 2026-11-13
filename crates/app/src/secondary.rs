// SPDX-License-Identifier: GPL-3.0-only
//! 同进程多窗口：`tervo --window` / 第二次 `tervo` 调用 / Ctrl+Shift+N /
//! `window.new` 的默认落点。
//!
//! 每个窗口复用 primary 的 [`crate::runtime::SharedServices`]，只创建自己的
//! Slint `MainWindow`、模型和 `AppState`。完整 UI 回调（键盘/IME、鼠标/选词、
//! tab/pane 菜单、命令面板、设置页、插件页、profile/SFTP/Vault、窗口操作等）
//! 与 primary 共用 [`crate::register_window_callbacks`]；闭包只捕获
//! [`WindowId`]，通过 `runtime::with_window` 显式路由，不共享任何窗口模型。
//!
//! 旧的“每个窗口一个子进程”路径只在显式 fallback 时保留：
//! `TERVO_MULTI_WINDOW_INPROC=0`（或 `TERVO_MULTI_WINDOW_LEGACY=1`）。

use std::time::Duration;

use slint::winit_030::WinitWindowAccessor;
use slint::ComponentHandle;

use crate::instance::OpenWindowRequest;
use crate::runtime::{self, WindowId, WindowRuntime};
use crate::workspace::Rect;
use crate::{MainWindow, TABBAR_H};

/// 旧的多进程窗口路径只是显式 fallback；默认（变量不存在或为真）走同进程。
///
/// `TERVO_MULTI_WINDOW_INPROC=0/false/no/off` 或 `TERVO_MULTI_WINDOW_LEGACY=1`
/// 时，`window.new` 才会 `current_exe --window` 起子进程。单实例 IPC 仍默认
/// 开启；legacy 子进程由 `window::new_window_command` 显式绕过锁。
pub fn legacy_fallback() -> bool {
    legacy_fallback_from(
        std::env::var("TERVO_MULTI_WINDOW_LEGACY").ok(),
        std::env::var("TERVO_MULTI_WINDOW_INPROC").ok(),
    )
}

fn legacy_fallback_from(legacy: Option<String>, inproc: Option<String>) -> bool {
    if legacy.as_deref().is_some_and(env_truthy) {
        return true;
    }
    inproc.as_deref().is_some_and(|value| env_falsey(value))
}

fn env_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn env_falsey(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// 无请求参数的兼容入口：等价于空 [`OpenWindowRequest`]。
pub fn create_inproc_window() -> Result<WindowId, String> {
    create_inproc_window_for(OpenWindowRequest::new())
}

/// 在当前 UI 线程按 primary 转发来的请求创建窗口；必须由事件循环回调调用
/// （不能在 `with_app` 借用期间调用）。
///
/// 返回时窗口已经 `show()`，调用方可以据此 ack secondary。请求里的 `cwd`
/// 只用于新窗口的首个本地 shell（配置了默认 Profile 时仍尊重 Profile）；
/// `profile` 选择尚未接入 UI，会明确报错而不是静默忽略。
pub fn create_inproc_window_for(request: OpenWindowRequest) -> Result<WindowId, String> {
    request.validate()?;
    if request.profile.is_some() {
        return Err("open_window profile selection is not supported yet".into());
    }
    let registry = runtime::registry().ok_or_else(|| "window registry unavailable".to_string())?;
    let (shared, window_id) = {
        let mut reg = registry.borrow_mut();
        let shared = reg.shared();
        let id = reg.allocate_id();
        (shared, id)
    };
    let (opts, fonts) = runtime::with_active_window(|st| (st.opts.clone(), st.fonts.clone()))
        .ok_or_else(|| "no active window".to_string())?;
    let mut opts = opts;
    opts.window = true;
    opts.title = format!("{} — {}", opts.title, window_id.get());
    let cell = (fonts.cell_w, fonts.cell_h);
    let win_w = opts.cols.max(20) as f32 * cell.0 as f32;
    let win_h = opts.rows.max(5) as f32 * cell.1 as f32 + TABBAR_H;
    let content = Rect::new(0.0, 0.0, win_w, win_h - TABBAR_H);
    let initial_theme = {
        let cfg = shared.cfg.with(|cfg| cfg.clone());
        let selected = crate::normalize_selection(&shared.themes, &cfg.theme);
        crate::resolve_theme(
            &shared.themes,
            &selected,
            cfg.auto_dark.as_deref(),
            cfg.auto_light.as_deref(),
        )
    };

    let ui = MainWindow::new().map_err(|err| format!("second MainWindow::new failed: {err}"))?;
    let backend = crate::REQUESTED_BACKEND
        .get()
        .cloned()
        .or_else(|| std::env::var("SLINT_BACKEND").ok())
        .unwrap_or_else(|| "winit-software".to_string());
    crate::render_probe::install_for(window_id, &ui.window(), &backend, false);
    let settings_legacy = std::env::var("TERVO_SETTINGS_LEGACY")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    ui.set_settings_legacy(settings_legacy);
    ui.set_window_title(opts.title.as_str().into());

    let state = crate::build_window_state(
        &ui,
        shared.clone(),
        window_id,
        &opts,
        fonts,
        cell,
        win_w,
        win_h,
        content,
        initial_theme,
    );
    let runtime = WindowRuntime::new(
        window_id,
        ui.clone_strong(),
        state.clone(),
        opts.title.clone(),
    );
    {
        let mut reg = registry.borrow_mut();
        reg.insert(runtime)
            .map_err(|err| format!("register {window_id}: {err}"))?;
        // 不主动抢活动窗口：新窗口只有真正收到 Focused 才成为路由目标，
        // 否则 primary 的旧回调会误改尚未聚焦的新窗口。
    }
    let runtime = {
        let reg = registry.borrow();
        reg.get(window_id)
            .ok_or_else(|| "registered window vanished".to_string())?
    };
    // 完整窗口回调（键盘/IME/鼠标/菜单/设置页/插件/profile/SFTP/窗口操作），
    // 与 primary 共用同一注册函数；回调只捕获 WindowId，经 registry 显式路由。
    crate::register_window_callbacks(&ui, state.clone(), window_id);
    let _ = runtime::with_state(&state, crate::init_window_ui);
    let request_cwd = request.cwd.clone();
    let _ = runtime::with_state(&state, |st| {
        crate::new_tab_with_default_or_cwd(st, request_cwd);
        crate::rebuild_tabs(st);
        crate::refresh_views(st);
        crate::redraw_visible(st);
    });
    start_timers(&runtime);
    let offset = 40 + (runtime::window_count().saturating_sub(1) as i32) * 40;
    runtime
        .ui
        .window()
        .set_position(slint::PhysicalPosition::new(offset, offset));
    // 先装原生事件再 show：首批 Focused/close 事件不能丢。
    crate::install_native_window_events(&runtime.ui, window_id);
    if let Err(err) = runtime.ui.show() {
        // show 失败时不能把半初始化的 WindowRuntime 留在 registry 里：否则
        // primary 会以为窗口存在，后续 open_window/关闭语义都会错。
        let _ = runtime::remove_window(window_id);
        crate::gpu_present::teardown_for(window_id);
        crate::gpu_native::teardown_for(window_id);
        return Err(format!("second window show: {err}"));
    }
    runtime.ui.window().request_redraw();
    // 第二窗口也要有 X11 最大化状态机：否则 on_toggle_maximize 只有 winit
    // fallback，无法和其它窗口/EWMH 同步。
    let initial_maximized = runtime.ui.window().is_maximized();
    runtime.ui.set_window_maximized(initial_maximized);
    let _ = runtime::with_state(&state, |st| st.window_maximized = initial_maximized);
    crate::start_maximize_when_window_ready(&runtime.ui, window_id, 0);
    eprintln!(
        "[ui] in-process window created id={} pid={} shared_plugins={} mcp_socket={}",
        window_id,
        std::process::id(),
        shared.identities()["plugins"],
        shared.mcp.socket_path().display()
    );
    Ok(window_id)
}

fn start_timers(rt: &WindowRuntime) {
    let poll_state = rt.state.clone();
    let poll = slint::Timer::default();
    poll.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(400),
        move || crate::poll_events_for(poll_state.clone()),
    );
    rt.keep_timer(poll);

    let focus_state = rt.state.clone();
    let focus = slint::Timer::default();
    focus.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            let state = focus_state.clone();
            let win = runtime::with_state(&state, |st| {
                st.ui
                    .window()
                    .with_winit_window(|w| w.has_focus())
                    .unwrap_or(true)
            })
            .unwrap_or(true);
            let state = focus_state.clone();
            let _ = runtime::with_state(&state, |st| {
                let was_focused = st.window_focused;
                crate::sync_terminal_focus(st, win);
                if win && !was_focused {
                    st.ime_spot_dirty = true;
                }
                crate::update_ime_spot(st);
            });
        },
    );
    rt.keep_timer(focus);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_path_is_inproc_and_legacy_is_explicit() {
        assert!(!legacy_fallback_from(None, None));
        assert!(!legacy_fallback_from(Some("0".into()), None));
        assert!(!legacy_fallback_from(Some("off".into()), Some("1".into())));
        assert!(legacy_fallback_from(None, Some("0".into())));
        assert!(legacy_fallback_from(None, Some("false".into())));
        assert!(legacy_fallback_from(Some("1".into()), Some("1".into())));
    }
}
