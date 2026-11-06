// SPDX-License-Identifier: GPL-3.0-only
mod activity;
mod askpass;
mod commands;
mod config;
mod glyph;
mod keys;
mod latency;
mod logging;
mod plugin;
mod profile;
mod render;
mod quake;
mod session;
mod sftp;
mod tabby_import;
mod themes;
mod window;
mod vault;
mod workspace;
mod xconn;

slint::include_modules!();

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tervo_core::{
    spawn, CoreEvent, Frame, Modifiers, MouseAction, MouseButton, MouseEvent, Notification,
    PaneConfig, PaneHandle, ProgressState, Theme, UiColors,
};
use slint::winit_030::{winit, WinitWindowAccessor};
use slint::{Model, ModelRc, VecModel};
use x11rb::connection::Connection;
use x11rb::rust_connection::RustConnection;

use crate::config::Config;
use crate::glyph::FontSet;
use crate::latency::{DamageWatcher, Injector, LatencyStats};
use crate::render::Renderer;
use crate::themes::{resolve_auto, ThemeData, ThemeRegistry, Variant};
use crate::workspace::{Dir, Divider, PaneId, Rect, Tab, Workspace, SPLIT_GAP};

/// 延迟探针使用的高辨识度字符（避免与 shell 提示符冲突）。
const MARKER: char = 'z';
const TITLE: &str = "Tervo";
/// 必须与 ui/main.slint 里的 `tabbar-height` 保持一致。
const TABBAR_H: f32 = 32.0;
/// `TermMode::BRACKETED_PASTE`
const BRACKETED_PASTE: u32 = 1 << 4;

struct PaneRuntime {
    handle: PaneHandle,
    renderer: Renderer,
    frame: Box<Frame>,
    image: slint::Image,
    grid: (usize, usize),
    rect: Rect,
    title: String,
    /// 进度 / 未读通知 / 完成标记，rebuild_tabs 时按 Tab 聚合。
    activity: activity::PaneActivity,
    /// 最近一条通知；切到该 Tab 时清空。
    notification: Option<Notification>,
    /// SSH 密码认证用的临时 askpass 脚本；随 pane 一起释放。
    _askpass: Option<askpass::Askpass>,
}

struct Drag {
    pane: PaneId,
    start: (f32, f32),
    moved: bool,
    cell: (u16, u16),
    /// true = 事件已转发给 PTY（终端在鼠标上报模式），本地上屏不再做选词。
    mouse: bool,
}

/// 自绘拖边缩放：按 root 坐标差值直接改窗口大小/位置。
#[derive(Clone, Copy)]
struct WindowResize {
    dir: i32,
    pointer: (i32, i32),
    pos: (i32, i32),
    size: (u32, u32),
    /// 按下时刻；WM 路径用它给 WM 一个接管窗口，超时仍未改尺寸就回退。
    since: Instant,
}

struct ResizeCtx {
    conn: RustConnection,
    root: u32,
}

struct AppState {
    ui: MainWindow,
    ws: Workspace,
    panes: HashMap<PaneId, PaneRuntime>,
    pane_model: Rc<VecModel<PaneView>>,
    tab_model: Rc<VecModel<TabView>>,
    pane_rows: HashMap<PaneId, usize>,
    divider_model: Rc<VecModel<DividerView>>,
    /// 与 divider_model 同序的几何/路径信息，拖拽时用。
    divider_infos: Vec<Divider>,
    fonts: Rc<FontSet>,
    cell: (usize, usize),
    echo: Arc<AtomicUsize>,
    opts: Options,
    content: Rect,
    next_title: usize,
    cfg: Config,
    /// 当前实际生效的主题（`cfg.theme == "auto"` 时是解析后的结果）。
    resolved_theme: ThemeData,
    /// 内置 + `themes/*.toml` 自定义主题。
    registry: ThemeRegistry,
    /// 是否已启动过系统配色监听线程（避免重复 spawn）。
    auto_watch_started: bool,
    search_open: bool,
    search_query: String,
    /// 命令表（Command Palette / 快捷键分发共用）
    commands: commands::Registry,
    /// Palette 当前展示项对应的命令 ID（与 palette_model 同序）
    palette_ids: Vec<&'static str>,
    palette_model: Rc<VecModel<slint::SharedString>>,
    /// Tab 搜索当前展示项对应的 ws.tabs 下标
    tab_search_indices: Vec<usize>,
    tab_search_model: Rc<VecModel<slint::SharedString>>,
    tab_search_query: String,
    palette_sel: usize,
    drag: Option<Drag>,
    /// 等待用户确认的多行粘贴内容
    pending_paste: Option<String>,
    /// 后台会话：关 Tab/Pane 时按配置保留的运行时（不 drop，子进程继续跑）
    detached: Vec<(PaneId, PaneRuntime, String)>,
    bg_model: Rc<VecModel<slint::SharedString>>,
    profiles: profile::ProfileStore,
    profile_model: Rc<VecModel<ProfileRow>>,
    /// 过滤后各列表行在 `profiles.profiles` 中的原始下标（点击/编辑/删除按此映射）。
    profile_indices: Vec<usize>,
    /// Profiles 面板搜索框当前内容（空 = 显示全部）。
    profile_query: String,
    /// 加密凭据容器：启动时锁定，需在设置 → Vault 里解锁。
    vault: vault::Vault,
    /// Clear vault 需要点两次确认。
    vault_clear_armed: bool,
    /// SFTP 面板里可选的 SSH Profile 索引（打开面板时快照）
    sftp_profiles: Vec<usize>,
    sftp_model: Rc<VecModel<slint::SharedString>>,
    hotkey_model: Rc<VecModel<HotkeyRow>>,
    /// 与 hotkey_model 同序的命令 ID
    hotkey_ids: Vec<&'static str>,
    /// 正在录制的命令 ID；Some 时所有按键只用于录制
    hotkey_recording: Option<&'static str>,
    plugins: plugin::PluginStore,
    plugin_model: Rc<VecModel<slint::SharedString>>,
    /// Plugins 设置页「最近插件活动」模型（新→旧）。
    plugin_activity_model: Rc<VecModel<slint::SharedString>>,
    /// 与 `PluginStore` 活动日志 revision 对齐，避免每 400ms 无谓刷新 Slint model。
    plugin_activity_revision: u64,
    /// Delete 需要点两次确认（避免误删远端文件）
    sftp_delete_armed: bool,
    /// 进行中的上传/下载；同一时间只允许一个。
    sftp_transfer: Option<sftp::TransferJob>,
    /// 传输状态机（进度事件在 poll_events 里消费后写入 UI）。
    sftp_transfer_state: sftp::TransferState,
    /// Quake 模式窗口当前是否可见；以及进入 Quake 前的普通几何（退出时恢复）
    quake_visible: bool,
    quake_saved: Option<(slint::PhysicalPosition, slint::PhysicalSize)>,
    /// “+” 下拉里各 Profile 在 store 中的下标（第 0 项是默认新 Tab）
    profile_menu_indices: Vec<usize>,
    profile_menu_model: Rc<VecModel<slint::SharedString>>,
    /// 右键按下的 pane/时间/内容区坐标：paste/clipboard 模式用抬起时间判断长按。
    right_down: Option<(PaneId, std::time::Instant, f32, f32)>,
    /// 广播输入：开启后键入同时发给当前 Tab 的所有 pane
    broadcast: bool,
    /// 上次左键点击 (pane, 行, 列, 时间)：用于 400ms 内的双击选行
    last_click: Option<(PaneId, i32, i32, std::time::Instant)>,
    /// 最近关闭的 Tab（最多 10 个），用于 Ctrl+Shift+T / 菜单重开
    closed_tabs: Vec<session::TabSnapshot>,
    fwd_profile_indices: Vec<usize>,
    fwd_model: Rc<VecModel<slint::SharedString>>,
    settings_result_cats: Vec<i32>,
    settings_model: Rc<VecModel<slint::SharedString>>,
    /// 1003 悬停上报的上一格（按格去重，避免鼠标移动刷爆有界 ctrl 队列）。
    hover_cell: Option<(PaneId, (u16, u16))>,
    resize: Option<WindowResize>,
    /// 本次边/角拖动已交给 WM/compositor（winit drag_resize）。
    /// true 时 drag 回调不再每帧 query_pointer + set_size；若 150ms 内 WM 没有
    /// 产生尺寸变化，drag 回调会解除它并回退自绘，保证 resize 不会“按了没反应”。
    wm_resize: bool,
    /// WM 接管后指针被它 grab，Slint 收不到真实 ButtonRelease，TouchArea 会一直
    /// pressed/grabbed；收到第一帧 WM resize 后补发一次 PointerReleased 清理状态。
    wm_release_pending: bool,
    xresize: Option<ResizeCtx>,
    /// 窗口拖动 resize 的 fast path：只更新 pane 矩形并拉伸旧帧，140ms 停下后
    /// 才 resize terminal grid / PTY / 软件重绘。
    resizing: bool,
    resize_gen: u64,
    resize_events: u32,
    resize_applied: u32,
    resize_max_layout_us: u128,
    resize_pending: Option<(f32, f32)>,
    resize_apply_scheduled: bool,
    resize_apply: slint::Timer,
    /// X11 SHAPE 自适应圆角 worker；None = 未启动（X11 连接/线程失败，窗口保持直角）。
    corners: Option<xconn::CornerShaper>,
    clipboard: Option<arboard::Clipboard>,
}

thread_local! {
    static APP: RefCell<Option<Rc<RefCell<AppState>>>> = const { RefCell::new(None) };
}

/// 应用内管线打点：key 回调 → 帧交到 Slint（不含 winit 帧节流与上屏）。
static LAST_KEY: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
static PIPE_SAMPLES: std::sync::Mutex<Vec<Duration>> = std::sync::Mutex::new(Vec::new());
static PIPE_ON: AtomicBool = AtomicBool::new(false);
/// 跟随系统的 `gsettings monitor` 子进程；退出事件循环时 kill，避免留下孤儿进程。
static SCHEME_WATCH_CHILD: std::sync::Mutex<Option<std::process::Child>> =
    std::sync::Mutex::new(None);
static SCHEME_WATCH_STOPPED: AtomicBool = AtomicBool::new(false);

fn with_app<R>(f: impl FnOnce(&mut AppState) -> R) -> Option<R> {
    let app = APP.with(|c| c.borrow().clone())?;
    // 防御性：正常路径不应重入；万一将来某个回调在借用期内又调 with_app，
    // 这里降级为“忽略本次操作 + 报错”，而不是整个进程 panic。
    let mut st = match app.try_borrow_mut() {
        Ok(st) => st,
        Err(_) => {
            eprintln!("[app] with_app 重入（AppState 已被借用），忽略本次操作");
            return None;
        },
    };
    Some(f(&mut st))
}

// ------------------------------------------------------------------ 主题

/// 配置里的 `theme = "auto"`：跟随系统深/浅色。
const AUTO_THEME: &str = "auto";

fn is_auto_theme(selected: &str) -> bool {
    selected.trim().eq_ignore_ascii_case(AUTO_THEME)
}

/// 把系统检测输出解析成「是否深色」（纯函数，便于单测）。
///
/// 优先级：GNOME `color-scheme` → `gtk-theme` → `COLORFGBG` → 默认深色。
fn scheme_from_outputs(color_scheme: &str, gtk_theme: &str, colorfgbg: Option<&str>) -> bool {
    if let Some(is_dark) = scheme_from_color_scheme(color_scheme) {
        return is_dark;
    }
    if let Some(is_dark) = scheme_from_gtk_theme(gtk_theme) {
        return is_dark;
    }
    colorfgbg.and_then(scheme_from_colorfgbg).unwrap_or(true)
}

fn scheme_from_color_scheme(output: &str) -> Option<bool> {
    let value = output.trim().to_ascii_lowercase();
    if value.contains("prefer-dark") {
        Some(true)
    } else if value.contains("prefer-light") || value.contains("default") {
        Some(false)
    } else {
        None
    }
}

fn scheme_from_gtk_theme(output: &str) -> Option<bool> {
    let value = output.trim().to_ascii_lowercase();
    if value.contains("dark") {
        Some(true)
    } else if value.contains("light") {
        Some(false)
    } else {
        None
    }
}

/// `COLORFGBG` 形如 `15;0`（前景;背景），最后一段是背景 ANSI 索引；
/// 0-6 与 8 视为深色，7/9-15 视为浅色。
fn scheme_from_colorfgbg(value: &str) -> Option<bool> {
    let last = value.rsplit(';').next()?.trim();
    let index: u8 = last.parse().ok()?;
    Some(index <= 6 || index == 8)
}

fn gsettings_get(key: &str) -> Option<String> {
    let output = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", key])
        .output()
        .ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 检测系统当前是否深色。gsettings 不可用（非 GNOME/无 dconf）时逐级回退。
fn detect_system_dark() -> bool {
    let color_scheme = gsettings_get("color-scheme").unwrap_or_default();
    let gtk_theme = if scheme_from_color_scheme(&color_scheme).is_some() {
        String::new()
    } else {
        gsettings_get("gtk-theme").unwrap_or_default()
    };
    let colorfgbg = std::env::var("COLORFGBG").ok();
    scheme_from_outputs(&color_scheme, &gtk_theme, colorfgbg.as_deref())
}

/// 把配置里的选择解析成实际生效的主题。
/// - `auto`：按系统深浅 + `auto_dark`/`auto_light`（可被 pair 补齐）
/// - 具体 id：找不到就回退内置默认（不改写配置）
fn resolve_theme(
    reg: &ThemeRegistry,
    selected: &str,
    auto_dark: Option<&str>,
    auto_light: Option<&str>,
) -> ThemeData {
    if is_auto_theme(selected) {
        let want = if detect_system_dark() { Variant::Dark } else { Variant::Light };
        resolve_auto(reg, want, auto_dark, auto_light)
    } else {
        reg.get(selected).cloned().unwrap_or_else(|| reg.dark_default().clone())
    }
}

/// 启动时把配置里的选择归一化成设置面板能高亮的 id（兼容旧的 mocha/latte 别名）。
fn normalize_selection(reg: &ThemeRegistry, selected: &str) -> String {
    if is_auto_theme(selected) {
        return AUTO_THEME.to_string();
    }
    if reg.get(selected).is_some() {
        return selected.to_string();
    }
    let alias = Theme::from_name(selected).id();
    if reg.get(alias).is_some() {
        return alias.to_string();
    }
    reg.dark_default().id.clone()
}

/// core 的 0xRRGGBB → Slint color，并映射成界面全局配色。
fn slint_theme(c: &UiColors) -> ThemeColors {
    let color = |v: u32| slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8);
    ThemeColors {
        bg: color(c.bg),
        surface: color(c.surface),
        raise: color(c.raise),
        hover: color(c.hover),
        border: color(c.border),
        text: color(c.text),
        subtext: color(c.subtext),
        muted: color(c.muted),
        accent: color(c.accent),
        accent_text: color(c.accent_text),
        danger: color(c.danger),
    }
}

/// Tabby 的默认配置路径（可用 TERVO_TABBY_CONFIG 覆盖，便于测试/多实例）。
fn tabby_config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TERVO_TABBY_CONFIG") {
        return std::path::PathBuf::from(p);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
        });
    base.join("tabby").join("config.yaml")
}

/// 设置搜索索引：(分类下标, 展示名, 关键词)。
const SETTINGS_CATALOG: &[(i32, &str, &str)] = &[
    (0, "Theme / Color scheme", "theme color scheme appearance dark light"),
    (0, "Background image", "background image wallpaper picture path 背景 壁纸"),
    (0, "Background opacity", "background opacity transparency alpha 背景 透明 不透明度"),
    (1, "Font size", "font size text zoom"),
    (1, "Scrollback", "scrollback history lines"),
    (1, "Copy on select", "copy select clipboard"),
    (1, "Confirm before closing", "confirm close window exit"),
    (1, "Confirm multi-line paste", "paste multiline confirm"),
    (2, "Tab bar at bottom", "tab bar position bottom top"),
    (3, "Config file path", "config path file folder"),
    (4, "Hotkey bindings", "hotkey shortcut key binding"),
    (5, "Installed plugins", "plugin plugins installed list"),
    (6, "Vault / master password", "vault master password credentials 凭据 解锁 锁定"),
];

/// 设置搜索：大小写不敏感的子串匹配（名称或关键词），返回 (分类, 展示行)。
fn settings_search(query: &str) -> Vec<(i32, String)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let cats = ["Appearance", "Terminal", "Window", "Config", "Hotkeys", "Plugins", "Vault"];
    SETTINGS_CATALOG
        .iter()
        .filter(|(_, label, keywords)| {
            label.to_lowercase().contains(&q) || keywords.to_lowercase().contains(&q)
        })
        .map(|(cat, label, _)| (*cat, format!("{} · {label}", cats[*cat as usize])))
        .collect()
}

/// Hotkeys 页数据：按分类/标题排序，返回 (摘要, 行, 行对应的命令 ID)。
fn hotkey_rows(
    reg: &commands::Registry,
    overrides: &[commands::HotkeyOverride],
) -> (String, Vec<HotkeyRow>, Vec<&'static str>) {
    let conflicts = reg.effective_conflicts(overrides);
    let mut cmds: Vec<&commands::Command> = reg.all().iter().collect();
    cmds.sort_by_key(|c| (c.category.label(), c.title));
    let bound = cmds
        .iter()
        .filter(|c| reg.effective_shortcut(c.id, overrides).is_some())
        .count();
    let custom = cmds
        .iter()
        .filter(|c| {
            reg.override_for(c.id, overrides)
                .is_some_and(|o| commands::Chord::parse(&o.chord).is_ok())
        })
        .count();
    let summary = format!("{} commands · {bound} bound · {custom} custom", cmds.len());
    let rows: Vec<HotkeyRow> = cmds
        .iter()
        .map(|c| {
            let shortcut = reg.effective_shortcut(c.id, overrides);
            let custom = reg
                .override_for(c.id, overrides)
                .is_some_and(|o| commands::Chord::parse(&o.chord).is_ok());
            let conflict = shortcut
                .as_ref()
                .is_some_and(|s| conflicts.iter().any(|c| c.shortcut == *s));
            let chord: slint::SharedString = shortcut
                .map(|s| s.to_string())
                .unwrap_or_else(|| "(none)".to_string())
                .into();
            HotkeyRow {
                command: c.id.into(),
                title: c.title.into(),
                chord,
                custom,
                conflict,
            }
        })
        .collect();
    let ids = cmds.iter().map(|c| c.id).collect();
    (summary, rows, ids)
}

/// 重新计算 Hotkeys 页（保存/重置后调用），并同步录制行状态。
fn refresh_hotkeys(st: &mut AppState) {
    let (summary, rows, ids) = hotkey_rows(&st.commands, &st.cfg.hotkeys);
    st.hotkey_ids = ids;
    st.hotkey_model.set_vec(rows);
    st.ui.set_hotkey_summary(summary.as_str().into());
    let conflicts = st.commands.effective_conflicts(&st.cfg.hotkeys).len();
    st.ui.set_hotkey_conflict_count(conflicts as i32);
    let recording = st
        .hotkey_recording
        .and_then(|id| st.hotkey_ids.iter().position(|x| *x == id))
        .map(|i| i as i32)
        .unwrap_or(-1);
    st.ui.set_hotkey_recording_row(recording);
}

/// 刷新 Plugins 设置页：已安装清单、统计、目录与最近活动。
fn refresh_plugin_page(st: &mut AppState) {
    let lines: Vec<slint::SharedString> =
        st.plugins.display_lines().iter().map(|line| line.as_str().into()).collect();
    st.plugin_model.set_vec(lines);
    let summary = format!(
        "{} plugins · {} enabled · {} commands · {} skipped",
        st.plugins.total_count(),
        st.plugins.enabled_count(),
        st.plugins.enabled_command_count(),
        st.plugins.skipped.len()
    );
    st.ui.set_plugin_summary(summary.as_str().into());
    st.ui.set_plugin_dir(plugin::PluginStore::dir().display().to_string().as_str().into());
    refresh_plugin_activity(st, true);
}

/// 活动列表变化时刷新 Slint model；`force` 用于首次填充。
///
/// 后台线程只往 `PluginStore` 的 ActivityLog push，UI 线程在 poll_events 里按
/// revision 拉取，避免跨线程碰 Slint model。
fn refresh_plugin_activity(st: &mut AppState, force: bool) {
    let (revision, lines) = st.plugins.activity_snapshot();
    if !force && revision == st.plugin_activity_revision {
        return;
    }
    st.plugin_activity_revision = revision;
    let rows: Vec<slint::SharedString> = lines.iter().map(|line| line.as_str().into()).collect();
    st.plugin_activity_model.set_vec(rows);
}

/// 异步分发一个插件事件；spawn 失败/插件执行失败都只写日志与活动列表。
fn dispatch_plugin_event(st: &mut AppState, event: plugin::PluginEvent, payload: serde_json::Value) {
    let spawned = st.plugins.spawn_event(event, payload);
    if spawned > 0 {
        eprintln!("[plugin] 事件 {} 已提交后台执行（{} 个插件）", event.as_str(), spawned);
    }
}

fn hotkey_title(reg: &commands::Registry, id: &str) -> String {
    reg.get(id).map(|c| c.title.to_string()).unwrap_or_else(|| id.to_string())
}

/// 进入录制：先让终端 IME 宿主拿回焦点，否则按键会落在设置搜索框里。
fn start_hotkey_edit(st: &mut AppState, index: i32) {
    let Some(id) = st.hotkey_ids.get(index.max(0) as usize).copied() else { return };
    let title = hotkey_title(&st.commands, id);
    st.hotkey_recording = Some(id);
    st.ui.set_hotkey_error("".into());
    st.ui.set_hotkey_status(format!("录制「{title}」：按下新的组合键").into());
    st.ui.set_hotkey_recording_row(index.max(0));
    st.ui.invoke_focus_terminal();
    eprintln!("[hotkey] record start {id}");
}

fn cancel_hotkey_edit(st: &mut AppState) {
    if st.hotkey_recording.take().is_some() {
        st.ui.set_hotkey_status("已取消录制".into());
        eprintln!("[hotkey] record cancel");
    }
    st.ui.set_hotkey_error("".into());
    st.ui.set_hotkey_recording_row(-1);
    st.ui.invoke_focus_terminal();
}

fn reset_hotkey(st: &mut AppState, index: i32) {
    let Some(id) = st.hotkey_ids.get(index.max(0) as usize).copied() else { return };
    let title = hotkey_title(&st.commands, id);
    let removed = commands::clear_override(&mut st.cfg.hotkeys, id);
    if removed {
        st.cfg.save();
    }
    if st.hotkey_recording == Some(id) {
        st.hotkey_recording = None;
        st.ui.invoke_focus_terminal();
    }
    st.ui.set_hotkey_error("".into());
    st.ui.set_hotkey_status(format!("{title} 已恢复默认").into());
    refresh_hotkeys(st);
    eprintln!("[hotkey] reset {id} removed={removed}");
}

fn reset_all_hotkeys(st: &mut AppState) {
    let count = st.cfg.hotkeys.len();
    if count > 0 {
        st.cfg.hotkeys.clear();
        st.cfg.save();
    }
    st.hotkey_recording = None;
    st.ui.set_hotkey_error("".into());
    st.ui.set_hotkey_status(format!("已重置 {count} 条自定义快捷键").into());
    st.ui.invoke_focus_terminal();
    refresh_hotkeys(st);
    eprintln!("[hotkey] reset all count={count}");
}

/// 录制中的按键：Esc 取消、Backspace/Delete 恢复默认，其余组合保存。
///
/// 冲突/非法组合只显示错误并保持录制，方便用户直接再按一个；不会触发任何命令。
fn record_hotkey_key(st: &mut AppState, text: &str, ctrl: bool, alt: bool, shift: bool) {
    let Some(id) = st.hotkey_recording else { return };
    if !ctrl && !alt && !shift {
        if text == "\u{1b}" {
            cancel_hotkey_edit(st);
            return;
        }
        if text == "\u{8}" || text == "\u{7f}" {
            let title = hotkey_title(&st.commands, id);
            let removed = commands::clear_override(&mut st.cfg.hotkeys, id);
            if removed {
                st.cfg.save();
            }
            st.hotkey_recording = None;
            st.ui.set_hotkey_error("".into());
            st.ui.set_hotkey_status(format!("{title} 已恢复默认").into());
            refresh_hotkeys(st);
            st.ui.invoke_focus_terminal();
            eprintln!("[hotkey] record clear {id} removed={removed}");
            return;
        }
    }
    let Some(chord) = keys::chord_from_key_event(text, ctrl, alt, shift) else { return };
    let canonical = chord.canonical();
    match st.commands.validate_override(id, &canonical, &st.cfg.hotkeys) {
        Ok(chord) => {
            let title = hotkey_title(&st.commands, id);
            commands::set_override(&mut st.cfg.hotkeys, id, &chord);
            st.cfg.save();
            st.hotkey_recording = None;
            st.ui.set_hotkey_error("".into());
            st.ui.set_hotkey_status(format!("{title} → {}（已保存）", chord).into());
            refresh_hotkeys(st);
            st.ui.invoke_focus_terminal();
            eprintln!("[hotkey] saved {id} = {canonical}");
        },
        Err(err) => {
            eprintln!("[hotkey] rejected {id} {canonical:?}: {err}");
            st.ui.set_hotkey_error(err.into());
        },
    }
}

fn theme_choices(reg: &ThemeRegistry) -> ModelRc<ThemeChoice> {
    let choices: Vec<ThemeChoice> = reg
        .all()
        .map(|t| ThemeChoice { id: t.id.as_str().into(), label: t.label.as_str().into() })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(choices)))
}

/// 设置面板高度：12 套内置主题 3 行；每多一行加 30px。

/// 下发实际生效的主题（不改配置与选中项；auto 监听回调走这里）。
fn push_theme(st: &mut AppState, theme: &ThemeData) {
    st.resolved_theme = theme.clone();
    let auto = is_auto_theme(&st.cfg.theme);
    st.ui.set_auto_resolved(if auto { theme.label.as_str().into() } else { "".into() });
    st.ui.global::<Palette>().set_colors(slint_theme(&theme.ui));
    let palette = theme.palette();
    for rt in st.panes.values_mut() {
        rt.renderer.set_theme(theme.spec.bg, theme.spec.cursor);
        rt.handle.set_theme(palette.clone());
    }
    redraw_visible(st);
}

/// 无背景图时强制不透明：opacity 只在与背景图混合时才有意义。
/// 这样旧配置里残留的低 opacity 不会让纯色终端发灰，也满足“默认无图不透明”。
fn renderer_opacity(st: &AppState) -> f32 {
    if st.ui.get_has_background_image() {
        config::clamp_opacity(st.cfg.background_opacity)
    } else {
        1.0
    }
}

/// 把“当前是否真的显示背景图”对应的 alpha 下发给所有 pane 并重绘。
/// 背景图加载/清除后必须调用，否则渲染器还停留在旧的 alpha。
fn sync_renderer_opacity(st: &mut AppState) {
    let value = renderer_opacity(st);
    if st.panes.is_empty() {
        return;
    }
    for rt in st.panes.values_mut() {
        rt.renderer.set_opacity(value);
    }
    redraw_visible(st);
}

/// 应用用户输入/配置里的背景图路径。空路径 = 关闭；非法路径/解码失败只返回错误，
/// 不改变当前已生效的背景（设置页据此显示红字并回退到上一张图/纯色）。
fn apply_background_image(st: &mut AppState, raw: &str) -> Result<(), String> {
    match config::resolve_background_image(raw) {
        Ok(None) => {
            st.ui.set_background_image(slint::Image::default());
            st.ui.set_has_background_image(false);
            st.ui.set_background_image_error("".into());
            sync_renderer_opacity(st);
            Ok(())
        },
        Ok(Some(path)) => match slint::Image::load_from_path(&path) {
            Ok(image) => {
                st.ui.set_background_image(image);
                st.ui.set_has_background_image(true);
                st.ui.set_background_image_error("".into());
                sync_renderer_opacity(st);
                eprintln!("[theme] 背景图 {} 已加载", path.display());
                Ok(())
            },
            Err(_) => Err(format!(
                "无法解码图片（支持 PNG/JPEG/SVG）：{}",
                path.display()
            )),
        },
        Err(e) => Err(e),
    }
}

/// 第一次成功设置背景图且当前 opacity 仍是 100% 时自动降到的值。
/// 否则终端底色完全不透明，背景图看起来“没生效”。
const FIRST_BACKGROUND_OPACITY: f32 = 0.35;

