mod commands;
mod config;
mod glyph;
mod keys;
mod latency;
mod profile;
mod render;
mod session;
mod sftp;
mod themes;
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

use rusttabby_core::{
    spawn, CoreEvent, Frame, Modifiers, MouseAction, MouseButton, MouseEvent, PaneConfig,
    PaneHandle, Theme, UiColors,
};
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
const TITLE: &str = "Rust Tabby";
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
    palette_sel: usize,
    drag: Option<Drag>,
    /// 等待用户确认的多行粘贴内容
    pending_paste: Option<String>,
    /// 后台会话：关 Tab/Pane 时按配置保留的运行时（不 drop，子进程继续跑）
    detached: Vec<(PaneId, PaneRuntime, String)>,
    bg_model: Rc<VecModel<slint::SharedString>>,
    profiles: profile::ProfileStore,
    profile_model: Rc<VecModel<slint::SharedString>>,
    /// SFTP 面板里可选的 SSH Profile 索引（打开面板时快照）
    sftp_profiles: Vec<usize>,
    sftp_model: Rc<VecModel<slint::SharedString>>,
    /// 1003 悬停上报的上一格（按格去重，避免鼠标移动刷爆有界 ctrl 队列）。
    hover_cell: Option<(PaneId, (u16, u16))>,
    resize: Option<WindowResize>,
    xresize: Option<ResizeCtx>,
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
    let mut st = app.borrow_mut();
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

fn theme_choices(reg: &ThemeRegistry) -> ModelRc<ThemeChoice> {
    let choices: Vec<ThemeChoice> = reg
        .all()
        .map(|t| ThemeChoice { id: t.id.as_str().into(), label: t.label.as_str().into() })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(choices)))
}

/// 设置面板高度：12 套内置主题 3 行；每多一行加 30px。
fn settings_height_for(count: usize, win_h: f32) -> f32 {
    // 主题区最多显示 4 行，更多靠 Flickable 滚动，面板高度不再继续增长。
    let rows = count.div_ceil(4).clamp(1, 4);
    // +92：设置面板新增两个开关行（Copy on select / Confirm close）+ 底部提示不被挤出面板
    let h = 210.0 + rows as f32 * 30.0 + 122.0;
    h.min((win_h - 40.0).max(280.0))
}

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
) -> std::io::Result<PaneRuntime> {
    // Profile 优先：它显式带 shell/args/env；否则沿用全局配置。
    let shell = profile
        .map(|p| p.shell.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| st.cfg.shell.clone());
    let args = profile.map(|p| p.args.clone()).unwrap_or_else(|| st.opts.args.clone());
    let env = profile.map(|p| p.env.clone()).unwrap_or_default();
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
    let mut renderer =
        Renderer::new(st.fonts.clone(), cols, rows, theme.spec.bg, theme.spec.cursor);
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
    })
}

fn new_tab_inner(st: &mut AppState) {
    let id = st.ws.alloc_id();
    let (cols, rows) = cell_of(st.content, st.cell);
    match create_pane(st, id, cols, rows, None, None) {
        Ok(rt) => {
            st.next_title += 1;
            let n = st.next_title;
            st.panes.insert(id, rt);
            st.ws.tabs.push(Tab::single(id, format!("Terminal {n}")));
            eprintln!("[ws] new tab: tabs={}", st.ws.tabs.len());
            st.ws.active = st.ws.tabs.len() - 1;
            refresh_views(st);
            rebuild_tabs(st);
            redraw_pane_inner(st, id);
        },
        Err(e) => eprintln!("创建 pane 失败: {e}"),
    }
}

