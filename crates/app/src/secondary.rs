// SPDX-License-Identifier: GPL-3.0-only
//! 同进程多窗口：`tervo --window` / 第二次 `tervo` 调用 / Ctrl+Shift+N /
//! `window.new` 的默认落点。
//!
//! 每个窗口复用 primary 的 [`crate::runtime::SharedServices`]，只创建自己的
//! Slint `MainWindow`、模型和 `AppState`。当前回调覆盖终端交互核心
//! （键盘/IME、选词、tab、resize、关闭/focus）；设置页/插件面板的完整回调
//! 仍走 primary 注册块，协议层已按 [`WindowId`] 预留路由。
//!
//! 旧的“每个窗口一个子进程”路径只在显式 fallback 时保留：
//! `TERVO_MULTI_WINDOW_INPROC=0`（或 `TERVO_MULTI_WINDOW_LEGACY=1`）。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::winit_030::WinitWindowAccessor;
use slint::ComponentHandle;
use tervo_core::{MouseAction, MouseButton, MouseEvent};

use crate::instance::OpenWindowRequest;
use crate::runtime::{self, WindowId, WindowRuntime};
use crate::workspace::{Rect, SPLIT_GAP};
use crate::{AppState, Drag, MainWindow, TABBAR_H};

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

/// 在当前 UI 线程创建第二窗口；必须由事件循环回调调用（不能在
/// `with_app` 借用期间调用）。默认新建一个 Tab。
pub fn create_inproc_window() -> Result<WindowId, String> {
    create_inproc_window_impl(None, OpenWindowRequest::new())
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
    create_inproc_window_impl(None, request)
}

/// 按 v2 快照恢复一个窗口（geometry/maximized/tabs/splits/focus）。
///
/// 恢复路径不依赖 `TERVO_MULTI_WINDOW_INPROC`：产品启动只要有 v2 多窗口
/// 快照就应恢复全部窗口。单个 Tab 恢复失败由 `crate::restore_window_snapshot`
/// 跳过；一个都没成功时回退一个普通新 Tab，不让整机启动失败。
pub fn create_inproc_window_restored(
    window: &crate::session::WindowSnapshot,
) -> Result<WindowId, String> {
    create_inproc_window_impl(Some(window), OpenWindowRequest::new())
}