/// 设置路径的统一入口：空 = 关闭；合法 = 加载并写盘；非法 = 保留当前背景。
/// `report_error` 为 false 用于输入过程中的即时预览（不弹红字），true 用于回车/失焦提交。
fn commit_background_path(st: &mut AppState, raw: &str, report_error: bool) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        st.cfg.background_image.clear();
        if let Err(e) = apply_background_image(st, "") {
            if report_error {
                st.ui.set_background_image_error(e.into());
            }
            return false;
        }
        st.cfg.save();
        eprintln!("[theme] background_image 关闭");
        return true;
    }
    // 以“当前是否真的显示着背景图”判断首次启用：坏路径回退后重新选图也会自动给可见 opacity。
    let first_image = !st.ui.get_has_background_image();
    match apply_background_image(st, raw) {
        Ok(()) => {
            st.cfg.background_image = raw.to_string();
            if first_image && st.cfg.background_opacity >= 0.99 {
                st.cfg.background_opacity = FIRST_BACKGROUND_OPACITY;
                st.ui.set_background_opacity(FIRST_BACKGROUND_OPACITY);
                sync_renderer_opacity(st);
                eprintln!(
                    "[theme] 首次启用背景图，opacity 自动设为 {FIRST_BACKGROUND_OPACITY:.2}"
                );
            }
            st.cfg.save();
            eprintln!("[theme] background_image={raw} opacity={:.2}", st.cfg.background_opacity);
            true
        },
        Err(e) => {
            if report_error {
                eprintln!("[theme] 背景图无效，保留当前背景: {e}");
                st.ui.set_background_image_error(e.into());
            } else {
                eprintln!("[theme] 背景图输入尚未生效: {e}");
            }
            false
        },
    }
}

/// 启动时把配置里的背景图/遮罩/透明度下发给 UI；坏路径回退为纯色并在设置页留错误。
fn init_background(st: &mut AppState) {
    let raw = st.cfg.background_image.clone();
    st.ui.set_background_image_path(raw.as_str().into());
    st.ui.set_background_opacity(st.cfg.background_opacity);
    st.ui.set_background_dim(st.cfg.background_dim);
    if let Err(e) = apply_background_image(st, &raw) {
        eprintln!("[theme] 背景图不可用，回退纯色背景: {e}");
        st.ui.set_background_image_error(e.into());
    }
}

/// 应用用户选择的主题（"auto" 或具体 id），并写回配置。
fn apply_theme(st: &mut AppState, selected: &str) {
    let auto = is_auto_theme(selected);
    if auto {
        st.cfg.theme = AUTO_THEME.to_string();
    } else if let Some(found) = st.registry.get(selected) {
        st.cfg.theme = found.id.clone();
    } else {
        eprintln!("[theme] 未知主题 {selected:?}，保持当前配置");
        return;
    }
    let theme = resolve_theme(
        &st.registry,
        &st.cfg.theme,
        st.cfg.auto_dark.as_deref(),
        st.cfg.auto_light.as_deref(),
    );
    st.ui.set_theme_name(if auto { AUTO_THEME.into() } else { theme.id.as_str().into() });
    push_theme(st, &theme);
    st.cfg.save();
    if auto {
        ensure_auto_watcher(st);
    }
}

/// 启动一次 `gsettings monitor` 监听；启动失败（非 GNOME、无 gsettings）就静默放弃。
fn ensure_auto_watcher(st: &mut AppState) {
    if st.auto_watch_started {
        return;
    }
    st.auto_watch_started = true;
    let spawned = std::thread::Builder::new()
        .name("theme-watch".into())
        .spawn(watch_system_scheme);
    if spawned.is_err() {
        eprintln!("[theme] 系统配色监听线程启动失败，Auto 仅在启动/切换时检测一次");
    }
}

/// 结束 `gsettings monitor` 子进程（可重复调用）。
fn stop_auto_watcher() {
    SCHEME_WATCH_STOPPED.store(true, Ordering::Release);
    if let Ok(mut slot) = SCHEME_WATCH_CHILD.lock() {
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 后台逐行读取 `gsettings monitor ... color-scheme`，变化时回到 UI 线程更新主题。
/// 不持有 AppState；monitor 启动失败或管道关闭时线程直接结束。
fn watch_system_scheme() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("gsettings");
    cmd.args(["monitor", "org.gnome.desktop.interface", "color-scheme"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // 父进程被 SIGTERM/SIGKILL 直接带走时，内核替我们杀掉 monitor，避免残留孤儿。
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return,
    };
    let Some(stdout) = child.stdout.take() else { return };
    // 子进程句柄交给全局槽位，主线程退出事件循环时可以统一 kill。
    // 与 stop_auto_watcher 用同一把锁，避免「刚启动就退出」时漏杀。
    if let Ok(mut slot) = SCHEME_WATCH_CHILD.lock() {
        if SCHEME_WATCH_STOPPED.load(Ordering::Acquire) {
            drop(slot);
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        *slot = Some(child);
    } else {
        return;
    }
    let mut lines = BufReader::new(stdout).lines();
    while let Some(Ok(_line)) = lines.next() {
        let notified = slint::invoke_from_event_loop(|| {
            with_app(|st| {
                // 用户已切到具体主题：停止跟随。
                if !is_auto_theme(&st.cfg.theme) {
                    return;
                }
                let theme = resolve_theme(
                    &st.registry,
                    AUTO_THEME,
                    st.cfg.auto_dark.as_deref(),
                    st.cfg.auto_light.as_deref(),
                );
                if st.resolved_theme.id != theme.id {
                    push_theme(st, &theme);
                }
            });
        });
        if notified.is_err() {
            break;
        }
    }
    stop_auto_watcher();
}

fn ensure_resize_conn(st: &mut AppState) {
    if st.xresize.is_some() {
        return;
    }
    if let Ok((conn, screen_num)) = crate::xconn::connect() {
        if let Some(root) = conn.setup().roots.get(screen_num).map(|s| s.root) {
            st.xresize = Some(ResizeCtx { conn, root });
        }
    }
}

fn resize_pointer(st: &AppState) -> Option<(i32, i32)> {
    st.xresize
        .as_ref()
        .and_then(|ctx| crate::xconn::query_pointer(&ctx.conn, ctx.root))
}

/// Slint `ResizeHandle` 的 0..7 → winit `ResizeDirection`（两者同序）。
fn winit_resize_direction(dir: i32) -> Option<winit::window::ResizeDirection> {
    use winit::window::ResizeDirection;
    Some(match dir {
        0 => ResizeDirection::NorthWest,
        1 => ResizeDirection::North,
        2 => ResizeDirection::NorthEast,
        3 => ResizeDirection::East,
        4 => ResizeDirection::SouthEast,
        5 => ResizeDirection::South,
        6 => ResizeDirection::SouthWest,
        7 => ResizeDirection::West,
        _ => return None,
    })
}

/// winit 当前窗口是否真的跑在 X11 后端。
///
/// Wayland 会话里通常同时有 `DISPLAY`（XWayland），`xconn` 也能连上；只看
/// `xresize` 会把 Wayland 窗口误判成 X11，从而错误地跳过 `xdg_toplevel.resize`。
fn winit_window_is_x11(window: &winit::window::Window) -> bool {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window
        .window_handle()
        .ok()
        .is_some_and(|handle| {
            matches!(
                handle.as_raw(),
                RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_)
            )
        })
}

/// 尝试把一次边/角拖动交给 winit/合成器。
///
/// X11 上必须由 **winit 自己的连接**先 `XUngrabPointer` 再发 `_NET_WM_MOVERESIZE`：
/// X server 只允许 grab 的持有者释放它，`xconn` 是另一个 client，跨连接 ungrab
/// 不会生效（实测会导致 WM 完全接不到 resize）。Wayland 没有 xconn，直接走
/// winit 的 `xdg_toplevel.resize`；只有 winit 实际使用 X11 后端时才先查
/// `_NET_SUPPORTED`，避免 winit 先 ungrab 后无 WM 接手导致自绘也失效。
///
/// 返回 true 表示请求已交给 winit；调用方置 `wm_resize`，并在 drag 回调里用
/// “是否还能收到指针 move + 是否已改尺寸”检测 WM 是否真的接管，未接管则回退自绘。
fn start_wm_live_resize(st: &AppState, dir: i32) -> bool {
    let Some(winit_dir) = winit_resize_direction(dir) else {
        eprintln!("[ui] 非法 resize 方向 {dir}，回退自绘 resize");
        return false;
    };

    // 只有 winit 实际使用 X11 后端时才做 EWMH 预检：没有 WM/不支持时**不能**
    // 调用 winit drag_resize_window——winit 会先 XUngrabPointer，若没有 WM 接手，
    // 后续 move 到窗外就收不到，自绘也会一起失效。Wayland（含 XWayland 同时存在
    // DISPLAY 的会话）不查 X11 atom，直接走 winit 的 xdg_toplevel.resize。
    let x11_window = st
        .ui
        .window()
        .with_winit_window(winit_window_is_x11)
        .unwrap_or(false);
    if x11_window {
        if let Some(ctx) = st.xresize.as_ref() {
            if !xconn::wm_supports_moveresize(&ctx.conn, ctx.root) {
                eprintln!("[ui] WM 未声明 _NET_WM_MOVERESIZE，回退自绘 resize");
                return false;
            }
        }
    }

    let (started, source) =
        match st.ui.window().with_winit_window(|window| window.drag_resize_window(winit_dir)) {
            Some(Ok(())) => (true, "winit"),
            Some(Err(err)) => {
                eprintln!("[ui] winit drag_resize_window 失败，回退自绘 resize: {err}");
                (false, "winit")
            }
            None => {
                // 当前构建固定 winit 后端；只有未来换后端时才会走到这里。
                let Some(ctx) = st.xresize.as_ref() else {
                    eprintln!("[ui] 没有 winit 窗口，也没有 X11 连接，无法拖边 resize");
                    return false;
                };
                let Some(window) = xconn::find_own_window(&ctx.conn, ctx.root, &st.opts.title)
                else {
                    eprintln!("[ui] 找不到本进程窗口，无法走 EWMH resize");
                    return false;
                };
                match xconn::start_wm_resize(&ctx.conn, ctx.root, window, dir) {
                    Ok(()) => {
                        eprintln!(
                            "[ui] xconn EWMH 兜底已发送；跨连接 ungrab 可能无效，仍保留自绘回退"
                        );
                        (true, "xconn-ewmh")
                    }
                    Err(err) => {
                        eprintln!("[ui] xconn EWMH 兜底失败，回退自绘 resize: {err}");
                        (false, "xconn-ewmh")
                    }
                }
            }
        };

    // winit 路径在 X11/Wayland 都由后端保证支持；xconn 兜底在发送前已查 _NET_SUPPORTED。
    let route = xconn::resize_route(true, started);
    if route == xconn::ResizeRoute::Wm {
        eprintln!("[ui] WM live resize dir={dir} source={source}");
        true
    } else {
        false
    }
}

fn count_marker(frame: &Frame, marker: char) -> usize {
    let row = frame.cursor.line as usize;
    if row >= frame.rows {
        return 0;
    }
    frame.row(row).iter().filter(|c| c.char() == marker).count()
}

fn cell_of(rect: Rect, cell: (usize, usize)) -> (usize, usize) {
    let cols = ((rect.w / cell.0 as f32).floor() as usize).max(10);
    let rows = ((rect.h / cell.1 as f32).floor() as usize).max(3);
    (cols, rows)
}

// ---------------------------------------------------------------- 生命周期

fn shell_name(opts: &Options) -> String {
    let path = opts.shell.clone().or_else(|| std::env::var("SHELL").ok());
    path.as_deref()
        .and_then(|s| std::path::Path::new(s).file_name().map(|f| f.to_string_lossy().to_string()))
        .unwrap_or_else(|| "shell".to_string())
}

fn create_pane(
    st: &AppState,
    id: PaneId,
    cols: usize,
    rows: usize,
    cwd: Option<PathBuf>,
    profile: Option<&profile::Profile>,
    mut askpass: Option<askpass::Askpass>,
) -> std::io::Result<PaneRuntime> {
    // Profile 优先：它显式带 shell/args/env；否则沿用全局配置。
    let shell = profile
        .map(|p| p.shell.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| st.cfg.shell.clone());
    let args = profile.map(|p| p.args.clone()).unwrap_or_else(|| st.opts.args.clone());
    let mut env = profile.map(|p| p.env.clone()).unwrap_or_default();
    if let Some(a) = askpass.as_mut() {
        a.apply(&mut env);
        // 只记变量名与“是否非空”，不记密码；用于确认 env 真的进入 ssh 子进程。
        let keys: Vec<&str> = env
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| k.starts_with("SSH_ASKPASS") || *k == askpass::SECRET_ENV)
            .collect();
        let secret_set =
            env.iter().any(|(k, v)| k.as_str() == askpass::SECRET_ENV && !v.is_empty());
        eprintln!("[askpass] 注入 ssh 子进程环境: keys={keys:?} secret_set={secret_set}");
    }
    let cwd = profile
        .and_then(|p| p.cwd.clone())
        .map(PathBuf::from)
        .or(cwd);
    let handle = spawn(PaneConfig {
        cols: cols as u16,
        rows: rows as u16,
        scrollback: st.cfg.scrollback,
        shell,
        args,
        palette: st.resolved_theme.palette(),
        cwd,
        env,
    })?;

    let wake = handle.wake_receiver();
    let pending = Arc::new(AtomicBool::new(false));
    std::thread::Builder::new().name(format!("wake-{id}")).spawn(move || {
        while wake.recv().is_ok() {
            if pending.swap(true, Ordering::AcqRel) {
                continue;
            }
            let pending = pending.clone();
            let _ = slint::invoke_from_event_loop(move || {
                pending.store(false, Ordering::Release);
                redraw_pane(id);
            });
        }
    })?;

    let theme = &st.resolved_theme;
    let mut renderer = Renderer::new(
        st.fonts.clone(),
        cols,
        rows,
        theme.spec.bg,
        theme.spec.cursor,
        renderer_opacity(st),
    );
    let mut frame = handle.new_frame();
    frame = handle.pull(frame);
    renderer.draw(&frame);
    let image = renderer.image();

    Ok(PaneRuntime {
        handle,
        renderer,
        frame,
        image,
        grid: (cols, rows),
        rect: Rect::new(0.0, 0.0, 0.0, 0.0),
        title: shell_name(&st.opts),
        activity: activity::PaneActivity::default(),
        notification: None,
        _askpass: askpass,
    })
}

fn new_tab_inner(st: &mut AppState) {
    let id = st.ws.alloc_id();
    let (cols, rows) = cell_of(st.content, st.cell);
    match create_pane(st, id, cols, rows, None, None, None) {
        Ok(rt) => {
            st.next_title += 1;
            let n = st.next_title;
            let title = format!("Terminal {n}");
            st.panes.insert(id, rt);
            st.ws.tabs.push(Tab::single(id, title.clone()));
            eprintln!("[ws] new tab: tabs={}", st.ws.tabs.len());
            st.ws.active = st.ws.tabs.len() - 1;
            dispatch_plugin_event(st, plugin::PluginEvent::TabOpen, plugin::tab_open_payload(&title, "local"));
            refresh_views(st);
            rebuild_tabs(st);
            redraw_pane_inner(st, id);
        },
        Err(e) => eprintln!("创建 pane 失败: {e}"),
    }
}

fn new_tab() {
    with_app(new_tab_with_default);
}

/// 新建窗口 = 拉起一个独立的 Tervo 进程（MVP，无单实例锁/IPC）。
/// 在 `with_app` 借用内调用也安全：不碰 AppState，只 spawn 并写日志。
fn new_window() {
    match window::spawn_new_window() {
        Ok(pid) => eprintln!("[ui] new window pid={pid}"),
        Err(e) => eprintln!("[ui] new window failed: {e}"),
    }
}

/// 关视图但保留会话：运行时挪到后台池，PaneHandle 不被 Drop，子进程继续跑。
fn detach_runtime(st: &mut AppState, id: PaneId) {
    let Some(rt) = st.panes.remove(&id) else { return };
    let title = if rt.title.is_empty() { format!("session {id}") } else { rt.title.clone() };
    eprintln!("[session] detach pane={id} title={title:?}（子进程保留）");
    st.detached.push((id, rt, title));
    refresh_background(st);
}

/// 后台会话列表（标题 + 窗口内 cwd 提示不吃内存）。
fn refresh_background(st: &mut AppState) {
    let items: Vec<slint::SharedString> =
        st.detached.iter().map(|(id, _, title)| format!("{title}   #{id}").into()).collect();
    st.bg_model.set_vec(items);
}

/// 刷新 Profiles 面板：按搜索词过滤，并记录每个可见行对应的 store 下标。
///
/// 列表模型只带展示字段；所有操作回调用 `profile_indices` 映射回真实 Profile，
/// 避免过滤后 “点第 0 行却打开第 0 个 store 项” 的错位。
fn refresh_profiles(st: &mut AppState) {
    let default_id = st.cfg.default_profile_id;
    let indices = profile::filter_indices(&st.profiles.profiles, &st.profile_query);
    let items: Vec<ProfileRow> = indices
        .iter()
        .filter_map(|&i| {
            let p = st.profiles.profiles.get(i)?;
            Some(ProfileRow {
                id: p.id as i32,
                label: format!("[{}] {}", p.kind, p.name).into(),
                is_default: default_id != 0 && p.id == default_id,
            })
        })
        .collect();
    let default_info = match st.profiles.default_index(default_id) {
        Some(i) => format!("New tab default: {}", st.profiles.profiles[i].name),
        None => "New tab default: system shell".to_string(),
    };
    st.profile_indices = indices;
    st.profile_model.set_vec(items);
    st.ui.set_profile_default_info(default_info.as_str().into());
}

/// 默认 Profile 的 store 下标；未设置 / ID 已失效时返回 None（回退默认 shell）。
fn default_profile_index(st: &AppState) -> Option<usize> {
    st.profiles.default_index(st.cfg.default_profile_id)
}

/// 面板行号 → store 下标。过滤/删除后行号可能失效，越界一律忽略。
fn profile_row_to_store(st: &AppState, row: i32) -> Option<usize> {
    st.profile_indices.get(row.max(0) as usize).copied()
}

/// 新建 Tab：默认 Profile 有效时用它打开，否则保持原来的默认 shell 行为。
///
/// 只用于用户主动新建（+ / Ctrl+T / 命令 tab.new / TabBar 菜单）；
/// 启动首 Tab 与“重启会话”仍走各自的路径，避免默认 SSH Profile 在启动时自动连接。
fn new_tab_with_default(st: &mut AppState) {
    match default_profile_index(st) {
        Some(index) => {
            let id = st.profiles.profiles[index].id;
            eprintln!("[profile] 新 Tab 使用默认 Profile id={id} index={index}");
            open_profile_tab(st, index);
        },
        None => new_tab_inner(st),
    }
}

/// 删除 Profile 后清理默认指向：删的正是默认项时回退 0（系统 shell）。
/// 返回是否发生回退，调用方据此决定是否写盘。
fn clear_default_after_remove(cfg: &mut Config, removed_id: u64) -> bool {
    if removed_id != 0 && cfg.default_profile_id == removed_id {
        cfg.default_profile_id = 0;
        true
    } else {
        false
    }
}

/// Quake Mode：F12 全局显示/隐藏顶部下拉终端。
fn toggle_quake() {
    with_app(toggle_quake_inner);
}

fn toggle_quake_inner(st: &mut AppState) {
    if !st.cfg.quake_enabled {
        eprintln!("[quake] 未启用（设置 → Window → Quake mode）");
        return;
    }
    if st.quake_visible {
        // 不能 hide()：Slint 在“最后一个窗口隐藏”时会退出事件循环，进程直接结束，
        // 全局热键线程也随之消失（表现为第二次 F12 后整个 Tervo 没了）。
        // 用最小化：窗口仍存在、事件循环/PTY/热键都活着；再由 F12 恢复。
        st.ui.window().set_minimized(true);
        st.quake_visible = false;
        eprintln!("[quake] hide (minimized)");
        return;
    }
    let (sw, sh) = crate::xconn::screen_size().unwrap_or((1920, 1080));
    let w = ((sw as f32 * st.cfg.quake_width).round() as u32).clamp(320, sw);
    let h = ((sh as f32 * st.cfg.quake_height).round() as u32).clamp(200, sh);
    let x = (sw.saturating_sub(w) / 2) as i32;
    if st.quake_saved.is_none() {
        st.quake_saved = Some((st.ui.window().position(), st.ui.window().size()));
    }
    st.ui.window().set_minimized(false);
    st.ui.window().set_size(slint::PhysicalSize::new(w, h));
    st.ui.window().set_position(slint::PhysicalPosition::new(x, 0));
    st.ui.window().show();
    let _ = crate::xconn::activate_own_window("");
    st.quake_visible = true;
    eprintln!("[quake] show {w}x{h} at {x},0");
}

/// 构建 Profile 选择菜单（+ 旁边独立图标；不包含“新建默认 Tab”）。
fn open_profile_menu(st: &mut AppState) {
    let mut items: Vec<slint::SharedString> = Vec::new();
    let mut indices = Vec::new();
    for (i, p) in st.profiles.profiles.iter().enumerate() {
        let kind = match p.kind.as_str() {
            "ssh" => "SSH",
            "serial" => "Serial",
            "telnet" => "Telnet",
            _ => "Local",
        };
        items.push(format!("{kind} · {}", p.name).into());
        indices.push(i);
    }
    st.profile_menu_indices = indices;
    st.profile_menu_model.set_vec(items);
    st.ui.set_profilemenu_open(true);
    eprintln!("[ui] profile menu profiles={}", st.profile_menu_indices.len());
}

fn open_profiles_panel(st: &mut AppState) {
    // 每次打开都回到完整列表：搜索词不跨面板生命周期残留。
    st.profile_query.clear();
    st.ui.set_profile_query("".into());
    refresh_profiles(st);
    st.ui.set_profiles_open(true);
    eprintln!("[profile] panel items={}", st.profile_indices.len());
}

/// 设置 → Vault 页 + 可见提示（Vault 未解锁时不弹终端密码框）。
fn prompt_vault_unlock(st: &mut AppState, msg: &str) {
    refresh_vault_ui(st);
    st.ui.set_settings_open(true);
    st.ui.set_settings_category(6);
    st.ui.set_vault_error(msg.into());
    st.ui.set_vault_password("".into());
    eprintln!("[vault] {msg}");
}

/// 回到 Profile 表单显示错误（例如 Vault 中没有该 Profile 的密码）。
fn prompt_profile_password(st: &mut AppState, index: usize, msg: &str) {
    edit_profile_form(st, index);
    st.ui.set_form_error(msg.into());
    eprintln!("[profile] {msg}");
}

/// 用 Profile 打开新 Tab（shell/args/env/cwd 全按 Profile）。
fn open_profile_tab(st: &mut AppState, index: usize) {
    let Some(p) = st.profiles.profiles.get(index).cloned() else { return };
    // kind → 实际启动命令：local 直接用 Profile；ssh 用系统 ssh 客户端（PTY 里跑）；
    // serial/telnet 后端还没做，明确拒绝而不是假装能连。
    let mut askpass = None;
    let spec = match p.kind.as_str() {
        "local" => p.clone(),
        "ssh" => {
            let (prog, args) = p.ssh_command();
            let mut q = p.clone();
            q.shell = prog;
            q.args = args;
            q.cwd = None;
            // auth=password + 已勾选保存 → 从 Vault 取密码，用 askpass 喂给系统 ssh；
            // 未勾选保存则交给 ssh 在终端里交互式询问（不经过 Vault，不落盘）。
            if p.auth == "password" && p.save_password {
                match st.vault.get(p.id) {
                    Ok(password) => {
                        // 只记布尔/长度，绝不记密码本身。
                        eprintln!(
                            "[vault] 已取出 Profile {} 的密码（len={}）",
                            p.id,
                            password.len()
                        );
                        if !askpass::host_key_known(&p) {
                            // 首次连接：不用 askpass，交给 ssh 在终端里显示指纹/确认
                            // 并让用户手输一次密码；之后 known_hosts 命中才自动填充。
                            eprintln!(
                                "[profile] 主机密钥未在 known_hosts 中确认：本次不启用 askpass，\
                                 请在终端确认主机密钥并手动输入密码（下次连接可用 Vault）"
                            );
                        } else {
                            match askpass::Askpass::new(&password) {
                                Ok(a) => {
                                    eprintln!(
                                        "[askpass] 已启用 profile={} require=force debug={}",
                                        p.id,
                                        askpass::debug_enabled()
                                    );
                                    askpass = Some(a);
                                },
                                Err(e) => {
                                    drop(password);
                                    prompt_profile_password(
                                        st,
                                        index,
                                        &format!("创建 ssh askpass 失败: {e}"),
                                    );
                                    return;
                                },
                            }
                        }
                    },
                    Err(vault::VaultError::Locked | vault::VaultError::NotInitialized) => {
                        prompt_vault_unlock(
                            st,
                            "Vault 已锁定：请先在 Vault 页解锁，再连接该 SSH Profile",
                        );
                        return;
                    },
                    Err(e) => {
                        prompt_profile_password(
                            st,
                            index,
                            &format!("{e}；请解锁 Vault 并重新保存密码"),
                        );
                        return;
                    },
                }
            }
            eprintln!("[profile] ssh 命令: {} {}", q.shell, q.args.join(" "));
            q
        },
        "telnet" => {
            let (prog, args) = p.telnet_command();
            let mut q = p.clone();
            q.shell = prog;
            q.args = args;
            q.cwd = None;
            eprintln!("[profile] telnet 命令: {} {}", q.shell, q.args.join(" "));
            q
        },
        "serial" => {
            let (prog, args) = p.serial_command();
            let mut q = p.clone();
            q.shell = prog;
            q.args = args;
            q.cwd = None;
            eprintln!("[profile] serial 命令: {} {}", q.shell, q.args.join(" "));
            q
        },
        other => {
            eprintln!("[profile] {:?} kind={other} 未知类型", p.name);
            return;
        },
    };
    let id = st.ws.alloc_id();
    let (cols, rows) = cell_of(st.content, st.cell);
    match create_pane(st, id, cols, rows, None, Some(&spec), askpass) {
        Ok(rt) => {
            let tab_title = p.name.clone();
            st.panes.insert(id, rt);
            st.ws.tabs.push(Tab::single(id, tab_title.clone()));
            st.ws.active = st.ws.tabs.len() - 1;
            st.ui.set_profiles_open(false);
            st.ui.invoke_focus_terminal();
            dispatch_plugin_event(st, plugin::PluginEvent::TabOpen, plugin::tab_open_payload(&tab_title, &p.kind));
            refresh_views(st);
            rebuild_tabs(st);
            redraw_pane_inner(st, id);
            eprintln!("[profile] open {:?} kind={}", p.name, p.kind);
        },
        Err(e) => eprintln!("[profile] 打开 {:?} 失败: {e}", p.name),
    }
}

/// 打开新建表单：预填一个不冲突的名字、当前 shell、聚焦 pane 的 cwd。
fn open_profile_form(st: &mut AppState) {
    let base = "profile".to_string();
    let mut name = base.clone();
    let mut n = 2;
    while st.profiles.profiles.iter().any(|p| p.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    let shell = st.cfg.shell.clone().or_else(|| std::env::var("SHELL").ok()).unwrap_or_default();
    let cwd = st
        .ws
        .active_tab()
        .map(|t| t.focused)
        .and_then(|id| st.panes.get(&id))
        .and_then(|rt| rt.handle.pid())
        .and_then(session::cwd_of)
        .unwrap_or_default();
    st.ui.set_form_edit_id(-1);
    st.ui.set_form_name(name.as_str().into());
    st.ui.set_form_kind(0);
    st.ui.set_form_shell(shell.as_str().into());
    st.ui.set_form_args("".into());
    st.ui.set_form_cwd(cwd.as_str().into());
    st.ui.set_form_host("".into());
    st.ui.set_form_port("22".into());
    st.ui.set_form_user(std::env::var("USER").unwrap_or_default().as_str().into());
    st.ui.set_form_auth("agent".into());
    st.ui.set_form_key("".into());
    st.ui.set_form_password("".into());
    st.ui.set_form_save_password(false);
    st.ui.set_form_extra("".into());
    st.ui.set_form_forwards("".into());
    st.ui.set_form_serial_port("/dev/ttyUSB0".into());
    st.ui.set_form_baud("115200".into());
    st.ui.set_form_error("".into());
    st.ui.set_profile_form_open(true);
    eprintln!("[profile] 打开新建表单 name={name:?} shell={shell:?} cwd={cwd:?}");
}

/// 打开编辑表单：回填已有 Profile 的所有字段。
fn edit_profile_form(st: &mut AppState, index: usize) {
    let Some(p) = st.profiles.profiles.get(index).cloned() else { return };
    let kind_idx = profile::KINDS.iter().position(|k| *k == p.kind).unwrap_or(0);
    st.ui.set_form_edit_id(p.id as i32);
    st.ui.set_form_name(p.name.as_str().into());
    st.ui.set_form_kind(kind_idx as i32);
    st.ui.set_form_shell(p.shell.as_str().into());
    st.ui.set_form_args(p.args.join(" ").as_str().into());
    st.ui.set_form_cwd(p.cwd.clone().unwrap_or_default().as_str().into());
    st.ui.set_form_host(p.host.as_str().into());
    st.ui.set_form_port(p.port.to_string().as_str().into());
    st.ui.set_form_user(p.user.as_str().into());
    st.ui.set_form_auth(p.auth.as_str().into());
    st.ui.set_form_key(p.key_path.as_str().into());
    // 密码绝不回填到 UI；勾选状态反映配置，真正密码只在 Vault 解锁时可写入。
    st.ui.set_form_password("".into());
    st.ui.set_form_save_password(p.save_password);
    st.ui.set_form_extra(p.extra_args.join(" ").as_str().into());
    st.ui.set_form_forwards(p.local_forwards.join(" ").as_str().into());
    st.ui.set_form_serial_port(p.serial_port.as_str().into());
    st.ui.set_form_baud(p.baud.to_string().as_str().into());
    st.ui.set_form_error("".into());
    st.ui.set_profile_form_open(true);
    eprintln!("[profile] 编辑表单 id={} name={:?} kind={}", p.id, p.name, p.kind);
}

fn save_profile_form(st: &mut AppState) {
    let kind_idx = st.ui.get_form_kind().clamp(0, 3) as usize;
    let kind = profile::KINDS[kind_idx];
    let name = st.ui.get_form_name().to_string();
    let parse_port = |s: String| s.trim().parse::<u16>().unwrap_or(0);
    let edit_id = st.ui.get_form_edit_id();
    let mut p = profile::Profile { name, kind: kind.to_string(), ..profile::Profile::default() };
    match kind {
        "ssh" => {
            p.host = st.ui.get_form_host().to_string();
            p.port = parse_port(st.ui.get_form_port().to_string());
            p.user = st.ui.get_form_user().to_string();
            p.auth = st.ui.get_form_auth().to_string();
            p.key_path = st.ui.get_form_key().to_string();
            p.extra_args =
                st.ui.get_form_extra().split_whitespace().map(|s| s.to_string()).collect();
            p.local_forwards =
                st.ui.get_form_forwards().split_whitespace().map(|s| s.to_string()).collect();
        },
        "serial" => {
            p.serial_port = st.ui.get_form_serial_port().to_string();
            p.baud = st.ui.get_form_baud().trim().parse::<u32>().unwrap_or(0);
        },
        "telnet" => {
            p.host = st.ui.get_form_host().to_string();
            p.port = parse_port(st.ui.get_form_port().to_string());
        },
        _ => {
            p.shell = st.ui.get_form_shell().to_string();
            p.args = st.ui.get_form_args().split_whitespace().map(|s| s.to_string()).collect();
            let cwd = st.ui.get_form_cwd().to_string();
            p.cwd = (!cwd.trim().is_empty()).then(|| cwd.trim().to_string());
        },
    }
    // ---- Vault 密码：只在 auth=password 且勾选保存时处理 ----
    let password = st.ui.get_form_password().to_string();
    let save_checked = st.ui.get_form_save_password();
    p.save_password = kind == "ssh" && p.auth == "password" && save_checked;
    let save_password = p.save_password;
    let previous_save =
        if edit_id > 0 { st.profiles.get(edit_id as u64).map(|q| q.save_password).unwrap_or(false) } else { false };

    // 先做 Vault 前置校验，任何错误都不改动 ProfileStore。
    if p.save_password {
        if !password.is_empty() {
            if !st.vault.is_unlocked() {
                let msg = match st.vault.status() {
                    vault::VaultStatus::Uninitialized => {
                        "Vault 未初始化：请先在 Settings → Vault 设置主密码，再保存密码"
                    },
                    _ => "Vault 未解锁：请先在 Settings → Vault 解锁，再保存密码",
                };
                st.ui.set_form_error(msg.into());
                eprintln!("[profile] {msg}");
                return;
            }
        } else if edit_id > 0 && previous_save {
            // 编辑时留空 = 保留 Vault 里已有的密码；若已解锁则确认条目还在。
            if st.vault.is_unlocked() && !matches!(st.vault.contains(edit_id as u64), Ok(true)) {
                let msg = "Vault 中没有该 Profile 的密码，请输入新密码后保存";
                st.ui.set_form_error(msg.into());
                eprintln!("[profile] {msg}");
                return;
            }
        } else {
            let msg = "勾选「保存到 Vault」时必须输入密码";
            st.ui.set_form_error(msg.into());
            eprintln!("[profile] {msg}");
            return;
        }
    }

    let result = if edit_id > 0 {
        st.profiles.update(edit_id as u64, p).map(|_| edit_id as u64)
    } else {
        st.profiles.add(p)
    };
    match result {
        Ok(id) => {
            let mut vault_write_error = None;
            if save_password && !password.is_empty() {
                if let Err(e) = st.vault.set(id, &password) {
                    vault_write_error = Some(format!("密码写入 Vault 失败: {e}"));
                }
            } else if !save_password && previous_save {
                // 取消勾选：解锁时同步删除旧凭据；锁定则只能先留着，等 Clear vault 清理。
                match st.vault.remove(id) {
                    Ok(_) => {},
                    Err(vault::VaultError::Locked | vault::VaultError::NotInitialized) => {
                        eprintln!("[vault] Vault 锁定，Profile {id} 的旧密码暂留（已被取消勾选，不再使用）");
                    },
                    Err(e) => eprintln!("[vault] 删除 Profile {id} 的密码失败: {e}"),
                }
            }
            if let Some(err) = vault_write_error {
                // Profile 已进内存：避免 save_password=true 却没有密码，回退标志并落盘。
                if let Some(profile) = st.profiles.profiles.iter_mut().find(|q| q.id == id) {
                    profile.save_password = false;
                }
                st.profiles.save();
                refresh_profiles(st);
                st.ui.set_form_save_password(false);
                st.ui.set_form_error(err.as_str().into());
                eprintln!("[profile] {err}（Profile 已保存，但未关联密码）");
                return;
            }
            st.profiles.save();
            refresh_profiles(st);
            st.ui.set_profile_form_open(false);
            st.ui.set_form_password("".into());
            st.ui.set_form_save_password(false);
            st.ui.set_form_error("".into());
            refresh_vault_ui(st);
            eprintln!("[profile] 保存 id={id} kind={kind}（共 {}）", st.profiles.profiles.len());
        },
        Err(e) => {
            eprintln!("[profile] 表单校验失败: {e}");
            st.ui.set_form_error(e.as_str().into());
        },
    }
}

// ------------------------------------------------------------ Vault（加密凭据）

/// Vault 状态文字（不含任何秘密）。
fn vault_status_text(v: &vault::Vault) -> String {
    match v.status() {
        vault::VaultStatus::Uninitialized => "未初始化：设置主密码后创建 vault.enc".into(),
        vault::VaultStatus::Locked => "已锁定：输入主密码后点 Unlock".into(),
        vault::VaultStatus::Unlocked => format!("已解锁 · {} 条凭据", v.entry_count()),
    }
}

/// 刷新设置页 Vault 状态/路径/条数（不清空错误提示）。
fn refresh_vault_ui(st: &mut AppState) {
    let status = vault_status_text(&st.vault);
    let path = st.vault.path().display().to_string();
    let entries = st.vault.entry_count() as i32;
    st.ui.set_vault_status(status.as_str().into());
    st.ui.set_vault_path(path.as_str().into());
    st.ui.set_vault_entries(entries);
}

/// 操作成功时的中性提示（不是错误）。
fn vault_notice(st: &mut AppState, msg: impl Into<String>) {
    st.ui.set_vault_notice(msg.into().as_str().into());
    st.ui.set_vault_error("".into());
}

/// 操作失败提示；调用方保证 msg 不含秘密。
fn vault_fail(st: &mut AppState, msg: impl Into<String>) {
    let msg = msg.into();
    st.ui.set_vault_error(msg.as_str().into());
    st.ui.set_vault_notice("".into());
    eprintln!("[vault] {msg}");
}

/// Set / Change master password：未初始化 → 创建；已解锁 → 用新密码重新加密；
/// 已锁定 → 必须先解锁（避免绕过旧主密码）。
fn vault_set_master_password(st: &mut AppState) {
    st.vault_clear_armed = false;
    let master = st.ui.get_vault_password().to_string();
    if master.is_empty() {
        vault_fail(st, "请输入主密码");
        return;
    }
    match st.vault.status() {
        vault::VaultStatus::Uninitialized => match st.vault.initialize(&master) {
            Ok(()) => {
                st.ui.set_vault_password("".into());
                refresh_vault_ui(st);
                vault_notice(st, "主密码已设置，Vault 已解锁（空 vault）");
                eprintln!("[vault] 初始化完成");
            },
            Err(e) => vault_fail(st, e.to_string()),
        },
        vault::VaultStatus::Unlocked => match st.vault.change_master(&master) {
            Ok(()) => {
                st.ui.set_vault_password("".into());
                refresh_vault_ui(st);
                vault_notice(st, "主密码已更新；凭据已用新密钥重新加密");
                eprintln!("[vault] 主密码已更新");
            },
            Err(e) => vault_fail(st, e.to_string()),
        },
        vault::VaultStatus::Locked => {
            vault_fail(st, "Vault 已锁定：先用当前主密码解锁，再修改主密码");
        },
    }
}

fn vault_unlock(st: &mut AppState) {
    st.vault_clear_armed = false;
    let master = st.ui.get_vault_password().to_string();
    if master.is_empty() {
        vault_fail(st, "请输入主密码");
        return;
    }
    match st.vault.unlock(&master) {
        Ok(()) => {
            st.ui.set_vault_password("".into());
            refresh_vault_ui(st);
            vault_notice(st, format!("已解锁 · {} 条凭据", st.vault.entry_count()));
            eprintln!("[vault] 已解锁 entries={}", st.vault.entry_count());
        },
        Err(e) => {
            // 密码本身不放进提示/日志。
            refresh_vault_ui(st);
            vault_fail(st, e.to_string());
        },
    }
}

fn vault_lock(st: &mut AppState) {
    st.vault.lock();
    st.ui.set_vault_password("".into());
    st.vault_clear_armed = false;
    refresh_vault_ui(st);
    vault_notice(st, "已锁定：内存中的密钥/凭据已丢弃");
    eprintln!("[vault] 已锁定");
}

fn vault_clear(st: &mut AppState) {
    if !st.vault.is_unlocked() {
        vault_fail(st, "Vault 未解锁，无法清空");
        return;
    }
    if !st.vault_clear_armed {
        st.vault_clear_armed = true;
        let n = st.vault.entry_count();
        vault_notice(st, format!("再次点击 Clear 确认清空 {n} 条凭据（主密码保留）"));
        return;
    }
    st.vault_clear_armed = false;
    match st.vault.clear() {
        Ok(()) => {
            refresh_vault_ui(st);
            vault_notice(st, "已清空全部凭据（vault.enc 与主密码保留）");
            eprintln!("[vault] 已清空全部凭据");
        },
        Err(e) => vault_fail(st, e.to_string()),
    }
}

// ------------------------------------------------------------ SFTP 面板

fn sftp_profile_indices(st: &AppState) -> Vec<usize> {
    st.profiles
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| p.kind == "ssh")
        .map(|(i, _)| i)
        .collect()
}

fn open_sftp_panel(st: &mut AppState) {
    let indices = sftp_profile_indices(st);
    let names: Vec<slint::SharedString> =
        indices.iter().map(|i| st.profiles.profiles[*i].name.clone().into()).collect();
    st.sftp_profiles = indices;
    let model = Rc::new(VecModel::from(names));
    st.ui.set_sftp_profiles(ModelRc::from(model));
    st.sftp_model.set_vec(Vec::<slint::SharedString>::new());
    st.ui.set_sftp_profile(0);
    st.ui.set_sftp_selected(-1);
    st.ui.set_sftp_status(
        if st.sftp_profiles.is_empty() {
            "没有 SSH Profile，先到 Profiles 里建一个"
        } else {
            "选择 Profile，点 List"
        }
        .into(),
    );
    // 面板可能在传输进行中被再次打开：以真实传输状态覆盖初始文案。
    if st.sftp_transfer_state.is_active() {
        let status = st.sftp_transfer_state.status_text();
        st.ui.set_sftp_status(status.as_str().into());
    }
    sftp_show_transfer(st);
    st.ui.set_sftp_open(true);
    eprintln!("[sftp] 打开面板 profiles={}", st.sftp_profiles.len());
}

fn sftp_current_target(st: &AppState) -> Option<sftp::SftpTarget> {
    let i = st.ui.get_sftp_profile().max(0) as usize;
    let idx = *st.sftp_profiles.get(i)?;
    sftp::SftpTarget::from_profile(&st.profiles.profiles[idx]).ok()
}

fn sftp_set_status(st: &mut AppState, msg: &str) {
    st.ui.set_sftp_status(msg.into());
    eprintln!("[sftp] {msg}");
}

/// 把状态机快照写进 Slint 属性（进度区只在 Running 时显示）。
fn sftp_show_transfer(st: &mut AppState) {
    let active = st.sftp_transfer_state.is_active();
    let current = st.sftp_transfer_state.current.clone();
    let percent = st.sftp_transfer_state.percent_i32();
    let detail = st.sftp_transfer_state.detail_text();
    st.ui.set_sftp_transfer_active(active);
    st.ui.set_sftp_transfer_name(current.as_str().into());
    st.ui.set_sftp_transfer_percent(percent);
    st.ui.set_sftp_transfer_detail(detail.as_str().into());
}

fn sftp_update_status(st: &mut AppState) {
    let status = st.sftp_transfer_state.status_text();
    if st.ui.get_sftp_status().as_str() != status.as_str() {
        st.ui.set_sftp_status(status.as_str().into());
    }
}

/// 启动一次后台传输；同一时间只允许一个任务。
fn sftp_start_transfer(
    st: &mut AppState,
    direction: sftp::TransferDirection,
    local: String,
    remote: String,
) {
    if st.sftp_transfer.is_some() {
        sftp_set_status(st, "已有传输进行中，先 Cancel 或等完成");
        return;
    }
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    // 下载始终 get -R（普通文件安全）；上传只有本地是目录才 put -R。
    let recursive =
        direction == sftp::TransferDirection::Upload && std::path::Path::new(&local).is_dir();
    let request = sftp::TransferRequest {
        direction,
        target,
        local: local.clone(),
        remote: remote.clone(),
        recursive,
    };
    match sftp::TransferJob::start(&request) {
        Ok(job) => {
            st.sftp_transfer = Some(job);
            st.sftp_transfer_state =
                sftp::TransferState::running(direction, local.clone(), remote.clone());
            sftp_show_transfer(st);
            let status = st.sftp_transfer_state.status_text();
            sftp_set_status(st, &status);
            eprintln!(
                "[sftp] {} 开始 local={local:?} remote={remote:?} recursive={recursive}",
                direction.verb()
            );
        },
        Err(e) => sftp_set_status(st, &format!("{}失败: {e}", direction.verb())),
    }
}

/// 由 400ms 的 poll_events 驱动：非阻塞收事件、更新 UI、终态回收线程。
fn poll_sftp_transfer(st: &mut AppState) {
    let Some(job) = st.sftp_transfer.as_ref() else {
        return;
    };
    let mut events = Vec::new();
    let mut disconnected = false;
    loop {
        match job.events.try_recv() {
            Ok(event) => events.push(event),
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                disconnected = true;
                break;
            },
        }
    }
    for event in events {
        st.sftp_transfer_state.apply(event);
    }

    let was_active = st.sftp_transfer_state.is_active();
    if was_active && disconnected {
        // 正常路径一定有 Finished；没有结束事件说明 worker 异常退出。
        st.sftp_transfer_state.apply(sftp::TransferEvent::Finished {
            cancelled: false,
            error: Some("传输进程意外结束（没有收到结束事件）".into()),
        });
    }
    if st.sftp_transfer_state.is_active() {
        sftp_show_transfer(st);
        sftp_update_status(st);
        return;
    }

    let need_kill = was_active && disconnected;
    if let Some(job) = st.sftp_transfer.take() {
        if need_kill {
            job.cancel();
        }
        job.join();
    }
    let status = st.sftp_transfer_state.status_text();
    let phase = st.sftp_transfer_state.phase;
    let upload_done = st.sftp_transfer_state.is_done_upload();
    sftp_show_transfer(st);
    sftp_set_status(st, &status);
    eprintln!("[sftp] 传输结束 phase={phase:?}");
    if upload_done {
        // 上传完成后刷新远端列表，但保留“已上传”结果文案。
        sftp_do_list(st);
        sftp_set_status(st, &status);
    }
}