fn new_tab() {
    with_app(new_tab_inner);
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

fn refresh_profiles(st: &mut AppState) {
    let items: Vec<slint::SharedString> = st
        .profiles
        .profiles
        .iter()
        .map(|p| format!("[{}] {}", p.kind, p.name).into())
        .collect();
    st.profile_model.set_vec(items);
}

fn open_profiles_panel(st: &mut AppState) {
    refresh_profiles(st);
    st.ui.set_profiles_open(true);
    eprintln!("[profile] panel items={}", st.profiles.profiles.len());
}

/// 用 Profile 打开新 Tab（shell/args/env/cwd 全按 Profile）。
fn open_profile_tab(st: &mut AppState, index: usize) {
    let Some(p) = st.profiles.profiles.get(index).cloned() else { return };
    // kind → 实际启动命令：local 直接用 Profile；ssh 用系统 ssh 客户端（PTY 里跑）；
    // serial/telnet 后端还没做，明确拒绝而不是假装能连。
    let spec = match p.kind.as_str() {
        "local" => p.clone(),
        "ssh" => {
            let (prog, args) = p.ssh_command();
            let mut q = p.clone();
            q.shell = prog;
            q.args = args;
            q.cwd = None;
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
    match create_pane(st, id, cols, rows, None, Some(&spec)) {
        Ok(rt) => {
            st.panes.insert(id, rt);
            st.ws.tabs.push(Tab::single(id, p.name.clone()));
            st.ws.active = st.ws.tabs.len() - 1;
            st.ui.set_profiles_open(false);
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
    let result = if edit_id > 0 {
        st.profiles.update(edit_id as u64, p).map(|_| edit_id as u64)
    } else {
        st.profiles.add(p)
    };
    match result {
        Ok(id) => {
            st.profiles.save();
            refresh_profiles(st);
            st.ui.set_profile_form_open(false);
            st.ui.set_form_error("".into());
            eprintln!("[profile] 保存 id={id} kind={kind}（共 {}）", st.profiles.profiles.len());
        },
        Err(e) => {
            eprintln!("[profile] 表单校验失败: {e}");
            st.ui.set_form_error(e.as_str().into());
        },
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
    st.ui.set_sftp_target(0);
    st.ui.set_sftp_selected(-1);
    st.ui.set_sftp_status(
        if st.sftp_profiles.is_empty() {
            "没有 SSH Profile，先到 Profiles 里建一个"
        } else {
            "选择 Profile，点 List"
        }
        .into(),
    );
    st.ui.set_sftp_open(true);
    eprintln!("[sftp] 打开面板 profiles={}", st.sftp_profiles.len());
}

fn sftp_current_target(st: &AppState) -> Option<sftp::SftpTarget> {
    let i = st.ui.get_sftp_target().max(0) as usize;
    let idx = *st.sftp_profiles.get(i)?;
    sftp::SftpTarget::from_profile(&st.profiles.profiles[idx]).ok()
}

fn sftp_set_status(st: &mut AppState, msg: &str) {
    st.ui.set_sftp_status(msg.into());
    eprintln!("[sftp] {msg}");
}

fn sftp_do_list(st: &mut AppState) {
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    let remote = st.ui.get_sftp_remote().to_string();
    sftp_set_status(st, "列目录中…");
    match sftp::list(&target, &remote) {
        Ok(items) => {
            let n = items.len();
            let names: Vec<slint::SharedString> = items.into_iter().map(|s| s.into()).collect();
            st.sftp_model.set_vec(names);
            st.ui.set_sftp_selected(-1);
            sftp_set_status(st, &format!("{n} 个条目"));
        },
        Err(e) => sftp_set_status(st, &format!("列目录失败: {e}")),
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
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    let sel = st.ui.get_sftp_selected();
    let Some(item) = st.sftp_model.row_data(sel.max(0) as usize) else {
        sftp_set_status(st, "先在列表里选一条");
        return;
    };
    let remote = join_remote(&st.ui.get_sftp_remote(), &item);
    let local = st.ui.get_sftp_local().to_string();
    match sftp::download(&target, &remote, &local) {
        Ok(()) => sftp_set_status(st, &format!("已下载 {remote} → {local}")),
        Err(e) => sftp_set_status(st, &format!("下载失败: {e}")),
    }
}

fn sftp_do_upload(st: &mut AppState) {
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    let local = st.ui.get_sftp_local().to_string();
    let name = std::path::Path::new(&local)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload.bin".to_string());
    let remote = join_remote(&st.ui.get_sftp_remote(), &name);
    match sftp::upload(&target, &local, &remote) {
        Ok(()) => {
            sftp_set_status(st, &format!("已上传 {local} → {remote}"));
            sftp_do_list(st);
        },
        Err(e) => sftp_set_status(st, &format!("上传失败: {e}")),
    }
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
    let background = st.cfg.close_action == "background";
    for id in st.ws.tabs[index].panes() {
        if background {
            detach_runtime(st, id);
        } else {
            st.panes.remove(&id); // Drop => 关闭 PTY / 结束 actor
        }
    }
    st.ws.tabs.remove(index);
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

/// 只保留指定 Tab，其它 Tab 的 PTY 全部关掉。
fn close_other_tabs(st: &mut AppState, keep: usize) {
    if keep >= st.ws.tabs.len() {
        return;
    }
    let dropped = st.ws.keep_only(keep);
    eprintln!("[ws] close other tabs, keep={keep}, 关闭 {} 个 pane", dropped.len());
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
        match create_pane(st, new_ids[i], cols, rows, cwd.as_ref().map(PathBuf::from), None) {
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
    st.ws.tabs.insert(
        index + 1,
        Tab {
            title: format!("{title} (copy)"),
            root: Some(new_root),
            focused: focused_new,
            grid: HashMap::new(),
        },
    );
    st.ws.active = index + 1;
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
}

fn close_pane_inner(st: &mut AppState, id: PaneId) {
    let Some(tab_index) = st.ws.tab_of(id) else { return };
    let emptied = st.ws.tabs[tab_index].remove(id);
    if st.cfg.close_action == "background" {
        detach_runtime(st, id);
    } else {
        st.panes.remove(&id);
    }
    // pane_rows 只由 refresh_views 维护：在这里提前删会破坏「模型行数是否仍然对齐」的判断。
    if emptied {
        st.ws.tabs.remove(tab_index);
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

    let (cols, rows) = cell_of(st.content, st.cell);
    for tab_snap in &snap.tabs {
        let cwds = tab_snap.tree.leaf_cwds();
        let ids: Vec<PaneId> = (0..cwds.len()).map(|_| st.ws.alloc_id()).collect();
        let Some(root) = tab_snap.tree.assign(&ids) else {
            eprintln!("[session] 布局与 pane 数量不符，跳过该标签");
            continue;
        };
        let mut created = 0;
        for (i, cwd) in cwds.iter().enumerate() {
            match create_pane(st, ids[i], cols, rows, cwd.as_ref().map(PathBuf::from), None) {
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
            // 半成品标签丢弃，避免留下没有 PTY 的叶子
            for id in &ids {
                st.panes.remove(id);
            }
            continue;
        }
        let focused = ids.get(tab_snap.focused).copied().unwrap_or(ids[0]);
        st.ws.tabs.push(Tab {
            title: tab_snap.title.clone(),
            root: Some(root),
            focused,
            grid: HashMap::new(),
        });
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
    match create_pane(st, new_id, guess.0, guess.1, None, None) {
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

/// F11 / --fullscreen：切换全屏。
fn toggle_fullscreen() {
    with_app(|st| {
        let win = st.ui.window();
        let next = !win.is_fullscreen();
        win.set_fullscreen(next);
        eprintln!("[ui] fullscreen={next}");
    });
}

fn set_active_tab(index: usize) {
    with_app(|st| set_active_tab_inner(st, index));
}

fn set_active_tab_inner(st: &mut AppState, index: usize) {
    if index >= st.ws.tabs.len() || st.ws.active == index {
        return;
    }
    st.ws.active = index;
    eprintln!("[ws] active tab={index}");
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

fn switch_tab_relative(delta: isize) {
    with_app(|st| {
        let len = st.ws.tabs.len();
        if len < 2 {
            return;
        }
        let next = next_tab_index(st.ws.active, len, delta);
        set_active_tab_inner(st, next);
    });
}

/// Ctrl+1..9：跳到第 n 个 Tab（不存在则忽略）。
fn switch_tab_number(n: usize) {
    with_app(|st| {
        if n >= 1 && n <= st.ws.tabs.len() {
            set_active_tab_inner(st, n - 1);
        }
    });
}

// ------------------------------------------------------------------- 布局

/// 重新计算布局：必要时 resize 各 pane 的网格，并刷新 Slint 模型。
fn refresh_views(st: &mut AppState) {
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
            if rt.grid != (cols, rows) {
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
        .map(|(i, t)| TabView { title: t.title.clone().into(), active: i == active })
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
    for rt in st.panes.values_mut() {
        let (cols, rows) = rt.grid;
        rt.renderer = Renderer::new(fonts.clone(), cols, rows, theme.spec.bg, theme.spec.cursor);
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
        "tab.new" => {
            new_tab_inner(st);
            rebuild_tabs(st);
        },
        "tab.close" => close_tab_inner(st, st.ws.active),
        "tab.next" => switch_tab_relative(1),
        "tab.prev" => switch_tab_relative(-1),
        "tab.duplicate" => duplicate_tab(st, st.ws.active),
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
        "view.toggle-fullscreen" => toggle_fullscreen(),
        "view.zoom-in" => {
            st.cfg.font_size = (st.cfg.font_size + 1.0).clamp(8.0, 40.0);
            apply_font_size(st);
            st.cfg.save();
        },
        "view.zoom-out" => {
            st.cfg.font_size = (st.cfg.font_size - 1.0).clamp(8.0, 40.0);
            apply_font_size(st);
            st.cfg.save();
        },
        "view.zoom-reset" => {
            st.cfg.font_size = 15.0;
            apply_font_size(st);
            st.cfg.save();
        },
        "session.restart" => restart_session(st),
        "session.reattach" => reattach_last_background(st),
        "settings.open" => st.ui.set_settings_open(true),
        other => eprintln!("[ui] 未知命令 {other}"),
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
    // F11：全屏切换
    if text == "\u{F70E}" {
        toggle_fullscreen();
        return;
    }

    // F2：重命名当前 Tab（Slint 特殊键编码见 keys.rs）
    if text == "\u{F705}" {
        with_app(|st| open_rename(st, st.ws.active));
        return;
    }

    // Tab 导航在任何输入状态下都生效（含搜索框开着时）
    if ctrl && !alt {
        if text == "\t" {
            switch_tab_relative(if shift { -1 } else { 1 });
            return;
        }
        if !shift {
            if let Some(c) = text.chars().next() {
                if ('1'..='9').contains(&c) {
                    switch_tab_number(c as usize - '0' as usize);
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
            _ => {},
        }
    }

    if ctrl {
        match text {
            "t" => {
                new_tab_inner(st);
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
            _ => {},
        }
    }

    if PIPE_ON.load(Ordering::Relaxed) {
        *LAST_KEY.lock().unwrap() = Some(Instant::now());
    }
    let Some(tab) = st.ws.active_tab() else { return };
    let focused = tab.focused;
    let encoded = keys::encode(text, ctrl, alt);
    match st.panes.get_mut(&focused) {
        Some(rt) => {
            if let Some(bytes) = &encoded {
                rt.handle.input(bytes);
                if keylog {
                    eprintln!("[send] pane={focused} bytes={bytes:?}");
                }
            } else if keylog {
                eprintln!("[send] pane={focused} 未编码 text={text:?}");
            }
        },
        None => {
            if keylog {
                eprintln!(
                    "[send] 找不到 pane {focused}（现有 {:?}）",
                    st.panes.keys().collect::<Vec<_>>()
                );
            }
        },
    }
}

fn poll_events() {
    with_app(|st| {
        let mut changed = false;
        let ids: Vec<PaneId> = st.panes.keys().copied().collect();
        for id in ids {
            let Some(rt) = st.panes.get_mut(&id) else { continue };
            let mut events = Vec::new();
            rt.handle.drain_events(&mut events);
            for ev in events {
                if let CoreEvent::Title(t) = ev {
                    if !t.is_empty() && rt.title != t {
                        rt.title = t;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            let active = st.ws.active;
            for (i, tab) in st.ws.tabs.iter_mut().enumerate() {
                if i != active {
                    continue;
                }
                if let Some(first) = tab.panes().first() {
                    if let Some(rt) = st.panes.get(first) {
                        tab.title = rt.title.clone();
                    }
                }
            }
            rebuild_tabs(st);
        }
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
}

fn parse_args(cfg: &Config) -> Options {
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
    let mut it = std::env::args().skip(1);
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
            "--" => {
                o.args = it.by_ref().collect();
                break;
            },
            other => o.args.push(other.to_string()),
        }
    }
    o
}

fn print_help() {
    println!(
        "Rust Tabby {}\n\n\
用法: rusttabby-app [选项] [-- <shell 参数>...]\n\n\
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
-h, --help          显示帮助\n\n\
快捷键: Ctrl+T 新建标签 / Ctrl+W 关闭 / Ctrl+F 搜索 / Ctrl+Shift+C 复制 / Ctrl+Shift+V 粘贴\n\
配置: ~/.config/rusttabby/config.toml (RUSTTABBY_CONFIG 可覆盖)\n\
主题目录: ~/.config/rusttabby/themes (RUSTTABBY_THEMES_DIR 可覆盖；支持 iTerm2/WT/Alacritty/kitty/Xresources/YAML)",
        env!("CARGO_PKG_VERSION")
    );
}

/// 内部探针：不启动 UI，直接用第一个 SSH Profile 跑一次 SFTP 操作。
/// 用法：`rusttabby-app --sftp-probe <list|get|put> <remote> [local]`
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
            sftp::download(&target, remote, local).map(|_| String::new())
        },
        "put" => {
            // put <local> <remote>（与 cp 习惯一致；get 则是 <remote> <local>）
            let local = args.get(1).map(String::as_str).unwrap_or("");
            let remote = args.get(2).map(String::as_str).unwrap_or("");
            sftp::upload(&target, local, remote).map(|_| String::new())
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let t_start = Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv.iter().position(|a| a == "--sftp-probe") {
        return Ok(std::process::exit(run_sftp_probe(&argv[pos + 1..])));
    }
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
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
    let cfg = Config::load();
    let opts = parse_args(&cfg);
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
    let bg_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_background_items(ModelRc::from(bg_model.clone()));
    let profile_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_profile_items(ModelRc::from(profile_model.clone()));
    let sftp_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_sftp_items(ModelRc::from(sftp_model.clone()));
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
        commands: commands::builtin(),
        palette_ids: Vec::new(),
        palette_model,
        palette_sel: 0,
        drag: None,
        pending_paste: None,
        detached: Vec::new(),
        bg_model,
        profiles: profile::ProfileStore::load(),
        profile_model,
        sftp_profiles: Vec::new(),
        sftp_model,
        hover_cell: None,
        resize: None,
        xresize: None,
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
    let keylog = std::env::var("RUSTTABBY_KEYLOG").is_ok();
    // 鼠标链路排查：RUSTTABBY_MOUSELOG=1 打印「命中哪格 / 是否上报」。
    let mouselog = std::env::var("RUSTTABBY_MOUSELOG").is_ok();
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
            let Some(pointer) = resize_pointer(st) else { return };
            let pos = st.ui.window().position();
            let size = st.ui.window().size();
            st.resize = Some(WindowResize {
                dir,
                pointer,
                pos: (pos.x, pos.y),
                size: (size.width, size.height),
            });
        });
    });

    ui.on_window_resize_drag(|| {
        with_app(|st| {
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
        with_app(|st| st.resize = None);
    });

    ui.on_pane_clicked(|id| focus_pane(id as u64));

    ui.on_menu_dismissed(|| {
        eprintln!("[ui] menu dismissed");
    });

    ui.on_pane_right_clicked(|id, x, y| {
        with_app(|st| open_pane_menu(st, id as u64, x, y));
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
            _ => {},
        }
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

    ui.on_sftp_list(|| {
        with_app(|st| sftp_do_list(st));
    });

    ui.on_sftp_pick(|index| {
        with_app(|st| st.ui.set_sftp_selected(index.max(0)));
    });

    ui.on_sftp_download(|| {
        with_app(|st| sftp_do_download(st));
    });

    ui.on_sftp_upload(|| {
        with_app(|st| sftp_do_upload(st));
    });

    ui.on_sftp_close(|| {});

    ui.on_profile_open(|index| {
        with_app(|st| open_profile_tab(st, index.max(0) as usize));
    });

    ui.on_profile_new(|| {
        with_app(|st| open_profile_form(st));
    });

    ui.on_profile_edit(|index| {
        with_app(|st| edit_profile_form(st, index.max(0) as usize));
    });

    ui.on_profile_form_save(|| {
        with_app(|st| save_profile_form(st));
    });

    ui.on_profile_form_cancel(|| {
        with_app(|st| st.ui.set_form_error("".into()));
    });

    ui.on_profile_delete(|index| {
        with_app(|st| {
            let idx = index.max(0) as usize;
            if let Some(p) = st.profiles.profiles.get(idx).map(|p| p.id) {
                if st.profiles.remove(p) {
                    st.profiles.save();
                    refresh_profiles(st);
                    eprintln!("[profile] 删除 id={p}（剩 {}）", st.profiles.profiles.len());
                }
            }
        });
    });

    ui.on_profiles_close(|| {});

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

    ui.on_toggle_confirm_paste(|| {
        with_app(|st| {
            st.cfg.confirm_multiline_paste = !st.cfg.confirm_multiline_paste;
            st.ui.set_cfg_confirm_paste(st.cfg.confirm_multiline_paste);
            st.cfg.save();
            eprintln!("[ui] confirm_multiline_paste={}", st.cfg.confirm_multiline_paste);
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

    ui.on_settings_toggled(|open| {
        eprintln!("[ui] settings open={open}");
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
                new_tab_inner(st);
                rebuild_tabs(st);
            },
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
        let count = state.borrow().registry.all().count();
        ui.set_theme_choices(theme_choices(&state.borrow().registry));
        ui.set_settings_height(settings_height_for(count, win_h));
    }
    ui.set_font_size(state.borrow().cfg.font_size as i32);
    ui.set_scrollback(state.borrow().cfg.scrollback as i32);

    ui.on_set_theme(|name| {
        eprintln!("[theme] 选择 -> {name}");
        with_app(|st| apply_theme(st, name.as_str()));
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
            }
        });
    }

    ui.on_tab_clicked(|i| set_active_tab(i as usize));
    ui.on_tab_closed(|i| close_tab(i as usize));
    ui.on_new_tab(new_tab);

    ui.on_resized(|w, h| {
        with_app(|st| {
            let w = w.max(1.0);
            let h = (h - TABBAR_H).max(1.0);
            if (st.content.w - w).abs() < 0.5 && (st.content.h - h).abs() < 0.5 {
                return;
            }
            st.content = Rect::new(0.0, 0.0, w, h);
            refresh_views(st);
            redraw_visible(st);
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
        st.ui.set_tabbar_bottom(st.cfg.tabbar_position == "bottom");
        st.ui.set_settings_path(Config::path().display().to_string().as_str().into());
        eprintln!("[ui] 启动 tabbar_position={}", st.cfg.tabbar_position);
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
    if opts.fullscreen {
        ui.window().set_fullscreen(true);
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
}
