// SPDX-License-Identifier: GPL-3.0-only
//! Phase 2：`TERVO_MULTI_WINDOW_INPROC=1` 下的同进程第二窗口。
//!
//! 第二窗口复用 primary 的 [`crate::runtime::SharedServices`]，只创建自己的
//! Slint `MainWindow`、模型和 `AppState`。当前回调覆盖终端交互核心
//! （键盘/IME、选词、tab、resize、关闭/focus）；设置页/插件面板的完整回调
//! 仍走 primary 注册块，协议层已按 [`WindowId`] 预留路由。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::winit_030::WinitWindowAccessor;
use slint::ComponentHandle;
use tervo_core::{MouseAction, MouseButton, MouseEvent};

use crate::runtime::{self, WindowId, WindowRuntime};
use crate::workspace::{Rect, SPLIT_GAP};
use crate::{AppState, Drag, MainWindow, TABBAR_H};

/// `TERVO_MULTI_WINDOW_INPROC=1`：新窗口走同进程路径，而不是 spawn 新进程。
pub fn enabled() -> bool {
    std::env::var("TERVO_MULTI_WINDOW_INPROC")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// 在当前 UI 线程创建第二窗口；必须由事件循环回调调用（不能在
/// `with_app` 借用期间调用）。
pub fn create_inproc_window() -> Result<WindowId, String> {
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
    let _ = runtime::with_state(&state, |st| {
        crate::new_tab_with_default(st);
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
    runtime
        .ui
        .show()
        .map_err(|err| format!("second window show: {err}"))?;
    crate::install_native_window_events(&runtime.ui, window_id);
    runtime.ui.window().request_redraw();
    eprintln!(
        "[ui] in-process window created id={} pid={} shared_plugins={} mcp_socket={}",
        window_id,
        std::process::id(),
        shared.identities()["plugins"],
        shared.mcp.socket_path().display()
    );
    Ok(window_id)
}

/// 第二窗口的核心回调。所有入口都显式走 `runtime::with_state`，不依赖
/// “当前活动窗口”猜测；回调内部调用的旧 `with_app` 会沿当前窗口作用域路由。
fn install_core_callbacks(ui: &MainWindow, state: Rc<RefCell<AppState>>, _window_id: WindowId) {
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