fn sftp_cancel_transfer(st: &mut AppState) {
    let Some(job) = st.sftp_transfer.as_ref() else {
        sftp_set_status(st, "没有进行中的传输");
        return;
    };
    job.cancel();
    sftp_set_status(st, "正在取消传输…");
}

/// 传输进行中时阻止会并发起 sftp 子进程的面板操作。
fn sftp_busy(st: &mut AppState) -> bool {
    if st.sftp_transfer.is_some() {
        sftp_set_status(st, "传输进行中，完成或取消后再操作");
        true
    } else {
        false
    }
}

fn sftp_do_list(st: &mut AppState) {
    if sftp_busy(st) {
        return;
    }
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    let remote = st.ui.get_sftp_remote().to_string();
    sftp_set_status(st, "列目录中…");
    match sftp::list(&target, &remote) {
        Ok(items) => {
            st.sftp_delete_armed = false;
            let n = items.len();
            let names: Vec<slint::SharedString> = items.into_iter().map(|s| s.into()).collect();
            st.sftp_model.set_vec(names);
            st.ui.set_sftp_selected(-1);
            sftp_set_status(st, &format!("{n} 个条目"));
        },
        Err(e) => sftp_set_status(st, &format!("列目录失败: {e}")),
    }
}

/// `ls -1 /abs` 有时回显绝对路径；别再把目录前缀拼一遍。
fn remote_entry_path(dir: &str, item: &str) -> String {
    if item.starts_with('/') {
        item.to_string()
    } else {
        join_remote(dir, item)
    }
}

fn join_remote(dir: &str, name: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        format!("/{name}")
    } else {
        format!("{trimmed}/{name}")
    }
}

fn sftp_do_download(st: &mut AppState) {
    if sftp_busy(st) {
        return;
    }
    if sftp_current_target(st).is_none() {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    }
    let sel = st.ui.get_sftp_selected();
    let Some(item) = st.sftp_model.row_data(sel.max(0) as usize) else {
        sftp_set_status(st, "先在列表里选一条");
        return;
    };
    let remote = remote_entry_path(&st.ui.get_sftp_remote(), &item);
    let local = st.ui.get_sftp_local().to_string();
    sftp_start_transfer(st, sftp::TransferDirection::Download, local, remote);
}

fn sftp_do_upload(st: &mut AppState) {
    if sftp_busy(st) {
        return;
    }
    if sftp_current_target(st).is_none() {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    }
    let local = st.ui.get_sftp_local().to_string();
    let name = std::path::Path::new(&local)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload.bin".to_string());
    let remote = join_remote(&st.ui.get_sftp_remote(), &name);
    sftp_start_transfer(st, sftp::TransferDirection::Upload, local, remote);
}

fn sftp_do_mkdir(st: &mut AppState) {
    if sftp_busy(st) {
        return;
    }
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    let name = st.ui.get_sftp_target().trim().to_string();
    if name.is_empty() {
        sftp_set_status(st, "先在 Target 里填目录名");
        return;
    }
    let remote = join_remote(&st.ui.get_sftp_remote(), &name);
    match sftp::mkdir(&target, &remote) {
        Ok(()) => {
            sftp_set_status(st, &format!("已创建 {remote}"));
            sftp_do_list(st);
        },
        Err(e) => sftp_set_status(st, &format!("建目录失败: {e}")),
    }
}

fn sftp_do_rename(st: &mut AppState) {
    if sftp_busy(st) {
        return;
    }
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    let sel = st.ui.get_sftp_selected();
    let Some(item) = st.sftp_model.row_data(sel.max(0) as usize) else {
        sftp_set_status(st, "先在列表里选一条");
        return;
    };
    let name = st.ui.get_sftp_target().trim().to_string();
    if name.is_empty() {
        sftp_set_status(st, "先在 Target 里填新名字");
        return;
    }
    let dir = st.ui.get_sftp_remote().to_string();
    let from = remote_entry_path(&dir, &item);
    let to = join_remote(&dir, &name);
    match sftp::rename(&target, &from, &to) {
        Ok(()) => {
            sftp_set_status(st, &format!("已重命名 {from} → {to}"));
            sftp_do_list(st);
        },
        Err(e) => sftp_set_status(st, &format!("重命名失败: {e}")),
    }
}

fn sftp_do_delete(st: &mut AppState) {
    if sftp_busy(st) {
        return;
    }
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    let sel = st.ui.get_sftp_selected();
    let Some(item) = st.sftp_model.row_data(sel.max(0) as usize) else {
        sftp_set_status(st, "先在列表里选一条");
        return;
    };
    let path = remote_entry_path(&st.ui.get_sftp_remote(), &item);
    if !st.sftp_delete_armed {
        st.sftp_delete_armed = true;
        sftp_set_status(st, &format!("再点一次 Delete 确认删除 {path}"));
        return;
    }
    st.sftp_delete_armed = false;
    match sftp::remove(&target, &path) {
        Ok(()) => {
            sftp_set_status(st, &format!("已删除 {path}"));
            sftp_do_list(st);
        },
        Err(e) => sftp_set_status(st, &format!("删除失败: {e}")),
    }
}

// ------------------------------------------------------- Port Forwards 面板

fn refresh_forwards(st: &mut AppState) {
    let idx = st.ui.get_fwd_profile().max(0) as usize;
    let items: Vec<slint::SharedString> = st
        .fwd_profile_indices
        .get(idx)
        .and_then(|i| st.profiles.profiles.get(*i))
        .map(|p| p.local_forwards.iter().map(|f| f.as_str().into()).collect())
        .unwrap_or_default();
    st.fwd_model.set_vec(items);
}

fn open_forwards_panel(st: &mut AppState) {
    let indices = sftp_profile_indices(st);
    let names: Vec<slint::SharedString> =
        indices.iter().map(|i| st.profiles.profiles[*i].name.clone().into()).collect();
    st.fwd_profile_indices = indices;
    let model = Rc::new(VecModel::from(names));
    st.ui.set_fwd_profiles(ModelRc::from(model));
    st.ui.set_fwd_profile(0);
    st.ui.set_fwd_status(
        if st.fwd_profile_indices.is_empty() {
            "没有 SSH Profile，先到 Profiles 里建一个"
        } else {
            "规则写入 Profile，下次连接生效"
        }
        .into(),
    );
    refresh_forwards(st);
    st.ui.set_fwd_open(true);
    eprintln!("[fwd] 打开面板 profiles={}", st.fwd_profile_indices.len());
}

fn fwd_add(st: &mut AppState) {
    let idx = st.ui.get_fwd_profile().max(0) as usize;
    let Some(profile_index) = st.fwd_profile_indices.get(idx).copied() else {
        st.ui.set_fwd_status("没有可用的 SSH Profile".into());
        return;
    };
    let spec = st.ui.get_fwd_new().trim().to_string();
    if let Err(e) = profile::validate_forward(&spec) {
        st.ui.set_fwd_status(e.as_str().into());
        return;
    }
    let Some(profile) = st.profiles.profiles.get_mut(profile_index) else { return };
    if profile.local_forwards.iter().any(|f| f == &spec) {
        st.ui.set_fwd_status("这条规则已存在".into());
        return;
    }
    profile.local_forwards.push(spec.clone());
    st.profiles.save();
    st.ui.set_fwd_new("".into());
    refresh_forwards(st);
    st.ui.set_fwd_status(format!("已添加 -L {spec}（下次连接生效）").as_str().into());
    eprintln!("[fwd] add {spec}");
}

fn fwd_delete(st: &mut AppState, index: usize) {
    let idx = st.ui.get_fwd_profile().max(0) as usize;
    let Some(profile_index) = st.fwd_profile_indices.get(idx).copied() else { return };
    let Some(profile) = st.profiles.profiles.get_mut(profile_index) else { return };
    if index >= profile.local_forwards.len() {
        return;
    }
    let spec = profile.local_forwards.remove(index);
    st.profiles.save();
    refresh_forwards(st);
    st.ui.set_fwd_status(format!("已删除 -L {spec}").as_str().into());
    eprintln!("[fwd] delete {spec}");
}

fn open_background_panel(st: &mut AppState) {
    refresh_background(st);
    st.ui.set_bg_open(true);
    eprintln!("[session] background panel items={}", st.detached.len());
}

/// 按索引把后台会话接回新 Tab。
fn reattach_background(st: &mut AppState, index: usize) {
    if index >= st.detached.len() {
        return;
    }
    let (id, rt, title) = st.detached.remove(index);
    st.panes.insert(id, rt);
    st.ws.tabs.push(Tab::single(id, title.clone()));
    st.ws.active = st.ws.tabs.len() - 1;
    refresh_views(st);
    rebuild_tabs(st);
    redraw_pane_inner(st, id);
    refresh_background(st);
    eprintln!("[session] reattach pane={id} title={title:?}（剩余后台 {}）", st.detached.len());
}

/// 把最近一个后台会话接回当前窗口（新 Tab）。
fn reattach_last_background(st: &mut AppState) {
    if st.detached.is_empty() {
        eprintln!("[session] 没有可重连的后台会话");
        return;
    }
    reattach_background(st, st.detached.len() - 1);
}

fn close_tab_inner(st: &mut AppState, index: usize) {
    if index >= st.ws.tabs.len() {
        return;
    }
    let tab_title = st.ws.tabs[index].title.clone();
    if let Some(snap) = snapshot_tab(st, index) {
        st.closed_tabs.push(snap);
        if st.closed_tabs.len() > 10 {
            st.closed_tabs.remove(0);
        }
    }
    let background = st.cfg.close_action == "background";
    for id in st.ws.tabs[index].panes() {
        if background {
            detach_runtime(st, id);
        } else {
            st.panes.remove(&id); // Drop => 关闭 PTY / 结束 actor
        }
    }
    st.ws.tabs.remove(index);
    dispatch_plugin_event(st, plugin::PluginEvent::TabClose, plugin::tab_close_payload(&tab_title));
    if st.ws.tabs.is_empty() {
        let _ = slint::quit_event_loop();
        return;
    }
    st.ws.active = st.ws.active.min(st.ws.tabs.len() - 1);
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
}

fn close_tab(index: usize) {
    with_app(|st| close_tab_inner(st, index));
}

/// 重命名输入清洗：去首尾空白、拒绝空标题、限制长度（避免把 TabBar 撑坏）。
fn sanitize_tab_title(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.chars().count() > 120 {
        return None;
    }
    Some(t.to_string())
}

fn open_rename(st: &mut AppState, index: usize) {
    let Some(tab) = st.ws.tabs.get(index) else { return };
    eprintln!("[ui] rename tab={index} title={:?}", tab.title);
    st.ui.set_tabctx_open(false);
    st.ui.set_rename_index(index as i32);
    st.ui.set_rename_text(tab.title.as_str().into());
    st.ui.set_rename_open(true);
}

/// 关闭 keep 左侧的所有 Tab；keep 本身保留并保持激活。
fn close_tabs_left(st: &mut AppState, keep: usize) {
    if keep == 0 || keep >= st.ws.tabs.len() {
        return;
    }
    eprintln!("[ws] close tabs left keep={keep}");
    let to_close = keep.min(st.ws.tabs.len().saturating_sub(1));
    for _ in 0..to_close {
        // 每轮都关第 0 个；目标 Tab 会逐步左移，关闭 keep 次后正好成为第 0 个。
        if st.ws.active == 0 {
            st.ws.active = 1;
        }
        close_tab_inner(st, 0);
        if st.ws.tabs.is_empty() {
            return;
        }
    }
    set_active_tab_inner(st, 0);
}