fn create_inproc_window_impl(
    restore: Option<&crate::session::WindowSnapshot>,
    request: OpenWindowRequest,
) -> Result<WindowId, String> {
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
    // v2 快照有像素几何时按快照开窗；v1/未知几何回退字符网格默认尺寸。
    let (win_w, win_h) = match restore
        .map(|window| window.geometry)
        .filter(|geometry| geometry.is_known())
    {
        Some(geometry) => (geometry.w as f32, geometry.h as f32),
        None => (
            opts.cols.max(20) as f32 * cell.0 as f32,
            opts.rows.max(5) as f32 * cell.1 as f32 + TABBAR_H,
        ),
    };
    let content = Rect::new(0.0, 0.0, win_w, (win_h - TABBAR_H).max(1.0));
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
    install_core_callbacks(&ui, state.clone(), window_id);
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
    let _ = runtime::with_state(&state, crate::init_window_ui);
    let request_cwd = request.cwd.clone();
    let restored = restore
        .map(|window| {
            runtime::with_state(&state, |st| crate::restore_window_snapshot(st, window))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let _ = runtime::with_state(&state, |st| {
        if !restored {
            crate::new_tab_with_default_or_cwd(st, request_cwd);
        }
        crate::rebuild_tabs(st);
        crate::refresh_views(st);
        crate::redraw_visible(st);
    });
    start_timers(&runtime);
    // 恢复窗口用快照位置；新窗口走层叠偏移。
    let position = restore
        .map(|window| window.geometry)
        .filter(|geometry| geometry.is_known())
        .map(|geometry| (geometry.x, geometry.y))
        .unwrap_or_else(|| {
            let offset = 40 + (runtime::window_count().saturating_sub(1) as i32) * 40;
            (offset, offset)
        });
    runtime
        .ui
        .window()
        .set_position(slint::PhysicalPosition::new(position.0, position.1));
    // 先装原生事件再 show：首批 Focused/close 事件不能丢。
    crate::install_native_window_events(&runtime.ui, window_id);
    if let Err(err) = runtime.ui.show() {
        // show 失败时不能把半初始化的 WindowRuntime 留在 registry 里。
        let _ = runtime::remove_window(window_id);
        crate::gpu_present::teardown_for(window_id);
        crate::gpu_native::teardown_for(window_id);
        return Err(format!("second window show: {err}"));
    }
    if let Some(window) = restore {
        crate::restore_window_geometry(&runtime.ui, window_id, window);
    }
    // 第二窗口不启动 maximize 状态机：Slint 1.18 的 WinitWindowAccessor 在
    // 多窗口下可能返回另一个窗口的 X11 handle，状态机会操作错窗口。
    runtime.ui.window().request_redraw();
    // 新窗口/恢复窗口都触发一次布局保存。恢复期间 SharedServices 会因
    // restoring=true 跳过，避免半恢复布局覆盖源快照。
    let _ = runtime::save_session_snapshot("window-new", false);
    eprintln!(
        "[ui] in-process window created id={} restored={} pid={} shared_plugins={} mcp_socket={}",
        window_id,
        restored,
        std::process::id(),
        shared.identities()["plugins"],
        shared.mcp.socket_path().display()
    );
    Ok(window_id)
}

/// 第二窗口的核心回调。所有入口都显式走 `runtime::with_state`，不依赖
/// “当前活动窗口”猜测；回调内部调用的旧 `with_app` 会沿当前窗口作用域路由。
fn install_core_callbacks(ui: &MainWindow, state: Rc<RefCell<AppState>>, window_id: WindowId) {
    let keylog = std::env::var("TERVO_KEYLOG").is_ok();
    let mouselog = std::env::var("TERVO_MOUSELOG").is_ok();

    {
        let state = state.clone();
        ui.on_key_input(move |text, ctrl, alt, shift, meta, repeat| {
            let text = text.to_string();
            let kind = if repeat {
                crate::keys::KeyEventKind::Repeat
            } else {
                crate::keys::KeyEventKind::Press
            };
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                crate::handle_input(st, &text, ctrl, alt, shift, meta, kind, keylog)
            });
        });
    }
    {
        let state = state.clone();
        ui.on_key_release(move |text, ctrl, alt, shift, meta| {
            let text = text.to_string();
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                crate::handle_key_release(st, &text, ctrl, alt, shift, meta, keylog)
            });
        });
    }
    {
        let state = state.clone();
        ui.on_ime_commit(move |text| {
            let text = text.to_string();
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                crate::handle_input(
                    st,
                    &text,
                    false,
                    false,
                    false,
                    false,
                    crate::keys::KeyEventKind::Press,
                    keylog,
                )
            });
        });
    }

    {
        let state = state.clone();
        ui.on_pane_clicked(move |id| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| crate::focus_pane_inner(st, id as u64));
        });
    }

    {
        let state = state.clone();
        ui.on_pane_mouse_down(move |id, x, y, button, mods| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                let id = id as u64;
                if st.search_open {
                    crate::close_search(st);
                }
                crate::focus_pane_inner(st, id);
                let (cell, wants_mouse) = {
                    let Some(rt) = st.panes.get(&id) else { return };
                    (crate::cell_at(rt, x, y), rt.handle.wants_mouse())
                };
                let modifiers = crate::mouse_mods(mods);
                let reporting = wants_mouse && !modifiers.shift;
                if crate::ime_click_anchor(button, wants_mouse, modifiers.shift) {
                    st.ime_override = Some((id, cell));
                    st.ime_spot_dirty = true;
                    crate::update_ime_spot(st);
                }
                let Some(rt) = st.panes.get(&id) else { return };
                if mouselog {
                    eprintln!(
                        "[mouse] secondary down pane={id} cell={cell:?} button={button} \
                         wants={} reporting={reporting}",
                        rt.handle.wants_mouse()
                    );
                }
                let link = !reporting && modifiers.ctrl;
                st.drag_autoscroll.stop();
                st.drag = None;
                if reporting {
                    rt.handle.selection_clear();
                    rt.handle.mouse(MouseEvent {
                        button: crate::mouse_button(button),
                        action: MouseAction::Press,
                        line: cell.0,
                        col: cell.1,
                        modifiers,
                    });
                } else if !link {
                    rt.handle.selection_start(cell.0, cell.1);
                }
                st.drag = Some(Drag {
                    pane: id,
                    start: (x, y),
                    moved: false,
                    cell,
                    mouse: reporting,
                    link,
                    last_pointer: (x, y),
                    autoscroll_dir: 0,
                    autoscroll_paused: false,
                });
                if !reporting && !link {
                    crate::start_drag_autoscroll_timer(st);
                }
            });
        });
    }

    {
        let state = state.clone();
        ui.on_pane_mouse_move(move |id, x, y, _button, mods| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                let id = id as u64;
                let modifiers = crate::mouse_mods(mods);
                let Some(drag) = st.drag.as_mut() else { return };
                if drag.pane != id {
                    return;
                }
                drag.last_pointer = (x, y);
                if (x - drag.start.0).abs() > 4.0 || (y - drag.start.1).abs() > 4.0 {
                    drag.moved = true;
                }
                let (forwarding, link, changed, cell) = {
                    let Some(rt) = st.panes.get(&id) else { return };
                    let cell = crate::cell_at(rt, x, y);
                    (drag.mouse, drag.link, drag.cell != cell, cell)
                };
                if changed {
                    drag.cell = cell;
                }
                if link || !changed {
                    return;
                }
                if forwarding {
                    if let Some(rt) = st.panes.get(&id) {
                        rt.handle.mouse(MouseEvent {
                            button: MouseButton::Left,
                            action: MouseAction::Motion,
                            line: cell.0,
                            col: cell.1,
                            modifiers,
                        });
                    }
                } else {
                    if let Some(rt) = st.panes.get(&id) {
                        rt.handle.selection_extend(cell.0, cell.1);
                    }
                    crate::redraw_pane_inner(st, id);
                }
            });
        });
    }

    {
        let state = state.clone();
        ui.on_pane_mouse_up(move |id, x, y, button, mods| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                let id = id as u64;
                st.drag_autoscroll.stop();
                let Some(drag) = st.drag.take() else { return };
                if drag.mouse {
                    if let Some(rt) = st.panes.get(&id) {
                        let cell = crate::cell_at(rt, x, y);
                        rt.handle.mouse(MouseEvent {
                            button: crate::mouse_button(button),
                            action: MouseAction::Release,
                            line: cell.0,
                            col: cell.1,
                            modifiers: crate::mouse_mods(mods),
                        });
                    }
                    return;
                }
                if drag.link {
                    return;
                }
                if !drag.moved {
                    let Some(rt) = st.panes.get(&id) else { return };
                    let (line_u, col_u) = crate::cell_at(rt, x, y);
                    let (line, col) = (line_u as i32, col_u as i32);
                    let now = std::time::Instant::now();
                    let is_double = matches!(st.last_click, Some((p, l, c, t))
                        if p == id && l == line && c == col
                            && now.duration_since(t) < std::time::Duration::from_millis(400));
                    if is_double {
                        st.last_click = None;
                        if let Some(rt) = st.panes.get_mut(&id) {
                            rt.handle.select_line(line);
                        }
                        if st.cfg.copy_on_select {
                            crate::copy_current_selection(st);
                        }
                        return;
                    }
                    st.last_click = Some((id, line, col, now));
                    return;
                }
                if !st.cfg.copy_on_select {
                    return;
                }
                let text = st
                    .panes
                    .get(&drag.pane)
                    .and_then(|rt| rt.handle.copy_selection())
                    .and_then(crate::normalize_clipboard_text);
                if let Some(text) = text {
                    crate::set_selection_clipboards(st, &text);
                }
            });
        });
    }

    {
        let state = state.clone();
        ui.on_pane_scrolled(move |id, dy, x, y, mods| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                let id = id as u64;
                let Some(rt) = st.panes.get(&id) else { return };
                let modifiers = crate::mouse_mods(mods);
                if rt.handle.wants_mouse() && !modifiers.shift {
                    let cell = crate::cell_at(rt, x, y);
                    let button = if dy > 0.0 {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };
                    rt.handle.mouse(MouseEvent {
                        button,
                        action: MouseAction::Press,
                        line: cell.0,
                        col: cell.1,
                        modifiers,
                    });
                    return;
                }
                let ch = rt.renderer.cell_h.max(1) as f32;
                let mut lines = (dy / ch).round();
                if lines.abs() < 1.0 {
                    lines = dy.signum();
                }
                if lines != 0.0 {
                    rt.handle.clear_selection();
                    rt.handle.scroll(lines as i32);
                }
            });
        });
    }

    {
        let state = state.clone();
        ui.on_divider_dragged(move |index, x, y| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                let Some(info) = st.divider_infos.get(index as usize).cloned() else {
                    return;
                };
                let usable = match info.dir {
                    crate::workspace::Dir::Row => info.parent.w - SPLIT_GAP,
                    crate::workspace::Dir::Col => info.parent.h - SPLIT_GAP,
                };
                if usable <= 1.0 {
                    return;
                }
                let ratio = match info.dir {
                    crate::workspace::Dir::Row => (x - info.parent.x) / usable,
                    crate::workspace::Dir::Col => (y - info.parent.y) / usable,
                };
                let Some(tab) = st.ws.tabs.get_mut(st.ws.active) else {
                    return;
                };
                if let Some(root) = tab.root.as_mut() {
                    root.set_ratio(&info.path, ratio.clamp(0.05, 0.95));
                }
                crate::refresh_views(st);
                crate::redraw_visible(st);
            });
        });
    }

    {
        let state = state.clone();
        ui.on_tab_clicked(move |i| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                if st.search_open {
                    crate::close_search(st);
                }
                crate::set_active_tab_inner(st, i.max(0) as usize);
            });
        });
    }
    {
        let state = state.clone();
        ui.on_tab_closed(move |i| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                crate::close_tab_inner(st, i.max(0) as usize);
                crate::rebuild_tabs(st);
            });
        });
    }
    {
        let state = state.clone();
        ui.on_new_tab(move || {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                if st.search_open {
                    crate::close_search(st);
                }
                crate::new_tab_with_default(st);
                crate::rebuild_tabs(st);
                crate::refresh_views(st);
                crate::redraw_visible(st);
            });
        });
    }
    {
        let state = state.clone();
        ui.on_resized(move |w, h| {
            let state = state.clone();
            let _ = runtime::with_state(&state, |st| {
                let w = w.max(1.0);
                let h = (h - TABBAR_H).max(1.0);
                if (st.content.w - w).abs() < 0.5 && (st.content.h - h).abs() < 0.5 {
                    return;
                }
                st.content = Rect::new(0.0, 0.0, w, h);
                st.ime_spot_dirty = true;
                crate::refresh_views(st);
                crate::redraw_visible(st);
            });
        });
    }

    // 生命周期回调：第二窗口的关闭按钮/确认框/最大化必须作用在自己身上，
    // 不能沿用 primary 的旧 `with_app` 缺省路由。
    {
        let state = state.clone();
        ui.on_close_window(move || {
            let state = state.clone();
            let _ = runtime::with_state(&state, crate::request_close);
        });
    }
    {
        let state = state.clone();
        ui.on_confirm_accept(move || {
            let state = state.clone();
            let _ = runtime::with_state(&state, crate::confirm_accept_inner);
        });
    }
    {
        let state = state.clone();
        ui.on_confirm_cancel(move || {
            let state = state.clone();
            let _ = runtime::with_state(&state, crate::confirm_cancel_inner);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_maximize(move || {
            if let Some(ui) = weak.upgrade() {
                crate::toggle_maximize_for(&ui, window_id);
            }
        });
    }
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
                // 第二窗口没有独立的 maximize 状态机（WinitWindowAccessor 在
                // 多窗口下可能给错 X11 handle）。用本窗口的 winit 状态轮询：
                // 用户点 WM 标题栏最大化/还原、恢复 snapshot 后的 WM 异步确认
                // 都能回写到 session 的 maximized 字段。
                let maximized = st.ui.window().is_maximized();
                if st.window_maximized != maximized {
                    st.window_maximized = maximized;
                    st.ui.set_window_maximized(maximized);
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