/// 关闭 keep 右侧的所有 Tab；keep 本身保留并保持激活。
fn close_tabs_right(st: &mut AppState, keep: usize) {
    if keep + 1 >= st.ws.tabs.len() {
        return;
    }
    eprintln!("[ws] close tabs right keep={keep}");
    while st.ws.tabs.len() > keep + 1 {
        close_tab_inner(st, st.ws.tabs.len() - 1);
        if st.ws.tabs.is_empty() {
            return;
        }
    }
    set_active_tab_inner(st, keep.min(st.ws.tabs.len() - 1));
}

/// 只保留指定 Tab，其它 Tab 的 PTY 全部关掉。
fn close_other_tabs(st: &mut AppState, keep: usize) {
    if keep >= st.ws.tabs.len() {
        return;
    }
    let closed_titles: Vec<String> = st
        .ws
        .tabs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != keep)
        .map(|(_, tab)| tab.title.clone())
        .collect();
    let dropped = st.ws.keep_only(keep);
    eprintln!("[ws] close other tabs, keep={keep}, 关闭 {} 个 pane", dropped.len());
    for title in &closed_titles {
        dispatch_plugin_event(st, plugin::PluginEvent::TabClose, plugin::tab_close_payload(title));
    }
    let background = st.cfg.close_action == "background";
    for id in dropped {
        if background {
            detach_runtime(st, id);
        } else {
            st.panes.remove(&id);
        }
    }
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
}

/// 复制一个 Tab：布局、分屏比例、每个 pane 的 cwd 都照抄。
fn duplicate_tab(st: &mut AppState, index: usize) {
    let (title, focused, ids, tree) = {
        let Some(src) = st.ws.tabs.get(index) else { return };
        let Some(root) = src.root.as_ref() else { return };
        let ids = root.panes();
        let mut cwds = HashMap::with_capacity(ids.len());
        for id in &ids {
            let cwd = st
                .panes
                .get(id)
                .and_then(|rt| rt.handle.pid())
                .and_then(session::cwd_of);
            cwds.insert(*id, cwd);
        }
        (src.title.clone(), src.focused, ids, session::Tree::from_node(root, &cwds))
    };

    let leaf_cwds = tree.leaf_cwds();
    eprintln!("[ws] duplicate tab={index} panes={}", leaf_cwds.len());
    let new_ids: Vec<PaneId> = (0..leaf_cwds.len()).map(|_| st.ws.alloc_id()).collect();
    let Some(new_root) = tree.assign(&new_ids) else { return };
    let (cols, rows) = cell_of(st.content, st.cell);

    let mut created = Vec::with_capacity(new_ids.len());
    for (i, cwd) in leaf_cwds.iter().enumerate() {
        match create_pane(st, new_ids[i], cols, rows, cwd.as_ref().map(PathBuf::from), None, None) {
            Ok(rt) => {
                st.panes.insert(new_ids[i], rt);
                created.push(new_ids[i]);
            },
            Err(e) => {
                eprintln!("[ws] 复制 Tab 时创建 pane 失败: {e}");
                break;
            },
        }
    }
    if created.len() != new_ids.len() {
        for id in &created {
            st.panes.remove(id);
        }
        return;
    }

    let focused_idx = ids.iter().position(|id| *id == focused).unwrap_or(0);
    let focused_new = new_ids.get(focused_idx).copied().unwrap_or(new_ids[0]);
    let new_title = format!("{title} (copy)");
    st.ws.tabs.insert(
        index + 1,
        Tab {
            title: new_title.clone(),
            root: Some(new_root),
            focused: focused_new,
            grid: HashMap::new(),
        },
    );
    st.ws.active = index + 1;
    dispatch_plugin_event(st, plugin::PluginEvent::TabOpen, plugin::tab_open_payload(&new_title, "local"));
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
}

fn close_pane_inner(st: &mut AppState, id: PaneId) {
    let Some(tab_index) = st.ws.tab_of(id) else { return };
    let tab_title = st.ws.tabs[tab_index].title.clone();
    let emptied = st.ws.tabs[tab_index].remove(id);
    if st.cfg.close_action == "background" {
        detach_runtime(st, id);
    } else {
        st.panes.remove(&id);
    }
    // pane_rows 只由 refresh_views 维护：在这里提前删会破坏「模型行数是否仍然对齐」的判断。
    if emptied {
        st.ws.tabs.remove(tab_index);
        dispatch_plugin_event(st, plugin::PluginEvent::TabClose, plugin::tab_close_payload(&tab_title));
        if st.ws.tabs.is_empty() {
            let _ = slint::quit_event_loop();
            return;
        }
        st.ws.active = st.ws.active.min(st.ws.tabs.len() - 1);
        rebuild_tabs(st);
    }
    refresh_views(st);
    redraw_visible(st);
    // 关窗排查：pane 数、模型行数、剩余 pane 的实际矩形，三者必须自洽。
    let panes = st.ws.active_tab().map(|t| t.panes().len()).unwrap_or(0);
    let rects: Vec<(PaneId, Rect)> = {
        let mut out = Vec::new();
        if let Some(tab) = st.ws.active_tab() {
            if let Some(root) = &tab.root {
                root.layout(st.content, &mut out);
            }
        }
        out
    };
    eprintln!(
        "[ws] close pane={id} emptied={emptied} panes={panes} model_rows={} rects={rects:?}",
        st.pane_model.row_count()
    );
}

fn close_pane(id: PaneId) {
    with_app(|st| close_pane_inner(st, id));
}

// --------------------------------------------------------------- 会话快照

/// 把当前布局 + 每个 pane 的 cwd 收成快照（不含屏幕内容）。
/// 关闭 Tab 时抓一份布局/cwd 快照，供「重开最近关闭的 Tab」使用。
/// 只存布局与 cwd，不保留进程（重开是新 shell）。
fn snapshot_tab(st: &AppState, index: usize) -> Option<session::TabSnapshot> {
    let tab = st.ws.tabs.get(index)?;
    let root = tab.root.as_ref()?;
    let ids = root.panes();
    let mut cwds = HashMap::with_capacity(ids.len());
    for id in &ids {
        let cwd = st
            .panes
            .get(id)
            .and_then(|rt| rt.handle.pid())
            .and_then(session::cwd_of);
        cwds.insert(*id, cwd);
    }
    let focused = ids.iter().position(|id| *id == tab.focused).unwrap_or(0);
    Some(session::TabSnapshot {
        title: tab.title.clone(),
        focused,
        tree: session::Tree::from_node(root, &cwds),
    })
}

/// 按快照恢复一个 Tab（新 Pane、新 shell）；失败时清理半成品。
fn restore_tab_snapshot(st: &mut AppState, tab_snap: &session::TabSnapshot) -> bool {
    let (cols, rows) = cell_of(st.content, st.cell);
    let cwds = tab_snap.tree.leaf_cwds();
    let ids: Vec<PaneId> = (0..cwds.len()).map(|_| st.ws.alloc_id()).collect();
    let Some(root) = tab_snap.tree.assign(&ids) else {
        eprintln!("[session] 布局与 pane 数量不符，跳过该标签");
        return false;
    };
    let mut created = 0;
    for (i, cwd) in cwds.iter().enumerate() {
        match create_pane(st, ids[i], cols, rows, cwd.as_ref().map(PathBuf::from), None, None) {
            Ok(rt) => {
                st.panes.insert(ids[i], rt);
                created += 1;
            },
            Err(e) => {
                eprintln!("[session] pane 恢复失败: {e}");
                break;
            },
        }
    }
    if created != ids.len() {
        for id in &ids {
            st.panes.remove(id);
        }
        return false;
    }
    let focused = ids.get(tab_snap.focused).copied().unwrap_or(ids[0]);
    let tab_title = tab_snap.title.clone();
    st.ws.tabs.push(Tab {
        title: tab_title.clone(),
        root: Some(root),
        focused,
        grid: HashMap::new(),
    });
    dispatch_plugin_event(st, plugin::PluginEvent::TabOpen, plugin::tab_open_payload(&tab_title, "local"));
    true
}

/// 重开最近关闭的 Tab（Ctrl+Shift+T / 菜单）。
fn reopen_closed_tab(st: &mut AppState) {
    let Some(snap) = st.closed_tabs.pop() else {
        eprintln!("[ws] 没有可重开的 Tab");
        return;
    };
    if !restore_tab_snapshot(st, &snap) {
        eprintln!("[ws] 重开失败: {:?}", snap.title);
        return;
    }
    st.ws.active = st.ws.tabs.len() - 1;
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
    eprintln!(
        "[ws] reopen closed tab {:?}（剩余 {}）",
        snap.title,
        st.closed_tabs.len()
    );
}

fn capture_snapshot(st: &AppState) -> Option<session::Snapshot> {
    if st.ws.tabs.is_empty() {
        return None;
    }
    let (cols, rows) = cell_of(st.content, st.cell);
    let mut tabs = Vec::with_capacity(st.ws.tabs.len());
    for tab in &st.ws.tabs {
        let root = tab.root.as_ref()?;
        let ids = root.panes();
        let mut cwds = HashMap::with_capacity(ids.len());
        for id in &ids {
            // cwd 从子进程的 /proc/<pid>/cwd 取，用户 cd 到哪就恢复到哪
            let cwd = st
                .panes
                .get(id)
                .and_then(|rt| rt.handle.pid())
                .and_then(session::cwd_of);
            cwds.insert(*id, cwd);
        }
        let focused = ids.iter().position(|id| *id == tab.focused).unwrap_or(0);
        tabs.push(session::TabSnapshot {
            title: tab.title.clone(),
            focused,
            tree: session::Tree::from_node(root, &cwds),
        });
    }
    Some(session::Snapshot {
        version: session::VERSION,
        cols: cols as u16,
        rows: rows as u16,
        active: st.ws.active,
        tabs,
    })
}

/// 保存快照 → 拉起新进程（带 `--restore`）→ 退出当前进程。
fn restart_session(st: &mut AppState) {
    let Some(snap) = capture_snapshot(st) else {
        eprintln!("[session] 没有可保存的布局，取消重启");
        return;
    };
    match session::save(&snap) {
        Ok(path) => {
            let cwds: Vec<String> = snap
                .tabs
                .iter()
                .flat_map(|t| t.tree.leaf_cwds())
                .map(|c| c.unwrap_or_else(|| "?".into()))
                .collect();
            eprintln!(
                "[session] 已保存 {}（{} 个标签 / {} 个 pane）cwd={cwds:?}",
                path.display(),
                snap.tabs.len(),
                snap.tabs.iter().map(|t| t.tree.leaf_count()).sum::<usize>()
            )
        },
        Err(e) => {
            eprintln!("[session] 保存失败，取消重启: {e}");
            return;
        },
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[session] 找不到自身可执行文件: {e}");
            return;
        },
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--restore")
        .arg("--cols")
        .arg(snap.cols.to_string())
        .arg("--rows")
        .arg(snap.rows.to_string())
        .arg("--font-size")
        .arg(st.opts.font_size.to_string())
        .arg("--scrollback")
        .arg(st.opts.scrollback.to_string());
    if let Some(shell) = &st.opts.shell {
        cmd.arg("--shell").arg(shell);
    }
    if !st.opts.args.is_empty() {
        cmd.arg("--").args(&st.opts.args);
    }
    // 脱离旧进程的会话：旧窗口关掉 / 终端退出都不该把新实例一起带走。
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => {
            eprintln!("[session] 已拉起新实例 pid={}，退出当前进程", child.id());
            let _ = slint::quit_event_loop();
        },
        Err(e) => eprintln!("[session] 重启失败（保留当前窗口）: {e}"),
    }
}

/// 按快照重建 tabs/panes；成功返回 true。
fn restore_session(st: &mut AppState) -> bool {
    let Some(snap) = session::load() else { return false };
    if snap.tabs.is_empty() {
        return false;
    }

    // 丢掉启动时建的默认 pane（Drop 会关掉它的 PTY）
    st.ws.tabs.clear();
    st.panes.clear();
    st.pane_rows.clear();

    for tab_snap in &snap.tabs {
        restore_tab_snapshot(st, tab_snap);
    }

    if st.ws.tabs.is_empty() {
        return false;
    }
    st.ws.active = snap.active.min(st.ws.tabs.len() - 1);
    st.next_title = st.ws.tabs.len();
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
    // 快照只消费一次：恢复成功就删掉，避免下次手动 --restore 又拉旧布局
    session::clear();
    true
}

fn split_pane_inner(st: &mut AppState, id: PaneId, dir: Dir) {
    eprintln!("[ws] split pane={id} dir={dir:?}");
    let Some(tab_index) = st.ws.tab_of(id) else { return };
    let Some((cols, rows)) = st.panes.get(&id).map(|r| r.grid) else { return };
    let guess = match dir {
        Dir::Row => ((cols / 2).max(10), rows),
        Dir::Col => (cols, (rows / 2).max(3)),
    };
    let new_id = st.ws.alloc_id();
    match create_pane(st, new_id, guess.0, guess.1, None, None, None) {
        Ok(rt) => {
            st.panes.insert(new_id, rt);
            st.ws.tabs[tab_index].split(id, dir, new_id);
            st.ws.tabs[tab_index].focused = new_id;
            refresh_views(st);
            redraw_visible(st);
        },
        Err(e) => eprintln!("分屏失败: {e}"),
    }
}

fn split_pane(id: PaneId, dir: Dir) {
    with_app(|st| split_pane_inner(st, id, dir));
}

fn focus_pane(id: PaneId) {
    with_app(|st| {
        let Some(idx) = st.ws.tab_of(id) else { return };
        if st.ws.tabs[idx].focused == id {
            return;
        }
        st.ws.tabs[idx].focused = id;
        refresh_views(st);
    });
}

/// 关闭前是否需要确认：开着开关且不止一个会话时弹窗，避免误关杀掉一堆 shell。
fn needs_close_confirm(tabs: usize, panes: usize, enabled: bool) -> bool {
    enabled && (tabs > 1 || panes > 1)
}

fn request_close(st: &mut AppState) {
    let tabs = st.ws.tabs.len();
    let panes: usize = st.ws.tabs.iter().map(|t| t.panes().len()).sum();
    eprintln!("[ui] close requested: tabs={tabs} panes={panes} confirm={}", st.cfg.confirm_close);
    if needs_close_confirm(tabs, panes, st.cfg.confirm_close) {
        let text = format!("将结束 {tabs} 个标签、{panes} 个终端会话，子进程会被终止。");
        eprintln!("[ui] close confirm: tabs={tabs} panes={panes}");
        st.ui.set_confirm_text(text.as_str().into());
        st.ui.set_confirm_open(true);
        return;
    }
    let _ = slint::quit_event_loop();
}

/// 通知后台 worker 重算 X11 SHAPE 圆角。
///
/// Slint 的尺寸/`resized` 回调都是逻辑像素，worker 会乘 `scale_factor` 换成物理
/// 像素并以 X11 `get_geometry` 为准；`state` 为 None 时查 `_NET_WM_STATE`，Some
/// 表示调用方刚设置了最大化/全屏（避免 WM 属性更新滞后导致角落留缝）。
fn request_window_corners(
    st: &AppState,
    logical: Option<(f32, f32)>,
    state: Option<xconn::CornerState>,
) {
    let Some(shaper) = &st.corners else { return };
    let window = st.ui.window();
    let scale = window.scale_factor();
    let (logical_width, logical_height) = match logical {
        Some(size) => size,
        None => {
            // window.size() 是物理像素；除以 scale 还原逻辑尺寸，供 worker 兜底/日志。
            let physical = window.size();
            let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
            (physical.width as f32 / scale, physical.height as f32 / scale)
        },
    };
    shaper.request(xconn::CornerRequest {
        logical_width,
        logical_height,
        scale_factor: scale,
        state,
    });
}

/// F11 / --fullscreen：切换全屏。
fn toggle_fullscreen(st: &AppState) {
    // 注意：本函数会被 handle_input / run_command 在 `with_app` 的借用内调用，
    // 这里绝不能再 with_app，否则 RefCell 二次可变借用直接 panic（F11 崩溃根因）。
    let win = st.ui.window();
    let next = !win.is_fullscreen();
    win.set_fullscreen(next);
    // 全屏状态是调用方刚设置的，作为权威状态直接重算圆角（不依赖 WM 属性更新时序）。
    request_window_corners(
        st,
        None,
        Some(xconn::CornerState { maximized: win.is_maximized(), fullscreen: next }),
    );
    eprintln!("[ui] fullscreen={next}");
}

fn set_active_tab(index: usize) {
    with_app(|st| set_active_tab_inner(st, index));
}

fn set_active_tab_inner(st: &mut AppState, index: usize) {
    if index >= st.ws.tabs.len() || st.ws.active == index {
        return;
    }
    st.ws.active = index;
    // 获得焦点的 Tab：清除未读通知点 / 完成标记（Tabby 的 unread 语义）。
    let ids = st.ws.tabs[index].panes();
    let mut cleared = 0usize;
    for id in ids {
        let Some(rt) = st.panes.get_mut(&id) else { continue };
        if rt.activity.attention || rt.activity.completed || rt.notification.is_some() {
            rt.activity.attention = false;
            rt.activity.completed = false;
            rt.notification = None;
            cleared += 1;
        }
    }
    eprintln!("[ws] active tab={index}");
    if cleared > 0 {
        eprintln!("[ui] tab focus tab={index} clear attention/notifications={cleared}");
    }
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
}

/// 拖拽落点：按平均槽位（padding-left 32px）把指针 x 换算成目标下标。
fn drag_target(drag_x: f32, strip_w: f32, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let slot = ((strip_w - 32.0) / n as f32).max(1.0);
    let idx = ((drag_x - 32.0) / slot).floor().max(0.0) as usize;
    idx.min(n - 1)
}

/// 相对切 Tab（循环）。`len > 0` 由调用方保证。
fn next_tab_index(cur: usize, len: usize, delta: isize) -> usize {
    let n = len as isize;
    ((((cur as isize + delta) % n) + n) % n) as usize
}

/// 注意：与 toggle_fullscreen 同理，本函数在 handle_input / run_command 的
/// `with_app` 借用内被调用，内部不得再 with_app，否则二次借用 panic。
fn switch_tab_relative(st: &mut AppState, delta: isize) {
    let len = st.ws.tabs.len();
    if len < 2 {
        return;
    }
    let next = next_tab_index(st.ws.active, len, delta);
    set_active_tab_inner(st, next);
}

/// Ctrl+1..9：跳到第 n 个 Tab（不存在则忽略）。
fn switch_tab_number(st: &mut AppState, n: usize) {
    if n >= 1 && n <= st.ws.tabs.len() {
        set_active_tab_inner(st, n - 1);
    }
}

// ------------------------------------------------------------------- 布局

/// 重新计算布局：必要时 resize 各 pane 的网格，并刷新 Slint 模型。
fn refresh_views(st: &mut AppState) {
    refresh_views_impl(st, true);
}

/// 拖动 resize 的快路径：只更新 pane 矩形，不 resize grid/PTY。
fn refresh_views_layout(st: &mut AppState) {
    refresh_views_impl(st, false);
}

fn refresh_views_impl(st: &mut AppState, resize_terminals: bool) {
    if st.content.w < 1.0 || st.content.h < 1.0 {
        return;
    }
    let (rects, focused, dividers) = {
        let Some(tab) = st.ws.active_tab() else { return };
        let mut rects: Vec<(PaneId, Rect)> = Vec::new();
        let mut dividers: Vec<Divider> = Vec::new();
        if let Some(root) = &tab.root {
            root.layout(st.content, &mut rects);
            root.dividers(st.content, &mut dividers);
        }
        (rects, tab.focused, dividers)
    };

    for (id, rect) in &rects {
        if let Some(rt) = st.panes.get_mut(id) {
            let (cols, rows) = cell_of(*rect, st.cell);
            rt.rect = *rect;
            if resize_terminals && rt.grid != (cols, rows) {
                rt.renderer.resize_grid(cols, rows);
                rt.handle.resize(cols as u16, rows as u16);
                rt.grid = (cols, rows);
            }
        }
    }

    let make_view = |id: &PaneId, rect: &Rect| -> Option<PaneView> {
        st.panes.get(id).map(|rt| PaneView {
            id: *id as i32,
            image: rt.image.clone(),
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            focused: *id == focused,
            iw: rt.renderer.width as f32,
            ih: rt.renderer.height as f32,
            mouse_reporting: rt.handle.wants_mouse(),
        })
    };

    // 拖拽/窗口缩放时布局顺序不变，用 set_row_data 避免重建 TouchArea，
    // 否则拖分隔条时按下状态会在每一帧被重置。
    //
    // 判断依据必须包含**模型实际行数**：关掉右侧/下方 pane 时，`close_pane` 会先把
    // 它从 `pane_rows` 删掉，若只看 map 长度就会误判「顺序没变」，set_row_data 只覆盖
    // 前 N 行，被关掉那个 pane 的视图留在模型里变成一块不再刷新的幽灵分屏。
    let order: Vec<PaneId> = rects.iter().map(|(id, _)| *id).collect();
    let same_order = rows_can_update_in_place(st.pane_model.row_count(), &st.pane_rows, &order);
    if same_order {
        for (i, (id, rect)) in rects.iter().enumerate() {
            if let Some(view) = make_view(id, rect) {
                st.pane_model.set_row_data(i, view);
            }
        }
    } else {
        st.pane_rows.clear();
        let mut views = Vec::with_capacity(rects.len());
        for (i, (id, rect)) in rects.iter().enumerate() {
            if let Some(view) = make_view(id, rect) {
                st.pane_rows.insert(*id, i);
                views.push(view);
            }
        }
        st.pane_model.set_vec(views);
    }

    // 分隔条模型 + 拖拽几何
    st.divider_infos = dividers;
    let views: Vec<DividerView> = st
        .divider_infos
        .iter()
        .enumerate()
        .map(|(i, d)| DividerView {
            index: i as i32,
            x: d.rect.x,
            y: d.rect.y,
            w: d.rect.w,
            h: d.rect.h,
            vertical: d.dir == Dir::Row,
        })
        .collect();
    if st.divider_model.row_count() == views.len() {
        for (i, view) in views.into_iter().enumerate() {
            st.divider_model.set_row_data(i, view);
        }
    } else {
        st.divider_model.set_vec(views);
    }
}

/// `pane_model` 能否原地更新：模型行数等于布局数量，且每行 id 顺序一致。
///
/// 只比较 `pane_rows` 长度是不够的：`close_pane_inner` 会先把被关的 pane 从
/// `pane_rows` 删掉，此时 map 长度已和布局一致，但模型里还留着旧行，
/// `set_row_data` 只会覆盖前几行，被关掉的 pane 就成了一块不再刷新的「幽灵分屏」。
fn rows_can_update_in_place(
    model_rows: usize,
    rows: &HashMap<PaneId, usize>,
    order: &[PaneId],
) -> bool {
    model_rows == order.len() && order.iter().enumerate().all(|(i, id)| rows.get(id) == Some(&i))
}

fn rebuild_tabs(st: &mut AppState) {
    let active = st.ws.active;
    let tabs: Vec<TabView> = st
        .ws
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            // 一个 Tab 可能有多个 pane：错误优先、进行中次之，通知/完成取并集。
            let agg = activity::aggregate(
                t.panes().iter().filter_map(|id| st.panes.get(id)).map(|rt| &rt.activity),
            );
            TabView {
                title: t.title.clone().into(),
                active: i == active,
                progress_state: agg.progress.state as i32,
                progress_ratio: activity::progress_ratio(agg.progress),
                progress_label: activity::progress_label(agg.progress).into(),
                attention: agg.attention,
                completed: agg.completed,
            }
        })
        .collect();
    st.tab_model.set_vec(tabs);
}

/// 重绘当前可见的 pane（切换 tab / 布局变化后）。
fn redraw_visible(st: &mut AppState) {
    let ids: Vec<PaneId> = {
        let Some(tab) = st.ws.active_tab() else { return };
        tab.panes()
    };
    for id in ids {
        redraw_pane_inner(st, id);
    }
}

// ------------------------------------------------------------------- 渲染

fn redraw_pane(id: PaneId) {
    if PIPE_ON.load(Ordering::Relaxed) {
        if let Some(t) = LAST_KEY.lock().unwrap().take() {
            PIPE_SAMPLES.lock().unwrap().push(t.elapsed());
        }
    }
    with_app(|st| redraw_pane_inner(st, id));
}

fn redraw_pane_inner(st: &mut AppState, id: PaneId) {
    let focused = st.ws.active_tab().map(|t| t.focused) == Some(id);
    let Some(rt) = st.panes.get_mut(&id) else { return };
    let frame = rt.handle.pull(std::mem::take(&mut rt.frame));
    rt.renderer.draw(&frame);
    rt.image = rt.renderer.image();

    if focused {
        st.echo.store(count_marker(&frame, MARKER), Ordering::Release);
        // IME 候选窗跟着光标走：把聚焦 pane 的光标格换算成内容区像素坐标。
        let px = rt.rect.x + frame.cursor.col as f32 * rt.renderer.cell_w as f32;
        let py = rt.rect.y + frame.cursor.line as f32 * rt.renderer.cell_h as f32;
        st.ui.set_ime_x(px);
        st.ui.set_ime_y(py);
    }

    if let Some(&row) = st.pane_rows.get(&id) {
        let rect = rt.rect;
        let view = PaneView {
            id: id as i32,
            image: rt.image.clone(),
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            focused,
            iw: rt.renderer.width as f32,
            ih: rt.renderer.height as f32,
            mouse_reporting: rt.handle.wants_mouse(),
        };
        st.pane_model.set_row_data(row, view);
    }
    rt.frame = frame;
}

/// 字号缩放（Ctrl+± / Ctrl+0 与命令面板共用）。
fn zoom_font(st: &mut AppState, delta: f32) {
    let next = (st.cfg.font_size + delta).clamp(8.0, 40.0);
    if (next - st.cfg.font_size).abs() < f32::EPSILON {
        return;
    }
    st.cfg.font_size = next;
    apply_font_size(st);
    st.cfg.save();
    eprintln!("[ui] font_size={next}");
}

fn reset_font(st: &mut AppState) {
    if (st.cfg.font_size - 15.0).abs() < f32::EPSILON {
        return;
    }
    st.cfg.font_size = 15.0;
    apply_font_size(st);
    st.cfg.save();
    eprintln!("[ui] font_size=15（重置）");
}

/// 字号变更：重建字体集与所有 pane 的渲染器，再按新网格重排。
fn apply_font_size(st: &mut AppState) {
    let Some(fonts) = FontSet::load(st.cfg.font_size) else {
        eprintln!("[config] 字体加载失败，字号变更忽略");
        return;
    };
    let fonts = Rc::new(fonts);
    st.cell = (fonts.cell_w, fonts.cell_h);
    st.fonts = fonts.clone();
    let theme = st.resolved_theme.clone();
    let opacity = renderer_opacity(st);
    for rt in st.panes.values_mut() {
        let (cols, rows) = rt.grid;
        rt.renderer = Renderer::new(
            fonts.clone(),
            cols,
            rows,
            theme.spec.bg,
            theme.spec.cursor,
            opacity,
        );
    }
    refresh_views(st);
    redraw_visible(st);
}

fn redraw_all() {
    let ids: Vec<PaneId> = with_app(|st| st.panes.keys().copied().collect()).unwrap_or_default();
    for id in ids {
        redraw_pane(id);
    }
}

// --------------------------------------------------------------- 选择/搜索

fn cell_at(rt: &PaneRuntime, x: f32, y: f32) -> (u16, u16) {
    let cw = rt.renderer.cell_w.max(1) as f32;
    let ch = rt.renderer.cell_h.max(1) as f32;
    let col = (x / cw).floor().max(0.0) as usize;
    let row = (y / ch).floor().max(0.0) as usize;
    let cols = rt.grid.0.max(1);
    let rows = rt.grid.1.max(1);
    (row.min(rows - 1) as u16, col.min(cols - 1) as u16)
}

/// 打开 pane 右键菜单；坐标是内容区坐标（Slint 侧负责夹取）。
/// Ctrl+Shift+方向键：按 pane 中心点的方向投影切焦点（同方向取最近）。
fn focus_pane_dir(st: &mut AppState, dx: f32, dy: f32) {
    let Some(tab) = st.ws.active_tab() else { return };
    let ids = tab.panes();
    if ids.len() < 2 {
        return;
    }
    let cur = tab.focused;
    let Some(cur_rect) = st.panes.get(&cur).map(|rt| rt.rect) else { return };
    let (cx, cy) = (cur_rect.x + cur_rect.w / 2.0, cur_rect.y + cur_rect.h / 2.0);
    let mut best: Option<(f32, PaneId)> = None;
    for id in ids {
        if id == cur {
            continue;
        }
        let Some(r) = st.panes.get(&id).map(|rt| rt.rect) else { continue };
        let (x, y) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (adx, ady) = (x - cx, y - cy);
        let primary = adx * dx + ady * dy;
        if primary <= 0.5 {
            continue;
        }
        let lateral = (adx * dy).abs() + (ady * dx).abs();
        let score = primary + lateral * 2.0;
        if best.map_or(true, |(b, _)| score < b) {
            best = Some((score, id));
        }
    }
    if let Some((_, id)) = best {
        if let Some(idx) = st.ws.tab_of(id) {
            st.ws.tabs[idx].focused = id;
        }
        refresh_views(st);
        redraw_visible(st);
        eprintln!("[ws] focus pane={id} dir=({dx},{dy})");
    }
}

fn open_pane_menu(st: &mut AppState, id: PaneId, x: f32, y: f32) {
    eprintln!("[ui] right click pane={id} at {x},{y}");
    st.ui.set_menu_pane(id as i32);
    st.ui.set_menu_x(x);
    st.ui.set_menu_y(y);
    st.ui.set_menu_open(true);
}

/// 点到矩形的最短距离平方（点在矩形内为 0）。
fn dist2_to_rect(r: Rect, x: f32, y: f32) -> f32 {
    let dx = (r.x - x).max(x - (r.x + r.w)).max(0.0);
    let dy = (r.y - y).max(y - (r.y + r.h)).max(0.0);
    dx * dx + dy * dy
}

/// 内容区坐标 → 最近的 pane。分隔线缝隙、窗口边缘的右键都靠它兜底。
fn nearest_pane(rects: &[(PaneId, Rect)], x: f32, y: f32) -> Option<PaneId> {
    rects
        .iter()
        .map(|(id, r)| (*id, dist2_to_rect(*r, x, y)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id)
}

/// Slint 修饰键位（1=Shift 2=Alt 4=Ctrl）→ 鼠标修饰键。
fn mouse_mods(bits: i32) -> Modifiers {
    Modifiers { shift: bits & 1 != 0, alt: bits & 2 != 0, ctrl: bits & 4 != 0 }
}

/// Slint 按键号（0=左 1=中 2=右）→ 上报按键。
fn mouse_button(code: i32) -> MouseButton {
    match code {
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => MouseButton::Left,
    }
}

fn set_clipboard(st: &mut AppState, text: &str) {
    eprintln!("[clipboard] 复制 {} 字节: {:?}", text.len(), text.chars().take(40).collect::<String>());
    if st.clipboard.is_none() {
        st.clipboard = arboard::Clipboard::new().ok();
    }
    match &mut st.clipboard {
        Some(cb) => {
            if let Err(e) = cb.set_text(text.to_string()) {
                eprintln!("[clipboard] 写入失败: {e}");
            }
        },
        None => eprintln!("[clipboard] 不可用"),
    }
}

fn get_clipboard(st: &mut AppState) -> Option<String> {
    if st.clipboard.is_none() {
        st.clipboard = arboard::Clipboard::new().ok();
    }
    st.clipboard.as_mut().and_then(|cb| cb.get_text().ok())
}

/// 复制前归一化：去掉行尾多余换行（全选时后面一堆空行不该进剪贴板），
/// 但保留行内空格与中间空行；全空白结果直接丢弃。
fn normalize_clipboard_text(text: String) -> Option<String> {
    let t = text.trim_end_matches(['\r', '\n']);
    (!t.is_empty()).then(|| t.to_string())
}

fn copy_current_selection(st: &mut AppState) {
    let focused = st.ws.active_tab().map(|t| t.focused);
    let Some(id) = focused else { return };
    let text = st.panes.get(&id).and_then(|rt| rt.handle.copy_selection()).and_then(normalize_clipboard_text);
    if let Some(text) = text {
        set_clipboard(st, &text);
    }
}

/// Tabby 的 Clipboard 右键：有选区复制并清除，否则粘贴。
fn copy_or_paste(st: &mut AppState) {
    let Some(id) = st.ws.active_tab().map(|t| t.focused) else { return };
    let text = st
        .panes
        .get(&id)
        .and_then(|rt| rt.handle.copy_selection())
        .and_then(normalize_clipboard_text);
    if let Some(text) = text {
        set_clipboard(st, &text);
        if let Some(rt) = st.panes.get(&id) {
            rt.handle.clear_selection();
        }
        eprintln!("[ui] right-click clipboard copy {} chars", text.chars().count());
    } else {
        paste_into_focused(st);
    }
}

/// 剪贴板文本 → 终端输入：CRLF/CR 统一成 CR（终端回车）。
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\r").replace('\n', "\r")
}

/// 组装实际写入 PTY 的字节；bracketed 模式包上 200~/201~。
fn paste_payload(normalized: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return normalized.as_bytes().to_vec();
    }
    let mut payload = Vec::with_capacity(normalized.len() + 12);
    payload.extend_from_slice(b"\x1b[200~");
    payload.extend_from_slice(normalized.as_bytes());
    payload.extend_from_slice(b"\x1b[201~");
    payload
}

/// 多行粘贴是否需要先确认。
fn needs_paste_confirm(text: &str, enabled: bool) -> bool {
    enabled && text.contains('\n')
}

fn deliver_paste(st: &mut AppState, text: &str) {
    let normalized = normalize_paste(text);
    let Some(focused) = st.ws.active_tab().map(|t| t.focused) else { return };
    let bracketed = st
        .panes
        .get(&focused)
        .map(|rt| rt.frame.mode & BRACKETED_PASTE != 0)
        .unwrap_or(false);
    let payload = paste_payload(&normalized, bracketed);
    eprintln!("[ui] paste bytes={} bracketed={bracketed}", payload.len());
    if let Some(rt) = st.panes.get_mut(&focused) {
        rt.handle.input(&payload);
    }
}

fn paste_into_focused(st: &mut AppState) {
    let Some(text) = get_clipboard(st) else { return };
    if text.is_empty() {
        return;
    }
    if needs_paste_confirm(&text, st.cfg.confirm_multiline_paste) {
        let lines = text.lines().count();
        let preview: String = text.lines().take(3).collect::<Vec<_>>().join("\n");
        let preview = if lines > 3 { format!("{preview}\n…") } else { preview };
        eprintln!("[ui] paste confirm lines={lines}");
        st.pending_paste = Some(text);
        st.ui.set_paste_lines(lines as i32);
        st.ui.set_paste_preview(preview.as_str().into());
        st.ui.set_paste_open(true);
        return;
    }
    deliver_paste(st, &text);
}

fn update_search_bar(st: &mut AppState) {
    st.ui.set_search_query(st.search_query.as_str().into());
}

// ------------------------------------------------------------ Command Palette

/// 按当前输入刷新结果列表（最多 10 条）。
/// 过滤当前窗口的 Tab（标题 + 各 pane 标题），更新最左侧 Tab 搜索列表。
fn refresh_tab_search(st: &mut AppState) {
    let q = st.tab_search_query.trim().to_lowercase();
    let mut items: Vec<slint::SharedString> = Vec::new();
    let mut indices: Vec<usize> = Vec::new();
    for (i, tab) in st.ws.tabs.iter().enumerate() {
        let mut hay = tab.title.to_lowercase();
        for pane in tab.panes() {
            if let Some(rt) = st.panes.get(&pane) {
                hay.push(' ');
                hay.push_str(&rt.title.to_lowercase());
            }
        }
        if q.is_empty() || hay.contains(&q) {
            let label = if tab.panes().len() > 1 {
                format!("{}  ·  {} panes", tab.title, tab.panes().len())
            } else {
                tab.title.clone()
            };
            items.push(label.into());
            indices.push(i);
        }
    }
    st.tab_search_indices = indices;
    st.tab_search_model.set_vec(items);
    st.ui.set_tabsearch_selected(0);
}

fn open_tab_search(st: &mut AppState) {
    st.tab_search_query.clear();
    st.ui.set_tabsearch_query("".into());
    st.ui.set_tabsearch_open(true);
    refresh_tab_search(st);
    eprintln!("[ui] tab search open items={}", st.tab_search_indices.len());
}

fn close_tab_search(st: &mut AppState) {
    st.ui.set_tabsearch_open(false);
    st.ui.set_tabsearch_query("".into());
    st.tab_search_query.clear();
    st.ui.invoke_focus_terminal();
}

fn tab_search_move(st: &mut AppState, delta: i32) {
    let n = st.tab_search_indices.len();
    if n == 0 {
        return;
    }
    let cur = st.ui.get_tabsearch_selected().max(0) as isize;
    let next = (((cur + delta as isize) % n as isize) + n as isize) % n as isize;
    st.ui.set_tabsearch_selected(next as i32);
}

fn tab_search_accept(st: &mut AppState) {
    let idx = st.ui.get_tabsearch_selected().max(0) as usize;
    tab_search_pick(st, idx);
}

fn tab_search_pick(st: &mut AppState, row: usize) {
    let target = st.tab_search_indices.get(row).copied();
    // 空结果时 Enter 不应把面板关掉后把键盘留在“已销毁的输入框”里。
    let Some(index) = target else {
        eprintln!("[ui] tab search pick row={row} (no match)");
        return;
    };
    st.ui.set_tabsearch_open(false);
    st.ui.set_tabsearch_query("".into());
    st.tab_search_query.clear();
    eprintln!("[ui] tab search pick={index}");
    set_active_tab_inner(st, index);
    st.ui.invoke_focus_terminal();
}

fn refresh_palette(st: &mut AppState) {
    let query = st.ui.get_palette_query().to_string();
    let hits: Vec<&commands::Command> = st.commands.search(&query).into_iter().take(10).collect();
    let items: Vec<slint::SharedString> = hits.iter().map(|c| c.title.into()).collect();
    st.palette_ids = hits.iter().map(|c| c.id).collect();
    st.palette_sel = 0;
    st.palette_model.set_vec(items);
    st.ui.set_palette_selected(0);
}

fn open_palette(st: &mut AppState) {
    st.ui.set_palette_query("".into());
    st.ui.set_palette_open(true);
    refresh_palette(st);
    eprintln!("[ui] palette open items={}", st.palette_ids.len());
}

fn palette_move(st: &mut AppState, delta: i32) {
    let n = st.palette_ids.len();
    if n == 0 {
        return;
    }
    let cur = st.ui.get_palette_selected().max(0) as isize;
    let next = (((cur + delta as isize) % n as isize) + n as isize) % n as isize;
    st.ui.set_palette_selected(next as i32);
    st.palette_sel = next as usize;
}

fn palette_accept(st: &mut AppState) {
    let idx = st.ui.get_palette_selected().max(0) as usize;
    let id = st.palette_ids.get(idx).copied();
    st.ui.set_palette_open(false);
    if let Some(id) = id {
        eprintln!("[ui] run command {id}");
        run_command(st, id);
    }
}

/// 执行一条命令。ID 与 `commands::builtin()` 一一对应。
fn run_command(st: &mut AppState, id: &str) {
    match id {
        "window.new" => new_window(),
        "tab.new" => {
            new_tab_with_default(st);
            rebuild_tabs(st);
        },
        "tab.close" => close_tab_inner(st, st.ws.active),
        "tab.next" => switch_tab_relative(st, 1),
        "tab.prev" => switch_tab_relative(st, -1),
        "tab.search" => open_tab_search(st),
        "tab.duplicate" => duplicate_tab(st, st.ws.active),
        "tab.reopen-closed" => reopen_closed_tab(st),
        "pane.split-right" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                split_pane_inner(st, focused, Dir::Row);
            }
        },
        "pane.split-down" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                split_pane_inner(st, focused, Dir::Col);
            }
        },
        "pane.close" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                close_pane_inner(st, focused);
            }
        },
        "pane.focus-next" => {
            let (panes, focused) = match st.ws.active_tab() {
                Some(t) => (t.panes(), t.focused),
                None => return,
            };
            if panes.len() > 1 {
                let cur = panes.iter().position(|p| *p == focused).unwrap_or(0);
                let next = panes[(cur + 1) % panes.len()];
                let idx = st.ws.active;
                st.ws.tabs[idx].focused = next;
                refresh_views(st);
            }
        },
        "terminal.copy" => copy_current_selection(st),
        "terminal.paste" => paste_into_focused(st),
        "terminal.find" => open_search(st),
        "terminal.select-all" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                if let Some(rt) = st.panes.get(&focused) {
                    rt.handle.select_all();
                }
            }
        },
        "terminal.clear" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                if let Some(rt) = st.panes.get(&focused) {
                    rt.handle.clear_screen();
                }
            }
        },
        "view.command-palette" => open_palette(st),
        "view.toggle-fullscreen" => toggle_fullscreen(st),
        "view.toggle-quake" => toggle_quake_inner(st),
        "view.zoom-in" => zoom_font(st, 1.0),
        "view.zoom-out" => zoom_font(st, -1.0),
        "view.zoom-reset" => reset_font(st),
        "session.restart" => restart_session(st),
        "session.reattach" => reattach_last_background(st),
        "settings.open" => st.ui.set_settings_open(true),
        other => {
            // 插件命令统一走 unknown 分支：spawn_command 只接受已启用插件声明的完整 ID，
            // 在后台线程执行，UI 线程立即返回。
            if st.plugins.spawn_command(other) {
                eprintln!("[plugin] 已提交后台执行 {other}");
            } else {
                eprintln!("[ui] 未知命令 {other}");
            }
        },
    }
}

fn open_search(st: &mut AppState) {
    st.search_open = true;
    st.ui.set_search_open(true);
    st.ui.set_search_status("".into());
    update_search_bar(st);
}

fn close_search(st: &mut AppState) {
    st.search_open = false;
    st.ui.set_search_open(false);
    st.ui.set_search_status("".into());
    let ids: Vec<PaneId> = st.panes.keys().copied().collect();
    for id in ids {
        if let Some(rt) = st.panes.get(&id) {
            rt.handle.search_clear();
        }
    }
    redraw_visible(st);
}

fn run_search(st: &mut AppState, forward: bool) {
    let query = st.search_query.clone();
    if query.is_empty() {
        return;
    }
    let Some(focused) = st.ws.active_tab().map(|t| t.focused) else { return };
    let hit = st.panes.get(&focused).and_then(|rt| rt.handle.search(&query, forward));
    st.ui.set_search_status(if hit.is_some() { "".into() } else { "no match".into() });
    redraw_visible(st);
}

// ------------------------------------------------------------------- 事件

/// Slint/winit 在 X11 下会把修饰键**本身**也当成 key-pressed 上报：
/// Ctrl→U+0011、Shift→U+0010、Alt→U+0012。这些不是用户输入，必须丢掉，
/// 否则终端会莫名收到 0x11/0x10（Ctrl+Q 的真实按键会另发一条 "q" 事件）。
fn is_modifier_artifact(text: &str, ctrl: bool, alt: bool, shift: bool) -> bool {
    match text {
        "\u{11}" => ctrl,
        "\u{12}" => alt,
        "\u{10}" => shift,
        _ => false,
    }
}

/// 统一的输入分发：物理按键与 IME 上屏都走这里。
/// `ctrl/alt/shift` 对 IME 提交恒为 false，避免把中文误判成快捷键。
fn handle_input(
    st: &mut AppState,
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    keylog: bool,
) {
    if is_modifier_artifact(text, ctrl, alt, shift) {
        if keylog {
            eprintln!("[key] 忽略修饰键伪事件 {text:?}");
        }
        return;
    }

    // 快捷键录制优先于一切分发：录制中按键只用于录制，不能触发原快捷键或写入 PTY。
    if st.hotkey_recording.is_some() {
        record_hotkey_key(st, text, ctrl, alt, shift);
        return;
    }

    // 用户 override 必须在硬编码默认键之前处理；命中即 return，避免同一按键触发两次。
    // 配置为空时完全跳过，保证无自定义快捷键时按键路径零变化。
    if !st.cfg.hotkeys.is_empty() {
        if let Some(chord) = keys::chord_from_key_event(text, ctrl, alt, shift) {
            if let Some(id) = st.commands.override_command_for_chord(&chord, &st.cfg.hotkeys) {
                if keylog {
                    eprintln!("[hotkey] override {id} ← {chord}");
                }
                run_command(st, id);
                return;
            }
            // 该默认键已被用户改到别的组合：吞掉，避免旧硬编码行为残留。
            if let Some(id) = st.commands.suppressed_default(&chord, &st.cfg.hotkeys) {
                if keylog {
                    eprintln!("[hotkey] suppressed old default {id} ← {chord}");
                }
                return;
            }
        }
    }

    // F11：全屏切换
    if text == "\u{F70E}" {
        toggle_fullscreen(st);
        return;
    }

    // F2：重命名当前 Tab（Slint 特殊键编码见 keys.rs）
    // 已经在 with_app 的借用内，不能再 with_app（同 F11 的崩溃根因）。
    if text == "\u{F705}" {
        open_rename(st, st.ws.active);
        return;
    }

    // Tab 导航在任何输入状态下都生效（含搜索框开着时）
    if ctrl && !alt {
        if text == "\t" {
            switch_tab_relative(st, if shift { -1 } else { 1 });
            return;
        }
        if !shift {
            if let Some(c) = text.chars().next() {
                if ('1'..='9').contains(&c) {
                    switch_tab_number(st, c as usize - '0' as usize);
                    return;
                }
            }
        }
    }

    // 搜索框打开时，键盘归搜索框（Esc 关闭、Enter 下一个、Shift+Enter 上一个）
    if st.search_open {
        match text {
            "\u{1b}" => {
                close_search(st);
                return;
            },
            "\n" | "\r" => {
                run_search(st, !shift);
                return;
            },
            "\u{8}" | "\u{7f}" => {
                st.search_query.pop();
                update_search_bar(st);
                if !st.search_query.is_empty() {
                    run_search(st, true);
                }
                return;
            },
            _ => {},
        }
        if !ctrl && !alt && !text.is_empty() {
            st.search_query.push_str(text);
            update_search_bar(st);
            run_search(st, true);
        }
        return;
    }

    if ctrl && shift {
        match text.to_ascii_lowercase().as_str() {
            "c" => {
                copy_current_selection(st);
                return;
            },
            "v" => {
                paste_into_focused(st);
                return;
            },
            "p" => {
                open_palette(st);
                return;
            },
            "a" => {
                open_tab_search(st);
                return;
            },
            // 部分 X11 文本映射会把 Ctrl+Shift+A 折成控制字符 ^A；仍按 Search Tabs 处理。
            "\u{1}" => {
                open_tab_search(st);
                return;
            },
            "n" => {
                new_window();
                return;
            },
            "e" => {
                if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                    split_pane_inner(st, focused, Dir::Row);
                }
                return;
            },
            "d" => {
                if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                    split_pane_inner(st, focused, Dir::Col);
                }
                return;
            },
            "w" => {
                if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                    close_pane_inner(st, focused);
                }
                return;
            },
            "t" => {
                reopen_closed_tab(st);
                return;
            },
            "\u{F700}" => {
                focus_pane_dir(st, 0.0, -1.0);
                return;
            },
            "\u{F701}" => {
                focus_pane_dir(st, 0.0, 1.0);
                return;
            },
            "\u{F702}" => {
                focus_pane_dir(st, -1.0, 0.0);
                return;
            },
            "\u{F703}" => {
                focus_pane_dir(st, 1.0, 0.0);
                return;
            },
            "+" => {
                zoom_font(st, 1.0);
                return;
            },
            _ => {},
        }
    }

    if ctrl {
        match text {
            "t" => {
                new_tab_with_default(st);
                rebuild_tabs(st);
                return;
            },
            "w" => {
                let idx = st.ws.active;
                close_tab_inner(st, idx);
                return;
            },
            "f" => {
                open_search(st);
                return;
            },
            "=" | "+" => {
                zoom_font(st, 1.0);
                return;
            },
            "-" => {
                zoom_font(st, -1.0);
                return;
            },
            "0" => {
                reset_font(st);
                return;
            },
            _ => {},
        }
    }

    // 插件声明的默认快捷键：放在内置硬编码分发**之后**，内置键（Ctrl+T 等）永远优先，
    // 插件只能使用内置未占用的组合；用户 override 已在更前面处理。
    if let Some(chord) = keys::chord_from_key_event(text, ctrl, alt, shift) {
        if let Some(id) = st.commands.plugin_default_for_chord(&chord) {
            if keylog {
                eprintln!("[plugin] 快捷键 {id} ← {chord}");
            }
            run_command(st, id);
            return;
        }
    }

    if PIPE_ON.load(Ordering::Relaxed) {
        *LAST_KEY.lock().unwrap() = Some(Instant::now());
    }
    let Some(tab) = st.ws.active_tab() else { return };
    let focused = tab.focused;
    let targets: Vec<PaneId> = if st.broadcast && tab.panes().len() > 1 {
        tab.panes()
    } else {
        vec![focused]
    };
    let encoded = keys::encode(text, ctrl, alt);
    if st.broadcast && targets.len() > 1 && keylog {
        eprintln!("[send] broadcast to {} panes", targets.len());
    }
    for target in targets {
    match st.panes.get_mut(&target) {
        Some(rt) => {
            if let Some(bytes) = &encoded {
                rt.handle.input(bytes);
                if keylog {
                    eprintln!("[send] pane={target} bytes={bytes:?}");
                }
            } else if keylog {
                eprintln!("[send] pane={target} 未编码 text={text:?}");
            }
        },
        None => {
            if keylog {
                eprintln!(
                    "[send] 找不到 pane {target}（现有 {:?}）",
                    st.panes.keys().collect::<Vec<_>>()
                );
            }
        },
    }
    }
}

/// 消费一个通知/Bell：非活动 Tab 置未读点并发桌面通知（若开启）。
/// 返回是否改变了 Tab 上的未读标记。
#[allow(clippy::too_many_arguments)]
fn apply_notification(
    rt: &mut PaneRuntime,
    note: Notification,
    id: PaneId,
    tab: Option<usize>,
    tab_title: &str,
    is_active: bool,
    desktop_notifications: bool,
    is_bell: bool,
) -> bool {
    let (title, body) = activity::notification_content(tab_title, &note);
    eprintln!(
        "[ui] notification pane={id} tab={tab:?} active={is_active} bell={is_bell} \
         title={title:?} body={body:?}"
    );
    rt.notification = Some(note);
    if is_active {
        // 当前正在看的 Tab：不产生未读点，也不弹桌面通知。
        return false;
    }
    rt.activity.attention = true;
    if desktop_notifications {
        send_desktop_notification(&title, &body);
    }
    true
}

/// 桌面通知：参数直传 notify-send，不经过 shell；未安装/无 DBus 时静默失败。
fn send_desktop_notification(title: &str, body: &str) {
    let title = title.to_owned();
    let body = body.to_owned();
    // 通知可能很频繁，绝不能在 UI 线程里等 notify-send 退出。
    let _ = std::thread::Builder::new().name("notify-send".into()).spawn(move || {
        let _ = std::process::Command::new("notify-send")
            .arg("--app-name=Tervo")
            .arg("--")
            .arg(&title)
            .arg(&body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

fn poll_events() {
    with_app(|st| {
        let active = st.ws.active;
        let desktop_notifications = st.cfg.desktop_notifications;
        let tab_titles: Vec<String> = st.ws.tabs.iter().map(|t| t.title.clone()).collect();
        let pane_tab: HashMap<PaneId, usize> = st
            .ws
            .tabs
            .iter()
            .enumerate()
            .flat_map(|(i, tab)| tab.panes().into_iter().map(move |id| (id, i)))
            .collect();

        let mut titles_dirty = false;
        let mut tabs_dirty = false;
        let ids: Vec<PaneId> = st.panes.keys().copied().collect();
        for id in ids {
            let Some(rt) = st.panes.get_mut(&id) else { continue };
            let mut events = Vec::new();
            rt.handle.drain_events(&mut events);
            let tab = pane_tab.get(&id).copied();
            let tab_title =
                tab.and_then(|i| tab_titles.get(i)).map(String::as_str).unwrap_or("Tervo");
            let is_active = tab == Some(active);
            for ev in events {
                match ev {
                    CoreEvent::Title(t) => {
                        if !t.is_empty() && rt.title != t {
                            rt.title = t;
                            titles_dirty = true;
                        }
                    },
                    CoreEvent::Progress(progress) => {
                        let had_progress = rt.activity.progress.state != ProgressState::Clear;
                        rt.activity.progress = progress;
                        // 进度从“有”变为清除 = 完成；活动 Tab 不放完成标记，避免打扰。
                        rt.activity.completed = progress.state == ProgressState::Clear
                            && had_progress
                            && !is_active;
                        tabs_dirty = true;
                        eprintln!(
                            "[ui] tab progress pane={id} tab={tab:?} state={:?} percent={:?}",
                            progress.state, progress.percent
                        );
                    },
                    CoreEvent::Notification(note) => {
                        tabs_dirty |= apply_notification(
                            rt,
                            note,
                            id,
                            tab,
                            tab_title,
                            is_active,
                            desktop_notifications,
                            false,
                        );
                    },
                    CoreEvent::Bell => {
                        tabs_dirty |= apply_notification(
                            rt,
                            activity::bell_notification(),
                            id,
                            tab,
                            tab_title,
                            is_active,
                            desktop_notifications,
                            true,
                        );
                    },
                    CoreEvent::Cwd(cwd) => {
                        eprintln!("[ui] cwd pane={id} tab={tab:?} path={cwd:?}");
                    },
                    CoreEvent::ResetTitle | CoreEvent::CursorBlinkChange | CoreEvent::Exited => {},
                }
            }
        }
        if titles_dirty {
            // 保持既有行为：只把活动 Tab 首个 pane 的标题写回 Tab。
            if let Some(tab) = st.ws.tabs.get_mut(active) {
                if let Some(first) = tab.panes().first() {
                    if let Some(rt) = st.panes.get(first) {
                        tab.title = rt.title.clone();
                    }
                }
            }
        }
        if titles_dirty || tabs_dirty {
            rebuild_tabs(st);
        }
        // 插件命令/事件在后台线程写活动日志；这里只按 revision 增量刷新设置页模型。
        refresh_plugin_activity(st, false);
        // SFTP 传输进度：400ms 一次非阻塞收事件；终态时回收任务线程。
        poll_sftp_transfer(st);
    });
}

// -------------------------------------------------------------------- 入口

#[derive(Default, Clone, Debug)]
struct Options {
    cols: u16,
    rows: u16,
    scrollback: usize,
    font_size: f32,
    shell: Option<String>,
    args: Vec<String>,
    latency: Option<usize>,
    fps: Option<f64>,
    title: String,
    exit_after_first: bool,
    dump: bool,
    frame_clock: String,
    demo_splits: bool,
    /// 启动时按快照恢复会话（由「Restart Session」菜单拉起）。
    restore: bool,
    /// 启动即全屏（F11 可切换）。
    fullscreen: bool,
    /// 由 `window.new` 拉起的子窗口进程；仅用于日志区分，不触发自动建窗。
    window: bool,
}

fn parse_args(cfg: &Config) -> Options {
    parse_args_from(cfg, std::env::args().skip(1))
}

/// 参数解析主体；与 `std::env::args()` 解耦，便于单测 `--window` 语义。
fn parse_args_from(cfg: &Config, args: impl IntoIterator<Item = String>) -> Options {
    let mut o = Options {
        cols: cfg.cols,
        rows: cfg.rows,
        scrollback: cfg.scrollback,
        font_size: cfg.font_size,
        shell: cfg.shell.clone(),
        title: TITLE.to_string(),
        frame_clock: "off".to_string(),
        ..Options::default()
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--cols" => o.cols = it.next().and_then(|v| v.parse().ok()).unwrap_or(o.cols),
            "--rows" => o.rows = it.next().and_then(|v| v.parse().ok()).unwrap_or(o.rows),
            "--scrollback" => o.scrollback = it.next().and_then(|v| v.parse().ok()).unwrap_or(o.scrollback),
            "--font-size" => o.font_size = it.next().and_then(|v| v.parse().ok()).unwrap_or(o.font_size),
            "--shell" => o.shell = it.next(),
            "--latency" | "-l" => o.latency = it.next().and_then(|v| v.parse().ok()),
            "--fps" => o.fps = it.next().and_then(|v| v.parse().ok()),
            "--title" => o.title = it.next().unwrap_or(o.title),
            "--exit-after-first" => o.exit_after_first = true,
            "--dump" => o.dump = true,
            "--frame-clock" => o.frame_clock = it.next().unwrap_or_else(|| "off".to_string()),
            "--demo-splits" => o.demo_splits = true,
            "--restore" => o.restore = true,
            "--fullscreen" => o.fullscreen = true,
            // 仅标记“本进程是新窗口子进程”，供日志区分；不触发任何自动建窗。
            window::WINDOW_FLAG => o.window = true,
            "--" => {
                o.args = it.by_ref().collect();
                break;
            },
            other => o.args.push(other.to_string()),
        }
    }
    o
}

/// `tervo --diagnose`：不启动 UI，打印环境/窗口/最近日志，便于定位“右键没反应”。
fn print_diagnostics() {
    println!("Tervo {}", env!("CARGO_PKG_VERSION"));
    println!("pid={}", std::process::id());
    for key in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "SLINT_BACKEND",
        "TERM",
        "TERVO_CONFIG",
        "TERVO_PROFILES",
    ] {
        println!("{key}={:?}", std::env::var(key).ok());
    }
    println!("config={}", Config::path().display());
    println!("profiles={}", profile::ProfileStore::path().display());
    let v = vault::Vault::open_default();
    println!("vault={} status={}", v.path().display(), v.status().label());
    match logging::path() {
        Some(p) => println!("log={}", p.display()),
        None => println!("log=(disabled)"),
    }
    if let Some((w, h)) = xconn::screen_size() {
        println!("screen={w}x{h}");
    }
    if let Some((x, y)) = xconn::pointer_position() {
        println!("pointer={x},{y}");
    }
    match xconn::active_window() {
        Some(id) => match xconn::window_snapshot(id) {
            Some(w) => println!(
                "active_window=0x{id:x} pid={} {}x{} at {},{} viewable={} title={:?}",
                w.pid, w.w, w.h, w.x, w.y, w.viewable, w.title
            ),
            None => println!("active_window=0x{id:x} (无法读取属性)"),
        },
        None => println!("active_window=(unknown)"),
    }
    println!("own windows:");
    for w in xconn::own_windows() {
        println!(
            "  0x{:x} pid={} {}x{} at {},{} viewable={} title={:?}",
            w.id, w.pid, w.w, w.h, w.x, w.y, w.viewable, w.title
        );
    }
    let others: Vec<_> =
        xconn::tervo_windows().into_iter().filter(|w| w.pid != std::process::id()).collect();
    if !others.is_empty() {
        println!("⚠ 其他 Tervo 实例窗口（可能挡住点击）:");
        for w in others {
            println!(
                "  0x{:x} pid={} {}x{} at {},{} viewable={} title={:?}",
                w.id, w.pid, w.w, w.h, w.x, w.y, w.viewable, w.title
            );
        }
    }
    println!("recent log:");
    for line in logging::tail(40) {
        println!("  {line}");
    }
}

fn print_help() {
    println!(
        "Tervo {}\n\n\
用法: tervo [选项] [-- <shell 参数>...]\n\n\
选项:\n  \
--cols N            初始列数 (默认 120)\n  \
--rows N            初始行数 (默认 32)\n  \
--scrollback N      历史缓冲行数 (默认 2000)\n  \
--font-size F       字号 (默认 15)\n  \
--shell PATH        指定 shell\n  \
--title TEXT        窗口标题\n  \
--frame-clock MODE  帧时钟实验项\n  \
-l, --latency N     按键延迟探针\n  \
--fps SECS          全屏帧率探针\n  \
--exit-after-first  首帧后退出\n  \
--dump              连续打印帧内容 (调试)\n  \
--restore           按上次快照恢复会话 (由 Restart 菜单调用)\n  \
--fullscreen        启动即全屏 (F11 切换)\n  \
--window            内部：以新窗口子进程启动 (仅日志区分，不自动再建窗口)\n  \
--import-tabby PATH 无 UI：导入 Tabby config.yaml（Profile/字号/快捷键/主题）后退出\n  \
--export-tabby PATH 无 UI：导出 Profile 为 Tabby YAML 后退出\n  \
--diagnose          打印环境/窗口/最近日志后退出（排查右键/焦点问题）\n  \
-h, --help          显示帮助\n\n\
快捷键: Ctrl+T 新建标签 / Ctrl+Shift+N 新建窗口 / Ctrl+W 关闭 / Ctrl+F 搜索 / Ctrl+Shift+C 复制 / Ctrl+Shift+V 粘贴\n\
配置: ~/.config/tervo/config.toml (TERVO_CONFIG 可覆盖)\n\
主题目录: ~/.config/tervo/themes (TERVO_THEMES_DIR 可覆盖；支持 iTerm2/WT/Alacritty/kitty/Xresources/YAML)",
        env!("CARGO_PKG_VERSION")
    );
}

/// 内部探针：不启动 UI，直接用第一个 SSH Profile 跑一次 SFTP 操作。
/// 用法：`tervo --sftp-probe <list|get|put> <remote> [local]`
fn run_sftp_probe(args: &[String]) -> i32 {
    let store = profile::ProfileStore::load();
    let Some(p) = store.profiles.iter().find(|p| p.kind == "ssh") else {
        eprintln!("[sftp-probe] 没有 SSH Profile");
        return 2;
    };
    let target = match sftp::SftpTarget::from_profile(p) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[sftp-probe] {e}");
            return 2;
        },
    };
    let op = args.first().map(String::as_str).unwrap_or("");
    let remote = args.get(1).map(String::as_str).unwrap_or(".");
    let result = match op {
        "list" => sftp::list(&target, remote).map(|items| items.join("\n")),
        "get" => {
            let local = args.get(2).map(String::as_str).unwrap_or("");
            sftp_probe_transfer(&target, sftp::TransferDirection::Download, local, remote)
        },
        "put" => {
            // put <local> <remote>（与 cp 习惯一致；get 则是 <remote> <local>）
            let local = args.get(1).map(String::as_str).unwrap_or("");
            let remote = args.get(2).map(String::as_str).unwrap_or("");
            sftp_probe_transfer(&target, sftp::TransferDirection::Upload, local, remote)
        },
        other => {
            eprintln!("[sftp-probe] 未知操作 {other}");
            return 2;
        },
    };
    match result {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            println!("[sftp-probe] ok {op}");
            0
        },
        Err(e) => {
            eprintln!("[sftp-probe] {e}");
            1
        },
    }
}

fn sftp_probe_transfer(
    target: &sftp::SftpTarget,
    direction: sftp::TransferDirection,
    local: &str,
    remote: &str,
) -> Result<String, String> {
    let request = sftp::TransferRequest {
        direction,
        target: target.clone(),
        local: local.to_string(),
        remote: remote.to_string(),
        recursive: direction == sftp::TransferDirection::Upload
            && std::path::Path::new(local).is_dir(),
    };
    sftp::TransferJob::start(&request)?.wait()?;
    Ok(String::new())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let t_start = Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv.iter().position(|a| a == "--sftp-probe") {
        std::process::exit(run_sftp_probe(&argv[pos + 1..]));
    }
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    logging::init();
    if argv.iter().any(|a| a == "--diagnose") {
        print_diagnostics();
        return Ok(());
    }

    // 无 UI 的 Tabby 配置迁移：在初始化窗口/后端之前处理，直接退出。
    let arg_value = |flag: &str| -> Option<std::path::PathBuf> {
        argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1)).map(std::path::PathBuf::from)
    };
    if let Some(path) = arg_value("--export-tabby") {
        let store = profile::ProfileStore::load();
        let yaml = tabby_import::export_yaml(&store);
        std::fs::write(&path, yaml)?;
        println!("[tabby] 导出 {} 个 Profile → {}", store.profiles.len(), path.display());
        return Ok(());
    }
    if let Some(path) = arg_value("--import-tabby") {
        let text = std::fs::read_to_string(&path)?;
        let mut store = profile::ProfileStore::load();
        let mut cfg = Config::load();
        let themes_dir = ThemeRegistry::default_dir();
        match tabby_import::import_yaml(&text, &mut store, &mut cfg, &themes_dir) {
            Ok(report) => {
                store.save();
                if let Some(fs) = report.font_size {
                    cfg.font_size = fs.clamp(8.0, 40.0);
                }
                if report.font_size.is_some() || report.hotkeys_added > 0 {
                    cfg.save();
                }
                println!(
                    "[tabby] 导入 {} 个 Profile（重命名 {}，跳过 {}）；\
                     快捷键 {} 条（未变 {}，未映射 {}，跳过 {}）；\
                     主题 {} 个（重命名 {}，跳过 {}，目录 {}）",
                    report.added,
                    report.renamed,
                    report.skipped.len(),
                    report.hotkeys_added,
                    report.hotkeys_unchanged,
                    report.hotkeys_unmapped.len(),
                    report.hotkeys_skipped.len(),
                    report.themes_added,
                    report.themes_renamed,
                    report.themes_skipped.len(),
                    themes_dir.display()
                );
                for (name, why) in &report.skipped {
                    eprintln!("  - 跳过 Profile {name}: {why}");
                }
                for (id, why) in &report.hotkeys_skipped {
                    eprintln!("  - 跳过快捷键 {id}: {why}");
                }
                for id in &report.hotkeys_unmapped {
                    eprintln!("  - 未映射快捷键 {id}");
                }
                if report.hotkeys_extra_ignored > 0 {
                    println!(
                        "  - 多候选键：另有 {} 条未采用（Tervo 每个命令只保留一条）",
                        report.hotkeys_extra_ignored
                    );
                }
                for (name, why) in &report.themes_skipped {
                    eprintln!("  - 跳过主题 {name}: {why}");
                }
                if let Some(fs) = report.font_size {
                    println!("  - 应用字号 {fs}px");
                }
                return Ok(());
            },
            Err(e) => {
                eprintln!("[tabby] 导入失败: {e}");
                std::process::exit(1);
            },
        }
    }

    env_logger::init();
    // 无 GPU（沙箱/无 /dev/dri）时 Skia 能建窗口但拿不到可呈现的 surface，
    // 表现为“窗口存在但全黑”。用户显式设置 SLINT_BACKEND 时不干预。
    if std::env::var_os("SLINT_BACKEND").is_none()
        && !std::path::Path::new("/dev/dri").exists()
    {
        std::env::set_var("SLINT_BACKEND", "winit-software");
        eprintln!("[render] 未检测到 /dev/dri，自动使用 winit-software 后端");
    }
    let mut cfg = Config::load();
    let opts = parse_args(&cfg);
    if opts.window {
        // 子窗口进程：仅日志区分。这里刻意不做任何“再建窗口”的动作，避免递归启动。
        eprintln!("[ui] new window process pid={} (--window)", std::process::id());
    }

    // 内置 + 用户自定义主题；加载失败只记录，不阻塞启动。
    let registry = ThemeRegistry::load(&ThemeRegistry::default_dir());
    for err in &registry.errors {
        eprintln!("[theme] {err}");
    }
    let theme_selection = normalize_selection(&registry, &cfg.theme);
    let initial_theme = resolve_theme(
        &registry,
        &theme_selection,
        cfg.auto_dark.as_deref(),
        cfg.auto_light.as_deref(),
    );

    let fonts = Rc::new(FontSet::load(opts.font_size).ok_or("找不到等宽字体")?);
    let cell = (fonts.cell_w, fonts.cell_h);
    let win_w = opts.cols as f32 * cell.0 as f32;
    let win_h = opts.rows as f32 * cell.1 as f32 + TABBAR_H;
    let content = Rect::new(0.0, 0.0, win_w, win_h - TABBAR_H);
    let t_font = t_start.elapsed();

    let ui = MainWindow::new()?;
    // --title 以前只解析不使用，窗口标题永远硬编码；现在透传到 Slint 属性。
    ui.set_window_title(opts.title.as_str().into());
    ui.window().set_size(slint::PhysicalSize::new(win_w as u32, win_h as u32));

    let pane_model = Rc::new(VecModel::<PaneView>::default());
    let tab_model = Rc::new(VecModel::<TabView>::default());
    let divider_model = Rc::new(VecModel::<DividerView>::default());
    ui.set_panes(ModelRc::from(pane_model.clone()));
    ui.set_tabs(ModelRc::from(tab_model.clone()));
    ui.set_dividers(ModelRc::from(divider_model.clone()));

    let echo = Arc::new(AtomicUsize::new(0));
    let palette_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_palette_items(ModelRc::from(palette_model.clone()));
    let tab_search_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_tabsearch_items(ModelRc::from(tab_search_model.clone()));
    let bg_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_background_items(ModelRc::from(bg_model.clone()));
    let profile_model = Rc::new(VecModel::<ProfileRow>::default());
    ui.set_profile_items(ModelRc::from(profile_model.clone()));
    let sftp_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_sftp_items(ModelRc::from(sftp_model.clone()));
    let hotkey_model = Rc::new(VecModel::<HotkeyRow>::default());
    ui.set_hotkey_rows(ModelRc::from(hotkey_model.clone()));
    let plugin_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_plugin_items(ModelRc::from(plugin_model.clone()));
    let plugin_activity_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_plugin_activity_items(ModelRc::from(plugin_activity_model.clone()));
    let settings_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_settings_results(ModelRc::from(settings_model.clone()));
    let fwd_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_fwd_items(ModelRc::from(fwd_model.clone()));
    let profile_menu_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_profilemenu_items(ModelRc::from(profile_menu_model.clone()));

    // 插件发现 + 命令注册：旧扁平清单只展示；v1 目录插件把启用命令合并进 Registry。
    let plugins = plugin::PluginStore::load();
    let mut command_registry = commands::builtin();
    let plugin_command_count = plugin::register_plugins(&mut command_registry, &plugins);
    eprintln!(
        "[plugin] 发现 {} 个插件（{} 个可执行包），注册 {} 条命令，跳过 {}",
        plugins.total_count(),
        plugins.packages.len(),
        plugin_command_count,
        plugins.skipped.len()
    );
    for (path, reason) in &plugins.skipped {
        eprintln!("[plugin] 跳过 {path}: {reason}");
    }

    // 圆角 worker 只依赖 X11；连不上/没有 SHAPE 时记录日志并保持直角，不阻塞 UI。
    // SHAPE 圆角与部分 WM 的 maximize/minimize/live-resize 组合会留下过期 mask
    //（露桌面、右上角按钮点不到）。默认关闭；需要时用 TERVO_CORNERS=1 显式开启。
    let corners = if std::env::var_os("TERVO_CORNERS").is_none()
        || std::env::var_os("TERVO_NO_CORNERS").is_some()
    {
        eprintln!("[corners] 默认关闭（TERVO_CORNERS=1 可开启 SHAPE 圆角）");
        None
    } else {
        match xconn::CornerShaper::spawn(&opts.title) {
            Ok(shaper) => Some(shaper),
            Err(e) => {
                eprintln!("[corners] {e}");
                None
            },
        }
    };

    let state = Rc::new(RefCell::new(AppState {
        ui: ui.clone_strong(),
        ws: Workspace::new(),
        panes: HashMap::new(),
        pane_model,
        tab_model,
        pane_rows: HashMap::new(),
        divider_model: divider_model.clone(),
        divider_infos: Vec::new(),
        fonts,
        cell,
        echo: echo.clone(),
        opts: opts.clone(),
        content,
        next_title: 0,
        resolved_theme: initial_theme.clone(),
        registry,
        auto_watch_started: false,
        search_open: false,
        search_query: String::new(),
        commands: command_registry,
        palette_ids: Vec::new(),
        palette_model,
        tab_search_indices: Vec::new(),
        tab_search_model,
        tab_search_query: String::new(),
        palette_sel: 0,
        drag: None,
        pending_paste: None,
        detached: Vec::new(),
        bg_model,
        profiles: profile::ProfileStore::load(),
        profile_model,
        profile_indices: Vec::new(),
        profile_query: String::new(),
        vault: vault::Vault::open_default(),
        vault_clear_armed: false,
        sftp_profiles: Vec::new(),
        sftp_model,
        hotkey_model,
        hotkey_ids: Vec::new(),
        hotkey_recording: None,
        plugins,
        plugin_model,
        plugin_activity_model,
        plugin_activity_revision: 0,
        sftp_delete_armed: false,
        sftp_transfer: None,
        sftp_transfer_state: sftp::TransferState::idle(),
        quake_visible: false,
        quake_saved: None,
        profile_menu_indices: Vec::new(),
        profile_menu_model,
        right_down: None,
        broadcast: false,
        last_click: None,
        closed_tabs: Vec::new(),
        fwd_profile_indices: Vec::new(),
        fwd_model,
        settings_result_cats: Vec::new(),
        settings_model,
        hover_cell: None,
        resize: None,
        wm_resize: false,
        wm_release_pending: false,
        xresize: None,
        resizing: false,
        resize_gen: 0,
        resize_events: 0,
        resize_applied: 0,
        resize_max_layout_us: 0,
        resize_pending: None,
        resize_apply_scheduled: false,
        resize_apply: slint::Timer::default(),
        corners,
        clipboard: arboard::Clipboard::new().ok(),
        cfg: {
            let mut c = cfg.clone();
            c.theme = theme_selection.clone();
            c.cols = opts.cols;
            c.rows = opts.rows;
            c.font_size = opts.font_size;
            c.scrollback = opts.scrollback;
            if opts.shell.is_some() {
                c.shell = opts.shell.clone();
            }
            c
        },
    }));
    APP.with(|c| *c.borrow_mut() = Some(state.clone()));

    // ---- 回调 ----
    let keylog = std::env::var("TERVO_KEYLOG").is_ok();
    // 鼠标链路排查：TERVO_MOUSELOG=1 打印「命中哪格 / 是否上报」。
    let mouselog = std::env::var("TERVO_MOUSELOG").is_ok();
    ui.on_key_input(move |text, ctrl, alt, shift| {
        let text = text.to_string();
        if keylog {
            eprintln!("[key] {:?} ctrl={ctrl} alt={alt} shift={shift}", text);
        }
        with_app(|st| handle_input(st, &text, ctrl, alt, shift, keylog));
    });

    // IME 上屏（中文/emoji）单独走一条路：没有修饰键语义，也不能触发 Ctrl 快捷键。
    ui.on_ime_commit(move |text| {
        let text = text.to_string();
        if keylog {
            eprintln!("[ime] {:?}", text);
        }
        with_app(|st| handle_input(st, &text, false, false, false, keylog));
    });

    ui.on_pane_mouse_down(move |id, x, y, button, mods| {
        let id = id as u64;
        with_app(|st| {
            let Some(rt) = st.panes.get(&id) else { return };
            let cell = cell_at(rt, x, y);
            let modifiers = mouse_mods(mods);
            // Shift 是「绕过鼠标上报、改成本地选词」的惯例逃生门（xterm/gnome-terminal 同款）。
            let reporting = rt.handle.wants_mouse() && !modifiers.shift;
            if mouselog {
                eprintln!(
                    "[mouse] down pane={id} cell={cell:?} button={button} mods={mods} \
                     wants={} reporting={reporting}",
                    rt.handle.wants_mouse()
                );
            }
            if reporting {
                // 两套状态不能并存：进终端鼠标模式就清掉本地选区。
                rt.handle.selection_clear();
                rt.handle.mouse(MouseEvent {
                    button: mouse_button(button),
                    action: MouseAction::Press,
                    line: cell.0,
                    col: cell.1,
                    modifiers,
                });
            } else {
                rt.handle.selection_start(cell.0, cell.1);
            }
            st.drag = Some(Drag { pane: id, start: (x, y), moved: false, cell, mouse: reporting });
        });
    });

    ui.on_pane_mouse_move(|id, x, y, _button, mods| {
        let id = id as u64;
        with_app(|st| {
            let modifiers = mouse_mods(mods);

            if st.drag.is_some() {
                let (cell, forwarding) = {
                    let Some(drag) = st.drag.as_mut() else { return };
                    if drag.pane != id {
                        return;
                    }
                    if (x - drag.start.0).abs() > 4.0 || (y - drag.start.1).abs() > 4.0 {
                        drag.moved = true;
                    }
                    let Some(rt) = st.panes.get(&id) else { return };
                    let cell = cell_at(rt, x, y);
                    if drag.cell == cell {
                        return;
                    }
                    drag.cell = cell;
                    (cell, drag.mouse)
                };
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
                    redraw_pane_inner(st, id);
                }
                return;
            }

            // 无按键悬停（1003）：只有声明 MOUSE_MOTION 的应用需要，按格去重防刷屏。
            let Some(rt) = st.panes.get(&id) else { return };
            if !rt.handle.wants_motion() {
                return;
            }
            let cell = cell_at(rt, x, y);
            if st.hover_cell == Some((id, cell)) {
                return;
            }
            st.hover_cell = Some((id, cell));
            rt.handle.mouse(MouseEvent {
                button: MouseButton::None,
                action: MouseAction::Motion,
                line: cell.0,
                col: cell.1,
                modifiers,
            });
        });
    });

    ui.on_pane_mouse_up(|id, x, y, button, mods| {
        let id = id as u64;
        with_app(|st| {
            let Some(drag) = st.drag.take() else { return };
            if drag.mouse {
                if let Some(rt) = st.panes.get(&id) {
                    let cell = cell_at(rt, x, y);
                    rt.handle.mouse(MouseEvent {
                        button: mouse_button(button),
                        action: MouseAction::Release,
                        line: cell.0,
                        col: cell.1,
                        modifiers: mouse_mods(mods),
                    });
                }
                return;
            }
            if !drag.moved {
                // 双击选行：同一 pane、同一格、400ms 内的第二次左键抬起。
                let Some(rt) = st.panes.get(&id) else { return };
                let (line_u, col_u) = cell_at(rt, x, y);
                let (line, col) = (line_u as i32, col_u as i32);
                let now = std::time::Instant::now();
                let is_double = matches!(st.last_click, Some((p, l, c, t))
                    if p == id && l == line && c == col && now.duration_since(t) < std::time::Duration::from_millis(400));
                if is_double {
                    st.last_click = None;
                    if let Some(rt) = st.panes.get_mut(&id) {
                        rt.handle.select_line(line);
                    }
                    eprintln!("[ui] double-click select line={line} pane={id}");
                    if st.cfg.copy_on_select {
                        copy_current_selection(st);
                    }
                    return;
                }
                st.last_click = Some((id, line, col, now));
                return;
            }
            // 关闭 Copy on select 时只保留高亮，不写剪贴板
            if !st.cfg.copy_on_select {
                return;
            }
            let text = st
                .panes
                .get(&drag.pane)
                .and_then(|rt| rt.handle.copy_selection())
                .and_then(normalize_clipboard_text);
            if let Some(text) = text {
                set_clipboard(st, &text);
            }
        });
    });

    ui.on_pane_scrolled(|id, dy, x, y, mods| {
        let id = id as u64;
        with_app(|st| {
            let Some(rt) = st.panes.get(&id) else { return };
            let modifiers = mouse_mods(mods);

            // 终端要鼠标：滚轮变成 64/65 上报（tmux 翻页、vim 滚动才生效）。
            if rt.handle.wants_mouse() && !modifiers.shift {
                let cell = cell_at(rt, x, y);
                let button =
                    if dy > 0.0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
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
            if lines == 0.0 {
                return;
            }
            // 约定：delta-y > 0 表示向上滚（回看历史）
            rt.handle.scroll(lines as i32);
        });
    });

    ui.on_divider_dragged(|index, x, y| {
        with_app(|st| {
            let Some(info) = st.divider_infos.get(index as usize).cloned() else { return };
            let usable = match info.dir {
                Dir::Row => info.parent.w - SPLIT_GAP,
                Dir::Col => info.parent.h - SPLIT_GAP,
            };
            if usable <= 1.0 {
                return;
            }
            let ratio = match info.dir {
                Dir::Row => (x - info.parent.x) / usable,
                Dir::Col => (y - info.parent.y) / usable,
            };
            let changed = {
                let Some(tab) = st.ws.active_tab_mut() else { return };
                let Some(root) = tab.root.as_mut() else { return };
                root.set_ratio(&info.path, ratio)
            };
            if changed {
                refresh_views(st);
                redraw_visible(st);
            }
        });
    });

    ui.on_start_window_resize(|dir| {
        with_app(|st| {
            ensure_resize_conn(st);
            st.wm_resize = false;
            st.wm_release_pending = false;
            // 先记录自绘起点。即使 WM 路径随后失败/没接管，drag 回调也能无缝回退，
            // 不会出现“按了边角却完全拖不动”。
            if let Some(pointer) = resize_pointer(st) {
                let pos = st.ui.window().position();
                let size = st.ui.window().size();
                st.resize = Some(WindowResize {
                    dir,
                    pointer,
                    pos: (pos.x, pos.y),
                    size: (size.width, size.height),
                    since: Instant::now(),
                });
            } else {
                st.resize = None;
            }
            if start_wm_live_resize(st, dir) {
                st.wm_resize = true;
                return;
            }
            if st.resize.is_none() {
                eprintln!("[ui] 自绘 resize 无法启动：X11 指针不可用");
            } else {
                eprintln!("[ui] 自绘 resize dir={dir}");
            }
        });
    });

    ui.on_window_resize_drag(|| {
        with_app(|st| {
            if st.wm_resize {
                let Some(r) = st.resize else { return };
                let now = st.ui.window().size();
                let wm_acted = (now.width, now.height) != r.size;
                // WM 可能仍在驱动 live resize，也可能只是比我们晚一帧。给它 150ms
                // 或一次真实尺寸变化；两者都没有才判定没接管，回退自绘保证能拖。
                if wm_acted || r.since.elapsed() < Duration::from_millis(150) {
                    return;
                }
                eprintln!("[ui] WM live resize 未接管指针，回退自绘 resize");
                st.wm_resize = false;
            }
            let Some(r) = st.resize else { return };
            let Some((px, py)) = resize_pointer(st) else { return };
            let dx = px - r.pointer.0;
            let dy = py - r.pointer.1;

            let left = matches!(r.dir, 0 | 6 | 7);
            let right = matches!(r.dir, 2 | 3 | 4);
            let top = matches!(r.dir, 0 | 1 | 2);
            let bottom = matches!(r.dir, 4 | 5 | 6);

            let mut x = r.pos.0;
            let mut y = r.pos.1;
            let mut w = r.size.0 as i32;
            let mut h = r.size.1 as i32;
            if left {
                x += dx;
                w -= dx;
            }
            if right {
                w += dx;
            }
            if top {
                y += dy;
                h -= dy;
            }
            if bottom {
                h += dy;
            }

            const MIN_W: i32 = 320;
            const MIN_H: i32 = 200;
            if w < MIN_W {
                if left {
                    x -= MIN_W - w;
                }
                w = MIN_W;
            }
            if h < MIN_H {
                if top {
                    y -= MIN_H - h;
                }
                h = MIN_H;
            }

            let want = slint::PhysicalSize::new(w as u32, h as u32);
            if st.ui.window().size() != want {
                st.ui.window().set_size(want);
            }
            if left || top {
                let want = slint::PhysicalPosition::new(x, y);
                if st.ui.window().position() != want {
                    st.ui.window().set_position(want);
                }
            }
        });
    });

    ui.on_end_window_resize(|| {
        with_app(|st| {
            st.wm_resize = false;
            st.wm_release_pending = false;
            st.resize = None;
        });
    });

    ui.on_input_trace(|kind, id, x, y| {
        with_app(|st| {
            eprintln!(
                "[trace] {kind} pane={id} at {x:.0},{y:.0} win_pos={:?} win_size={:?} content={:.0}x{:.0}",
                st.ui.window().position(),
                st.ui.window().size(),
                st.content.w,
                st.content.h
            );
        });
    });

    ui.on_pane_clicked(|id| focus_pane(id as u64));

    ui.on_menu_dismissed(|| {
        eprintln!("[ui] menu dismissed");
        // 右键菜单/遮罩会抢走 IME 焦点；关闭后必须还给终端，否则 TUI 无法输入。
        with_app(|st| st.ui.invoke_focus_terminal());
    });

    ui.on_pane_right_clicked(|id, x, y| {
        with_app(|st| {
            let id = id as u64;
            if let Some(idx) = st.ws.tab_of(id) {
                st.ws.tabs[idx].focused = id;
                refresh_views(st);
            }
            let mode = st.cfg.effective_right_click().to_string();
            st.right_down = Some((id, std::time::Instant::now(), x, y));
            eprintln!("[ui] right-down pane={id} mode={mode}");
            if mode == "menu" {
                open_pane_menu(st, id, x, y);
            } else if mode == "off" {
                // off = 完全不拦截：TUI 需要右键时把 press 转发给 PTY。
                if let Some(rt) = st.panes.get(&id) {
                    if rt.handle.wants_mouse() {
                        let cell = cell_at(rt, x, y);
                        rt.handle.mouse(MouseEvent {
                            button: MouseButton::Right,
                            action: MouseAction::Press,
                            line: cell.0,
                            col: cell.1,
                            modifiers: Modifiers::default(),
                        });
                    }
                }
            }
        });
    });

    // 右键抬起：paste/clipboard 用 250ms 区分短按（执行）与长按（弹菜单）。
    ui.on_pane_right_up(|id, _x, _y| {
        with_app(|st| {
            let id = id as u64;
            let Some((down_id, at, dx, dy)) = st.right_down.take() else { return };
            if down_id != id {
                return;
            }
            let mode = st.cfg.effective_right_click().to_string();
            let short = at.elapsed() < Duration::from_millis(250);
            match mode.as_str() {
                "paste" => {
                    if short {
                        paste_into_focused(st);
                    } else {
                        open_pane_menu(st, id, dx, dy);
                    }
                },
                "clipboard" => {
                    if short {
                        copy_or_paste(st);
                    } else {
                        open_pane_menu(st, id, dx, dy);
                    }
                },
                _ => {},
            }
            eprintln!("[ui] right-up pane={id} mode={mode} short={short}");
        });
    });

    ui.on_pane_middle_click(|_id| {
        with_app(|st| {
            if st.cfg.paste_on_middle_click {
                paste_into_focused(st);
                eprintln!("[ui] middle-click paste");
            }
        });
    });

    // 分隔线/边缘发起的右键：按内容区坐标找最近的 pane。
    ui.on_pane_right_clicked_at(|x, y| {
        with_app(|st| {
            let rects: Vec<(PaneId, Rect)> = {
                let Some(tab) = st.ws.active_tab() else { return };
                tab.panes()
                    .into_iter()
                    .filter_map(|id| st.panes.get(&id).map(|rt| (id, rt.rect)))
                    .collect()
            };
            if let Some(id) = nearest_pane(&rects, x, y) {
                open_pane_menu(st, id, x, y);
            }
        });
    });

    ui.on_menu_action(|id, action| {
        eprintln!("[ui] menu action pane={id} action={action}");
        let id = id as u64;
        match action {
            0 => split_pane(id, Dir::Row),
            1 => split_pane(id, Dir::Col),
            2 => close_pane(id),
            3 => {
                with_app(copy_current_selection);
            },
            4 => {
                with_app(paste_into_focused);
            },
            5 => {
                with_app(|st| run_command(st, "terminal.find"));
            },
            6 => {
                with_app(|st| run_command(st, "terminal.select-all"));
            },
            7 => {
                with_app(|st| run_command(st, "terminal.clear"));
            },
            8 => {
                with_app(|st| {
                    st.broadcast = !st.broadcast;
                    st.ui.set_broadcast(st.broadcast);
                    let panes = st.ws.active_tab().map(|t| t.panes().len()).unwrap_or(0);
                    if st.broadcast && panes < 2 {
                        eprintln!("[ui] broadcast=on，但当前 Tab 只有 1 个 pane");
                    } else {
                        eprintln!("[ui] broadcast={} panes={panes}", st.broadcast);
                    }
                });
            },
            _ => {},
        }
        with_app(|st| st.ui.invoke_focus_terminal());
    });

    ui.on_tab_rename(|index| {
        with_app(|st| open_rename(st, index.max(0) as usize));
    });

    ui.on_rename_committed(|index, text| {
        with_app(|st| {
            let idx = index.max(0) as usize;
            if let Some(title) = sanitize_tab_title(text.as_str()) {
                if let Some(tab) = st.ws.tabs.get_mut(idx) {
                    tab.title = title;
                    eprintln!("[ws] rename tab={idx} → {:?}", tab.title);
                }
            }
            st.ui.set_rename_open(false);
            rebuild_tabs(st);
        });
    });

    ui.on_rename_cancelled(|| {
        with_app(|st| st.ui.set_rename_open(false));
    });

    ui.on_fwd_select(|index| {
        with_app(|st| {
            st.ui.set_fwd_profile(index.max(0));
            refresh_forwards(st);
        });
    });

    ui.on_fwd_add(|| {
        with_app(|st| fwd_add(st));
    });

    ui.on_fwd_delete(|index| {
        with_app(|st| fwd_delete(st, index.max(0) as usize));
    });

    ui.on_fwd_close(|| {});

    ui.on_config_import_tabby(|| {
        with_app(|st| {
            let path = tabby_config_path();
            let themes_dir = ThemeRegistry::default_dir();
            let status = match std::fs::read_to_string(&path) {
                Ok(text) => match tabby_import::import_yaml(
                    &text,
                    &mut st.profiles,
                    &mut st.cfg,
                    &themes_dir,
                ) {
                    Ok(report) => {
                        st.profiles.save();
                        refresh_profiles(st);
                        if let Some(fs) = report.font_size {
                            st.cfg.font_size = fs.clamp(8.0, 40.0);
                            apply_font_size(st);
                        }
                        if report.font_size.is_some() || report.hotkeys_added > 0 {
                            st.cfg.save();
                        }
                        if report.hotkeys_added > 0 {
                            refresh_hotkeys(st);
                        }
                        if report.themes_added > 0 {
                            // 主题目录刚写入新文件：重新加载注册表并刷新设置页网格。
                            st.registry = ThemeRegistry::load(&themes_dir);
                            for err in &st.registry.errors {
                                eprintln!("[theme] {err}");
                            }
                            st.ui.set_theme_choices(theme_choices(&st.registry));
                        }
                        let mut msg = format!(
                            "已从 {} 导入 {} 个 Profile、{} 条快捷键、{} 个主题\
                             （Profile 跳过 {}，快捷键未映射 {} / 跳过 {}，主题跳过 {}）",
                            path.display(),
                            report.added,
                            report.hotkeys_added,
                            report.themes_added,
                            report.skipped.len(),
                            report.hotkeys_unmapped.len(),
                            report.hotkeys_skipped.len(),
                            report.themes_skipped.len()
                        );
                        let first_skip = report
                            .skipped
                            .first()
                            .map(|(name, why)| format!("{name}（{why}）"))
                            .or_else(|| {
                                report
                                    .hotkeys_skipped
                                    .first()
                                    .map(|(id, why)| format!("{id}（{why}）"))
                            })
                            .or_else(|| {
                                report
                                    .themes_skipped
                                    .first()
                                    .map(|(name, why)| format!("{name}（{why}）"))
                            });
                        if let Some(skip) = first_skip {
                            msg.push_str(&format!("；首个跳过：{skip}"));
                        }
                        eprintln!("[tabby] {msg}");
                        msg
                    },
                    Err(e) => format!("导入失败: {e}"),
                },
                Err(e) => format!("读不到 {}: {e}", path.display()),
            };
            st.ui.set_config_status(status.as_str().into());
        });
    });

    ui.on_config_export_tabby(|| {
        with_app(|st| {
            let path = Config::path().with_file_name("tabby-profiles.yaml");
            let yaml = tabby_import::export_yaml(&st.profiles);
            let status = match std::fs::write(&path, yaml) {
                Ok(()) => {
                    let msg = format!("已导出 {} 个 Profile → {}", st.profiles.profiles.len(), path.display());
                    eprintln!("[tabby] {msg}");
                    msg
                },
                Err(e) => format!("导出失败: {e}"),
            };
            st.ui.set_config_status(status.as_str().into());
        });
    });

    ui.on_sftp_list(|| {
        with_app(|st| sftp_do_list(st));
    });

    ui.on_sftp_pick(|index| {
        with_app(|st| {
            st.ui.set_sftp_selected(index.max(0));
            st.sftp_delete_armed = false;
        });
    });

    ui.on_sftp_download(|| {
        with_app(|st| sftp_do_download(st));
    });

    ui.on_sftp_upload(|| {
        with_app(|st| sftp_do_upload(st));
    });

    ui.on_sftp_cancel(|| {
        with_app(|st| sftp_cancel_transfer(st));
    });

    ui.on_sftp_mkdir(|| {
        with_app(|st| sftp_do_mkdir(st));
    });

    ui.on_sftp_rename(|| {
        with_app(|st| sftp_do_rename(st));
    });

    ui.on_sftp_delete(|| {
        with_app(|st| sftp_do_delete(st));
    });

    ui.on_sftp_close(|| {});

    ui.on_profile_open(|index| {
        with_app(|st| {
            if let Some(store_index) = profile_row_to_store(st, index) {
                open_profile_tab(st, store_index);
            }
        });
    });

    ui.on_profile_new(|| {
        with_app(|st| open_profile_form(st));
    });

    ui.on_profile_edit(|index| {
        with_app(|st| {
            if let Some(store_index) = profile_row_to_store(st, index) {
                edit_profile_form(st, store_index);
            }
        });
    });

    ui.on_profile_filter(|query| {
        with_app(|st| {
            st.profile_query = query.to_string();
            refresh_profiles(st);
            eprintln!("[profile] filter={:?} items={}", st.profile_query, st.profile_indices.len());
        });
    });

    // 星标：点已默认的项 = 取消默认（回退系统 shell）；点其他项 = 切换默认。
    ui.on_profile_set_default(|index| {
        with_app(|st| {
            let Some(store_index) = profile_row_to_store(st, index) else { return };
            let Some(id) = st.profiles.profiles.get(store_index).map(|p| p.id) else { return };
            st.cfg.default_profile_id = if st.cfg.default_profile_id == id { 0 } else { id };
            st.cfg.save();
            refresh_profiles(st);
            if st.cfg.default_profile_id == 0 {
                eprintln!("[profile] 已清除默认 Profile（新 Tab 用系统 shell）");
            } else {
                eprintln!("[profile] 默认 Profile id={id} name={:?}",
                    st.profiles.profiles.get(store_index).map(|p| p.name.as_str()).unwrap_or(""));
            }
        });
    });

    ui.on_profile_duplicate(|index| {
        with_app(|st| {
            let Some(store_index) = profile_row_to_store(st, index) else { return };
            let Some(source_id) = st.profiles.profiles.get(store_index).map(|p| p.id) else { return };
            match st.profiles.duplicate(source_id) {
                Ok(id) => {
                    st.profiles.save();
                    refresh_profiles(st);
                    eprintln!(
                        "[profile] 复制 id={source_id} → id={id} name={:?}（共 {}）",
                        st.profiles.get(id).map(|p| p.name.as_str()).unwrap_or(""),
                        st.profiles.profiles.len()
                    );
                },
                Err(e) => eprintln!("[profile] 复制 id={source_id} 失败: {e}"),
            }
        });
    });

    ui.on_profile_form_save(|| {
        with_app(|st| save_profile_form(st));
    });

    ui.on_profile_form_cancel(|| {
        with_app(|st| st.ui.set_form_error("".into()));
    });

    ui.on_profile_delete(|index| {
        with_app(|st| {
            let Some(store_index) = profile_row_to_store(st, index) else { return };
            let Some(p) = st.profiles.profiles.get(store_index).map(|p| p.id) else { return };
            if st.profiles.remove(p) {
                // 删除的是默认项时必须回退 0，否则新 Tab 会一直解析失败。
                let cleared = clear_default_after_remove(&mut st.cfg, p);
                st.profiles.save();
                if cleared {
                    st.cfg.save();
                    eprintln!("[profile] 默认 Profile id={p} 已删除，回退系统 shell");
                }
                refresh_profiles(st);
                eprintln!("[profile] 删除 id={p}（剩 {}）", st.profiles.profiles.len());
            }
        });
    });

    // 面板内控件（搜索框）拿过焦点；关闭后必须还给终端 IME 宿主，否则按键丢失。
    ui.on_profiles_close(|| {
        with_app(|st| {
            st.ui.invoke_focus_terminal();
            eprintln!("[profile] panel closed");
        });
    });

    ui.on_vault_set_master(|| {
        with_app(|st| vault_set_master_password(st));
    });

    ui.on_vault_unlock(|| {
        with_app(|st| vault_unlock(st));
    });

    ui.on_vault_lock(|| {
        with_app(|st| vault_lock(st));
    });

    ui.on_vault_clear(|| {
        with_app(|st| vault_clear(st));
    });

    ui.on_bg_reattach(|index| {
        with_app(|st| reattach_background(st, index.max(0) as usize));
    });

    ui.on_bg_close(|| {});

    ui.on_paste_accept(|| {
        with_app(|st| {
            st.ui.set_paste_open(false);
            if let Some(text) = st.pending_paste.take() {
                deliver_paste(st, &text);
            }
        });
    });

    ui.on_paste_always(|| {
        with_app(|st| {
            st.cfg.confirm_multiline_paste = false;
            st.ui.set_cfg_confirm_paste(false);
            st.cfg.save();
            st.ui.set_paste_open(false);
            if let Some(text) = st.pending_paste.take() {
                deliver_paste(st, &text);
            }
        });
    });

    ui.on_paste_cancel(|| {
        with_app(|st| {
            st.pending_paste = None;
            st.ui.set_paste_open(false);
        });
    });

    ui.on_set_right_click(|mode| {
        with_app(|st| {
            let mode = mode.to_string();
            if !["menu", "paste", "clipboard", "off"].contains(&mode.as_str()) {
                return;
            }
            st.cfg.right_click = mode.clone();
            // 旧字段统一折算到新字段，之后不再写回 true。
            st.cfg.right_click_paste = false;
            st.ui.set_cfg_right_click(mode.as_str().into());
            st.cfg.save();
            eprintln!("[ui] right_click={mode}");
        });
    });

    ui.on_toggle_paste_middle(|| {
        with_app(|st| {
            st.cfg.paste_on_middle_click = !st.cfg.paste_on_middle_click;
            st.ui.set_cfg_paste_middle(st.cfg.paste_on_middle_click);
            st.cfg.save();
            eprintln!("[ui] paste_on_middle_click={}", st.cfg.paste_on_middle_click);
        });
    });

    ui.on_toggle_confirm_paste(|| {
        with_app(|st| {
            st.cfg.confirm_multiline_paste = !st.cfg.confirm_multiline_paste;
            st.ui.set_cfg_confirm_paste(st.cfg.confirm_multiline_paste);
        st.ui.set_cfg_right_click(st.cfg.effective_right_click().into());
        st.ui.set_cfg_paste_middle(st.cfg.paste_on_middle_click);
            st.cfg.save();
            eprintln!("[ui] confirm_multiline_paste={}", st.cfg.confirm_multiline_paste);
        });
    });

    ui.on_toggle_desktop_notifications(|| {
        with_app(|st| {
            st.cfg.desktop_notifications = !st.cfg.desktop_notifications;
            st.ui.set_cfg_desktop_notifications(st.cfg.desktop_notifications);
            st.cfg.save();
            eprintln!("[ui] desktop_notifications={}", st.cfg.desktop_notifications);
        });
    });

    ui.on_settings_open_path(|| {
        with_app(|_st| {
            let path = Config::path();
            let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.clone());
            eprintln!("[ui] open config folder {}", dir.display());
            match std::process::Command::new("xdg-open").arg(&dir).spawn() {
                Ok(_) => {},
                Err(e) => eprintln!("[ui] 打开配置目录失败: {e}"),
            }
        });
    });

    ui.on_settings_category_changed(|c| {
        with_app(|st| {
            if c == 6 {
                refresh_vault_ui(st);
            } else {
                st.vault_clear_armed = false;
            }
        });
        eprintln!("[ui] settings category={c}");
        if c != 4 {
            // 离开 Hotkeys 页时结束录制，避免在别的页面上吞键盘。
            with_app(|st| {
                if st.hotkey_recording.is_some() {
                    cancel_hotkey_edit(st);
                }
            });
        }
    });

    ui.on_settings_filter(|query| {
        with_app(|st| {
            let q = query.to_string();
            st.ui.set_settings_query(q.as_str().into());
            let hits = settings_search(&q);
            st.settings_result_cats = hits.iter().map(|(c, _)| *c).collect();
            st.settings_model
                .set_vec(hits.into_iter().map(|(_, line)| line.into()).collect::<Vec<_>>());
            eprintln!("[ui] settings filter q={q:?} hits={}", st.settings_result_cats.len());
        });
    });

    ui.on_settings_open_result(|index| {
        with_app(|st| {
            if let Some(cat) = st.settings_result_cats.get(index.max(0) as usize) {
                st.ui.set_settings_category(*cat);
                eprintln!("[ui] settings goto cat={cat}");
            }
            st.ui.set_settings_query("".into());
            st.settings_model.set_vec(Vec::<slint::SharedString>::new());
        });
    });

    ui.on_settings_toggled(|open| {
        with_app(|st| {
            if open {
                refresh_vault_ui(st);
            } else {
                st.vault_clear_armed = false;
            }
        });
        eprintln!("[ui] settings open={open}");
        with_app(|st| {
            if !open {
                // 关闭设置时退出录制，并确保终端重新拿到键盘焦点。
                st.hotkey_recording = None;
                st.ui.set_hotkey_recording_row(-1);
                st.ui.set_hotkey_error("".into());
                st.ui.invoke_focus_terminal();
            }
        });
    });

    ui.on_hotkey_start_edit(|index| {
        with_app(|st| start_hotkey_edit(st, index));
    });

    ui.on_hotkey_cancel_edit(|| {
        with_app(|st| cancel_hotkey_edit(st));
    });

    ui.on_hotkey_reset(|index| {
        with_app(|st| reset_hotkey(st, index));
    });

    ui.on_hotkey_reset_all(|| {
        with_app(|st| reset_all_hotkeys(st));
    });

    ui.on_toggle_copy_on_select(|| {
        with_app(|st| {
            st.cfg.copy_on_select = !st.cfg.copy_on_select;
            st.ui.set_cfg_copy_on_select(st.cfg.copy_on_select);
            st.cfg.save();
            eprintln!("[ui] copy_on_select={}", st.cfg.copy_on_select);
        });
    });

    ui.on_toggle_confirm_close(|| {
        with_app(|st| {
            st.cfg.confirm_close = !st.cfg.confirm_close;
            st.ui.set_cfg_confirm_close(st.cfg.confirm_close);
            st.cfg.save();
            eprintln!("[ui] confirm_close={}", st.cfg.confirm_close);
        });
    });

    ui.on_tabsearch_open_requested(|| {
        let _ = with_app(open_tab_search);
    });
    ui.on_tabsearch_query_changed(|_q| {
        with_app(|st| {
            st.tab_search_query = st.ui.get_tabsearch_query().to_string();
            refresh_tab_search(st);
        });
    });
    ui.on_tabsearch_move(|delta| {
        let _ = with_app(|st| tab_search_move(st, delta));
    });
    ui.on_tabsearch_accept(|| {
        let _ = with_app(tab_search_accept);
    });
    ui.on_tabsearch_cancel(|| {
        let _ = with_app(close_tab_search);
    });
    ui.on_tabsearch_pick(|row| {
        let _ = with_app(|st| tab_search_pick(st, row.max(0) as usize));
    });

    ui.on_palette_query_changed(|_q| {
        with_app(|st| refresh_palette(st));
    });

    ui.on_palette_move(|delta| {
        with_app(|st| palette_move(st, delta));
    });

    ui.on_palette_accept(|| {
        with_app(|st| palette_accept(st));
    });

    ui.on_palette_cancel(|| {
        with_app(|st| st.ui.set_palette_open(false));
    });

    ui.on_palette_pick(|index| {
        with_app(|st| {
            st.ui.set_palette_selected(index.max(0));
            palette_accept(st);
        });
    });

    ui.on_tab_reorder(|from, drag_x, strip_w| {
        with_app(|st| {
            let n = st.ws.tabs.len();
            if n < 2 || from < 0 {
                return;
            }
            let from = from as usize;
            if from >= n {
                return;
            }
            let target = drag_target(drag_x, strip_w, n);
            if target != from && st.ws.move_tab(from, target) {
                eprintln!("[ws] reorder tab {from} → {target}");
                rebuild_tabs(st);
            }
        });
    });

    ui.on_tab_context(|index, x| {
        with_app(|st| {
            st.ui.set_tabctx_index(index);
            st.ui.set_tabctx_x(x);
            st.ui.set_tabctx_open(true);
        });
    });

    ui.on_tab_menu_action(|index, action| {
        eprintln!("[ui] tab action={action} index={index}");
        with_app(|st| {
            let index = index.max(0) as usize;
            match action {
                0 => close_tab_inner(st, index),
                1 => close_other_tabs(st, index),
                2 => duplicate_tab(st, index),
                3 => open_rename(st, index),
                4 => close_tabs_left(st, index),
                5 => close_tabs_right(st, index),
                _ => {},
            }
        });
    });

    ui.on_tabbar_context(|x, y| {
        eprintln!("[ui] tabbar menu at {x},{y}");
        with_app(|st| {
            st.ui.set_tabmenu_x(x);
            st.ui.set_tabmenu_y(y);
            st.ui.set_tabmenu_open(true);
        });
    });

    ui.on_tabbar_menu_action(|action| {
        eprintln!("[ui] tabbar action={action}");
        with_app(|st| match action {
            0 => {}, // 置顶由 Slint 侧直接切换（绑定 Window.always-on-top）
            1 => {
                new_tab_with_default(st);
                rebuild_tabs(st);
            },
            13 => new_window(),
            2 => {
                st.ui.set_settings_open(true);
            },
            3 => {
                let _ = slint::quit_event_loop();
            },
            4 => restart_session(st),
            5 => open_palette(st),
            7 => reattach_last_background(st),
            8 => open_background_panel(st),
            9 => open_profiles_panel(st),
            10 => open_sftp_panel(st),
            11 => open_forwards_panel(st),
            6 => {
                st.cfg.tabbar_position =
                    if st.cfg.tabbar_position == "bottom" { "top".into() } else { "bottom".into() };
                st.ui.set_tabbar_bottom(st.cfg.tabbar_position == "bottom");
                st.cfg.save();
                eprintln!("[ui] tabbar_position={}", st.cfg.tabbar_position);
            },
            _ => {},
        });
    });

    ui.on_close_window(|| {
        with_app(|st| request_close(st));
    });

    ui.on_confirm_accept(|| {
        with_app(|st| {
            st.ui.set_confirm_open(false);
            let _ = slint::quit_event_loop();
        });
    });

    ui.on_confirm_cancel(|| {
        eprintln!("[ui] close cancelled");
        with_app(|st| st.ui.set_confirm_open(false));
    });

    {
        let auto = is_auto_theme(&theme_selection);
        ui.set_theme_name(if auto { AUTO_THEME.into() } else { initial_theme.id.as_str().into() });
        ui.set_auto_resolved(if auto { initial_theme.label.as_str().into() } else { "".into() });
        ui.global::<Palette>().set_colors(slint_theme(&initial_theme.ui));
        ui.set_theme_choices(theme_choices(&state.borrow().registry));
    }
    ui.set_font_size(state.borrow().cfg.font_size as i32);
    ui.set_scrollback(state.borrow().cfg.scrollback as i32);

    ui.on_set_theme(|name| {
        eprintln!("[theme] 选择 -> {name}");
        with_app(|st| apply_theme(st, name.as_str()));
    });

    // Appearance：背景图路径（空 = 关闭）。合法路径输入过程中即时加载/写盘；
    // 非法路径只在回车/失焦提交时显示错误，并保留上一张已生效背景（回退）。
    ui.on_background_image_changed(|path| {
        with_app(|st| {
            commit_background_path(st, path.as_str(), false);
        });
    });
    ui.on_background_image_commit(|path| {
        with_app(|st| {
            commit_background_path(st, path.as_str(), true);
        });
    });

    // 拖动时只预览（重绘终端帧），松手才写盘，避免每一帧都原子写 config.toml。
    // 无背景图时不改有效 alpha（见 renderer_opacity），所以不会把纯色终端拖灰。
    ui.on_background_opacity_preview(|value| {
        with_app(|st| {
            let value = config::clamp_opacity(value);
            st.cfg.background_opacity = value;
            if st.ui.get_has_background_image() {
                sync_renderer_opacity(st);
            }
        });
    });

    ui.on_background_opacity_changed(|value| {
        with_app(|st| {
            let value = config::clamp_opacity(value);
            st.cfg.background_opacity = value;
            st.ui.set_background_opacity(value);
            if st.ui.get_has_background_image() {
                sync_renderer_opacity(st);
            }
            st.cfg.save();
            eprintln!("[theme] background_opacity={value:.2}（仅终端默认背景）");
        });
    });

    // 遮罩层只影响背景图，由 Slint 直接改属性完成即时预览；松手才落盘。
    ui.on_background_dim_preview(|value| {
        with_app(|st| {
            st.cfg.background_dim = config::clamp_dim(value);
        });
    });

    ui.on_background_dim_changed(|value| {
        with_app(|st| {
            let value = config::clamp_dim(value);
            st.cfg.background_dim = value;
            st.ui.set_background_dim(value);
            st.cfg.save();
            eprintln!("[theme] background_dim={value:.2}");
        });
    });

    ui.on_bump_font_size(|delta| {
        with_app(|st| {
            let new = (st.cfg.font_size + delta as f32).clamp(8.0, 32.0);
            if (new - st.cfg.font_size).abs() < 0.1 {
                return;
            }
            st.cfg.font_size = new;
            st.ui.set_font_size(new as i32);
            st.cfg.save();
            apply_font_size(st);
        });
    });

    ui.on_bump_scrollback(|delta| {
        with_app(|st| {
            let new = (st.cfg.scrollback as i64 + delta as i64).clamp(500, 100_000) as usize;
            st.cfg.scrollback = new;
            st.ui.set_scrollback(new as i32);
            st.cfg.save();
        });
    });

    {
        let weak = ui.as_weak();
        ui.on_minimize_window(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_toggle_maximize(move || {
            if let Some(ui) = weak.upgrade() {
                // 内置 `maximized` 属性与窗口状态双向同步，直接翻转即可
                let target = !ui.window().is_maximized();
                ui.window().set_maximized(target);
                // 先按目标状态更新圆角，避免 WM 尚未写回 _NET_WM_STATE 时四角留缝。
                with_app(|st| {
                    request_window_corners(
                        st,
                        None,
                        Some(xconn::CornerState {
                            maximized: target,
                            fullscreen: st.ui.window().is_fullscreen(),
                        }),
                    );
                });
            }
        });
    }

    ui.on_tab_clicked(|i| set_active_tab(i as usize));
    ui.on_tab_closed(|i| close_tab(i as usize));
    ui.on_new_tab(new_tab);

    ui.on_profilemenu_menu(|| {
        with_app(open_profile_menu);
    });

    ui.on_profilemenu_pick(|index| {
        with_app(|st| {
            st.ui.set_profilemenu_open(false);
            let index = index.max(0) as usize;
            eprintln!("[ui] profile pick={index}");
            if let Some(&profile_index) = st.profile_menu_indices.get(index) {
                open_profile_tab(st, profile_index);
            }
        });
    });

    ui.on_profilemenu_manage(|| {
        with_app(|st| {
            st.ui.set_profilemenu_open(false);
            open_profiles_panel(st);
        });
    });

    // Quake 全局热键：X11 被动 grab；Wayland 下会返回错误并只打日志。
    if let Err(e) = quake::spawn_listener(&cfg.quake_hotkey, || {
        let _ = slint::invoke_from_event_loop(toggle_quake);
    }) {
        eprintln!("[quake] {e}");
    }
    ui.set_cfg_quake(cfg.quake_enabled);

    ui.on_toggle_quake(|| {
        with_app(|st| {
            st.cfg.quake_enabled = !st.cfg.quake_enabled;
            st.ui.set_cfg_quake(st.cfg.quake_enabled);
            if !st.cfg.quake_enabled && st.quake_visible {
                if let Some((pos, size)) = st.quake_saved.take() {
                    st.ui.window().set_size(size);
                    st.ui.window().set_position(pos);
                }
                st.quake_visible = false;
            }
            st.cfg.save();
            eprintln!("[ui] quake_enabled={}", st.cfg.quake_enabled);
        });
    });

    ui.on_resized(|w, h| {
        with_app(|st| {
            // WM live resize 期间指针被 WM grab，winit/Slint 收不到真实 ButtonRelease；
            // TouchArea 会一直保持 pressed/grabbed，后续点击会继续命中旧方向。
            // 收到第一帧由 WM 驱动的 resize 后，异步补发一次 PointerReleased 清理状态。
            if st.wm_resize && !st.wm_release_pending {
                st.wm_release_pending = true;
                let weak = st.ui.as_weak();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let still_wm =
                        matches!(with_app(|st| st.wm_resize && st.wm_release_pending), Some(true));
                    if still_wm {
                        ui.window().dispatch_event(
                            slint::platform::WindowEvent::PointerReleased {
                                position: slint::LogicalPosition::new(0.0, 0.0),
                                button: slint::platform::PointerEventButton::Left,
                            },
                        );
                    }
                });
            }
            // w/h 是 Slint 逻辑像素；worker 按 scale_factor 换算物理像素并节流去重。
            request_window_corners(st, Some((w, h)), None);
            let w = w.max(1.0);
            let h = (h - TABBAR_H).max(1.0);
            if (st.content.w - w).abs() < 0.5 && (st.content.h - h).abs() < 0.5 {
                return;
            }
            st.resize_events = st.resize_events.saturating_add(1);
            st.resize_pending = Some((w, h));
            // 高速拖动时 configure 可能比帧率还密；16ms 只处理最新尺寸，但每帧都
            // 真实 resize terminal grid + PTY + 重绘，字符保持 1:1，不再拉伸。
            if !st.resize_apply_scheduled {
                st.resize_apply_scheduled = true;
                st.resize_apply.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(16),
                    move || {
                        with_app(|st| {
                            st.resize_apply_scheduled = false;
                            let Some((w, h)) = st.resize_pending.take() else { return };
                            st.content = Rect::new(0.0, 0.0, w, h);
                            let t0 = Instant::now();
                            refresh_views(st);
                            redraw_visible(st);
                            if std::env::var_os("TERVO_RESIZE_PROFILE").is_some() {
                                eprintln!(
                                    "[ui] resize apply events={} cost={:.1}ms content={:.0}x{:.0}",
                                    st.resize_events,
                                    t0.elapsed().as_secs_f64() * 1000.0,
                                    st.content.w,
                                    st.content.h
                                );
                            }
                            st.resize_events = 0;
                        });
                    },
                );
            }
        });
    });

    // ---- OSC 标题轮询 ----
    let poll_timer = slint::Timer::default();
    poll_timer.start(slint::TimerMode::Repeated, Duration::from_millis(400), poll_events);

    // 设置开关的初始状态
    with_app(|st| {
        st.ui.set_cfg_copy_on_select(st.cfg.copy_on_select);
        st.ui.set_cfg_confirm_close(st.cfg.confirm_close);
        st.ui.set_cfg_confirm_paste(st.cfg.confirm_multiline_paste);
        st.ui.set_cfg_desktop_notifications(st.cfg.desktop_notifications);
        st.ui.set_tabbar_bottom(st.cfg.tabbar_position == "bottom");
        st.ui.set_settings_path(Config::path().display().to_string().as_str().into());
        init_background(st);
        refresh_hotkeys(st);
        refresh_vault_ui(st);
        refresh_plugin_page(st);
        eprintln!(
            "[plugin] 设置页 {} 个插件，{} 个启用，{} 条命令，跳过 {}",
            st.plugins.total_count(),
            st.plugins.enabled_count(),
            st.plugins.enabled_command_count(),
            st.plugins.skipped.len()
        );
        eprintln!("[ui] 启动 tabbar_position={}", st.cfg.tabbar_position);
    });

    // 启动事件：应用状态就绪后、首个 Tab 创建前后台分发；失败不影响主流程。
    with_app(|st| {
        dispatch_plugin_event(st, plugin::PluginEvent::Startup, plugin::startup_payload());
    });

    // ---- 首个标签页 / 会话恢复 ----
    let restored = with_app(|st| {
        let restored = opts.restore && restore_session(st);
        if !restored {
            new_tab_inner(st);
        }
        rebuild_tabs(st);
        restored
    })
    .unwrap_or(false);
    if restored {
        let (tabs, panes) = with_app(|st| {
            (st.ws.tabs.len(), st.ws.tabs.iter().map(|t| t.panes().len()).sum::<usize>())
        })
        .unwrap_or((0, 0));
        eprintln!("[session] 已按快照恢复：{tabs} 个标签 / {panes} 个 pane");
    }

    // ---- 可选：帧时钟 spike ----
    if opts.frame_clock != "off" {
        let interval = match opts.frame_clock.as_str() {
            "timer0" => Duration::from_micros(200),
            "timer1000" => Duration::from_micros(1000),
            "timer2000" => Duration::from_micros(2000),
            _ => Duration::from_micros(1000),
        };
        let weak = ui.as_weak();
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, interval, move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().request_redraw();
            }
        });
        std::mem::forget(timer);
    }

    // ---- 演示布局（调试用）----
    if opts.demo_splits && !restored {
        with_app(|st| {
            if let Some(tab) = st.ws.active_tab() {
                let first = tab.focused;
                split_pane_inner(st, first, Dir::Row);
                let second = st.ws.active_tab().map(|t| t.focused).unwrap_or(first);
                split_pane_inner(st, second, Dir::Col);
            }
        });
    }

    // ---- 首帧 ----
    ui.show()?;
    // 启动/窗口 map 后先应用一次圆角；worker 会按 _NET_WM_PID 等本进程窗口出现。
    with_app(|st| request_window_corners(st, None, None));
    {
        let wins = xconn::own_windows();
        if let Some(w) = wins.iter().find(|w| w.viewable && w.w > 100 && w.h > 100) {
            eprintln!(
                "[ui] main window 0x{:x} at {},{} {}x{} (all own windows: {})",
                w.id,
                w.x,
                w.y,
                w.w,
                w.h,
                wins.len()
            );
        }
        // 旧实例可能全屏盖住新窗口：日志里明确写出来，右键“没反应”时可直接判断。
        for other in xconn::tervo_windows()
            .into_iter()
            .filter(|w| w.pid != std::process::id() && w.viewable)
        {
            eprintln!(
                "[ui] 警告：另一个可见的 Tervo 窗口 0x{:x}（pid={}）{}x{} at {},{}，可能挡住点击",
                other.id, other.pid, other.w, other.h, other.x, other.y
            );
        }
    }
    if opts.fullscreen {
        ui.window().set_fullscreen(true);
        // 启动即全屏：直接按方角状态重算，避免首帧先画圆角再等 WM 属性更新。
        with_app(|st| {
            request_window_corners(
                st,
                None,
                Some(xconn::CornerState { maximized: false, fullscreen: true }),
            );
        });
        eprintln!("[ui] fullscreen=true (--fullscreen)");
    }

    // Auto 主题：窗口显示后开始监听系统深浅色变化。
    if is_auto_theme(&theme_selection) {
        with_app(ensure_auto_watcher);
    }

    // 窗口图标兜底：Slint 内置 icon 在本环境没有写进 _NET_WM_ICON
    {
        const ICON_RGBA: &[u8] = include_bytes!("../../../assets/icon-64.rgba");
        let title_for_icon = opts.title.clone();
        std::thread::Builder::new().name("icon".into()).spawn(move || {
            // 等 Slint 完成首轮 window properties，避免被它的空 icon 覆盖
            std::thread::sleep(std::time::Duration::from_millis(800));
            match xconn::set_window_icon(ICON_RGBA, 64, &title_for_icon) {
                Ok(win) => eprintln!("[ui] _NET_WM_ICON 已设置 (win=0x{win:x})"),
                Err(e) => eprintln!("[ui] 设置窗口图标失败: {e}"),
            }
        })?;
    }

    redraw_all();
    let t_show = t_start.elapsed();

    if opts.exit_after_first {
        std::thread::Builder::new().name("first-frame".into()).spawn(move || {
            let Some(watcher) = DamageWatcher::start(&opts.title, Duration::from_secs(30)) else {
                println!("FIRST_DAMAGE unavailable");
                let _ = slint::quit_event_loop();
                return;
            };
            // 监听建立后再强制重绘一帧：否则首帧可能早于 damage 选择，测到的是下一次
            // damage（如光标/后续输出），first_damage 会被高估。
            redraw_all();
            match watcher.recv_timeout(Duration::from_secs(30)) {
                Some(_) => println!("FIRST_DAMAGE_US {}", t_start.elapsed().as_micros()),
                None => println!("FIRST_DAMAGE timeout"),
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let _ = slint::quit_event_loop();
        })?;
    }

    {
        use std::io::Write;
        println!(
            "READY TIMING font_us={} show_us={} total_us={} cell={}x{}",
            t_font.as_micros(),
            t_show.as_micros(),
            t_start.elapsed().as_micros(),
            cell.0,
            cell.1,
        );
        let _ = std::io::stdout().flush();
    }

    if opts.dump {
        std::thread::Builder::new().name("dump".into()).spawn(|| {
            for i in 0..40 {
                std::thread::sleep(Duration::from_millis(250));
                // thread_local 的 APP 只在 UI 线程可见，必须切回事件循环
                let _ = slint::invoke_from_event_loop(move || {
                    with_app(|st| {
                        let Some(focused) = st.ws.active_tab().map(|t| t.focused) else { return };
                        let Some(rt) = st.panes.get(&focused) else { return };
                        let frame = &rt.frame;
                        println!(
                            "DUMP#{i} grid={}x{} offset={} cursor=({},{}) pid={:?} bytes={} pulls={}",
                            frame.cols, frame.rows, frame.display_offset, frame.cursor.line,
                            frame.cursor.col, rt.handle.pid(),
                            rt.handle.stats.bytes_parsed.load(Ordering::Relaxed),
                            rt.handle.stats.pulls.load(Ordering::Relaxed)
                        );
                        for row in 0..frame.rows.min(4) {
                            let line: String = frame.row(row).iter().map(|c| c.char()).collect();
                            println!("  [{row:02}] {:?}", line.trim_end());
                        }
                    });
                });
            }
            std::thread::sleep(Duration::from_millis(300));
            let _ = slint::quit_event_loop();
        })?;
    }

    if let Some(n) = opts.latency {
        let echo = echo.clone();
        std::thread::Builder::new()
            .name("latency-probe".into())
            .spawn(move || run_latency_probe(n, echo, TITLE.to_string()))?;
    }
    if let Some(secs) = opts.fps {
        std::thread::Builder::new()
            .name("fps-probe".into())
            .spawn(move || run_fps_probe(secs, TITLE.to_string()))?;
    }

    let _keepalive = (poll_timer, state);
    let run_result = slint::run_event_loop();
    stop_auto_watcher();
    run_result?;
    Ok(())
}

// ------------------------------------------------------------------- 探针

fn run_latency_probe(n: usize, echo: Arc<AtomicUsize>, title: String) {
    const WARMUP: usize = 20;

    let Some(mut inj) = Injector::connect(&title, Duration::from_secs(15)) else {
        println!("{{\"mode\":\"latency\",\"error\":\"x11_inject_unavailable\"}}");
        let _ = slint::quit_event_loop();
        return;
    };
    let Some(watcher) = DamageWatcher::start(&title, Duration::from_secs(15)) else {
        println!("{{\"mode\":\"latency\",\"error\":\"xdamage_unavailable\"}}");
        let _ = slint::quit_event_loop();
        return;
    };

    PIPE_ON.store(true, Ordering::Relaxed);
    inj.focus();
    std::thread::sleep(Duration::from_millis(400));
    watcher.drain();

    let mut samples = Vec::with_capacity(n);
    let mut timeouts = 0usize;

    for i in 0..n {
        inj.ctrl_u();
        wait_echo(&watcher, &echo, 0, Duration::from_millis(500));
        watcher.drain();

        let Some(t0) = inj.tap_char(MARKER) else { break };
        match wait_echo(&watcher, &echo, 1, Duration::from_millis(1500)) {
            Some(t) => {
                if i >= WARMUP {
                    samples.push(t.duration_since(t0));
                }
            },
            None => timeouts += 1,
        }
        std::thread::sleep(Duration::from_millis(6));
    }

    PIPE_ON.store(false, Ordering::Relaxed);
    let stats = LatencyStats::from_samples(&samples);
    let pipe: Vec<Duration> = PIPE_SAMPLES.lock().unwrap().drain(..).collect();
    let pstats = LatencyStats::from_samples(&pipe);
    println!(
        "{}",
        stats.to_json(&format!(
            ",\"mode\":\"latency\",\"timeouts\":{timeouts},\"pipe_median_us\":{:.1},\"pipe_p99_us\":{:.1},\"pipe_n\":{}",
            pstats.median_us, pstats.p99_us, pstats.n
        ))
    );
    let _ = slint::quit_event_loop();
}

fn wait_echo(watcher: &DamageWatcher, echo: &AtomicUsize, target: usize, timeout: Duration) -> Option<Instant> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let t = watcher.recv_timeout(left)?;
        if echo.load(Ordering::Acquire) >= target {
            return Some(t);
        }
    }
}

fn run_fps_probe(secs: f64, title: String) {
    let Some(watcher) = DamageWatcher::start(&title, Duration::from_secs(15)) else {
        println!("{{\"mode\":\"fps\",\"error\":\"xdamage_unavailable\"}}");
        let _ = slint::quit_event_loop();
        return;
    };
    std::thread::sleep(Duration::from_secs(2));
    watcher.drain();

    let start = Instant::now();
    let mut stamps: Vec<Instant> = Vec::new();
    while start.elapsed() < Duration::from_secs_f64(secs) {
        if let Some(t) = watcher.recv_timeout(Duration::from_millis(500)) {
            stamps.push(t);
        }
    }
    let intervals: Vec<Duration> = stamps.windows(2).map(|w| w[1] - w[0]).collect();
    let stats = LatencyStats::from_samples(&intervals);
    let fps = if stats.mean_us > 0.0 { 1e6 / stats.mean_us } else { 0.0 };
    println!(
        "{{\"mode\":\"fps\",\"frames\":{},\"fps\":{:.1},\"interval_median_ms\":{:.2},\"interval_p99_ms\":{:.2},\"interval_max_ms\":{:.2}}}",
        stamps.len().saturating_sub(1),
        fps,
        stats.median_us / 1000.0,
        stats.p99_us / 1000.0,
        stats.max_us / 1000.0
    );
    let _ = slint::quit_event_loop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_scheme_has_priority() {
        // color-scheme 有明确答案时，忽略 gtk-theme 与 COLORFGBG。
        assert!(scheme_from_outputs("'prefer-dark'", "'Yaru'", Some("0;15")));
        assert!(!scheme_from_outputs("'prefer-light'", "'Yaru-dark'", Some("15;0")));
        assert!(!scheme_from_outputs("'default'", "'Yaru-dark'", Some("15;0")));
    }

    #[test]
    fn gtk_theme_is_second_fallback() {
        assert!(scheme_from_outputs("", "'Yaru-dark'", Some("0;15")));
        assert!(!scheme_from_outputs("", "'Yaru-light'", Some("15;0")));
        assert!(!scheme_from_outputs("", "'Adwaita'", Some("0;15")));
    }

    #[test]
    fn colorfgbg_is_last_fallback() {
        assert!(scheme_from_outputs("", "", Some("15;0")));
        assert!(scheme_from_outputs("", "", Some("15;6")));
        assert!(scheme_from_outputs("", "", Some("15;8")));
        assert!(!scheme_from_outputs("", "", Some("0;7")));
        assert!(!scheme_from_outputs("", "", Some("0;15")));
        // 只看最后一段
        assert!(scheme_from_outputs("", "", Some("0;15;2")));
    }

    #[test]
    fn malformed_outputs_default_to_dark() {
        assert!(scheme_from_outputs("", "", None));
        assert!(scheme_from_outputs("", "", Some("bogus")));
        assert!(scheme_from_outputs("garbage", "Adwaita", None));
    }

    #[test]
    fn auto_defaults_to_yaru() {
        let reg = ThemeRegistry::from_parts(
            Theme::ALL.iter().copied().map(crate::themes::builtin).collect(),
            Vec::new(),
        );
        assert_eq!(resolve_auto(&reg, Variant::Dark, None, None).id, "yaru-dark");
        assert_eq!(resolve_auto(&reg, Variant::Light, None, None).id, "yaru-light");
        assert!(is_auto_theme("auto"));
        assert!(is_auto_theme(" AUTO "));
        assert!(!is_auto_theme("tokyo-night"));
    }

    #[test]
    fn modifier_only_events_are_filtered() {
        // Slint 的修饰键伪事件
        assert!(is_modifier_artifact("\u{11}", true, false, false));
        assert!(is_modifier_artifact("\u{10}", false, false, true));
        assert!(is_modifier_artifact("\u{12}", false, true, false));
        assert!(is_modifier_artifact("\u{11}", true, true, true));
        // 真实按键不能被误杀
        assert!(!is_modifier_artifact("q", true, false, false));
        assert!(!is_modifier_artifact("\u{10}", true, false, false)); // Ctrl+P
        assert!(!is_modifier_artifact("\u{1b}", false, false, false)); // Esc
        assert!(!is_modifier_artifact("\n", true, false, false)); // Ctrl+Enter
        assert!(!is_modifier_artifact("你", false, false, false));
    }

    #[test]
    fn selection_normalization_trims_trailing_blank_lines() {
        assert_eq!(normalize_clipboard_text("a\nb\n\n\n".into()), Some("a\nb".to_string()));
        assert_eq!(normalize_clipboard_text("a\nb  ".into()), Some("a\nb  ".to_string()));
        assert_eq!(normalize_clipboard_text("\n\n\n".into()), None);
        assert_eq!(normalize_clipboard_text(String::new()), None);
        // 中间空行要保留
        assert_eq!(normalize_clipboard_text("a\n\nb".into()), Some("a\n\nb".to_string()));
    }

    #[test]
    fn settings_search_matches_labels_and_keywords() {
        let hits = settings_search("font");
        assert!(hits.iter().any(|(cat, line)| *cat == 1 && line.contains("Font size")), "{hits:?}");
        assert!(settings_search("copy").iter().any(|(_, l)| l.contains("Copy on select")));
        assert!(settings_search("  theme  ").iter().any(|(cat, _)| *cat == 0));
        assert!(settings_search("zzzz").is_empty());
        assert!(settings_search("").is_empty());
        // 结果按目录顺序返回
        let cats: Vec<i32> = settings_search("e").iter().map(|(c, _)| *c).collect();
        assert!(cats.windows(2).all(|w| w[0] <= w[1]), "{cats:?}");
    }

    #[test]
    fn hotkey_page_lists_bindings_and_conflicts() {
        let reg = commands::builtin();
        let (summary, rows, ids) = hotkey_rows(&reg, &[]);
        assert!(summary.contains("26 commands"), "{summary}");
        assert_eq!(rows.len(), reg.all().len());
        assert_eq!(ids.len(), rows.len());
        assert!(rows.iter().any(|r| r.title == "New Tab" && r.chord == "Ctrl+T" && !r.custom));
        assert!(rows.iter().any(|r| r.title == "Duplicate Tab" && r.chord == "(none)"));
        assert!(rows.iter().all(|r| !r.conflict));

        // 自定义标记：override 后 chord 生效并标 custom
        let mut overrides = vec![commands::HotkeyOverride {
            command: "tab.new".into(),
            chord: "ctrl+alt+n".into(),
        }];
        let (summary, rows, _) = hotkey_rows(&reg, &overrides);
        assert!(summary.contains("1 custom"), "{summary}");
        let row = rows.iter().find(|r| r.title == "New Tab").unwrap();
        assert_eq!(row.chord, "Ctrl+Alt+N");
        assert!(row.custom);

        // 手工写入的冲突 override 会被计入行标记与冲突数
        overrides.push(commands::HotkeyOverride {
            command: "tab.close".into(),
            chord: "ctrl+alt+n".into(),
        });
        let (summary, rows, _) = hotkey_rows(&reg, &overrides);
        assert!(summary.contains("2 custom"), "{summary}");
        assert_eq!(reg.effective_conflicts(&overrides).len(), 1);
        assert_eq!(rows.iter().filter(|r| r.conflict).count(), 2);
    }

    #[test]
    fn remote_join_avoids_double_slashes() {
        assert_eq!(join_remote("/", "a"), "/a");
        assert_eq!(join_remote("/tmp", "a"), "/tmp/a");
        assert_eq!(join_remote("/tmp/", "a"), "/tmp/a");
    }

    #[test]
    fn paste_normalization_and_brackets() {
        assert_eq!(normalize_paste("a\r\nb\nc"), "a\rb\rc");
        assert_eq!(normalize_paste("plain"), "plain");
        assert_eq!(paste_payload("x\ry", false), b"x\ry".to_vec());
        let wrapped = paste_payload("x\ry", true);
        assert_eq!(&wrapped[..6], b"\x1b[200~");
        assert!(wrapped.ends_with(b"\x1b[201~"));
        assert_eq!(wrapped.len(), "x\ry".len() + 12);
    }

    #[test]
    fn paste_confirm_only_for_multiline_when_enabled() {
        assert!(!needs_paste_confirm("one line", true));
        assert!(needs_paste_confirm("a\nb", true));
        assert!(!needs_paste_confirm("a\nb", false));
        // 单独 \r 不算多行（单行回车粘贴不烦人）
        assert!(!needs_paste_confirm("a\rb", true));
    }

    #[test]
    fn drag_target_maps_pointer_to_slot() {
        // 3 个 Tab：strip 宽 = padding 32 + 3*140 + 2*4
        let strip = 32.0 + 3.0 * 140.0 + 2.0 * 4.0;
        assert_eq!(drag_target(32.0, strip, 3), 0);
        assert_eq!(drag_target(32.0 + 150.0, strip, 3), 1);
        assert_eq!(drag_target(32.0 + 300.0, strip, 3), 2);
        // 超出两端要夹住
        assert_eq!(drag_target(-100.0, strip, 3), 0);
        assert_eq!(drag_target(5000.0, strip, 3), 2);
        // 单 Tab 永远 0
        assert_eq!(drag_target(100.0, 100.0, 1), 0);
    }

    #[test]
    fn close_confirm_policy() {
        // 单标签单 pane：直接关，不打扰
        assert!(!needs_close_confirm(1, 1, true));
        // 多标签或多 pane：确认
        assert!(needs_close_confirm(2, 2, true));
        assert!(needs_close_confirm(1, 3, true));
        // 用户在配置里关掉确认
        assert!(!needs_close_confirm(5, 9, false));
    }

    #[test]
    fn tab_title_sanitizing() {
        assert_eq!(sanitize_tab_title("  work  "), Some("work".to_string()));
        assert_eq!(sanitize_tab_title("我的终端"), Some("我的终端".to_string()));
        assert_eq!(sanitize_tab_title(""), None);
        assert_eq!(sanitize_tab_title("   \t "), None);
        assert_eq!(sanitize_tab_title(&"x".repeat(121)), None);
        assert_eq!(sanitize_tab_title(&"x".repeat(120)), Some("x".repeat(120)));
    }

    #[test]
    fn tab_navigation_wraps() {
        assert_eq!(next_tab_index(0, 3, 1), 1);
        assert_eq!(next_tab_index(2, 3, 1), 0);
        assert_eq!(next_tab_index(0, 3, -1), 2);
        assert_eq!(next_tab_index(1, 3, -1), 0);
        assert_eq!(next_tab_index(0, 1, 1), 0);
        assert_eq!(next_tab_index(0, 9, -3), 6);
    }

    #[test]
    fn nearest_pane_handles_gaps_and_edges() {
        let rects = vec![
            (1u64, Rect::new(0.0, 0.0, 100.0, 100.0)),
            (2u64, Rect::new(104.0, 0.0, 100.0, 100.0)),
        ];
        assert_eq!(nearest_pane(&rects, 50.0, 50.0), Some(1));
        assert_eq!(nearest_pane(&rects, 150.0, 50.0), Some(2));
        // 分隔线 4px 缝隙：按距离归到较近的一侧
        assert_eq!(nearest_pane(&rects, 101.5, 50.0), Some(1));
        assert_eq!(nearest_pane(&rects, 103.0, 50.0), Some(2));
        // 窗口边缘/内容区之外也必须有归属
        assert_eq!(nearest_pane(&rects, -20.0, 50.0), Some(1));
        assert_eq!(nearest_pane(&rects, 500.0, 500.0), Some(2));
        assert_eq!(nearest_pane(&[], 0.0, 0.0), None);
    }

    #[test]
    fn mouse_modifier_bits_decode() {
        // Slint 位：1=Shift 2=Alt 4=Ctrl
        assert_eq!(mouse_mods(0), Modifiers::default());
        assert!(mouse_mods(1).shift);
        assert!(mouse_mods(2).alt);
        assert!(mouse_mods(4).ctrl);
        let all = mouse_mods(7);
        assert!(all.shift && all.alt && all.ctrl);
    }

    #[test]
    fn mouse_button_codes_map() {
        assert_eq!(mouse_button(0), MouseButton::Left);
        assert_eq!(mouse_button(1), MouseButton::Middle);
        assert_eq!(mouse_button(2), MouseButton::Right);
        // 未知编码按左键兜底，绝不丢事件
        assert_eq!(mouse_button(99), MouseButton::Left);
    }

    #[test]
    fn winit_resize_direction_matches_slint_resize_handles() {
        use slint::winit_030::winit::window::ResizeDirection;
        // 0=左上 1=上 2=右上 3=右 4=右下 5=下 6=左下 7=左，与 EWMH 完全同序。
        let expected = [
            ResizeDirection::NorthWest,
            ResizeDirection::North,
            ResizeDirection::NorthEast,
            ResizeDirection::East,
            ResizeDirection::SouthEast,
            ResizeDirection::South,
            ResizeDirection::SouthWest,
            ResizeDirection::West,
        ];
        for (dir, want) in expected.into_iter().enumerate() {
            assert_eq!(winit_resize_direction(dir as i32), Some(want), "dir={dir}");
        }
        for invalid in [-2, -1, 8, 9, 100, i32::MIN, i32::MAX] {
            assert_eq!(winit_resize_direction(invalid), None, "dir={invalid}");
        }
    }

    #[test]
    fn closing_last_pane_must_not_update_rows_in_place() {
        let mut rows: HashMap<PaneId, usize> = HashMap::new();
        rows.insert(1, 0);
        rows.insert(2, 1);
        // close_pane 会先把被关的 pane 从 pane_rows 删掉，而模型此刻仍是 2 行。
        rows.remove(&2);
        // 只比较 pane_rows.len() 会误判成「顺序没变」，于是 2 号 pane 的视图留在模型里，
        // 右侧/下方出现一块不再更新的「幽灵分屏」，剩下的 pane 也没有真正铺满。
        assert!(!rows_can_update_in_place(2, &rows, &[1]));

        // 真正干净的状态才允许原地更新
        assert!(rows_can_update_in_place(1, &rows, &[1]));
        // 顺序变化（例如关掉左侧，右侧顶到第 0 行）同样必须重建
        rows.insert(3, 0);
        rows.insert(4, 1);
        assert!(!rows_can_update_in_place(2, &rows, &[4, 3]));
        assert!(rows_can_update_in_place(2, &rows, &[3, 4]));
    }

    #[test]
    fn deleting_default_profile_falls_back_to_shell() {
        let mut cfg = Config { default_profile_id: 7, ..Config::default() };
        // 删别的 Profile：默认保持不变
        assert!(!clear_default_after_remove(&mut cfg, 8));
        assert_eq!(cfg.default_profile_id, 7);
        // 删默认 Profile：回退 0（+ / Ctrl+T 走系统 shell）
        assert!(clear_default_after_remove(&mut cfg, 7));
        assert_eq!(cfg.default_profile_id, 0);
        // id=0 不可能是“默认 Profile”，不能误触发写盘
        assert!(!clear_default_after_remove(&mut cfg, 0));
    }

    #[test]
    fn window_flag_marks_child_process_without_parent_args() {
        let cfg = Config::default();
        let opts = parse_args_from(&cfg, ["--window".to_string()]);
        assert!(opts.window);
        // 新窗口进程不能继承父进程的一次性参数（否则 --restore 会在子进程里再恢复一次布局）
        assert!(opts.args.is_empty());
        assert!(!opts.restore);
        assert!(!opts.fullscreen);
        assert_eq!(opts.shell, cfg.shell);
        // 正常启动不带标记
        assert!(!parse_args_from(&cfg, Vec::new()).window);
        // 与 spawn 侧常量一致
        assert_eq!(window::WINDOW_FLAG, "--window");
    }
}
