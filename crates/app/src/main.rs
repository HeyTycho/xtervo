// SPDX-License-Identifier: GPL-3.0-only
mod activity;
mod askpass;
mod bell;
mod commands;
mod config;
mod config_watch;
mod diag;
mod feedback;
mod glyph;
mod gpu_native;
mod gpu_present;
// 单实例 primary/secondary IPC + open_window 转发（产品启动路径）。
#[cfg(unix)]
mod instance;
mod keys;
mod known_hosts;
mod latency;
mod logging;
mod mcp;
// Plugin API v3 protocol exposes a few forward-compatible helpers/limits that are
// intentionally kept (and covered by tests) even when the current UI does not use
// every one of them.
#[allow(dead_code)]
mod plugin_v3;
mod plugin_wasm;
mod profile;
mod quake;
mod render;
#[cfg(test)]
mod image_layer_diff_tests;
#[cfg(test)]
mod render_bench;
#[cfg(test)]
mod render_damage_tests;
mod render_probe;
mod runtime;
mod secondary;
mod session;
mod sftp;
mod tabby_import;
mod themes;
mod vault;
mod window;
mod workspace;
mod xconn;

slint::include_modules!();

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::winit_030::{winit, EventResult, WinitWindowAccessor};
use slint::{Model, ModelRc, VecModel};
use tervo_core::{
    link::is_allowed_uri, spawn, ClipboardRequest, ClipboardSelection, CoreEvent, Frame, LinkKind,
    Modifiers, MouseAction, MouseButton, MouseEvent, Notification, Palette as TermPalette,
    PaneConfig, PaneHandle, ProgressState, ScrollPosition, SearchHit, SearchOptions, Theme,
    UiColors, WakeSignal,
};
use x11rb::connection::Connection;
use x11rb::rust_connection::RustConnection;

use crate::config::{Config, ScrollbarPolicy, UNLIMITED_SCROLLBACK_LINES};
use crate::glyph::FontSet;
#[cfg(unix)]
use crate::instance::{InstanceLock, InstancePaths, OpenWindowRequest, PrimaryServer};
use crate::latency::{DamageWatcher, Injector, LatencyStats};
use crate::mcp::McpPermissions;
use crate::render::Renderer;
use crate::runtime::{Shared, SharedServices, WindowId, WindowRegistry, WindowRuntime};
use crate::themes::{resolve_auto, ThemeData, ThemeRegistry, Variant};
use crate::workspace::{Dir, Divider, PaneId, Rect, Tab, Workspace, SPLIT_GAP};

/// 延迟探针使用的高辨识度字符（避免与 shell 提示符冲突）。
const MARKER: char = 'z';
const TITLE: &str = "Tervo";

/// 显式关闭单实例：仅测试/排障/legacy fallback 使用。默认必须是 true。
const SINGLE_INSTANCE_ENV: &str = "TERVO_SINGLE_INSTANCE";
/// secondary 等 primary 启动 control socket / --restore 等旧 primary 退出的上限。
const INSTANCE_CLAIM_TIMEOUT: Duration = Duration::from_secs(10);
/// primary 的 open_window handler 等待 UI 线程真正建完窗口的上限。
const INSTANCE_UI_ACK_TIMEOUT: Duration = Duration::from_secs(15);
/// 必须与 ui/main.slint 里的 `tabbar-height` 保持一致。
const TABBAR_H: f32 = 32.0;

/// 侧栏宽度（逻辑像素）：只在 `sidebar_visible` 时占位，否则 0 —— 默认关闭时
/// 布局与 0.5.0 逐像素一致。
fn sidebar_width_of(cfg: &crate::config::Config) -> f32 {
    if cfg.sidebar_visible {
        cfg.sidebar_width()
    } else {
        0.0
    }
}

/// 翻转侧栏可见性，返回新的内容区（纯函数：窗口逻辑宽度由旧内容区 + 旧栏宽反推）。
fn toggle_sidebar_math(content: Rect, cfg: &mut crate::config::Config) -> Rect {
    let old_sw = sidebar_width_of(cfg);
    let win_w = content.w + old_sw;
    let win_h = content.h + TABBAR_H;
    cfg.sidebar_visible = !cfg.sidebar_visible;
    content_rect(win_w, win_h, sidebar_width_of(cfg))
}

/// 终端内容区矩形（TabBar 之下、侧栏之右）。`st.content` 的坐标系原点固定为
/// 内容区左上角，所以侧栏只影响宽度，不影响 pane 自己的坐标。
fn content_rect(win_w: f32, win_h: f32, sidebar_w: f32) -> Rect {
    let w = (win_w - sidebar_w).max(1.0);
    let h = (win_h - TABBAR_H).max(1.0);
    Rect::new(0.0, 0.0, w, h)
}
/// `TermMode::BRACKETED_PASTE`
const BRACKETED_PASTE: u32 = 1 << 4;
/// 视觉铃边框闪一下的时长；到期由 30ms timer 清除。
const BELL_FLASH_MS: u64 = 180;
/// 滚动条 thumb 的最小像素高度（长 scrollback 时仍可抓取）。
const SCROLLBAR_MIN_THUMB_PX: f32 = 24.0;

/// 一个 pane 的外观覆盖（Profile 显式值 > 全局 Config；`None` = 继承）。
///
/// 只保存“选择”，不保存解析后的 `ThemeData`：全局主题/系统深浅变化时，
/// Profile 选择 `auto` 的 pane 也能跟着重新解析；渲染用字体集则按字号缓存。
#[derive(Clone, Debug, Default)]
struct PaneAppearance {
    font_size: Option<f32>,
    /// 主题 id（`auto` / 内置 id / `custom:<stem>`）。
    theme: Option<String>,
    /// 规范化后的 `block` / `underline` / `ibeam` 等。
    cursor_shape: Option<String>,
    /// `Some(true/false)` = Profile 显式闪/不闪；`None` = 跟随全局。
    cursor_blink: Option<bool>,
    scrollback: Option<usize>,
}

impl PaneAppearance {
    fn from_profile(profile: &profile::Profile) -> Self {
        Self {
            font_size: profile.effective_font_size(),
            theme: profile.effective_theme().map(str::to_string),
            cursor_shape: profile
                .effective_cursor_shape()
                .map(|shape| shape.config_name().to_string()),
            cursor_blink: profile.effective_cursor_blink(),
            scrollback: profile.effective_scrollback(),
        }
    }

    /// 该 pane 实际使用的光标形状：Profile 值优先，否则全局。
    fn cursor_shape(&self, cfg: &Config) -> tervo_core::CursorShape {
        self.cursor_shape
            .as_deref()
            .map(tervo_core::CursorShape::from_name)
            .unwrap_or_else(|| cfg.cursor_shape())
    }

    /// 该 pane 是否需要闪烁（Profile 覆盖全局）。
    fn cursor_blinks(&self, global: bool) -> bool {
        self.cursor_blink.unwrap_or(global)
    }
}

struct PaneRuntime {
    handle: PaneHandle,
    renderer: Renderer,
    frame: Box<Frame>,
    image: slint::Image,
    /// 该 pane 打开时使用的 Profile 外观覆盖；全局设置变化时用于决定谁优先。
    appearance: PaneAppearance,
    /// 该 pane 配置的 scrollback 行数（Profile 覆盖或全局），作为预算上限。
    scrollback_limit: usize,
    /// 已下发给 core 的 scrollback 行数，避免每次布局重复 set_options。
    scrollback_applied: usize,
    /// 该 pane 关联的 Profile ID（None = 默认 shell / 无关联）。会话快照用它
    /// 保留连接类型；Profile 已删除或启动失败时恢复路径回退普通 shell。
    profile_id: Option<u64>,
    /// 渲染字符尺寸（Profile 字号可能不同，布局/PTY resize 必须按 pane 自己的）。
    cell: (usize, usize),
    /// 光标 overlay（app 不把光标画进终端帧，闪烁只切可见性）。
    cursor_image: slint::Image,
    /// Sixel 图片 overlay 的 Slint 模型；只在图片集合/位置变化时重建。
    inline_model: Rc<VecModel<PaneImage>>,
    /// 图片 ID → Slint Image 缓存（避免每次滚动/输出重复上传整张图）。
    /// 源像素 ID 而非 placement ID：同一 placement 数据变化也不能复用旧纹理。
    inline_cache: HashMap<u64, slint::Image>,
    /// 上一次下发的图片布局签名；相同则跳过模型重建。
    inline_sig: Vec<(u64, u64, i32, u16, u32, u32)>,
    /// 点击命中只依赖 placement + 不可变 source；与可见纹理缓存分离。
    inline_hits: HashMap<u64, ImageSource>,
    /// 图片 overlay 依赖的 cell/grid 几何。字体/resize/分屏变化时图片的
    /// x/y/scale 必须重算，不能只比较图片 id/row/col。
    inline_geometry: (usize, usize, usize, usize),
    /// overlay 在 pane 内的 x/y/w/h（像素）。
    cursor_rect: (f32, f32, f32, f32),
    /// 当前帧光标是否显示（不含闪烁相位；phase 由 AppState 统一控制）。
    cursor_visible: bool,
    grid: (usize, usize),
    /// 最近一帧的终端光标；IME/焦点同步用。
    cursor: (usize, usize),
    /// 已发送的 1004 focus 状态；None=应用未启用。
    focus_sent: Option<bool>,
    rect: Rect,
    title: String,
    /// 进度 / 未读通知 / 完成标记，rebuild_tabs 时按 Tab 聚合。
    activity: activity::PaneActivity,
    /// 最近一条通知；切到该 Tab 时清空。
    notification: Option<Notification>,
    /// 视觉铃闪烁到期时间；Some = 正在闪。由 `tick_bell_flash` 清除。
    bell_until: Option<Instant>,
    /// 当前 pane 的进程是否通过 SSH/telnet/serial Profile 启动。
    /// 远端会话里的路径不能交给本地 xdg-open，避免把远端路径当本地文件打开。
    remote: bool,
    /// 远端会话退出状态（进程退出后填充；SSH 断开时用于 UI 提示/重连）。
    exit_code: Option<i32>,
    exit_signal: Option<String>,
    /// 已经弹过「会话已结束」提示，避免重复 Exited 事件反复弹窗。
    exit_notified: bool,
    /// 鼠标正悬停在可点击链接上（Slint mouse-cursor / 避免每帧重绘终端）。
    link_hover: bool,
    /// 上一帧是否由 `TERVO_GPU_NATIVE` 出图。GPU 失败切回 CPU 时必须强制
    /// 整屏重绘，因为 GPU 期间没有维护 CPU 双缓冲的 damage 历史。
    gpu_native_active: bool,
    /// SSH 密码认证用的临时 askpass 脚本；随 pane 一起释放。
    _askpass: Option<askpass::Askpass>,
}

#[derive(Clone)]
struct ImageSource {
    source_id: u64,
    width: u32,
    height: u32,
    data: Arc<[u8]>,
}

/// SSH 会话结束提示：记录 pane/Profile，重连时复用同一格。
struct SshExitNotice {
    pane: PaneId,
    profile_id: Option<u64>,
    text: String,
}

struct Drag {
    pane: PaneId,
    start: (f32, f32),
    moved: bool,
    cell: (u16, u16),
    /// true = 事件已转发给 PTY（终端在鼠标上报模式），本地上屏不再做选词。
    mouse: bool,
    /// Ctrl+左键：不开选区，抬起时解析并打开链接。
    link: bool,
    /// 最近一次指针位置（内容区坐标），用于边缘自动滚动。
    last_pointer: (f32, f32),
    /// 当前自动滚动方向，避免每 50ms 重复刷日志。
    autoscroll_dir: i32,
    /// 指针已离开顶层窗口（或窗口失焦），自动滚动计时器已暂停。
    /// 指针回到窗口内且左键仍按住时恢复；避免拖出窗口后计时器空转/继续滚动。
    autoscroll_paused: bool,
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

/// 首次启用：先展示权限再确认，不做“启用即信任”的静默授权。
struct PluginConfirmState {
    key: String,
    name: String,
    permissions: Vec<String>,
}

/// 当前打开的声明式插件 UI（面板或 settings）。
struct PluginSurfaceState {
    plugin_id: String,
    plugin_name: String,
    /// `"settings"` 或 `"panel:<id>"`。
    surface: String,
    panel: Option<String>,
    doc: plugin_v3::SurfaceDoc,
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
    cfg: Shared<Config>,
    /// 本窗口身份：所有 GPU/session/插件/MCP 路由都必须显式带上它。
    window_id: WindowId,
    /// 每用户共享服务（Config/Theme/PluginHost/MCP/Session/Vault）。
    shared: Rc<SharedServices>,
    /// 确认关闭后要关掉的窗口。主窗口和 second 都可能有自己的确认弹窗；
    /// 存 id 而不是布尔值，避免确认框打开期间焦点切换关错窗口。
    pending_close_window: Option<WindowId>,
    /// 当前实际生效的主题（`cfg.theme == "auto"` 时是解析后的结果）。
    resolved_theme: ThemeData,
    /// 内置 + `themes/*.toml` 自定义主题。
    registry: Shared<ThemeRegistry>,
    /// 是否已启动过系统配色监听线程（避免重复 spawn）。
    auto_watch_started: bool,
    search_open: bool,
    search_query: String,
    /// 查找选项：区分大小写 / 正则。
    search_match_case: bool,
    search_regex: bool,
    /// Save Contents 弹窗打开时锁定的 pane（右键菜单可能不是焦点 pane）。
    save_pane: Option<PaneId>,
    /// 命令表（Command Palette / 快捷键分发共用）
    commands: commands::Registry,
    /// Palette 当前展示项对应的命令 ID（与 palette_model 同序）
    palette_ids: Vec<String>,
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
    profiles: Shared<profile::ProfileStore>,
    profile_model: Rc<VecModel<ProfileRow>>,
    /// 过滤后各列表行在 `profiles.profiles` 中的原始下标（点击/编辑/删除按此映射）。
    profile_indices: Vec<usize>,
    /// Profiles 面板搜索框当前内容（空 = 显示全部）。
    profile_query: String,
    /// 加密凭据容器：启动时锁定，需在设置 → Vault 里解锁。
    vault: Shared<vault::Vault>,
    /// Vault 后台 KDF 任务（解锁/初始化/改密码）；Argon2 不跑 UI 线程。
    vault_job: Option<vault::VaultTask>,
    /// 正在跑的后台动作（决定成功提示文案）。
    vault_action: Option<VaultAction>,
    /// `true` 时 Vault 页显示“正在派生密钥”，按钮由 Rust 侧防重入。
    vault_busy: bool,
    /// Clear vault 需要点两次确认。
    vault_clear_armed: bool,
    /// SSH 连接前的 known_hosts 后台预检。
    hostkey_job: Option<known_hosts::HostKeyJob>,
    /// 等待预检结果的 Profile 下标 + 缓存键。
    hostkey_pending: Option<(usize, String)>,
    /// 预检结论等待用户选择（index, cache key, verdict）。
    hostkey_prompt: Option<(usize, String, known_hosts::Verdict)>,
    /// 进程内已确认过的连接配置（避免每次连接都 keyscan）。
    hostkey_verified: std::collections::HashSet<String>,
    /// 最近一个结束的 SSH 会话（用于「会话已结束 / 重连」提示）。
    ssh_exit: Option<SshExitNotice>,
    /// SFTP 面板里可选的 SSH Profile 索引（打开面板时快照）
    sftp_profiles: Vec<usize>,
    sftp_model: Rc<VecModel<slint::SharedString>>,
    hotkey_model: Rc<VecModel<HotkeyRow>>,
    /// 与 hotkey_model 同序的命令 ID
    hotkey_ids: Vec<&'static str>,
    /// 正在录制的命令 ID；Some 时所有按键只用于录制
    hotkey_recording: Option<&'static str>,
    /// kitty REPORT_EVENT_TYPES 下已经转发给各 pane 的按键身份。
    ///
    /// release 只回放这里登记过的键：快捷键/搜索框/录制中的按键 press 没进 PTY，
    /// 对应 release 也绝不能进 PTY。普通 shell（未请求协议）永远为空。
    held_keys: HashMap<PaneId, std::collections::HashSet<keys::KeyId>>,
    plugins: Shared<plugin_v3::PluginStore>,
    /// 后台事件唤醒（插件活动/SFTP 结果共用，容量 1）：写入结果后通知 UI。
    app_wake: WakeSignal,
    /// Plugins 设置页的行模型（声明式；所有控件由宿主渲染）。
    plugin_row_model: Rc<VecModel<PluginRowView>>,
    /// Plugins 设置页「最近插件活动」模型（新→旧）。
    plugin_activity_model: Rc<VecModel<slint::SharedString>>,
    /// 与 `PluginStore` 活动日志 revision 对齐，避免每 400ms 无谓刷新 Slint model。
    plugin_activity_revision: u64,
    /// 状态提供者 revision：变化时才更新底部状态栏文本。
    plugin_status_revision: u64,
    /// 运行状态 revision：常驻插件崩溃/重启/禁用后刷新设置页列表。
    plugin_runtime_revision: u64,
    /// 底部状态栏当前可点击执行的命令（空 = 不可点击）。
    plugin_status_command: Option<String>,
    /// 首次启用时的权限确认卡片；None = 不显示。
    plugin_confirm: Option<PluginConfirmState>,
    /// 当前打开的声明式面板/settings。
    plugin_surface: Option<PluginSurfaceState>,
    plugin_surface_model: Rc<VecModel<PluginUiElement>>,
    /// terminal/clipboard ask：当前展示的确认请求。
    plugin_approval: Option<plugin_v3::ApprovalRequest>,
    /// 上一次 pane focus；用于去重 `pane-focus` 事件。
    last_plugin_focus: Option<PaneId>,
    /// Delete 需要点两次确认（避免误删远端文件）
    sftp_delete_armed: bool,
    /// 进行中的上传/下载；同一时间只允许一个。
    sftp_transfer: Option<sftp::TransferJob>,
    /// 当前传输请求；失败时保留用于 Retry。
    sftp_current_request: Option<sftp::TransferRequest>,
    /// 上传/下载队列（当前任务结束后自动取下一个）。
    sftp_queue: sftp::TransferQueue,
    sftp_queue_model: Rc<VecModel<slint::SharedString>>,
    /// 最近一次失败的传输请求（Retry 按钮用；成功后清空）。
    sftp_last_failed: Option<sftp::TransferRequest>,
    /// 传输状态机（进度事件在 poll_events 里消费后写入 UI）。
    sftp_transfer_state: sftp::TransferState,
    /// 进行中的远端元数据操作（list/mkdir/rename/delete）；后台线程 + 10s 硬超时。
    sftp_op: Option<sftp::SftpOpJob>,
    /// 仅在有元数据操作时运行的 100ms poll：结果/超时尽快写回 UI（不影响 400ms 主轮询）。
    sftp_op_timer: slint::Timer,
    /// Quake 模式窗口当前是否可见；以及进入 Quake 前的普通几何（退出时恢复）
    quake_visible: bool,
    quake_saved: Option<(slint::PhysicalPosition, slint::PhysicalSize)>,
    /// “+” 下拉里各 Profile 在 store 中的下标（第 0 项是默认新 Tab）
    profile_menu_indices: Vec<usize>,
    profile_menu_model: Rc<VecModel<slint::SharedString>>,
    /// 右键按下的 pane/时间/内容区坐标：paste/clipboard 模式用抬起时间判断长按。
    right_down: Option<(PaneId, std::time::Instant, f32, f32)>,
    /// 打开右键菜单时命中的链接（空 = 当前格没有可打开链接）。
    menu_link: String,
    /// 广播输入：开启后键入同时发给当前 Tab 的所有 pane
    broadcast: bool,
    /// 上次左键点击 (pane, 行, 列, 时间)：用于 400ms 内的双击选行
    last_click: Option<(PaneId, i32, i32, std::time::Instant)>,
    /// 最近关闭的 Tab（最多 10 个），用于 Ctrl+Shift+T / 菜单重开
    closed_tabs: Vec<session::TabSnapshot>,
    fwd_profile_indices: Vec<usize>,
    fwd_model: Rc<VecModel<slint::SharedString>>,
    settings_result_cats: Vec<i32>,
    /// 与搜索结果同序的卡片锚点（用于点击结果后滚动/高亮具体设置分组）。
    settings_result_sections: Vec<&'static str>,
    /// 每次跳转自增，Slint 侧 `changed` 触发滚动（同锚点重复点击也能再滚一次）。
    settings_jump_token: i32,
    /// 布局完成后再补一次 token：搜索结果首次下发时卡片 y 可能还没算出来。
    settings_jump_timer: slint::Timer,
    settings_model: Rc<VecModel<slint::SharedString>>,
    /// 1003 悬停上报的上一格（按格去重，避免鼠标移动刷爆有界 ctrl 队列）。
    hover_cell: Option<(PaneId, (u16, u16))>,
    /// 链接 hover 的上一格（按格去重；只有换格才扫描行文本/改鼠标指针）。
    link_hover_cell: Option<(PaneId, (u16, u16))>,
    resize: Option<WindowResize>,
    /// 窗口是否聚焦（100ms 轮询 winit），用于 DECSET 1004 focus 上报。
    window_focused: bool,
    /// TERVO_RENDER_HUD 的重复计时器；None = HUD 关闭。
    render_hud_timer: Option<slint::Timer>,
    /// Rust 侧权威的最大化 desired state；Slint 属性只做图标显示。
    window_maximized: bool,
    /// EWMH 最大化状态机（显式 xid + PropertyNotify）；非 X11 时为 None。
    maximize: Option<tervo_maximize::MaximizeRuntime>,
    /// 上次同步给输入法的候选点（含 pane/DPR/锚点来源）；None 表示尚未发送。
    ime_spot: Option<ImeSpotKey>,
    /// 强制下一次 IME 同步重新下发（pane/Tab 切换、resize、窗口 focus/DPR 变化）。
    /// 输入法可能丢弃过期的 XNSpotLocation，即使坐标没变也要重发一次。
    ime_spot_dirty: bool,
    /// mouse-reporting TUI（dsh 侧栏等）不移动硬件光标时，用最近一次点击的格子
    /// 作为 IME 候选锚点；否则输入法只能停在左侧 chat 光标。
    ime_override: Option<(PaneId, (u16, u16))>,
    /// 本次边/角拖动已交给 WM/compositor（winit drag_resize）。
    /// true 时 drag 回调不再每帧 query_pointer + set_size；若 150ms 内 WM 没有
    /// 产生尺寸变化，drag 回调会解除它并回退自绘，保证 resize 不会“按了没反应”。
    wm_resize: bool,
    /// WM 接管后指针被它 grab，Slint 收不到真实 ButtonRelease，TouchArea 会一直
    /// pressed/grabbed；收到第一帧 WM resize 后补发一次 PointerReleased 清理状态。
    wm_release_pending: bool,
    xresize: Option<ResizeCtx>,
    resize_events: u32,
    resize_applied: u32,
    resize_pending: Option<(f32, f32)>,
    resize_apply_scheduled: bool,
    resize_apply: slint::Timer,
    /// 最近一次 configure（WM 或自绘）时间；watchdog 用“最近还在动”而不是只看
    /// 按下后过了多久，避免 GPU/渲染排队导致 configure 稍晚到达时误判 WM 未接管。
    last_resize_event: Option<Instant>,
    /// 本次拖拽开始后是否收到过 configure；一旦收到就说明 WM 确实接管了。
    wm_resize_saw_configure: bool,
    /// resize 期间的 UI redraw 合并：缩小时仍立即 request_redraw（否则右上角按钮
    /// 会被新窗口裁掉一帧），放大时最多 60fps。
    resize_redraw_scheduled: bool,
    resize_redraw: slint::Timer,
    /// 停止拖动 200ms 后恢复背景图并做最终重绘。
    resize_finalize: slint::Timer,
    /// 视觉铃到期清除（30ms 重复；没有闪烁时停止）。
    bell_timer: slint::Timer,
    /// 光标闪烁相位：true = 显示；无闪烁/闪烁关闭时恒 true。
    cursor_blink_on: bool,
    /// 光标闪烁计时器；`cursor_blink=off` 时停掉，空闲零开销。
    cursor_blink_timer: slint::Timer,
    /// 选中拖到上下边缘时的自动滚动计时器；只在拖选期间运行。
    drag_autoscroll: slint::Timer,
    resize_in_progress: bool,
    /// 最近一次 resize apply 的最大耗时（profile 汇总）。
    resize_max_apply_us: f64,
    /// X11 SHAPE 自适应圆角 worker；None = 未启动（X11 连接/线程失败，窗口保持直角）。
    corners: Option<xconn::CornerShaper>,
    clipboard: Option<arboard::Clipboard>,
}

/// 应用内管线打点：key 回调 → 帧交到 Slint（不含 winit 帧节流与上屏）。
static LAST_KEY: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
static PIPE_SAMPLES: std::sync::Mutex<Vec<Duration>> = std::sync::Mutex::new(Vec::new());
static PIPE_ON: AtomicBool = AtomicBool::new(false);
/// 跟随系统的 `gsettings monitor` 子进程；退出事件循环时 kill，避免留下孤儿进程。
static SCHEME_WATCH_CHILD: std::sync::Mutex<Option<std::process::Child>> =
    std::sync::Mutex::new(None);
static SCHEME_WATCH_STOPPED: AtomicBool = AtomicBool::new(false);

/// `TERVO_RENDER_PROFILE=1`：打印每 pane 的栅格化/帧缓冲耗时与 resize apply 分项。
fn render_profile() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TERVO_RENDER_PROFILE").is_some())
}

/// 事件唤醒 A/B 诊断开关：默认开启；`TERVO_EVENT_WAKE=0` 回到纯 400ms 轮询。
///
/// 只用于性能对比/排障，默认路径不读取额外配置；关闭后默认行为与旧版一致。
fn event_wake_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("TERVO_EVENT_WAKE") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off"),
        Err(_) => true,
    })
}

/// 唤醒/轮询计数：`TERVO_WAKE_STATS=1` 时每 5s 打一行。
///
/// 空闲时 core/app 唤醒数应保持不变（没有事件就没有回调），400ms 兜底仍按
/// 周期跑；这是“不是把轮询改快”的直接证据。
static CORE_EVENT_WAKES: AtomicUsize = AtomicUsize::new(0);
static APP_EVENT_WAKES: AtomicUsize = AtomicUsize::new(0);
static PERIODIC_POLLS: AtomicUsize = AtomicUsize::new(0);
/// 实际请求的 Slint 后端（第二窗口安装 render notifier 用）。
static REQUESTED_BACKEND: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// 把后台唤醒信号桥接到 Slint 事件循环。
///
/// `rx` 是容量 1 的合并信号；`pending` 保证同一时刻最多排一个 UI 回调。
/// `rx` 断开（AppState/所有发送端释放）后线程自行退出，不留后台空转。
fn spawn_app_wake_bridge(rx: crossbeam_channel::Receiver<()>) {
    let pending = Arc::new(AtomicBool::new(false));
    let spawned = std::thread::Builder::new().name("app-wake".into()).spawn(move || {
        while rx.recv().is_ok() {
            if pending.swap(true, Ordering::AcqRel) {
                continue;
            }
            let pending = pending.clone();
            let _ = slint::invoke_from_event_loop(move || {
                // 先清 pending 再处理：drain 期间到达的新事件还能再排一次回调；
                // 处理函数只做非阻塞 try_recv / revision 去重，重复回调无副作用。
                pending.store(false, Ordering::Release);
                on_app_event_wake();
            });
        }
    });
    if let Err(e) = spawned {
        eprintln!("[event] app wake 桥接线程启动失败: {e}");
    }
}

/// 解析 live 命令的可选 `window_id`：不传 = None（缺省 active），显式值必须
/// 命中 WindowRegistry，否则命令失败而不是悄悄改到别的窗口。
fn live_window_arg(args: &serde_json::Value) -> Result<Option<WindowId>, String> {
    let value = match args.get("window_id") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let raw = value
        .as_u64()
        .filter(|raw| *raw >= 1)
        .ok_or_else(|| "window_id must be a positive integer".to_string())?;
    let id = WindowId::new(raw);
    let known = runtime::registry()
        .map(|registry| registry.borrow().get(id).is_some())
        .unwrap_or(false);
    if !known {
        return Err(format!("unknown window_id {raw}"));
    }
    Ok(Some(id))
}

fn live_target_window(args: &serde_json::Value) -> Result<WindowId, String> {
    live_window_arg(args)?
        .or_else(runtime::current_window)
        .or_else(|| runtime::registry().and_then(|registry| registry.borrow().active()))
        .ok_or_else(|| "no active window".to_string())
}

fn live_audit(cmd: &str, window: Option<WindowId>, pane: Option<u64>, bytes: usize) {
    // Audit sink 是普通 Tervo 日志；绝不记录命令文本/token。
    match window {
        Some(id) => eprintln!("[mcp] action={cmd} window={id} pane={pane:?} arg_bytes={bytes}"),
        None => eprintln!("[mcp] action={cmd} window=active pane={pane:?} arg_bytes={bytes}"),
    }
}

/// live control socket 的写操作入口；只在 UI 线程调用。
fn live_control(cmd: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    // Audit: never log command text or secrets; only the action, target window
    // and bounded size/shape metadata. The normal Tervo log file is the audit sink.
    let pane = args.get("pane_id").and_then(serde_json::Value::as_u64);
    let bytes = args
        .get("text")
        .or_else(|| args.get("command"))
        .and_then(serde_json::Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    if cmd == "list" {
        let requested = live_window_arg(args)?;
        let audit_window = requested
            .or_else(|| runtime::registry().and_then(|registry| registry.borrow().active()));
        live_audit(cmd, audit_window, pane, bytes);
        return live_list_windows(args, requested);
    }
    if cmd == "new_window" {
        live_audit(cmd, None, None, 0);
        // 默认同进程；只有显式 legacy fallback 时 MCP 也没有子进程语义，
        // 因此这里始终走 WindowRegistry（不再依赖 TERVO_MULTI_WINDOW_INPROC）。
        let id = secondary::create_inproc_window()?;
        eprintln!("[mcp] action=new_window created window={id}");
        return Ok(serde_json::json!({"window_id": id.get()}));
    }
    if cmd == "close_window" {
        let id = live_window_arg(args)?
            .ok_or_else(|| "window_id required for close_window".to_string())?;
        live_audit(cmd, Some(id), pane, bytes);
        if runtime::window_count() <= 1 {
            return Err("refusing to close the last window; quit the window instead".into());
        }
        close_window_now(id, "mcp-close");
        return Ok(
            serde_json::json!({"window_id": id.get(), "remaining": runtime::window_count()}),
        );
    }
    let window = live_target_window(args)?;
    live_audit(cmd, Some(window), pane, bytes);
    let value = runtime::with_window(window, |st| match cmd {
        "output" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let rt = st.panes.get(&pane).ok_or_else(|| format!("unknown pane {pane}"))?;
            let max = args.get("max_bytes").and_then(serde_json::Value::as_u64).unwrap_or(65536).min(262144) as usize;
            let mut text = String::new();
            for line in 0..rt.frame.rows {
                for col in 0..rt.frame.cols {
                    let cell = rt.frame.cell(line, col);
                    if cell.ch != 0 { text.push(char::from_u32(cell.ch).unwrap_or('?')); }
                }
                text.push('\n');
            }
            let bytes = text.as_bytes();
            let start = bytes.len().saturating_sub(max);
            Ok(serde_json::json!({
                "schema":"tervo.control.v1",
                "pane_id": pane,
                "text": String::from_utf8_lossy(&bytes[start..]).to_string(),
                "truncated": start > 0,
            }))
        },
        "send_text" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let text = args.get("text").and_then(serde_json::Value::as_str)
                .ok_or_else(|| "text required".to_string())?;
            if text.len() > 65536 { return Err("text too large".into()); }
            let rt = st.panes.get(&pane).ok_or_else(|| format!("unknown pane {pane}"))?;
            rt.handle.input(text.as_bytes());
            Ok(serde_json::json!({"pane_id":pane,"bytes":text.len()}))
        },
        "send_key" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let key = args.get("key").and_then(serde_json::Value::as_str)
                .ok_or_else(|| "key required".to_string())?;
            let bytes: &[u8] = match key {
                "Enter" => b"\r", "Tab" => b"\t", "Escape" => b"\x1b",
                "Ctrl+C" => b"\x03", "Ctrl+D" => b"\x04", "Ctrl+Z" => b"\x1a",
                "Up" => b"\x1b[A", "Down" => b"\x1b[B", "Right" => b"\x1b[C", "Left" => b"\x1b[D",
                _ => return Err(format!("unknown key {key}")),
            };
            let rt = st.panes.get(&pane).ok_or_else(|| format!("unknown pane {pane}"))?;
            rt.handle.input(bytes);
            Ok(serde_json::json!({"pane_id":pane,"key":key}))
        },
        "run" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let command = args.get("command").and_then(serde_json::Value::as_str)
                .ok_or_else(|| "command required".to_string())?;
            if command.len() > 65536 || command.contains(['\r','\n','\0']) { return Err("invalid command".into()); }
            let rt = st.panes.get(&pane).ok_or_else(|| format!("unknown pane {pane}"))?;
            rt.handle.input(command.as_bytes());
            rt.handle.input(b"\r");
            Ok(serde_json::json!({"pane_id":pane,"command_bytes":command.len()}))
        },
        "focus" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let idx = st.ws.tab_of(pane).ok_or_else(|| format!("unknown pane {pane}"))?;
            st.ws.tabs[idx].focused = pane;
            st.ime_spot_dirty = true;
            refresh_views(st);
            let win = st.window_focused;
            sync_terminal_focus(st, win);
            update_ime_spot(st);
            Ok(serde_json::json!({"pane_id":pane}))
        },
        "new_tab" => { new_tab_inner(st); Ok(serde_json::json!({"ok":true})) },
        "split" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let dir = match args.get("direction").and_then(serde_json::Value::as_str).unwrap_or("right") {
                "right" => Dir::Row, "down" => Dir::Col,
                _ => return Err("direction must be right/down".into()),
            };
            // 窗口路由可能把命令送到不含该 pane 的窗口：这里必须显式报错，
            // 否则 split_pane_inner 静默 return，调用方却收到 ok=true。
            if !st.panes.contains_key(&pane) {
                return Err(format!("unknown pane {pane} in window {}", st.window_id.get()));
            }
            split_pane_inner(st, pane, dir);
            Ok(serde_json::json!({"pane_id":pane,"direction":format!("{dir:?}")}))
        },
        "scroll" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let lines = args.get("lines").and_then(serde_json::Value::as_i64)
                .ok_or_else(|| "lines required".to_string())?;
            let rt = st.panes.get(&pane).ok_or_else(|| format!("unknown pane {pane}"))?;
            rt.handle.scroll(lines as i32);
            Ok(serde_json::json!({"pane_id":pane,"lines":lines}))
        },
        "close_pane" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            if !st.panes.contains_key(&pane) { return Err(format!("unknown pane {pane}")); }
            close_pane_inner(st, pane);
            Ok(serde_json::json!({"pane_id":pane}))
        },
        "close_tab" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let idx = st.ws.tab_of(pane).ok_or_else(|| format!("unknown pane {pane}"))?;
            close_tab_inner(st, idx);
            Ok(serde_json::json!({"pane_id":pane,"tab":idx}))
        },
        "kill" => {
            let pane = args.get("pane_id").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| "pane_id required".to_string())?;
            let rt = st.panes.get(&pane).ok_or_else(|| format!("unknown pane {pane}"))?;
            rt.handle.input(b"\x03");
            Ok(serde_json::json!({"pane_id":pane,"signal":"Ctrl+C"}))
        },
        _ => Err(format!("unknown control command {cmd}")),
    })
    .unwrap_or_else(|| Err("app unavailable".into()))?;
    // 响应里回显实际路由到的窗口（additive；bridge 可据此校验）。
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "window_id".to_string(),
            serde_json::json!(window.get()),
        );
    }
    Ok(value)
}

/// MCP `list`：返回所有窗口 + 每窗口 panes；`active_window_id` 不含猜测。
///
/// `requested` 是调用方显式给的 window_id（已校验存在）；顶层 `panes` 兼容
/// 旧 bridge，指向该窗口，缺省 active。
fn live_list_windows(
    _args: &serde_json::Value,
    requested: Option<WindowId>,
) -> Result<serde_json::Value, String> {
    let Some(registry) = runtime::registry() else {
        return Err("window registry unavailable".into());
    };
    let active = registry.borrow().active();
    let runtimes = registry.borrow().runtimes();
    let mut windows = Vec::new();
    for rt in runtimes {
        let state = rt.state.clone();
        let Ok(st) = state.try_borrow() else { continue };
        let focused = st.ws.active_tab().map(|t| t.focused);
        let panes: Vec<serde_json::Value> = st
            .panes
            .iter()
            .map(|(id, pane)| {
                serde_json::json!({
                    "window_id": rt.id.get(),
                    "pane_id": id,
                    "title": pane.title,
                    "grid": [pane.grid.0, pane.grid.1],
                    "rect": [pane.rect.x, pane.rect.y, pane.rect.w, pane.rect.h],
                    "focused": focused == Some(*id),
                })
            })
            .collect();
        windows.push(serde_json::json!({
            "window_id": rt.id.get(),
            "title": rt.title.borrow().clone(),
            "active": active == Some(rt.id),
            "tabs": st.ws.tabs.len(),
            "active_tab": st.ws.active,
            "maximized": st.window_maximized,
            "geometry": {
                "x": rt.geometry().x,
                "y": rt.geometry().y,
                "w": rt.geometry().w,
                "h": rt.geometry().h,
            },
            "panes": panes,
            // 两个窗口必须指向同一份 SharedServices（Config/Theme/Plugin/Vault/
            // Profile/Session/MCP）；e2e 断言这些 identity 相等。
            "shared": st.shared.identities(),
            // 每个窗口自己的 UI/模型状态：e2e 用它证明设置页/命令面板/插件页
            // 确实开在目标窗口，且两个窗口的模型不共享。
            "ui": {
                "settings_open": st.ui.get_settings_open(),
                "settings_tab_open": st.ui.get_settings_tab_open(),
                "settings_category": st.ui.get_settings_category(),
                "settings_query": st.ui.get_settings_query().to_string(),
                "settings_results": st.settings_model.row_count(),
                "palette_open": st.ui.get_palette_open(),
                "palette_selected": st.ui.get_palette_selected(),
                "palette_items": st.palette_model.row_count(),
                "tabsearch_open": st.ui.get_tabsearch_open(),
                "tabsearch_items": st.tab_search_model.row_count(),
                "profiles_open": st.ui.get_profiles_open(),
                "profile_form_open": st.ui.get_profile_form_open(),
                "profile_rows": st.profile_model.row_count(),
                "sftp_open": st.ui.get_sftp_open(),
                "fwd_open": st.ui.get_fwd_open(),
                "search_open": st.ui.get_search_open(),
                "confirm_open": st.ui.get_confirm_open(),
                "rename_open": st.ui.get_rename_open(),
                "paste_open": st.ui.get_paste_open(),
                "save_open": st.ui.get_save_open(),
                "plugin_surface_open": st.ui.get_plugin_surface_open(),
                "plugin_rows": st.plugin_row_model.row_count(),
                "image_preview_open": st.ui.get_image_preview_open(),
            },
        }));
    }
    // 兼容旧 bridge：顶层 panes = 目标窗口（显式 window_id 或 active）。
    let target = requested
        .map(|id| id.get())
        .and_then(|raw| windows.iter().find(|w| w["window_id"] == raw).cloned())
        .or_else(|| windows.iter().find(|w| w["active"] == true).cloned());
    let panes = target
        .as_ref()
        .and_then(|w| w.get("panes"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    Ok(serde_json::json!({
        "schema": "tervo.control.v1",
        "active_window_id": active.map(|id| id.get()),
        "windows": windows,
        "panes": panes,
    }))
}

/// live control socket 的 snapshot：只在 UI 线程调用。
fn live_diag_snapshot() -> serde_json::Value {
    let Some(registry) = runtime::registry() else {
        return serde_json::json!({
            "schema":"tervo.diag.v1","kind":"live","error":"app unavailable"
        });
    };
    let active_window = registry.borrow().active();
    let runtimes = registry.borrow().runtimes();
    let mut windows = Vec::new();
    let mut all_panes = Vec::new();
    for rt in runtimes {
        let state = rt.state.clone();
        let Ok(st) = state.try_borrow() else { continue };
        let active = st.ws.active_tab().map(|t| t.focused);
        let mut panes = Vec::new();
        for (id, pane) in &st.panes {
            let mut f = diag::frame_json(&pane.frame, "live");
            f["window_id"] = serde_json::json!(rt.id.get());
            f["pane_id"] = serde_json::json!(id);
            f["title"] = serde_json::json!(pane.title);
            f["active"] = serde_json::json!(Some(*id) == active);
            f["grid"] = serde_json::json!([pane.grid.0, pane.grid.1]);
            panes.push(f.clone());
            all_panes.push(f);
        }
        windows.push(serde_json::json!({
            "window_id": rt.id.get(),
            "title": rt.title.borrow().clone(),
            "active": active_window == Some(rt.id),
            "tabs": st.ws.tabs.len(),
            "active_pane": active,
            "panes": panes,
        }));
    }
    serde_json::json!({
        "schema": "tervo.diag.v1",
        "kind": "live",
        "version": BUILD_VERSION,
        "commit": BUILD_COMMIT,
        "target": BUILD_TARGET,
        "active_window_id": active_window.map(|id| id.get()),
        "gpu": {
            "present_ready": gpu_present::ready(),
            "native_ready": gpu_native::ready(),
            "present_windows": gpu_present::windows().iter().map(|id| id.get()).collect::<Vec<_>>(),
            "present_frames": gpu_present::upload_stats().frames,
            "native_frames": gpu_native::totals().frames,
        },
        "windows": windows,
        // 兼容旧 MCP/诊断调用方：所有窗口 pane 平铺。
        "panes": all_panes,
    })
}

fn with_app<R>(f: impl FnOnce(&mut AppState) -> R) -> Option<R> {
    runtime::with_primary_window(f)
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
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
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
        let want = if detect_system_dark() {
            Variant::Dark
        } else {
            Variant::Light
        };
        resolve_auto(reg, want, auto_dark, auto_light)
    } else {
        reg.get(selected)
            .cloned()
            .unwrap_or_else(|| reg.dark_default().clone())
    }
}

/// 应用配置/Profile 的主题选择；`None` = 继承全局（返回 None）。
///
/// Profile 的 `theme` 只影响该 Tab 的终端内容（渲染器背景/调色板），不改
/// 应用 UI 主题；未知 id 记日志并回退当前全局主题，不改写 Profile。
fn theme_for_selection(
    reg: &ThemeRegistry,
    cfg: &Config,
    selection: Option<&str>,
) -> Option<ThemeData> {
    let selected = selection?.trim();
    if selected.is_empty() {
        return None;
    }
    if is_auto_theme(selected) {
        return Some(resolve_theme(
            reg,
            AUTO_THEME,
            cfg.auto_dark.as_deref(),
            cfg.auto_light.as_deref(),
        ));
    }
    match reg.get(selected) {
        Some(theme) => Some(theme.clone()),
        None => {
            eprintln!("[profile] 未知主题 {selected:?}，该 pane 回退全局主题");
            None
        },
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
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".config")
        });
    base.join("tabby").join("config.yaml")
}

/// 界面文案二选一：`lang == "zh"` 用中文，否则英文（未知语言已在 config 层回退）。
fn ui_text<'a>(lang: &str, en: &'a str, zh: &'a str) -> &'a str {
    if lang == "zh" {
        zh
    } else {
        en
    }
}

/// 设置分类显示名；下标就是 Slint 侧的 `settings-category`。
/// 0=Appearance 1=Terminal 2=Window 3=Advanced 4=Keys 5=Plugins 6=Vault 7=Profiles 8=Help 9=Privacy。
const SETTINGS_CATS_EN: [&str; 10] = [
    "Appearance",
    "Terminal",
    "Window",
    "Advanced",
    "Keys",
    "Plugins",
    "Vault",
    "Profiles",
    "Help",
    "Privacy",
];
const SETTINGS_CATS_ZH: [&str; 10] = [
    "外观", "终端", "窗口", "高级", "快捷键", "插件", "保险库", "配置", "帮助", "隐私",
];

fn settings_cat_label(cat: i32, lang: &str) -> &'static str {
    let i = cat.clamp(0, (SETTINGS_CATS_EN.len() - 1) as i32) as usize;
    if lang == "zh" {
        SETTINGS_CATS_ZH[i]
    } else {
        SETTINGS_CATS_EN[i]
    }
}

/// 一条设置搜索索引：(分类下标, 卡片锚点, 英文名, 中文名, 关键词)。
/// 锚点用于搜索结果点击后把内容滚到对应卡片并高亮；卡片 id 在 Slint 侧同名。
/// 名称/关键词同时索引中英文，展示行跟随当前 UI 语言。
struct SettingsCatalogEntry {
    cat: i32,
    section: &'static str,
    en: &'static str,
    zh: &'static str,
    keywords: &'static str,
}

const SETTINGS_CATALOG: &[SettingsCatalogEntry] = &[
    SettingsCatalogEntry { cat: 0, section: "theme", en: "Theme / Color scheme", zh: "主题 / 配色方案", keywords: "theme color scheme appearance dark light 主题 配色 外观 深色 浅色" },
    SettingsCatalogEntry { cat: 0, section: "background", en: "Background image", zh: "背景图", keywords: "background image wallpaper picture path 背景 壁纸 图片 路径" },
    SettingsCatalogEntry { cat: 0, section: "background", en: "Background opacity", zh: "背景不透明度", keywords: "background opacity transparency alpha 背景 透明 不透明度" },
    SettingsCatalogEntry { cat: 0, section: "background", en: "Background dim", zh: "背景暗化", keywords: "background dim mask overlay 背景 遮罩 压暗 暗化" },
    SettingsCatalogEntry { cat: 0, section: "cursor", en: "Cursor shape", zh: "光标形状", keywords: "cursor shape block underline ibeam beam 光标 形状" },
    SettingsCatalogEntry { cat: 0, section: "cursor", en: "Cursor blink", zh: "光标闪烁", keywords: "cursor blink on off system 光标 闪烁" },
    SettingsCatalogEntry { cat: 0, section: "cursor", en: "Cursor color", zh: "光标颜色", keywords: "cursor color colour text 光标 颜色" },
    SettingsCatalogEntry { cat: 0, section: "bold", en: "Bold is bright", zh: "粗体使用亮色", keywords: "bold bright ansi color 粗体 亮色" },
    SettingsCatalogEntry { cat: 0, section: "bold", en: "Bold color", zh: "粗体颜色", keywords: "bold color same as fg 粗体 颜色" },
    SettingsCatalogEntry { cat: 1, section: "text", en: "Font size", zh: "字号", keywords: "font size text zoom 字号 字体" },
    SettingsCatalogEntry { cat: 1, section: "scrollback", en: "Scrollback", zh: "回滚缓冲", keywords: "scrollback history lines 回滚 历史 缓冲" },
    SettingsCatalogEntry { cat: 1, section: "scrollback", en: "Unlimited scrollback", zh: "无限回滚", keywords: "unlimited scrollback history cap memory 无限 历史 内存" },
    SettingsCatalogEntry { cat: 1, section: "behavior", en: "Copy on select", zh: "选中即复制", keywords: "copy select clipboard 复制 选区" },
    SettingsCatalogEntry { cat: 1, section: "behavior", en: "Confirm before closing", zh: "关闭前确认", keywords: "confirm close window exit 确认 关闭" },
    SettingsCatalogEntry { cat: 1, section: "behavior", en: "Restore session on startup", zh: "启动时恢复会话", keywords: "restore session startup workspace tabs splits cwd 会话 恢复 启动" },
    SettingsCatalogEntry { cat: 1, section: "behavior", en: "Confirm multi-line paste", zh: "多行粘贴前确认", keywords: "paste multiline confirm 粘贴 确认 多行" },
    SettingsCatalogEntry { cat: 1, section: "behavior", en: "Right click", zh: "右键", keywords: "right click menu paste clipboard 右键" },
    SettingsCatalogEntry { cat: 1, section: "behavior", en: "Paste on middle-click", zh: "中键粘贴", keywords: "middle click paste 中键 粘贴" },
    SettingsCatalogEntry { cat: 1, section: "behavior", en: "Desktop notifications", zh: "桌面通知", keywords: "desktop notification notify-send 通知 桌面" },
    SettingsCatalogEntry { cat: 1, section: "bell", en: "Visual bell", zh: "视觉铃", keywords: "visual bell BEL flash border 视觉铃 响铃" },
    SettingsCatalogEntry { cat: 1, section: "bell", en: "Audible bell", zh: "声音铃", keywords: "audible bell BEL beep sound system 响铃 声音" },
    SettingsCatalogEntry { cat: 1, section: "scroll", en: "Scroll on output", zh: "输出时滚动", keywords: "scroll output follow bottom 输出 跟随 滚动" },
    SettingsCatalogEntry { cat: 1, section: "scroll", en: "Scroll on keystroke", zh: "按键时滚动", keywords: "scroll keystroke keyboard bottom 按键 回底 滚动" },
    SettingsCatalogEntry { cat: 1, section: "scrollbar", en: "Scrollbar policy", zh: "滚动条策略", keywords: "scrollbar always auto never policy 滚动条 策略" },
    SettingsCatalogEntry { cat: 1, section: "images", en: "Sixel inline images", zh: "Sixel 内联图片", keywords: "sixel image inline graphics 图片 内联" },
    SettingsCatalogEntry { cat: 1, section: "images", en: "iTerm2 IIP images", zh: "iTerm2 IIP 图片", keywords: "iterm2 iip osc 1337 image inline png jpeg 图片" },
    SettingsCatalogEntry { cat: 1, section: "images", en: "Render HUD", zh: "渲染 HUD", keywords: "render hud fps frame rate frame time renderer gpu debug overlay stage0 atlas 帧率 渲染 调试 覆盖层 性能" },
    SettingsCatalogEntry { cat: 1, section: "keys", en: "Backspace / Delete binding", zh: "退格 / 删除绑定", keywords: "backspace delete ascii del control-h key encoding 退格 删除 编码" },
    SettingsCatalogEntry { cat: 2, section: "window", en: "Tab bar at bottom", zh: "标签栏置底", keywords: "tab bar position bottom top 标签栏 位置 底部" },
    SettingsCatalogEntry { cat: 2, section: "window", en: "Quake mode", zh: "Quake 模式", keywords: "quake drop down hotkey F12 下拉 模式" },
    SettingsCatalogEntry { cat: 3, section: "config", en: "Config file path", zh: "配置文件路径", keywords: "config path file folder 配置 路径 文件" },
    SettingsCatalogEntry { cat: 3, section: "config", en: "Import / export", zh: "导入 / 导出", keywords: "import export tabby profiles 导入 导出" },
    SettingsCatalogEntry { cat: 4, section: "hotkeys", en: "Hotkey bindings", zh: "快捷键绑定", keywords: "hotkey shortcut key binding 快捷键 绑定" },
    SettingsCatalogEntry { cat: 5, section: "plugins", en: "Installed plugins", zh: "已安装插件", keywords: "plugin plugins installed list 插件 已安装" },
    SettingsCatalogEntry { cat: 6, section: "vault", en: "Vault / master password", zh: "保险库 / 主密码", keywords: "vault master password credentials 凭据 解锁 锁定 保险库 主密码" },
    SettingsCatalogEntry { cat: 7, section: "profiles", en: "Profiles", zh: "配置", keywords: "profile connection ssh serial telnet 连接 配置" },
    SettingsCatalogEntry { cat: 8, section: "quickstart", en: "Quick start / Keyboard shortcuts", zh: "快速开始 / 快捷键", keywords: "help getting started quick start keyboard shortcuts hotkey keybinding ctrl+t ctrl+w ctrl+f f11 settings 帮助 快速开始 快捷键 设置" },
    SettingsCatalogEntry { cat: 8, section: "paths", en: "Config & log paths", zh: "配置文件与日志", keywords: "help config log logs path directory diagnostic copy 帮助 配置 日志 路径 目录 诊断 复制" },
    SettingsCatalogEntry { cat: 8, section: "version", en: "Version information", zh: "版本信息", keywords: "help version commit build time target 帮助 版本 提交 构建 时间 目标" },
    SettingsCatalogEntry { cat: 8, section: "version", en: "Run Doctor", zh: "运行 Doctor", keywords: "doctor diagnostics health check issue evidence 诊断 健康 检查 证据 问题" },
    SettingsCatalogEntry { cat: 8, section: "limits", en: "Known limitations / Documentation", zh: "已知限制 / 更多文档", keywords: "help limitations docs documentation ime wayland plugin sandbox soak 帮助 限制 文档 输入法 沙箱 插件" },
    // MCP / Privacy：默认开启但默认只读；分组授权与 socket/token 状态。
    SettingsCatalogEntry { cat: 9, section: "mcp", en: "MCP live control", zh: "MCP 实时控制", keywords: "mcp model context protocol live control enable disable privacy socket token 启用 关闭 控制 权限 隐私" },
    SettingsCatalogEntry { cat: 9, section: "mcp", en: "MCP read access", zh: "MCP 读取权限", keywords: "mcp read list snapshot output capabilities readonly 只读 读取 输出 列表 快照" },
    SettingsCatalogEntry { cat: 9, section: "mcp", en: "MCP input access", zh: "MCP 输入权限", keywords: "mcp input send text key run write terminal 输入 发送 文本 按键 写入 终端" },
    SettingsCatalogEntry { cat: 9, section: "mcp", en: "MCP tab control", zh: "MCP 标签控制", keywords: "mcp tabs new tab split focus scroll 标签 新建 分屏 焦点 滚动" },
    SettingsCatalogEntry { cat: 9, section: "mcp", en: "MCP destructive actions", zh: "MCP 破坏性操作", keywords: "mcp destructive close kill terminate 破坏性 关闭 终止 权限" },
    SettingsCatalogEntry { cat: 9, section: "mcp", en: "MCP socket & token", zh: "MCP socket 与 token", keywords: "mcp socket token state path 0600 permission 路径 令牌 权限 状态" },
];

/// 一条设置搜索命中：分类、卡片锚点、展示行（分类 · 名称）。
#[derive(Debug)]
struct SettingsHit {
    cat: i32,
    section: &'static str,
    line: String,
}

/// 设置搜索：大小写不敏感的子串匹配（中英文名称或关键词都索引）；
/// 结果展示行跟随 `lang`，避免中文界面出现英文分类名。
fn settings_search_hits(query: &str, lang: &str) -> Vec<SettingsHit> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    SETTINGS_CATALOG
        .iter()
        .filter(|e| {
            e.en.to_lowercase().contains(&q)
                || e.zh.contains(&q)
                || e.keywords.to_lowercase().contains(&q)
        })
        .map(|e| SettingsHit {
            cat: e.cat,
            section: e.section,
            line: format!(
                "{} · {}",
                settings_cat_label(e.cat, lang),
                ui_text(lang, e.en, e.zh)
            ),
        })
        .collect()
}

/// 兼容单测/旧调用：只返回 (分类, 展示行)，默认英文。
#[cfg(test)]
fn settings_search(query: &str) -> Vec<(i32, String)> {
    settings_search_hits(query, "en")
        .into_iter()
        .map(|h| (h.cat, h.line))
        .collect()
}

/// 内置命令的中文标题。命令表保持稳定英文 ID；插件命令没有翻译时回退原名。
fn command_title(lang: &str, id: &str, fallback: &'static str) -> &'static str {
    if lang != "zh" {
        return fallback;
    }
    match id {
        "window.new" => "新建窗口",
        "tab.new" => "新建标签",
        "tab.search" => "搜索标签",
        "tab.close" => "关闭标签",
        "tab.next" => "下一个标签",
        "tab.prev" => "上一个标签",
        "tab.duplicate" => "复制标签",
        "tab.reopen-closed" => "重新打开已关闭标签",
        "pane.split-right" => "向右拆分",
        "pane.split-down" => "向下拆分",
        "pane.close" => "关闭窗格",
        "pane.focus-next" => "聚焦下一个窗格",
        "terminal.copy" => "复制",
        "terminal.paste" => "粘贴",
        "terminal.find" => "查找",
        "terminal.clear" => "清屏",
        "terminal.select-all" => "全选",
        "terminal.toggle-read-only" => "切换只读",
        "terminal.reset" => "重置终端",
        "terminal.reset-and-clear" => "重置并清空",
        "terminal.save-contents" => "保存内容…",
        "view.command-palette" => "命令面板",
        "view.toggle-fullscreen" => "切换全屏",
        "view.toggle-quake" => "切换 Quake 模式",
        "view.zoom-in" => "放大",
        "view.zoom-out" => "缩小",
        "view.zoom-reset" => "重置缩放",
        "session.restart" => "重启会话",
        "session.reattach" => "重连后台会话",
        "settings.open" => "打开设置",
        _ => fallback,
    }
}

/// Hotkeys 页数据：按分类/标题排序，返回 (摘要, 行, 行对应的命令 ID)。
/// 摘要/命令标题/空绑定都跟随 `lang`，避免中文设置页出现整句英文。
fn hotkey_rows(
    reg: &commands::Registry,
    overrides: &[commands::HotkeyOverride],
    lang: &str,
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
    let summary = if lang == "zh" {
        format!("{} 条命令 · {bound} 条已绑定 · {custom} 条自定义", cmds.len())
    } else {
        format!("{} commands · {bound} bound · {custom} custom", cmds.len())
    };
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
                .unwrap_or_else(|| ui_text(lang, "(none)", "（无）").to_string())
                .into();
            HotkeyRow {
                command: c.id.into(),
                title: command_title(lang, c.id, c.title).into(),
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
    let (summary, rows, ids) = hotkey_rows(&st.commands, &st.cfg.hotkeys, st.cfg.lang());
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

/// 插件活动行：动作/结果词跟随语言；插件名与输出摘要原样保留。
fn localize_plugin_activity(line: &str, lang: &str) -> String {
    if lang != "zh" {
        return line.to_string();
    }
    line.replace(" · command ", " · 命令 ")
        .replace(" · event ", " · 事件 ")
        .replace(" · ui.notify ", " · 通知 ")
        .replace(" · ui.render ", " · 渲染 ")
        .replace(" · supervisor ", " · supervisor ")
        .replace(" · lifecycle ", " · 生命周期 ")
        .replace(" · denied", " · 拒绝")
        .replace(" · ok", " · 成功")
        .replace(" · error", " · 错误")
        .replace(" · timeout", " · 超时")
        .replace(" · restart", " · 重启")
        .replace(" · failed", " · 失败")
        .replace(" · unavailable", " · 不可用")
        .replace(" · spawn-error", " · 启动失败")
}

/// 状态词本地化；插件名/错误正文（第三方或宿主诊断）不翻译。
fn localize_plugin_status(status: &str, lang: &str) -> String {
    if lang != "zh" {
        return status.to_string();
    }
    match status {
        "running" => "运行中".to_string(),
        "starting" => "启动中".to_string(),
        "restarting" => "重启中".to_string(),
        "stopped" => "已停止".to_string(),
        "failed" => "失败".to_string(),
        "disabled" => "已禁用".to_string(),
        "unsupported" => "不支持（v1/v2）".to_string(),
        "invalid" => "清单无效".to_string(),
        "permission confirmation required" => "需要确认权限".to_string(),
        other => other.to_string(),
    }
}

fn plugin_row_view(row: &plugin_v3::PluginRow, lang: &str) -> PluginRowView {
    PluginRowView {
        key: row.key.clone().into(),
        name: row.name.clone().into(),
        version: row.version.clone().into(),
        api: row.api.clone().into(),
        runtime: row.runtime.clone().into(),
        status: localize_plugin_status(&row.status, lang).into(),
        error: row.error.clone().into(),
        permissions: row.permissions.clone().into(),
        restart_count: row.restart_count,
        enabled: row.enabled,
        needs_approval: row.needs_approval,
        supported: row.supported,
        has_panel: row.has_panel,
        has_settings: row.has_settings,
        assistant: row.assistant,
        surface_ready: row.surface_ready,
        source: row.source.clone().into(),
        signature: row.signature.clone().into(),
        has_backup: row.has_backup,
        backup_version: row.backup_version.clone().into(),
    }
}

/// 刷新 Plugins 设置页：v3 行（name/version/api/status/error/permissions/
/// restart_count + 操作按钮）、unsupported v1/v2 行、目录与最近活动。
fn refresh_plugin_page(st: &mut AppState) {
    let lang = st.cfg.lang();
    let rows = st.plugins.rows();
    let views: Vec<PluginRowView> = rows
        .iter()
        .map(|row| plugin_row_view(row, lang))
        .collect();
    st.plugin_row_model.set_vec(views);
    let summary = if lang == "zh" {
        format!(
            "{} 个插件 · {} 个已启用 · {} 条命令 · {} unsupported/invalid · API v3",
            st.plugins.total_count(),
            st.plugins.enabled_count(),
            st.plugins.enabled_command_count(),
            st.plugins.unsupported_count()
        )
    } else {
        format!(
            "{} plugins · {} enabled · {} commands · {} unsupported/invalid · API v3",
            st.plugins.total_count(),
            st.plugins.enabled_count(),
            st.plugins.enabled_command_count(),
            st.plugins.unsupported_count()
        )
    };
    st.ui.set_plugin_summary(summary.as_str().into());
    st.ui.set_plugin_dir(
        match st.plugins.state_error() {
            Some(error) => format!(
                "{} — state error: {}",
                st.plugins.plugin_dir().display(),
                error
            ),
            None => st.plugins.plugin_dir().display().to_string(),
        }
        .as_str()
        .into(),
    );
    refresh_plugin_activity(st, true);
    if st.plugin_surface.is_some() {
        update_plugin_surface_model(st);
    }
}

/// 向插件提供非敏感配置元数据（只读 snapshot；不读环境变量/Vault）。
fn sync_plugin_config_metadata(st: &AppState) {
    st.plugins.set_config_metadata(serde_json::json!({
        "theme": st.cfg.theme,
        "language": st.cfg.lang(),
        "font_size": st.cfg.font_size,
        "tabbar_position": st.cfg.tabbar_position,
        "plugin_api": plugin_v3::PLUGIN_API_VERSION,
    }));
}

/// Settings → Plugins：本地安装/更新/回滚。安装过程不执行插件；成功后才刷新
/// store（已启用的插件经 v3 host reload，不重启 Tervo）。
fn plugin_install_action(st: &mut AppState, action: &str, key: &str, path: &str) {
    let lang = st.cfg.lang();
    let keep = plugin_v3::install::DEFAULT_BACKUP_KEEP;
    match action {
        "install" | "update" => {
            let update = action == "update";
            let source = path.trim();
            if source.is_empty() {
                set_status_toast(
                    st,
                    if lang == "zh" {
                        "先选择目录/安装包，或在路径框输入路径。".to_string()
                    } else {
                        "Choose a folder/package or enter a path first.".to_string()
                    },
                );
                return;
            }
            let source = config::expand_tilde(source);
            let result = st.plugins.install_local(
                &source,
                update,
                st.cfg.plugin_require_signature,
                keep,
            );
            match result {
                Ok(report) => {
                    let message = if lang == "zh" {
                        format!(
                            "{} {} 已{}（签名: {}）",
                            report.name,
                            report.version,
                            if update { "更新" } else { "安装" },
                            report.signature.label()
                        )
                    } else {
                        format!(
                            "{} {} {} (signature: {})",
                            report.name,
                            report.version,
                            if update { "updated" } else { "installed" },
                            report.signature.label()
                        )
                    };
                    set_status_toast(st, message);
                    st.ui.set_plugin_install_path("".into());
                    refresh_plugin_page(st);
                },
                Err(error) => set_status_toast(st, error),
            }
        },
        "rollback" => match st.plugins.rollback_local(
            key,
            None,
            st.cfg.plugin_require_signature,
            keep,
        ) {
            Ok(report) => {
                let message = if lang == "zh" {
                    format!(
                        "{} 已回滚 {} → {}",
                        report.id, report.from_version, report.to_version
                    )
                } else {
                    format!(
                        "{} rolled back {} → {}",
                        report.id, report.from_version, report.to_version
                    )
                };
                set_status_toast(st, message);
                refresh_plugin_page(st);
            },
            Err(error) => set_status_toast(st, error),
        },
        other => eprintln!("[ui] 未知插件安装动作 {other}"),
    }
}

fn show_plugin_confirm(st: &mut AppState) {
    match &st.plugin_confirm {
        Some(confirm) => {
            let lang = st.cfg.lang();
            let permissions = if confirm.permissions.is_empty() {
                ui_text(lang, "No capabilities declared.", "未声明任何能力。").to_string()
            } else {
                confirm.permissions.join(", ")
            };
            st.ui.set_plugin_confirm_visible(true);
            st.ui
                .set_plugin_confirm_name(confirm.name.clone().into());
            st.ui
                .set_plugin_confirm_permissions(permissions.into());
        },
        None => {
            st.ui.set_plugin_confirm_visible(false);
            st.ui.set_plugin_confirm_name("".into());
            st.ui.set_plugin_confirm_permissions("".into());
        },
    }
}

fn refresh_plugin_approval_ui(st: &mut AppState) {
    match st.plugin_approval.clone() {
        Some(approval) => {
            st.ui.set_plugin_approval_visible(true);
            st.ui.set_plugin_approval_plugin(
                format!("{} · {}", approval.plugin_name, approval.method).into(),
            );
            st.ui
                .set_plugin_approval_summary(approval.summary.clone().into());
            st.ui
                .set_plugin_approval_details(approval.details.clone().into());
            st.ui
                .set_plugin_approval_seconds(approval.seconds_left() as i32);
        },
        None => {
            st.ui.set_plugin_approval_visible(false);
            st.ui.set_plugin_approval_plugin("".into());
            st.ui.set_plugin_approval_summary("".into());
            st.ui.set_plugin_approval_details("".into());
            st.ui.set_plugin_approval_seconds(0);
        },
    }
}

/// 声明式 UI 文档 -> 宿主固定元素模型（text/markdown/list/input/button/
/// select/progress）。插件不能提供布局/样式/任意控件。
fn update_plugin_surface_model(st: &mut AppState) {
    let Some(surface) = st.plugin_surface.as_ref() else {
        st.ui.set_plugin_surface_open(false);
        st.plugin_surface_model.set_vec(Vec::new());
        return;
    };
    let title = if surface.doc.title.trim().is_empty() {
        format!("{} · {}", surface.plugin_name, surface.surface)
    } else {
        surface.doc.title.clone()
    };
    let elements: Vec<PluginUiElement> = surface
        .doc
        .elements
        .iter()
        .map(|element| match element {
            plugin_v3::SurfaceElement::Text { text } => PluginUiElement {
                kind: "text".into(),
                id: "".into(),
                label: "".into(),
                text: text.clone().into(),
                value: "".into(),
                placeholder: "".into(),
                progress: 0.0,
            },
            plugin_v3::SurfaceElement::Markdown { text } => PluginUiElement {
                kind: "markdown".into(),
                id: "".into(),
                label: "".into(),
                text: plugin_v3::render_markdown_plain(text).into(),
                value: "".into(),
                placeholder: "".into(),
                progress: 0.0,
            },
            plugin_v3::SurfaceElement::List { items } => PluginUiElement {
                kind: "list".into(),
                id: "".into(),
                label: "".into(),
                text: items.join("\n").into(),
                value: "".into(),
                placeholder: "".into(),
                progress: 0.0,
            },
            plugin_v3::SurfaceElement::Input {
                id,
                label,
                value,
                placeholder,
            } => PluginUiElement {
                kind: "input".into(),
                id: id.clone().into(),
                label: label.clone().into(),
                text: "".into(),
                value: value.clone().into(),
                placeholder: placeholder.clone().into(),
                progress: 0.0,
            },
            plugin_v3::SurfaceElement::Button { id, label } => PluginUiElement {
                kind: "button".into(),
                id: id.clone().into(),
                label: label.clone().into(),
                text: "".into(),
                value: "".into(),
                placeholder: "".into(),
                progress: 0.0,
            },
            plugin_v3::SurfaceElement::Select {
                id,
                label,
                value,
                options,
            } => PluginUiElement {
                kind: "select".into(),
                id: id.clone().into(),
                label: label.clone().into(),
                text: options.join(" · ").into(),
                value: value.clone().into(),
                placeholder: "".into(),
                progress: 0.0,
            },
            plugin_v3::SurfaceElement::Progress { label, value } => PluginUiElement {
                kind: "progress".into(),
                id: "".into(),
                label: label.clone().into(),
                text: "".into(),
                value: "".into(),
                placeholder: "".into(),
                progress: *value,
            },
        })
        .collect();
    st.plugin_surface_model.set_vec(elements);
    st.ui.set_plugin_surface_title(title.into());
    st.ui.set_plugin_surface_open(true);
    st.ui.set_plugin_surface_loading(false);
    st.ui.set_plugin_surface_error("".into());
}

fn close_plugin_surface(st: &mut AppState) {
    st.plugin_surface = None;
    st.plugin_surface_model.set_vec(Vec::new());
    st.ui.set_plugin_surface_open(false);
    st.ui.set_plugin_surface_title("".into());
    st.ui.set_plugin_surface_error("".into());
    st.ui.set_plugin_surface_loading(false);
}

fn open_plugin_surface(st: &mut AppState, key: &str, kind: &str) {
    let Some(row) = st.plugins.rows().into_iter().find(|row| row.key == key) else {
        return;
    };
    if !row.supported || !row.surface_ready {
        set_status_toast(
            st,
            ui_text(
                st.cfg.lang(),
                "Plugin is not enabled or permissions are not confirmed.",
                "插件未启用或权限未确认。",
            )
            .to_string(),
        );
        return;
    }
    let entry = st
        .plugins
        .entries()
        .iter()
        .find(|entry| entry.key == key)
        .cloned();
    let Some(manifest) = entry.and_then(|entry| entry.manifest) else {
        return;
    };
    let (surface, panel, surface_kind) = if kind == "settings" {
        ("settings".to_string(), None, "settings")
    } else if let Some(panel) = manifest.contributes.panels.first().cloned() {
        (format!("panel:{panel}"), Some(panel), "panel")
    } else {
        set_status_toast(st, "Plugin has no panel contribution".to_string());
        return;
    };
    st.plugin_surface = Some(PluginSurfaceState {
        plugin_id: manifest.id.clone(),
        plugin_name: manifest.name.clone(),
        surface: surface.clone(),
        panel: panel.clone(),
        doc: plugin_v3::SurfaceDoc {
            title: String::new(),
            elements: Vec::new(),
        },
    });
    st.plugin_surface_model.set_vec(Vec::new());
    st.ui.set_plugin_surface_title(
        format!("{} · {}", manifest.name, surface).into(),
    );
    st.ui.set_plugin_surface_open(true);
    st.ui.set_plugin_surface_loading(true);
    st.ui.set_plugin_surface_error("".into());
    if let Err(error) = st.plugins.request_surface_for(
        &manifest.id,
        surface_kind,
        panel,
        Some(st.window_id.get()),
    ) {
        st.ui.set_plugin_surface_loading(false);
        st.ui.set_plugin_surface_error(error.clone().into());
        set_status_toast(st, error);
    }
}

/// 插件 -> 宿主 request 的 UI 侧执行：terminal/clipboard 的实际读写只能由
/// 持有 `panes`/`arboard` 的 UI 线程完成。宿主不会读取 Vault/env 明文。
fn resolve_plugin_approval(st: &mut AppState, allow: bool) {
    let Some(approval) = st.plugin_approval.take() else {
        return;
    };
    refresh_plugin_approval_ui(st);
    // Allow/Deny 必须由请求窗口处理；别的窗口不能代为执行 terminal/clipboard。
    if approval
        .window_id
        .is_some_and(|window| window != st.window_id.get())
    {
        eprintln!(
            "[plugin-v3] {}: 忽略 {} 的 ask（请求窗口 {:?}，当前 {}）",
            approval.plugin_id, approval.method, approval.window_id, st.window_id
        );
        st.plugins.resolve_host_request(
            &approval.plugin_id,
            approval.request_id,
            plugin_v3::HostOutcome::Unavailable(
                "approval belongs to another window".to_string(),
            ),
        );
        return;
    }
    let params = approval.params.clone();
    // stale ask：插件已停止/已撤销/已超时，绝不执行 terminal/clipboard 副作用。
    let still_pending = st
        .plugins
        .pending_approvals()
        .iter()
        .any(|pending| {
            pending.plugin_id == approval.plugin_id
                && pending.request_id == approval.request_id
        })
        && approval.seconds_left() > 0;
    if !still_pending || st.plugins.phase(&approval.plugin_id) != Some(plugin_v3::Phase::Running) {
        let _ = st.plugins.resolve_host_request(
            &approval.plugin_id,
            approval.request_id,
            plugin_v3::HostOutcome::Unavailable(
                "request is no longer pending or plugin is not running".to_string(),
            ),
        );
        return;
    }
    if !allow {
        st.plugins.resolve_host_request(
            &approval.plugin_id,
            approval.request_id,
            plugin_v3::HostOutcome::Denied,
        );
        eprintln!(
            "[plugin-v3] {}: user denied {}",
            approval.plugin_id, approval.method
        );
        return;
    }
    let outcome = match approval.method.as_str() {
        "terminal.read" => match plugin_terminal_tail(st, &params) {
            Ok(value) => {
                eprintln!(
                    "[plugin-v3] {}: terminal.read applied window={} bytes={}",
                    approval.plugin_id,
                    st.window_id,
                    value
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::len)
                        .unwrap_or(0)
                );
                plugin_v3::HostOutcome::Ok(value)
            },
            Err(error) => plugin_v3::HostOutcome::Unavailable(error),
        },
        "terminal.write" => {
            let text = params
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let Some(pane) = st.ws.active_tab().map(|tab| tab.focused) else {
                st.plugins.resolve_host_request(
                    &approval.plugin_id,
                    approval.request_id,
                    plugin_v3::HostOutcome::Unavailable("没有焦点终端".to_string()),
                );
                return;
            };
            let Some(runtime) = st.panes.get(&pane) else {
                st.plugins.resolve_host_request(
                    &approval.plugin_id,
                    approval.request_id,
                    plugin_v3::HostOutcome::Unavailable("焦点终端不可用".to_string()),
                );
                return;
            };
            runtime.handle.input(text.as_bytes());
            eprintln!(
                "[plugin-v3] {}: terminal.write applied window={} bytes={}",
                approval.plugin_id,
                st.window_id,
                text.len()
            );
            plugin_v3::HostOutcome::Ok(serde_json::json!({}))
        },
        "clipboard.read" => match with_clipboard(st, |clipboard| clipboard.get_text()) {
            Some(Ok(text)) => {
                let text = plugin_v3::sanitize_text(&text, plugin_v3::MAX_CLIPBOARD_CHARS);
                plugin_v3::HostOutcome::Ok(serde_json::json!({ "text": text }))
            },
            Some(Err(error)) => {
                plugin_v3::HostOutcome::Unavailable(format!("读取剪贴板失败: {error}"))
            },
            None => plugin_v3::HostOutcome::Unavailable("剪贴板不可用".to_string()),
        },
        "clipboard.write" => {
            let text = params
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            match with_clipboard(st, |clipboard| clipboard.set_text(text)) {
                Some(Ok(())) => plugin_v3::HostOutcome::Ok(serde_json::json!({})),
                Some(Err(error)) => {
                    plugin_v3::HostOutcome::Unavailable(format!("写入剪贴板失败: {error}"))
                },
                None => plugin_v3::HostOutcome::Unavailable("剪贴板不可用".to_string()),
            }
        },
        other => plugin_v3::HostOutcome::Error(format!("不支持的授权请求 {other}")),
    };
    st.plugins
        .resolve_host_request(&approval.plugin_id, approval.request_id, outcome);
}

/// 从焦点终端取尾部文本；先按行再按字节限制，绝不返回 cwd/env/Vault。
fn plugin_terminal_tail(st: &AppState, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    let Some(pane) = st.ws.active_tab().map(|tab| tab.focused) else {
        return Err("没有焦点终端".to_string());
    };
    let Some(text) = st.panes.get(&pane).and_then(|runtime| runtime.handle.contents()) else {
        return Err("读取终端内容失败".to_string());
    };
    let max_lines = params
        .get("max_lines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(80)
        .clamp(1, 500) as usize;
    let max_bytes = params
        .get("max_bytes")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(32 * 1024)
        .clamp(1, 64 * 1024) as usize;
    let total_lines = text.lines().count();
    let truncated_lines = total_lines > max_lines;
    let mut tail = text
        .lines()
        .rev()
        .take(max_lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let truncated_bytes = tail.len() > max_bytes;
    if truncated_bytes {
        let mut cut = tail.len() - max_bytes;
        while cut < tail.len() && !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail = tail[cut..].to_string();
    }
    Ok(serde_json::json!({
        "text": tail,
        "truncated": truncated_lines || truncated_bytes,
        "max_lines": max_lines,
        "max_bytes": max_bytes,
    }))
}

/// 活动列表变化时刷新 Slint model；`force` 用于首次填充。
///
/// 后台线程只往 `PluginStore` 的 ActivityLog push 并唤醒 UI；UI 线程按
/// revision 拉取，避免跨线程碰 Slint model。400ms 兜底仍会调用（revision
/// 未变时直接返回）。
fn refresh_plugin_activity(st: &mut AppState, force: bool) {
    let (revision, lines) = st.plugins.activity_snapshot();
    if !force && revision == st.plugin_activity_revision {
        return;
    }
    st.plugin_activity_revision = revision;
    let lang = st.cfg.lang();
    let rows: Vec<slint::SharedString> = lines
        .iter()
        .map(|line| localize_plugin_activity(line, lang).into())
        .collect();
    let row_count = rows.len();
    st.plugin_activity_model.set_vec(rows);
    // rev=0 是首次空表填充，不打日志；有活动时记录 UI 实际刷新时间，
    // 供 e2e 与插件自己的完成时间戳对差（目标 <50ms）。
    if revision > 0 {
        eprintln!(
            "[ui] ts_ms={} plugin activity rev={revision} rows={row_count}",
            logging::now_ms()
        );
    }
}

/// 插件 UI 汇总刷新：提示浮层、状态栏、设置页行、声明式 surface、ask 确认、
/// pane-focus 事件。所有操作非阻塞；后台线程只写队列 + 唤醒 UI。
fn refresh_plugin_ui(st: &mut AppState) {
    // context.config_metadata 只包含这些非敏感字段；不读取环境变量/Vault。
    sync_plugin_config_metadata(st);
    // supervisor 解析“缺省 window_id=active”用；registry 不在时保持上次值。
    let active_window = runtime::registry()
        .and_then(|registry| registry.borrow().active())
        .map(|id| id.get());
    st.plugins.set_active_window(active_window);
    let window_id = st.window_id.get();
    // 活动列表按 revision 增量刷新（后台线程 push 后唤醒）。
    refresh_plugin_activity(st, false);

    // 命令结果 / 插件 ui.notify：只取目标窗口；其它窗口的留在队列里。
    let notices = st.plugins.take_notices_for(window_id, active_window);
    if let Some(last) = notices.last() {
        let prefix = match last.level.as_str() {
            "error" => "error: ",
            "warn" => "warn: ",
            _ => "",
        };
        let mut msg = format!("{}: {}{}", last.plugin_name, prefix, last.text);
        if msg.chars().count() > 320 {
            msg = msg.chars().take(319).collect::<String>() + "…";
        }
        if last.level == "error" && st.ui.get_plugin_surface_loading() {
            st.ui.set_plugin_surface_loading(false);
            st.ui
                .set_plugin_surface_error(msg.clone().into());
        }
        eprintln!(
            "[plugin-v3] notice window={} plugin={} level={} chars={}",
            st.window_id,
            last.plugin_id,
            last.level,
            last.text.chars().count()
        );
        set_status_toast(st, msg);
    }

    // 状态栏：只显示全局状态 + 本窗口专属 ui.status。
    let (status_rev, items) = st.plugins.status_snapshot_for_window(window_id);
    if status_rev != st.plugin_status_revision {
        st.plugin_status_revision = status_rev;
        let text = items
            .iter()
            .map(|item| format!("{}: {}", item.plugin_name, item.text))
            .collect::<Vec<_>>()
            .join(" · ");
        let tooltip = items
            .iter()
            .find(|item| !item.tooltip.is_empty())
            .map(|item| item.tooltip.clone())
            .unwrap_or_default();
        st.plugin_status_command = items
            .iter()
            .find_map(|item| item.command.clone());
        if !text.is_empty() {
            eprintln!(
                "[plugin-v3] status window={} text={:?}",
                st.window_id, text
            );
        }
        st.ui.set_plugin_status(text.as_str().into());
        st.ui.set_plugin_status_tooltip(tooltip.as_str().into());
    }

    // 运行状态（running/restarting/disabled: 原因）变化时刷新 Plugins 设置页。
    let runtime_rev = st.plugins.runtime_revision();
    if runtime_rev != st.plugin_runtime_revision {
        st.plugin_runtime_revision = runtime_rev;
        refresh_plugin_page(st);
        if let Some(surface) = &st.plugin_surface {
            let plugin_id = surface.plugin_id.clone();
            let running = st.plugins.phase(&plugin_id) == Some(plugin_v3::Phase::Running);
            if !running && st.ui.get_plugin_surface_loading() {
                st.ui.set_plugin_surface_loading(false);
                st.ui.set_plugin_surface_error(
                    "Plugin is not running; the declarative UI is unavailable.".into(),
                );
            }
        }
    }

    // 声明式 surface 更新：两级 key 是 (window_id, surface)；只接受本窗口
    // 且当前打开的那个 (plugin, surface)。插件 push 到没有打开 surface 的
    // 显式目标窗口时，在该窗口显示（不会覆盖别的窗口）。
    for update in st.plugins.take_surfaces_for(window_id, active_window) {
        let matches = st.plugin_surface.as_ref().is_some_and(|surface| {
            surface.plugin_id == update.plugin_id && surface.surface == update.surface
        });
        if matches {
            if let Some(surface) = st.plugin_surface.as_mut() {
                surface.doc = update.doc;
                surface.panel = update.panel;
                surface.plugin_name = update.plugin_name;
            }
            update_plugin_surface_model(st);
            continue;
        }
        if update.pushed
            && update.window_id == Some(window_id)
            && st.plugin_surface.is_none()
        {
            eprintln!(
                "[plugin-v3] {} surface={} window={} opened by ui.render",
                update.plugin_id, update.surface, st.window_id
            );
            st.plugin_surface = Some(PluginSurfaceState {
                plugin_id: update.plugin_id,
                plugin_name: update.plugin_name,
                surface: update.surface,
                panel: update.panel,
                doc: update.doc,
            });
            st.ui.set_plugin_surface_loading(false);
            st.ui.set_plugin_surface_error("".into());
            update_plugin_surface_model(st);
        }
    }

    // terminal/clipboard ask：只展示本窗口的请求；Allow/Deny 路由回同一窗口。
    let approvals = st
        .plugins
        .pending_approvals_for(window_id, active_window);
    if let Some(current) = &st.plugin_approval {
        let still_pending = approvals.iter().any(|approval| {
            approval.plugin_id == current.plugin_id
                && approval.request_id == current.request_id
        });
        if !still_pending || current.seconds_left() == 0 {
            st.plugin_approval = None;
        }
    }
    if st.plugin_approval.is_none() {
        if let Some(approval) = approvals
            .into_iter()
            .find(|approval| approval.seconds_left() > 0)
        {
            eprintln!(
                "[plugin-v3] {}: ask {} shown in window={window_id}",
                approval.plugin_id, approval.method
            );
            st.plugin_approval = Some(approval);
        }
    }
    refresh_plugin_approval_ui(st);

    // pane-focus 事件：只在焦点实际变化时投递；payload 只有不透明 pane id。
    let focus = st.ws.active_tab().map(|tab| tab.focused);
    if focus != st.last_plugin_focus {
        let previous = st.last_plugin_focus;
        st.last_plugin_focus = focus;
        if previous.is_some() {
            if let Some(pane) = focus {
                dispatch_plugin_event(
                    st,
                    plugin_v3::PluginEvent::PaneFocus,
                    plugin_v3::pane_focus_payload(pane),
                );
            }
        }
    }
}

/// 异步分发一个插件事件；失败只写日志，不影响终端。
fn dispatch_plugin_event(
    st: &mut AppState,
    event: plugin_v3::PluginEvent,
    payload: serde_json::Value,
) {
    // 窗口上下文占位：v3 协议 additive 字段，旧插件忽略即可；新插件应读取
    // `window_id`，不能靠宿主“当前窗口”猜测。命令/terminal.read 的 window_id
    // 路由在下一阶段接入 supervisor 请求参数。
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "window_id".to_string(),
            serde_json::json!(st.window_id.get()),
        );
    }
    let _ = st.plugins.spawn_event(event, &payload);
}

/// Settings → Plugins 的用户操作：生命周期不重启 Tervo。
fn plugin_action(st: &mut AppState, action: &str, key: &str) {
    let lang = st.cfg.lang();
    match action {
        "enable" => match st.plugins.request_enable(key) {
            Ok(plugin_v3::EnableDecision::Approved) => {
                set_status_toast(st, ui_text(lang, "Plugin enabled.", "插件已启用。").to_string());
                st.plugin_confirm = None;
                show_plugin_confirm(st);
            },
            Ok(plugin_v3::EnableDecision::NeedsApproval { permissions }) => {
                let row = st.plugins.rows().into_iter().find(|row| row.key == key);
                let name = row
                    .as_ref()
                    .map(|row| row.name.clone())
                    .unwrap_or_else(|| key.to_string());
                st.plugin_confirm = Some(PluginConfirmState {
                    key: key.to_string(),
                    name,
                    permissions,
                });
                show_plugin_confirm(st);
            },
            Err(error) => set_status_toast(st, error),
        },
        "disable" => match st.plugins.disable_plugin(key) {
            Ok(()) => set_status_toast(
                st,
                ui_text(lang, "Plugin disabled.", "插件已禁用。").to_string(),
            ),
            Err(error) => set_status_toast(st, error),
        },
        "reload" => match st.plugins.reload_plugin(key) {
            Ok(()) => set_status_toast(
                st,
                ui_text(lang, "Plugin reloaded from disk.", "插件已从磁盘重载。").to_string(),
            ),
            Err(error) => set_status_toast(st, error),
        },
        "restart" => match st.plugins.restart_plugin(key) {
            Ok(()) => set_status_toast(
                st,
                ui_text(lang, "Plugin restarted.", "插件已重启。").to_string(),
            ),
            Err(error) => set_status_toast(st, error),
        },
        "folder" => match st.plugins.open_folder(key) {
            Ok(dir) => {
                eprintln!("[plugin-v3] open folder {}", dir.display());
                set_status_toast(
                    st,
                    ui_text(lang, "Opened plugin folder.", "已打开插件目录。").to_string(),
                );
            },
            Err(error) => set_status_toast(st, error),
        },
        other => eprintln!("[ui] 未知插件动作 {other}"),
    }
    if matches!(action, "disable" | "reload")
        && st
            .plugin_surface
            .as_ref()
            .is_some_and(|surface| surface.plugin_id == key)
    {
        close_plugin_surface(st);
    }
    refresh_plugin_page(st);
}

fn confirm_plugin_enable(st: &mut AppState) {
    let Some(confirm) = st.plugin_confirm.take() else {
        return;
    };
    match st.plugins.confirm_enable(&confirm.key) {
        Ok(()) => {
            eprintln!(
                "[ui] plugin {} permissions confirmed: [{}]",
                confirm.key,
                confirm.permissions.join(", ")
            );
            set_status_toast(
                st,
                ui_text(
                    st.cfg.lang(),
                    "Permissions confirmed; plugin enabled.",
                    "权限已确认，插件已启用。",
                )
                .to_string(),
            );
        },
        Err(error) => set_status_toast(st, error),
    }
    show_plugin_confirm(st);
    refresh_plugin_page(st);
}

fn cancel_plugin_enable(st: &mut AppState) {
    st.plugin_confirm = None;
    show_plugin_confirm(st);
}

/// 声明式 UI 的 button/select 动作。select 的 next 值由宿主从文档计算后发给
/// 插件，插件仍然拥有“接受/拒绝”的最终决定权（可回 ui.render 更新）。
fn plugin_surface_action(st: &mut AppState, element_id: &str) {
    let Some(surface) = st.plugin_surface.as_mut() else {
        return;
    };
    let (action, value) = match surface.doc.element(element_id) {
        Some(plugin_v3::SurfaceElement::Button { .. }) => ("click", None),
        Some(plugin_v3::SurfaceElement::Select { .. }) => {
            let next = surface.doc.next_select_value(element_id);
            if let Some(next) = &next {
                if let Some(element) = surface
                    .doc
                    .elements
                    .iter_mut()
                    .find(|element| element.id() == Some(element_id))
                {
                    if let plugin_v3::SurfaceElement::Select { value, .. } = element {
                        *value = next.clone();
                    }
                }
            }
            ("change", next)
        },
        _ => return,
    };
    let plugin_id = surface.plugin_id.clone();
    let surface_kind = surface.surface.clone();
    let panel = surface.panel.clone();
    st.plugins.send_ui_action_for(
        &plugin_id,
        &surface_kind,
        panel,
        Some(st.window_id.get()),
        element_id,
        action,
        value,
    );
    update_plugin_surface_model(st);
}

/// input 的 change 只发给插件，不重建 Slint model（避免打断光标/输入法）。
fn plugin_surface_input(st: &mut AppState, element_id: &str, value: String) {
    let Some(surface) = st.plugin_surface.as_mut() else {
        return;
    };
    let mut updated = false;
    for element in &mut surface.doc.elements {
        if let plugin_v3::SurfaceElement::Input { id, value: current, .. } = element {
            if id == element_id {
                *current = value.clone();
                updated = true;
                break;
            }
        }
    }
    if !updated {
        return;
    }
    let plugin_id = surface.plugin_id.clone();
    let surface_kind = surface.surface.clone();
    let panel = surface.panel.clone();
    st.plugins.send_ui_action_for(
        &plugin_id,
        &surface_kind,
        panel,
        Some(st.window_id.get()),
        element_id,
        "change",
        Some(value),
    );
}

fn hotkey_title(reg: &commands::Registry, lang: &str, id: &str) -> String {
    reg.get(id)
        .map(|c| command_title(lang, c.id, c.title).to_string())
        .unwrap_or_else(|| id.to_string())
}

/// 命令层只产出中文的快捷键校验错误；英文界面下做最小映射，
/// 解析器/底层错误保持原样（不伪装成已翻译）。
fn localize_hotkey_error(lang: &str, err: String) -> String {
    if lang != "en" {
        return err;
    }
    if let Some(rest) = err.strip_prefix("冲突：") {
        return format!("Conflict: {rest}");
    }
    if let Some(rest) = err.strip_prefix("未知命令: ") {
        return format!("Unknown command: {rest}");
    }
    if err.starts_with("快捷键需要") {
        return "Shortcut requires Ctrl/Alt/Meta, or F1–F24".to_string();
    }
    err
}

/// 进入录制：先让终端 IME 宿主拿回焦点，否则按键会落在设置搜索框里。
fn start_hotkey_edit(st: &mut AppState, index: i32) {
    let Some(id) = st.hotkey_ids.get(index.max(0) as usize).copied() else {
        return;
    };
    let title = hotkey_title(&st.commands, st.cfg.lang(), id);
    st.hotkey_recording = Some(id);
    st.ui.set_hotkey_error("".into());
    let status = if st.cfg.lang() == "zh" {
        format!("录制「{title}」：按下新的组合键")
    } else {
        format!("Recording \"{title}\": press a new chord")
    };
    st.ui.set_hotkey_status(status.into());
    st.ui.set_hotkey_recording_row(index.max(0));
    st.ui.invoke_focus_terminal();
    eprintln!("[hotkey] record start {id}");
}

fn cancel_hotkey_edit(st: &mut AppState) {
    if st.hotkey_recording.take().is_some() {
        st.ui
            .set_hotkey_status(ui_text(st.cfg.lang(), "Recording cancelled", "已取消录制").into());
        eprintln!("[hotkey] record cancel");
    }
    st.ui.set_hotkey_error("".into());
    st.ui.set_hotkey_recording_row(-1);
    st.ui.invoke_focus_terminal();
}

fn reset_hotkey(st: &mut AppState, index: i32) {
    let Some(id) = st.hotkey_ids.get(index.max(0) as usize).copied() else {
        return;
    };
    let title = hotkey_title(&st.commands, st.cfg.lang(), id);
    let removed = commands::clear_override(&mut st.cfg.hotkeys, id);
    if removed {
        st.cfg.save();
    }
    if st.hotkey_recording == Some(id) {
        st.hotkey_recording = None;
        st.ui.invoke_focus_terminal();
    }
    st.ui.set_hotkey_error("".into());
    let status = if st.cfg.lang() == "zh" {
        format!("{title} 已恢复默认")
    } else {
        format!("{title} reset to default")
    };
    st.ui.set_hotkey_status(status.into());
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
    let status = if st.cfg.lang() == "zh" {
        format!("已重置 {count} 条自定义快捷键")
    } else {
        format!("Reset {count} custom shortcuts")
    };
    st.ui.set_hotkey_status(status.into());
    st.ui.invoke_focus_terminal();
    refresh_hotkeys(st);
    eprintln!("[hotkey] reset all count={count}");
}

/// 录制中的按键：Esc 取消、Backspace/Delete 恢复默认，其余组合保存。
///
/// 冲突/非法组合只显示错误并保持录制，方便用户直接再按一个；不会触发任何命令。
fn record_hotkey_key(st: &mut AppState, text: &str, ctrl: bool, alt: bool, shift: bool) {
    let Some(id) = st.hotkey_recording else {
        return;
    };
    if !ctrl && !alt && !shift {
        if text == "\u{1b}" {
            cancel_hotkey_edit(st);
            return;
        }
        if text == "\u{8}" || text == "\u{7f}" {
            let title = hotkey_title(&st.commands, st.cfg.lang(), id);
            let removed = commands::clear_override(&mut st.cfg.hotkeys, id);
            if removed {
                st.cfg.save();
            }
            st.hotkey_recording = None;
            st.ui.set_hotkey_error("".into());
            let status = if st.cfg.lang() == "zh" {
                format!("{title} 已恢复默认")
            } else {
                format!("{title} reset to default")
            };
            st.ui.set_hotkey_status(status.into());
            refresh_hotkeys(st);
            st.ui.invoke_focus_terminal();
            eprintln!("[hotkey] record clear {id} removed={removed}");
            return;
        }
    }
    let Some(chord) = keys::chord_from_key_event(text, ctrl, alt, shift) else {
        return;
    };
    let canonical = chord.canonical();
    match st
        .commands
        .validate_override(id, &canonical, &st.cfg.hotkeys)
    {
        Ok(chord) => {
            let title = hotkey_title(&st.commands, st.cfg.lang(), id);
            commands::set_override(&mut st.cfg.hotkeys, id, &chord);
            st.cfg.save();
            st.hotkey_recording = None;
            st.ui.set_hotkey_error("".into());
            let status = if st.cfg.lang() == "zh" {
                format!("{title} → {}（已保存）", chord)
            } else {
                format!("{title} → {} (saved)", chord)
            };
            st.ui.set_hotkey_status(status.into());
            refresh_hotkeys(st);
            st.ui.invoke_focus_terminal();
            eprintln!("[hotkey] saved {id} = {canonical}");
        }
        Err(err) => {
            eprintln!("[hotkey] rejected {id} {canonical:?}: {err}");
            let err = localize_hotkey_error(st.cfg.lang(), err);
            st.ui.set_hotkey_error(err.into());
        }
    }
}

fn theme_choices(reg: &ThemeRegistry) -> ModelRc<ThemeChoice> {
    let choices: Vec<ThemeChoice> = reg
        .all()
        .map(|t| ThemeChoice {
            id: t.id.as_str().into(),
            label: t.label.as_str().into(),
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(choices)))
}

/// 设置面板高度：12 套内置主题 3 行；每多一行加 30px。

/// 下发实际生效的主题（不改配置与选中项；auto 监听回调走这里）。
fn push_theme(st: &mut AppState, theme: &ThemeData) {
    st.resolved_theme = theme.clone();
    let auto = is_auto_theme(&st.cfg.theme);
    st.ui.set_auto_resolved(if auto { theme.label.as_str().into() } else { "".into() });
    st.ui.set_theme_display(if auto {
        format!(
            "{} · {}",
            ui_text(st.cfg.lang(), "Auto", "自动"),
            theme.label
        )
        .into()
    } else {
        theme.label.as_str().into()
    });
    st.ui.global::<Palette>().set_colors(slint_theme(&theme.ui));
    // 优先级：Profile 显式 theme > 全局 theme。Profile 选 auto 时跟随系统/全局
    // 的深色侧设置重新解析；未知 id 已在 theme_for_selection 里回退全局。
    let registry = &st.registry;
    let cfg = &st.cfg;
    let bold = (cfg.bold_is_bright, cfg.bold_color_rgb(), cfg.bold_color_same_as_fg);
    for rt in st.panes.values_mut() {
        let pane_theme = rt
            .appearance
            .theme
            .as_deref()
            .and_then(|sel| theme_for_selection(registry, cfg, Some(sel)))
            .unwrap_or_else(|| theme.clone());
        rt.renderer
            .set_theme(pane_theme.spec.bg, pane_theme.spec.cursor);
        rt.handle
            .set_theme(pane_theme.palette().with_bold(bold.0, bold.1, bold.2));
    }
    redraw_visible(st);
}

/// 无背景图时强制不透明：opacity 只在与背景图混合时才有意义。
/// 这样旧配置里残留的低 opacity 不会让纯色终端发灰，也满足“默认无图不透明”。
fn renderer_opacity(st: &AppState) -> f32 {
    // 无背景图时也允许半透明：终端帧的 alpha 直接透出桌面。是否真的透明由
    // `terminal_transparent` 控制窗口/ pane 背景；这里只负责帧 alpha。
    config::clamp_opacity(st.cfg.background_opacity)
}

/// 把“当前是否真的显示背景图”对应的 alpha 下发给所有 pane 并重绘。
/// 背景图加载/清除后必须调用，否则渲染器还停留在旧的 alpha。
fn sync_renderer_opacity(st: &mut AppState) {
    let value = renderer_opacity(st);
    // 有图时终端层本来就透出背景图；无图时只有 opacity < 1 才需要透明窗口。
    let transparent = st.ui.get_has_background_image() || value < 0.999;
    st.ui.set_terminal_transparent(transparent);
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
fn localize_background_error(lang: &str, err: String) -> String {
    if lang != "en" {
        return err;
    }
    if let Some(rest) = err.strip_prefix("不是普通文件：") {
        return format!("Not a regular file: {rest}");
    }
    if let Some(rest) = err.strip_prefix("文件不存在：") {
        return format!("File does not exist: {rest}");
    }
    if let Some(rest) = err.strip_prefix("无法访问 ") {
        return format!("Cannot access {rest}");
    }
    err
}

fn apply_background_image(st: &mut AppState, raw: &str) -> Result<(), String> {
    match config::resolve_background_image(raw) {
        Ok(None) => {
            st.ui.set_background_image(slint::Image::default());
            st.ui.set_has_background_image(false);
            st.ui.set_background_image_error("".into());
            sync_renderer_opacity(st);
            Ok(())
        }
        Ok(Some(path)) => match slint::Image::load_from_path(&path) {
            Ok(image) => {
                st.ui.set_background_image(image);
                st.ui.set_has_background_image(true);
                st.ui.set_background_image_error("".into());
                sync_renderer_opacity(st);
                eprintln!("[theme] 背景图 {} 已加载", path.display());
                Ok(())
            }
            Err(_) => Err(if st.cfg.lang() == "zh" {
                format!("无法解码图片（支持 PNG/JPEG/SVG）：{}", path.display())
            } else {
                format!(
                    "Cannot decode image (PNG/JPEG/SVG supported): {}",
                    path.display()
                )
            }),
        },
        Err(e) => Err(localize_background_error(st.cfg.lang(), e)),
    }
}

/// 打开系统文件选择器选择背景图。返回：
/// - `Ok(Some(path))`：用户选中；
/// - `Ok(None)`：用户取消；
/// - `Err(msg)`：zenity/kdialog 不可用或启动失败。
///
/// 不拼 shell；只把固定参数传给 picker。`TERVO_FILE_DIALOG` 可覆盖可执行文件，
/// 便于无桌面测试使用 fake picker。
fn pick_background_image(initial: &str) -> Result<Option<String>, String> {
    let exe = std::env::var("TERVO_FILE_DIALOG").unwrap_or_else(|_| "zenity".to_string());
    let mut cmd = std::process::Command::new(&exe);
    cmd.args([
        "--file-selection",
        "--title=Tervo — choose a background image",
        "--file-filter=Images | *.png *.jpg *.jpeg *.svg *.webp *.bmp",
    ]);
    let initial = config::expand_tilde(initial.trim());
    let start = if initial.is_dir() {
        initial
    } else {
        initial.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    if !start.as_os_str().is_empty() {
        let filename_arg = format!("--filename={}/", start.display());
        cmd.arg(filename_arg);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("无法启动文件选择器 {exe}: {e}（可直接输入路径）"))?;
    if !out.status.success() {
        // zenity 取消返回 1；其它非 0 也按取消处理，不把 stderr 当错误弹给用户。
        return Ok(None);
    }
    let path = String::from_utf8(out.stdout)
        .map_err(|_| "文件选择器返回了非 UTF-8 路径".to_string())?
        .trim()
        .to_string();
    if path.is_empty() {
        Ok(None)
    } else {
        Ok(Some(path))
    }
}

/// 选择本地插件安装源：`kind = "folder"` 选目录，`"file"` 选 zip/tar 包。
/// 与背景图选择器一致：只传固定参数给 zenity，不拼 shell；`TERVO_FILE_DIALOG`
/// 可覆盖可执行文件，便于无桌面测试使用 fake picker。
fn pick_plugin_source(kind: &str, initial: &str) -> Result<Option<String>, String> {
    let exe = std::env::var("TERVO_FILE_DIALOG").unwrap_or_else(|_| "zenity".to_string());
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--file-selection");
    match kind {
        "folder" => {
            cmd.args(["--directory", "--title=Tervo — choose a plugin folder"]);
        },
        "file" => {
            cmd.args([
                "--title=Tervo — choose a plugin package",
                "--file-filter=Plugin packages | *.zip *.tar *.tgz *.tar.gz",
            ]);
        },
        other => return Err(format!("未知安装源类型 {other:?}")),
    }
    let initial = config::expand_tilde(initial.trim());
    let start = if initial.is_dir() {
        initial
    } else {
        initial.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    if !start.as_os_str().is_empty() {
        cmd.arg(format!("--filename={}/", start.display()));
    }
    let out = cmd
        .output()
        .map_err(|e| format!("无法启动文件选择器 {exe}: {e}"))?;
    if !out.status.success() {
        return Ok(None);
    }
    let path = String::from_utf8(out.stdout)
        .map_err(|_| "文件选择器返回了非 UTF-8 路径".to_string())?
        .trim()
        .to_string();
    if path.is_empty() {
        Ok(None)
    } else {
        Ok(Some(path))
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
                eprintln!("[theme] 首次启用背景图，opacity 自动设为 {FIRST_BACKGROUND_OPACITY:.2}");
            }
            st.cfg.save();
            eprintln!(
                "[theme] background_image={raw} opacity={:.2}",
                st.cfg.background_opacity
            );
            true
        }
        Err(e) => {
            if report_error {
                eprintln!("[theme] 背景图无效，保留当前背景: {e}");
                st.ui.set_background_image_error(e.into());
            } else {
                eprintln!("[theme] 背景图输入尚未生效: {e}");
            }
            false
        }
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

/// 应用主题选择（"auto" 或具体 id），但不写盘。
///
/// 返回 false 表示主题 id 不存在，调用方应保留原配置/旧主题。设置页保存和
/// 外部热更新共用这一段，避免两条路径的主题行为漂移。
fn install_theme_selection(st: &mut AppState, selected: &str) -> bool {
    let auto = is_auto_theme(selected);
    if auto {
        st.cfg.theme = AUTO_THEME.to_string();
    } else if let Some(found) = st.registry.get(selected) {
        st.cfg.theme = found.id.clone();
    } else {
        eprintln!("[theme] 未知主题 {selected:?}，保持当前配置");
        return false;
    }
    let theme = resolve_theme(
        &st.registry,
        &st.cfg.theme,
        st.cfg.auto_dark.as_deref(),
        st.cfg.auto_light.as_deref(),
    );
    st.ui.set_theme_name(if auto {
        AUTO_THEME.into()
    } else {
        theme.id.as_str().into()
    });
    push_theme(st, &theme);
    if auto {
        ensure_auto_watcher(st);
    }
    true
}

/// 应用用户选择的主题（"auto" 或具体 id），并写回配置。
fn apply_theme(st: &mut AppState, selected: &str) {
    if install_theme_selection(st, selected) {
        st.cfg.save();
    }
}

/// 外部配置热更新的字段分类。只用于日志与文档矩阵的同源核对；
/// 真正套用仍按字段逐项进行，JSON/TOML 里没写到的字段不会被“假装热更”。
#[derive(Debug, Default, PartialEq, Eq)]
struct ReconfigSummary {
    hot: Vec<&'static str>,
    new_pane: Vec<&'static str>,
    restart: Vec<&'static str>,
}

fn reconfig_summary(old: &Config, new: &Config) -> ReconfigSummary {
    let mut s = ReconfigSummary::default();
    let mut hot = |cond: bool, name: &'static str| {
        if cond {
            s.hot.push(name);
        }
    };
    hot(old.language != new.language, "language");
    hot(
        old.theme != new.theme
            || old.auto_dark != new.auto_dark
            || old.auto_light != new.auto_light,
        "theme",
    );
    hot(old.font_size != new.font_size, "font_size");
    hot(
        old.background_image != new.background_image,
        "background_image",
    );
    hot(
        old.background_opacity != new.background_opacity,
        "background_opacity",
    );
    hot(old.background_dim != new.background_dim, "background_dim");
    hot(old.visual_bell != new.visual_bell, "visual_bell");
    hot(old.audible_bell != new.audible_bell, "audible_bell");
    hot(
        old.desktop_notifications != new.desktop_notifications,
        "desktop_notifications",
    );
    hot(
        old.scroll_on_output != new.scroll_on_output,
        "scroll_on_output",
    );
    hot(
        old.scroll_on_keystroke != new.scroll_on_keystroke,
        "scroll_on_keystroke",
    );
    hot(
        old.scrollbar_policy != new.scrollbar_policy,
        "scrollbar_policy",
    );
    hot(old.sixel != new.sixel, "sixel");
    hot(
        old.sixel_scrolling != new.sixel_scrolling,
        "sixel_scrolling",
    );
    hot(old.iip != new.iip, "iip");
    hot(old.cursor_shape != new.cursor_shape, "cursor_shape");
    hot(old.cursor_blink != new.cursor_blink, "cursor_blink");
    hot(old.cursor_color != new.cursor_color, "cursor_color");
    hot(
        old.cursor_text_color != new.cursor_text_color,
        "cursor_text_color",
    );
    hot(old.bold_is_bright != new.bold_is_bright, "bold_is_bright");
    hot(old.bold_color != new.bold_color, "bold_color");
    hot(
        old.bold_color_same_as_fg != new.bold_color_same_as_fg,
        "bold_color_same_as_fg",
    );
    hot(
        old.right_click != new.right_click || old.right_click_paste != new.right_click_paste,
        "right_click",
    );
    hot(
        old.paste_on_middle_click != new.paste_on_middle_click,
        "paste_on_middle_click",
    );
    hot(
        old.confirm_multiline_paste != new.confirm_multiline_paste,
        "confirm_multiline_paste",
    );
    hot(old.copy_on_select != new.copy_on_select, "copy_on_select");
    hot(old.confirm_close != new.confirm_close, "confirm_close");
    hot(old.osc52 != new.osc52, "osc52");
    hot(
        old.tabbar_position != new.tabbar_position,
        "tabbar_position",
    );
    hot(
        old.backspace_binding != new.backspace_binding,
        "backspace_binding",
    );
    hot(old.delete_binding != new.delete_binding, "delete_binding");
    hot(old.close_action != new.close_action, "close_action");
    hot(old.hotkeys != new.hotkeys, "hotkeys");
    hot(old.quake_enabled != new.quake_enabled, "quake_enabled");
    hot(
        old.quake_width != new.quake_width || old.quake_height != new.quake_height,
        "quake_geometry",
    );

    if old.scrollback != new.scrollback || old.scrollback_unlimited != new.scrollback_unlimited {
        s.new_pane.push("scrollback");
    }
    if old.shell != new.shell {
        s.new_pane.push("shell");
    }
    if old.default_profile_id != new.default_profile_id {
        s.new_pane.push("default_profile_id");
    }
    if old.restore_session_on_startup != new.restore_session_on_startup {
        s.restart.push("restore_session_on_startup");
    }
    if old.cols != new.cols || old.rows != new.rows {
        s.restart.push("cols/rows");
    }
    if old.quake_hotkey != new.quake_hotkey {
        s.restart.push("quake_hotkey");
    }
    // MCP 权限全部热生效：socket supervisor 与每个请求都读运行期权限。
    hot(old.mcp_enabled != new.mcp_enabled, "mcp_enabled");
    hot(old.mcp_read != new.mcp_read, "mcp_read");
    hot(old.mcp_input != new.mcp_input, "mcp_input");
    hot(old.mcp_tabs != new.mcp_tabs, "mcp_tabs");
    hot(old.mcp_destructive != new.mcp_destructive, "mcp_destructive");
    s
}

/// 外部 `config.toml` 成功解析后套用。**绝不调用 `save`**，避免 watcher 自己
/// 触发第二轮文件变化；设置页保存走各自现有回调，不经过这里。
fn apply_external_config(st: &mut AppState, new_cfg: Config) {
    let old = st.cfg.with(|cfg| cfg.clone());
    if old == new_cfg {
        // 典型情况：设置页刚 save 完、watcher 收到还没消费掉的自身写入；或只是
        // 重新格式化/改注释/换行。内容等价就不重载、不重绘。
        eprintln!("[config] 外部变更与当前配置等价，跳过套用（自身写入/仅格式变化）");
        return;
    }
    apply_external_config_from(st, old, new_cfg);
}

/// 已经由 SharedServices 统一替换配置后，逐窗口同步 UI/终端状态。
///
/// 不能再用 `st.cfg == new_cfg` 提前返回：调用方已把共享配置写成新值，第二
/// 个窗口仍需要应用字体/语言/主题等 per-window 状态。
fn apply_external_config_from(st: &mut AppState, old: Config, new_cfg: Config) {
    let summary = reconfig_summary(&old, &new_cfg);
    let language_changed = old.lang() != new_cfg.lang();
    let theme_changed = old.theme != new_cfg.theme
        || old.auto_dark != new_cfg.auto_dark
        || old.auto_light != new_cfg.auto_light;
    let font_changed = old.font_size != new_cfg.font_size;
    let bg_image_changed = old.background_image != new_cfg.background_image;
    let bg_opacity_changed = old.background_opacity != new_cfg.background_opacity;
    let scroll_output_changed = old.scroll_on_output != new_cfg.scroll_on_output;
    let sixel_changed = old.sixel != new_cfg.sixel;
    let sixel_scrolling_changed = old.sixel_scrolling != new_cfg.sixel_scrolling;
    let iip_changed = old.iip != new_cfg.iip;
    let scrollbar_changed = old.scrollbar_policy != new_cfg.scrollbar_policy;
    let cursor_changed = old.cursor_shape != new_cfg.cursor_shape
        || old.cursor_color != new_cfg.cursor_color
        || old.cursor_text_color != new_cfg.cursor_text_color;
    let blink_changed = old.cursor_blink != new_cfg.cursor_blink;
    let bold_changed = old.bold_is_bright != new_cfg.bold_is_bright
        || old.bold_color != new_cfg.bold_color
        || old.bold_color_same_as_fg != new_cfg.bold_color_same_as_fg;
    let visual_bell_changed = old.visual_bell != new_cfg.visual_bell;
    let tabbar_changed = old.tabbar_position != new_cfg.tabbar_position;
    let hotkeys_changed = old.hotkeys != new_cfg.hotkeys;
    let mcp_changed = old.mcp_enabled != new_cfg.mcp_enabled
        || old.mcp_read != new_cfg.mcp_read
        || old.mcp_input != new_cfg.mcp_input
        || old.mcp_tabs != new_cfg.mcp_tabs
        || old.mcp_destructive != new_cfg.mcp_destructive;

    *st.cfg = new_cfg;
    // 先换运行期权限，再同步 UI；socket supervisor 会自行创建/移除 listener。
    if mcp_changed {
        mcp::apply_config(&st.cfg);
    }

    if theme_changed {
        let selected = st.cfg.theme.clone();
        let before = old.theme.clone();
        if !install_theme_selection(st, &selected) {
            // 配置解析成功但主题 id 不存在：保留旧主题，其余字段仍继续套用。
            st.cfg.theme = before;
        }
    }

    if font_changed {
        let requested = st.cfg.font_size;
        if !apply_font_size(st) {
            eprintln!("[config] 外部 font_size={requested} 应用失败，保留旧字号");
            st.cfg.font_size = old.font_size;
        }
    }

    if bg_image_changed {
        let raw = st.cfg.background_image.clone();
        if let Err(e) = apply_background_image(st, &raw) {
            // 不改回 cfg.path：设置页应看到用户刚写的路径和错误，但屏幕继续用旧图。
            eprintln!("[config] 外部 background_image 无效，保留当前背景: {e}");
        }
    }
    if bg_opacity_changed || bg_image_changed {
        sync_renderer_opacity(st);
    }

    if scroll_output_changed {
        let enabled = st.cfg.scroll_on_output;
        for rt in st.panes.values() {
            rt.handle.set_scroll_on_output(enabled);
        }
        for (_, rt, _) in &st.detached {
            rt.handle.set_scroll_on_output(enabled);
        }
    }
    if sixel_changed {
        let enabled = st.cfg.sixel;
        for rt in st.panes.values() {
            rt.handle.set_sixel(enabled);
        }
        for (_, rt, _) in &st.detached {
            rt.handle.set_sixel(enabled);
        }
    }
    if sixel_scrolling_changed {
        let enabled = st.cfg.sixel_scrolling;
        for rt in st.panes.values() {
            rt.handle.set_sixel_scrolling(enabled);
        }
        for (_, rt, _) in &st.detached {
            rt.handle.set_sixel_scrolling(enabled);
        }
    }
    if iip_changed {
        let enabled = st.cfg.iip;
        for rt in st.panes.values() {
            rt.handle.set_iip(enabled);
        }
        for (_, rt, _) in &st.detached {
            rt.handle.set_iip(enabled);
        }
    }
    if scrollbar_changed {
        refresh_all_pane_views(st);
    }
    if cursor_changed {
        apply_cursor_settings(st);
    }
    if blink_changed {
        configure_cursor_blink(st);
    }
    if bold_changed {
        apply_bold_settings(st);
    }
    if visual_bell_changed && !st.cfg.visual_bell {
        for rt in st.panes.values_mut() {
            rt.bell_until = None;
        }
        st.bell_timer.stop();
        refresh_all_pane_views(st);
    }
    if hotkeys_changed {
        refresh_hotkeys(st);
    }

    sync_reload_ui(st);
    if language_changed {
        sync_language_ui(st);
    }
    if tabbar_changed {
        st.ime_spot_dirty = true;
        update_ime_spot(st);
    }
    if mcp_changed {
        // listener 的 bind/remove 在 supervisor 线程里最多 ~100ms；稍后再刷一次状态。
        schedule_mcp_status_refresh(st.window_id);
    }

    eprintln!("[config] 已套用外部热更新: {}", summary.hot.join(", "));
    if !summary.new_pane.is_empty() {
        eprintln!(
            "[config] 以下字段仅对之后新建的 pane/Tab 生效，旧 pane 保持原行为: {}",
            summary.new_pane.join(", ")
        );
    }
    if !summary.restart.is_empty() {
        eprintln!(
            "[config] 以下字段需要重启 Tervo 才生效: {}",
            summary.restart.join(", ")
        );
    }
}

/// 外部热更新后同步设置页控件状态。只改 Slint 属性，不触发回调、不写盘。
fn sync_reload_ui(st: &mut AppState) {
    st.ui.set_language(st.cfg.lang().into());
    st.ui.set_sidebar_visible(st.cfg.sidebar_visible);
    st.ui.set_sidebar_width(sidebar_width_of(&st.cfg));
    st.ui.set_cfg_sidebar_visible(st.cfg.sidebar_visible);
    st.ui.set_font_size(st.cfg.font_size as i32);
    st.ui.set_scrollback(st.cfg.scrollback as i32);
    st.ui
        .set_background_image_path(st.cfg.background_image.as_str().into());
    st.ui.set_background_opacity(st.cfg.background_opacity);
    st.ui.set_background_dim(st.cfg.background_dim);
    st.ui.set_cfg_copy_on_select(st.cfg.copy_on_select);
    st.ui.set_cfg_confirm_close(st.cfg.confirm_close);
    st.ui
        .set_cfg_restore_session(st.cfg.restore_session_on_startup);
    st.ui.set_cfg_confirm_paste(st.cfg.confirm_multiline_paste);
    st.ui
        .set_cfg_right_click(st.cfg.effective_right_click().into());
    st.ui.set_cfg_paste_middle(st.cfg.paste_on_middle_click);
    st.ui
        .set_cfg_desktop_notifications(st.cfg.desktop_notifications);
    st.ui.set_cfg_visual_bell(st.cfg.visual_bell);
    st.ui.set_cfg_audible_bell(st.cfg.audible_bell);
    st.ui.set_cfg_scroll_output(st.cfg.scroll_on_output);
    st.ui.set_cfg_scroll_keystroke(st.cfg.scroll_on_keystroke);
    st.ui.set_cfg_sixel(st.cfg.sixel);
    st.ui.set_cfg_sixel_scrolling(st.cfg.sixel_scrolling);
    st.ui.set_cfg_iip(st.cfg.iip);
    st.ui
        .set_cfg_plugin_require_signature(st.cfg.plugin_require_signature);
    st.ui.set_cfg_render_hud(st.cfg.render_hud);
    set_render_hud(st, render_hud_forced_by_env() || st.cfg.render_hud);
    st.ui
        .set_cfg_scrollback_unlimited(st.cfg.scrollback_unlimited);
    st.ui
        .set_cfg_scrollbar_policy(st.cfg.scrollbar_policy().as_str().into());
    st.ui
        .set_cfg_backspace_binding(st.cfg.key_bindings().backspace.as_str().into());
    st.ui
        .set_cfg_delete_binding(st.cfg.key_bindings().delete.as_str().into());
    st.ui
        .set_cfg_cursor_shape(st.cfg.cursor_shape.clone().into());
    st.ui
        .set_cfg_cursor_blink(st.cfg.cursor_blink.clone().into());
    st.ui
        .set_cfg_cursor_color(st.cfg.cursor_color.clone().into());
    st.ui
        .set_cfg_cursor_text_color(st.cfg.cursor_text_color.clone().into());
    st.ui.set_cfg_bold_is_bright(st.cfg.bold_is_bright);
    st.ui.set_cfg_bold_color(st.cfg.bold_color.clone().into());
    st.ui
        .set_cfg_bold_color_same_as_fg(st.cfg.bold_color_same_as_fg);
    st.ui.set_cfg_quake(st.cfg.quake_enabled);
    st.ui.set_tabbar_bottom(st.cfg.tabbar_position == "bottom");
    sync_mcp_ui(st);
}

/// MCP 设置/状态同步到 Slint。只写属性；socket 生命周期由 diag supervisor 管。
fn sync_mcp_ui(st: &mut AppState) {
    st.ui.set_cfg_mcp_enabled(st.cfg.mcp_enabled);
    st.ui.set_cfg_mcp_read(st.cfg.mcp_read);
    st.ui.set_cfg_mcp_input(st.cfg.mcp_input);
    st.ui.set_cfg_mcp_tabs(st.cfg.mcp_tabs);
    st.ui.set_cfg_mcp_destructive(st.cfg.mcp_destructive);

    let (permissions, socket_path, token_path, active, error, token_ready, env_write, env_destructive) =
        match mcp::runtime() {
            Some(runtime) => (
                runtime.permissions(),
                runtime.socket_path().to_path_buf(),
                runtime.token_path().to_path_buf(),
                runtime.active(),
                runtime.last_error(),
                runtime.token().is_some(),
                runtime.env_write_override(),
                runtime.env_destructive_override(),
            ),
            None => (
                McpPermissions::from_config(&st.cfg),
                mcp::default_socket_path(),
                mcp::token_file_path(),
                false,
                None,
                false,
                false,
                false,
            ),
        };
    st.ui
        .set_mcp_socket_path(socket_path.display().to_string().into());
    st.ui
        .set_mcp_token_path(token_path.display().to_string().into());
    st.ui.set_mcp_active(permissions.enabled && active);
    st.ui.set_mcp_status(
        mcp_status_text(
            st.cfg.lang(),
            &permissions,
            active,
            token_ready,
            error.as_deref(),
            env_write,
            env_destructive,
        )
        .into(),
    );
}

fn mcp_status_text(
    lang: &str,
    permissions: &McpPermissions,
    active: bool,
    token_ready: bool,
    error: Option<&str>,
    env_write: bool,
    env_destructive: bool,
) -> String {
    if !permissions.enabled {
        return ui_text(
            lang,
            "Off — no live socket is created; live tools are hidden.",
            "已关闭 — 不会创建 live socket，也不会暴露 live 工具。",
        )
        .to_string();
    }
    if !token_ready {
        return ui_text(
            lang,
            "Enabled — state token unavailable; live requests are rejected (see log).",
            "已启用 — state token 不可用，live 请求会被拒绝（见日志）。",
        )
        .to_string();
    }
    if let Some(error) = error {
        return format!(
            "{}: {error}",
            ui_text(lang, "Socket unavailable", "Socket 不可用")
        );
    }
    let mut groups = Vec::new();
    if permissions.read {
        groups.push(ui_text(lang, "read", "读取"));
    }
    if permissions.input {
        groups.push(ui_text(lang, "input", "输入"));
    }
    if permissions.tabs {
        groups.push(ui_text(lang, "tabs", "标签"));
    }
    if permissions.destructive {
        groups.push(ui_text(lang, "destructive", "破坏性"));
    }
    let groups = if groups.is_empty() {
        ui_text(lang, "no groups", "未授权任何分组").to_string()
    } else {
        groups.join(", ")
    };
    let socket = if active {
        ui_text(lang, "socket active", "socket 已就绪")
    } else {
        ui_text(lang, "socket starting", "socket 启动中")
    };
    let mut status = format!(
        "{} · {groups} · {socket}",
        ui_text(lang, "Enabled", "已启用")
    );
    if env_write || env_destructive {
        status.push_str(ui_text(
            lang,
            " · env override active",
            " · 环境变量覆盖生效",
        ));
    }
    status
}

/// listener 在 supervisor 线程里异步 bind/remove；500ms 后刷新一次状态文字。
fn schedule_mcp_status_refresh(window_id: WindowId) {
    slint::Timer::single_shot(Duration::from_millis(500), move || {
        let _ = runtime::with_window(window_id, sync_mcp_ui);
    });
}

#[derive(Clone, Copy)]
enum McpSetting {
    Enabled,
    Read,
    Input,
    Tabs,
    Destructive,
}

/// Settings → Privacy：立即写 config 并换运行期权限；socket 由 supervisor
/// 在 100ms 内创建/移除，不需要重启 Tervo。
fn toggle_mcp_setting(st: &mut AppState, setting: McpSetting) {
    match setting {
        McpSetting::Enabled => st.cfg.mcp_enabled = !st.cfg.mcp_enabled,
        McpSetting::Read => st.cfg.mcp_read = !st.cfg.mcp_read,
        McpSetting::Input => st.cfg.mcp_input = !st.cfg.mcp_input,
        McpSetting::Tabs => st.cfg.mcp_tabs = !st.cfg.mcp_tabs,
        McpSetting::Destructive => st.cfg.mcp_destructive = !st.cfg.mcp_destructive,
    }
    st.cfg.save();
    mcp::apply_config(&st.cfg);
    sync_mcp_ui(st);
    let permissions = mcp::runtime()
        .map(|runtime| runtime.permissions())
        .unwrap_or_else(|| McpPermissions::from_config(&st.cfg));
    eprintln!(
        "[mcp] settings enabled={} read={} input={} tabs={} destructive={}",
        permissions.enabled,
        permissions.read,
        permissions.input,
        permissions.tabs,
        permissions.destructive
    );
    schedule_mcp_status_refresh(st.window_id);
}

/// 退出设置搜索/清空后重算结果；展示行与“无匹配”状态跟随当前语言。
fn refresh_settings_results(st: &mut AppState) {
    let q = st.ui.get_settings_query().to_string();
    let hits = settings_search_hits(&q, st.cfg.lang());
    st.settings_result_cats = hits.iter().map(|h| h.cat).collect();
    st.settings_result_sections = hits.iter().map(|h| h.section).collect();
    st.settings_model
        .set_vec(hits.into_iter().map(|h| h.line.into()).collect::<Vec<_>>());
}

/// 语言切换/外部热更新后统一刷新：静态文案由 Slint `Lang.t` 自动重算，
/// 这里只重建 Rust 侧动态数据（快捷键/插件/Vault/配置/搜索/主题显示名）。
fn sync_language_ui(st: &mut AppState) {
    let lang = st.cfg.lang().to_string();
    st.ui.set_language(lang.as_str().into());
    refresh_hotkeys(st);
    refresh_plugin_page(st);
    refresh_vault_ui(st);
    refresh_profiles(st);
    let theme_display = if is_auto_theme(&st.cfg.theme) {
        format!(
            "{} · {}",
            ui_text(&lang, "Auto", "自动"),
            st.resolved_theme.label
        )
    } else {
        st.resolved_theme.label.clone()
    };
    st.ui.set_theme_display(theme_display.into());
    refresh_settings_results(st);
    sync_mcp_ui(st);
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
    let Some(stdout) = child.stdout.take() else {
        return;
    };
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
            runtime::with_all_windows(|_id, st| {
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

/// 自绘/ WM resize 共用的收尾：清 Rust 状态，并补发一次 PointerReleased 清理
/// Slint TouchArea 的 pressed/抓取状态。WM live resize 期间 mouse-up 可能被
/// WM grab 吃掉；如果只清 Rust 状态，ResizeHandle 仍认为按着，后续每次 move
/// 都会继续 set_size，表现为“松开左键后还能放缩窗口”。
fn finish_window_resize(st: &mut AppState, reason: &str) {
    let was_active = st.wm_resize || st.resize.is_some();
    st.wm_resize = false;
    st.wm_release_pending = false;
    st.resize = None;
    if !was_active {
        return;
    }
    eprintln!("[ui] window resize end reason={reason}");
    if st.resize_in_progress {
        schedule_resize_finalize(st);
    }
    // 用当前指针位置补发，命中原来的 ResizeHandle；0,0 会落到窗口外/别的控件，
    // 不会清掉 TouchArea 的 pressed 状态。
    let local = resize_pointer(st).map(|(px, py)| {
        let pos = st.ui.window().position();
        let scale = st.ui.window().scale_factor().max(0.1) as f32;
        let size = st.ui.window().size();
        let max_x = (size.width as f32 / scale - 1.0).max(0.0);
        let max_y = (size.height as f32 / scale - 1.0).max(0.0);
        (
            ((px as f32 - pos.x as f32) / scale).clamp(0.0, max_x),
            ((py as f32 - pos.y as f32) / scale).clamp(0.0, max_y),
        )
    });
    if let Some((x, y)) = local {
        // 不能在 with_app 借用期间同步 dispatch：Slint 会立刻回调 resize/up
        // callback，再进 with_app 变成重入，操作被丢弃（鼠标松开后状态卡住）。
        // 当前借用先结束，回到事件循环再补发 PointerReleased。
        let weak = st.ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(0), move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().dispatch_event(
                    slint::platform::WindowEvent::PointerReleased {
                        position: slint::LogicalPosition::new(x, y),
                        button: slint::platform::PointerEventButton::Left,
                    },
                );
            }
        });
    }
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
fn winit_window_x11_id(window: &winit::window::Window) -> Option<u32> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xlib(h) => Some(h.window as u32),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

fn winit_window_is_x11(window: &winit::window::Window) -> bool {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window.window_handle().ok().is_some_and(|handle| {
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

    let (started, source) = match st
        .ui
        .window()
        .with_winit_window(|window| window.drag_resize_window(winit_dir))
    {
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
            let Some(window) = xconn::find_own_window(&ctx.conn, ctx.root, &st.opts.title) else {
                eprintln!("[ui] 找不到本进程窗口，无法走 EWMH resize");
                return false;
            };
            match xconn::start_wm_resize(&ctx.conn, ctx.root, window, dir) {
                Ok(()) => {
                    eprintln!("[ui] xconn EWMH 兜底已发送；跨连接 ungrab 可能无效，仍保留自绘回退");
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
        .and_then(|s| {
            std::path::Path::new(s)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "shell".to_string())
}

fn create_pane(
    st: &AppState,
    id: PaneId,
    rect: Rect,
    cwd: Option<PathBuf>,
    profile: Option<&profile::Profile>,
    mut askpass: Option<askpass::Askpass>,
) -> std::io::Result<PaneRuntime> {
    // pane id 可能被重连复用；旧 GPU 纹理不能跨会话复用。
    gpu_present::forget(id);

    // Profile 优先：它显式带 shell/args/env；否则沿用全局配置。
    // 远端 Profile（ssh/telnet/serial）里出现的路径属于远端文件系统，不能本地打开。
    let remote = profile.is_some_and(|p| p.kind != "local");
    let shell = profile
        .map(|p| p.shell.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| st.cfg.shell.clone())
        // 设置/Profile 里的 shell 和 cwd 允许 `~`；SSH 远端路径不展开。
        .map(|s| {
            if remote {
                s
            } else {
                config::expand_tilde(&s).to_string_lossy().into_owned()
            }
        });
    let args = profile
        .map(|p| p.args.clone())
        .unwrap_or_else(|| st.opts.args.clone());
    let mut env = profile.map(|p| p.env.clone()).unwrap_or_default();
    if let Some(a) = askpass.as_mut() {
        a.apply(&mut env);
        // 只记变量名与“是否非空”，不记密码；用于确认 env 真的进入 ssh 子进程。
        let keys: Vec<&str> = env
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| k.starts_with("SSH_ASKPASS") || *k == askpass::SECRET_ENV)
            .collect();
        let secret_set = env
            .iter()
            .any(|(k, v)| k.as_str() == askpass::SECRET_ENV && !v.is_empty());
        eprintln!("[askpass] 注入 ssh 子进程环境: keys={keys:?} secret_set={secret_set}");
    }
    // 显式 cwd（会话恢复的 /proc/<pid>/cwd）优先于 Profile 的起始目录：
    // 恢复要回到用户实际 cd 到的目录，而不是每次都回到 Profile 配置的目录。
    let cwd = cwd
        .or_else(|| profile.and_then(|p| p.cwd.clone()).map(PathBuf::from))
        .map(|p| {
            if remote {
                p
            } else {
                config::expand_tilde(&p.to_string_lossy())
            }
        });

    // per-profile 外观：字号决定渲染字符尺寸/PTY 网格；空 = 继承全局。
    let mut appearance = profile.map(PaneAppearance::from_profile).unwrap_or_default();
    let fonts = match appearance.font_size {
        Some(size) if (size - st.cfg.font_size).abs() > f32::EPSILON => match FontSet::load(size) {
            Some(fonts) => Rc::new(fonts),
            None => {
                eprintln!("[profile] Profile 字号 {size} 加载字体失败，回退全局字号");
                appearance.font_size = None;
                st.fonts.clone()
            },
        },
        _ => st.fonts.clone(),
    };
    let (cols, rows) = cell_of(rect, (fonts.cell_w, fonts.cell_h));
    let theme = theme_for_selection(&st.registry, &st.cfg, appearance.theme.as_deref())
        .unwrap_or_else(|| st.resolved_theme.clone());
    let scrollback = appearance
        .scrollback
        .unwrap_or_else(|| st.cfg.effective_scrollback());
    let palette = theme.palette().with_bold(
        st.cfg.bold_is_bright,
        st.cfg.bold_color_rgb(),
        st.cfg.bold_color_same_as_fg,
    );
    let handle = spawn(PaneConfig {
        cols: cols as u16,
        rows: rows as u16,
        scrollback,
        scroll_on_output: st.cfg.scroll_on_output,
        keyboard_protocol: st.cfg.keyboard_protocol_enabled(),
        sixel: st.cfg.sixel,
        sixel_scrolling: st.cfg.sixel_scrolling,
        iip: st.cfg.iip,
        // 所有 pane 共享同一个 WASM host：设置页 Enable/Disable 原地生效。
        // 没有 WASM 插件时为 None，core 热路径不增加分支之外的任何开销。
        output_filter: st.plugins.output_filter_handle(),
        cell_width: fonts.cell_w as u16,
        cell_height: fonts.cell_h as u16,
        shell,
        args,
        palette,
        cwd,
        env,
    })?;

    let wake = handle.wake_receiver();
    // 语义事件唤醒与内容拉帧唤醒分开：内容 wake 只重绘该 pane，事件 wake 走
    // 标题/Bell/OSC52 的 UI 处理而且必须 drain 全部事件，两者不能互相冒充。
    //
    // A/B 诊断关闭时给一个永不触发的通道（保留 sender 在闭包内），行为回到
    // 旧版纯 400ms 轮询；正常路径下 Slint 事件循环被 CoreEvent 立即叫醒。
    let (event_wake, event_wake_keepalive) = if event_wake_enabled() {
        (handle.event_wake_receiver(), None)
    } else {
        let (tx, rx) = crossbeam_channel::bounded(1);
        (rx, Some(tx))
    };
    let content_pending = Arc::new(AtomicBool::new(false));
    let event_pending = Arc::new(AtomicBool::new(false));
    std::thread::Builder::new()
        .name(format!("wake-{id}"))
        .spawn(move || {
            let _event_wake_keepalive = event_wake_keepalive;
            loop {
                crossbeam_channel::select! {
                    recv(wake) -> msg => {
                        if msg.is_err() {
                            break; // pane 句柄释放：内容与事件通道都会断开
                        }
                        if content_pending.swap(true, Ordering::AcqRel) {
                            continue;
                        }
                        let pending = content_pending.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            pending.store(false, Ordering::Release);
                            redraw_pane(id);
                        });
                    },
                    recv(event_wake) -> msg => {
                        if msg.is_err() {
                            break;
                        }
                        if event_pending.swap(true, Ordering::AcqRel) {
                            continue;
                        }
                        let pending = event_pending.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            // 先清 pending 再处理：drain 期间新到的事件还能再唤醒；
                            // drain + revision 去重保证不会重复消费同一条事件。
                            pending.store(false, Ordering::Release);
                            on_core_event_wake();
                        });
                    },
                }
            }
        })?;

    let mut renderer = Renderer::new(
        fonts.clone(),
        cols,
        rows,
        theme.spec.bg,
        theme.spec.cursor,
        renderer_opacity(st),
    );
    renderer.set_cursor_style(
        appearance.cursor_shape(&st.cfg),
        st.cfg.cursor_color_rgb(),
        st.cfg.cursor_text_color_rgb(),
    );
    let mut frame = handle.new_frame();
    frame = handle.pull(frame);
    // 光标不画进终端帧：单独生成 cell 大小 overlay，闪烁只切 Slint visible。
    renderer.draw_without_cursor(&frame);
    let image = renderer.image();
    let (cursor_image, cursor_rect, cursor_visible) = match renderer.cursor_overlay(&frame) {
        Some(o) => (o.image, (o.x, o.y, o.width, o.height), true),
        None => (slint::Image::default(), (0.0, 0.0, 0.0, 0.0), false),
    };

    Ok(PaneRuntime {
        handle,
        renderer,
        frame,
        image,
        appearance,
        scrollback_limit: scrollback,
        scrollback_applied: scrollback,
        profile_id: profile.map(|p| p.id).filter(|id| *id != 0),
        cell: (fonts.cell_w, fonts.cell_h),
        cursor_image,
        cursor_rect,
        cursor_visible,
        inline_model: Rc::new(VecModel::from(Vec::<PaneImage>::new())),
        inline_cache: HashMap::new(),
        inline_sig: Vec::new(),
        inline_hits: HashMap::new(),
        inline_geometry: (fonts.cell_w, fonts.cell_h, cols, rows),
        grid: (cols, rows),
        cursor: (0, 0),
        focus_sent: None,
        rect: Rect::new(0.0, 0.0, 0.0, 0.0),
        title: shell_name(&st.opts),
        activity: activity::PaneActivity::default(),
        notification: None,
        bell_until: None,
        remote,
        exit_code: None,
        exit_signal: None,
        exit_notified: false,
        link_hover: false,
        gpu_native_active: false,
        _askpass: askpass,
    })
}

fn new_tab_inner(st: &mut AppState) {
    new_tab_inner_with_cwd(st, None);
}

/// 启动/转发请求带 cwd 时的首 Tab；没有默认 Profile 才用请求 cwd，
/// 有默认 Profile 时保持既有“新 Tab 用默认 Profile”的语义。
pub(crate) fn new_tab_with_default_or_cwd(st: &mut AppState, cwd: Option<PathBuf>) {
    if cwd.is_some() && default_profile_index(st).is_none() {
        new_tab_inner_with_cwd(st, cwd);
    } else {
        new_tab_with_default(st);
    }
}

fn new_tab_inner_with_cwd(st: &mut AppState, cwd: Option<PathBuf>) {
    let id = st.shared.alloc_pane_id();
    match create_pane(st, id, st.content, cwd, None, None) {
        Ok(rt) => {
            st.next_title += 1;
            let n = st.next_title;
            let title = format!("Terminal {n}");
            st.panes.insert(id, rt);
            st.ws.tabs.push(Tab::single(id, title.clone()));
            eprintln!("[ws] new tab: tabs={}", st.ws.tabs.len());
            st.ws.active = st.ws.tabs.len() - 1;
            dispatch_plugin_event(
                st,
                plugin_v3::PluginEvent::TabOpen,
                plugin_v3::tab_open_payload(&title, "local"),
            );
            refresh_views(st);
            rebuild_tabs(st);
            redraw_pane_inner(st, id);
        }
        Err(e) => eprintln!("创建 pane 失败: {e}"),
    }
}

/// 新建窗口：默认在当前 primary 进程内创建（WindowRegistry）。
///
/// 只有显式 `TERVO_MULTI_WINDOW_INPROC=0` / `TERVO_MULTI_WINDOW_LEGACY=1` 时
/// 才回退到旧的多进程 `current_exe --window`。旧路径由子进程显式绕过单实例锁。
/// 在 `with_app` 借用内调用也安全：本函数只排队，不直接碰 AppState。
fn new_window() {
    if secondary::legacy_fallback() {
        match window::spawn_new_window() {
            Ok(pid) => eprintln!("[ui] new window pid={pid}（legacy subprocess fallback）"),
            Err(e) => eprintln!("[ui] new window failed: {e}"),
        }
        return;
    }
    static PENDING: AtomicBool = AtomicBool::new(false);
    if PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    match slint::invoke_from_event_loop(|| {
        PENDING.store(false, Ordering::Release);
        match secondary::create_inproc_window() {
            Ok(id) => eprintln!("[ui] in-process new window id={id}"),
            Err(error) => eprintln!("[ui] in-process new window failed: {error}"),
        }
    }) {
        Ok(()) => {}
        Err(error) => {
            PENDING.store(false, Ordering::Release);
            eprintln!("[ui] in-process new window unavailable: {error}");
        }
    }
}

/// 关视图但保留会话：运行时挪到后台池，PaneHandle 不被 Drop，子进程继续跑。
fn detach_runtime(st: &mut AppState, id: PaneId) {
    let Some(rt) = st.panes.remove(&id) else {
        return;
    };
    let title = if rt.title.is_empty() {
        format!("session {id}")
    } else {
        rt.title.clone()
    };
    eprintln!("[session] detach pane={id} title={title:?}（子进程保留）");
    st.detached.push((id, rt, title));
    refresh_background(st);
}

/// 后台会话列表（标题 + 窗口内 cwd 提示不吃内存）。
fn refresh_background(st: &mut AppState) {
    let items: Vec<slint::SharedString> = st
        .detached
        .iter()
        .map(|(id, _, title)| format!("{title}   #{id}").into())
        .collect();
    st.bg_model.set_vec(items);
}

/// 刷新 Profiles 面板：按搜索词过滤，并记录每个可见行对应的 store 下标。
///
/// 列表模型只带展示字段；所有操作回调用 `profile_indices` 映射回真实 Profile，
/// 避免过滤后 “点第 0 行却打开第 0 个 store 项” 的错位。
fn refresh_profiles(st: &mut AppState) {
    let default_id = st.cfg.default_profile_id;
    let lang = st.cfg.lang();
    let kind_label = |kind: &str| -> String {
        match kind {
            "local" => ui_text(lang, "local", "本地").to_string(),
            "serial" => ui_text(lang, "serial", "串口").to_string(),
            other => other.to_string(),
        }
    };
    let indices = profile::filter_indices(&st.profiles.profiles, &st.profile_query);
    let items: Vec<ProfileRow> = indices
        .iter()
        .filter_map(|&i| {
            let p = st.profiles.profiles.get(i)?;
            Some(ProfileRow {
                id: p.id as i32,
                label: format!("[{}] {}", kind_label(&p.kind), p.name).into(),
                is_default: default_id != 0 && p.id == default_id,
            })
        })
        .collect();
    let default_info = match st.profiles.default_index(default_id) {
        Some(i) => {
            if lang == "zh" {
                format!("新标签默认：{}", st.profiles.profiles[i].name)
            } else {
                format!("New tab default: {}", st.profiles.profiles[i].name)
            }
        },
        None => ui_text(lang, "New tab default: system shell", "新标签默认：系统 Shell").to_string(),
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
        }
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
    // 多显示器：用当前窗口所在 monitor，而不是整个 root（双屏时 5120 会
    // 把 Quake 放到接缝上）。顶部/底部 panel 用 EWMH workarea 让开。
    let pos = st.ui.window().position();
    let monitors = crate::xconn::monitors();
    let monitor = crate::xconn::monitor_at(&monitors, pos.x, pos.y)
        .unwrap_or(crate::xconn::Monitor {
            x: 0,
            y: 0,
            width: crate::xconn::screen_size().map(|v| v.0).unwrap_or(1920),
            height: crate::xconn::screen_size().map(|v| v.1).unwrap_or(1080),
            primary: true,
        });
    let wa = crate::xconn::workarea();
    let top_inset = wa
        .map(|(_, wy, _, _)| (wy - monitor.y).max(0) as u32)
        .unwrap_or(0)
        .min(monitor.height.saturating_sub(200));
    let bottom_inset = wa
        .map(|(_, wy, _, wh)| {
            ((monitor.y + monitor.height as i32) - (wy + wh as i32)).max(0) as u32
        })
        .unwrap_or(0)
        .min(monitor.height.saturating_sub(200));
    let avail_h = monitor
        .height
        .saturating_sub(top_inset)
        .saturating_sub(bottom_inset)
        .max(200);
    let w = ((monitor.width as f32 * st.cfg.quake_width).round() as u32)
        .clamp(320, monitor.width);
    let h = ((avail_h as f32 * st.cfg.quake_height).round() as u32)
        .clamp(200, avail_h);
    let x = monitor.x + ((monitor.width.saturating_sub(w)) / 2) as i32;
    let y = monitor.y + top_inset as i32;
    if st.quake_saved.is_none() {
        st.quake_saved = Some((st.ui.window().position(), st.ui.window().size()));
    }
    st.ui.window().set_minimized(false);
    st.ui.window().set_size(slint::PhysicalSize::new(w, h));
    st.ui
        .window()
        .set_position(slint::PhysicalPosition::new(x, y));
    st.ui.window().show();
    let _ = crate::xconn::activate_own_window("");
    st.quake_visible = true;
    eprintln!("[quake] show {w}x{h} at {x},0");
}

/// 重建 Profile 候选项（"+" 下拉与侧栏 Profiles 列表共用）。
///
/// 以前只在 `open_profile_menu` 里构建，侧栏复用同一份模型时列表会是空的；
/// 现在启动/侧栏可见/Profile 变更都调用它。
fn rebuild_profile_choices(st: &mut AppState) {
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
}

/// 构建 Profile 选择菜单（+ 旁边独立图标；不包含“新建默认 Tab”）。
fn open_profile_menu(st: &mut AppState) {
    rebuild_profile_choices(st);
    st.ui.set_profilemenu_open(true);
    eprintln!(
        "[ui] profile menu profiles={}",
        st.profile_menu_indices.len()
    );
}

fn open_profiles_panel(st: &mut AppState) {
    // 每次打开都回到完整列表：搜索词不跨面板生命周期残留。
    st.profile_query.clear();
    st.ui.set_profile_query("".into());
    refresh_profiles(st);
    st.ui.set_profiles_open(true);
    eprintln!("[profile] panel items={}", st.profile_indices.len());
}

/// 关闭叠在设置页之上的模态（Profiles/Profile 表单）后恢复键盘焦点：
/// 设置页还开着就回设置 FocusScope（Esc 仍能关设置），否则回终端 IME。
fn refocus_after_modal(st: &mut AppState) {
    if st.ui.get_settings_open() {
        let next = st.ui.get_settings_refocus_token().wrapping_add(1);
        st.ui.set_settings_refocus_token(next);
    } else {
        st.ui.invoke_focus_terminal();
    }
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
/// Profile 启动时可能失败的分类（不包含密码明文）。
#[derive(Debug)]
enum ProfileLaunchError {
    NotFound,
    VaultLocked,
    Vault(String),
    Askpass(String),
    UnknownKind(String),
}

impl ProfileLaunchError {
    fn message(&self) -> String {
        match self {
            Self::NotFound => "Profile 不存在".into(),
            Self::VaultLocked => "Vault 已锁定".into(),
            Self::Vault(e) => e.clone(),
            Self::Askpass(e) => format!("创建 ssh askpass 失败: {e}"),
            Self::UnknownKind(kind) => format!("未知 Profile 类型: {kind}"),
        }
    }
}

/// 构造 Profile 的启动 spec（ssh/telnet/serial 会替换成系统客户端命令）。
///
/// 不修改 UI/配置；Vault/askpass 失败用错误分类返回，调用方决定是弹设置页
/// （用户主动打开 Profile）还是回退普通 shell（会话恢复/复制 Tab）。
fn build_profile_launch(
    st: &AppState,
    index: usize,
) -> Result<(profile::Profile, Option<askpass::Askpass>), ProfileLaunchError> {
    build_profile_launch_with(st, index, true)
}

/// [`build_profile_launch`] 的可控版本：`allow_askpass=false` 用于
/// 「首次/变更主机密钥只在终端确认」以及预检失败的回退路径，确保 ssh 在
/// PTY 里亲自询问密码/指纹，而不是被 askpass 自动回答。
fn build_profile_launch_with(
    st: &AppState,
    index: usize,
    allow_askpass: bool,
) -> Result<(profile::Profile, Option<askpass::Askpass>), ProfileLaunchError> {
    let p = st
        .profiles
        .profiles
        .get(index)
        .cloned()
        .ok_or(ProfileLaunchError::NotFound)?;
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
                        if !allow_askpass {
                            eprintln!(
                                "[profile] 本次连接按用户选择/预检回退：不启用 askpass，\
                                 由 ssh 在终端确认主机密钥并询问密码"
                            );
                        } else if !askpass::host_key_known(&p) {
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
                                }
                                Err(e) => {
                                    drop(password);
                                    return Err(ProfileLaunchError::Askpass(e.to_string()));
                                }
                            }
                        }
                    }
                    Err(vault::VaultError::Locked | vault::VaultError::NotInitialized) => {
                        return Err(ProfileLaunchError::VaultLocked);
                    }
                    Err(e) => return Err(ProfileLaunchError::Vault(e.to_string())),
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
        other => return Err(ProfileLaunchError::UnknownKind(other.to_string())),
    };
    Ok((spec, askpass))
}

/// 用 Profile 打开新 Tab（shell/args/env/cwd/外观全按 Profile）。
///
/// SSH Profile 会先做 known_hosts 预检（后台，最多 10s，可取消）：首次连接 /
/// 主机密钥变更时弹窗让用户明确选择，绝不静默信任；同一条连接配置在一次进程内
/// 预检通过后缓存，后续快速打开。其它 kind 直接打开。
fn open_profile_tab(st: &mut AppState, index: usize) {
    let Some(p) = st.profiles.profiles.get(index).cloned() else {
        eprintln!("[profile] 打开失败: Profile 不存在");
        return;
    };
    if p.kind != "ssh" {
        open_profile_tab_now(st, index, true);
        return;
    }
    // Vault 锁定要先弹解锁，没必要先做网络预检（否则用户要等两次）。
    if p.auth == "password" && p.save_password && !st.vault.is_unlocked() {
        prompt_vault_unlock(st, "Vault 已锁定：请先在 Vault 页解锁，再连接该 SSH Profile");
        return;
    }
    let cache_key = hostkey_cache_key(&p);
    if st.hostkey_verified.contains(&cache_key) {
        open_profile_tab_now(st, index, true);
        return;
    }
    start_hostkey_check(st, p, index, cache_key);
}

/// 预检缓存键：包含会影响 ssh 连接/known_hosts 查找的字段。
fn hostkey_cache_key(p: &profile::Profile) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        p.id,
        p.host.trim(),
        p.port,
        p.proxy_jump.trim(),
        p.extra_args.join(" ")
    )
}

fn start_hostkey_check(
    st: &mut AppState,
    p: profile::Profile,
    index: usize,
    cache_key: String,
) {
    // 同一时间只保留一个预检；用户连点另一个 Profile 时取消旧的。
    if let Some(old) = st.hostkey_job.take() {
        old.cancel();
    }
    let host = format!("{}:{}", p.host.trim(), p.port);
    eprintln!("[known_hosts] 后台预检 host={host} profile={}", p.id);
    st.hostkey_pending = Some((index, cache_key));
    st.hostkey_prompt = None;
    st.hostkey_job = Some(known_hosts::HostKeyJob::start(p, st.app_wake.clone()));
    st.ui.set_hostkey_open(true);
    st.ui.set_hostkey_mode("checking".into());
    st.ui.set_hostkey_title("Checking host key…".into());
    st.ui
        .set_hostkey_detail(format!("{host}\n正在读取 known_hosts 并检查服务器公钥（最多 10s）").as_str().into());
    st.ui.set_hostkey_error("".into());
}

/// 预检结果轮询：Known 直接开 Tab；其余弹 known_hosts 对话框。
fn poll_hostkey_check(st: &mut AppState) {
    let Some(job) = st.hostkey_job.as_ref() else {
        return;
    };
    match job.try_recv() {
        Ok(verdict) => {
            st.hostkey_job = None;
            let Some((index, cache_key)) = st.hostkey_pending.take() else {
                st.ui.set_hostkey_open(false);
                return;
            };
            eprintln!("[known_hosts] 预检结果: {}", verdict.summary());
            if verdict == known_hosts::Verdict::Known {
                st.hostkey_verified.insert(cache_key);
                st.ui.set_hostkey_open(false);
                st.ui.set_hostkey_error("".into());
                open_profile_tab_now(st, index, true);
            } else {
                show_hostkey_verdict(st, index, cache_key, verdict);
            }
        },
        Err(std::sync::mpsc::TryRecvError::Empty) => {},
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            st.hostkey_job = None;
            let Some((index, cache_key)) = st.hostkey_pending.take() else {
                st.ui.set_hostkey_open(false);
                return;
            };
            show_hostkey_verdict(
                st,
                index,
                cache_key,
                known_hosts::Verdict::Unverifiable {
                    reason: "主机密钥预检线程意外结束".into(),
                    local: false,
                },
            );
        },
    }
}

fn show_hostkey_verdict(
    st: &mut AppState,
    index: usize,
    cache_key: String,
    verdict: known_hosts::Verdict,
) {
    let profile = st.profiles.profiles.get(index).cloned();
    let host = profile
        .as_ref()
        .map(|p| format!("{}:{}", p.host.trim(), p.port))
        .unwrap_or_else(|| "SSH".into());
    let (mode, title) = match &verdict {
        known_hosts::Verdict::Known => ("known", "Host key already known"),
        known_hosts::Verdict::Unknown { .. } => ("unknown", "First connection: confirm host key"),
        known_hosts::Verdict::Changed { .. } => ("changed", "WARNING: host key has changed"),
        known_hosts::Verdict::Unverifiable { .. } => ("unverifiable", "Host key not pre-checked"),
    };
    eprintln!("[known_hosts] host={host} verdict={mode} profile={index}");
    st.ui.set_hostkey_open(true);
    st.ui.set_hostkey_mode(mode.into());
    st.ui.set_hostkey_title(title.into());
    let detail = format!("{host}\n\n{}", verdict.detail());
    st.ui.set_hostkey_detail(detail.as_str().into());
    st.ui.set_hostkey_error("".into());
    st.hostkey_prompt = Some((index, cache_key, verdict));
}

/// 用户点「信任/更新」：写 known_hosts 后按正常路径连接（可用 Vault）。
fn hostkey_trust(st: &mut AppState) {
    let Some((index, cache_key, verdict)) = st.hostkey_prompt.take() else {
        return;
    };
    let Some(profile) = st.profiles.profiles.get(index).cloned() else {
        st.ui.set_hostkey_open(false);
        return;
    };
    let keys = match &verdict {
        known_hosts::Verdict::Unknown { current } => current.clone(),
        known_hosts::Verdict::Changed { new, .. } => new.clone(),
        _ => {
            st.hostkey_prompt = Some((index, cache_key, verdict));
            return;
        },
    };
    match known_hosts::trust_keys(&profile, &keys) {
        Ok(()) => {
            st.hostkey_verified.insert(cache_key);
            st.ui.set_hostkey_open(false);
            st.ui.set_hostkey_error("".into());
            open_profile_tab_now(st, index, true);
        },
        Err(e) => {
            eprintln!("[known_hosts] 写入失败: {e}");
            st.ui.set_hostkey_error(format!("写入 known_hosts 失败: {e}").as_str().into());
            st.hostkey_prompt = Some((index, cache_key, verdict));
        },
    }
}

/// 用户选择「只在终端确认」/预检失败继续：本次不启用 askpass，交给 ssh 询问。
fn hostkey_once(st: &mut AppState) {
    let Some((index, _, _)) = st.hostkey_prompt.take() else {
        return;
    };
    st.ui.set_hostkey_open(false);
    st.ui.set_hostkey_error("".into());
    open_profile_tab_now(st, index, false);
}

fn hostkey_cancel(st: &mut AppState) {
    if let Some(job) = st.hostkey_job.take() {
        job.cancel();
    }
    st.hostkey_pending = None;
    st.hostkey_prompt = None;
    st.ui.set_hostkey_open(false);
    st.ui.set_hostkey_error("".into());
    eprintln!("[known_hosts] 用户取消连接");
}

fn open_profile_tab_now(st: &mut AppState, index: usize, allow_askpass: bool) {
    let (spec, askpass) = match build_profile_launch_with(st, index, allow_askpass) {
        Ok(v) => v,
        Err(err @ (ProfileLaunchError::NotFound | ProfileLaunchError::UnknownKind(_))) => {
            eprintln!("[profile] 打开失败: {}", err.message());
            return;
        },
        Err(ProfileLaunchError::VaultLocked) => {
            prompt_vault_unlock(st, "Vault 已锁定：请先在 Vault 页解锁，再连接该 SSH Profile");
            return;
        },
        Err(err @ ProfileLaunchError::Vault(_)) => {
            prompt_profile_password(
                st,
                index,
                &format!("{}；请解锁 Vault 并重新保存密码", err.message()),
            );
            return;
        },
        Err(err @ ProfileLaunchError::Askpass(_)) => {
            prompt_profile_password(st, index, &err.message());
            return;
        },
    };
    let id = st.shared.alloc_pane_id();
    let tab_title = spec.name.clone();
    let kind = spec.kind.clone();
    match create_pane(st, id, st.content, None, Some(&spec), askpass) {
        Ok(rt) => {
            st.panes.insert(id, rt);
            st.ws.tabs.push(Tab::single(id, tab_title.clone()));
            st.ws.active = st.ws.tabs.len() - 1;
            st.ui.set_profiles_open(false);
            // 从设置页的 Profiles 列表直接开 Tab 时，顺手退出设置页，
            // 否则新终端被设置页盖住，看起来像“没反应”。
            st.ui.set_settings_open(false);
            st.ui.set_settings_theme_open(false);
            st.ui.set_settings_query("".into());
            st.ui.set_settings_focus("".into());
            st.ui.invoke_focus_terminal();
            dispatch_plugin_event(
                st,
                plugin_v3::PluginEvent::TabOpen,
                plugin_v3::tab_open_payload(&tab_title, &kind),
            );
            refresh_views(st);
            rebuild_tabs(st);
            redraw_pane_inner(st, id);
            eprintln!(
                "[profile] open {:?} kind={kind} appearance font_size={:?} theme={:?} cursor={:?}/{:?} scrollback={:?} cell={:?}",
                spec.name,
                spec.effective_font_size(),
                spec.effective_theme(),
                spec.effective_cursor_shape_name(),
                spec.effective_cursor_blink(),
                spec.effective_scrollback(),
                st.panes.get(&id).map(|rt| rt.cell)
            );
        },
        Err(e) => eprintln!("[profile] 打开 {:?} 失败: {e}", spec.name),
    }
}

/// SSH 进程结束：只对 SSH Profile 弹横幅（本地 shell/串口退出不打扰）。
fn note_ssh_exit(
    st: &mut AppState,
    pane: PaneId,
    profile_id: Option<u64>,
    code: u32,
    signal: Option<String>,
) {
    let Some(profile) = profile_id.and_then(|id| st.profiles.get(id)).cloned() else {
        return;
    };
    if profile.kind != "ssh" {
        return;
    }
    let text = match signal.as_deref() {
        Some(signal) => format!("{}（{}）· 进程被信号终止: {signal}", profile.name, profile.host),
        None => format!("{}（{}）· 退出码 {code}", profile.name, profile.host),
    };
    eprintln!(
        "[ssh] ts_ms={} pane={pane} profile={} host={} exit={code} signal={:?}",
        logging::now_ms(),
        profile.id,
        profile.host.trim(),
        signal
    );
    st.ssh_exit = Some(SshExitNotice {
        pane,
        profile_id,
        text: text.clone(),
    });
    st.ui.set_ssh_exit_text(text.as_str().into());
    st.ui.set_ssh_exit_reconnectable(profile_id.is_some());
    st.ui.set_ssh_exit_open(true);
}

fn dismiss_ssh_exit(st: &mut AppState) {
    st.ssh_exit = None;
    st.ui.set_ssh_exit_open(false);
    st.ui.set_ssh_exit_text("".into());
}

/// 重连：复用原 pane 的格子/外观，按当前 Profile 配置重新拉起 ssh。
///
/// 不自动循环重连（避免断网时刷屏/打爆服务器）；主密码在 Vault 里且已解锁时
/// 仍走 askpass。主机密钥变更由 ssh 自己拒绝（askpass 对非密码 prompt 回答 no）。
fn reconnect_ssh_exit(st: &mut AppState) {
    let Some(notice) = st.ssh_exit.take() else {
        return;
    };
    st.ui.set_ssh_exit_open(false);
    let Some(profile_id) = notice.profile_id else {
        return;
    };
    let Some(index) = st.profiles.profiles.iter().position(|p| p.id == profile_id) else {
        eprintln!("[ssh] 重连失败：Profile {profile_id} 已被删除");
        return;
    };
    let Some(rect) = st.panes.get(&notice.pane).map(|rt| rt.rect) else {
        eprintln!("[ssh] 重连失败：pane {} 已关闭", notice.pane);
        return;
    };
    let (spec, askpass) = match build_profile_launch_with(st, index, true) {
        Ok(v) => v,
        Err(ProfileLaunchError::VaultLocked) => {
            prompt_vault_unlock(st, "Vault 已锁定：请先在 Vault 页解锁，再重连该 SSH Profile");
            return;
        },
        Err(e) => {
            let text = format!("重连失败: {}", e.message());
            eprintln!("[ssh] {text}");
            st.ssh_exit = Some(SshExitNotice {
                pane: notice.pane,
                profile_id: Some(profile_id),
                text: text.clone(),
            });
            st.ui.set_ssh_exit_text(text.as_str().into());
            st.ui.set_ssh_exit_reconnectable(true);
            st.ui.set_ssh_exit_open(true);
            return;
        },
    };
    // 旧 runtime 已经退出；换掉它但保留 pane id（Tab/分屏布局不变）。
    st.panes.remove(&notice.pane);
    match create_pane(st, notice.pane, rect, None, Some(&spec), askpass) {
        Ok(rt) => {
            st.panes.insert(notice.pane, rt);
            if let Some(tab) = st.ws.tabs.iter_mut().find(|t| t.panes().contains(&notice.pane)) {
                tab.title = spec.name.clone();
            }
            refresh_views(st);
            rebuild_tabs(st);
            redraw_pane_inner(st, notice.pane);
            eprintln!("[ssh] 已重连 pane={} profile={profile_id}", notice.pane);
        },
        Err(e) => {
            let text = format!("重连失败: {e}");
            eprintln!("[ssh] {text}");
            st.ssh_exit = Some(SshExitNotice {
                pane: notice.pane,
                profile_id: Some(profile_id),
                text: text.clone(),
            });
            st.ui.set_ssh_exit_text(text.as_str().into());
            st.ui.set_ssh_exit_reconnectable(true);
            st.ui.set_ssh_exit_open(true);
        },
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
    let shell = st
        .cfg
        .shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_default();
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
    st.ui
        .set_form_user(std::env::var("USER").unwrap_or_default().as_str().into());
    st.ui.set_form_auth("agent".into());
    st.ui.set_form_key("".into());
    st.ui.set_form_password("".into());
    st.ui.set_form_save_password(false);
    st.ui.set_form_extra("".into());
    st.ui.set_form_forwards("".into());
    st.ui.set_form_remote_forwards("".into());
    st.ui.set_form_dynamic_forwards("".into());
    st.ui.set_form_proxy_jump("".into());
    st.ui.set_form_agent_forward(false);
    st.ui.set_form_serial_port("/dev/ttyUSB0".into());
    st.ui.set_form_baud("115200".into());
    st.ui.set_form_font_size("".into());
    st.ui.set_form_theme("".into());
    st.ui.set_form_cursor_shape("".into());
    st.ui.set_form_cursor_blink("".into());
    st.ui.set_form_scrollback("".into());
    st.ui.set_form_error("".into());
    st.ui.set_profile_form_open(true);
    eprintln!("[profile] 打开新建表单 name={name:?} shell={shell:?} cwd={cwd:?}");
}

/// 打开编辑表单：回填已有 Profile 的所有字段。
fn edit_profile_form(st: &mut AppState, index: usize) {
    let Some(p) = st.profiles.profiles.get(index).cloned() else {
        return;
    };
    let kind_idx = profile::KINDS
        .iter()
        .position(|k| *k == p.kind)
        .unwrap_or(0);
    st.ui.set_form_edit_id(p.id as i32);
    st.ui.set_form_name(p.name.as_str().into());
    st.ui.set_form_kind(kind_idx as i32);
    st.ui.set_form_shell(p.shell.as_str().into());
    st.ui.set_form_args(p.args.join(" ").as_str().into());
    st.ui
        .set_form_cwd(p.cwd.clone().unwrap_or_default().as_str().into());
    st.ui.set_form_host(p.host.as_str().into());
    st.ui.set_form_port(p.port.to_string().as_str().into());
    st.ui.set_form_user(p.user.as_str().into());
    st.ui.set_form_auth(p.auth.as_str().into());
    st.ui.set_form_key(p.key_path.as_str().into());
    // 密码绝不回填到 UI；勾选状态反映配置，真正密码只在 Vault 解锁时可写入。
    st.ui.set_form_password("".into());
    st.ui.set_form_save_password(p.save_password);
    st.ui.set_form_extra(p.extra_args.join(" ").as_str().into());
    st.ui
        .set_form_forwards(p.local_forwards.join(" ").as_str().into());
    st.ui.set_form_remote_forwards(
        p.remote_forwards.join(" ").as_str().into(),
    );
    st.ui.set_form_dynamic_forwards(
        p.dynamic_forwards.join(" ").as_str().into(),
    );
    st.ui.set_form_proxy_jump(p.proxy_jump.as_str().into());
    st.ui.set_form_agent_forward(p.agent_forward);
    st.ui.set_form_serial_port(p.serial_port.as_str().into());
    st.ui.set_form_baud(p.baud.to_string().as_str().into());
    st.ui.set_form_font_size(
        p.font_size
            .map(|v| v.to_string())
            .unwrap_or_default()
            .as_str()
            .into(),
    );
    st.ui
        .set_form_theme(p.theme.clone().unwrap_or_default().as_str().into());
    st.ui.set_form_cursor_shape(
        p.cursor_shape
            .clone()
            .unwrap_or_default()
            .as_str()
            .into(),
    );
    st.ui.set_form_cursor_blink(
        p.cursor_blink
            .clone()
            .unwrap_or_default()
            .as_str()
            .into(),
    );
    st.ui.set_form_scrollback(
        p.scrollback
            .map(|v| v.to_string())
            .unwrap_or_default()
            .as_str()
            .into(),
    );
    st.ui.set_form_error("".into());
    st.ui.set_profile_form_open(true);
    eprintln!(
        "[profile] 编辑表单 id={} name={:?} kind={}",
        p.id, p.name, p.kind
    );
}

fn save_profile_form(st: &mut AppState) {
    let kind_idx = st.ui.get_form_kind().clamp(0, 3) as usize;
    let kind = profile::KINDS[kind_idx];
    let name = st.ui.get_form_name().to_string();
    let parse_port = |s: String| s.trim().parse::<u16>().unwrap_or(0);
    let edit_id = st.ui.get_form_edit_id();
    let mut p = profile::Profile {
        name,
        kind: kind.to_string(),
        ..profile::Profile::default()
    };
    match kind {
        "ssh" => {
            p.host = st.ui.get_form_host().to_string();
            p.port = parse_port(st.ui.get_form_port().to_string());
            p.user = st.ui.get_form_user().to_string();
            p.auth = st.ui.get_form_auth().to_string();
            p.key_path = st.ui.get_form_key().to_string();
            p.extra_args = st
                .ui
                .get_form_extra()
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            p.local_forwards = st
                .ui
                .get_form_forwards()
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            p.remote_forwards = st
                .ui
                .get_form_remote_forwards()
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            p.dynamic_forwards = st
                .ui
                .get_form_dynamic_forwards()
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            p.proxy_jump = st.ui.get_form_proxy_jump().trim().to_string();
            p.agent_forward = st.ui.get_form_agent_forward();
        }
        "serial" => {
            p.serial_port = st.ui.get_form_serial_port().to_string();
            p.baud = st.ui.get_form_baud().trim().parse::<u32>().unwrap_or(0);
        }
        "telnet" => {
            p.host = st.ui.get_form_host().to_string();
            p.port = parse_port(st.ui.get_form_port().to_string());
        }
        _ => {
            p.shell = st.ui.get_form_shell().to_string();
            p.args = st
                .ui
                .get_form_args()
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            let cwd = st.ui.get_form_cwd().to_string();
            p.cwd = (!cwd.trim().is_empty()).then(|| cwd.trim().to_string());
        }
    }
    // ---- 外观（可选；空 = 继承全局）----
    let trim_opt = |s: String| -> Option<String> {
        let t = s.trim().to_string();
        (!t.is_empty()).then_some(t)
    };
    let font_size = match trim_opt(st.ui.get_form_font_size().to_string()) {
        None => None,
        Some(raw) => match raw.parse::<f32>() {
            Ok(v) => Some(v),
            Err(_) => {
                let msg = format!(
                    "字号无效: {raw}（应为 {:.0}–{:.0}）",
                    profile::MIN_FONT_SIZE,
                    profile::MAX_FONT_SIZE
                );
                st.ui.set_form_error(msg.as_str().into());
                return;
            },
        },
    };
    let scrollback = match trim_opt(st.ui.get_form_scrollback().to_string()) {
        None => None,
        Some(raw) => match raw.parse::<usize>() {
            Ok(v) if v > 0 => Some(v),
            _ => {
                let msg = format!("scrollback 无效: {raw}（应为正整数，留空继承全局）");
                st.ui.set_form_error(msg.as_str().into());
                return;
            },
        },
    };
    p.font_size = font_size;
    p.theme = trim_opt(st.ui.get_form_theme().to_string());
    p.cursor_shape = trim_opt(st.ui.get_form_cursor_shape().to_string());
    p.cursor_blink =
        trim_opt(st.ui.get_form_cursor_blink().to_string()).map(|s| s.to_ascii_lowercase());
    p.scrollback = scrollback;

    // ---- Vault 密码：只在 auth=password 且勾选保存时处理 ----
    let password = st.ui.get_form_password().to_string();
    let save_checked = st.ui.get_form_save_password();
    p.save_password = kind == "ssh" && p.auth == "password" && save_checked;
    let save_password = p.save_password;
    let previous_save = if edit_id > 0 {
        st.profiles
            .get(edit_id as u64)
            .map(|q| q.save_password)
            .unwrap_or(false)
    } else {
        false
    };

    // 先做 Vault 前置校验，任何错误都不改动 ProfileStore。
    if p.save_password {
        if !password.is_empty() {
            if !st.vault.is_unlocked() {
                let msg = match st.vault.status() {
                    vault::VaultStatus::Uninitialized => {
                        "Vault 未初始化：请先在 Settings → Vault 设置主密码，再保存密码"
                    }
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
        st.profiles
            .update(edit_id as u64, p)
            .map(|_| edit_id as u64)
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
                    Ok(_) => {}
                    Err(vault::VaultError::Locked | vault::VaultError::NotInitialized) => {
                        eprintln!("[vault] Vault 锁定，Profile {id} 的旧密码暂留（已被取消勾选，不再使用）");
                    }
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
            refocus_after_modal(st);
            eprintln!("[profile] 保存 id={id} kind={kind}（共 {}）", st.profiles.profiles.len());
        },
        Err(e) => {
            eprintln!("[profile] 表单校验失败: {e}");
            st.ui.set_form_error(e.as_str().into());
        }
    }
}

// ------------------------------------------------------------ Vault（加密凭据）

/// 后台 KDF 成功后用于选择提示文案；不携带任何密码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VaultAction {
    Initialize,
    Unlock,
    ChangeMaster,
}

/// Vault 状态文字（不含任何秘密）。跟随 UI 语言。
fn vault_status_text(v: &vault::Vault, lang: &str) -> String {
    match v.status() {
        vault::VaultStatus::Uninitialized => ui_text(
            lang,
            "Not initialized: set a master password to create vault.enc",
            "未初始化：设置主密码后创建 vault.enc",
        )
        .into(),
        vault::VaultStatus::Locked => ui_text(
            lang,
            "Locked: enter the master password and click Unlock",
            "已锁定：输入主密码后点解锁",
        )
        .into(),
        vault::VaultStatus::Unlocked => {
            if lang == "zh" {
                format!("已解锁 · {} 条凭据", v.entry_count())
            } else {
                format!("Unlocked · {} credential(s)", v.entry_count())
            }
        },
    }
}

/// 刷新设置页 Vault 状态/路径/条数（不清空错误提示）。
fn refresh_vault_ui(st: &mut AppState) {
    let lang = st.cfg.lang();
    let status = if st.vault_busy {
        ui_text(
            lang,
            "Deriving key in background (Argon2id)…",
            "正在后台派生密钥（Argon2id）…",
        )
        .to_string()
    } else {
        vault_status_text(&st.vault, lang)
    };
    let path = st.vault.path().display().to_string();
    let entries = st.vault.entry_count() as i32;
    st.ui.set_vault_status(status.as_str().into());
    st.ui.set_vault_path(path.as_str().into());
    st.ui.set_vault_entries(entries);
    st.ui.set_vault_busy(st.vault_busy);
}

/// 后台任务启动前统一防重入；返回 true = 已有任务在跑。
fn vault_busy_guard(st: &mut AppState) -> bool {
    if st.vault_busy {
        vault_fail(st, ui_text(st.cfg.lang(), "Vault is deriving a key in the background; try again shortly (this page stays usable)", "Vault 正在后台派生密钥，请稍候（设置页仍可操作）"));
        return true;
    }
    false
}

fn start_vault_task(st: &mut AppState, action: VaultAction, task: vault::VaultTask) {
    st.vault_busy = true;
    st.vault_action = Some(action);
    st.vault_job = Some(task);
    // 密码输入框立刻清空：后台任务已经拿到自己的副本，UI 不再持有明文。
    st.ui.set_vault_password("".into());
    refresh_vault_ui(st);
    st.ui.set_vault_notice("".into());
    st.ui.set_vault_error("".into());
    eprintln!("[vault] 后台 KDF 开始 action={action:?}");
}

/// Vault 后台任务轮询：成功才 adopt；失败保持原锁定状态。
fn poll_vault(st: &mut AppState) {
    let Some(job) = st.vault_job.as_ref() else {
        return;
    };
    use std::sync::mpsc::TryRecvError;
    match job.try_recv() {
        Ok(vault::VaultTaskOutcome::Ok(session)) => {
            let entries = session.entry_count();
            st.vault.adopt(session);
            st.vault_job = None;
            st.vault_busy = false;
            let action = st.vault_action.take();
            refresh_vault_ui(st);
            match action {
                Some(VaultAction::Initialize) => {
                    vault_notice(st, ui_text(st.cfg.lang(), "Master password set; Vault is unlocked (empty vault)", "主密码已设置，Vault 已解锁（空 vault）"));
                    eprintln!("[vault] 后台初始化完成");
                },
                Some(VaultAction::ChangeMaster) => {
                    vault_notice(st, ui_text(st.cfg.lang(), "Master password updated; credentials re-encrypted with the new key", "主密码已更新；凭据已用新密钥重新加密"));
                    eprintln!("[vault] 后台改主密码完成");
                },
                _ => {
                    vault_notice(st, if st.cfg.lang() == "zh" { format!("已解锁 · {entries} 条凭据") } else { format!("Unlocked · {entries} credential(s)") });
                    eprintln!("[vault] 后台解锁完成 entries={entries}");
                },
            }
        },
        Ok(vault::VaultTaskOutcome::Err(e)) => {
            st.vault_job = None;
            st.vault_busy = false;
            st.vault_action = None;
            if e == vault::VaultError::NotInitialized {
                st.vault.refresh_status();
            }
            refresh_vault_ui(st);
            vault_fail(st, e.to_string());
        },
        Err(TryRecvError::Empty) => {},
        Err(TryRecvError::Disconnected) => {
            st.vault_job = None;
            st.vault_busy = false;
            st.vault_action = None;
            refresh_vault_ui(st);
            vault_fail(st, ui_text(st.cfg.lang(), "Vault background task ended unexpectedly (no result)", "Vault 后台任务异常结束（没有结果）"));
        },
    }
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
/// 已锁定 → 必须先解锁（避免绕过旧主密码）。Argon2 全部在后台线程。
fn vault_set_master_password(st: &mut AppState) {
    st.vault_clear_armed = false;
    if vault_busy_guard(st) {
        return;
    }
    let master = st.ui.get_vault_password().to_string();
    if master.is_empty() {
        vault_fail(st, ui_text(st.cfg.lang(), "Enter the master password", "请输入主密码"));
        return;
    }
    let path = st.vault.path().to_path_buf();
    match st.vault.status() {
        vault::VaultStatus::Uninitialized => {
            let task = vault::VaultTask::initialize(path, master, st.app_wake.clone());
            start_vault_task(st, VaultAction::Initialize, task);
        },
        vault::VaultStatus::Unlocked => {
            let snapshot = match st.vault.change_snapshot() {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    vault_fail(st, e.to_string());
                    return;
                },
            };
            let task =
                vault::VaultTask::change_master(path, snapshot, master, st.app_wake.clone());
            start_vault_task(st, VaultAction::ChangeMaster, task);
        },
        vault::VaultStatus::Locked => {
            vault_fail(st, ui_text(st.cfg.lang(), "Vault is locked: unlock with the current master password before changing it", "Vault 已锁定：先用当前主密码解锁，再修改主密码"));
        },
    }
}

fn vault_unlock(st: &mut AppState) {
    st.vault_clear_armed = false;
    if vault_busy_guard(st) {
        return;
    }
    let master = st.ui.get_vault_password().to_string();
    if master.is_empty() {
        vault_fail(st, ui_text(st.cfg.lang(), "Enter the master password", "请输入主密码"));
        return;
    }
    let task = vault::VaultTask::unlock(st.vault.path().to_path_buf(), master, st.app_wake.clone());
    start_vault_task(st, VaultAction::Unlock, task);
}

fn vault_lock(st: &mut AppState) {
    if st.vault_busy {
        vault_fail(st, ui_text(st.cfg.lang(), "Vault is deriving a key; lock again when it finishes", "Vault 正在后台派生密钥，完成后再锁定"));
        return;
    }
    if st.vault.status() == vault::VaultStatus::Uninitialized {
        vault_fail(st, ui_text(st.cfg.lang(), "Vault is not initialized: set a master password first", "Vault 尚未初始化：先设置主密码"));
        return;
    }
    st.vault.lock();
    st.ui.set_vault_password("".into());
    st.vault_clear_armed = false;
    refresh_vault_ui(st);
    vault_notice(st, ui_text(st.cfg.lang(), "Locked: keys/credentials in memory were discarded", "已锁定：内存中的密钥/凭据已丢弃"));
    eprintln!("[vault] 已锁定");
}

fn vault_clear(st: &mut AppState) {
    if st.vault_busy {
        vault_fail(st, ui_text(st.cfg.lang(), "Vault is deriving a key; clear again when it finishes", "Vault 正在后台派生密钥，完成后再清空"));
        return;
    }
    if !st.vault.is_unlocked() {
        vault_fail(st, ui_text(st.cfg.lang(), "Vault is locked; cannot clear", "Vault 未解锁，无法清空"));
        return;
    }
    if !st.vault_clear_armed {
        st.vault_clear_armed = true;
        let n = st.vault.entry_count();
        let notice = if st.cfg.lang() == "zh" {
            format!("再次点击 Clear 确认清空 {n} 条凭据（主密码保留）")
        } else {
            format!("Click Clear again to erase {n} credential(s) (master password kept)")
        };
        vault_notice(st, notice);
        return;
    }
    st.vault_clear_armed = false;
    match st.vault.clear() {
        Ok(()) => {
            refresh_vault_ui(st);
            vault_notice(st, ui_text(st.cfg.lang(), "All credentials cleared (vault.enc and master password kept)", "已清空全部凭据（vault.enc 与主密码保留）"));
            eprintln!("[vault] 已清空全部凭据");
        }
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
    let names: Vec<slint::SharedString> = indices
        .iter()
        .map(|i| st.profiles.profiles[*i].name.clone().into())
        .collect();
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
    refresh_sftp_queue(st);
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
    st.ui.set_sftp_op_active(st.sftp_op.is_some());
}

/// 刷新队列行（当前失败项可重试）、队列计数/按钮状态。
fn refresh_sftp_queue(st: &mut AppState) {
    let mut items: Vec<slint::SharedString> =
        st.sftp_queue.labels(2).into_iter().map(Into::into).collect();
    if let Some(failed) = &st.sftp_last_failed {
        if st.sftp_transfer.is_none() {
            items.push(format!("失败 · 可重试: {}", failed.queue_label()).into());
        }
    }
    let count = st.sftp_queue.len() as i32;
    st.sftp_queue_model.set_vec(items);
    st.ui.set_sftp_queue_count(count);
    st.ui.set_sftp_retry_available(st.sftp_last_failed.is_some());
}

fn sftp_update_status(st: &mut AppState) {
    let status = st.sftp_transfer_state.status_text();
    if st.ui.get_sftp_status().as_str() != status.as_str() {
        st.ui.set_sftp_status(status.as_str().into());
    }
}

/// 启动队列里的一个请求（不做并发检查：调用方保证没有活动 job）。
fn sftp_launch_request(st: &mut AppState, request: sftp::TransferRequest) -> bool {
    let direction = request.direction;
    let local = request.local.clone();
    let remote = request.remote.clone();
    let recursive = request.recursive;
    match sftp::TransferJob::start_woken(&request, st.app_wake.clone()) {
        Ok(job) => {
            st.sftp_transfer = Some(job);
            st.sftp_current_request = Some(request);
            st.sftp_transfer_state =
                sftp::TransferState::running(direction, local.clone(), remote.clone());
            sftp_show_transfer(st);
            refresh_sftp_queue(st);
            let status = st.sftp_transfer_state.status_text();
            sftp_set_status(st, &status);
            eprintln!(
                "[sftp] {} 开始 local={local:?} remote={remote:?} recursive={recursive} queue={}",
                direction.verb(),
                st.sftp_queue.len()
            );
            true
        },
        Err(e) => {
            let msg = format!("{}失败: {e}", direction.verb());
            sftp_set_status(st, &msg);
            st.sftp_last_failed = Some(request);
            sftp_show_transfer(st);
            refresh_sftp_queue(st);
            false
        },
    }
}

/// 发起一次传输：空闲则立即开始；已有传输时加入队列（可连续点多次）。
fn sftp_submit_transfer(st: &mut AppState, request: sftp::TransferRequest) {
    if st.sftp_transfer.is_some() {
        let n = st.sftp_queue.push(request);
        refresh_sftp_queue(st);
        sftp_set_status(st, &format!("已加入队列（{n} 项），当前传输完成后自动开始"));
        eprintln!("[sftp] 传输已入队 queue={n}");
        return;
    }
    sftp_launch_request(st, request);
}

/// 启动一次后台传输（从当前 UI 选择构造请求）。
fn sftp_start_transfer(
    st: &mut AppState,
    direction: sftp::TransferDirection,
    local: String,
    remote: String,
) {
    if st.sftp_op.is_some() {
        sftp_set_status(st, "远端元数据操作进行中，请稍候（10s 超时/可取消）");
        return;
    }
    let Some(target) = sftp_current_target(st) else {
        sftp_set_status(st, "请选择 SSH Profile");
        return;
    };
    // 下载始终 get -R（普通文件安全）；上传只有本地是目录才 put -R。
    let recursive =
        direction == sftp::TransferDirection::Upload && std::path::Path::new(&local).is_dir();
    let request = sftp::TransferRequest { direction, target, local, remote, recursive };
    sftp_submit_transfer(st, request);
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
            }
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
    let finished_request = st.sftp_current_request.take();
    if phase == sftp::TransferPhase::Failed {
        // 保留失败请求，Retry 按钮可重新入队（不自动无限重试）。
        st.sftp_last_failed = finished_request;
    } else if phase == sftp::TransferPhase::Done {
        st.sftp_last_failed = None;
    }
    sftp_show_transfer(st);
    refresh_sftp_queue(st);
    sftp_set_status(st, &status);
    eprintln!(
        "[sftp] ts_ms={} 传输结束 phase={phase:?} queue={}",
        logging::now_ms(),
        st.sftp_queue.len()
    );
    if upload_done && st.sftp_queue.is_empty() {
        // 上传完成后刷新远端列表，但保留“已上传”结果文案。
        sftp_do_list(st);
        sftp_set_status(st, &status);
    }
    // 当前任务结束（成功/失败/取消）后自动开始下一个排队项。
    if st.sftp_transfer.is_none() {
        if let Some(next) = st.sftp_queue.pop() {
            eprintln!("[sftp] 队列取出下一个: {}", next.queue_label());
            sftp_launch_request(st, next);
        }
    }
}

/// Cancel：优先取消传输；没有传输时取消正在跑的远端元数据操作。
fn sftp_cancel_active(st: &mut AppState) {
    if st.sftp_transfer.is_some() {
        sftp_cancel_transfer(st);
        return;
    }
    if let Some(job) = st.sftp_op.as_ref() {
        job.cancel();
        sftp_set_status(st, "正在取消远端操作…");
        return;
    }
    sftp_set_status(st, "没有进行中的远端操作");
}

/// Retry：把最近失败的传输重新插到队首（若有活动任务则等待其结束）。
fn sftp_retry_failed(st: &mut AppState) {
    let Some(request) = st.sftp_last_failed.take() else {
        sftp_set_status(st, "没有可重试的失败传输");
        return;
    };
    if st.sftp_transfer.is_some() {
        let n = st.sftp_queue.push_front(request);
        sftp_set_status(st, &format!("已把重试项插到队首（队列 {n} 项）"));
        refresh_sftp_queue(st);
        return;
    }
    sftp_launch_request(st, request);
}

/// 清空尚未开始的排队项（当前传输不受影响）。
fn sftp_clear_queue(st: &mut AppState) {
    let n = st.sftp_queue.clear();
    refresh_sftp_queue(st);
    sftp_set_status(st, &format!("已清空队列（{n} 项）"));
    eprintln!("[sftp] 清空队列 items={n}");
}

fn sftp_do_chmod(st: &mut AppState) {
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
    let mode = match sftp::parse_mode(st.ui.get_sftp_mode().as_str()) {
        Ok(mode) => mode,
        Err(e) => {
            sftp_set_status(st, &e);
            return;
        },
    };
    let remote = remote_entry_path(&st.ui.get_sftp_remote(), &item);
    sftp_start_op(
        st,
        sftp::SftpCommand::Chmod { target, remote, mode },
        "修改权限中…（后台）",
    );
}

fn sftp_cancel_transfer(st: &mut AppState) {
    let Some(job) = st.sftp_transfer.as_ref() else {
        sftp_set_status(st, "没有进行中的传输");
        return;
    };
    job.cancel();
    sftp_set_status(st, "正在取消传输…");
}

/// 有 SFTP 子进程/后台操作在进行时阻止并发面板操作。
fn sftp_busy(st: &mut AppState) -> bool {
    if st.sftp_op.is_some() {
        sftp_set_status(st, "远端操作进行中，请稍候（10s 超时）");
        true
    } else if st.sftp_transfer.is_some() {
        sftp_set_status(st, "传输进行中，完成或取消后再操作");
        true
    } else {
        false
    }
}

/// 启动元数据后台任务：只 spawn 线程，Slint 回调立即返回，UI 不冻结。
fn sftp_start_op(st: &mut AppState, command: sftp::SftpCommand, status: &str) {
    eprintln!("[sftp] 后台操作开始: {command:?}（timeout=10s）");
    sftp_set_status(st, status);
    st.sftp_op = Some(sftp::SftpOpJob::start_woken(command, st.app_wake.clone()));
    // 立刻把 op-active 写进 UI：否则用户看不到 Cancel（慢连接尤其明显）。
    sftp_show_transfer(st);
    // 操作期间 100ms 收一次结果；完成/失败即 stop，空闲时零开销。
    let window_id = st.window_id;
    st.sftp_op_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let _ = runtime::with_window(window_id, poll_sftp_op);
        },
    );
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
    sftp_start_op(
        st,
        sftp::SftpCommand::List { target, remote },
        "列目录中…（后台）",
    );
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
    // 传输进行中允许继续入队（队列会依次执行）；只拒绝与元数据操作并发。
    if st.sftp_op.is_some() {
        sftp_set_status(st, "远端元数据操作进行中，请稍候（10s 超时/可取消）");
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
    if st.sftp_op.is_some() {
        sftp_set_status(st, "远端元数据操作进行中，请稍候（10s 超时/可取消）");
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
    sftp_start_op(
        st,
        sftp::SftpCommand::Mkdir { target, remote },
        "建目录中…（后台）",
    );
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
    sftp_start_op(
        st,
        sftp::SftpCommand::Rename { target, from, to },
        "重命名中…（后台）",
    );
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
    sftp_start_op(
        st,
        sftp::SftpCommand::Remove { target, remote: path },
        "删除中…（后台）",
    );
}

/// 400ms poll：非阻塞收后台元数据结果；收到后清空 job 并更新列表/状态。
fn poll_sftp_op(st: &mut AppState) {
    let event = st.sftp_op.as_ref().map(|job| job.try_recv());
    match event {
        Some(Ok(outcome)) => {
            st.sftp_op = None;
            st.sftp_op_timer.stop();
            eprintln!(
                "[sftp] ts_ms={} 后台操作完成: {}",
                logging::now_ms(),
                outcome.describe()
            );
            apply_sftp_outcome(st, outcome);
            sftp_show_transfer(st);
        },
        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
            st.sftp_op = None;
            st.sftp_op_timer.stop();
            eprintln!("[sftp] 后台操作异常结束（worker 没有发出结果）");
            sftp_set_status(st, "远端操作异常结束（没有结果）");
            sftp_show_transfer(st);
        },
        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {},
        None => {
            // 没有 job 时 timer 不应继续跑（防御性：避免异常路径留下空转）。
            st.sftp_op_timer.stop();
        },
    }
}

fn apply_sftp_outcome(st: &mut AppState, outcome: sftp::SftpOutcome) {
    match outcome {
        sftp::SftpOutcome::List { items: Ok(items), .. } => {
            st.sftp_delete_armed = false;
            let n = items.len();
            let names: Vec<slint::SharedString> = items.into_iter().map(|s| s.into()).collect();
            st.sftp_model.set_vec(names);
            st.ui.set_sftp_selected(-1);
            sftp_set_status(st, &format!("{n} 个条目"));
        },
        sftp::SftpOutcome::List { items: Err(e), .. } => {
            sftp_set_status(st, &format!("列目录失败: {e}"));
        },
        sftp::SftpOutcome::Mkdir { remote, result: Ok(()) } => {
            sftp_set_status(st, &format!("已创建 {remote}"));
            sftp_do_list(st);
        },
        sftp::SftpOutcome::Mkdir { result: Err(e), .. } => {
            sftp_set_status(st, &format!("建目录失败: {e}"));
        },
        sftp::SftpOutcome::Rename { from, to, result: Ok(()) } => {
            sftp_set_status(st, &format!("已重命名 {from} → {to}"));
            sftp_do_list(st);
        },
        sftp::SftpOutcome::Rename { result: Err(e), .. } => {
            sftp_set_status(st, &format!("重命名失败: {e}"));
        },
        sftp::SftpOutcome::Remove { path, result: Ok(()) } => {
            sftp_set_status(st, &format!("已删除 {path}"));
            sftp_do_list(st);
        },
        sftp::SftpOutcome::Remove { result: Err(e), .. } => {
            sftp_set_status(st, &format!("删除失败: {e}"));
        },
        sftp::SftpOutcome::Chmod { path, mode, result: Ok(()) } => {
            // `ls -1` 不显示权限，列表内容不会变；保留成功文案而不是刷新成“N 个条目”。
            sftp_set_status(st, &format!("已把 {path} 权限改为 {mode:04o}"));
        },
        sftp::SftpOutcome::Chmod { path, result: Err(e), .. } => {
            sftp_set_status(st, &format!("修改 {path} 权限失败: {e}"));
        },
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
    let names: Vec<slint::SharedString> = indices
        .iter()
        .map(|i| st.profiles.profiles[*i].name.clone().into())
        .collect();
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
    let Some(profile) = st.profiles.profiles.get_mut(profile_index) else {
        return;
    };
    if profile.local_forwards.iter().any(|f| f == &spec) {
        st.ui.set_fwd_status("这条规则已存在".into());
        return;
    }
    profile.local_forwards.push(spec.clone());
    st.profiles.save();
    st.ui.set_fwd_new("".into());
    refresh_forwards(st);
    st.ui
        .set_fwd_status(format!("已添加 -L {spec}（下次连接生效）").as_str().into());
    eprintln!("[fwd] add {spec}");
}

fn fwd_delete(st: &mut AppState, index: usize) {
    let idx = st.ui.get_fwd_profile().max(0) as usize;
    let Some(profile_index) = st.fwd_profile_indices.get(idx).copied() else {
        return;
    };
    let Some(profile) = st.profiles.profiles.get_mut(profile_index) else {
        return;
    };
    if index >= profile.local_forwards.len() {
        return;
    }
    let spec = profile.local_forwards.remove(index);
    st.profiles.save();
    refresh_forwards(st);
    st.ui
        .set_fwd_status(format!("已删除 -L {spec}").as_str().into());
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
    eprintln!(
        "[session] reattach pane={id} title={title:?}（剩余后台 {}）",
        st.detached.len()
    );
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
    dispatch_plugin_event(
        st,
        plugin_v3::PluginEvent::TabClose,
        plugin_v3::tab_close_payload(&tab_title),
    );
    if st.ws.tabs.is_empty() {
        let _ = slint::quit_event_loop();
        return;
    }
    st.ws.active = st.ws.active.min(st.ws.tabs.len() - 1);
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
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
    let Some(tab) = st.ws.tabs.get(index) else {
        return;
    };
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
    eprintln!(
        "[ws] close other tabs, keep={keep}, 关闭 {} 个 pane",
        dropped.len()
    );
    for title in &closed_titles {
        dispatch_plugin_event(
            st,
            plugin_v3::PluginEvent::TabClose,
            plugin_v3::tab_close_payload(title),
        );
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
    let (title, focused, ids, tree, pinned) = {
        let Some(src) = st.ws.tabs.get(index) else {
            return;
        };
        let Some(root) = src.root.as_ref() else {
            return;
        };
        let ids = root.panes();
        let mut cwds = HashMap::with_capacity(ids.len());
        let mut profiles = HashMap::with_capacity(ids.len());
        for id in &ids {
            let cwd = st
                .panes
                .get(id)
                .and_then(|rt| rt.handle.pid())
                .and_then(session::cwd_of);
            cwds.insert(*id, cwd);
            profiles.insert(*id, st.panes.get(id).and_then(|rt| rt.profile_id));
        }
        (
            src.title.clone(),
            src.focused,
            ids,
            session::Tree::from_node_with_profiles(root, &cwds, &profiles),
            src.pinned,
        )
    };

    let leaf_cwds = tree.leaf_cwds();
    let leaf_profiles = tree.leaf_profiles();
    eprintln!("[ws] duplicate tab={index} panes={}", leaf_cwds.len());
    let new_ids: Vec<PaneId> = (0..leaf_cwds.len()).map(|_| st.shared.alloc_pane_id()).collect();
    let Some(new_root) = tree.assign(&new_ids) else {
        return;
    };
    let mut created = Vec::with_capacity(new_ids.len());
    for (i, cwd) in leaf_cwds.iter().enumerate() {
        let (profile, askpass) =
            restore_pane_launch(st, leaf_profiles.get(i).copied().flatten());
        match create_pane(
            st,
            new_ids[i],
            st.content,
            cwd.as_ref().map(PathBuf::from),
            profile.as_ref(),
            askpass,
        ) {
            Ok(rt) => {
                st.panes.insert(new_ids[i], rt);
                created.push(new_ids[i]);
            }
            Err(e) => {
                eprintln!("[ws] 复制 Tab 时创建 pane 失败: {e}");
                break;
            }
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
            pinned,
        },
    );
    st.ws.active = index + 1;
    dispatch_plugin_event(
        st,
        plugin_v3::PluginEvent::TabOpen,
        plugin_v3::tab_open_payload(&new_title, "local"),
    );
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
}

/// Tab 右键菜单 action 6：Pin/Unpin 当前 Tab。
///
/// Pin 移到最后一个 pinned 后；Unpin 回到第一个 unpinned 前。
/// 只影响 Tab 栏顺序，不重建 pane/PTY。
fn toggle_tab_pin(st: &mut AppState, index: usize) {
    let Some(tab) = st.ws.tabs.get(index) else {
        return;
    };
    let pinned = !tab.pinned;
    let Some(new_index) = st.ws.set_tab_pinned(index, pinned) else {
        return;
    };
    eprintln!(
        "[ws] {} tab {index} → {new_index}",
        if pinned { "pin" } else { "unpin" }
    );
    rebuild_tabs(st);
}

fn close_pane_inner(st: &mut AppState, id: PaneId) {
    let Some(tab_index) = st.ws.tab_of(id) else {
        return;
    };
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
        dispatch_plugin_event(
            st,
            plugin_v3::PluginEvent::TabClose,
            plugin_v3::tab_close_payload(&tab_title),
        );
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

// --------------------------------------------------------------- 会话快照

/// 把当前布局 + 每个 pane 的 cwd 收成快照（不含屏幕内容）。
/// 关闭 Tab 时抓一份布局/cwd 快照，供「重开最近关闭的 Tab」使用。
/// 只存布局与 cwd，不保留进程（重开是新 shell）。
fn snapshot_tab(st: &AppState, index: usize) -> Option<session::TabSnapshot> {
    let tab = st.ws.tabs.get(index)?;
    let root = tab.root.as_ref()?;
    let ids = root.panes();
    let mut cwds = HashMap::with_capacity(ids.len());
    let mut profiles = HashMap::with_capacity(ids.len());
    for id in &ids {
        let cwd = st
            .panes
            .get(id)
            .and_then(|rt| rt.handle.pid())
            .and_then(session::cwd_of);
        cwds.insert(*id, cwd);
        profiles.insert(*id, st.panes.get(id).and_then(|rt| rt.profile_id));
    }
    let focused = ids.iter().position(|id| *id == tab.focused).unwrap_or(0);
    Some(session::TabSnapshot {
        title: tab.title.clone(),
        focused,
        pinned: tab.pinned,
        tree: session::Tree::from_node_with_profiles(root, &cwds, &profiles),
    })
}

/// 会话/复制 Tab 恢复一个 pane 的 Profile 启动信息。
///
/// Profile 已删除、Vault 锁定、askpass 创建失败等一律回退普通 shell（返回
/// `None`），保证“恢复失败也能开出新 Tab”，不因为坏 Profile 拒绝启动。
fn restore_pane_launch(
    st: &AppState,
    profile_id: Option<u64>,
) -> (Option<profile::Profile>, Option<askpass::Askpass>) {
    let Some(profile_id) = profile_id else {
        return (None, None);
    };
    let Some(index) = st.profiles.profiles.iter().position(|p| p.id == profile_id) else {
        eprintln!("[session] Profile id={profile_id} 已删除，回退普通 shell");
        return (None, None);
    };
    // 只自动恢复 local Profile 的 shell/外观；SSH/Serial/Telnet 需要主动连接，
    // 会话恢复不自动建连（避免启动即连远端/Vault 弹窗），回退普通 shell。
    if st.profiles.profiles[index].kind != "local" {
        eprintln!(
            "[session] Profile id={profile_id} 类型为 {}，会话恢复不自动连接，回退普通 shell",
            st.profiles.profiles[index].kind
        );
        return (None, None);
    }
    match build_profile_launch(st, index) {
        Ok((spec, askpass)) => {
            eprintln!(
                "[session] 恢复 local Profile id={profile_id} name={:?}，外观/启动按 Profile",
                spec.name
            );
            (Some(spec), askpass)
        },
        Err(err) => {
            eprintln!(
                "[session] Profile id={profile_id} 启动信息不可用（{}），回退普通 shell",
                err.message()
            );
            (None, None)
        },
    }
}

/// 按快照恢复一个 Tab（新 Pane、新 shell）；失败时清理半成品。
/// 返回插入后的 workspace 下标（调用方用于设置 active / 重开最近关闭）。
fn restore_tab_snapshot(st: &mut AppState, tab_snap: &session::TabSnapshot) -> Option<usize> {
    let cwds = tab_snap.tree.leaf_cwds();
    let profiles = tab_snap.tree.leaf_profiles();
    let ids: Vec<PaneId> = (0..cwds.len()).map(|_| st.shared.alloc_pane_id()).collect();
    let Some(root) = tab_snap.tree.assign(&ids) else {
        eprintln!("[session] 布局与 pane 数量不符，跳过该标签");
        return None;
    };
    let mut created = 0;
    for (i, cwd) in cwds.iter().enumerate() {
        let (profile, askpass) = restore_pane_launch(st, profiles.get(i).copied().flatten());
        match create_pane(
            st,
            ids[i],
            st.content,
            cwd.as_ref().map(PathBuf::from),
            profile.as_ref(),
            askpass,
        ) {
            Ok(rt) => {
                st.panes.insert(ids[i], rt);
                created += 1;
            }
            Err(e) => {
                eprintln!("[session] pane 恢复失败: {e}");
                break;
            }
        }
    }
    if created != ids.len() {
        for id in &ids {
            st.panes.remove(id);
        }
        return None;
    }
    let focused = ids.get(tab_snap.focused).copied().unwrap_or(ids[0]);
    let tab_title = tab_snap.title.clone();
    let index = st.ws.insert_tab(Tab {
        title: tab_title.clone(),
        root: Some(root),
        focused,
        grid: HashMap::new(),
        pinned: tab_snap.pinned,
    });
    eprintln!(
        "[session] restore tab {tab_title:?} pinned={} index={index}",
        tab_snap.pinned
    );
    dispatch_plugin_event(
        st,
        plugin_v3::PluginEvent::TabOpen,
        plugin_v3::tab_open_payload(&tab_title, "local"),
    );
    Some(index)
}

/// 重开最近关闭的 Tab（Ctrl+Shift+T / 菜单）。
fn reopen_closed_tab(st: &mut AppState) {
    let Some(snap) = st.closed_tabs.pop() else {
        eprintln!("[ws] 没有可重开的 Tab");
        return;
    };
    let Some(index) = restore_tab_snapshot(st, &snap) else {
        eprintln!("[ws] 重开失败: {:?}", snap.title);
        return;
    };
    st.ws.active = index;
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
    eprintln!(
        "[ws] reopen closed tab {:?}（剩余 {}）",
        snap.title,
        st.closed_tabs.len()
    );
}

/// 只收集布局 tabs（多窗口 v2 快照用；窗口几何由 WindowRuntime 补）。
fn capture_tabs_snapshot(st: &AppState) -> Vec<session::TabSnapshot> {
    let mut tabs = Vec::with_capacity(st.ws.tabs.len());
    for tab in &st.ws.tabs {
        let Some(root) = tab.root.as_ref() else { continue };
        let ids = root.panes();
        let mut cwds = HashMap::with_capacity(ids.len());
        let mut profiles = HashMap::with_capacity(ids.len());
        for id in &ids {
            // cwd 从子进程的 /proc/<pid>/cwd 取，用户 cd 到哪就恢复到哪
            let cwd = st
                .panes
                .get(id)
                .and_then(|rt| rt.handle.pid())
                .and_then(session::cwd_of);
            cwds.insert(*id, cwd);
            profiles.insert(*id, st.panes.get(id).and_then(|rt| rt.profile_id));
        }
        let focused = ids.iter().position(|id| *id == tab.focused).unwrap_or(0);
        tabs.push(session::TabSnapshot {
            title: tab.title.clone(),
            focused,
            pinned: tab.pinned,
            tree: session::Tree::from_node_with_profiles(root, &cwds, &profiles),
        });
    }
    tabs
}

/// 保存多窗口快照 → 拉起新进程（带 `--restore`）→ 退出当前进程。
fn restart_session(st: &mut AppState) {
    // 当前窗口的 AppState 正被 `with_app` 可变借用，必须显式传进去，否则
    // capture 会跳过它、只保存其它窗口。
    let snapshot = runtime::capture_restorable_snapshot_v2_with_window(st.window_id, st);
    if snapshot.windows.is_empty() {
        eprintln!("[session] 没有可保存的布局，取消重启");
        return;
    }
    let (cols, rows) = cell_of(st.content, st.cell);
    match session::save_v2(&snapshot) {
        Ok(path) => {
            let tabs: usize = snapshot.windows.iter().map(|w| w.tabs.len()).sum();
            let panes: usize = snapshot
                .windows
                .iter()
                .flat_map(|w| w.tabs.iter())
                .map(|t| t.tree.leaf_count())
                .sum();
            eprintln!(
                "[session] 重启前已保存 {}（{} 个窗口 / {} 个标签 / {} 个 pane）",
                path.display(),
                snapshot.windows.len(),
                tabs,
                panes
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
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--restore")
        .arg("--cols")
        .arg(cols.to_string())
        .arg("--rows")
        .arg(rows.to_string())
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
            st.shared.session.with_mut(|session| session.restarting = true);
            eprintln!("[session] 已拉起新实例 pid={}，退出当前进程", child.id());
            let _ = slint::quit_event_loop();
        },
        Err(e) => eprintln!("[session] 重启失败（保留当前窗口）: {e}"),
    }
}

/// 按一个窗口快照重建 tabs/panes；成功返回 true。
///
/// 逐 Tab 恢复：单个 Tab 的 pane 创建失败只跳过该 Tab，不抛弃整个窗口；
/// 一个 Tab 都没成功时返回 false，调用方回退一个新 Tab（不整机失败）。
fn restore_window_snapshot(st: &mut AppState, window: &session::WindowSnapshot) -> bool {
    if window.tabs.is_empty() {
        return false;
    }

    // 丢掉启动时建的默认 pane（Drop 会关掉它的 PTY）
    st.ws.tabs.clear();
    st.panes.clear();
    st.pane_rows.clear();

    let wanted_active = window.active_tab.min(window.tabs.len() - 1);
    let mut active_restored = None;
    let mut restored = 0usize;
    for (original, tab_snap) in window.tabs.iter().enumerate() {
        if let Some(index) = restore_tab_snapshot(st, tab_snap) {
            if original == wanted_active {
                active_restored = Some(index);
            }
            restored += 1;
        } else {
            eprintln!(
                "[session] 窗口 {} 的标签 {:?} 恢复失败，跳过",
                window.id, tab_snap.title
            );
        }
    }

    if restored == 0 {
        return false;
    }
    st.ws.active = active_restored.unwrap_or_else(|| wanted_active.min(st.ws.tabs.len() - 1));
    // 旧/坏快照可能把 pinned 放在 unpinned 后面；恢复后按组稳定归位。
    st.ws.normalize_pinned();
    st.next_title = st.ws.tabs.len();
    refresh_views(st);
    rebuild_tabs(st);
    redraw_visible(st);
    eprintln!(
        "[session] 窗口 {} 恢复完成：{restored}/{} 个标签 active={}",
        window.id,
        window.tabs.len(),
        st.ws.active
    );
    true
}

fn split_pane_inner(st: &mut AppState, id: PaneId, dir: Dir) {
    eprintln!("[ws] split pane={id} dir={dir:?}");
    let Some(tab_index) = st.ws.tab_of(id) else {
        return;
    };
    let Some(rect) = st.panes.get(&id).map(|r| r.rect) else {
        return;
    };
    // 新 pane 大致占原 pane 的一半；refresh_views 会按最终矩形重算网格。
    let new_rect = match dir {
        Dir::Row => Rect::new(
            rect.x + rect.w / 2.0,
            rect.y,
            (rect.w / 2.0).max(1.0),
            rect.h,
        ),
        Dir::Col => Rect::new(
            rect.x,
            rect.y + rect.h / 2.0,
            rect.w,
            (rect.h / 2.0).max(1.0),
        ),
    };
    let new_id = st.shared.alloc_pane_id();
    match create_pane(st, new_id, new_rect, None, None, None) {
        Ok(rt) => {
            st.panes.insert(new_id, rt);
            st.ws.tabs[tab_index].split(id, dir, new_id);
            st.ws.tabs[tab_index].focused = new_id;
            refresh_views(st);
            redraw_visible(st);
        }
        Err(e) => eprintln!("分屏失败: {e}"),
    }
}

fn focus_pane_inner(st: &mut AppState, id: PaneId) {
    let Some(idx) = st.ws.tab_of(id) else { return };
    if st.ws.tabs[idx].focused == id {
        return;
    }
    st.ws.tabs[idx].focused = id;
    runtime::note_pane_focus(st.window_id, id);
    // 显式切 pane：即使新旧 pane 光标屏幕坐标恰好相同也要让输入法重收 spot。
    st.ime_spot_dirty = true;
    refresh_views(st);
    let win = st.window_focused;
    sync_terminal_focus(st, win);
    update_ime_spot(st);
}

/// 把窗口/ pane 焦点同步给启用了 DECSET 1004 的终端。
/// 每个 pane 记录上次发送的状态；模式刚启用时也会补发当前 FocusIn。
fn sync_terminal_focus(st: &mut AppState, window_focused: bool) {
    st.window_focused = window_focused;
    let active_focused = st.ws.active_tab().map(|t| t.focused);
    for (id, rt) in st.panes.iter_mut() {
        if !rt.handle.wants_focus() {
            rt.focus_sent = None;
            continue;
        }
        let want = window_focused && Some(*id) == active_focused;
        if rt.focus_sent != Some(want) {
            rt.handle.focus_event(want);
            rt.focus_sent = Some(want);
            if std::env::var_os("TERVO_FOCUS_LOG").is_some() {
                eprintln!("[focus] pane={id} focused={want} window={window_focused}");
            }
        }
    }
}

/// 关闭前是否需要确认：开着开关且不止一个会话时弹窗，避免误关杀掉一堆 shell。
fn needs_close_confirm(tabs: usize, panes: usize, enabled: bool) -> bool {
    enabled && (tabs > 1 || panes > 1)
}

/// 立即关闭一个窗口。必须从事件循环回调/延迟任务调用：如果调用方正持有该
/// 窗口的 AppState 可变借用，drop runtime 会释放仍被借用的 RefCell。
fn close_window_now(id: WindowId, reason: &str) {
    if runtime::window_count() <= 1 {
        let _ = slint::quit_event_loop();
        return;
    }
    // remove_window 会清理插件的 per-window surface/status/ask（窗口 id 不复用）。
    let Some(window_runtime) = runtime::remove_window(id) else {
        return;
    };
    gpu_present::teardown_for(id);
    gpu_native::teardown_for(id);
    let _ = window_runtime.ui.hide();
    drop(window_runtime);
    // 窗口布局变化立即落盘。writer/timer 在 SharedServices 里：primary 关闭后
    // 剩余窗口仍会继续 autosave，这里只是让磁盘尽快反映新的窗口集合。
    let _ = runtime::save_session_snapshot(reason, false);
    eprintln!(
        "[ui] {id} closed（{reason}，remaining={}）",
        runtime::window_count()
    );
}

/// 从 UI 回调里安全地关闭当前窗口：延迟到回调返回、借用释放后执行。
fn defer_close_window(id: WindowId) {
    let _ = slint::invoke_from_event_loop(move || close_window_now(id, "close-request"));
}

fn request_close(st: &mut AppState) {
    let tabs = st.ws.tabs.len();
    let panes: usize = st.ws.tabs.iter().map(|t| t.panes().len()).sum();
    let window_id = st.window_id;
    eprintln!(
        "[ui] close requested: window={window_id} tabs={tabs} panes={panes} confirm={}",
        st.cfg.confirm_close
    );
    if needs_close_confirm(tabs, panes, st.cfg.confirm_close) {
        let text = if runtime::window_count() > 1 {
            format!("将结束该窗口的 {tabs} 个标签、{panes} 个终端会话（其它窗口保留）。")
        } else {
            format!("将结束 {tabs} 个标签、{panes} 个终端会话，子进程会被终止。")
        };
        eprintln!("[ui] close confirm: window={window_id} tabs={tabs} panes={panes}");
        st.pending_close_window = Some(window_id);
        st.ui.set_confirm_text(text.as_str().into());
        st.ui.set_confirm_open(true);
        return;
    }
    close_current_window(st);
}

/// 关闭一个窗口：多窗口时只移除本窗口（PTY/模型/timer/GPU 引用随
/// `WindowRuntime` 释放），其它窗口继续运行；最后一个窗口才退出事件循环。
///
/// registry 移除立即完成（`window_count`/焦点/MCP 路由马上正确），但
/// `WindowRuntime` 的 drop 延后到回调返回之后：本函数由 Slint 控件回调进入，
/// 立即 drop 最后一个 `MainWindow` 强引用会在回调栈上销毁正在执行的对象。
fn close_current_window(st: &mut AppState) {
    let window_id = st.window_id;
    if runtime::window_count() <= 1 {
        let _ = slint::quit_event_loop();
        return;
    }
    eprintln!("[ui] {window_id} close（保留其它窗口）");
    let runtime = runtime::remove_window(window_id);
    gpu_present::teardown_for(window_id);
    gpu_native::teardown_for(window_id);
    let _ = st.ui.hide();
    if let Some(runtime) = runtime {
        // Timer 回调只要求 'static（不需要 Send），且只在 UI 线程执行；用它把
        // Rc<WindowRuntime> 的 drop 排到当前 Slint 回调返回之后。
        slint::Timer::single_shot(Duration::ZERO, move || drop(runtime));
    }
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
            let scale = if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            };
            (
                physical.width as f32 / scale,
                physical.height as f32 / scale,
            )
        }
    };
    shaper.request(xconn::CornerRequest {
        logical_width,
        logical_height,
        scale_factor: scale,
        state,
    });
}

/// 状态机 worker 通过 UI 事件循环回调到这里：只读 snapshot，同步图标/圆角。
///
/// 不查询几何、不按标题找窗口；desired 是唯一 target。回调排队期间用户可能
/// 又点了一次，所以这里重新读当前 snapshot，而不是用回调参数里的旧值。
fn sync_maximize_from_state_machine(st: &mut AppState) {
    let Some(snapshot) = st.maximize.as_ref().map(|runtime| runtime.snapshot()) else {
        return;
    };
    let previous = st.window_maximized;
    st.window_maximized = snapshot.desired;
    st.ui.set_window_maximized(snapshot.desired);
    if previous != snapshot.desired {
        request_window_corners(
            st,
            None,
            Some(xconn::CornerState {
                maximized: snapshot.desired,
                fullscreen: st.ui.window().is_fullscreen(),
            }),
        );
        eprintln!(
            "[maximize] ui-sync desired={} observed={} pending={:?} error={:?}",
            snapshot.desired, snapshot.observed, snapshot.pending, snapshot.last_error
        );
    }
}

/// 启动状态机。
///
/// `ui.show()` 在 winit-skia 下是异步建原生窗口：紧接着读 raw handle 可能还是
/// None（这就是旧启动逻辑在 X11 上偶尔拿不到 xid 的原因）。每 50ms 重试，直到
/// winit 给出 Xlib/Xcb handle；最多 2s，之后明确回退 winit。重试只用于窗口
/// 句柄就绪，不参与最大化状态判断。
fn start_maximize_when_window_ready(ui: &MainWindow, window_id: WindowId, attempt: u32) {
    let xid = ui.window().with_winit_window(winit_window_x11_id).flatten();
    if let Some(id) = xid {
        let on_event = move |_snapshot: tervo_maximize::MaximizeSnapshot| {
            let _ = slint::invoke_from_event_loop(move || {
                runtime::with_window(window_id, sync_maximize_from_state_machine);
            });
        };
        match tervo_maximize::MaximizeRuntime::start(id, on_event) {
            Ok(runtime) => {
                let snapshot = runtime.snapshot();
                runtime::with_window(window_id, |st| {
                    st.window_maximized = snapshot.desired;
                    st.ui.set_window_maximized(snapshot.desired);
                    request_window_corners(
                        st,
                        None,
                        Some(xconn::CornerState {
                            maximized: snapshot.desired,
                            fullscreen: st.ui.window().is_fullscreen(),
                        }),
                    );
                    st.maximize = Some(runtime);
                });
                eprintln!(
                    "[maximize] runtime ready win=0x{id:x} desired={} observed={} pending={:?}",
                    snapshot.desired, snapshot.observed, snapshot.pending
                );
            },
            Err(err) => {
                eprintln!(
                    "[maximize] runtime start failed win=0x{id:x} err={err}; fallback=winit"
                );
            },
        }
        return;
    }

    if attempt >= 40 {
        eprintln!("[maximize] 2s 内未拿到 winit X11 handle；fallback=winit");
        return;
    }
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(ui) = weak.upgrade() {
            start_maximize_when_window_ready(&ui, window_id, attempt + 1);
        }
    });
}

/// 让恢复中的窗口进入快照记录的最大化状态。
///
/// `set_maximized` 在 X11 上是 WM 异步处理的；400ms 后如果 maximize 状态机
/// 已就绪且仍不是 desired，就通过状态机再请求一次，避免恢复后掉回普通窗口。
fn restore_window_geometry(
    ui: &MainWindow,
    window_id: WindowId,
    window: &session::WindowSnapshot,
) {
    let geometry = window.geometry;
    if geometry.is_known() {
        ui.window().set_size(slint::PhysicalSize::new(geometry.w, geometry.h));
        ui.window()
            .set_position(slint::PhysicalPosition::new(geometry.x, geometry.y));
    }
    if window.maximized {
        ui.window().set_maximized(true);
        let _ = runtime::with_window(window_id, |st| {
            st.window_maximized = true;
            st.ui.set_window_maximized(true);
        });
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(400), move || {
            let Some(ui) = weak.upgrade() else { return };
            let handled = runtime::with_window(window_id, |st| {
                let Some(maximize) = st.maximize.as_ref() else {
                    return false;
                };
                if !st.window_maximized {
                    eprintln!("[session] {window_id} 恢复最大化（走 X11 状态机）");
                    let target = maximize.click();
                    st.window_maximized = target;
                    st.ui.set_window_maximized(target);
                }
                true
            });
            if handled != Some(true) {
                // 非 X11 / 状态机启动失败：保留 winit 原生路径。
                ui.window().set_maximized(true);
            }
        });
    }
    eprintln!(
        "[session] {window_id} 恢复 geometry=({},{} {}x{}) maximized={}",
        geometry.x, geometry.y, geometry.w, geometry.h, window.maximized
    );
}

/// 切换指定窗口最大化。primary/second 共用同一套 X11 状态机逻辑，不再依赖
/// “当前窗口”猜测。
fn toggle_maximize_for(ui: &MainWindow, window_id: WindowId) {
    // X11：MaximizeController 是唯一权威。click 只翻转 desired，
    // 发送/PropertyNotify 确认/外部同步都在 worker，UI 只显示 target。
    let from_controller = runtime::with_window(window_id, |st| {
        let target = st.maximize.as_ref()?.click();
        st.window_maximized = target;
        Some(target)
    })
    .flatten();
    if let Some(target) = from_controller {
        ui.set_window_maximized(target);
        let _ = runtime::with_window(window_id, |st| {
            request_window_corners(
                st,
                None,
                Some(xconn::CornerState {
                    maximized: target,
                    fullscreen: st.ui.window().is_fullscreen(),
                }),
            );
        });
        eprintln!("[maximize] {window_id} ui click target={target}");
        return;
    }
    // 非 X11 / 状态机启动失败：保留 winit 原生路径；不按标题找窗口。
    let target = runtime::with_window(window_id, |st| {
        st.window_maximized = !st.window_maximized;
        st.window_maximized
    })
    .unwrap_or_else(|| !ui.get_window_maximized());
    ui.set_window_maximized(target);
    ui.window().set_maximized(target);
    eprintln!("[maximize] {window_id} fallback winit target={target}");
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
        Some(xconn::CornerState {
            maximized: win.is_maximized(),
            fullscreen: next,
        }),
    );
    eprintln!("[ui] fullscreen={next}");
}

fn set_active_tab_inner(st: &mut AppState, index: usize) {
    if index >= st.ws.tabs.len() || st.ws.active == index {
        return;
    }
    st.ws.active = index;
    st.ime_override = None;
    st.ime_spot_dirty = true;
    // 获得焦点的 Tab：清除未读通知点 / 完成标记（Tabby 的 unread 语义）。
    let ids = st.ws.tabs[index].panes();
    let mut cleared = 0usize;
    for id in ids {
        let Some(rt) = st.panes.get_mut(&id) else {
            continue;
        };
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
    update_ime_spot(st);
    let win = st.window_focused;
    sync_terminal_focus(st, win);
}

/// 拖拽落点：按平均槽位把指针 x（Flickable 内容坐标）换算成目标下标。
///
/// Tab 从内容 x=0 开始排；`strip_w` 是 tab-strip 的内容宽度（含 padding）。
/// 视觉上插入线按真实 layout 绘制，这里只负责“落在第几个槽”。
fn drag_target(drag_x: f32, strip_w: f32, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let slot = (strip_w / n as f32).max(1.0);
    let idx = (drag_x / slot).floor().max(0.0) as usize;
    idx.min(n - 1)
}

/// 组内落点：pinned 只能落在 pinned 区，unpinned 只能落在 unpinned 区。
/// 返回最终 workspace 下标；落在另一组、原地或越界返回 None（不移动/不亮插入线）。
fn drag_target_in_group(
    drag_x: f32,
    strip_w: f32,
    n: usize,
    pinned_count: usize,
    from: usize,
) -> Option<usize> {
    if n == 0 || from >= n {
        return None;
    }
    let pinned_count = pinned_count.min(n);
    let target = drag_target(drag_x, strip_w, n);
    let (lo, hi) = if from < pinned_count {
        (0, pinned_count)
    } else {
        (pinned_count, n)
    };
    (target >= lo && target < hi && target != from).then_some(target)
}

/// 最终下标 → 视觉插入边界（0..=n）：插在目标 Tab 前/后。
fn drag_gap_index(from: usize, target: usize) -> usize {
    if target < from {
        target
    } else {
        target + 1
    }
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

// ------------------------------------------------------------------- resize 收尾

/// WM watchdog：最近不再有 configure、且按下后已超过宽限，才算没接管。
///
/// 之前用固定的 150ms × 按下时刻判断；GPU/Skia 首帧或事件队列排队时 configure
/// 可能晚到，于是被误判成“WM 未接管”而回退自绘——用户日志里的
/// `WM live resize 未接管指针，回退自绘 resize` 就是这么来的。
fn wm_resize_timed_out(
    saw_configure: bool,
    since_last_configure: Option<Duration>,
    since_start: Duration,
) -> bool {
    // 收到过 configure 后给更宽的窗口；只要最近还在动就绝不回退。
    let timeout = if saw_configure {
        Duration::from_millis(800)
    } else {
        Duration::from_millis(400)
    };
    if since_start < timeout {
        return false;
    }
    !since_last_configure.is_some_and(|d| d < timeout)
}

/// 缩小/首次 configure 必须立即重绘；放大可以合并到 60fps。拆成纯函数便于回归测试。
///
/// 布局本身已经在每个 configure 立即更新，因此即使 16ms 后重绘，按钮也已在
/// 新布局的正确位置；缩小则不能等，否则旧帧按钮会露在窗口外一帧。
fn should_redraw_resize_immediately(old_w: f32, new_w: f32, was_resizing: bool) -> bool {
    new_w + 0.5 < old_w || !was_resizing
}

fn schedule_resize_redraw(st: &mut AppState, immediate: bool) {
    if immediate {
        st.ui.window().request_redraw();
        return;
    }
    if st.resize_redraw_scheduled {
        return;
    }
    st.resize_redraw_scheduled = true;
    let window_id = st.window_id;
    st.resize_redraw.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(16),
        move || {
            runtime::with_window(window_id, |st| {
                st.resize_redraw_scheduled = false;
                st.ui.window().request_redraw();
            });
        },
    );
}

/// 拖动停止 200ms 后：恢复背景图、做最终重绘、打印汇总。
fn schedule_resize_finalize(st: &mut AppState) {
    let window_id = st.window_id;
    st.resize_finalize.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(200),
        move || {
            runtime::with_window(window_id, |st| {
                // 定时器是 single-shot；如果 200ms 内又来了 configure，重新计时。
                if st
                    .last_resize_event
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(180))
                {
                    schedule_resize_finalize(st);
                    return;
                }
                let was_active = st.resize_in_progress;
                st.resize_in_progress = false;
                // 无论 apply/收尾顺序如何，都必须恢复背景，避免一直隐藏。
                if st.ui.get_background_suspended() {
                    st.ui.set_background_suspended(false);
                    st.ui.window().request_redraw();
                }
                if (was_active || st.resize_applied > 0)
                    && std::env::var_os("TERVO_RESIZE_PROFILE").is_some()
                {
                    eprintln!(
                        "[ui] resize done applies={} max_apply={:.1}ms content={:.0}x{:.0}",
                        st.resize_applied,
                        st.resize_max_apply_us / 1000.0,
                        st.content.w,
                        st.content.h
                    );
                }
                st.resize_applied = 0;
                st.resize_max_apply_us = 0.0;
                st.resize_events = 0;
            });
        },
    );
}

// ------------------------------------------------------------------- 布局

/// 由滚动条策略与当前滚动位置算可见性和 thumb 几何（纯函数，便于单测）。
///
/// - `always`：没有历史时 thumb 占满轨道（表示“有滚动条但内容全在视口内”）；
/// - `auto`：只有真的能滚动时才显示（既有行为）；
/// - `never`：始终隐藏。
fn scrollbar_geometry(
    policy: ScrollbarPolicy,
    pos: ScrollPosition,
    min_thumb: f32,
    track_h: f32,
) -> (bool, f32, f32) {
    let visible = match policy {
        ScrollbarPolicy::Always => true,
        ScrollbarPolicy::Auto => pos.scrollable(),
        ScrollbarPolicy::Never => false,
    };
    if !visible {
        return (false, 0.0, 0.0);
    }
    match pos.thumb(min_thumb) {
        Some((top, height)) => (true, top * track_h, height * track_h),
        None => (true, 0.0, track_h),
    }
}

/// PaneRuntime → Slint 视图。滚动条几何在这里一次算好，Slint 只负责画，
/// 避免在 UI 线程上做 clamp/浮点分支。
fn pane_view(
    id: PaneId,
    rt: &PaneRuntime,
    focused: bool,
    cursor_blink_on: bool,
    global_blink: bool,
    policy: ScrollbarPolicy,
) -> PaneView {
    let pos = rt.handle.scroll_position();
    let track_h = rt.renderer.height.max(1) as f32;
    let min_thumb = (SCROLLBAR_MIN_THUMB_PX / track_h).clamp(0.0, 1.0);
    let (scrollbar_visible, thumb_y, thumb_h) = scrollbar_geometry(policy, pos, min_thumb, track_h);
    // Profile 可显式关闭/开启闪烁；`off` 时 overlay 恒亮，不跟全局相位。
    let blink = rt.appearance.cursor_blinks(global_blink);
    PaneView {
        id: id as i32,
        image: rt.image.clone(),
        x: rt.rect.x,
        y: rt.rect.y,
        w: rt.rect.w,
        h: rt.rect.h,
        focused,
        iw: rt.renderer.width as f32,
        ih: rt.renderer.height as f32,
        cursor_image: rt.cursor_image.clone(),
        cursor_x: rt.cursor_rect.0,
        cursor_y: rt.cursor_rect.1,
        cursor_w: rt.cursor_rect.2,
        cursor_h: rt.cursor_rect.3,
        cursor_visible: rt.cursor_visible && (!blink || cursor_blink_on),
        mouse_reporting: rt.handle.wants_mouse(),
        scrollbar_visible,
        scrollbar_thumb_y: thumb_y,
        scrollbar_thumb_h: thumb_h,
        scrollbar_track_h: track_h,
        bell_flash: rt.bell_until.is_some(),
        read_only: rt.handle.read_only(),
        link_hover: rt.link_hover,
        images: ModelRc::from(rt.inline_model.clone()),
        line_numbers: line_number_model(rt.renderer.rows, rt.renderer.cell_h),
        line_h: rt.renderer.cell_h as f32,
    }
}

/// 调试行号：仅 TERVO_LINE_NUMBERS=1 时生成 1..N 的字符串模型。
fn line_number_model(rows: usize, _cell_h: usize) -> ModelRc<slint::SharedString> {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| {
        std::env::var("TERVO_LINE_NUMBERS").map(|v| v == "1").unwrap_or(false)
    });
    if !enabled {
        return ModelRc::from(Rc::new(VecModel::from(Vec::<slint::SharedString>::new())));
    }
    let items: Vec<slint::SharedString> =
        (1..=rows).map(|n| n.to_string().into()).collect();
    ModelRc::from(Rc::new(VecModel::from(items)))
}

/// 刷新一个 pane 在模型里的视图（滚动条/铃/尺寸），不拉帧不重绘。
fn refresh_pane_view(st: &mut AppState, id: PaneId) {
    let Some(&row) = st.pane_rows.get(&id) else {
        return;
    };
    let focused = st.ws.active_tab().map(|t| t.focused) == Some(id);
    let policy = st.cfg.scrollbar_policy();
    let global_blink = st.cfg.cursor_blinking();
    let Some(rt) = st.panes.get(&id) else { return };
    st.pane_model.set_row_data(
        row,
        pane_view(id, rt, focused, st.cursor_blink_on, global_blink, policy),
    );
}

/// 配置变化后立即刷新所有 pane 的滚动条/铃视图（不重排布局）。
fn refresh_all_pane_views(st: &mut AppState) {
    let ids: Vec<PaneId> = st.pane_rows.keys().copied().collect();
    for id in ids {
        refresh_pane_view(st, id);
    }
}

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
        let Some(tab) = st.ws.active_tab() else {
            return;
        };
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
            let (cols, rows) = cell_of(*rect, rt.cell);
            rt.rect = *rect;
            if resize_terminals && rt.grid != (cols, rows) {
                rt.renderer.resize_grid(cols, rows);
                rt.handle.resize(cols as u16, rows as u16);
                rt.grid = (cols, rows);
            }
        }
    }

    let policy = st.cfg.scrollbar_policy();
    let global_blink = st.cfg.cursor_blinking();
    let make_view = |id: &PaneId, rect: &Rect| -> Option<PaneView> {
        st.panes.get(id).map(|rt| {
            let mut view = pane_view(
                *id,
                rt,
                *id == focused,
                st.cursor_blink_on,
                global_blink,
                policy,
            );
            view.x = rect.x;
            view.y = rect.y;
            view.w = rect.w;
            view.h = rect.h;
            view
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

    // pane 数变化（开/关 Tab、分屏）时按全局预算收缩 scrollback。
    apply_scrollback_budget(st);
    // 分屏/切 pane/Tab 切换/Tab 栏换边最终都会刷新布局：候选点跟着新矩形走。
    update_ime_spot(st);
    // 新建/关闭 Profile pane 后，按“是否还有 pane 需要闪烁”启停计时器。
    ensure_cursor_blink_timer(st);
}

/// 全进程 scrollback 行数预算：22 Tab + 4 pane（25 pane）时每 pane 约 960 行。
///
/// 每个 pane 的 scrollback 行（1–4KiB/行）会随访问过的 Tab 数线性常驻；25 pane
/// 各 2000 行可达 ~160MiB，是长时 soak RSS 增长的主因。这里用**全进程行预算**做
/// 上限：pane 少时保持用户配置，pane 多时按比例收缩（有明确下限 200 行），
/// 避免“Tab 越多内存越高”。不是改配置默认值，也不动 soak 阈值。
const GLOBAL_SCROLLBACK_LINES: usize = 24_000;
const MIN_SCROLLBACK_LINES: usize = 200;

fn scrollback_budget(pane_count: usize, configured: usize) -> usize {
    let per_pane = GLOBAL_SCROLLBACK_LINES / pane_count.max(1);
    per_pane.clamp(MIN_SCROLLBACK_LINES, configured.max(MIN_SCROLLBACK_LINES))
}

/// 把全局行预算下发给所有 pane；只在目标值变化时调用 core（缩容会立即释放旧行）。
fn apply_scrollback_budget(st: &mut AppState) {
    let count = st.panes.len();
    for rt in st.panes.values_mut() {
        let target = scrollback_budget(count, rt.scrollback_limit);
        if target != rt.scrollback_applied {
            rt.handle.set_scrollback(target);
            rt.scrollback_applied = target;
        }
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
    model_rows == order.len()
        && order
            .iter()
            .enumerate()
            .all(|(i, id)| rows.get(id) == Some(&i))
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
                t.panes()
                    .iter()
                    .filter_map(|id| st.panes.get(id))
                    .map(|rt| &rt.activity),
            );
            TabView {
                title: t.title.clone().into(),
                active: i == active,
                progress_state: agg.progress.state as i32,
                progress_ratio: activity::progress_ratio(agg.progress),
                progress_label: activity::progress_label(agg.progress).into(),
                attention: agg.attention,
                completed: agg.completed,
                pinned: t.pinned,
            }
        })
        .collect();
    st.ui.set_pinned_count(st.ws.pinned_count() as i32);
    // Tab 模型重建时任何进行中的落点都不再有效（拖拽中不会触发 rebuild）。
    st.ui.set_drop_index(-1);
    st.ui.set_drop_gap(-1);
    st.ui.set_drop_valid(false);
    st.tab_model.set_vec(tabs);
}

/// 重绘当前可见的 pane（切换 tab / 布局变化后）。
fn redraw_visible(st: &mut AppState) {
    let ids: Vec<PaneId> = {
        let Some(tab) = st.ws.active_tab() else {
            return;
        };
        tab.panes()
    };
    for id in ids {
        redraw_pane_inner(st, id);
    }
    // 切换 Tab / 字号 / 主题时，旧 pane 的帧缓冲已被新 Image 替换并 free，但 glibc
    // 默认把几 MB 的堆块留在 arena 里不还给 OS，多 Tab 场景会显示为 RSS 持续增长。
    // 这里只在“可见 pane 全量重绘”这类低频路径显式归还已释放页；空闲/单行输出
    // 不走该路径，不引入每帧开销。非 glibc 平台无此 API，直接跳过。
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

// ------------------------------------------------------------------- 渲染

/// 重新生成 pane 光标 overlay（从当前 `frame` 读光标/格内容，不拉帧）。
fn refresh_cursor_overlay(rt: &mut PaneRuntime) {
    match rt.renderer.cursor_overlay(&rt.frame) {
        Some(o) => {
            rt.cursor_image = o.image;
            rt.cursor_rect = (o.x, o.y, o.width, o.height);
            rt.cursor_visible = true;
        }
        None => {
            rt.cursor_image = slint::Image::default();
            rt.cursor_rect = (0.0, 0.0, 0.0, 0.0);
            rt.cursor_visible = false;
        }
    }
}

/// 光标外观变化：只换 renderer 字段 + 重新生成小 overlay，不重绘整屏。
///
/// 优先级：Profile 显式 cursor_shape > 全局 `config.cursor_shape`；颜色仍全局。
fn apply_cursor_settings(st: &mut AppState) {
    let global_shape = st.cfg.cursor_shape();
    let (color, text_color) = (st.cfg.cursor_color_rgb(), st.cfg.cursor_text_color_rgb());
    for rt in st.panes.values_mut() {
        let shape = rt
            .appearance
            .cursor_shape
            .as_deref()
            .map(tervo_core::CursorShape::from_name)
            .unwrap_or(global_shape);
        if rt.renderer.set_cursor_style(shape, color, text_color) {
            refresh_cursor_overlay(rt);
        }
    }
    refresh_cursor_views(st);
}

/// 只刷新 pane 视图（光标 overlay 可见性/几何），不拉帧不栅格化。
fn refresh_cursor_views(st: &mut AppState) {
    let ids: Vec<PaneId> = st.panes.keys().copied().collect();
    for id in ids {
        refresh_pane_view(st, id);
    }
}

/// 粗体配置变化：按每个 pane 的主题重建调色板并让 core 重取整帧。
fn apply_bold_settings(st: &mut AppState) {
    let fallback = st.resolved_theme.clone();
    let registry = &st.registry;
    let cfg = &st.cfg;
    let bold = (cfg.bold_is_bright, cfg.bold_color_rgb(), cfg.bold_color_same_as_fg);
    for rt in st.panes.values_mut() {
        let theme = rt
            .appearance
            .theme
            .as_deref()
            .and_then(|sel| theme_for_selection(registry, cfg, Some(sel)))
            .unwrap_or_else(|| fallback.clone());
        rt.handle
            .set_theme(theme.palette().with_bold(bold.0, bold.1, bold.2));
    }
    redraw_visible(st);
}

/// 是否有 pane 需要闪烁（全局开启，或任一 Profile 显式 on）。
fn needs_cursor_blink(st: &AppState) -> bool {
    st.cfg.cursor_blinking()
        || st
            .panes
            .values()
            .any(|rt| rt.appearance.cursor_blink == Some(true))
}

/// 截图/自动化冻结相位：`TERVO_CURSOR_BLINK_PHASE=on|off`。
fn frozen_cursor_blink_phase() -> Option<bool> {
    let phase = std::env::var_os("TERVO_CURSOR_BLINK_PHASE")?;
    match phase.to_string_lossy().to_ascii_lowercase().as_str() {
        "on" | "1" | "true" => Some(true),
        "off" | "0" | "false" => Some(false),
        _ => None,
    }
}

/// 光标闪烁半周期：只切换 overlay 的 `cursor-visible`，终端帧不重绘。
fn tick_cursor_blink(st: &mut AppState) {
        if !needs_cursor_blink(st) {
            st.cursor_blink_timer.stop();
            st.cursor_blink_on = true;
            refresh_cursor_views(st);
            return;
        }
        st.cursor_blink_on = !st.cursor_blink_on;
        refresh_cursor_views(st);
}

/// 按配置/Profile 启停闪烁计时器；没有 pane 需要闪时停掉，空闲零唤醒。
///
/// `TERVO_CURSOR_BLINK_PHASE=on|off` 用于截图/自动化：冻结相位、不启动计时器。
fn configure_cursor_blink(st: &mut AppState) {
    if let Some(on) = frozen_cursor_blink_phase() {
        st.cursor_blink_timer.stop();
        st.cursor_blink_on = on;
        refresh_cursor_views(st);
        return;
    }
    if needs_cursor_blink(st) {
        st.cursor_blink_on = true;
        if st.cursor_blink_timer.running() {
            st.cursor_blink_timer.restart();
        } else {
            let window_id = st.window_id;
            st.cursor_blink_timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(config::CURSOR_BLINK_MS),
                move || {
                    let _ = runtime::with_window(window_id, tick_cursor_blink);
                },
            );
        }
    } else {
        st.cursor_blink_timer.stop();
        st.cursor_blink_on = true;
    }
    refresh_cursor_views(st);
}

/// 新建/关闭 Profile pane 后同步计时器：只在“需要闪且未运行”时启动，
/// 避免每次布局刷新都重置相位。
fn ensure_cursor_blink_timer(st: &mut AppState) {
    if frozen_cursor_blink_phase().is_some() {
        return;
    }
    let need = needs_cursor_blink(st);
    if need && !st.cursor_blink_timer.running() {
        st.cursor_blink_on = true;
        let window_id = st.window_id;
        st.cursor_blink_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(config::CURSOR_BLINK_MS),
            move || {
                let _ = runtime::with_window(window_id, tick_cursor_blink);
            },
        );
    } else if !need && st.cursor_blink_timer.running() {
        st.cursor_blink_timer.stop();
        st.cursor_blink_on = true;
    }
}

fn redraw_pane(id: PaneId) {
    if PIPE_ON.load(Ordering::Relaxed) {
        if let Some(t) = LAST_KEY.lock().unwrap().take() {
            PIPE_SAMPLES.lock().unwrap().push(t.elapsed());
        }
    }
    // PTY 后台线程只带 pane id。PaneId 全进程唯一，必须按它路由到所属窗口；
    // 继续用 with_app 会在 B 聚焦时把 A 的输出画到 B（A 永不更新）。
    match runtime::window_for_pane(id) {
        Some(window) => {
            let _ = runtime::with_window(window, |st| redraw_pane_inner(st, id));
        },
        None => {
            with_app(|st| redraw_pane_inner(st, id));
        },
    }
}

/// IME 候选点去重键。
///
/// 逻辑坐标以毫点保存（`set_ime_cursor_area` 收 LogicalPosition），同时把
/// 物理像素与 DPR 入键：窗口在不同缩放屏之间移动、或只改 DPR 不改逻辑坐标时
/// 也必须重新下发，不能被当成“坐标没变”。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImeSpotKey {
    pane: PaneId,
    logical_x_milli: i32,
    logical_y_milli: i32,
    logical_w_milli: i32,
    logical_h_milli: i32,
    physical_x: i32,
    physical_y: i32,
    physical_w: i32,
    physical_h: i32,
    scale_milli: u32,
    /// 候选点来自 mouse-reporting 点击锚点而非硬件光标；来源变化也要重发。
    anchored: bool,
}

/// 纯函数：由 pane 矩形、字符尺寸、光标格算出 IME 候选点。
///
/// `rect`/`cell_w`/`cell_h` 都是 Slint 逻辑像素；`tabbar_offset` 是内容区在
/// 窗口内的纵向偏移。物理值只用于去重与日志，真正下发给 winit 的仍是逻辑值。
fn ime_spot_key(
    pane: PaneId,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cell: (usize, usize),
    tabbar_offset: f32,
    scale: f64,
    anchored: bool,
) -> ImeSpotKey {
    let cw = cell_w.max(1.0);
    let ch = cell_h.max(1.0);
    let x = rect.x + cell.1 as f32 * cw;
    let y = tabbar_offset + rect.y + cell.0 as f32 * ch;
    let dpr = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let phys = |v: f32| (v as f64 * dpr).round() as i32;
    ImeSpotKey {
        pane,
        logical_x_milli: (x * 1000.0).round() as i32,
        logical_y_milli: (y * 1000.0).round() as i32,
        logical_w_milli: (cw * 1000.0).round() as i32,
        logical_h_milli: (ch * 1000.0).round() as i32,
        physical_x: phys(x),
        physical_y: phys(y),
        physical_w: phys(cw),
        physical_h: phys(ch),
        scale_milli: (dpr * 1000.0).round() as u32,
        anchored,
    }
}

/// 左键在 mouse-reporting TUI 里才记录 IME 锚点；Shift 是本地选择逃生门。
fn ime_click_anchor(button: i32, wants_mouse: bool, shift: bool) -> bool {
    button == 0 && wants_mouse && !shift
}

/// 把聚焦 pane 的终端光标同步给输入法候选窗。
///
/// Slint 只在 TextInput 文本变化时发 IME Update，移动隐藏 TextInput 不会更新
/// preedit spot；这里直接调 winit `set_ime_cursor_area`，使用**窗口坐标**
/// （内容区 + tabbar 偏移）。逻辑像素交给 winit，由它按 DPR 转物理像素；
/// 去重键同时记录物理坐标与 DPR，缩放变化不会被误判成“没动”。
fn update_ime_spot(st: &mut AppState) {
    use slint::winit_030::winit::dpi::{LogicalPosition, LogicalSize};

    let Some(tab) = st.ws.active_tab() else {
        return;
    };
    let focused = tab.focused;
    let Some(rt) = st.panes.get(&focused) else {
        return;
    };
    if !rt.handle.wants_mouse() {
        // TUI 已关闭鼠标上报：点击锚点不应再压过真实硬件光标。
        st.ime_override = None;
    }
    let mouse_anchor = st
        .ime_override
        .filter(|(pid, _)| *pid == focused)
        .map(|(_, cell)| cell);
    let (line, col) = mouse_anchor
        .map(|(l, c)| (l as usize, c as usize))
        .unwrap_or_else(|| {
            let (l, c) = rt.handle.cursor_pos();
            (l as usize, c as usize)
        });
    let content_y = if st.cfg.tabbar_position == "bottom" {
        0.0
    } else {
        TABBAR_H
    };
    let scale = st.ui.window().scale_factor() as f64;
    let key = ime_spot_key(
        focused,
        rt.rect,
        rt.renderer.cell_w as f32,
        rt.renderer.cell_h as f32,
        (line, col),
        content_y,
        scale,
        mouse_anchor.is_some(),
    );
    // 普通 50ms 轮询按完整键去重；pane/Tab/resize/focus 变化由 dirty 强制重发。
    if !st.ime_spot_dirty && st.ime_spot == Some(key) {
        return;
    }
    st.ime_spot = Some(key);

    let x = key.logical_x_milli as f64 / 1000.0;
    let y = key.logical_y_milli as f64 / 1000.0;
    let w = key.logical_w_milli as f64 / 1000.0;
    let h = key.logical_h_milli as f64 / 1000.0;
    // 隐藏 TextInput 继续跟随**内容坐标**（tabbar 偏移由 Slint 布局负责）；
    // 真正的候选窗位置由 winit 直接设置。
    st.ui.set_ime_x(x as f32);
    st.ui.set_ime_y((y - content_y as f64) as f32);
    let pos = LogicalPosition::new(x, y);
    let size = LogicalSize::new(w, h);
    let sent = st
        .ui
        .window()
        .with_winit_window(|window| window.set_ime_cursor_area(pos, size))
        .is_some();
    if sent {
        // 只有真的交给 winit 才清 dirty；窗口还没建好时下次轮询会重试。
        st.ime_spot_dirty = false;
    }
    if std::env::var_os("TERVO_IME_LOG").is_some() {
        eprintln!(
            "[ime] spot pane={focused} cell={line},{col} win={x:.1},{y:.1} \
             phys={},{} scale={:.3} anchor={}",
            key.physical_x, key.physical_y, scale, key.anchored
        );
    }
}

/// 把 core 帧里的 Sixel 图片同步成 Slint Image overlay。
///
/// - 只在图片集合或锚点变化时重建模型；光标闪烁/纯文本输出不会重新上传图片。
/// - Slint Image 按 core 图片 ID 缓存，滚动/resize 时只改 x/y，不重复拷贝 RGBA。
fn sync_inline_images(rt: &mut PaneRuntime, frame: &Frame) {
    let sig = inline_image_sig(frame);
    let geometry = (
        rt.renderer.cell_w.max(1),
        rt.renderer.cell_h.max(1),
        rt.grid.0,
        rt.grid.1,
    );
    if sig == rt.inline_sig && geometry == rt.inline_geometry {
        return;
    }
    rt.inline_sig = sig;
    rt.inline_geometry = geometry;
    if std::env::var_os("TERVO_SIXEL_DEBUG").is_some() {
        let pos = rt.handle.scroll_position();
        eprintln!(
            "[sixel] offset={} scroll=({},{}) images={:?}",
            frame.display_offset,
            pos.offset,
            pos.history,
            frame
                .images
                .iter()
                .map(|i| (i.id, i.row, i.col))
                .collect::<Vec<_>>()
        );
    }

    let cell_w = rt.renderer.cell_w.max(1) as f32;
    let cell_h = rt.renderer.cell_h.max(1) as f32;
    let mut views = Vec::with_capacity(frame.images.len());
    let mut hits = HashMap::with_capacity(frame.images.len());
    rt.inline_cache.clear();
    for img in &frame.images {
        if img.width == 0 || img.height == 0 {
            continue;
        }
        let expected = img.width as usize * img.height as usize * 4;
        if img.data.len() != expected {
            continue;
        }
        hits.insert(
            img.id,
            ImageSource {
                source_id: img.source_id,
                width: img.width,
                height: img.height,
                data: img.data.clone(),
            },
        );
        if !rt.inline_cache.contains_key(&img.source_id) {
            rt.inline_cache.insert(
                img.source_id,
                image_from_source(img.width, img.height, &img.data),
            );
        }
        let x = img.col as f32 * cell_w;
        let y = img.row as f32 * cell_h;
        // Sixel/IIP 的原始像素尺寸可能远大于终端；以前直接按原始 w/h 绘制，
        // 只能被 pane clip 截掉。这里先按 pane 可用区域等比缩小，保留原图给灯箱。
        let avail_w = (rt.grid.0 as f32 * cell_w - x).max(cell_w);
        let avail_h = (rt.grid.1 as f32 * cell_h - y).max(cell_h);
        let (w, h) = crate::render::fit_inline_image_size(img.width, img.height, avail_w, avail_h);
        views.push(PaneImage {
            id: img.id as i32,
            x,
            y,
            w,
            h,
        });
    }
    rt.inline_hits = hits;
    rt.inline_model.set_vec(views);
}

fn inline_image_sig(frame: &Frame) -> Vec<(u64, u64, i32, u16, u32, u32)> {
    frame
        .images
        .iter()
        .map(|img| (img.id, img.source_id, img.row, img.col, img.width, img.height))
        .collect()
}

fn refresh_inline_hits(rt: &mut PaneRuntime, frame: &Frame) {
    rt.inline_hits = frame
        .images
        .iter()
        .filter(|img| {
            img.width > 0
                && img.height > 0
                && img.data.len() == img.width as usize * img.height as usize * 4
        })
        .map(|img| {
            (
                img.id,
                ImageSource {
                    source_id: img.source_id,
                    width: img.width,
                    height: img.height,
                    data: img.data.clone(),
                },
            )
        })
        .collect();
}

fn cached_image_source(rt: &mut PaneRuntime, source_id: u64) -> Option<slint::Image> {
    if let Some(image) = rt.inline_cache.get(&source_id) {
        return Some(image.clone());
    }
    let source = rt
        .inline_hits
        .values()
        .find(|source| source.source_id == source_id)
        .cloned()?;
    let image = image_from_source(source.width, source.height, &source.data);
    rt.inline_cache.insert(source_id, image.clone());
    Some(image)
}

fn image_from_source(width: u32, height: u32, rgba: &[u8]) -> slint::Image {
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(rgba);
    slint::Image::from_rgba8(buffer)
}

fn redraw_pane_inner(st: &mut AppState, id: PaneId) {
    let focused = st.ws.active_tab().map(|t| t.focused) == Some(id);
    let blink_on = st.cursor_blink_on;
    let global_blink = st.cfg.cursor_blinking();
    let policy = st.cfg.scrollbar_policy();
    // 回收已关闭 pane 的 GPU 纹理/离屏 target，避免全局 HashMap 持有已删除 pane。
    if gpu_present::ready() || gpu_native::ready() {
        let active: Vec<u64> = st.panes.keys().copied().collect();
        if gpu_present::ready() {
            gpu_present::retain_for(st.window_id, &active);
        }
        if gpu_native::ready() {
            gpu_native::retain_for(st.window_id, &active);
        }
    }
    let Some(rt) = st.panes.get_mut(&id) else {
        return;
    };
    let frame = rt.handle.pull(std::mem::take(&mut rt.frame));
    if frame.skipped {
        // actor 忙/异常：保留上一帧画面，不重绘；下一次 pull 会 force_full 补全。
        rt.frame = frame;
        return;
    }
    rt.cursor = (frame.cursor.line as usize, frame.cursor.col as usize);
    // 诊断：TERVO_FRAME_DUMP=/tmp/frame.txt 时，每次重绘前把 Frame 文本落盘。
    if let Some(path) = std::env::var_os("TERVO_FRAME_DUMP") {
        let mut out = String::with_capacity(frame.rows * frame.cols + frame.rows);
        for row in 0..frame.rows {
            for col in 0..frame.cols {
                out.push(frame.cell(row, col).char());
            }
            out.push('\n');
        }
        let _ = std::fs::write(&path, out);
    }
    // TERVO_GPU_NATIVE=1：真实 Frame -> Scene -> WGPU glyph atlas/背景，先尝试
    // GPU 出图。成功时**不**碰 CPU framebuffer，也不走 stage 0 纹理上传。
    // 失败返回 None 才回 CPU，并强制下一帧整屏重绘。
    let native_image = if gpu_native::ready() {
        gpu_native::pane_image(&gpu_native::PaneFrame {
            pane: id,
            frame: &frame,
            cols: rt.renderer.cols,
            rows: rt.renderer.rows,
            cell_w: rt.renderer.cell_w,
            cell_h: rt.renderer.cell_h,
            baseline: rt.renderer.baseline(),
            font_size: rt.renderer.font_size(),
            font_paths: rt.renderer.font_paths(),
            theme_bg: rt.renderer.theme_bg_rgb(),
        })
    } else {
        None
    };

    let gpu_before = gpu_present::upload_stats();
    if let Some(image) = native_image {
        rt.gpu_native_active = true;
        rt.image = image;
    } else {
        if rt.gpu_native_active {
            // GPU 期间 CPU 双缓冲的 damage 历史是旧的：回退首帧必须整屏。
            rt.renderer.force_full_redraw();
            rt.gpu_native_active = false;
        }
        // 光标不进帧缓冲：用独立 overlay，闪烁时只换 visible。
        rt.renderer.draw_without_cursor(&frame);
        // stage 0 只作为显式 TERVO_GPU_NATIVE=0 的兼容路径；默认 GPU-native
        // 失败时直接 CPU，绝不偷偷回“CPU framebuffer 上传”冒充 GPU。
        let gpu_image = if gpu_present::ready() && !gpu_native::requested() {
            gpu_present::pane_image(
                id,
                rt.renderer.pixels(),
                rt.renderer.width,
                rt.renderer.height,
                rt.renderer.last_draw_full,
                rt.renderer.dirty_rows(),
                !st.resize_in_progress,
            )
        } else {
            None
        };
        rt.image = match gpu_image {
            Some(image) => image,
            None => rt.renderer.image(),
        };
    }
    let gpu_after = gpu_present::upload_stats();
    if rt.gpu_native_active {
        // GPU Scene 已经按 Scene.image 合成图片；再显示 Slint 图片 overlay 会双画。
        rt.inline_model.set_vec(Vec::new());
        rt.inline_sig.clear();
    } else {
        sync_inline_images(rt, &frame);
    }
    // 可见纹理缓存与点击 source 分离：即使 Slint 纹理缓存淘汰，本帧命中的
    // 不可变 RGBA 仍在，首次点击无需再等 actor 回包。
    refresh_inline_hits(rt, &frame);
    if render_profile() && !rt.gpu_native_active {
        eprintln!(
            "[render] pane={id} grid={}x{} full={} rows={} copy={} full_redraws={} \
             damage_rows={} draw={:.2}ms image={:.2}ms detach={} bytes={}",
            frame.cols,
            frame.rows,
            rt.renderer.last_draw_full,
            rt.renderer.last_draw_rows,
            rt.renderer.last_copied_rows,
            rt.renderer.full_redraws,
            frame.damage.len(),
            rt.renderer.last_draw_us,
            rt.renderer.last_image_us,
            rt.renderer.last_buffer_detached,
            rt.renderer.width * rt.renderer.height * 4
        );
        let gpu_rows = gpu_after.rows.saturating_sub(gpu_before.rows);
        let gpu_bytes = gpu_after.bytes.saturating_sub(gpu_before.bytes);
        let gpu_ns = gpu_after
            .upload_nanos
            .saturating_sub(gpu_before.upload_nanos);
        if gpu_after.frames > gpu_before.frames {
            eprintln!(
                "[gpu-profile] frames={} rows={} bytes={} upload_us={:.2} total_frames={}",
                gpu_after.frames.saturating_sub(gpu_before.frames),
                gpu_rows,
                gpu_bytes,
                gpu_ns as f64 / 1000.0,
                gpu_after.frames
            );
        }
    }

    if focused {
        st.echo
            .store(count_marker(&frame, MARKER), Ordering::Release);
    }

    rt.frame = frame;
    refresh_cursor_overlay(rt);
    if let Some(&row) = st.pane_rows.get(&id) {
        st.pane_model.set_row_data(
            row,
            pane_view(id, rt, focused, blink_on, global_blink, policy),
        );
    }
    if focused {
        update_ime_spot(st);
    }
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
fn apply_font_size(st: &mut AppState) -> bool {
    let Some(fonts) = FontSet::load(st.cfg.font_size) else {
        eprintln!("[config] 字体加载失败，字号变更忽略");
        return false;
    };
    let global_fonts = Rc::new(fonts);
    st.cell = (global_fonts.cell_w, global_fonts.cell_h);
    st.fonts = global_fonts.clone();
    let global_size = st.cfg.font_size;

    // 每个不同的 Profile 字号只加载一次字体集（同一字号的多个 pane 共享）。
    let mut sizes: Vec<u32> = st
        .panes
        .values()
        .filter_map(|rt| rt.appearance.font_size)
        .filter(|size| (*size - global_size).abs() > f32::EPSILON)
        .map(f32::to_bits)
        .collect();
    sizes.sort_unstable();
    sizes.dedup();
    let mut cache: HashMap<u32, Rc<FontSet>> = HashMap::new();
    for bits in sizes {
        let size = f32::from_bits(bits);
        match FontSet::load(size) {
            Some(fonts) => {
                cache.insert(bits, Rc::new(fonts));
            },
            None => eprintln!("[config] Profile 字号 {size} 加载字体失败，回退全局字号"),
        }
    }

    let fallback = st.resolved_theme.clone();
    let registry = &st.registry;
    let cfg = &st.cfg;
    let opacity = renderer_opacity(st);
    let global_shape = cfg.cursor_shape();
    let (cursor_color, cursor_text_color) = (cfg.cursor_color_rgb(), cfg.cursor_text_color_rgb());
    for rt in st.panes.values_mut() {
        let fonts = rt
            .appearance
            .font_size
            .and_then(|size| cache.get(&size.to_bits()).cloned())
            .unwrap_or_else(|| global_fonts.clone());
        let (cols, rows) = rt.grid;
        let theme = rt
            .appearance
            .theme
            .as_deref()
            .and_then(|sel| theme_for_selection(registry, cfg, Some(sel)))
            .unwrap_or_else(|| fallback.clone());
        rt.cell = (fonts.cell_w, fonts.cell_h);
        // IIP 的 width/height 以 cell 为单位时需要最新字号；Sixel 可见性也用 cell 高。
        rt.handle
            .set_cell_size(fonts.cell_w as u16, fonts.cell_h as u16);
        rt.renderer = Renderer::new(
            fonts,
            cols,
            rows,
            theme.spec.bg,
            theme.spec.cursor,
            opacity,
        );
        let shape = rt
            .appearance
            .cursor_shape
            .as_deref()
            .map(tervo_core::CursorShape::from_name)
            .unwrap_or(global_shape);
        rt.renderer
            .set_cursor_style(shape, cursor_color, cursor_text_color);
    }
    refresh_views(st);
    redraw_visible(st);
    true
}

fn redraw_all() {
    // 所有窗口都重绘；不能只取 active window 的 pane。
    let ids = runtime::all_pane_ids();
    for id in ids {
        redraw_pane(id);
    }
}

/// 环境变量是否被设置为“开”（任意非空值都可，`0` 视为关）。
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| v != "0")
}

fn snapshot_path(base: &Path, suffix: &str) -> PathBuf {
    let mut p = base.as_os_str().to_os_string();
    p.push(suffix);
    PathBuf::from(p)
}

/// 无头验收：`TERVO_GPU_SNAPSHOT=<前缀>` 时用 `Window::take_snapshot()` 读回
/// Slint 合成后的像素，写二进制 PPM，并打印非背景像素/颜色统计。
///
/// 可选：
/// - `TERVO_GPU_SNAPSHOT_RESIZE=1`：首张快照后程序化改窗口尺寸，1.5s 后再拍一张
///   （验证 resize 不黑；走 Slint `resized` 回调 + PTY resize）。
/// - `TERVO_GPU_SNAPSHOT_EXIT=1`：拍完退出事件循环，供脚本无头跑。
fn render_hud_forced_by_env() -> bool {
    std::env::var("TERVO_RENDER_HUD").is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

fn install_render_hud(st: &mut AppState) {
    let forced = render_hud_forced_by_env();
    st.ui.set_cfg_render_hud(st.cfg.render_hud);
    set_render_hud(st, forced || st.cfg.render_hud);
}

/// 启动/停止 HUD 采样；重复调用幂等。设置页和环境变量都走这里。
fn set_render_hud(st: &mut AppState, enabled: bool) {
    if !enabled {
        st.render_hud_timer = None;
        st.ui.set_render_hud("".into());
        return;
    }
    if st.render_hud_timer.is_some() {
        return;
    }
    let weak = st.ui.as_weak();
    let window_id = st.window_id;
    let mut last = Instant::now();
    let mut last_frames = render_probe::frames();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(500),
        move || {
            let now = Instant::now();
            let frames = render_probe::frames();
            let elapsed = now.duration_since(last).as_secs_f64().max(0.001);
            let fps = frames.saturating_sub(last_frames) as f64 / elapsed;
            last = now;
            last_frames = frames;
            let native = gpu_native::totals();
            let uploads = gpu_present::upload_stats();
            let grid = runtime::with_window(window_id, |st| {
                st.panes
                    .values()
                    .next()
                    .map(|rt| (rt.renderer.cols, rt.renderer.rows))
            })
            .flatten()
            .unwrap_or((0, 0));
            let text = format!(
                "FPS {fps:>5.1}  {:>5.1}ms | {} | native {}f/{}dc partial {}p full {}f reuse {}r rects {} atlas {}up/{}B | stage0 {}f/{}B | {}x{}",
                if fps > 0.0 { 1000.0 / fps } else { 0.0 },
                render_probe::backend_label(),
                native.frames,
                native.draw_calls,
                native.partial_frames,
                native.full_frames,
                native.reused_frames,
                native.damage_rects,
                native.atlas_uploads,
                native.atlas_uploaded_bytes,
                uploads.frames,
                uploads.bytes,
                grid.0,
                grid.1
            );
            if let Some(ui) = weak.upgrade() {
                ui.set_render_hud(text.into());
            }
        },
    );
    st.render_hud_timer = Some(timer);
}

fn install_gpu_snapshot_hook(ui: &MainWindow) {
    let Some(base) = std::env::var_os("TERVO_GPU_SNAPSHOT") else {
        return;
    };
    let base = PathBuf::from(base);
    let base_for_log = base.clone();
    let want_resize = env_flag("TERVO_GPU_SNAPSHOT_RESIZE");
    let exit_after = env_flag("TERVO_GPU_SNAPSHOT_EXIT");
    // llvmpipe first WGPU present can land after 2.5s; tests that need the
    // imported texture to have been presented should pass a larger delay.
    let first_delay = std::env::var("TERVO_GPU_SNAPSHOT_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(2500));
    let weak = ui.as_weak();
    let started = Instant::now();
    let stage = Rc::new(std::cell::Cell::new(0u8));
    let resized_at = Rc::new(std::cell::Cell::new(None::<Instant>));
    let timer = slint::Timer::default();
    let stage_c = stage.clone();
    let resized_at_c = resized_at.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            match stage_c.get() {
                0 if started.elapsed() >= first_delay => {
                    take_snapshot_and_log(&ui, &snapshot_path(&base, ".before.ppm"));
                    stage_c.set(1);
                    if want_resize {
                        let size = ui.window().size();
                        let (w, h) = (size.width + 160, size.height + 120);
                        eprintln!("[gpu-snapshot] resize {size:?} -> {w}x{h}");
                        ui.window().set_size(slint::PhysicalSize::new(w, h));
                        resized_at_c.set(Some(Instant::now()));
                    } else if exit_after {
                        slint::quit_event_loop();
                    }
                },
                1 => {
                    let Some(at) = resized_at_c.get() else { return };
                    if at.elapsed() < Duration::from_millis(1500) {
                        return;
                    }
                    take_snapshot_and_log(&ui, &snapshot_path(&base, ".resized.ppm"));
                    stage_c.set(2);
                    if exit_after {
                        slint::quit_event_loop();
                    }
                },
                _ => {},
            }
        },
    );
    // Slint Timer 需在事件循环期间存活；与 app 内现有定时器同样用 forget 保活。
    std::mem::forget(timer);
    eprintln!(
        "[gpu-snapshot] 已安装：base={} resize={} exit={}",
        base_for_log.display(),
        want_resize,
        exit_after
    );
}

fn take_snapshot_and_log(ui: &MainWindow, path: &Path) {
    match ui.window().take_snapshot() {
        Ok(buffer) => {
            let width = buffer.width();
            let height = buffer.height();
            let bytes = buffer.as_bytes();
            let total = width as usize * height as usize;
            let mut histogram: HashMap<u32, usize> = HashMap::new();
            for px in bytes.chunks_exact(4) {
                let key = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
                *histogram.entry(key).or_default() += 1;
            }
            let (top, top_n) = histogram
                .iter()
                .max_by_key(|(_, count)| **count)
                .map(|(color, count)| (*color, *count))
                .unwrap_or((0, 0));
            let nonbg = total.saturating_sub(top_n);
            let center_idx = (height as usize / 2 * width as usize + width as usize / 2) * 4;
            let center = bytes.get(center_idx..center_idx + 4).unwrap_or(&[0; 4]);
            let mut ppm = Vec::with_capacity(total * 3 + 32);
            ppm.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
            for px in bytes.chunks_exact(4) {
                ppm.extend_from_slice(&px[..3]);
            }
            match std::fs::write(path, &ppm) {
                Ok(()) => {
                    let native = gpu_native::totals();
                    let last = gpu_native::last_stats();
                    let text_stats = last.as_ref().and_then(|stats| stats.text.as_ref());
                    eprintln!(
                        "[gpu-snapshot] path={} size={width}x{height} gpu_ready={} \
                         gpu_native_ready={} gpu_native_frames={} draw_calls={} \
                         atlas_uploads={} atlas_bytes={} vertex_bytes={} \
                         image_uploads={} image_sources={} \
                         last(fallback={} missing={} wide={} spacer={} hidden={} dim={} \
                         selected={} searched={} images_skipped={} cursor_skipped={} \
                         background_from_scene={}) total={total} nonbg={nonbg} distinct={} \
                         top=#{top:08x} center=#{}",
                        path.display(),
                        gpu_present::ready(),
                        gpu_native::ready(),
                        native.frames,
                        native.draw_calls,
                        native.atlas_uploads,
                        native.atlas_uploaded_bytes,
                        native.vertex_uploaded_bytes,
                        native.image_uploads,
                        native.image_sources,
                        text_stats.map_or(0, |stats| stats.fallback_glyphs),
                        text_stats.map_or(0, |stats| stats.missing_glyphs),
                        text_stats.map_or(0, |stats| stats.wide_cells),
                        text_stats.map_or(0, |stats| stats.wide_spacer_cells),
                        text_stats.map_or(0, |stats| stats.hidden_cells),
                        text_stats.map_or(0, |stats| stats.dim_cells),
                        text_stats.map_or(0, |stats| stats.selected_cells),
                        text_stats.map_or(0, |stats| stats.searched_cells),
                        last.as_ref().map_or(0, |stats| stats.below.images + stats.above.images),
                        true,
                        true,
                        histogram.len(),
                        center
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>()
                    )
                },
                Err(err) => eprintln!(
                    "[gpu-snapshot] 写 PPM 失败 path={} error={err}",
                    path.display()
                ),
            }
        },
        Err(err) => eprintln!(
            "[gpu-snapshot] take_snapshot 失败 path={} error={err}",
            path.display()
        ),
    }
}

// --------------------------------------------------------------- 选择/搜索

/// Pointer in the top/bottom edge band: +1 = scroll into history (up),
/// -1 = scroll toward latest (down), 0 = no autoscroll.
fn selection_autoscroll_dir(pointer_y: f32, height: f32, cell_h: f32) -> i32 {
    if height <= 0.0 || cell_h <= 0.0 {
        return 0;
    }
    let edge = (cell_h * 0.75).max(1.0);
    if pointer_y <= edge {
        1
    } else if pointer_y >= height - edge {
        -1
    } else {
        0
    }
}

/// Runs while a local text selection drag is held at the top/bottom edge.
/// Scroll and extend are sent in order; the actor reads the new display_offset
/// when it processes the extension, so the selection keeps following history.
fn selection_autoscroll_tick(st: &mut AppState) {
    let Some(drag) = st.drag.as_ref() else {
        st.drag_autoscroll.stop();
        return;
    };
    if !selection_drag_autoscrolls(drag.mouse, drag.link, drag.moved) {
        return;
    }
    if drag.autoscroll_paused {
        return;
    }
    let id = drag.pane;
    let (x, y) = drag.last_pointer;
    let Some(rt) = st.panes.get(&id) else {
        st.drag_autoscroll.stop();
        return;
    };
    // 拖出顶层窗口：停表并记住“已暂停”。窗口内（包括 tab bar/其它 pane）
    // 仍按拖拽 pane 的边缘继续滚动；指针回来后由 move 回调恢复计时器。
    let outside =
        pointer_outside_window(rt.rect.x, rt.rect.y, x, y, st.content.w, st.content.h);
    if outside {
        st.drag_autoscroll.stop();
        if let Some(drag) = st.drag.as_mut() {
            if !drag.autoscroll_paused {
                drag.autoscroll_paused = true;
                drag.autoscroll_dir = 0;
                eprintln!(
                    "[select] autoscroll-stop pane={id} reason=pointer-outside-window pointer=({x:.1},{y:.1})"
                );
            }
        }
        return;
    }
    let height = rt.renderer.height.max(1) as f32;
    let cell_h = rt.renderer.cell_h.max(1) as f32;
    let dir = selection_autoscroll_dir(y, height, cell_h);
    let pos = rt.handle.scroll_position();
    let Some((delta, row)) =
        selection_autoscroll_step(dir, pos.offset, pos.history, rt.grid.1 as u16)
    else {
        if let Some(drag) = st.drag.as_mut() {
            drag.autoscroll_dir = 0;
        }
        return;
    };
    let col = cell_at(rt, x, y).1;
    if let Some(drag) = st.drag.as_mut() {
        if drag.autoscroll_dir != dir {
            drag.autoscroll_dir = dir;
            eprintln!(
                "[select] autoscroll pane={id} dir={dir} offset={} history={} edge_cell={row},{col}",
                pos.offset, pos.history
            );
        }
    }
    // 已经到历史顶/最新时不再发送无效滚动，但继续把选区扩到边缘行。
    if delta != 0 {
        rt.handle.scroll(delta);
    }
    rt.handle.selection_extend(row, col);
}

/// 自动滚动步进：`(要滚的行数, 要扩到的视口行)`。
///
/// `dir` 来自 [`selection_autoscroll_dir`]：+1 = 向历史（上），-1 = 向最新（下）。
/// 到达边界时只停止滚动、仍返回边缘行，保证选区能扩到视口第一/最后一行。
fn selection_autoscroll_step(
    dir: i32,
    offset: usize,
    history: usize,
    rows: u16,
) -> Option<(i32, u16)> {
    let last = rows.saturating_sub(1);
    match dir {
        1 => Some((i32::from(offset < history), 0)),
        -1 => Some((-i32::from(offset > 0), last)),
        _ => None,
    }
}

/// 只有“本地选词 + 已产生实际位移”的拖拽才运行边缘自动滚动：
/// 转发给 PTY 的鼠标拖动、Ctrl 链接拖动、以及还没移动的按下都不滚。
fn selection_drag_autoscrolls(mouse: bool, link: bool, moved: bool) -> bool {
    !mouse && !link && moved
}

/// 启动 50ms 边缘自动滚动计时器。
///
/// 重复调用是安全的：Slint 会重置周期；暂停后（指针离开窗口）重新进入窗口
/// 也走这里恢复。计时器本体只在拖选按下时启动，抬起/失焦/窗口关闭都会停。
fn start_drag_autoscroll_timer(st: &mut AppState) {
    let window_id = st.window_id;
    st.drag_autoscroll.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            let _ = runtime::with_window(window_id, selection_autoscroll_tick);
        },
    );
}

/// 取消本地选词拖拽并停表。转发给 PTY 的鼠标拖拽/链接拖拽不在这里取消：
/// 它们没有自动滚动，仍应等待真实 release 把状态交回给 TUI。
fn cancel_local_selection_drag(st: &mut AppState, reason: &str) -> bool {
    if !st.drag.as_ref().is_some_and(|d| !d.mouse && !d.link) {
        return false;
    }
    st.drag_autoscroll.stop();
    if let Some(drag) = st.drag.take() {
        eprintln!(
            "[select] drag-cancel pane={} reason={reason} moved={}",
            drag.pane, drag.moved
        );
    }
    true
}

/// 指针（pane 内容坐标）是否已离开顶层窗口。`content_w/content_h` 是
/// Slint 窗口内容区尺寸（高度不含 tab bar），窗口坐标 = tab bar + pane 偏移。
fn pointer_outside_window(
    pane_x: f32,
    pane_y: f32,
    x: f32,
    y: f32,
    content_w: f32,
    content_h: f32,
) -> bool {
    if content_w <= 0.0 || content_h <= 0.0 {
        return false;
    }
    let abs_x = pane_x + x;
    let abs_y = TABBAR_H + pane_y + y;
    abs_x < 0.0 || abs_y < 0.0 || abs_x >= content_w || abs_y >= content_h + TABBAR_H
}

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
    let Some(tab) = st.ws.active_tab() else {
        return;
    };
    let ids = tab.panes();
    if ids.len() < 2 {
        return;
    }
    let cur = tab.focused;
    let Some(cur_rect) = st.panes.get(&cur).map(|rt| rt.rect) else {
        return;
    };
    let (cx, cy) = (cur_rect.x + cur_rect.w / 2.0, cur_rect.y + cur_rect.h / 2.0);
    let mut best: Option<(f32, PaneId)> = None;
    for id in ids {
        if id == cur {
            continue;
        }
        let Some(r) = st.panes.get(&id).map(|rt| rt.rect) else {
            continue;
        };
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
        let win = st.window_focused;
        sync_terminal_focus(st, win);
        eprintln!("[ws] focus pane={id} dir=({dx},{dy})");
    }
}

fn open_pane_menu(st: &mut AppState, id: PaneId, x: f32, y: f32) {
    // 菜单打开时顺带解析“右键那一格”的链接：OSC 8 优先、普通 URL/本地路径兜底。
    // x/y 是内容区坐标，pane rect 也是内容区坐标；超出 pane 时不给链接。
    let target = st.panes.get(&id).and_then(|rt| {
        let (px, py) = (x - rt.rect.x, y - rt.rect.y);
        if px < 0.0 || py < 0.0 || px >= rt.rect.w || py >= rt.rect.h {
            return None;
        }
        let (row, col) = cell_at(rt, px, py);
        frame_target_at(&rt.frame, row, col)
    });
    let link = target.as_ref().map(|t| t.text().to_string()).unwrap_or_default();
    eprintln!(
        "[ui] right click pane={id} at {x},{y} link={} kind={:?}",
        !link.is_empty(),
        target.as_ref().map(LinkTarget::kind_label)
    );
    st.menu_link = link.clone();
    st.ui.set_menu_link(link.as_str().into());
    st.ui
        .set_menu_read_only(st.panes.get(&id).is_some_and(|rt| rt.handle.read_only()));
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
    Modifiers {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        ctrl: bits & 4 != 0,
    }
}

/// Slint 按键号（0=左 1=中 2=右）→ 上报按键。
fn mouse_button(code: i32) -> MouseButton {
    match code {
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => MouseButton::Left,
    }
}

/// 可打开目标：OSC 8 / 行内 URL 或**本地**文件路径。
#[derive(Clone, Debug, PartialEq, Eq)]
enum LinkTarget {
    Uri(String),
    Path(String),
}

impl LinkTarget {
    fn text(&self) -> &str {
        match self {
            Self::Uri(s) | Self::Path(s) => s,
        }
    }

    fn is_path(&self) -> bool {
        matches!(self, Self::Path(_))
    }

    fn kind_label(&self) -> &'static str {
        match self {
            Self::Uri(_) => "uri",
            Self::Path(_) => "path",
        }
    }
}

/// 从一帧的某格解析可打开目标：**OSC 8 优先**，普通 URL/本地路径兜底。
///
/// 裸 IPv4[:port] 虽然能被识别，但这里不返回：没有 scheme 的 IP 很可能只是
/// 日志/版本/时间戳，强行打开会误判（`http://IP:port` 已由 URL 分支覆盖）。
///
/// 不安全的 OSC 8 scheme（`javascript:` / `data:` 等）只记日志，不阻止同格
/// 可见文本里的普通 URL 兜底——否则恶意 OSC 8 可以“劫持”一个可见的 http 链接。
fn frame_target_at(frame: &Frame, row: u16, col: u16) -> Option<LinkTarget> {
    let (row, col) = (row as usize, col as usize);
    if let Some(uri) = frame.hyperlink_at(row, col) {
        if is_allowed_uri(uri) {
            return Some(LinkTarget::Uri(uri.to_string()));
        }
        eprintln!("[link] 拒绝不安全的 OSC 8 URI: {uri:?}（cell={row},{col}）");
    }
    let (kind, text) = frame.link_at(row, col)?;
    match kind {
        LinkKind::Url => {
            if is_allowed_uri(&text) {
                Some(LinkTarget::Uri(text))
            } else {
                eprintln!("[link] 拒绝不安全的 URL: {text:?}（cell={row},{col}）");
                None
            }
        }
        LinkKind::Path => Some(LinkTarget::Path(text)),
        LinkKind::Ipv4 => None,
    }
}

/// hover 用：远端 pane 里的路径不提示可点（不会在本地打开），URL/OSC 8 仍可点。
fn hover_target_at(frame: &Frame, row: u16, col: u16, remote: bool) -> Option<LinkTarget> {
    let target = frame_target_at(frame, row, col)?;
    (!(remote && target.is_path())).then_some(target)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// 把行内路径文本解析成绝对路径；只为 `~` 展开 HOME，不执行任何 shell。
///
/// 相对路径以 pane 的 shell cwd 为基准（`/proc/<pid>/cwd`），不依赖 Tervo
/// 进程自己的 cwd，避免 `cd /tmp` 后点 `./a` 打开到错误目录。
fn resolve_local_path(
    text: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    if text.is_empty() {
        return Err("路径为空".to_string());
    }
    if text.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\0') {
        return Err("路径含空白或控制字符".to_string());
    }
    if text == "~" {
        return home.map(Path::to_path_buf).ok_or_else(|| "无法展开 ~：HOME 未设置".to_string());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        let home = home.ok_or_else(|| "无法展开 ~：HOME 未设置".to_string())?;
        return Ok(home.join(rest));
    }
    let path = Path::new(text);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = cwd.ok_or_else(|| "无法确定当前 shell 目录".to_string())?;
    Ok(cwd.join(path))
}

/// 打开前检查本地路径是否存在且可读；返回给用户的失败原因（不含路径）。
///
/// 用 `access(2)` 而不是尝试 `File::open`：后者对 FIFO/设备文件可能阻塞；
/// 对目录要求 R_OK|X_OK，避免把“存在但不可读”的目录说成能打开。
fn inspect_local_path(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "路径不存在".to_string(),
        std::io::ErrorKind::PermissionDenied => "无权限访问路径".to_string(),
        _ => format!("无法访问路径：{e}"),
    })?;
    // CString 失败只可能是路径里有 NUL；用安全错误而不是 panic。
    let c_path =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| "路径含 NUL".to_string())?;
    let mode = if meta.is_dir() { libc::R_OK | libc::X_OK } else { libc::R_OK };
    if unsafe { libc::access(c_path.as_ptr(), mode) } != 0 {
        let err = std::io::Error::last_os_error();
        return Err(match err.kind() {
            std::io::ErrorKind::PermissionDenied => "无权限读取路径".to_string(),
            _ => format!("路径不可读：{err}"),
        });
    }
    Ok(())
}

/// pane shell 的当前目录；拿不到时退回 Tervo 进程 cwd。
fn pane_cwd(rt: &PaneRuntime) -> Option<PathBuf> {
    rt.handle
        .pid()
        .and_then(session::cwd_of)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
}

/// 本地路径 Ctrl+click：先校验，再在后台用 `xdg-open <绝对路径>` 打开。
///
/// 校验失败（不存在/无权限/路径非法）只写日志 + 底部提示，**不** spawn
/// xdg-open；打开时只传单个参数，绝不拼 shell。
fn open_local_path(text: &str, cwd: Option<&Path>) {
    let home = home_dir();
    let resolved = match resolve_local_path(text, cwd, home.as_deref()) {
        Ok(path) => path,
        Err(e) => {
            let msg = format!("无法打开路径 {text}：{e}");
            eprintln!("[link] {msg}");
            show_link_status(msg);
            return;
        },
    };
    if let Err(e) = inspect_local_path(&resolved) {
        let msg = format!("无法打开路径 {}：{e}", resolved.display());
        eprintln!("[link] {msg}");
        show_link_status(msg);
        return;
    }
    let shown = resolved.display().to_string();
    eprintln!("[link] open path source={text:?} resolved={shown:?}");
    open_with_xdg(resolved.into_os_string(), shown, "path");
}

/// 统一的可打开目标动作：URL 走白名单，本地路径做存在/权限检查，远端路径拒绝。
fn open_target(target: &LinkTarget, cwd: Option<&Path>, remote: bool) {
    match target {
        LinkTarget::Uri(uri) => {
            if !is_allowed_uri(uri) {
                eprintln!("[link] 拒绝打开不安全的 URI: {uri:?}");
                return;
            }
            let shown = uri.clone();
            eprintln!("[link] open uri={shown:?}");
            open_with_xdg(std::ffi::OsString::from(uri), shown, "uri");
        },
        LinkTarget::Path(path) => {
            if remote {
                let msg = format!("远程会话中的路径不会在本地打开：{path}");
                eprintln!("[link] {msg}");
                show_link_status(msg);
                return;
            }
            open_local_path(path, cwd);
        },
    }
}

/// xdg-open 结果观察者：Help 页用它把失败写回页面状态；普通链接传 None。
type XdgObserver = Option<std::sync::Arc<dyn Fn(Result<(), String>) + Send + Sync + 'static>>;

/// 在 UI 线程外跑 `xdg-open`，失败时把原因写日志并显示短暂浮层。
///
/// - 用 arg 传 URI/路径，绝不拼 shell，天然避免命令注入；
/// - stdin/stdout 丢弃，stderr 截断后反馈；
/// - `xdg-open` 进程失败（非 0 退出）也算失败，不能只看 spawn 是否成功。
fn open_with_xdg(target: std::ffi::OsString, shown: String, kind: &'static str) {
    let fail_prefix = if kind == "path" { "打开路径失败" } else { "打开链接失败" };
    open_with_xdg_observed(target, shown, kind, fail_prefix.to_string(), None);
}

/// 统一实现：链接打开、Advanced“打开目录”、Help 页目录按钮共用。
fn open_with_xdg_observed(
    target: std::ffi::OsString,
    shown: String,
    kind: &'static str,
    fail_prefix: String,
    observer: XdgObserver,
) {
    let thread_target = target.clone();
    let thread_shown = shown.clone();
    let thread_observer = observer.clone();
    let thread_prefix = fail_prefix.clone();
    let sep = if fail_prefix.is_ascii() { ": " } else { "：" };
    let spawned = std::thread::Builder::new().name("xdg-open".into()).spawn(move || {
        let target = thread_target;
        let shown = thread_shown;
        let result = std::process::Command::new("xdg-open")
            .arg(&target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        match result {
            Ok(out) if out.status.success() => {
                eprintln!("[link] xdg-open ok {kind}={shown:?}");
                if let Some(obs) = &thread_observer {
                    obs(Ok(()));
                }
            },
            Ok(out) => {
                let detail: String = String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .chars()
                    .take(300)
                    .collect();
                let msg = if detail.is_empty() {
                    format!("{thread_prefix}{sep}xdg-open {}", out.status)
                } else {
                    format!("{thread_prefix}{sep}xdg-open {} — {detail}", out.status)
                };
                eprintln!("[link] {msg} {kind}={shown:?}");
                if let Some(obs) = &thread_observer {
                    obs(Err(msg.clone()));
                }
                show_link_status(msg);
            },
            Err(e) => {
                let run_detail = if thread_prefix.is_ascii() {
                    format!("could not run xdg-open ({e})")
                } else {
                    format!("无法运行 xdg-open（{e}）")
                };
                let msg = format!("{thread_prefix}{sep}{run_detail}");
                eprintln!("[link] {msg} {kind}={shown:?}");
                if let Some(obs) = &thread_observer {
                    obs(Err(msg.clone()));
                }
                show_link_status(msg);
            },
        }
    });
    if let Err(e) = spawned {
        let detail = if fail_prefix.is_ascii() {
            format!("could not spawn xdg-open thread ({e})")
        } else {
            format!("无法创建 xdg-open 线程（{e}）")
        };
        let msg = format!("{fail_prefix}{sep}{detail}");
        eprintln!("[link] {msg} {kind}={shown:?}");
        if let Some(obs) = &observer {
            obs(Err(msg.clone()));
        }
        show_link_status(msg);
    }
}

/// 只在悬停状态变化时切系统指针；不触发终端帧重绘。
fn set_link_cursor(st: &AppState, hand: bool) {
    use winit::window::CursorIcon;
    let icon = if hand { CursorIcon::Pointer } else { CursorIcon::Default };
    let _ = st.ui.window().with_winit_window(|w| w.set_cursor(icon));
    eprintln!("[link] cursor={}", if hand { "pointer" } else { "default" });
}

/// 无按键悬停：换格时扫描一次当前格链接，状态变化才更新模型/系统指针。
///
/// 同一格内移动、以及从空白移动到空白都不会进入扫描；因此不会每帧重绘终端。
fn update_link_hover(st: &mut AppState, id: PaneId, x: f32, y: f32) {
    // 拖选/拖分隔条期间不切手型，避免和选择交互抢指针。
    if st.drag.is_some() {
        return;
    }
    let (cell, remote, was) = {
        let Some(rt) = st.panes.get(&id) else { return };
        (cell_at(rt, x, y), rt.remote, rt.link_hover)
    };
    if st.link_hover_cell == Some((id, cell)) {
        return;
    }
    st.link_hover_cell = Some((id, cell));
    let target = st
        .panes
        .get(&id)
        .and_then(|rt| hover_target_at(&rt.frame, cell.0, cell.1, remote));
    let hand = target.is_some();
    if hand == was {
        return;
    }
    if let Some(rt) = st.panes.get_mut(&id) {
        rt.link_hover = hand;
    }
    refresh_pane_view(st, id);
    set_link_cursor(st, hand);
    match target {
        Some(t) => eprintln!(
            "[link] hover pane={id} cell={},{} kind={} text={:?}",
            cell.0,
            cell.1,
            t.kind_label(),
            t.text()
        ),
        None => eprintln!("[link] hover end pane={id} cell={},{}", cell.0, cell.1),
    }
}

/// 指针离开 pane 的 TouchArea：清掉手型，避免残留到 TabBar/设置页。
fn clear_link_hover(st: &mut AppState, id: PaneId) {
    if st.link_hover_cell.is_some_and(|(pane, _)| pane == id) {
        st.link_hover_cell = None;
    }
    let was = st.panes.get(&id).is_some_and(|rt| rt.link_hover);
    if !was {
        return;
    }
    if let Some(rt) = st.panes.get_mut(&id) {
        rt.link_hover = false;
    }
    refresh_pane_view(st, id);
    set_link_cursor(st, false);
    eprintln!("[link] hover end pane={id} leave");
}

/// 底部短暂状态浮层；4 秒后自动清空。调用方可在任意线程（内部切回 UI 线程）。
fn show_link_status(msg: String) {
    let _ = slint::invoke_from_event_loop(move || {
        runtime::with_active_window(|st| set_status_toast(st, msg));
    });
}

/// 已在 UI 线程时的状态浮层版本（Read Only / Save Contents 等直接调用）。
fn set_status_toast(st: &mut AppState, msg: String) {
    st.ui.set_link_status(msg.as_str().into());
    let ui = st.ui.clone_strong();
    slint::Timer::single_shot(Duration::from_secs(4), move || {
        ui.set_link_status("".into());
    });
}

/// 惰性创建 arboard 句柄；没有 X11/剪贴板服务时返回 None（不 panic）。
fn with_clipboard<R>(st: &mut AppState, f: impl FnOnce(&mut arboard::Clipboard) -> R) -> Option<R> {
    if st.clipboard.is_none() {
        st.clipboard = arboard::Clipboard::new().ok();
    }
    st.clipboard.as_mut().map(f)
}

/// 写系统剪贴板；保留原有日志格式（soak 统计依赖 `[clipboard] 复制`）。
/// 返回可读错误给 Help 页等需要展示成功/失败状态的调用方。
fn set_clipboard_checked(st: &mut AppState, text: &str) -> Result<(), String> {
    eprintln!(
        "[clipboard] 复制 {} 字节: {:?}",
        text.len(),
        text.chars().take(40).collect::<String>()
    );
    match with_clipboard(st, |cb| cb.set_text(text.to_string())) {
        Some(Ok(())) => Ok(()),
        Some(Err(e)) => {
            eprintln!("[clipboard] 写入失败: {e}");
            Err(format!("clipboard write failed: {e}"))
        },
        None => {
            eprintln!("[clipboard] 不可用");
            Err("clipboard unavailable".to_string())
        },
    }
}

fn set_clipboard(st: &mut AppState, text: &str) {
    if let Err(e) = set_clipboard_checked(st, text) {
        eprintln!("[clipboard] 写入失败: {e}");
    }
}

fn get_clipboard(st: &mut AppState) -> Option<String> {
    with_clipboard(st, |cb| cb.get_text().ok()).flatten()
}

/// 剪贴板里的图像 → 临时 PNG 文件路径。
///
/// 终端 PTY 只能收文本；图片必须先落盘，再把路径作为 bracketed paste
/// 交给 TUI（dsh 会把路径识别成 `[Image #N]`）。文件名无空格，0600。
fn write_clipboard_image(image: arboard::ImageData) -> std::io::Result<PathBuf> {
    if image.width == 0 || image.height == 0 || image.bytes.len() % 4 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "clipboard image has invalid dimensions/pixels",
        ));
    }
    let expected = image.width.saturating_mul(image.height).saturating_mul(4);
    if image.bytes.len() < expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "clipboard image RGBA buffer is short",
        ));
    }
    // 32 MP 上限：避免恶意/误触剪贴板把 UI 线程内存打爆。
    if image.width as u64 * image.height as u64 > 32_000_000 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "clipboard image exceeds 32 MP",
        ));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "tervo-paste-{}-{stamp}.png",
        std::process::id()
    ));
    let file = {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&path)?
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.bytes[..expected])?;
    writer.finish()?;
    Ok(path)
}

fn get_clipboard_image(st: &mut AppState) -> Option<arboard::ImageData> {
    with_clipboard(st, |cb| cb.get_image().ok()).flatten()
}

fn get_primary_image(st: &mut AppState) -> Option<arboard::ImageData> {
    use arboard::{GetExtLinux, LinuxClipboardKind};
    with_clipboard(st, |cb| {
        cb.get().clipboard(LinuxClipboardKind::Primary).image().ok()
    })
    .flatten()
}

/// 有图片就粘贴图片路径；返回 false 表示剪贴板没有图片。
fn paste_clipboard_image(st: &mut AppState, primary: bool) -> bool {
    let image = if primary {
        get_primary_image(st)
    } else {
        get_clipboard_image(st)
    };
    let Some(image) = image else { return false };
    match write_clipboard_image(image) {
        Ok(path) => {
            eprintln!(
                "[clipboard] image paste source={} path={}",
                if primary { "primary" } else { "clipboard" },
                path.display()
            );
            set_status_toast(st, format!("Image pasted: {}", path.display()));
            paste_text_into_focused(st, &path.to_string_lossy());
            true
        }
        Err(e) => {
            eprintln!("[clipboard] image encode failed: {e}");
            false
        }
    }
}

/// 写 X11 PRIMARY（中键粘贴）。arboard 的 Linux 扩展 API 不需要 Wayland data-control。
fn set_primary(st: &mut AppState, text: &str) {
    use arboard::{LinuxClipboardKind, SetExtLinux};
    eprintln!("[clipboard] PRIMARY 写入 {} 字节", text.len());
    match with_clipboard(st, |cb| {
        cb.set()
            .clipboard(LinuxClipboardKind::Primary)
            .text(text.to_string())
    }) {
        Some(Ok(())) => {}
        Some(Err(e)) => eprintln!("[clipboard] PRIMARY 写入失败: {e}"),
        None => eprintln!("[clipboard] 不可用"),
    }
}

fn get_primary(st: &mut AppState) -> Option<String> {
    use arboard::{GetExtLinux, LinuxClipboardKind};
    with_clipboard(st, |cb| {
        cb.get().clipboard(LinuxClipboardKind::Primary).text().ok()
    })
    .flatten()
}

fn clear_clipboard(st: &mut AppState) {
    eprintln!("[clipboard] OSC 52 清空剪贴板");
    match with_clipboard(st, |cb| cb.clear()) {
        Some(Ok(())) => {}
        Some(Err(e)) => eprintln!("[clipboard] 清空失败: {e}"),
        None => eprintln!("[clipboard] 不可用"),
    }
}

fn clear_primary(st: &mut AppState) {
    use arboard::{ClearExtLinux, LinuxClipboardKind};
    eprintln!("[clipboard] OSC 52 清空 PRIMARY");
    match with_clipboard(st, |cb| {
        cb.clear_with().clipboard(LinuxClipboardKind::Primary)
    }) {
        Some(Ok(())) => {}
        Some(Err(e)) => eprintln!("[clipboard] PRIMARY 清空失败: {e}"),
        None => eprintln!("[clipboard] 不可用"),
    }
}

/// 用户主动复制（Ctrl+Shift+C / 右键 Copy / copy-on-select）：同时写普通剪贴板
/// 与 X11 PRIMARY，保证之后中键粘贴也能拿到。
fn set_selection_clipboards(st: &mut AppState, text: &str) {
    set_clipboard(st, text);
    set_primary(st, text);
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
    let text = st
        .panes
        .get(&id)
        .and_then(|rt| rt.handle.copy_selection())
        .and_then(normalize_clipboard_text);
    if let Some(text) = text {
        set_selection_clipboards(st, &text);
    }
}

/// Tabby 的 Clipboard 右键：有选区复制并清除，否则粘贴。
fn copy_or_paste(st: &mut AppState) {
    let Some(id) = st.ws.active_tab().map(|t| t.focused) else {
        return;
    };
    let text = st
        .panes
        .get(&id)
        .and_then(|rt| rt.handle.copy_selection())
        .and_then(normalize_clipboard_text);
    if let Some(text) = text {
        set_selection_clipboards(st, &text);
        if let Some(rt) = st.panes.get(&id) {
            rt.handle.clear_selection();
        }
        eprintln!(
            "[ui] right-click clipboard copy {} chars",
            text.chars().count()
        );
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
    let Some(focused) = st.ws.active_tab().map(|t| t.focused) else {
        return;
    };
    if st
        .panes
        .get(&focused)
        .is_some_and(|rt| rt.handle.read_only())
    {
        eprintln!(
            "[readonly] pane={focused} 丢弃粘贴 bytes={}",
            normalized.len()
        );
        set_status_toast(st, "Read Only: paste ignored".to_string());
        return;
    }
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

/// shell 单引号转义：除单引号外的字符全部按字面保留（`$`、空格、`;` 等都不展开）。
///
/// 只做「送进 shell 行编辑器不产生语法/展开」这一层；不做危险命令过滤——
/// bracketed paste 只插入文本，不会自动回车执行。
fn shell_escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// 拖放路径 → shell 命令行文本：逐个单引号转义、空格分隔、末尾留一个空格。
fn drop_paths_text(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&shell_escape_text(&path.to_string_lossy()));
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out
}

/// 把拖入的文件/目录插入目标 pane：尊重 bracketed paste 模式，末尾带空格。
fn drop_files_into_pane(id: PaneId, paths: &[PathBuf]) {
    // PaneId 全进程唯一：拖放结果必须落到该 pane 所属窗口，不能落到 primary。
    let Some(window_id) = runtime::window_for_pane(id) else {
        eprintln!("[ui] drop pane={id} 已关闭，忽略");
        return;
    };
    runtime::with_window(window_id, |st| {
        let Some(rt) = st.panes.get(&id) else { return };
        if rt.handle.read_only() {
            eprintln!("[readonly] pane={id} 丢弃拖放文件 {}", paths.len());
            set_status_toast(st, "Read Only: drop ignored".to_string());
            return;
        }
        let bracketed = rt.frame.mode & BRACKETED_PASTE != 0;
        let text = drop_paths_text(paths);
        let payload = paste_payload(&text, bracketed);
        eprintln!(
            "[ui] drop pane={id} files={} bracketed={bracketed} bytes={}",
            paths.len(),
            payload.len()
        );
        rt.handle.input(&payload);
    });
}

/// 指针当前落在哪个 pane（物理 root 坐标 → 窗口逻辑坐标 → 最近 pane）。
///
/// Slint 1.18 的 winit 后端不把 winit 的 `DroppedFile`/`HoveredFile` 转成
/// `DataTransfer`，拿不到 DropEvent 里的位置；这里直接查 X11 root 指针，
/// 再换算到 Slint 逻辑坐标。非 X11（Wayland）返回 None。
fn drop_target_at(win: &winit::window::Window, window_id: WindowId) -> Option<PaneId> {
    if !winit_window_is_x11(win) {
        return None;
    }
    let (conn, screen) = crate::xconn::connect().ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let (rx, ry) = crate::xconn::query_pointer(&conn, root)?;
    let inner = win.inner_position().ok()?;
    let scale = win.scale_factor() as f32;
    let x = (rx - inner.x) as f32 / scale;
    let y = (ry - inner.y) as f32 / scale;
    runtime::with_window(window_id, |st| {
        let tab = st.ws.active_tab()?;
        let rects: Vec<(PaneId, Rect)> = tab
            .panes()
            .into_iter()
            .filter_map(|id| st.panes.get(&id).map(|rt| (id, rt.rect)))
            .collect();
        nearest_pane(&rects, x, y)
    })
    .flatten()
}

/// 原生窗口事件桥接：文件拖放（Slint 1.18 winit 后端不投递 DropArea），
/// 以及窗口 focus / DPR 变化时的 IME 候选点重同步。
fn install_native_window_events(ui: &MainWindow, window_id: WindowId) {
    let weak = ui.as_weak();
    ui.window().on_winit_window_event(move |win, event| {
        match event {
            winit::event::WindowEvent::Focused(focused) => {
                eprintln!("[focus] {window_id} focused={focused}");
                if *focused {
                    runtime::set_active_window(window_id);
                }
                // 比 50ms 轮询更及时；失焦时不发，等重新获得焦点再强制补发。
                if *focused {
                    runtime::with_window(window_id, |st| {
                        st.ime_spot_dirty = true;
                        update_ime_spot(st);
                    });
                } else {
                    if let Some(ui) = weak.upgrade() {
                        // 窗口失焦：取消可能正在进行的 Tab 拖拽，并复位 ghost/插入线/让位动画。
                        ui.invoke_cancel_drag();
                    }
                    // 选词拖拽没有跨窗口的 release 保证（alt-tab/点其它窗口时 up 可能
                    // 永远不到本窗口）。失焦必须主动取消 drag 并停掉 50ms 自动滚动，
                    // 否则窗口在后台仍会持续滚动历史、且指针回来后旧 drag 会继续扩选。
                    runtime::with_window(window_id, |st| {
                        st.drag_autoscroll.stop();
                        cancel_local_selection_drag(st, "window-blur");
                    });
                }
            }
            winit::event::WindowEvent::CursorLeft { .. } => {
                // 指针离开顶层窗口：报读/转发拖拽不受影响；本地选词拖拽停掉自动
                // 滚动（窗口内仍可继续，包括拖到 tab bar/另一个 pane 上）。
                runtime::with_window(window_id, |st| {
                    st.drag_autoscroll.stop();
                    let Some(drag) = st.drag.as_mut() else { return };
                    if drag.autoscroll_paused
                        || !selection_drag_autoscrolls(drag.mouse, drag.link, drag.moved)
                    {
                        return;
                    }
                    drag.autoscroll_paused = true;
                    drag.autoscroll_dir = 0;
                    eprintln!(
                        "[select] autoscroll-stop pane={} reason=pointer-left-window",
                        drag.pane
                    );
                });
            }
            winit::event::WindowEvent::CursorEntered { .. } => {
                // 左键仍按着拖回窗口：恢复计时器，继续按新的指针位置滚动。
                runtime::with_window(window_id, |st| {
                    let resume = st.drag.as_ref().is_some_and(|drag| {
                        drag.autoscroll_paused
                            && selection_drag_autoscrolls(drag.mouse, drag.link, drag.moved)
                    });
                    if !resume {
                        return;
                    }
                    if let Some(drag) = st.drag.as_mut() {
                        drag.autoscroll_paused = false;
                    }
                    start_drag_autoscroll_timer(st);
                    eprintln!("[select] autoscroll-resume reason=pointer-entered-window");
                });
            }
            winit::event::WindowEvent::CloseRequested => {
                // 多窗口：关闭 B 只销毁 B 的 PTY/UI/GPU surface，A 继续运行。
                // close_window_now 会立即保存剩余窗口布局；最后一个显式退出，
                // 让主流程完成最终 save + mark_clean。
                if runtime::window_count() > 1 {
                    eprintln!("[ui] {window_id} close requested（多窗口，保留其它窗口）");
                    close_window_now(window_id, "wm-close");
                } else {
                    eprintln!("[ui] {window_id} close requested（最后一个窗口，退出）");
                    if let Some(ui) = weak.upgrade() {
                        let _ = ui.hide();
                    }
                    let _ = slint::quit_event_loop();
                }
                return EventResult::Propagate;
            }
            winit::event::WindowEvent::ScaleFactorChanged { .. } => {
                // DPR 变化后逻辑尺寸可能还没更新；强制按新 scale 重发 spot。
                runtime::with_window(window_id, |st| {
                    st.ime_spot_dirty = true;
                    update_ime_spot(st);
                });
            }
            winit::event::WindowEvent::HoveredFile(_) => {
                let target = win
                    .with_winit_window(|win| drop_target_at(win, window_id))
                    .flatten();
                if let Some(ui) = weak.upgrade() {
                    ui.set_drop_hover_pane(target.map(|id| id as i32).unwrap_or(-1));
                }
                eprintln!("[ui] native drag hover pane={target:?}");
            }
            winit::event::WindowEvent::HoveredFileCancelled => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_drop_hover_pane(-1);
                }
                eprintln!("[ui] native drag cancelled");
            }
            winit::event::WindowEvent::DroppedFile(path) => {
                let target = win
                    .with_winit_window(|win| drop_target_at(win, window_id))
                    .flatten()
                    .or_else(|| runtime::with_window(window_id, |st| st.ws.active_tab().map(|t| t.focused)).flatten());
                if let Some(ui) = weak.upgrade() {
                    ui.set_drop_hover_pane(-1);
                }
                let Some(id) = target else {
                    eprintln!("[ui] native drop 无目标 pane，忽略 {}", path.display());
                    return EventResult::Propagate;
                };
                eprintln!("[ui] native drop pane={id} path={}", path.display());
                drop_files_into_pane(id, std::slice::from_ref(path));
                // 拖到哪个 pane 就聚焦哪个，随后输入落到刚插入的路径上。
                runtime::with_window(window_id, |st| focus_pane_inner(st, id));
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

fn paste_text_into_focused(st: &mut AppState, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(id) = focused_pane(st) {
        if st.panes.get(&id).is_some_and(|rt| rt.handle.read_only()) {
            eprintln!("[readonly] pane={id} 丢弃粘贴 bytes={}", text.len());
            set_status_toast(st, "Read Only: paste ignored".to_string());
            return;
        }
    }
    if needs_paste_confirm(text, st.cfg.confirm_multiline_paste) {
        let lines = text.lines().count();
        let preview: String = text.lines().take(3).collect::<Vec<_>>().join("\n");
        let preview = if lines > 3 {
            format!("{preview}\n…")
        } else {
            preview
        };
        eprintln!("[ui] paste confirm lines={lines}");
        st.pending_paste = Some(text.to_string());
        st.ui.set_paste_lines(lines as i32);
        st.ui.set_paste_preview(preview.as_str().into());
        st.ui.set_paste_open(true);
        return;
    }
    deliver_paste(st, text);
}

fn paste_into_focused(st: &mut AppState) {
    if paste_clipboard_image(st, false) {
        return;
    }
    let Some(text) = get_clipboard(st) else {
        return;
    };
    paste_text_into_focused(st, &text);
}

/// 中键粘贴：优先 X11 PRIMARY，没有内容（或读取失败）再回退普通剪贴板。
fn paste_primary_into_focused(st: &mut AppState) {
    if let Some(text) = get_primary(st).filter(|t| !t.is_empty()) {
        eprintln!(
            "[clipboard] middle paste source=primary bytes={}",
            text.len()
        );
        paste_text_into_focused(st, &text);
    } else if paste_clipboard_image(st, true) {
        eprintln!("[clipboard] middle paste source=primary image");
    } else {
        eprintln!("[clipboard] middle paste source=clipboard (primary empty)");
        paste_into_focused(st);
    }
}

fn update_search_bar(st: &mut AppState) {
    st.ui.set_search_query(st.search_query.as_str().into());
    st.ui.set_search_match_case(st.search_match_case);
    st.ui.set_search_regex(st.search_regex);
}

/// 搜索条右侧的 `当前/总数` 文本；无命中固定 `0/0`。
fn search_count_label(hit: Option<&SearchHit>) -> String {
    match hit {
        Some(hit) if hit.total > 0 => format!("{}/{}", hit.index.max(1), hit.total),
        _ => "0/0".to_string(),
    }
}

/// 搜索框文本由 Slint TextInput 编辑（鼠标点击/选区/Ctrl+A/C/V 原生支持），
/// `edited` 只带文本变化、不带内容，所以从 UI 读回归一到 Rust 状态。
fn search_edited(st: &mut AppState) {
    st.search_query = st.ui.get_search_query().to_string();
    run_search(st, true);
}

/// 当前查找选项快照。
fn search_options(st: &AppState) -> SearchOptions {
    SearchOptions {
        match_case: st.search_match_case,
        regex: st.search_regex,
    }
}

/// 切换 Match case；查询非空时从当前位置重新搜索（并清掉旧命中起点）。
fn toggle_search_case(st: &mut AppState) {
    st.search_match_case = !st.search_match_case;
    update_search_bar(st);
    eprintln!(
        "[ui] find option match_case={} regex={}",
        st.search_match_case, st.search_regex
    );
    rerun_search_after_option_change(st);
}

/// 切换 Match regex；非法 regex 的错误由 run_search 显示。
fn toggle_search_regex(st: &mut AppState) {
    st.search_regex = !st.search_regex;
    update_search_bar(st);
    eprintln!(
        "[ui] find option match_case={} regex={}",
        st.search_match_case, st.search_regex
    );
    rerun_search_after_option_change(st);
}

fn rerun_search_after_option_change(st: &mut AppState) {
    // 选项变化后旧匹配位置不再可靠，清掉高亮起点再搜。
    let focused = st.ws.active_tab().map(|t| t.focused);
    if let Some(rt) = focused.and_then(|id| st.panes.get(&id)) {
        rt.handle.search_clear();
    }
    if st.search_query.is_empty() {
        st.ui.set_search_status("0/0".into());
        st.ui.set_search_error(false);
        redraw_visible(st);
    } else {
        run_search(st, true);
    }
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
    // 内置命令 + 已启用 v3 插件命令；插件命令不进静态 Registry，Enable/Disable
    // 后下一次打开 Palette 即生效，不需要重启 Tervo。
    let mut entries: Vec<(String, String)> = st
        .commands
        .search(&query)
        .into_iter()
        .map(|command| (command.title.to_string(), command.id.to_string()))
        .collect();
    entries.extend(
        st.plugins
            .palette_commands(&query)
            .into_iter()
            .map(|(id, title)| (title, id.to_string())),
    );
    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    entries.truncate(10);
    st.palette_ids = entries.iter().map(|(_, id)| id.clone()).collect();
    let items: Vec<slint::SharedString> =
        entries.into_iter().map(|(title, _)| title.into()).collect();
    st.palette_sel = 0;
    st.palette_model.set_vec(items);
    st.ui.set_palette_selected(0);
}

fn open_palette(st: &mut AppState) {
    st.ui.set_palette_query("".into());
    st.ui.set_palette_open(true);
    refresh_palette(st);
    eprintln!(
        "[ui] palette open window={} items={}",
        st.window_id,
        st.palette_ids.len()
    );
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
    let id = st.palette_ids.get(idx).cloned();
    st.ui.set_palette_open(false);
    if let Some(id) = id {
        eprintln!("[ui] run command {id} window={}", st.window_id);
        run_command(st, &id);
    }
}

/// 执行一条命令。ID 与 `commands::builtin()` 一一对应。
fn run_command(st: &mut AppState, id: &str) {
    match id {
        "window.new" => new_window(),
        "tab.new" => {
            new_tab_with_default(st);
            rebuild_tabs(st);
        }
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
        }
        "pane.split-down" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                split_pane_inner(st, focused, Dir::Col);
            }
        }
        "pane.close" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                close_pane_inner(st, focused);
            }
        }
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
        }
        "terminal.copy" => copy_current_selection(st),
        "terminal.paste" => paste_into_focused(st),
        "terminal.find" => open_search(st),
        "terminal.select-all" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                if let Some(rt) = st.panes.get(&focused) {
                    rt.handle.select_all();
                }
            }
        }
        "terminal.clear" => {
            if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                if let Some(rt) = st.panes.get(&focused) {
                    rt.handle.clear_screen();
                }
            }
        }
        "terminal.toggle-read-only" => toggle_read_only(st, focused_pane(st)),
        "terminal.reset" => reset_focused_pane(st, false),
        "terminal.reset-and-clear" => reset_focused_pane(st, true),
        "terminal.save-contents" => {
            if let Some(id) = focused_pane(st) {
                open_save_contents(st, id);
            }
        }
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
            if st
                .plugins
                .spawn_command_for(other, Some(st.window_id.get()))
            {
                eprintln!("[plugin] 已提交后台执行 {other} window={}", st.window_id);
            } else {
                eprintln!("[ui] 未知命令 {other}");
            }
        }
    }
}

fn open_search(st: &mut AppState) {
    st.search_open = true;
    let has_query = !st.search_query.is_empty();
    st.ui.set_search_status("0/0".into());
    st.ui.set_search_error(false);
    // 先同步 query/选项，再显示浮层：TextInput 创建时的 init 会 focus，
    // 文本已就位，Ctrl+F 打开即可直接输入。
    update_search_bar(st);
    st.ui.set_search_open(true);
    eprintln!(
        "[ui] find open case={} regex={}",
        st.search_match_case, st.search_regex
    );
    // 保留上次查询时直接恢复高亮与计数；空查询保持 0/0。
    if has_query {
        run_search(st, true);
    }
}

fn close_search(st: &mut AppState) {
    st.search_open = false;
    st.ui.set_search_open(false);
    st.ui.set_search_status("".into());
    st.ui.set_search_error(false);
    let ids: Vec<PaneId> = st.panes.keys().copied().collect();
    for id in ids {
        if let Some(rt) = st.panes.get(&id) {
            rt.handle.search_clear();
        }
    }
    // 搜索框是真正的 TextInput，关闭后必须把 IME/键盘焦点还给终端；
    // 同时标脏 spot，让下一次轮询把候选窗锚回光标。
    st.ui.invoke_focus_terminal();
    st.ime_spot_dirty = true;
    redraw_visible(st);
}

fn run_search(st: &mut AppState, forward: bool) {
    let query = st.search_query.clone();
    let options = search_options(st);
    let Some(focused) = st.ws.active_tab().map(|t| t.focused) else {
        return;
    };
    if query.is_empty() {
        if let Some(rt) = st.panes.get(&focused) {
            rt.handle.search_clear();
        }
        st.ui.set_search_status("0/0".into());
        st.ui.set_search_error(false);
        redraw_visible(st);
        return;
    }
    let result = st
        .panes
        .get(&focused)
        .map(|rt| rt.handle.search_with(&query, forward, options));
    match result {
        Some(Ok(Some(hit))) => {
            st.ui.set_search_status(search_count_label(Some(&hit)).into());
            st.ui.set_search_error(false);
            eprintln!(
                "[ui] find query={query:?} forward={forward} case={} regex={} hit={hit:?} count={}/{}",
                options.match_case, options.regex, hit.index, hit.total
            );
        }
        Some(Ok(None)) => {
            st.ui.set_search_status("0/0".into());
            st.ui.set_search_error(false);
            eprintln!(
                "[ui] find query={query:?} forward={forward} case={} regex={} hit=none count=0/0",
                options.match_case, options.regex
            );
        }
        Some(Err(err)) => {
            // 搜索条空间小，显示短状态；完整错误进日志，红框提示非法 regex。
            st.ui.set_search_status("invalid regex".into());
            st.ui.set_search_error(true);
            eprintln!(
                "[ui] find query={query:?} forward={forward} case={} regex={} error={err}",
                options.match_case, options.regex
            );
        }
        None => {
            st.ui.set_search_status("0/0".into());
            st.ui.set_search_error(false);
        }
    }
    redraw_visible(st);
}

/// 切换 Read Only（None = 当前焦点 pane）。状态通过菜单项、pane 角标与底部浮层可见。
fn toggle_read_only(st: &mut AppState, pane: Option<PaneId>) {
    let id = pane.or_else(|| st.ws.active_tab().map(|t| t.focused));
    let Some(id) = id else { return };
    let on = match st.panes.get(&id) {
        Some(rt) => {
            let on = !rt.handle.read_only();
            rt.handle.set_read_only(on);
            on
        }
        None => return,
    };
    refresh_pane_view(st, id);
    st.ui.set_menu_read_only(on);
    eprintln!("[ui] read-only pane={id} on={on}");
    set_status_toast(st, format!("Read Only: {}", if on { "ON" } else { "OFF" }));
}

/// 复位指定 pane；`clear_scrollback` 对应 Reset and Clear。
fn reset_pane(st: &mut AppState, id: PaneId, clear_scrollback: bool) {
    let Some(rt) = st.panes.get(&id) else { return };
    if clear_scrollback {
        rt.handle.reset_and_clear();
    } else {
        rt.handle.reset();
    }
    eprintln!("[ui] reset pane={id} clear-scrollback={clear_scrollback}");
    set_status_toast(
        st,
        if clear_scrollback {
            "Terminal reset and cleared"
        } else {
            "Terminal reset"
        }
        .to_string(),
    );
}

fn reset_focused_pane(st: &mut AppState, clear_scrollback: bool) {
    if let Some(id) = st.ws.active_tab().map(|t| t.focused) {
        reset_pane(st, id, clear_scrollback);
    }
}

fn focused_pane(st: &AppState) -> Option<PaneId> {
    st.ws.active_tab().map(|t| t.focused)
}

/// 默认保存路径：`$HOME/tervo-save-<epoch>.txt`；无 HOME 时退到临时目录。
fn default_save_path(stamp: u64) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        PathBuf::from(home).join(format!("tervo-save-{stamp}.txt"))
    } else {
        std::env::temp_dir().join(format!("tervo-save-{stamp}.txt"))
    }
}

/// 展开 `~` / 相对路径，返回真正要写的绝对路径（不访问文件系统）。
fn resolve_save_path(
    raw: &str,
    home: Option<&std::path::Path>,
    cwd: &std::path::Path,
) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("保存路径为空".to_string());
    }
    let path = if raw == "~" {
        home.ok_or_else(|| "无法展开 ~：HOME 未设置".to_string())?
            .to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.ok_or_else(|| "无法展开 ~：HOME 未设置".to_string())?
            .join(rest)
    } else {
        PathBuf::from(raw)
    };
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

/// 写文件（同步；内容量是 scrollback 级别，UI 线程可接受）。返回绝对路径。
fn save_contents_file(
    raw: &str,
    text: &str,
    home: Option<&std::path::Path>,
    cwd: &std::path::Path,
) -> Result<PathBuf, String> {
    let path = resolve_save_path(raw, home, cwd)?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    Ok(path)
}

/// Save Contents 入口：锁存目标 pane，给出明确默认路径，由 UI 弹窗编辑/确认。
fn open_save_contents(st: &mut AppState, pane: PaneId) {
    if !st.panes.contains_key(&pane) {
        set_status_toast(st, "Save Contents: pane 不存在".to_string());
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = default_save_path(stamp);
    st.save_pane = Some(pane);
    st.ui.set_save_path(path.to_string_lossy().as_ref().into());
    st.ui.set_save_status("".into());
    st.ui.set_save_open(true);
    eprintln!(
        "[ui] save-contents open pane={pane} path={}",
        path.display()
    );
}

fn save_contents_commit(st: &mut AppState, raw: &str) {
    let Some(pane) = st.save_pane else {
        st.ui.set_save_status("Save Contents: 没有目标 pane".into());
        return;
    };
    let Some(text) = st.panes.get(&pane).and_then(|rt| rt.handle.contents()) else {
        let msg = "读取终端内容失败（会话可能已结束）";
        eprintln!("[ui] save-contents error pane={pane} path={raw:?} error={msg}");
        st.ui.set_save_status(msg.into());
        return;
    };
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match save_contents_file(raw, &text, home.as_deref(), &cwd) {
        Ok(path) => {
            st.save_pane = None;
            st.ui.set_save_open(false);
            st.ui.set_save_status("".into());
            eprintln!(
                "[ui] save-contents ok pane={pane} path={} bytes={}",
                path.display(),
                text.len()
            );
            set_status_toast(
                st,
                format!("Saved {} bytes to {}", text.len(), path.display()),
            );
            st.ui.invoke_focus_terminal();
        }
        Err(err) => {
            eprintln!("[ui] save-contents error pane={pane} path={raw:?} error={err}");
            st.ui.set_save_status(err.as_str().into());
        }
    }
}

fn cancel_save_contents(st: &mut AppState) {
    st.save_pane = None;
    st.ui.set_save_open(false);
    st.ui.set_save_status("".into());
    st.ui.invoke_focus_terminal();
}

// ------------------------------------------------------------------- 事件

/// Slint/winit 在 X11 下会把修饰键**本身**也当成 key-pressed 上报：
/// Ctrl→U+0011、Shift→U+0010、Alt→U+0012。这些不是用户输入，必须丢掉，
/// 否则终端会莫名收到 0x11/0x10（Ctrl+Q 的真实按键会另发一条 "q" 事件）。
fn is_modifier_artifact(text: &str, ctrl: bool, alt: bool, _shift: bool) -> bool {
    match text {
        // Slint 的修饰键伪字符：Ctrl=\u{11}、Alt=\u{12}、Shift_L=\u{10}、Shift_R=\u{15}。
        // Shift 伪字符在 Ctrl+Shift 组合里会带 ctrl=true（Ctrl+Shift+P/C/V 都会先发
        // 一条 "\u{10}"）；若不无条件过滤，就会往 PTY 写 0x10（Ctrl+P）。
        "\u{11}" => ctrl,
        "\u{12}" => alt,
        "\u{10}" | "\u{15}" => true,
        _ => false,
    }
}

/// 按键是否应把 pane 拉回底部（GNOME scroll-on-keystroke）。
///
/// 只有开启配置且当前确实上滚过才返回 true；判断只读无锁原子快照，
/// 不向 actor 发同步请求。
#[inline]
fn key_scrolls_to_bottom(scroll_on_keystroke: bool, pos: ScrollPosition) -> bool {
    scroll_on_keystroke && pos.offset > 0
}

/// 统一的输入分发：物理按键与 IME 上屏都走这里。
/// `ctrl/alt/shift/meta` 对 IME 提交恒为 false，避免把中文误判成快捷键。
/// `kind` 区分 press/repeat；release 走 [`handle_key_release`]。
fn handle_input(
    st: &mut AppState,
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    kind: keys::KeyEventKind,
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
            if let Some(id) = st
                .commands
                .override_command_for_chord(&chord, &st.cfg.hotkeys)
            {
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
        // Alt+C / Alt+R 是 GNOME Terminal 查找栏的 Match case / regex 开关。
        if alt && !ctrl && !shift {
            match text.to_ascii_lowercase().as_str() {
                "c" => {
                    toggle_search_case(st);
                    return;
                }
                "r" => {
                    toggle_search_regex(st);
                    return;
                }
                _ => {}
            }
        }
        match text {
            "\u{1b}" => {
                close_search(st);
                return;
            }
            "\n" | "\r" => {
                run_search(st, !shift);
                return;
            }
            "\u{8}" | "\u{7f}" => {
                st.search_query.pop();
                update_search_bar(st);
                // 空查询也要跑一次：清掉旧高亮并复位状态。
                run_search(st, true);
                return;
            }
            _ => {}
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
            }
            "v" => {
                paste_into_focused(st);
                return;
            }
            "p" => {
                open_palette(st);
                return;
            }
            "a" => {
                open_tab_search(st);
                return;
            }
            // 部分 X11 文本映射会把 Ctrl+Shift+A 折成控制字符 ^A；仍按 Search Tabs 处理。
            "\u{1}" => {
                open_tab_search(st);
                return;
            }
            "n" => {
                new_window();
                return;
            }
            "e" => {
                if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                    split_pane_inner(st, focused, Dir::Row);
                }
                return;
            }
            "d" => {
                if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                    split_pane_inner(st, focused, Dir::Col);
                }
                return;
            }
            "w" => {
                if let Some(focused) = st.ws.active_tab().map(|t| t.focused) {
                    close_pane_inner(st, focused);
                }
                return;
            }
            "t" => {
                reopen_closed_tab(st);
                return;
            }
            "\u{F700}" => {
                focus_pane_dir(st, 0.0, -1.0);
                return;
            }
            "\u{F701}" => {
                focus_pane_dir(st, 0.0, 1.0);
                return;
            }
            "\u{F702}" => {
                focus_pane_dir(st, -1.0, 0.0);
                return;
            }
            "\u{F703}" => {
                focus_pane_dir(st, 1.0, 0.0);
                return;
            }
            "+" => {
                zoom_font(st, 1.0);
                return;
            }
            _ => {}
        }
    }

    if ctrl {
        match text {
            "t" => {
                new_tab_with_default(st);
                rebuild_tabs(st);
                return;
            }
            "w" => {
                let idx = st.ws.active;
                close_tab_inner(st, idx);
                return;
            }
            "f" => {
                open_search(st);
                return;
            }
            "=" | "+" => {
                zoom_font(st, 1.0);
                return;
            }
            "-" => {
                zoom_font(st, -1.0);
                return;
            }
            "0" => {
                reset_font(st);
                return;
            }
            _ => {}
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
    let Some(tab) = st.ws.active_tab() else {
        return;
    };
    let focused = tab.focused;
    let targets: Vec<PaneId> = if st.broadcast && tab.panes().len() > 1 {
        tab.panes()
    } else {
        vec![focused]
    };
    if st.broadcast && targets.len() > 1 && keylog {
        eprintln!("[send] broadcast to {} panes", targets.len());
    }
    let key_bindings = st.cfg.key_bindings();
    let scroll_on_keystroke = st.cfg.scroll_on_keystroke;
    let held_keys = &mut st.held_keys;
    let panes = &mut st.panes;
    for target in targets {
        match panes.get_mut(&target) {
            Some(rt) => {
                // UI 层先挡一次，避免编码/入队；core 的 input()/actor 仍有第二道门禁。
                if rt.handle.read_only() {
                    eprintln!("[readonly] pane={target} 丢弃按键 text={text:?}");
                    continue;
                }
                let mods = keys::KeyModifiers {
                    ctrl,
                    alt,
                    shift,
                    meta,
                    ..keys::KeyModifiers::default()
                };
                let flags = rt.handle.keyboard_flags();
                let modify_level =
                    if flags.is_empty() { rt.handle.modify_other_keys() } else { 0 };
                let encoded = keys::encode_key_event(
                    text,
                    mods,
                    kind,
                    flags,
                    modify_level,
                    key_bindings,
                );
                if let Some(bytes) = &encoded {
                    // 只有应用请求 REPORT_EVENT_TYPES、且这次 press/repeat 真的以
                    // 转义序列进入 PTY，才登记 release；普通文本键没有 CSI-u press
                    // 就不配对，避免产生孤立 release。
                    if flags.contains(tervo_core::KeyboardFlags::REPORT_EVENT_TYPES)
                        && kind != keys::KeyEventKind::Release
                        && bytes.first() == Some(&0x1b)
                    {
                        if let Some(id) = keys::key_id(text) {
                            let held = held_keys.entry(target).or_default();
                            if held.len() >= 64 {
                                held.clear();
                            }
                            held.insert(id);
                        }
                    }
                    rt.handle.input(bytes);
                    // GNOME scroll_on_keystroke：按键后回到底部。非阻塞读取原子快照，
                    // 只有确实上滚过才发异步滚动消息，避免每次按键都占 ctrl 队列。
                    if key_scrolls_to_bottom(scroll_on_keystroke, rt.handle.scroll_position()) {
                        rt.handle.scroll_to_bottom();
                    }
                    if keylog {
                        eprintln!(
                            "[send] pane={target} flags={:?} modify={modify_level} bytes={bytes:?}",
                            flags.bits()
                        );
                    }
                } else if keylog {
                    eprintln!("[send] pane={target} 未编码 text={text:?}");
                }
            }
            None => {
                if keylog {
                    eprintln!(
                        "[send] 找不到 pane {target}（现有 {:?}）",
                        panes.keys().collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}

/// 回放 kitty release：只发给「press 真的进过 PTY」的 pane。
///
/// 普通 shell / 未请求 REPORT_EVENT_TYPES / 搜索框独占键盘 / 录制快捷键时
/// 全部静默丢弃，保证日常输入零行为变化。
fn handle_key_release(
    st: &mut AppState,
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    keylog: bool,
) {
    if is_modifier_artifact(text, ctrl, alt, shift) {
        return;
    }
    if st.hotkey_recording.is_some() || st.search_open {
        return;
    }
    let Some(tab) = st.ws.active_tab() else {
        return;
    };
    let focused = tab.focused;
    let targets: Vec<PaneId> = if st.broadcast && tab.panes().len() > 1 {
        tab.panes()
    } else {
        vec![focused]
    };
    let mods =
        keys::KeyModifiers { ctrl, alt, shift, meta, ..keys::KeyModifiers::default() };
    let key_bindings = st.cfg.key_bindings();
    let held_keys = &mut st.held_keys;
    let panes = &mut st.panes;
    for target in targets {
        let Some(rt) = panes.get_mut(&target) else { continue };
        if rt.handle.read_only() {
            continue;
        }
        let Some(id) = keys::key_id(text) else { continue };
        // 先消费登记：即使应用刚关掉协议，也不会留下永远等不到的 release。
        let released = held_keys
            .get_mut(&target)
            .map(|held| held.remove(&id))
            .unwrap_or(false);
        if !released {
            continue;
        }
        let flags = rt.handle.keyboard_flags();
        if !flags.contains(tervo_core::KeyboardFlags::REPORT_EVENT_TYPES) {
            continue;
        }
        if let Some(bytes) = keys::encode_key_event(
            text,
            mods,
            keys::KeyEventKind::Release,
            flags,
            0,
            key_bindings,
        ) {
            rt.handle.input(&bytes);
            if keylog {
                eprintln!("[send] pane={target} release bytes={bytes:?}");
            }
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
    let _ = std::thread::Builder::new()
        .name("notify-send".into())
        .spawn(move || {
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

fn selection_label(selection: ClipboardSelection) -> &'static str {
    match selection {
        ClipboardSelection::Clipboard => "clipboard",
        ClipboardSelection::Primary => "primary",
    }
}

/// 消费 core 的 OSC 52 事件。`osc52 = "deny"` 时两个方向都拒绝
/// （query 同样会泄漏本机剪贴板，不能只拦写入）。
fn apply_osc52(st: &mut AppState, pane: PaneId, request: ClipboardRequest) {
    let ts = logging::now_ms();
    if !st.cfg.osc52_allowed() {
        eprintln!(
            "[clipboard] ts_ms={ts} osc52 denied pane={pane} osc52={:?}",
            st.cfg.osc52
        );
        return;
    }
    match request {
        ClipboardRequest::Store { selection, text } => {
            eprintln!(
                "[clipboard] ts_ms={ts} osc52 store pane={pane} selection={} bytes={}",
                selection_label(selection),
                text.len()
            );
            match selection {
                ClipboardSelection::Clipboard => set_clipboard(st, &text),
                ClipboardSelection::Primary => set_primary(st, &text),
            }
        }
        ClipboardRequest::Clear { selection } => {
            eprintln!(
                "[clipboard] ts_ms={ts} osc52 clear pane={pane} selection={}",
                selection_label(selection)
            );
            match selection {
                ClipboardSelection::Clipboard => clear_clipboard(st),
                ClipboardSelection::Primary => clear_primary(st),
            }
        }
        ClipboardRequest::Load { selection } => {
            let text = match selection {
                ClipboardSelection::Clipboard => get_clipboard(st),
                ClipboardSelection::Primary => get_primary(st),
            };
            eprintln!(
                "[clipboard] ts_ms={ts} osc52 query pane={pane} selection={} bytes={}",
                selection_label(selection),
                text.as_ref().map_or(0, String::len)
            );
            if let (Some(text), Some(rt)) = (text, st.panes.get(&pane)) {
                eprintln!(
                    "[clipboard] ts_ms={ts} osc52 reply pane={pane} selection={} bytes={}",
                    selection_label(selection),
                    text.len()
                );
                rt.handle.clipboard_reply(selection, &text);
            }
        }
    }
}

/// 处理已排队的 CoreEvent（标题/Bell/OSC52/通知/进度/Cwd）。
///
/// 事件唤醒与 400ms 兜底都调用它；每次 drain 到空，保证不丢、不重复。
/// 返回 `(是否允许响铃, 本轮 BEL 数)`，响铃由调用方在释放 `AppState` 借用后执行。
fn drain_core_events(st: &mut AppState) -> (bool, u32) {
    let active = st.ws.active;
    let desktop_notifications = st.cfg.desktop_notifications;
    let visual_bell = st.cfg.visual_bell;
    let audible_bell = st.cfg.audible_bell;
    let mut bell_count = 0u32;
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
    // OSC 52 需要读写 st 的剪贴板状态，不能在内层 `rt` 可变借用期间处理；
    // 先收集 (pane, request)，等 pane 遍历结束后统一消费。
    let mut clipboard_events: Vec<(PaneId, ClipboardRequest)> = Vec::new();
    let mut bell_panes: Vec<PaneId> = Vec::new();
    // 远端进程退出：先收集，等 pane 可变借用结束后再改 AppState UI 状态。
    let mut exited_panes: Vec<(PaneId, Option<u64>, u32, Option<String>)> = Vec::new();
    let ids: Vec<PaneId> = st.panes.keys().copied().collect();
    for id in ids {
        let Some(rt) = st.panes.get_mut(&id) else {
            continue;
        };
        let mut events = Vec::new();
        rt.handle.drain_events(&mut events);
        let tab = pane_tab.get(&id).copied();
        let tab_title = tab
            .and_then(|i| tab_titles.get(i))
            .map(String::as_str)
            .unwrap_or("Tervo");
        let is_active = tab == Some(active);
        for ev in events {
            match ev {
                CoreEvent::Title(t) => {
                    if !t.is_empty() && rt.title != t {
                        eprintln!(
                            "[event] ts_ms={} pane={id} kind=title title={t:?}",
                            logging::now_ms()
                        );
                        rt.title = t;
                        titles_dirty = true;
                    }
                }
                CoreEvent::Progress(progress) => {
                    let had_progress = rt.activity.progress.state != ProgressState::Clear;
                    rt.activity.progress = progress;
                    // 进度从“有”变为清除 = 完成；活动 Tab 不放完成标记，避免打扰。
                    rt.activity.completed =
                        progress.state == ProgressState::Clear && had_progress && !is_active;
                    tabs_dirty = true;
                    eprintln!(
                        "[ui] ts_ms={} tab progress pane={id} tab={tab:?} state={:?} percent={:?}",
                        logging::now_ms(),
                        progress.state,
                        progress.percent
                    );
                }
                CoreEvent::Notification(note) => {
                    eprintln!(
                        "[event] ts_ms={} pane={id} kind=notification title={:?}",
                        logging::now_ms(),
                        note.title
                    );
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
                }
                CoreEvent::Bell => {
                    bell_count += 1;
                    eprintln!("[event] ts_ms={} pane={id} kind=bell", logging::now_ms());
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
                    // 视觉铃只闪当前活动 pane；非活动 Tab 仍走未读点/桌面通知。
                    if is_active && visual_bell {
                        bell_panes.push(id);
                    }
                }
                CoreEvent::Cwd(cwd) => {
                    eprintln!(
                        "[ui] ts_ms={} cwd pane={id} tab={tab:?} path={cwd:?}",
                        logging::now_ms()
                    );
                }
                CoreEvent::Clipboard(request) => clipboard_events.push((id, request)),
                CoreEvent::Exited => {
                    // Exited 可能由多个来源重复上报；只有真正等到子进程退出且
                    // 本轮还没提示过时才记一次。
                    if !rt.exit_notified {
                        if let Some((code, signal)) = rt.handle.try_exit_code() {
                            rt.exit_code = Some(code as i32);
                            rt.exit_signal = signal.clone();
                            rt.exit_notified = true;
                            exited_panes.push((id, rt.profile_id, code, signal));
                        }
                    }
                },
                CoreEvent::ResetTitle | CoreEvent::CursorBlinkChange => {}
            }
        }
    }
    for (id, request) in clipboard_events {
        apply_osc52(st, id, request);
    }
    for (id, profile_id, code, signal) in exited_panes {
        note_ssh_exit(st, id, profile_id, code, signal);
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
    for id in bell_panes {
        flash_bell(st, id);
    }
    (audible_bell, bell_count)
}

/// 响铃放在 `with_app` 之外：X11 连接懒建立可能做一次同步 connect，不能占着
/// RefCell 借用；同一轮里的多个 BEL 合并成一次响铃，避免 BEL 风暴。
fn ring_bell(audible_bell: bool, bell_count: u32) {
    if audible_bell && bell_count > 0 {
        let ok = bell::ring();
        eprintln!(
            "[bell] ts_ms={} audible count={bell_count} backend={}",
            logging::now_ms(),
            if ok { "ok" } else { "unavailable" }
        );
    }
}

/// CoreEvent 唤醒回调（UI 事件循环线程）：只做语义事件，不跑周期任务。
fn on_core_event_wake() {
    CORE_EVENT_WAKES.fetch_add(1, Ordering::Relaxed);
    // 事件来自任意窗口的 pane；必须逐窗口 drain，不能只处理 active window。
    let mut audible = false;
    let mut bells = 0u32;
    runtime::with_all_windows(|_id, st| {
        let (a, b) = drain_core_events(st);
        audible |= a;
        bells = bells.saturating_add(b);
    });
    ring_bell(audible, bells);
}

/// 插件活动 / SFTP 后台结果唤醒回调：立即收结果；周期兜底仍在 400ms timer。
fn on_app_event_wake() {
    APP_EVENT_WAKES.fetch_add(1, Ordering::Relaxed);
    runtime::with_all_windows(|_id, st| {
        // 插件命令/事件在后台线程写活动日志；这里按 revision 增量刷新设置页/
        // 状态栏/结果浮层。所有操作非阻塞。
        refresh_plugin_ui(st);
        // SFTP 传输进度：非阻塞收事件；终态时回收任务线程。
        poll_sftp_transfer(st);
        // SFTP 元数据命令后台结果（list/mkdir/rename/delete）。
        poll_sftp_op(st);
        // SSH known_hosts 后台预检结果（首次/变更提示）。
        poll_hostkey_check(st);
        // Vault 后台 Argon2 结果（设置页在派生期间仍可重绘）。
        poll_vault(st);
    });
}

/// 400ms 兜底/周期轮询：语义事件 + 插件活动 + SFTP 传输/元数据。
///
/// 事件唤醒是低延迟路径，本函数仍是必需的兜底（唤醒丢失、合并窗口、超时与
/// 周期性刷新都靠它），但不会因为事件多就变成高频轮询。
fn poll_events_inner(st: &mut AppState) -> (bool, u32) {
    let bells = drain_core_events(st);
    refresh_plugin_ui(st);
    poll_sftp_transfer(st);
    poll_sftp_op(st);
    poll_hostkey_check(st);
    poll_vault(st);
    bells
}

fn poll_events() {
    PERIODIC_POLLS.fetch_add(1, Ordering::Relaxed);
    let (audible_bell, bell_count) = with_app(poll_events_inner).unwrap_or((false, 0));
    ring_bell(audible_bell, bell_count);
}

/// 第二窗口的 400ms 轮询：显式按 state 路由，不依赖“当前活动窗口”。
fn poll_events_for(state: Rc<RefCell<AppState>>) {
    PERIODIC_POLLS.fetch_add(1, Ordering::Relaxed);
    if std::env::var_os("TERVO_MW_DEBUG").is_some() {
        let id = state.borrow().window_id;
        eprintln!("[poll] {id}");
    }
    let state_for_poll = state.clone();
    let bells = runtime::with_state(&state_for_poll, poll_events_inner).unwrap_or((false, 0));
    ring_bell(bells.0, bells.1);
}

/// 活动 pane 收到 BEL：闪一下边框（`visual_bell = false` 时完全不做）。
///
/// 只更新视图属性，不重新拉帧/不 overlay 终端内容；重绘由 Slint 的
/// border/opacity 动画驱动。
fn flash_bell(st: &mut AppState, id: PaneId) {
    if !st.cfg.visual_bell {
        return;
    }
    let Some(rt) = st.panes.get_mut(&id) else {
        return;
    };
    rt.bell_until = Some(Instant::now() + Duration::from_millis(BELL_FLASH_MS));
    refresh_pane_view(st, id);
    // 已经在跑就重置计时；到期没有新的闪烁时 tick 会 stop。
    let window_id = st.window_id;
    st.bell_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(30),
        move || {
            let _ = runtime::with_window(window_id, tick_bell_flash);
        },
    );
}

/// 清除到期的视觉铃闪烁；全部清完就停掉 30ms timer，空闲时零开销。
fn tick_bell_flash(st: &mut AppState) {
        let now = Instant::now();
        let expired: Vec<PaneId> = st
            .panes
            .iter()
            .filter(|(_, rt)| rt.bell_until.is_some_and(|until| now >= until))
            .map(|(id, _)| *id)
            .collect();
        let still_flashing = st
            .panes
            .values()
            .any(|rt| rt.bell_until.is_some_and(|until| now < until));
        for id in expired {
            let Some(rt) = st.panes.get_mut(&id) else {
                continue;
            };
            rt.bell_until = None;
            refresh_pane_view(st, id);
        }
        if !still_flashing {
            st.bell_timer.stop();
        }
}

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
    /// `--window` 兼容标记。默认会转发给已运行的 primary；没有 primary 时
    /// 作为普通 primary 启动（会清掉此标记）。只有显式 legacy/无单实例模式
    /// 才保留“子窗口进程”语义。
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
            "--scrollback" => {
                o.scrollback = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(o.scrollback)
            }
            "--font-size" => {
                o.font_size = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(o.font_size)
            }
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
            }
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
    let explicit_backend = std::env::var("SLINT_BACKEND").ok();
    let effective_backend = explicit_backend.clone().unwrap_or_else(|| {
        if std::path::Path::new("/dev/dri").exists() {
            "winit-skia".to_string()
        } else {
            "winit-software".to_string()
        }
    });
    println!(
        "requested_backend={effective_backend}{}",
        if explicit_backend.is_some() {
            " (env)"
        } else {
            " (default)"
        }
    );
    println!("/dev/dri={}", std::path::Path::new("/dev/dri").exists());
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
    for m in xconn::monitors() {
        println!(
            "monitor={}x{}+{}+{} primary={}",
            m.width, m.height, m.x, m.y, m.primary
        );
    }
    if let Some((x, y, w, h)) = xconn::workarea() {
        println!("workarea={w}x{h}+{x}+{y}");
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
    let others: Vec<_> = xconn::tervo_windows()
        .into_iter()
        .filter(|w| w.pid != std::process::id())
        .collect();
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

/// 编译期注入的发布信息；`scripts/package.sh` 用固定的 `SOURCE_DATE_EPOCH`
/// 构建以保证同 commit 的产物可复现。
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_COMMIT: &str = env!("TERVO_GIT_COMMIT");
const BUILD_DIRTY: &str = env!("TERVO_GIT_DIRTY");
const BUILD_TIME: &str = env!("TERVO_BUILD_TIME");
const BUILD_TARGET: &str = env!("TERVO_BUILD_TARGET");

/// `--version` 的完整文本；与打印分离，便于单测。
fn version_text() -> String {
    let dirty = if BUILD_DIRTY == "1" { " (dirty)" } else { "" };
    format!(
        "Tervo {BUILD_VERSION}\n\
         commit:  {BUILD_COMMIT}{dirty}\n\
         built:   {BUILD_TIME}\n\
         target:  {BUILD_TARGET}"
    )
}

fn print_version() {
    println!("{}", version_text());
}

/// Settings → Help 展示的本地信息；版本类字段全部来自 build.rs 注入的
/// compile-time 常量，不在 UI 里写死版本号。
struct HelpInfo {
    config_dir: PathBuf,
    log_dir: PathBuf,
    version: &'static str,
    commit: String,
    built: &'static str,
    target: &'static str,
}

fn help_info() -> HelpInfo {
    let config = Config::path();
    let config_dir = config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.clone());
    let log_dir = logging::path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(logging::dir);
    let dirty = if BUILD_DIRTY == "1" { " (dirty)" } else { "" };
    HelpInfo {
        config_dir,
        log_dir,
        version: BUILD_VERSION,
        commit: format!("{BUILD_COMMIT}{dirty}"),
        built: BUILD_TIME,
        target: BUILD_TARGET,
    }
}

/// 下发 Help 页的路径/版本属性并打一条可 grep 的日志。
fn refresh_help_info(st: &mut AppState) {
    let info = help_info();
    st.ui
        .set_help_config_path(info.config_dir.display().to_string().as_str().into());
    st.ui
        .set_help_log_path(info.log_dir.display().to_string().as_str().into());
    st.ui
        .set_help_version(format!("Tervo {}", info.version).as_str().into());
    st.ui.set_help_commit(info.commit.as_str().into());
    st.ui.set_help_built(info.built.into());
    st.ui.set_help_target(info.target.into());
    eprintln!(
        "[help] info version={} commit={} built={} target={} config={} logs={}",
        info.version,
        info.commit,
        info.built,
        info.target,
        info.config_dir.display(),
        info.log_dir.display()
    );
}

/// 复制到剪贴板的诊断文本：路径 + 版本，不包含任何密码/密钥/环境变量。
fn help_diagnostics_text(info: &HelpInfo, lang: &str) -> String {
    format!(
        "Tervo diagnostics\n\
         version: {}\n\
         commit:  {}\n\
         built:   {}\n\
         target:  {}\n\
         config:  {}\n\
         logs:    {}\n\
         language: {lang}",
        info.version,
        info.commit,
        info.built,
        info.target,
        info.config_dir.display(),
        info.log_dir.display()
    )
}

/// Help 页“复制诊断信息”：成功/失败都写回 help-status，不 panic。
fn copy_help_diagnostics(st: &mut AppState) {
    let info = help_info();
    let lang = st.cfg.lang();
    let text = help_diagnostics_text(&info, lang);
    let bytes = text.len();
    match set_clipboard_checked(st, &text) {
        Ok(()) => {
            eprintln!("[help] copy diagnostics ok=true bytes={bytes}");
            st.ui.set_help_status(
                ui_text(
                    lang,
                    "Diagnostics copied to clipboard (no secrets included).",
                    "诊断信息已复制到剪贴板（不含密钥）。",
                )
                .into(),
            );
        },
        Err(e) => {
            eprintln!("[help] copy diagnostics ok=false bytes={bytes} error={e}");
            let msg = format!(
                "{}{e}",
                ui_text(
                    lang,
                    "Could not copy diagnostics: ",
                    "复制诊断信息失败："
                )
            );
            st.ui.set_help_status(msg.as_str().into());
        },
    }
}

/// Help 页目录按钮：校验目录后复用统一的 shell-safe `xdg-open <单参数>` helper。
/// 打开失败（目录不存在/进程非 0/找不到 xdg-open）写回 Help 页可见状态，不 panic。
fn open_help_directory(
    dir: PathBuf,
    kind: &'static str,
    lang: &'static str,
) -> Result<(), String> {
    eprintln!("[help] open {kind} path={}", dir.display());
    if !dir.is_dir() {
        let msg = format!(
            "{}{}",
            ui_text(lang, "Directory does not exist: ", "目录不存在："),
            dir.display()
        );
        eprintln!("[help] open {kind} failed: {msg}");
        return Err(msg);
    }
    let prefix = ui_text(lang, "Could not open directory", "无法打开目录").to_string();
    let observer: XdgObserver = Some(std::sync::Arc::new(move |result: Result<(), String>| {
        match result {
            Ok(()) => eprintln!("[help] open {kind} ok"),
            Err(msg) => {
                eprintln!("[help] open {kind} failed: {msg}");
                let _ = slint::invoke_from_event_loop(move || {
                    runtime::with_active_window(|st| {
                        st.ui.set_help_status(msg.as_str().into());
                    });
                });
            },
        }
    }));
    let shown = dir.display().to_string();
    open_with_xdg_observed(dir.into_os_string(), shown, "path", prefix, observer);
    Ok(())
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
--window            兼容：在已运行的 primary 中新开窗口；无 primary 时正常启动\n  \
--import-tabby PATH 无 UI：导入 Tabby config.yaml（Profile/字号/快捷键/主题）后退出\n  \
--export-tabby PATH 无 UI：导出 Profile 为 Tabby YAML 后退出\n  \
--diagnose          打印环境/窗口/最近日志后退出（排查右键/焦点问题）\n  \
  diag [env|snapshot|replay|trace|metrics|observability|scene-diff]  harness 可调用 JSON 诊断（tervo diag help）\n  \
-V, --version       显示版本、git commit 与构建时间\n  \
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
        }
    };
    let op = args.first().map(String::as_str).unwrap_or("");
    let remote = args.get(1).map(String::as_str).unwrap_or(".");
    let result = match op {
        "list" => sftp::list(&target, remote).map(|items| items.join("\n")),
        "get" => {
            let local = args.get(2).map(String::as_str).unwrap_or("");
            sftp_probe_transfer(&target, sftp::TransferDirection::Download, local, remote)
        }
        "put" => {
            // put <local> <remote>（与 cp 习惯一致；get 则是 <remote> <local>）
            let local = args.get(1).map(String::as_str).unwrap_or("");
            let remote = args.get(2).map(String::as_str).unwrap_or("");
            sftp_probe_transfer(&target, sftp::TransferDirection::Upload, local, remote)
        }
        other => {
            eprintln!("[sftp-probe] 未知操作 {other}");
            return 2;
        }
    };
    match result {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            println!("[sftp-probe] ok {op}");
            0
        }
        Err(e) => {
            eprintln!("[sftp-probe] {e}");
            1
        }
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

/// TERVO_GPU=1 但 WGPU30 backend/adapter/device 初始化失败：exec 重启自己并
/// `TERVO_GPU=0`，走原来的默认 CPU/Skia 路径。保留参数，日志明确写出原因。
/// `TERVO_GPU_FALLBACK=1` 只用于标记这次重启，不再请求 GPU（TERVO_GPU=0 已足够）。
fn exec_cpu_fallback(requested: &str, reason: &str) -> ! {
    use std::os::unix::process::CommandExt;
    eprintln!(
        "[gpu] requested_backend={requested} 的 WGPU 显示链路初始化失败：{reason}；\
         exec 重启并 TERVO_GPU=0 TERVO_GPU_NATIVE=0 自动回退 CPU/Skia（默认路径）"
    );
    logging::restore_stderr();
    std::thread::sleep(Duration::from_millis(50));
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tervo"));
    let err = std::process::Command::new(exe)
        .args(args)
        .env("TERVO_GPU", "0")
        .env("TERVO_GPU_NATIVE", "0")
        .env("TERVO_GPU_FALLBACK", "1")
        .exec();
    eprintln!("[gpu] exec CPU 回退失败: {err}");
    std::process::exit(3);
}

/// Skia 已创建窗口但 1s 内没有拿到 GPU/GL 上下文（software surface 在本机实测
/// 黑屏），用 exec 换成 winit-software 重新启动；保留原参数/环境，且用
/// `TERVO_RENDER_FALLBACK=1` 防止循环。`exec` 前恢复 stderr，避免新进程继承
/// 旧 tee pipe 导致日志全部丢失。
fn exec_software_fallback(requested: &str) -> ! {
    use std::os::unix::process::CommandExt;
    eprintln!(
        "[render] requested_backend={requested} 在 1s 内未确认 GPU/GL 上下文；\
         Skia software surface 在本机会黑屏，exec 自动回退 winit-software"
    );
    logging::restore_stderr();
    // 给 tee 线程一点时间把上面这行写进日志。
    std::thread::sleep(Duration::from_millis(50));
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tervo"));
    let err = std::process::Command::new(exe)
        .args(args)
        .env("SLINT_BACKEND", "winit-software")
        .env("TERVO_GPU_NATIVE", "0")
        .env("TERVO_RENDER_FALLBACK", "1")
        .env_remove("TERVO_TEST_FORCE_UI_INIT_FAIL")
        .env_remove("TERVO_TEST_SUSPEND_BG")
        .exec();
    eprintln!("[render] exec winit-software 回退失败: {err}");
    std::process::exit(3);
}

/// 构造一个窗口自己的模型 + `AppState`。
///
/// 单窗口（WindowId(1)）和第二窗口走同一路径；共享服务只通过句柄注入，
/// 这里不会重新 load Config/PluginStore/Vault/ProfileStore，也不会启动插件进程。
#[allow(clippy::too_many_arguments)]
fn build_window_state(
    ui: &MainWindow,
    shared: Rc<SharedServices>,
    window_id: WindowId,
    opts: &Options,
    fonts: Rc<FontSet>,
    cell: (usize, usize),
    win_w: f32,
    win_h: f32,
    content: Rect,
    initial_theme: ThemeData,
) -> Rc<RefCell<AppState>> {
    ui.window()
        .set_size(slint::PhysicalSize::new(win_w as u32, win_h as u32));

    // 所有 Slint model 都是窗口私有的；两个窗口共享同一个 Rust 模型会串台。
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
    let sftp_queue_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_sftp_queue(ModelRc::from(sftp_queue_model.clone()));
    let hotkey_model = Rc::new(VecModel::<HotkeyRow>::default());
    ui.set_hotkey_rows(ModelRc::from(hotkey_model.clone()));
    let plugin_row_model = Rc::new(VecModel::<PluginRowView>::default());
    ui.set_plugin_rows(ModelRc::from(plugin_row_model.clone()));
    let plugin_activity_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_plugin_activity_items(ModelRc::from(plugin_activity_model.clone()));
    let plugin_surface_model = Rc::new(VecModel::<PluginUiElement>::default());
    ui.set_plugin_surface_elements(ModelRc::from(plugin_surface_model.clone()));
    let settings_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_settings_results(ModelRc::from(settings_model.clone()));
    let fwd_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_fwd_items(ModelRc::from(fwd_model.clone()));
    let profile_menu_model = Rc::new(VecModel::<slint::SharedString>::default());
    ui.set_profilemenu_items(ModelRc::from(profile_menu_model.clone()));
    // 侧栏 Profiles 列表复用同一份模型（"+"/Ctrl+T 下拉与侧栏永远一致）。
    ui.set_sidebar_items(ModelRc::from(profile_menu_model.clone()));

    let (app_wake, app_wake_rx) = WakeSignal::new();
    if event_wake_enabled() {
        spawn_app_wake_bridge(app_wake_rx);
    } else {
        drop(app_wake_rx);
    }
    let command_registry = commands::builtin();
    // 圆角 worker 只依赖 X11；多个窗口各自持有自己的 SHAPE worker（默认关闭）。
    let corners = if std::env::var_os("TERVO_CORNERS").is_none()
        || std::env::var_os("TERVO_NO_CORNERS").is_some()
    {
        None
    } else {
        xconn::CornerShaper::spawn(&opts.title).ok()
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
        window_id,
        shared: shared.clone(),
        content,
        next_title: 0,
        pending_close_window: None,
        resolved_theme: initial_theme.clone(),
        registry: shared.themes.clone_handle(),
        auto_watch_started: false,
        search_open: false,
        search_query: String::new(),
        search_match_case: false,
        search_regex: false,
        save_pane: None,
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
        profiles: shared.profiles.clone_handle(),
        profile_model,
        profile_indices: Vec::new(),
        profile_query: String::new(),
        vault: shared.vault.clone_handle(),
        vault_job: None,
        vault_action: None,
        vault_busy: false,
        vault_clear_armed: false,
        hostkey_job: None,
        hostkey_pending: None,
        hostkey_prompt: None,
        hostkey_verified: std::collections::HashSet::new(),
        ssh_exit: None,
        sftp_profiles: Vec::new(),
        sftp_model,
        hotkey_model,
        hotkey_ids: Vec::new(),
        hotkey_recording: None,
        held_keys: HashMap::new(),
        plugins: shared.plugins.clone_handle(),
        app_wake: app_wake.clone(),
        plugin_row_model,
        plugin_activity_model,
        plugin_surface_model,
        plugin_activity_revision: 0,
        plugin_status_revision: 0,
        plugin_runtime_revision: 0,
        plugin_status_command: None,
        plugin_confirm: None,
        plugin_surface: None,
        plugin_approval: None,
        last_plugin_focus: None,
        sftp_delete_armed: false,
        sftp_transfer: None,
        sftp_current_request: None,
        sftp_queue: sftp::TransferQueue::new(),
        sftp_queue_model,
        sftp_last_failed: None,
        sftp_transfer_state: sftp::TransferState::idle(),
        sftp_op: None,
        sftp_op_timer: slint::Timer::default(),
        quake_visible: false,
        quake_saved: None,
        profile_menu_indices: Vec::new(),
        profile_menu_model,
        right_down: None,
        menu_link: String::new(),
        broadcast: false,
        last_click: None,
        closed_tabs: Vec::new(),
        fwd_profile_indices: Vec::new(),
        fwd_model,
        settings_result_cats: Vec::new(),
        settings_result_sections: Vec::new(),
        settings_jump_token: 0,
        settings_jump_timer: slint::Timer::default(),
        settings_model,
        hover_cell: None,
        link_hover_cell: None,
        resize: None,
        window_focused: true,
        render_hud_timer: None,
        window_maximized: false,
        maximize: None,
        ime_spot: None,
        ime_spot_dirty: true,
        ime_override: None,
        wm_resize: false,
        wm_release_pending: false,
        xresize: None,
        resize_events: 0,
        resize_applied: 0,
        resize_pending: None,
        resize_apply_scheduled: false,
        resize_apply: slint::Timer::default(),
        last_resize_event: None,
        wm_resize_saw_configure: false,
        resize_redraw_scheduled: false,
        resize_redraw: slint::Timer::default(),
        resize_finalize: slint::Timer::default(),
        bell_timer: slint::Timer::default(),
        cursor_blink_on: true,
        cursor_blink_timer: slint::Timer::default(),
        drag_autoscroll: slint::Timer::default(),
        resize_in_progress: false,
        resize_max_apply_us: 0.0,
        corners,
        clipboard: arboard::Clipboard::new().ok(),
        cfg: shared.cfg.clone_handle(),
    }));
    state
}

/// 单窗口/第二窗口共用的 UI 初始状态同步（只改 Slint 属性，不写盘）。
fn init_window_ui(st: &mut AppState) {

    st.ui.set_cfg_copy_on_select(st.cfg.copy_on_select);
    st.ui.set_cfg_confirm_close(st.cfg.confirm_close);
    st.ui
        .set_cfg_restore_session(st.cfg.restore_session_on_startup);
    st.ui.set_cfg_confirm_paste(st.cfg.confirm_multiline_paste);
    st.ui
        .set_cfg_desktop_notifications(st.cfg.desktop_notifications);
    st.ui.set_cfg_visual_bell(st.cfg.visual_bell);
    // 光标/粗体外观（GNOME Terminal 基础项）
    st.ui
        .set_cfg_cursor_shape(st.cfg.cursor_shape.clone().into());
    st.ui
        .set_cfg_cursor_blink(st.cfg.cursor_blink.clone().into());
    st.ui
        .set_cfg_cursor_color(st.cfg.cursor_color.clone().into());
    st.ui
        .set_cfg_cursor_text_color(st.cfg.cursor_text_color.clone().into());
    st.ui.set_cfg_bold_is_bright(st.cfg.bold_is_bright);
    st.ui.set_cfg_bold_color(st.cfg.bold_color.clone().into());
    st.ui
        .set_cfg_bold_color_same_as_fg(st.cfg.bold_color_same_as_fg);
    configure_cursor_blink(st);
    st.ui.set_cfg_audible_bell(st.cfg.audible_bell);
    st.ui.set_cfg_scroll_output(st.cfg.scroll_on_output);
    st.ui.set_cfg_scroll_keystroke(st.cfg.scroll_on_keystroke);
    st.ui.set_cfg_sixel(st.cfg.sixel);
    st.ui.set_cfg_sixel_scrolling(st.cfg.sixel_scrolling);
    st.ui.set_cfg_iip(st.cfg.iip);
    st.ui
        .set_cfg_plugin_require_signature(st.cfg.plugin_require_signature);
    st.ui
        .set_cfg_scrollback_unlimited(st.cfg.scrollback_unlimited);
    st.ui
        .set_cfg_scrollbar_policy(st.cfg.scrollbar_policy().as_str().into());
    st.ui
        .set_cfg_backspace_binding(st.cfg.key_bindings().backspace.as_str().into());
    st.ui
        .set_cfg_delete_binding(st.cfg.key_bindings().delete.as_str().into());
    st.ui.set_tabbar_bottom(st.cfg.tabbar_position == "bottom");
    st.ui
        .set_settings_path(Config::path().display().to_string().as_str().into());
    refresh_help_info(st);
    init_background(st);
    // 测试/对比用：启动即隐藏背景，测量 cover 缩放成本（不影响正常路径）。
    if std::env::var_os("TERVO_TEST_SUSPEND_BG").is_some() {
        st.ui.set_background_suspended(true);
    }
    st.ui.set_language(st.cfg.lang().into());
    refresh_hotkeys(st);
    refresh_vault_ui(st);
    sync_plugin_config_metadata(st);
    refresh_plugin_page(st);
    refresh_settings_results(st);
    sync_mcp_ui(st);
    eprintln!(
        "[plugin-v3] 设置页 {} 个插件，{} 个启用，{} 条启用命令，{} unsupported/invalid",
        st.plugins.total_count(),
        st.plugins.enabled_count(),
        st.plugins.enabled_command_count(),
        st.plugins.unsupported_count()
    );
    eprintln!("[ui] 启动 tabbar_position={}", st.cfg.tabbar_position);}

/// primary 与 secondary 共用的完整窗口回调注册。
///
/// 闭包内所有 `AppState` 访问都走 `runtime::with_window(window_id, ...)`，显式携带
/// 窗口身份；不再调用无作用域的 `with_app` 去猜窗口。这样两个窗口各自注册同一套
/// 键盘/IME/鼠标/菜单/设置页/插件页/profile/SFTP 回调，而配置/插件/Vault/MCP
/// 仍来自同一个 [`SharedServices`]。
fn register_window_callbacks(
    ui: &MainWindow,
    state: Rc<RefCell<AppState>>,
    window_id: WindowId,
) {
    eprintln!("[ui] {window_id} register window callbacks");

    // ---- 回调 ----
    let keylog = std::env::var("TERVO_KEYLOG").is_ok();
    // 鼠标链路排查：TERVO_MOUSELOG=1 打印「命中哪格 / 是否上报」。
    let mouselog = std::env::var("TERVO_MOUSELOG").is_ok();
    ui.on_key_input(move |text, ctrl, alt, shift, meta, repeat| {
        let text = text.to_string();
        if keylog {
            eprintln!(
                "[key] {:?} ctrl={ctrl} alt={alt} shift={shift} meta={meta} repeat={repeat}",
                text
            );
        }
        let kind = if repeat {
            keys::KeyEventKind::Repeat
        } else {
            keys::KeyEventKind::Press
        };
        runtime::with_window(window_id, |st| handle_input(st, &text, ctrl, alt, shift, meta, kind, keylog));
    });

    // kitty release：只有应用请求 REPORT_EVENT_TYPES 且 press 进过 PTY 才回放。
    ui.on_key_release(move |text, ctrl, alt, shift, meta| {
        let text = text.to_string();
        if keylog {
            eprintln!(
                "[key] release {:?} ctrl={ctrl} alt={alt} shift={shift} meta={meta}",
                text
            );
        }
        runtime::with_window(window_id, |st| handle_key_release(st, &text, ctrl, alt, shift, meta, keylog));
    });

    // IME 上屏（中文/emoji）单独走一条路：没有修饰键语义，也不能触发 Ctrl 快捷键。
    ui.on_ime_commit(move |text| {
        let text = text.to_string();
        if keylog {
            eprintln!("[ime] {:?}", text);
        }
        runtime::with_window(window_id, |st| {
            handle_input(
                st,
                &text,
                false,
                false,
                false,
                false,
                keys::KeyEventKind::Press,
                keylog,
            )
        });
    });

    ui.on_pane_mouse_down(move |id, x, y, button, mods| {
        let id = id as u64;
        runtime::with_window(window_id, |st| {
            // 点击终端属于“find 之外”：先收起查找条，再按正常点击处理。
            if st.search_open {
                close_search(st);
            }
            let (cell, wants_mouse) = {
                let Some(rt) = st.panes.get(&id) else { return };
                (cell_at(rt, x, y), rt.handle.wants_mouse())
            };
            let modifiers = mouse_mods(mods);
            // Shift 是「绕过鼠标上报、改成本地选词」的惯例逃生门（xterm/gnome-terminal 同款）。
            let reporting = wants_mouse && !modifiers.shift;
            if ime_click_anchor(button, wants_mouse, modifiers.shift) {
                // dsh 的 Btw 侧栏 composer 只画反色 caret，不移动硬件光标；
                // 记住点击格，IME 候选才会贴到右侧输入框。
                st.ime_override = Some((id, cell));
                // 每次点击都强制重发：输入法可能已丢弃旧 spot（例如刚从别的窗口回来）。
                st.ime_spot_dirty = true;
                update_ime_spot(st);
            }
            let Some(rt) = st.panes.get(&id) else { return };
            if mouselog {
                eprintln!(
                    "[mouse] down pane={id} cell={cell:?} button={button} mods={mods} \
                     wants={} reporting={reporting}",
                    rt.handle.wants_mouse()
                );
            }
            // Ctrl+左键只在“非鼠标上报”时是打开链接；上报模式仍把 Ctrl 位传给 TUI。
            let link = !reporting && modifiers.ctrl;
            eprintln!(
                "[select] down pane={id} cell={cell:?} button={button} shift={} \
                 wants_mouse={wants_mouse} reporting={reporting} link={link}",
                modifiers.shift
            );
            // 上一次拖拽可能因窗口失焦/丢 release 残留；新的按下必须重置计时器，
            // 否则旧 drag 的 50ms tick 会继续滚动/扩选到新拖拽上。
            st.drag_autoscroll.stop();
            if let Some(old) = st.drag.take() {
                if !old.mouse && !old.link {
                    eprintln!(
                        "[select] drag-cancel pane={} reason=new-press moved={}",
                        old.pane, old.moved
                    );
                }
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
                start_drag_autoscroll_timer(st);
            }
        });
    });

    ui.on_pane_mouse_move(move |id, x, y, _button, mods| {
        let id = id as u64;
        runtime::with_window(window_id, |st| {
            let modifiers = mouse_mods(mods);

            if st.drag.is_some() {
                let (cell, forwarding, link, resume, changed) = {
                    let Some(drag) = st.drag.as_mut() else { return };
                    if drag.pane != id {
                        return;
                    }
                    drag.last_pointer = (x, y);
                    if (x - drag.start.0).abs() > 4.0 || (y - drag.start.1).abs() > 4.0 {
                        drag.moved = true;
                    }
                    let Some(rt) = st.panes.get(&id) else { return };
                    let inside = !pointer_outside_window(
                        rt.rect.x,
                        rt.rect.y,
                        x,
                        y,
                        st.content.w,
                        st.content.h,
                    );
                    // 拖出窗口会暂停计时器；指针回到窗口内（即使还在同一格）
                    // 必须恢复，否则之后的边缘拖动再也不会自动滚动。仍在外面的
                    // move 不能恢复，否则每次移动都会空转一次 tick。
                    let resume = drag.autoscroll_paused
                        && inside
                        && selection_drag_autoscrolls(drag.mouse, drag.link, drag.moved);
                    if resume {
                        drag.autoscroll_paused = false;
                    }
                    let cell = cell_at(rt, x, y);
                    let changed = drag.cell != cell;
                    if changed {
                        drag.cell = cell;
                    }
                    (cell, drag.mouse, drag.link, resume, changed)
                };
                if resume {
                    start_drag_autoscroll_timer(st);
                    eprintln!(
                        "[select] autoscroll-resume pane={id} reason=pointer-reentered-window"
                    );
                }
                if link {
                    // Ctrl 拖拽不扩选；抬起时是否打开链接由 moved 决定。
                    return;
                }
                if !changed {
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

    // 链接 hover：只改指针/悬停状态，不拉帧不重绘终端。Rust 侧按格去重。
    ui.on_pane_hover(move |id, x, y| {
        let id = id as u64;
        runtime::with_window(window_id, |st| update_link_hover(st, id, x, y));
    });
    ui.on_pane_unhover(move |id| {
        let id = id as u64;
        runtime::with_window(window_id, |st| clear_link_hover(st, id));
    });

    ui.on_pane_mouse_up(move |id, x, y, button, mods| {
        let id = id as u64;
        runtime::with_window(window_id, |st| {
            // 先停表再取 drag：即使 drag 已被失焦/新按下清掉，也不能留下
            // 一个仍在 50ms tick 的计时器。
            st.drag_autoscroll.stop();
            let Some(drag) = st.drag.take() else { return };
            if !drag.mouse && !drag.link {
                eprintln!(
                    "[select] drag-end pane={id} reason=release moved={} paused={}",
                    drag.moved, drag.autoscroll_paused
                );
            }
            if drag.link {
                // Ctrl+点击（没有明显拖动）才打开；拖动过就当作取消。
                if !drag.moved && button == 0 {
                    let (target, remote, cwd, cell) = {
                        let Some(rt) = st.panes.get(&id) else { return };
                        let cell = cell_at(rt, x, y);
                        (
                            frame_target_at(&rt.frame, cell.0, cell.1),
                            rt.remote,
                            pane_cwd(rt),
                            cell,
                        )
                    };
                    match target {
                        Some(target) => {
                            eprintln!(
                                "[link] ctrl+click pane={id} cell={},{} kind={} text={:?}",
                                cell.0,
                                cell.1,
                                target.kind_label(),
                                target.text()
                            );
                            open_target(&target, cwd.as_deref(), remote);
                        },
                        None => {
                            if mouselog {
                                eprintln!(
                                    "[link] ctrl+click pane={id} cell={},{} 无链接",
                                    cell.0, cell.1
                                );
                            }
                        },
                    }
                }
                return;
            }
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
            if !drag.mouse {
                let up_cell = st.panes.get(&id).map(|rt| cell_at(rt, x, y));
                eprintln!(
                    "[select] up pane={id} cell={up_cell:?} moved={} copy_on_select={}",
                    drag.moved,
                    st.cfg.copy_on_select
                );
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
                set_selection_clipboards(st, &text);
            }
        });
    });

    ui.on_pane_scrolled(move |id, dy, x, y, mods| {
        let id = id as u64;
        runtime::with_window(window_id, |st| {
            let Some(rt) = st.panes.get(&id) else { return };
            let modifiers = mouse_mods(mods);

            // 终端要鼠标：滚轮变成 64/65 上报（tmux 翻页、vim 滚动才生效）。
            if rt.handle.wants_mouse() && !modifiers.shift {
                let cell = cell_at(rt, x, y);
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
            if lines == 0.0 {
                return;
            }
            // 约定：delta-y > 0 表示向上滚（回看历史）。
            // 应用内 TUI（如 dsh 侧栏）滚动后屏幕内容会重画；原生选区若继续
            // 跟随 grid 行会粘在错误文本上，因此用户主动滚轮时清除选区。
            rt.handle.clear_selection();
            rt.handle.scroll(lines as i32);
        });
    });

    ui.on_pane_scrollbar(move |id, fraction| {
        let id = id as u64;
        runtime::with_window(window_id, |st| {
            let Some(rt) = st.panes.get(&id) else { return };
            let pos = rt.handle.scroll_position();
            let track_px = rt.renderer.height.max(1) as f32;
            let min_thumb = (SCROLLBAR_MIN_THUMB_PX / track_px).clamp(0.0, 1.0);
            let offset = pos.offset_for_track_fraction(fraction, min_thumb);
            eprintln!(
                "[ui] scrollbar pane={id} fraction={fraction:.3} offset={offset} history={} rows={}",
                pos.history, pos.rows
            );
            rt.handle.clear_selection();
            rt.handle.scroll_to(offset);
        });
    });

    // 拖放只接受文件/目录；其他 payload（纯文本/图片）不抢，交回平台。
    ui.on_can_drop_files(|data| data.has_file_paths());
    ui.on_files_dropped(|id, data| {
        let paths: Vec<PathBuf> = match data.file_paths() {
            Ok(paths) => paths.map(|p| p.to_path_buf()).collect(),
            Err(e) => {
                eprintln!("[ui] drop 无法读取文件列表: {e:?}");
                return;
            }
        };
        if paths.is_empty() {
            return;
        }
        drop_files_into_pane(id as u64, &paths);
    });

    // 侧栏右边缘拖宽：只改逻辑宽度，再让内容区让位并重排 pane。
    ui.on_sidebar_dragged(move |x| {
        runtime::with_window(window_id, |st| {
            if !st.cfg.sidebar_visible {
                return;
            }
            let want = crate::config::clamp_sidebar_width(x as f32);
            if (want - sidebar_width_of(&st.cfg)).abs() < 1.0 {
                return;
            }
            st.cfg.sidebar_width = want;
            st.ui.set_sidebar_width(want);
            // 用当前内容区反推窗口逻辑宽度，避免再走一遍 DPI 换算。
            let old_sw = sidebar_width_of(&st.cfg);
            let win_w = st.content.w + old_sw;
            let win_h = st.content.h + TABBAR_H;
            st.content = content_rect(win_w, win_h, want);
            refresh_views(st);
            redraw_visible(st);
        });
    });
    ui.on_sidebar_drag_finished(move || {
        runtime::with_window(window_id, |st| {
            if st.cfg.sidebar_visible {
                st.cfg.save();
            }
        });
    });

    ui.on_divider_dragged(move |index, x, y| {
        runtime::with_window(window_id, |st| {
            let Some(info) = st.divider_infos.get(index as usize).cloned() else {
                return;
            };
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
                let Some(tab) = st.ws.active_tab_mut() else {
                    return;
                };
                let Some(root) = tab.root.as_mut() else {
                    return;
                };
                root.set_ratio(&info.path, ratio)
            };
            if changed {
                refresh_views(st);
                redraw_visible(st);
            }
        });
    });

    ui.on_start_window_resize(move |dir| {
        runtime::with_window(window_id, |st| {
            ensure_resize_conn(st);
            st.wm_resize = false;
            st.wm_release_pending = false;
            st.wm_resize_saw_configure = false;
            st.last_resize_event = None;
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

    ui.on_window_resize_drag(move || {
        runtime::with_window(window_id, |st| {
            // 松开左键后的 move 不能继续 resize：WM grab 可能让 Slint 收不到 up。
            if st.wm_resize || st.resize.is_some() {
                if let Some(ctx) = st.xresize.as_ref() {
                    if let Some((_, _, mask)) =
                        crate::xconn::query_pointer_with_mask(&ctx.conn, ctx.root)
                    {
                        if !crate::xconn::button1_down(mask) {
                            finish_window_resize(st, "pointer-up");
                            return;
                        }
                    }
                }
            }
            if st.wm_resize {
                let Some(r) = st.resize else { return };
                let now = st.ui.window().size();
                let wm_acted = (now.width, now.height) != r.size;
                // WM 可能仍在驱动 live resize，也可能只是比我们晚一帧。判断依据：
                // 真实尺寸变化、最近一次 configure、或按下后还没超过首个 configure
                // 的宽限；三者都没有才回退自绘。不能只看按下后是否过了 150ms——
                // GPU 首帧/渲染排队会让 configure 晚到。
                if wm_acted
                    || !wm_resize_timed_out(
                        st.wm_resize_saw_configure,
                        st.last_resize_event.map(|t| t.elapsed()),
                        r.since.elapsed(),
                    )
                {
                    return;
                }
                eprintln!("[ui] WM live resize 未接管指针，回退自绘 resize");
                st.wm_resize = false;
            }
            let Some(r) = st.resize else { return };
            let Some((px, py)) = resize_pointer(st) else {
                return;
            };
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

    ui.on_end_window_resize(move || {
        runtime::with_window(window_id, |st| finish_window_resize(st, "callback"));
    });

    ui.on_input_trace(move |kind, id, x, y| {
        runtime::with_window(window_id, |st| {
            eprintln!(
                "[trace] {kind} pane={id} at {x:.0},{y:.0} win_pos={:?} win_size={:?} content={:.0}x{:.0}",
                st.ui.window().position(),
                st.ui.window().size(),
                st.content.w,
                st.content.h
            );
        });
    });

    ui.on_pane_clicked(move |id| {
        runtime::with_window(window_id, |st| focus_pane_inner(st, id.max(0) as u64));
    });

    // 灯箱只接收 id，从 UI 缓存里取原始 RGBA。先设 source 再 open，避免
    // 第一次点击时 source 还是 0×0，出现灰面板/无图；第二次才显示。
    ui.on_image_open(move |pane_id, image_id| {
        runtime::with_window(window_id, |st| {
            let id = image_id as u64;
            let image = {
                let Some(rt) = st.panes.get_mut(&(pane_id as u64)) else { return };
                let source_id = rt.inline_hits.get(&id).map(|source| source.source_id);
                match source_id {
                    Some(source_id) => {
                        let mut image = cached_image_source(rt, source_id);
                        if image.is_none() {
                            image = rt.handle.image_rgba(source_id).map(|(w, h, rgba)| {
                                let image = image_from_source(w, h, &rgba);
                                rt.inline_cache.insert(source_id, image.clone());
                                image
                            });
                        }
                        match image {
                            Some(image) => image,
                            None => {
                                eprintln!(
                                    "[image] preview source missing placement={id} source={source_id}"
                                );
                                return;
                            },
                        }
                    },
                    None => {
                        // 兼容旧调用方传 source ID 的路径。
                        let mut image = cached_image_source(rt, id);
                        if image.is_none() {
                            image = rt.handle.image_rgba(id).map(|(w, h, rgba)| {
                                let image = image_from_source(w, h, &rgba);
                                rt.inline_cache.insert(id, image.clone());
                                image
                            });
                        }
                        match image {
                            Some(image) => image,
                            None => {
                                eprintln!("[image] preview source missing id={image_id}");
                                return;
                            },
                        }
                    },
                }
            };
            st.ui.set_image_preview_open(false);
            st.ui.set_image_preview_source(image);
            st.ui.set_image_preview_fit(true);
            st.ui.set_image_preview_zoom(1.0);
            st.ui.set_image_preview_open(true);
            eprintln!("[image] preview open id={image_id}");
        });
    });

    ui.on_menu_dismissed(move || {
        eprintln!("[ui] menu dismissed");
        // 右键菜单/遮罩会抢走 IME 焦点；关闭后必须还给终端，否则 TUI 无法输入。
        runtime::with_window(window_id, |st| st.ui.invoke_focus_terminal());
    });

    ui.on_pane_right_clicked(move |id, x, y| {
        runtime::with_window(window_id, |st| {
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
    ui.on_pane_right_up(move |id, _x, _y| {
        runtime::with_window(window_id, |st| {
            let id = id as u64;
            let Some((down_id, at, dx, dy)) = st.right_down.take() else {
                return;
            };
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
                }
                "clipboard" => {
                    if short {
                        copy_or_paste(st);
                    } else {
                        open_pane_menu(st, id, dx, dy);
                    }
                }
                _ => {}
            }
            eprintln!("[ui] right-up pane={id} mode={mode} short={short}");
        });
    });

    ui.on_pane_middle_click(move |_id| {
        runtime::with_window(window_id, |st| {
            if st.cfg.paste_on_middle_click {
                paste_primary_into_focused(st);
                eprintln!("[ui] middle-click paste");
            }
        });
    });

    // 分隔线/边缘发起的右键：按内容区坐标找最近的 pane。
    ui.on_pane_right_clicked_at(move |x, y| {
        runtime::with_window(window_id, |st| {
            let rects: Vec<(PaneId, Rect)> = {
                let Some(tab) = st.ws.active_tab() else {
                    return;
                };
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

    ui.on_menu_action(move |id, action| {
        eprintln!("[ui] menu action pane={id} action={action}");
        let id = id as u64;
        match action {
            0 => {
                runtime::with_window(window_id, |st| split_pane_inner(st, id, Dir::Row));
            },
            1 => {
                runtime::with_window(window_id, |st| split_pane_inner(st, id, Dir::Col));
            },
            2 => {
                runtime::with_window(window_id, |st| close_pane_inner(st, id));
            },
            3 => {
                runtime::with_window(window_id, copy_current_selection);
            }
            4 => {
                runtime::with_window(window_id, paste_into_focused);
            }
            5 => {
                runtime::with_window(window_id, |st| run_command(st, "terminal.find"));
            }
            6 => {
                runtime::with_window(window_id, |st| run_command(st, "terminal.select-all"));
            }
            7 => {
                runtime::with_window(window_id, |st| run_command(st, "terminal.clear"));
            }
            8 => {
                runtime::with_window(window_id, |st| {
                    st.broadcast = !st.broadcast;
                    st.ui.set_broadcast(st.broadcast);
                    let panes = st.ws.active_tab().map(|t| t.panes().len()).unwrap_or(0);
                    if st.broadcast && panes < 2 {
                        eprintln!("[ui] broadcast=on，但当前 Tab 只有 1 个 pane");
                    } else {
                        eprintln!("[ui] broadcast={} panes={panes}", st.broadcast);
                    }
                });
            }
            9 => {
                runtime::with_window(window_id, |st| {
                    let text = st.menu_link.clone();
                    if text.is_empty() {
                        eprintln!("[link] Open Link：当前菜单没有链接");
                        return;
                    }
                    // 菜单只保存展示文本；path 不会与 http(s)/file URI 混淆。
                    let target = if is_allowed_uri(&text) {
                        LinkTarget::Uri(text)
                    } else {
                        LinkTarget::Path(text)
                    };
                    let (cwd, remote) = match st.panes.get(&id) {
                        Some(rt) => (pane_cwd(rt), rt.remote),
                        None => (None, false),
                    };
                    eprintln!(
                        "[link] menu open kind={} text={:?}",
                        target.kind_label(),
                        target.text()
                    );
                    open_target(&target, cwd.as_deref(), remote);
                });
            }
            10 => {
                runtime::with_window(window_id, |st| {
                    let text = st.menu_link.clone();
                    if !text.is_empty() {
                        eprintln!("[link] copy link={text:?}");
                        set_clipboard(st, &text);
                    }
                });
            }
            11 => {
                runtime::with_window(window_id, |st| toggle_read_only(st, Some(id)));
            }
            12 => {
                runtime::with_window(window_id, |st| reset_pane(st, id, false));
            }
            13 => {
                runtime::with_window(window_id, |st| reset_pane(st, id, true));
            }
            14 => {
                runtime::with_window(window_id, |st| open_save_contents(st, id));
            }
            _ => {}
        }
        // Save Contents 需要把焦点交给输入路径的 TextInput，不能抢回终端。
        if action != 14 {
            runtime::with_window(window_id, |st| st.ui.invoke_focus_terminal());
        }
    });

    ui.on_plugin_status_clicked(move || {
        runtime::with_window(window_id, |st| {
            if let Some(command) = st.plugin_status_command.clone() {
                eprintln!("[plugin-v3] 状态栏点击 → {command}");
                run_command(st, &command);
            }
        });
    });

    // ---- Plugin API v3：所有 UI 由宿主渲染 ----
    ui.on_plugin_action(move |action, key| {
        runtime::with_window(window_id, |st| plugin_action(st, action.as_str(), key.as_str()));
    });
    ui.on_toggle_plugin_require_signature(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.plugin_require_signature = !st.cfg.plugin_require_signature;
            st.ui
                .set_cfg_plugin_require_signature(st.cfg.plugin_require_signature);
            st.cfg.save();
            eprintln!(
                "[plugin-v3] plugin_require_signature={}",
                st.cfg.plugin_require_signature
            );
        });
    });
    let plugin_picker_weak = ui.as_weak();
    ui.on_plugin_pick_install(move |kind| {
        let Some(current_ui) = plugin_picker_weak.upgrade() else { return };
        let initial = current_ui.get_plugin_install_path().to_string();
        let kind = kind.to_string();
        let weak = plugin_picker_weak.clone();
        std::thread::Builder::new()
            .name("plugin-install-picker".into())
            .spawn(move || {
                let result = pick_plugin_source(&kind, &initial);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(_ui) = weak.upgrade() else { return };
                    runtime::with_window(window_id, |st| match result {
                        Ok(Some(path)) => {
                            eprintln!("[plugin-v3] 安装源选择: {path}");
                            st.ui.set_plugin_install_path(path.as_str().into());
                        },
                        Ok(None) => {},
                        Err(error) => {
                            let message = if st.cfg.lang() == "zh" {
                                format!("文件选择器不可用: {error}")
                            } else {
                                format!("File picker unavailable: {error}")
                            };
                            set_status_toast(st, message);
                        },
                    });
                });
            })
            .ok();
    });
    ui.on_plugin_install_apply(move |action, key, path| {
        runtime::with_window(window_id, |st| {
            plugin_install_action(st, action.as_str(), key.as_str(), path.as_str())
        });
    });
    ui.on_plugin_confirm_enable(move || {
        runtime::with_window(window_id, confirm_plugin_enable);
    });
    ui.on_plugin_confirm_cancel(move || {
        runtime::with_window(window_id, cancel_plugin_enable);
    });
    ui.on_plugin_open_surface(move |key, kind| {
        runtime::with_window(window_id, |st| open_plugin_surface(st, key.as_str(), kind.as_str()));
    });
    ui.on_plugin_surface_close(move || {
        runtime::with_window(window_id, close_plugin_surface);
    });
    ui.on_plugin_element_action(move |element| {
        runtime::with_window(window_id, |st| plugin_surface_action(st, element.as_str()));
    });
    ui.on_plugin_element_input(move |element, value| {
        runtime::with_window(window_id, |st| plugin_surface_input(st, element.as_str(), value.to_string()));
    });
    ui.on_plugin_approval_allow(move || {
        runtime::with_window(window_id, |st| resolve_plugin_approval(st, true));
    });
    ui.on_plugin_approval_deny(move || {
        runtime::with_window(window_id, |st| resolve_plugin_approval(st, false));
    });

    ui.on_tab_rename(move |index| {
        runtime::with_window(window_id, |st| open_rename(st, index.max(0) as usize));
    });

    ui.on_rename_committed(move |index, text| {
        runtime::with_window(window_id, |st| {
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

    ui.on_rename_cancelled(move || {
        runtime::with_window(window_id, |st| st.ui.set_rename_open(false));
    });

    ui.on_fwd_select(move |index| {
        runtime::with_window(window_id, |st| {
            st.ui.set_fwd_profile(index.max(0));
            refresh_forwards(st);
        });
    });

    ui.on_fwd_add(move || {
        runtime::with_window(window_id, |st| fwd_add(st));
    });

    ui.on_fwd_delete(move |index| {
        runtime::with_window(window_id, |st| fwd_delete(st, index.max(0) as usize));
    });

    ui.on_fwd_close(|| {});

    ui.on_config_import_tabby(move || {
        runtime::with_window(window_id, |st| {
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
                            *st.registry = ThemeRegistry::load(&themes_dir);
                            for err in &st.registry.errors {
                                eprintln!("[theme] {err}");
                            }
                            st.ui.set_theme_choices(theme_choices(&st.registry));
                        }
                        let lang = st.cfg.lang();
                        let mut msg = if lang == "zh" {
                            format!(
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
                            )
                        } else {
                            format!(
                                "Imported {} profiles, {} shortcuts, {} themes from {} \
                                 (profiles skipped {}, hotkeys unmapped {} / skipped {}, themes skipped {})",
                                report.added,
                                report.hotkeys_added,
                                report.themes_added,
                                path.display(),
                                report.skipped.len(),
                                report.hotkeys_unmapped.len(),
                                report.hotkeys_skipped.len(),
                                report.themes_skipped.len()
                            )
                        };
                        let first_skip = report
                            .skipped
                            .first()
                            .map(|(name, why)| format!("{name} ({why})"))
                            .or_else(|| {
                                report
                                    .hotkeys_skipped
                                    .first()
                                    .map(|(id, why)| format!("{id} ({why})"))
                            })
                            .or_else(|| {
                                report
                                    .themes_skipped
                                    .first()
                                    .map(|(name, why)| format!("{name} ({why})"))
                            });
                        if let Some(skip) = first_skip {
                            if lang == "zh" {
                                msg.push_str(&format!("；首个跳过：{skip}"));
                            } else {
                                msg.push_str(&format!("; first skipped: {skip}"));
                            }
                        }
                        eprintln!("[tabby] {msg}");
                        msg
                    }
                    Err(e) => {
                        if st.cfg.lang() == "zh" {
                            format!("导入失败: {e}")
                        } else {
                            format!("Import failed: {e}")
                        }
                    },
                },
                Err(e) => {
                    if st.cfg.lang() == "zh" {
                        format!("读不到 {}: {e}", path.display())
                    } else {
                        format!("Cannot read {}: {e}", path.display())
                    }
                }
            };
            st.ui.set_config_status(status.as_str().into());
        });
    });

    ui.on_config_export_tabby(move || {
        runtime::with_window(window_id, |st| {
            let path = Config::path().with_file_name("tabby-profiles.yaml");
            let yaml = tabby_import::export_yaml(&st.profiles);
            let status = match std::fs::write(&path, yaml) {
                Ok(()) => {
                    let msg = if st.cfg.lang() == "zh" {
                        format!(
                            "已导出 {} 个 Profile → {}",
                            st.profiles.profiles.len(),
                            path.display()
                        )
                    } else {
                        format!(
                            "Exported {} profiles → {}",
                            st.profiles.profiles.len(),
                            path.display()
                        )
                    };
                    eprintln!("[tabby] {msg}");
                    msg
                }
                Err(e) => {
                    if st.cfg.lang() == "zh" {
                        format!("导出失败: {e}")
                    } else {
                        format!("Export failed: {e}")
                    }
                }
            };
            st.ui.set_config_status(status.as_str().into());
        });
    });

    ui.on_sftp_list(move || {
        runtime::with_window(window_id, |st| sftp_do_list(st));
    });

    ui.on_sftp_pick(move |index| {
        runtime::with_window(window_id, |st| {
            st.ui.set_sftp_selected(index.max(0));
            st.sftp_delete_armed = false;
        });
    });

    ui.on_sftp_download(move || {
        runtime::with_window(window_id, |st| sftp_do_download(st));
    });

    ui.on_sftp_upload(move || {
        runtime::with_window(window_id, |st| sftp_do_upload(st));
    });

    ui.on_sftp_cancel(move || {
        runtime::with_window(window_id, |st| sftp_cancel_active(st));
    });

    ui.on_sftp_chmod(move || {
        runtime::with_window(window_id, |st| sftp_do_chmod(st));
    });

    ui.on_sftp_retry(move || {
        runtime::with_window(window_id, |st| sftp_retry_failed(st));
    });

    ui.on_sftp_clear_queue(move || {
        runtime::with_window(window_id, |st| sftp_clear_queue(st));
    });

    ui.on_sftp_mkdir(move || {
        runtime::with_window(window_id, |st| sftp_do_mkdir(st));
    });

    ui.on_sftp_rename(move || {
        runtime::with_window(window_id, |st| sftp_do_rename(st));
    });

    ui.on_sftp_delete(move || {
        runtime::with_window(window_id, |st| sftp_do_delete(st));
    });

    ui.on_sftp_close(|| {});

    ui.on_profile_open(move |index| {
        runtime::with_window(window_id, |st| {
            if let Some(store_index) = profile_row_to_store(st, index) {
                open_profile_tab(st, store_index);
            }
        });
    });

    ui.on_profile_new(move || {
        runtime::with_window(window_id, |st| open_profile_form(st));
    });

    ui.on_profile_edit(move |index| {
        runtime::with_window(window_id, |st| {
            if let Some(store_index) = profile_row_to_store(st, index) {
                edit_profile_form(st, store_index);
            }
        });
    });

    ui.on_profile_filter(move |query| {
        runtime::with_window(window_id, |st| {
            st.profile_query = query.to_string();
            refresh_profiles(st);
            eprintln!(
                "[profile] filter={:?} items={}",
                st.profile_query,
                st.profile_indices.len()
            );
        });
    });

    // 星标：点已默认的项 = 取消默认（回退系统 shell）；点其他项 = 切换默认。
    ui.on_profile_set_default(move |index| {
        runtime::with_window(window_id, |st| {
            let Some(store_index) = profile_row_to_store(st, index) else {
                return;
            };
            let Some(id) = st.profiles.profiles.get(store_index).map(|p| p.id) else {
                return;
            };
            st.cfg.default_profile_id = if st.cfg.default_profile_id == id {
                0
            } else {
                id
            };
            st.cfg.save();
            refresh_profiles(st);
            if st.cfg.default_profile_id == 0 {
                eprintln!("[profile] 已清除默认 Profile（新 Tab 用系统 shell）");
            } else {
                eprintln!(
                    "[profile] 默认 Profile id={id} name={:?}",
                    st.profiles
                        .profiles
                        .get(store_index)
                        .map(|p| p.name.as_str())
                        .unwrap_or("")
                );
            }
        });
    });

    ui.on_profile_duplicate(move |index| {
        runtime::with_window(window_id, |st| {
            let Some(store_index) = profile_row_to_store(st, index) else {
                return;
            };
            let Some(source_id) = st.profiles.profiles.get(store_index).map(|p| p.id) else {
                return;
            };
            match st.profiles.duplicate(source_id) {
                Ok(id) => {
                    st.profiles.save();
                    refresh_profiles(st);
                    eprintln!(
                        "[profile] 复制 id={source_id} → id={id} name={:?}（共 {}）",
                        st.profiles.get(id).map(|p| p.name.as_str()).unwrap_or(""),
                        st.profiles.profiles.len()
                    );
                }
                Err(e) => eprintln!("[profile] 复制 id={source_id} 失败: {e}"),
            }
        });
    });

    ui.on_profile_form_save(move || {
        runtime::with_window(window_id, |st| save_profile_form(st));
    });

    ui.on_profile_form_cancel(move || {
        runtime::with_window(window_id, |st| {
            st.ui.set_form_error("".into());
            refocus_after_modal(st);
        });
    });

    ui.on_hostkey_trust(move || {
        runtime::with_window(window_id, |st| hostkey_trust(st));
    });
    ui.on_hostkey_once(move || {
        runtime::with_window(window_id, |st| hostkey_once(st));
    });
    ui.on_hostkey_cancel(move || {
        runtime::with_window(window_id, |st| hostkey_cancel(st));
    });

    ui.on_ssh_exit_reconnect(move || {
        runtime::with_window(window_id, |st| reconnect_ssh_exit(st));
    });
    ui.on_ssh_exit_dismiss(move || {
        runtime::with_window(window_id, |st| dismiss_ssh_exit(st));
    });

    ui.on_profile_delete(move |index| {
        runtime::with_window(window_id, |st| {
            let Some(store_index) = profile_row_to_store(st, index) else {
                return;
            };
            let Some(p) = st.profiles.profiles.get(store_index).map(|p| p.id) else {
                return;
            };
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

    // 面板内控件（搜索框）拿过焦点；关闭后必须还给设置 FocusScope 或终端 IME，
    // 否则 Esc/按键全丢（设置页叠在下面时尤其明显）。
    ui.on_profiles_close(move || {
        runtime::with_window(window_id, |st| {
            refocus_after_modal(st);
            eprintln!("[profile] panel closed");
        });
    });

    ui.on_vault_set_master(move || {
        runtime::with_window(window_id, |st| vault_set_master_password(st));
    });

    ui.on_vault_unlock(move || {
        runtime::with_window(window_id, |st| vault_unlock(st));
    });

    ui.on_vault_lock(move || {
        runtime::with_window(window_id, |st| vault_lock(st));
    });

    ui.on_vault_clear(move || {
        runtime::with_window(window_id, |st| vault_clear(st));
    });

    ui.on_bg_reattach(move |index| {
        runtime::with_window(window_id, |st| reattach_background(st, index.max(0) as usize));
    });

    ui.on_bg_close(|| {});

    ui.on_paste_accept(move || {
        runtime::with_window(window_id, |st| {
            st.ui.set_paste_open(false);
            if let Some(text) = st.pending_paste.take() {
                deliver_paste(st, &text);
            }
        });
    });

    ui.on_paste_always(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.confirm_multiline_paste = false;
            st.ui.set_cfg_confirm_paste(false);
            st.cfg.save();
            st.ui.set_paste_open(false);
            if let Some(text) = st.pending_paste.take() {
                deliver_paste(st, &text);
            }
        });
    });

    ui.on_paste_cancel(move || {
        runtime::with_window(window_id, |st| {
            st.pending_paste = None;
            st.ui.set_paste_open(false);
        });
    });

    ui.on_set_right_click(move |mode| {
        runtime::with_window(window_id, |st| {
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

    ui.on_toggle_paste_middle(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.paste_on_middle_click = !st.cfg.paste_on_middle_click;
            st.ui.set_cfg_paste_middle(st.cfg.paste_on_middle_click);
            st.cfg.save();
            eprintln!(
                "[ui] paste_on_middle_click={}",
                st.cfg.paste_on_middle_click
            );
        });
    });

    ui.on_toggle_confirm_paste(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.confirm_multiline_paste = !st.cfg.confirm_multiline_paste;
            st.ui.set_cfg_confirm_paste(st.cfg.confirm_multiline_paste);
            st.ui
                .set_cfg_right_click(st.cfg.effective_right_click().into());
            st.ui.set_cfg_paste_middle(st.cfg.paste_on_middle_click);
            st.cfg.save();
            eprintln!(
                "[ui] confirm_multiline_paste={}",
                st.cfg.confirm_multiline_paste
            );
        });
    });

    ui.on_toggle_desktop_notifications(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.desktop_notifications = !st.cfg.desktop_notifications;
            st.ui
                .set_cfg_desktop_notifications(st.cfg.desktop_notifications);
            st.cfg.save();
            eprintln!(
                "[ui] desktop_notifications={}",
                st.cfg.desktop_notifications
            );
        });
    });

    ui.on_toggle_visual_bell(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.visual_bell = !st.cfg.visual_bell;
            st.ui.set_cfg_visual_bell(st.cfg.visual_bell);
            if !st.cfg.visual_bell {
                // 关掉时立即清掉所有进行中的闪烁，避免残留边框。
                for rt in st.panes.values_mut() {
                    rt.bell_until = None;
                }
                st.bell_timer.stop();
                refresh_all_pane_views(st);
            }
            st.cfg.save();
            eprintln!("[ui] visual_bell={}", st.cfg.visual_bell);
        });
    });

    ui.on_toggle_audible_bell(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.audible_bell = !st.cfg.audible_bell;
            st.ui.set_cfg_audible_bell(st.cfg.audible_bell);
            st.cfg.save();
            eprintln!("[ui] audible_bell={}", st.cfg.audible_bell);
        });
    });

    ui.on_toggle_scroll_output(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.scroll_on_output = !st.cfg.scroll_on_output;
            st.ui.set_cfg_scroll_output(st.cfg.scroll_on_output);
            // 已存在的 pane 立即生效（actor 侧字段），不只影响新 pane。
            let enabled = st.cfg.scroll_on_output;
            for rt in st.panes.values() {
                rt.handle.set_scroll_on_output(enabled);
            }
            // 后台保留的会话重新接管时也应使用新配置。
            for (_, rt, _) in &st.detached {
                rt.handle.set_scroll_on_output(enabled);
            }
            st.cfg.save();
            eprintln!("[ui] scroll_on_output={enabled}");
        });
    });

    ui.on_toggle_scroll_keystroke(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.scroll_on_keystroke = !st.cfg.scroll_on_keystroke;
            st.ui.set_cfg_scroll_keystroke(st.cfg.scroll_on_keystroke);
            st.cfg.save();
            eprintln!("[ui] scroll_on_keystroke={}", st.cfg.scroll_on_keystroke);
        });
    });

    ui.on_toggle_sixel(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.sixel = !st.cfg.sixel;
            st.ui.set_cfg_sixel(st.cfg.sixel);
            let enabled = st.cfg.sixel;
            for rt in st.panes.values() {
                rt.handle.set_sixel(enabled);
            }
            for (_, rt, _) in &st.detached {
                rt.handle.set_sixel(enabled);
            }
            st.cfg.save();
            eprintln!("[ui] sixel={enabled}");
        });
    });

    ui.on_toggle_sixel_scrolling(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.sixel_scrolling = !st.cfg.sixel_scrolling;
            st.ui.set_cfg_sixel_scrolling(st.cfg.sixel_scrolling);
            let scrolling = st.cfg.sixel_scrolling;
            for rt in st.panes.values() {
                rt.handle.set_sixel_scrolling(scrolling);
            }
            for (_, rt, _) in &st.detached {
                rt.handle.set_sixel_scrolling(scrolling);
            }
            st.cfg.save();
            eprintln!("[ui] sixel_scrolling={scrolling}");
        });
    });

    ui.on_toggle_iip(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.iip = !st.cfg.iip;
            st.ui.set_cfg_iip(st.cfg.iip);
            let enabled = st.cfg.iip;
            for rt in st.panes.values() {
                rt.handle.set_iip(enabled);
            }
            for (_, rt, _) in &st.detached {
                rt.handle.set_iip(enabled);
            }
            st.cfg.save();
            eprintln!("[ui] iip={enabled}");
        });
    });

    ui.on_toggle_render_hud(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.render_hud = !st.cfg.render_hud;
            st.ui.set_cfg_render_hud(st.cfg.render_hud);
            let enabled = render_hud_forced_by_env() || st.cfg.render_hud;
            set_render_hud(st, enabled);
            st.cfg.save();
            eprintln!("[ui] render_hud={} forced={}", st.cfg.render_hud, render_hud_forced_by_env());
        });
    });

    ui.on_toggle_scrollback_unlimited(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.scrollback_unlimited = !st.cfg.scrollback_unlimited;
            st.ui
                .set_cfg_scrollback_unlimited(st.cfg.scrollback_unlimited);
            st.cfg.save();
            eprintln!(
                "[ui] scrollback_unlimited={} effective={} (仅新 pane)",
                st.cfg.scrollback_unlimited,
                st.cfg.effective_scrollback()
            );
        });
    });

    ui.on_toggle_mcp_enabled(move || {
        runtime::with_window(window_id, |st| toggle_mcp_setting(st, McpSetting::Enabled));
    });
    ui.on_toggle_mcp_read(move || {
        runtime::with_window(window_id, |st| toggle_mcp_setting(st, McpSetting::Read));
    });
    ui.on_toggle_mcp_input(move || {
        runtime::with_window(window_id, |st| toggle_mcp_setting(st, McpSetting::Input));
    });
    ui.on_toggle_mcp_tabs(move || {
        runtime::with_window(window_id, |st| toggle_mcp_setting(st, McpSetting::Tabs));
    });
    ui.on_toggle_mcp_destructive(move || {
        runtime::with_window(window_id, |st| toggle_mcp_setting(st, McpSetting::Destructive));
    });

    ui.on_set_scrollbar_policy(move |value| {
        runtime::with_window(window_id, |st| {
            let policy = ScrollbarPolicy::from_config(&value);
            st.cfg.scrollbar_policy = policy.as_str().to_string();
            st.ui.set_cfg_scrollbar_policy(policy.as_str().into());
            refresh_all_pane_views(st);
            st.cfg.save();
            eprintln!("[ui] scrollbar_policy={}", policy.as_str());
        });
    });

    ui.on_set_backspace_binding(move |value| {
        runtime::with_window(window_id, |st| {
            let binding = keys::BackspaceBinding::from_config(&value);
            st.cfg.backspace_binding = binding.as_str().to_string();
            st.ui.set_cfg_backspace_binding(binding.as_str().into());
            st.cfg.save();
            eprintln!("[ui] backspace_binding={}", binding.as_str());
        });
    });

    ui.on_set_delete_binding(move |value| {
        runtime::with_window(window_id, |st| {
            let binding = keys::DeleteBinding::from_config(&value);
            st.cfg.delete_binding = binding.as_str().to_string();
            st.ui.set_cfg_delete_binding(binding.as_str().into());
            st.cfg.save();
            eprintln!("[ui] delete_binding={}", binding.as_str());
        });
    });

    ui.on_settings_open_path(move || {
        runtime::with_window(window_id, |_st| {
            let path = Config::path();
            let dir = path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| path.clone());
            eprintln!("[ui] open config folder {}", dir.display());
            // 复用 shell-safe 的 xdg-open helper：单个参数、失败显示浮层、不 panic。
            let shown = dir.display().to_string();
            open_with_xdg(dir.into_os_string(), shown, "path");
        });
    });

    ui.on_settings_help_open(move |kind| {
        runtime::with_window(window_id, |st| {
            let info = help_info();
            let lang = st.cfg.lang();
            let is_logs = kind.as_str() == "logs";
            let kind_static: &'static str = if is_logs { "logs" } else { "config" };
            let dir = if is_logs { info.log_dir } else { info.config_dir };
            if let Err(msg) = open_help_directory(dir, kind_static, lang) {
                st.ui.set_help_status(msg.as_str().into());
            }
        });
    });

    ui.on_run_doctor(move || {
        runtime::with_window(window_id, |st| {
            st.ui.set_doctor_status("Running deterministic Doctor…".into());
            match diag::doctor_markdown() {
                Ok(markdown) => {
                    st.ui.set_doctor_text(markdown.into());
                    st.ui.set_doctor_open(true);
                    st.ui.set_doctor_status("".into());
                },
                Err(error) => st.ui.set_doctor_status(error.into()),
            }
        });
    });

    ui.on_close_doctor(move || {
        runtime::with_window(window_id, |st| {
            st.ui.set_doctor_open(false);
            st.ui.set_doctor_text("".into());
        });
    });

    ui.on_create_feedback(move || {
        runtime::with_window(window_id, |st| {
            let description = st.ui.get_feedback_text().to_string();
            match feedback::create_bundle(&description) {
                Ok(bundle) => {
                    let url = std::env::var("TERVO_FEEDBACK_ISSUE_URL")
                        .ok()
                        .filter(|v| !v.trim().is_empty())
                        .unwrap_or_else(|| st.cfg.feedback_issue_url.clone());
                    let result = if url.trim().is_empty() {
                        bundle
                            .parent()
                            .ok_or_else(|| "bundle parent missing".to_string())
                            .and_then(feedback::open_folder)
                            .map(|()| format!("Bundle: {}", bundle.display()))
                    } else {
                        feedback::open_issue(&url, &bundle)
                            .map(|()| format!("Bundle: {} (issue page opened)", bundle.display()))
                    };
                    st.ui.set_feedback_status(match result {
                        Ok(message) => message.into(),
                        Err(error) => format!("Feedback error: {error}").into(),
                    });
                },
                Err(error) => st.ui.set_feedback_status(format!("Feedback error: {error}").into()),
            }
        });
    });

    ui.on_settings_help_copy_diagnostics(move || {
        runtime::with_window(window_id, |st| copy_help_diagnostics(st));
    });

    ui.on_settings_category_changed(move |c| {
        runtime::with_window(window_id, |st| {
            if c == 6 {
                refresh_vault_ui(st);
            } else {
                st.vault_clear_armed = false;
            }
            // Profiles 分类直接读 profile-model；每次进入都刷新（含搜索清空）。
            if c == 7 {
                refresh_profiles(st);
            }
            // Help 每次进入都重取路径（TERVO_CONFIG/TERVO_LOG_DIR 可能变了）。
            if c == 8 {
                refresh_help_info(st);
            }
            // Privacy/MCP 的 socket 状态由后台 supervisor 更新，进入页面时刷新。
            if c == 9 {
                sync_mcp_ui(st);
            }
        });
        eprintln!("[ui] settings category={c}");
        if c != 4 {
            // 离开 Hotkeys 页时结束录制，避免在别的页面上吞键盘。
            runtime::with_window(window_id, |st| {
                if st.hotkey_recording.is_some() {
                    cancel_hotkey_edit(st);
                }
            });
        }
    });

    ui.on_settings_filter(move |query| {
        runtime::with_window(window_id, |st| {
            let q = query.to_string();
            st.ui.set_settings_query(q.as_str().into());
            let hits = settings_search_hits(&q, st.cfg.lang());
            st.settings_result_cats = hits.iter().map(|h| h.cat).collect();
            st.settings_result_sections = hits.iter().map(|h| h.section).collect();
            st.settings_model
                .set_vec(hits.into_iter().map(|h| h.line.into()).collect::<Vec<_>>());
            eprintln!("[ui] settings filter q={q:?} hits={}", st.settings_result_cats.len());
        });
    });

    ui.on_settings_open_result(move |index| {
        runtime::with_window(window_id, |st| {
            let idx = index.max(0) as usize;
            if let Some(cat) = st.settings_result_cats.get(idx) {
                st.ui.set_settings_category(*cat);
                eprintln!("[ui] settings goto cat={cat}");
            }
            if let Some(section) = st.settings_result_sections.get(idx) {
                // 先下发锚点再自增 token，保证 Slint 的 changed 处理器看到新锚点。
                st.ui.set_settings_focus((*section).into());
                st.settings_jump_token = st.settings_jump_token.wrapping_add(1);
                st.ui.set_settings_jump_token(st.settings_jump_token);
                // 首次下发的同一轮 Fluent 布局里卡片 y 可能还是 0；80ms 后
                // 再 bump 一次 token，让 Slint 在布局完成后再滚一次（幂等）。
                let weak = st.ui.as_weak();
                st.settings_jump_timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(80),
                    move || {
                        if let Some(ui) = weak.upgrade() {
                            let next = ui.get_settings_jump_token().wrapping_add(1);
                            ui.set_settings_jump_token(next);
                        }
                    },
                );
                eprintln!("[ui] settings jump section={section}");
            }
            st.ui.set_settings_query("".into());
            st.settings_model.set_vec(Vec::<slint::SharedString>::new());
        });
    });

    ui.on_settings_manage_profiles(move || {
        runtime::with_window(window_id, |st| {
            refresh_profiles(st);
            open_profiles_panel(st);
        });
    });

    ui.on_settings_toggled(move |open| {
        if open {
            runtime::with_window(window_id, |st| {
                if st.search_open {
                    close_search(st);
                }
            });
        }
        runtime::with_window(window_id, |st| {
            if open {
                refresh_vault_ui(st);
                if st.ui.get_settings_category() == 7 {
                    refresh_profiles(st);
                }
                st.ui.set_settings_focus("".into());
            } else {
                st.vault_clear_armed = false;
                st.ui.set_settings_focus("".into());
            }
        });
        eprintln!("[ui] settings open={open}");
        runtime::with_window(window_id, |st| {
            if !open {
                // 关闭设置时退出录制，并确保终端重新拿到键盘焦点。
                st.hotkey_recording = None;
                st.ui.set_hotkey_recording_row(-1);
                st.ui.set_hotkey_error("".into());
                st.ui.invoke_focus_terminal();
            }
        });
    });

    ui.on_hotkey_start_edit(move |index| {
        runtime::with_window(window_id, |st| start_hotkey_edit(st, index));
    });

    ui.on_hotkey_cancel_edit(move || {
        runtime::with_window(window_id, |st| cancel_hotkey_edit(st));
    });

    ui.on_hotkey_reset(move |index| {
        runtime::with_window(window_id, |st| reset_hotkey(st, index));
    });

    ui.on_hotkey_reset_all(move || {
        runtime::with_window(window_id, |st| reset_all_hotkeys(st));
    });

    ui.on_toggle_copy_on_select(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.copy_on_select = !st.cfg.copy_on_select;
            st.ui.set_cfg_copy_on_select(st.cfg.copy_on_select);
            st.cfg.save();
            eprintln!("[ui] copy_on_select={}", st.cfg.copy_on_select);
        });
    });

    ui.on_toggle_confirm_close(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.confirm_close = !st.cfg.confirm_close;
            st.ui.set_cfg_confirm_close(st.cfg.confirm_close);
            st.cfg.save();
            eprintln!("[ui] confirm_close={}", st.cfg.confirm_close);
        });
    });

    ui.on_toggle_sidebar_visible(move || {
        runtime::with_window(window_id, |st| {
            st.content = toggle_sidebar_math(st.content, &mut st.cfg);
            st.ui.set_cfg_sidebar_visible(st.cfg.sidebar_visible);
            st.ui.set_sidebar_visible(st.cfg.sidebar_visible);
            let w = sidebar_width_of(&st.cfg);
            st.ui.set_sidebar_width(w);
            refresh_views(st);
            redraw_visible(st);
            st.cfg.save();
            eprintln!(
                "[ui] sidebar_visible={}（宽 {}px）",
                st.cfg.sidebar_visible, w
            );
        });
    });

    ui.on_toggle_restore_session(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.restore_session_on_startup = !st.cfg.restore_session_on_startup;
            st.ui
                .set_cfg_restore_session(st.cfg.restore_session_on_startup);
            st.cfg.save();
            eprintln!(
                "[ui] restore_session_on_startup={}（下次启动生效；正常退出时写快照）",
                st.cfg.restore_session_on_startup
            );
        });
    });

    ui.on_tabsearch_open_requested(move || {
        let _ = runtime::with_window(window_id, open_tab_search);
    });
    ui.on_tabsearch_query_changed(move |_q| {
        runtime::with_window(window_id, |st| {
            st.tab_search_query = st.ui.get_tabsearch_query().to_string();
            refresh_tab_search(st);
        });
    });
    ui.on_tabsearch_move(move |delta| {
        let _ = runtime::with_window(window_id, |st| tab_search_move(st, delta));
    });
    ui.on_tabsearch_accept(move || {
        let _ = runtime::with_window(window_id, tab_search_accept);
    });
    ui.on_tabsearch_cancel(move || {
        let _ = runtime::with_window(window_id, close_tab_search);
    });
    ui.on_tabsearch_pick(move |row| {
        let _ = runtime::with_window(window_id, |st| tab_search_pick(st, row.max(0) as usize));
    });

    ui.on_palette_query_changed(move |_q| {
        runtime::with_window(window_id, |st| refresh_palette(st));
    });

    ui.on_palette_move(move |delta| {
        runtime::with_window(window_id, |st| palette_move(st, delta));
    });

    ui.on_palette_accept(move || {
        runtime::with_window(window_id, |st| palette_accept(st));
    });

    ui.on_palette_cancel(move || {
        runtime::with_window(window_id, |st| st.ui.set_palette_open(false));
    });

    ui.on_palette_pick(move |index| {
        runtime::with_window(window_id, |st| {
            st.ui.set_palette_selected(index.max(0));
            palette_accept(st);
        });
    });

    ui.on_search_toggle_case(move || {
        runtime::with_window(window_id, toggle_search_case);
    });

    ui.on_search_toggle_regex(move || {
        runtime::with_window(window_id, toggle_search_regex);
    });

    // TextInput 的编辑（键入/退格/粘贴/鼠标输入）都会回调 edited；文本以 UI 为准。
    ui.on_search_edited(move || {
        runtime::with_window(window_id, search_edited);
    });

    // Enter / Shift+Enter：从当前命中继续向前/向后，并刷新 index/total。
    ui.on_search_next(move |forward| {
        runtime::with_window(window_id, |st| run_search(st, forward));
    });

    // Esc：关闭搜索条并把 IME/键盘焦点还给终端。
    ui.on_search_close(move || {
        runtime::with_window(window_id, close_search);
    });

    ui.on_save_commit(move |path| {
        runtime::with_window(window_id, |st| save_contents_commit(st, path.as_str()));
    });

    ui.on_save_cancel(move || {
        runtime::with_window(window_id, cancel_save_contents);
    });

    ui.on_tab_reorder(move |from, drag_x, strip_w| {
        runtime::with_window(window_id, |st| {
            let n = st.ws.tabs.len();
            st.ui.set_drop_index(-1);
            st.ui.set_drop_gap(-1);
            st.ui.set_drop_valid(false);
            if n < 2 || from < 0 {
                return;
            }
            let from = from as usize;
            if from >= n {
                return;
            }
            let target =
                drag_target_in_group(drag_x, strip_w, n, st.ws.pinned_count(), from);
            let Some(target) = target else {
                eprintln!("[ui] tab drop blocked from={from} x={drag_x:.1} strip={strip_w:.1}");
                return;
            };
            if st.ws.move_tab(from, target) {
                eprintln!("[ws] reorder tab {from} → {target}");
                rebuild_tabs(st);
            }
        });
    });

    // 拖拽过程中只更新落点属性；不重建 Tab 模型/TouchArea，避免每帧重排。
    ui.on_tab_drag_move(move |from, drag_x, strip_w| {
        runtime::with_window(window_id, |st| {
            let n = st.ws.tabs.len();
            if from < 0 || from as usize >= n {
                st.ui.set_drop_index(-1);
                st.ui.set_drop_gap(-1);
                st.ui.set_drop_valid(false);
                return;
            }
            let from = from as usize;
            let target =
                drag_target_in_group(drag_x, strip_w, n, st.ws.pinned_count(), from);
            let gap: i32 = target
                .map(|t| drag_gap_index(from, t) as i32)
                .unwrap_or(-1);
            let prev = (st.ui.get_drop_index(), st.ui.get_drop_gap(), st.ui.get_drop_valid());
            st.ui.set_drop_index(target.map(|t| t as i32).unwrap_or(-1));
            st.ui.set_drop_gap(gap);
            st.ui.set_drop_valid(target.is_some());
            if prev != (target.map(|t| t as i32).unwrap_or(-1), gap, target.is_some()) {
                eprintln!(
                    "[ui] tab drag from={from} x={drag_x:.1} strip={strip_w:.1} target={} gap={gap}",
                    target.map(|t| t.to_string()).unwrap_or_else(|| "blocked".into())
                );
            }
        });
    });

    ui.on_tab_drag_cancelled(move || {
        runtime::with_window(window_id, |st| {
            st.ui.set_drop_index(-1);
            st.ui.set_drop_gap(-1);
            st.ui.set_drop_valid(false);
        });
        eprintln!("[ui] tab drag cancelled");
    });

    ui.on_tab_context(move |index, x| {
        eprintln!("[ui] tab context index={index} x={x:.1}");
        runtime::with_window(window_id, |st| {
            st.ui.set_tabctx_index(index);
            st.ui.set_tabctx_x(x);
            st.ui.set_tabctx_open(true);
        });
    });

    ui.on_tab_menu_action(move |index, action| {
        eprintln!("[ui] tab action={action} index={index}");
        runtime::with_window(window_id, |st| {
            let index = index.max(0) as usize;
            match action {
                0 => close_tab_inner(st, index),
                1 => close_other_tabs(st, index),
                2 => duplicate_tab(st, index),
                3 => open_rename(st, index),
                4 => close_tabs_left(st, index),
                5 => close_tabs_right(st, index),
                // 6 = Pin/Unpin 切换（additive；旧 ID 0..5 不变）
                6 => toggle_tab_pin(st, index),
                _ => {}
            }
        });
    });

    ui.on_tabbar_context(move |x, y| {
        eprintln!("[ui] tabbar menu at {x},{y}");
        runtime::with_window(window_id, |st| {
            st.ui.set_tabmenu_x(x);
            st.ui.set_tabmenu_y(y);
            st.ui.set_tabmenu_open(true);
        });
    });

    ui.on_tabbar_menu_action(move |action| {
        eprintln!("[ui] tabbar action={action}");
        runtime::with_window(window_id, |st| match action {
            0 => {} // 置顶由 Slint 侧直接切换（绑定 Window.always-on-top）
            1 => {
                new_tab_with_default(st);
                rebuild_tabs(st);
            }
            13 => new_window(),
            2 => {
                st.ui.set_settings_open(true);
            }
            3 => {
                let _ = slint::quit_event_loop();
            }
            4 => restart_session(st),
            5 => open_palette(st),
            7 => reattach_last_background(st),
            8 => open_background_panel(st),
            9 => open_profiles_panel(st),
            10 => open_sftp_panel(st),
            11 => open_forwards_panel(st),
            6 => {
                st.cfg.tabbar_position = if st.cfg.tabbar_position == "bottom" {
                    "top".into()
                } else {
                    "bottom".into()
                };
                st.ui.set_tabbar_bottom(st.cfg.tabbar_position == "bottom");
                st.cfg.save();
                // 内容区在窗口内的纵向偏移变了：候选点必须重新下发。
                st.ime_spot_dirty = true;
                update_ime_spot(st);
                eprintln!("[ui] tabbar_position={}", st.cfg.tabbar_position);
            }
            _ => {}
        });
    });

    ui.on_close_window(move || {
        runtime::with_window(window_id, |st| request_close(st));
    });

    ui.on_confirm_accept(move || {
        runtime::with_window(window_id, |st| {
            st.ui.set_confirm_open(false);
            close_current_window(st);
        });
    });

    ui.on_confirm_cancel(move || {
        eprintln!("[ui] close cancelled");
        runtime::with_window(window_id, |st| st.ui.set_confirm_open(false));
    });

    // 每个窗口自己的主题展示/字号/回滚行数/Quake 开关初始状态。
    // 配置本身是 SharedServices 里唯一一份，这里只同步 UI mirror。
    let state_handle = state.clone();
    runtime::with_state(&state_handle, |st| {
        let auto = is_auto_theme(&st.cfg.theme);
        let theme = st.resolved_theme.clone();
        st.ui.set_theme_name(if auto {
            AUTO_THEME.into()
        } else {
            theme.id.as_str().into()
        });
        st.ui.set_auto_resolved(if auto {
            theme.label.as_str().into()
        } else {
            "".into()
        });
        st.ui.set_theme_display(if auto {
            format!(
                "{} · {}",
                ui_text(st.cfg.lang(), "Auto", "自动"),
                theme.label
            )
            .into()
        } else {
            theme.label.as_str().into()
        });
        st.ui
            .global::<Palette>()
            .set_colors(slint_theme(&theme.ui));
        st.ui.set_theme_choices(theme_choices(&st.registry));
        st.ui.set_font_size(st.cfg.font_size as i32);
        st.ui.set_scrollback(st.cfg.scrollback as i32);
        st.ui.set_cfg_quake(st.cfg.quake_enabled);
        install_render_hud(st);
    });

    ui.on_set_theme(move |name| {
        eprintln!("[theme] 选择 -> {name}");
        runtime::with_window(window_id, |st| apply_theme(st, name.as_str()));
    });

    // Settings → Appearance → Language：立即切换静态文案（Slint 侧已先改
    // root.language），Rust 侧再重建动态列表并写回 config；无需重启。
    ui.on_language_changed(move |code| {
        runtime::with_window(window_id, |st| {
            let next = config::normalize_language(code.as_str());
            if st.cfg.language == next {
                return;
            }
            st.cfg.language = next.clone();
            st.cfg.save();
            sync_language_ui(st);
            eprintln!("[ui] language={next}");
        });
    });

    // Appearance：背景图路径（空 = 关闭）。合法路径输入过程中即时加载/写盘；
    // 非法路径只在回车/失焦提交时显示错误，并保留上一张已生效背景（回退）。
    ui.on_background_image_changed(move |path| {
        runtime::with_window(window_id, |st| {
            commit_background_path(st, path.as_str(), false);
        });
    });
    ui.on_background_image_commit(move |path| {
        runtime::with_window(window_id, |st| {
            commit_background_path(st, path.as_str(), true);
        });
    });

    let image_picker_weak = ui.as_weak();
    ui.on_background_image_browse(move || {
        let Some(current_ui) = image_picker_weak.upgrade() else { return };
        let initial = current_ui.get_background_image_path().to_string();
        let weak = image_picker_weak.clone();
        std::thread::Builder::new()
            .name("image-picker".into())
            .spawn(move || {
                let result = pick_background_image(&initial);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(_ui) = weak.upgrade() else { return };
                    runtime::with_window(window_id, |st| match result {
                        Ok(Some(path)) => {
                            eprintln!("[theme] 文件选择器选择背景图: {path}");
                            st.ui.set_background_image_path(path.as_str().into());
                            commit_background_path(st, &path, true);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let err = e.clone();
                            let msg = if st.cfg.lang() == "zh" {
                                e
                            } else {
                                format!("Image picker unavailable: {e}")
                            };
                            st.ui.set_background_image_error(msg.into());
                            eprintln!("[theme] 背景图选择器失败: {err}");
                        }
                    });
                });
            })
            .ok();
    });

    // 拖动时只预览（重绘终端帧），松手才写盘，避免每一帧都原子写 config.toml。
    // 无背景图时不改有效 alpha（见 renderer_opacity），所以不会把纯色终端拖灰。
    ui.on_background_opacity_preview(move |value| {
        runtime::with_window(window_id, |st| {
            let value = config::clamp_opacity(value);
            st.cfg.background_opacity = value;
            sync_renderer_opacity(st);
        });
    });

    ui.on_background_opacity_changed(move |value| {
        runtime::with_window(window_id, |st| {
            let value = config::clamp_opacity(value);
            st.cfg.background_opacity = value;
            st.ui.set_background_opacity(value);
            sync_renderer_opacity(st);
            st.cfg.save();
            eprintln!("[theme] background_opacity={value:.2}（仅终端默认背景）");
        });
    });

    // 遮罩层只影响背景图，由 Slint 直接改属性完成即时预览；松手才落盘。
    ui.on_background_dim_preview(move |value| {
        runtime::with_window(window_id, |st| {
            st.cfg.background_dim = config::clamp_dim(value);
        });
    });

    ui.on_background_dim_changed(move |value| {
        runtime::with_window(window_id, |st| {
            let value = config::clamp_dim(value);
            st.cfg.background_dim = value;
            st.ui.set_background_dim(value);
            st.cfg.save();
            eprintln!("[theme] background_dim={value:.2}");
        });
    });

    // ---- Appearance：光标形状/闪烁/颜色 + 粗体（GNOME Terminal 基础项）----
    // 形状/颜色只影响光标 overlay，不重绘终端帧；闪烁用 UI timer 切换 overlay 可见性。
    ui.on_set_cursor_shape(move |shape| {
        runtime::with_window(window_id, |st| {
            let next = config::normalize_cursor_shape(shape.as_str());
            if st.cfg.cursor_shape == next {
                return;
            }
            st.cfg.cursor_shape = next.clone();
            st.ui.set_cfg_cursor_shape(next.as_str().into());
            apply_cursor_settings(st);
            st.cfg.save();
            eprintln!("[appearance] cursor_shape={next}");
        });
    });

    ui.on_set_cursor_blink(move |mode| {
        runtime::with_window(window_id, |st| {
            let next = config::normalize_cursor_blink(mode.as_str());
            if st.cfg.cursor_blink == next {
                return;
            }
            st.cfg.cursor_blink = next.clone();
            st.ui.set_cfg_cursor_blink(next.as_str().into());
            configure_cursor_blink(st);
            st.cfg.save();
            eprintln!("[appearance] cursor_blink={next}");
        });
    });

    // 输入过程中空串=跟随主题、合法 #RGB/#RRGGBB 即时生效；半截/非法输入不写配置。
    ui.on_cursor_color_changed(move |text| {
        runtime::with_window(window_id, |st| {
            let raw = text.as_str();
            let next = if raw.trim().is_empty() {
                String::new()
            } else {
                let Some(color) = config::normalize_hex_color(raw) else {
                    return;
                };
                color
            };
            if st.cfg.cursor_color == next {
                return;
            }
            st.cfg.cursor_color = next.clone();
            apply_cursor_settings(st);
            st.cfg.save();
            eprintln!("[appearance] cursor_color={next:?}");
        });
    });

    ui.on_cursor_text_color_changed(move |text| {
        runtime::with_window(window_id, |st| {
            let raw = text.as_str();
            let next = if raw.trim().is_empty() {
                String::new()
            } else {
                let Some(color) = config::normalize_hex_color(raw) else {
                    return;
                };
                color
            };
            if st.cfg.cursor_text_color == next {
                return;
            }
            st.cfg.cursor_text_color = next.clone();
            apply_cursor_settings(st);
            st.cfg.save();
            eprintln!("[appearance] cursor_text_color={next:?}");
        });
    });

    ui.on_toggle_bold_is_bright(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.bold_is_bright = !st.cfg.bold_is_bright;
            st.ui.set_cfg_bold_is_bright(st.cfg.bold_is_bright);
            apply_bold_settings(st);
            st.cfg.save();
            eprintln!("[appearance] bold_is_bright={}", st.cfg.bold_is_bright);
        });
    });

    ui.on_bold_color_changed(move |text| {
        runtime::with_window(window_id, |st| {
            let raw = text.as_str();
            let next = if raw.trim().is_empty() {
                String::new()
            } else {
                let Some(color) = config::normalize_hex_color(raw) else {
                    return;
                };
                color
            };
            if st.cfg.bold_color == next {
                return;
            }
            st.cfg.bold_color = next.clone();
            apply_bold_settings(st);
            st.cfg.save();
            eprintln!("[appearance] bold_color={next:?}");
        });
    });

    ui.on_toggle_bold_color_same_as_fg(move || {
        runtime::with_window(window_id, |st| {
            st.cfg.bold_color_same_as_fg = !st.cfg.bold_color_same_as_fg;
            st.ui
                .set_cfg_bold_color_same_as_fg(st.cfg.bold_color_same_as_fg);
            apply_bold_settings(st);
            st.cfg.save();
            eprintln!(
                "[appearance] bold_color_same_as_fg={}",
                st.cfg.bold_color_same_as_fg
            );
        });
    });

    ui.on_bump_font_size(move |delta| {
        runtime::with_window(window_id, |st| {
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

    ui.on_bump_scrollback(move |delta| {
        runtime::with_window(window_id, |st| {
            // 无限滚动下点 +/-：视为用户要改回固定值，先关闭 unlimited。
            if st.cfg.scrollback_unlimited {
                st.cfg.scrollback_unlimited = false;
                st.ui.set_cfg_scrollback_unlimited(false);
            }
            let new = (st.cfg.scrollback as i64 + delta as i64)
                .clamp(500, UNLIMITED_SCROLLBACK_LINES as i64) as usize;
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
        ui.on_max_pointer_down(|| eprintln!("[ui] max-pointer-down"));
        ui.on_toggle_maximize(move || {
            if let Some(ui) = weak.upgrade() {
                // X11：MaximizeController 是唯一权威。click 只翻转 desired，
                // 发送/PropertyNotify 确认/外部同步都在 worker，UI 只显示 target。
                let from_controller = runtime::with_window(window_id, |st| {
                    let target = st.maximize.as_ref()?.click();
                    st.window_maximized = target;
                    Some(target)
                })
                .flatten();
                if let Some(target) = from_controller {
                    ui.set_window_maximized(target);
                    runtime::with_window(window_id, |st| {
                        request_window_corners(
                            st,
                            None,
                            Some(xconn::CornerState {
                                maximized: target,
                                fullscreen: st.ui.window().is_fullscreen(),
                            }),
                        );
                    });
                    eprintln!("[maximize] ui click target={target}");
                    return;
                }
                // 非 X11 / 状态机启动失败：保留 winit 原生路径；不按标题找窗口。
                let target = runtime::with_window(window_id, |st| {
                    st.window_maximized = !st.window_maximized;
                    st.window_maximized
                })
                .unwrap_or_else(|| !ui.get_window_maximized());
                ui.set_window_maximized(target);
                ui.window().set_maximized(target);
                eprintln!("[maximize] fallback winit target={target}");
            }
        });
    }

    ui.on_tab_clicked(move |i| {
        runtime::with_window(window_id, |st| {
            if st.search_open {
                close_search(st);
            }
        });
        runtime::with_window(window_id, |st| set_active_tab_inner(st, i.max(0) as usize));
    });
    ui.on_tab_closed(move |i| {
        runtime::with_window(window_id, |st| close_tab_inner(st, i.max(0) as usize));
    });
    ui.on_new_tab(move || {
        runtime::with_window(window_id, |st| {
            if st.search_open {
                close_search(st);
            }
        });
        runtime::with_window(window_id, new_tab_with_default);
    });

    ui.on_profilemenu_menu(move || {
        runtime::with_window(window_id, open_profile_menu);
    });

    ui.on_sidebar_command(move |id| {
        runtime::with_window(window_id, |st| {
            let id = id.to_string();
            eprintln!("[ui] sidebar command={id}");
            if id == "profiles.open" {
                open_profile_menu(st);
            } else {
                run_command(st, &id);
            }
        });
    });
    ui.on_sidebar_item_clicked(move |index| {
        runtime::with_window(window_id, |st| {
            let index = index.max(0) as usize;
            if let Some(&profile_index) = st.profile_menu_indices.get(index) {
                eprintln!("[ui] sidebar profile pick={index} profile={profile_index}");
                open_profile_tab(st, profile_index);
            }
        });
    });
    ui.on_profilemenu_pick(move |index| {
        runtime::with_window(window_id, |st| {
            st.ui.set_profilemenu_open(false);
            let index = index.max(0) as usize;
            eprintln!("[ui] profile pick={index}");
            if let Some(&profile_index) = st.profile_menu_indices.get(index) {
                open_profile_tab(st, profile_index);
            }
        });
    });

    ui.on_profilemenu_manage(move || {
        runtime::with_window(window_id, |st| {
            st.ui.set_profilemenu_open(false);
            open_profiles_panel(st);
        });
    });

    ui.on_toggle_quake(move || {
        runtime::with_window(window_id, |st| {
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

    ui.on_resized(move |w, h| {
        runtime::with_window(window_id, |st| {
            // WM live resize 期间指针被 WM grab，winit/Slint 收不到真实 ButtonRelease；
            // TouchArea 会一直保持 pressed/grabbed，后续点击会继续命中旧方向。
            // 收到第一帧由 WM 驱动的 resize 后，异步补发一次 PointerReleased 清理状态。
            if st.wm_resize && !st.wm_release_pending {
                st.wm_release_pending = true;
                let weak = st.ui.as_weak();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let still_wm =
                        matches!(runtime::with_window(window_id, |st| st.wm_resize && st.wm_release_pending), Some(true));
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
            let h = h.max(1.0);
            let sidebar_w = sidebar_width_of(&st.cfg);
            let content_w = (w - sidebar_w).max(1.0);
            let content_h = (h - TABBAR_H).max(1.0);
            // configure 本身就是“WM/窗口系统正在动”的证据；即使尺寸和上一帧
            // 相同也刷新时间戳，watchdog 不会把它误判成 WM 没接管。
            st.last_resize_event = Some(Instant::now());
            if st.wm_resize {
                st.wm_resize_saw_configure = true;
            }
            if (st.content.w - content_w).abs() < 0.5
                && (st.content.h - content_h).abs() < 0.5
            {
                return;
            }
            // 窗口尺寸/DPR 变化：即使光标格相对窗口没动，也让输入法重收 spot。
            st.ime_spot_dirty = true;
            let old_w = st.content.w;
            let was_resizing = st.resize_in_progress;
            // 只有用户正在拖动边/角（WM live resize 或自绘 resize）才隐藏背景/做
            // resize 收尾；最大化、字体变化等程序化 set_size 不应让背景闪一下。
            let live_drag = st.wm_resize || st.resize.is_some();
            st.resize_events = st.resize_events.saturating_add(1);
            st.resize_pending = Some((w, h));
            // UI 布局必须每个 configure 立即更新：缩小时如果晚一帧，右上角按钮
            // 还在旧宽度的右边界外，会被新窗口裁掉，下一帧才“吐出来”。
            st.content = content_rect(w, h, sidebar_w);
            refresh_views_layout(st);
            if live_drag {
                st.resize_in_progress = true;
            }
            // resize 期间隐藏**大窗口**的背景图，避免 Skia 每帧对整张图做 cover
            // 缩放（2560x1440 llvmpipe 实测每帧省约 2ms）。小窗口收益小、闪一下
            // 不划算，故按面积设阈值；停下 200ms 后自动恢复。字符帧不缩放，仍是 1:1。
            // TERVO_RESIZE_KEEP_BG=1 可关闭该优化，用于 A/B 性能对比。
            const RESIZE_BG_SUSPEND_AREA: f32 = 1_500_000.0;
            if live_drag
                && w * h > RESIZE_BG_SUSPEND_AREA
                && st.ui.get_has_background_image()
                && std::env::var_os("TERVO_RESIZE_KEEP_BG").is_none()
            {
                if !st.ui.get_background_suspended()
                    && std::env::var_os("TERVO_RESIZE_PROFILE").is_some()
                {
                    eprintln!("[ui] resize 期间隐藏背景图（减少 cover 缩放）");
                }
                st.ui.set_background_suspended(true);
            }
            // 缩小必须立即重绘，否则旧帧的右上角按钮会露在窗口外一帧；放大可以
            // 合并到 60fps（没有裁切风险），避免高速拖动把渲染线程灌满。
            schedule_resize_redraw(
                st,
                should_redraw_resize_immediately(old_w, w, was_resizing),
            );
            if live_drag {
                schedule_resize_finalize(st);
            }
            // terminal grid/PTY/字符重绘仍 16ms 合并，避免高速拖动把 UI 线程灌满。
            if !st.resize_apply_scheduled {
                st.resize_apply_scheduled = true;
                st.resize_apply.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(16),
                    move || {
                        runtime::with_window(window_id, |st| {
                            st.resize_apply_scheduled = false;
                            let Some((w, h)) = st.resize_pending.take() else { return };
                            let sidebar_w = sidebar_width_of(&st.cfg);
                            st.content = content_rect(w, h, sidebar_w);
                            let t0 = Instant::now();
                            refresh_views(st);
                            let t_layout = t0.elapsed();
                            redraw_visible(st);
                            let total = t0.elapsed();
                            let cost_us = total.as_secs_f64() * 1_000_000.0;
                            st.resize_applied = st.resize_applied.saturating_add(1);
                            st.resize_max_apply_us = st.resize_max_apply_us.max(cost_us);
                            // apply 更新了 pane 图像，必须安排一次重绘；此时布局
                            // 已是新尺寸，不会出现按钮晚一帧。
                            schedule_resize_redraw(st, false);
                            if std::env::var_os("TERVO_RESIZE_PROFILE").is_some() {
                                eprintln!(
                                    "[ui] resize apply events={} cost={:.1}ms layout={:.1}ms redraw={:.1}ms content={:.0}x{:.0}",
                                    st.resize_events,
                                    total.as_secs_f64() * 1000.0,
                                    t_layout.as_secs_f64() * 1000.0,
                                    (total - t_layout).as_secs_f64() * 1000.0,
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
}

/// 启动时对单实例锁的判定结果。
#[cfg(unix)]
enum InstanceClaim {
    /// 本进程持有 primary 锁；稍后 UI 平台就绪再启动 control server。
    Primary(InstanceLock),
    /// 显式 `TERVO_SINGLE_INSTANCE=0`：旧行为，独立进程、无 lock/server。
    Standalone,
}

#[cfg(unix)]
fn single_instance_disabled() -> bool {
    std::env::var(SINGLE_INSTANCE_ENV)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(false)
}

/// 转发给 primary 的请求：保留当前 cwd（旧 `--window` 子进程会继承 cwd）。
#[cfg(unix)]
fn new_window_request() -> OpenWindowRequest {
    let mut request = OpenWindowRequest::new();
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.is_absolute() {
            request = request.with_cwd(cwd);
        }
    }
    request
}

/// 获取 primary 锁；锁被占用时按情况做 secondary 转发或等待 `--restore`。
///
/// 返回 `Primary` 后调用方必须让 guard 存活到进程退出。secondary 成功 ack 后
/// 直接 `exit(0)`，不会走到 UI/Config/MCP/session 初始化。
#[cfg(unix)]
fn claim_instance_or_forward(paths: &InstancePaths, restore_requested: bool) -> InstanceClaim {
    let deadline = Instant::now() + INSTANCE_CLAIM_TIMEOUT;
    let mut waiting = false;
    loop {
        match InstanceLock::try_acquire(paths) {
            Ok(Some(lock)) => {
                if waiting {
                    eprintln!("[instance] 现有 primary 已退出，本进程接管 single-instance lock");
                }
                return InstanceClaim::Primary(lock);
            }
            Ok(None) if restore_requested => {
                // Restart Session 先 spawn 新进程再退出旧进程；新进程必须等锁
                // 释放后成为 primary，不能把 --restore 转发成“再开一个窗口”。
                if Instant::now() >= deadline {
                    crate::logging::restore_stderr();
                    eprintln!(
                        "[instance] --restore 等待现有 primary 退出超时（{}s）；\
                         另一个 Tervo 仍在运行：{}",
                        INSTANCE_CLAIM_TIMEOUT.as_secs(),
                        paths.lock_path().display()
                    );
                    std::process::exit(1);
                }
                if !waiting {
                    crate::logging::restore_stderr();
                    eprintln!("[instance] --restore 等待现有 primary 退出...");
                    waiting = true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let request = new_window_request();
                match crate::instance::forward_open_window(paths, &request) {
                    Ok(()) => {
                        // secondary 立刻退出；恢复原始 stderr，避免 logging tee
                        // 线程来不及刷出，用户/脚本看不到 ack 证据。
                        crate::logging::restore_stderr();
                        eprintln!(
                            "[instance] secondary: open_window ack 收到，退出 0（primary={} cwd={:?}）",
                            paths.socket_path().display(),
                            request.cwd
                        );
                        std::process::exit(0);
                    }
                    Err(error) if error.is_connect_retryable() && Instant::now() < deadline => {
                        // primary 已持锁但 control.sock/token 尚未就绪：连接没有
                        // 建立，重试不会重复开窗。只对 connect/token 失败重试。
                        if !waiting {
                            crate::logging::restore_stderr();
                            eprintln!(
                                "[instance] 已运行的 primary 正在启动，等待 {} ...",
                                paths.socket_path().display()
                            );
                            waiting = true;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(error) => {
                        crate::logging::restore_stderr();
                        eprintln!("[instance] 无法把新窗口转发给已运行的 Tervo primary: {error}");
                        eprintln!(
                            "[instance] 提示：确认 {} 可访问；仅排障/测试可设置 \
                             {}=0 以独立进程启动",
                            paths.socket_path().display(),
                            SINGLE_INSTANCE_ENV
                        );
                        std::process::exit(1);
                    }
                }
            }
            Err(error) => {
                crate::logging::restore_stderr();
                eprintln!(
                    "[instance] 无法初始化单实例控制目录 {}: {error}",
                    paths.dir().display()
                );
                eprintln!(
                    "[instance] 提示：设置 TERVO_INSTANCE_DIR 到可写的私有目录，或仅排障/测试时 \
                     设置 {SINGLE_INSTANCE_ENV}=0 跳过单实例"
                );
                std::process::exit(1);
            }
        }
    }
}

/// primary 的 `open_window` handler。运行在 control 连接线程：
/// 请求转给 UI 线程，等窗口 `show()` 完成后才返回 Ok，server 才回 ack。
#[cfg(unix)]
fn open_window_handler(
) -> impl Fn(OpenWindowRequest) -> Result<(), String> + Send + Sync + 'static {
    move |request| {
        let (tx, rx) = std::sync::mpsc::channel::<Result<WindowId, String>>();
        slint::invoke_from_event_loop(move || {
            let _ = tx.send(secondary::create_inproc_window_for(request));
        })
        .map_err(|error| format!("UI event loop is not available: {error}"))?;
        match rx.recv_timeout(INSTANCE_UI_ACK_TIMEOUT) {
            Ok(Ok(id)) => {
                eprintln!("[instance] open_window -> in-process {id}");
                Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err("timed out waiting for the UI thread to create the window".into()),
        }
    }
}

/// 关闭窗口策略：只剩一个时交给 Slint/主流程退出；否则只移除该窗口。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowCloseAction {
    Remove,
    Quit,
}

fn window_close_action(window_count: usize) -> WindowCloseAction {
    if window_count > 1 {
        WindowCloseAction::Remove
    } else {
        WindowCloseAction::Quit
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 默认 GPU/Skia：用户没有显式设置 SLINT_BACKEND 时请求 winit-skia。
    // `SLINT_BACKEND=winit-software` 仍是显式回退开关；显式设置任何值都以用户为准。
    // Skia 初始化失败时由 render_probe/下面的重试自动回退 software，不黑屏、不崩溃。
    let explicit_backend = std::env::var("SLINT_BACKEND").ok();
    // TERVO_GPU=1 是显式 opt-in：请求 Skia 的 WGPU30 API（require_wgpu_30），
    // 从而在 RenderingSetup 里拿到 WGPU30 device/queue。默认完全不改变原路径。
    let tervo_gpu = gpu_present::requested();
    let tervo_gpu_native = gpu_native::requested();
    // 两个显式开关都需要 Slint 的 WGPU30 device/queue；但只有 TERVO_GPU=1
    // 走 stage 0 CPU framebuffer 上传，TERVO_GPU_NATIVE=1 走 Scene 文字。
    let tervo_gpu_any = tervo_gpu || tervo_gpu_native;
    let mut requested_backend = explicit_backend
        .clone()
        .unwrap_or_else(|| "winit-skia".to_string());
    if tervo_gpu_any && explicit_backend.is_none() {
        requested_backend = "winit-skia".to_string();
    }
    let backend_from_env = explicit_backend.is_some();
    // 没有 DRM 设备时 Skia 可能只能建 software surface，而该 surface 在本机
    // 实测会黑屏（窗口在、内容全黑）。这不是“GPU 初始化失败后回退”而是压根
    // 没有可呈现的 GPU surface，所以默认直接走 winit-software；用户仍可显式
    // `SLINT_BACKEND=winit-skia` 强制尝试（headless/llvmpipe 验证用）。
    let mut auto_software_no_drm = false;
    if !backend_from_env
        && !tervo_gpu_any
        && !std::path::Path::new("/dev/dri").exists()
        && std::env::var_os("TERVO_FORCE_GPU").is_none()
    {
        requested_backend = "winit-software".to_string();
        auto_software_no_drm = true;
    }

    let _ = REQUESTED_BACKEND.set(requested_backend.clone());
    let t_start = Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    if let Some(pos) = argv.iter().position(|a| a == "--sftp-probe") {
        std::process::exit(run_sftp_probe(&argv[pos + 1..]));
    }
    // 版本信息必须在 logging/后端初始化之前返回：`--version` 不能写日志、
    // 不能建窗口，脚本才能在没有 DISPLAY 的打包/安装环境里安全调用。
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        print_version();
        return Ok(());
    }
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    if argv.get(1).map(String::as_str) == Some("diag") {
        return diag::run(&argv[2..]);
    }
    // 插件本地安装/更新/回滚：无 UI，不初始化 Slint，不执行插件代码。
    if argv.get(1).map(String::as_str) == Some("plugins") {
        std::process::exit(plugin_v3::run_cli(&argv[2..]));
    }

    logging::init();
    if argv.iter().any(|a| a == "--diagnose") {
        print_diagnostics();
        return Ok(());
    }

    // 无 UI 的 Tabby 配置迁移：在初始化窗口/后端之前处理，直接退出。
    let arg_value = |flag: &str| -> Option<std::path::PathBuf> {
        argv.iter()
            .position(|a| a == flag)
            .and_then(|i| argv.get(i + 1))
            .map(std::path::PathBuf::from)
    };
    if let Some(path) = arg_value("--export-tabby") {
        let store = profile::ProfileStore::load();
        let yaml = tabby_import::export_yaml(&store);
        std::fs::write(&path, yaml)?;
        println!(
            "[tabby] 导出 {} 个 Profile → {}",
            store.profiles.len(),
            path.display()
        );
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
            }
            Err(e) => {
                eprintln!("[tabby] 导入失败: {e}");
                std::process::exit(1);
            }
        }
    }

    env_logger::init();

    // ---- 单实例 primary/secondary -------------------------------------
    // 越早越好：secondary 必须在 Config/MCP/UI/session marker 之前退出。
    // primary 先持锁；control server 等 Slint 平台初始化、registry 安装后再启动
    // （open_window handler 需要 invoke_from_event_loop）。
    #[cfg(unix)]
    let mut instance_lock = {
        let paths = InstancePaths::from_env();
        if single_instance_disabled() {
            eprintln!(
                "[instance] {SINGLE_INSTANCE_ENV}=0：跳过单实例锁/control server（legacy/排障）"
            );
            None
        } else {
            let restore_requested = argv.iter().any(|arg| arg == "--restore");
            match claim_instance_or_forward(&paths, restore_requested) {
                InstanceClaim::Primary(lock) => {
                    eprintln!(
                        "[instance] primary lock={} pid={}",
                        paths.lock_path().display(),
                        std::process::id()
                    );
                    Some(lock)
                }
                InstanceClaim::Standalone => None,
            }
        }
    };
    #[cfg(not(unix))]
    let mut instance_lock: Option<()> = None;

    // 必须在任何 Slint backend 初始化之前设置；放在 logging::init 之后是为了让
    // 日志里的 SLINT_BACKEND 能区分“程序默认 GPU”与“用户显式设置”。
    // TERVO_GPU=1 时不能写 SLINT_BACKEND：BackendSelector::select() 会读它，
    // 而我们要用 require_wgpu_30() 明确选中 Skia 的 WGPU30 API。
    if !backend_from_env && !tervo_gpu_any {
        std::env::set_var("SLINT_BACKEND", &requested_backend);
    }
    if tervo_gpu_any && !backend_from_env {
        eprintln!(
            "[gpu] {}：请求 Slint WGPU30 显示链路（BackendSelector.require_wgpu_30；\
             失败会自动回退 CPU/Skia，默认路径不受影响）",
            if tervo_gpu_native {
                "TERVO_GPU_NATIVE=1（Frame -> Scene -> WGPU 文字/背景）"
            } else {
                "TERVO_GPU=1（stage 0 CPU framebuffer 纹理）"
            }
        );
    } else if backend_from_env {
        eprintln!("[render] requested_backend={requested_backend}（显式设置 SLINT_BACKEND）");
    } else if auto_software_no_drm {
        eprintln!(
            "[render] requested_backend=winit-software（默认 GPU/Skia，但未检测到 /dev/dri；\
             为避免 Skia software surface 黑屏而自动选软件；设置 SLINT_BACKEND=winit-skia \
             可强制尝试，headless 不能验证真实 GPU 性能）"
        );
    } else {
        eprintln!("[render] requested_backend={requested_backend}（默认 GPU/Skia；可用 SLINT_BACKEND 覆盖）");
    }
    let cfg = Config::load();
    // MCP 是内嵌功能：默认开启，启动时创建 0600 socket + state token（0600）。
    // 这里只初始化运行期权限/路径；listener 在首帧后由 supervisor 创建。
    let mcp_runtime = mcp::init(&cfg);
    let mut opts = parse_args(&cfg);
    // `--window` 兼容：没有运行中的 primary 时它就是一个普通 primary 窗口，
    // 必须正常管理 session/marker，而不是继续沿用“子窗口”语义。
    #[cfg(unix)]
    if instance_lock.is_some() && opts.window {
        eprintln!("[instance] --window: 没有运行中的 primary，本进程作为 primary 启动");
        opts.window = false;
    }
    // 启动自动恢复（默认关）。开启且有快照时沿用快照网格打开窗口；
    // 没有快照/解析失败时完全保持旧的启动行为（新建单 Tab）。
    // 子窗口进程（--window）不参与自动恢复/退出快照：session.json 只有一份，
    // 让任意子窗口覆盖主窗口布局会在下次启动恢复错窗口。
    let auto_restore = cfg.restore_session_on_startup && !opts.restore && !opts.window;
    // The marker is written before any UI starts and removed only by a normal
    // exit. A leftover marker therefore means this is a crash/kill recovery.
    let unclean = !opts.window && session::was_unclean();
    if unclean {
        eprintln!(
            "[session] 检测到上次异常退出；{}",
            if auto_restore {
                "尝试恢复最近 autosave"
            } else {
                "restore_session_on_startup=false，保留新启动布局"
            }
        );
    }
    if !opts.window {
        if let Err(error) = session::mark_running() {
            eprintln!("[session] 写 running marker 失败: {error}");
        }
    }
    if auto_restore {
        // v1 的字符网格没有 v2 对应字段，只用于沿用旧启动尺寸；多窗口 v2
        // 返回 None（几何在 windows[] 里）。
        if let Some((cols, rows)) = session::legacy_grid() {
            if cols > 0 && rows > 0 {
                opts.cols = cols;
                opts.rows = rows;
            }
        }
    }
    // 多窗口 v2 恢复计划：`--restore` 显式请求或配置开启。子窗口进程
    // （--window）永不恢复，避免递归建窗。
    let restore_requested = !opts.window && (opts.restore || auto_restore);
    let restore_snapshot = if restore_requested {
        session::load_v2()
    } else {
        None
    };
    let restore_windows = restore_snapshot
        .as_ref()
        .map(|snap| snap.restorable_windows())
        .unwrap_or_default();
    if let Some(snap) = &restore_snapshot {
        eprintln!(
            "[session] 读取 v{} 快照：{} 个窗口，其中 {} 个可恢复{}",
            snap.version,
            snap.windows.len(),
            restore_windows.len(),
            if restore_windows.is_empty() {
                "（回退新 Tab）"
            } else {
                ""
            }
        );
    }
    let primary_restore = restore_windows.first().cloned();
    if opts.window {
        // 子窗口进程：仅日志区分。这里刻意不做任何“再建窗口”的动作，避免递归启动。
        eprintln!(
            "[ui] new window process pid={} (--window)",
            std::process::id()
        );
    }

    // 插件发现提前到主题加载之前：声明式 Theme Pack（可选）要并入主题列表；
    // 此时只读 manifest，不启动任何插件进程。
    let plugins = plugin_v3::PluginStore::load();

    // 内置 + 用户自定义主题 + 插件声明的主题；加载失败只记录，不阻塞启动。
    let mut registry = ThemeRegistry::load(&ThemeRegistry::default_dir());
    for (plugin_name, path) in plugins.theme_contributions() {
        match themes::load_plugin_theme(&path, &plugin_name) {
            Ok(theme) => {
                eprintln!(
                    "[theme] 插件 {} 提供主题 {}（{}）",
                    plugin_name,
                    theme.id,
                    path.display()
                );
                registry.add_custom(theme);
            },
            Err(e) => eprintln!(
                "[theme] 插件 {} 的主题 {} 加载失败: {e}",
                plugin_name,
                path.display()
            ),
        }
    }
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
    // v2 快照有像素几何时优先用它开窗；v1/未知几何回退 --cols/--rows。
    let primary_geometry = primary_restore
        .as_ref()
        .map(|window| window.geometry)
        .filter(|geometry| geometry.is_known());
    let (win_w, win_h) = match primary_geometry {
        Some(geometry) => (geometry.w as f32, geometry.h as f32),
        None => (
            opts.cols as f32 * cell.0 as f32,
            opts.rows as f32 * cell.1 as f32 + TABBAR_H,
        ),
    };
    let content = content_rect(win_w, win_h, sidebar_width_of(&cfg));
    let t_font = t_start.elapsed();

    // 后端初始化失败时显式重试软件渲染。注意：大多数 GPU/renderer 失败会被
    // Slint winit 后端内部按 Skia→software 的顺序处理（窗口仍能创建）；这里兜底
    // “连 winit-skia backend 都建不起来”的情况，避免直接退出/黑屏。
    // 自测钩子（仅 debug）：注入一次 UI 初始化失败，验证“失败→回退 software→
    // 窗口正常”的路径。release 构建里该分支恒为 false。
    #[cfg(debug_assertions)]
    let force_init_fail = std::env::var_os("TERVO_TEST_FORCE_UI_INIT_FAIL").is_some();
    #[cfg(not(debug_assertions))]
    let force_init_fail = false;

    // TERVO_GPU=1 / TERVO_GPU_NATIVE=1：在任何 Slint UI 初始化之前明确要求 WGPU30。
    // require_wgpu_30 请求 WGPU30 API；adapter/device 不可用时 select() 返回错误，
    // 这里 exec 回退 CPU/Skia（默认路径），不会黑屏硬撑。
    if tervo_gpu_any && !backend_from_env && !force_init_fail {
        match slint::BackendSelector::new()
            .require_wgpu_30(slint::wgpu_30::WGPUConfiguration::default())
            .select()
        {
            Ok(()) => eprintln!(
                "[gpu] BackendSelector::require_wgpu_30(...).select() = OK（等待 RenderingSetup 的 device/queue）"
            ),
            Err(err) => exec_cpu_fallback(
                &requested_backend,
                &format!("require_wgpu_30/select: {err}"),
            ),
        }
    }
    let first_ui = if force_init_fail {
        eprintln!("[render] 测试注入：强制首次 UI 初始化失败（TERVO_TEST_FORCE_UI_INIT_FAIL）");
        Err(slint::PlatformError::Other(
            "test-injected UI init failure".into(),
        ))
    } else {
        MainWindow::new()
    };

    let (ui, backend_fell_back) = match first_ui {
        Ok(ui) => (ui, false),
        Err(err) if tervo_gpu_any => {
            // WGPU platform 已经 select；再改 SLINT_BACKEND 重试无效，直接 exec 回退。
            exec_cpu_fallback(&requested_backend, &format!("MainWindow::new: {err}"))
        }
        Err(err) if requested_backend != "winit-software" => {
            eprintln!(
                "[render] requested_backend={requested_backend} 初始化失败: {err}；\
                 自动回退 SLINT_BACKEND=winit-software"
            );
            std::env::set_var("SLINT_BACKEND", "winit-software");
            match MainWindow::new() {
                Ok(ui) => (ui, true),
                Err(err2) => {
                    eprintln!(
                        "[render] winit-software 回退也失败: {err2}；\
                         这通常是没有可用的 X11/Wayland display，而不是 GPU 问题"
                    );
                    return Err(err2.into());
                }
            }
        }
        Err(err) => {
            eprintln!("[render] requested_backend={requested_backend} 初始化失败: {err}");
            return Err(err.into());
        }
    };
    // 在 show/首帧之前安装：读取实际渲染器（Skia/GL vs winit-software）和 GL 信息。
    let window_id = WindowId::FIRST;
    render_probe::install_for(
        window_id,
        &ui.window(),
        &requested_backend,
        backend_fell_back,
    );
    // --title 以前只解析不使用，窗口标题永远硬编码；现在透传到 Slint 属性。
    ui.set_window_title(opts.title.as_str().into());
    // 工作区侧栏（ACT-046）：默认关闭时 `sidebar-width` 为 0，内容区与 0.5.0 一致。
    ui.set_sidebar_visible(cfg.sidebar_visible);
    ui.set_sidebar_width(sidebar_width_of(&cfg));
    ui.set_cfg_sidebar_visible(cfg.sidebar_visible);
    // 回退开关：TERVO_SETTINGS_LEGACY=1 时设置页退回居中 modal（同一套控件，
    // 只是不再占用 Tab/内容区）。默认走新的 Settings Tab。
    let settings_legacy = std::env::var("TERVO_SETTINGS_LEGACY")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    ui.set_settings_legacy(settings_legacy);
    eprintln!("[ui] settings ui={}", if settings_legacy { "legacy-modal" } else { "tab" });
    ui.window().set_size(slint::PhysicalSize::new(win_w as u32, win_h as u32));

    // ---- 每用户共享服务（只创建一次，窗口只拿句柄）----
    let mut shared_cfg = cfg.clone();
    shared_cfg.theme = theme_selection.clone();
    shared_cfg.cols = opts.cols;
    shared_cfg.rows = opts.rows;
    shared_cfg.font_size = opts.font_size;
    shared_cfg.scrollback = opts.scrollback;
    if opts.shell.is_some() {
        shared_cfg.shell = opts.shell.clone();
    }
    let shared = Rc::new(SharedServices::new(
        shared_cfg,
        registry,
        plugins,
        vault::Vault::open_default(),
        profile::ProfileStore::load(),
        mcp_runtime,
        runtime::SessionService::new(!opts.window),
    ));
    let registry_cell = Rc::new(RefCell::new(WindowRegistry::new(shared.clone())));
    runtime::install_registry(registry_cell.clone());
    // 恢复期间先抑制 autosave：逐窗口建 tab 的过程中写到一半的布局不能
    // 覆盖磁盘上原快照；全部窗口就绪后再开启。
    shared.session.with_mut(|session| {
        session.restoring = restore_requested;
        session.last_snapshot = None;
    });
    shared.start_session_autosave();

    // 模型/AppState 构造已抽到 `build_window_state`，单窗口与第二窗口共用同一条路径。
    let state = build_window_state(
        &ui,
        shared.clone(),
        window_id,
        &opts,
        fonts,
        cell,
        win_w,
        win_h,
        content,
        initial_theme.clone(),
    );
    {
        let runtime = WindowRuntime::new(
            window_id,
            ui.clone_strong(),
            state.clone(),
            opts.title.clone(),
        );
        let mut reg = registry_cell.borrow_mut();
        reg.insert(runtime).expect("first window id must be free");
        // 侧栏可见时启动就把 Profiles 候选项填好（否则要等点 "+" 才有内容）。
        if state.borrow().cfg.sidebar_visible {
            let mut st = state.borrow_mut();
            rebuild_profile_choices(&mut st);
        }
        // 异常退出提示（ACT-036 的“恢复提示”第一步）：只提示，不自动改布局。
        if unclean {
            let st = state.borrow();
            let lang_zh = st.cfg.lang() == "zh";
            let msg = if auto_restore {
                if lang_zh { "上次异常退出，已按 autosave 恢复布局" } else { "Recovered layout from the last unclean exit" }
            } else if lang_zh {
                "上次异常退出；启动时恢复未开启"
            } else {
                "Last exit was unclean; startup restore is off"
            };
            st.ui.set_status_items(ModelRc::from(Rc::new(VecModel::from(
                vec![slint::SharedString::from(msg)],
            ))));
        }
        reg.set_active(window_id);
    }
    // Slint 平台/registry 已就绪：现在启动 control server。handler 只把请求
    // 转给 UI 线程，窗口真正 show() 后才 ack secondary。
    #[cfg(unix)]
    let _primary_server = match instance_lock.take() {
        Some(lock) => match PrimaryServer::start_with_lock(lock, open_window_handler()) {
            Ok(server) => {
                eprintln!(
                    "[instance] primary control server listening socket={} token={}",
                    server.paths().socket_path().display(),
                    server.paths().token_path().display()
                );
                Some(server)
            }
            Err(error) => {
                eprintln!("[instance] primary control server 启动失败: {error}");
                return Err(error.into());
            }
        },
        None => None,
    };
    // 插件宿主是共享服务：只启动一次，唤醒信号使用 primary 窗口的 app_wake。
    shared.plugins.set_activity_wake(state.borrow().app_wake.clone());
    shared.plugins.with_mut(|plugins| plugins.start_process_plugins());

    register_window_callbacks(&ui, state.clone(), window_id);

    // Quake 全局热键是进程级 service：只注册一次，不随窗口数量增长。
    if let Err(e) = quake::spawn_listener(&cfg.quake_hotkey, || {
        let _ = slint::invoke_from_event_loop(toggle_quake);
    }) {
        eprintln!("[quake] {e}");
    }


    // ---- OSC 标题轮询 ----
    let poll_timer = slint::Timer::default();
    let poll_state = state.clone();
    poll_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(400),
        move || poll_events_for(poll_state.clone()),
    );

    // Session autosave 不在这里创建：它归 SharedServices，primary 关闭后仍要
    // 为剩余窗口写盘。启动见 `shared.start_session_autosave()`。

    // 多 Tab + 窗口 resize 会反复重建 pane 帧缓冲/alacritty 行存储；glibc 默认把
    // 已 free 的几 MB 堆块留在 arena 不归还，RSS 曲线会持续缓慢上涨后才平台。
    // 低频显式归还已释放页（只影响 allocator，不触碰任何仍存活的终端状态）。
    #[cfg(target_os = "linux")]
    {
        let trim_timer = slint::Timer::default();
        trim_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(3),
            || unsafe {
                libc::malloc_trim(0);
            },
        );
        std::mem::forget(trim_timer);
    }

    // 唤醒计数诊断：空闲时 core/app 唤醒数不应增长（只有 400ms 兜底在跑）。
    // 只在显式设置时启动，正常使用不产生额外 timer/日志。
    if std::env::var_os("TERVO_WAKE_STATS").is_some() {
        let stats_timer = slint::Timer::default();
        stats_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(5),
            || {
                eprintln!(
                    "[wake] ts_ms={} core_event={} app_event={} periodic_400ms={}",
                    logging::now_ms(),
                    CORE_EVENT_WAKES.load(Ordering::Relaxed),
                    APP_EVENT_WAKES.load(Ordering::Relaxed),
                    PERIODIC_POLLS.load(Ordering::Relaxed),
                );
            },
        );
        std::mem::forget(stats_timer);
    }

    // DECSET 1004：窗口焦点变化及时同步给 TUI（50ms 轮询）。
    let focus_timer = slint::Timer::default();
    let focus_state = state.clone();
    focus_timer.start(
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
                sync_terminal_focus(st, win);
                if win && !was_focused {
                    // 输入法在失焦期间可能丢弃旧 spot；重新获得焦点时强制补发。
                    st.ime_spot_dirty = true;
                }
                // dsh 侧栏可能只移动终端光标、不产生内容重绘；独立轮询光标，
                // 不能只等 redraw_visible 才更新 IME spot。
                update_ime_spot(st);
            });
        },
    );
    // 计时器属于窗口生命周期：关掉 primary 而 second 仍存活时，必须能随
    // WindowRuntime 一起 drop，不能只留在 main 的局部变量里。
    if let Some(rt) = registry_cell.borrow().get(window_id) {
        rt.keep_timer(poll_timer);
        rt.keep_timer(focus_timer);
    }

    // 设置开关的初始状态（所有窗口共用同一份配置）。
    with_app(init_window_ui);
    // ---- 外部 config.toml 热更新监听 ----
    // 基线在 watcher 启动时采样，不主动触发一次；只有启动之后的内容变化才套用。
    let cfg_path = Config::path();
    let watcher_path = cfg_path.clone();
    match config_watch::ConfigWatcher::start(cfg_path.clone(), move |cfg, hash| {
        let check_path = watcher_path.clone();
        if slint::invoke_from_event_loop(move || {
            // 事件排队期间设置页可能又保存过：以磁盘当前内容为准，旧事件直接丢弃。
            if !config_watch::file_hash_matches(&check_path, hash) {
                eprintln!("[config] 热更新事件已过期（文件又被修改），跳过旧配置");
                return;
            }
            // 配置是共享服务：先替换唯一 cfg，再逐窗口应用。不能只 with_app
            // 改“当前窗口”，否则另一个窗口看不到热更新。
            let Some(shared) = runtime::registry().map(|reg| reg.borrow().shared()) else {
                return;
            };
            let old = shared.cfg.with(|current| current.clone());
            if old == cfg {
                return;
            }
            shared.cfg.replace(cfg.clone());
            runtime::with_all_windows(|_id, st| {
                apply_external_config_from(st, old.clone(), cfg.clone());
            });
        })
        .is_err()
        {
            eprintln!("[config] watcher 回调投递失败（事件循环已退出）");
        }
    }) {
        Ok(watcher) => {
            // 监听器归 SharedServices：primary 关闭后只要还有窗口，热更新继续。
            shared.set_config_watcher(watcher);
            eprintln!(
                "[config] 热更新监听已启动: {} ({}ms 轮询，mtime/hash)",
                cfg_path.display(),
                config_watch::POLL_INTERVAL.as_millis()
            );
        }
        Err(e) => eprintln!("[config] 热更新监听启动失败（配置仍可用）: {e}"),
    }

    // 启动事件：应用状态就绪后、首个 Tab 创建前后台分发；失败不影响主流程。
    with_app(|st| {
        dispatch_plugin_event(st, plugin_v3::PluginEvent::Startup, plugin_v3::startup_payload());
    });

    // ---- 首个标签页 / 会话恢复 ----
    let restored = with_app(|st| {
        // 手动 Restart Session（--restore）或配置开启的启动自动恢复；
        // 没有快照/全部恢复失败时走下面的 new_tab_inner，保证总有一个可用 Tab。
        let restored = primary_restore
            .as_ref()
            .is_some_and(|window| restore_window_snapshot(st, window));
        if !restored {
            // 首次启动也必须尊重 default_profile_id；之前只有 Ctrl+T 走默认 Profile，
            // 导致 SSH 默认 Profile 启动后落到本地 shell。
            new_tab_with_default(st);
        }
        rebuild_tabs(st);
        restored
    })
    .unwrap_or(false);
    if restored {
        let (tabs, panes) = with_app(|st| {
            (
                st.ws.tabs.len(),
                st.ws.tabs.iter().map(|t| t.panes().len()).sum::<usize>(),
            )
        })
        .unwrap_or((0, 0));
        eprintln!("[session] 已按快照恢复 primary：{tabs} 个标签 / {panes} 个 pane");
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
    // 位置要在 map 前尽量设一次（部分 WM 在 map 时重新居中）；show 之后再
    // 按真实客户端尺寸/最大化状态校正。
    if let Some(window) = &primary_restore {
        if window.geometry.is_known() {
            ui.window().set_position(slint::PhysicalPosition::new(
                window.geometry.x,
                window.geometry.y,
            ));
        }
    }
    ui.show()?;
    if let Some(window) = &primary_restore {
        restore_window_geometry(&ui, window_id, window);
    }
    // 先按 winit 当前状态显示图标；X11 状态机在拿到 raw handle 后启动，
    // 再用 EWMH 初始查询校正。不要用窗口几何/_NET_WORKAREA 推断最大化。
    let initial_maximized =
        ui.window().is_maximized() || primary_restore.as_ref().is_some_and(|w| w.maximized);
    ui.set_window_maximized(initial_maximized);
    with_app(|st| st.window_maximized = initial_maximized);
    start_maximize_when_window_ready(&ui, window_id, 0);
    // 无头验收钩子：TERVO_GPU_SNAPSHOT=<前缀> 时定时 take_snapshot 写 PPM 并打像素统计。
    install_gpu_snapshot_hook(&ui);
    // RenderingSetup 可能首帧后才给出 device/queue；GPU 就绪后主动重绘一次。
    // Stage 0 和 Scene 文字链路都需要这一脚，否则首帧以后没有输入时画面
    // 会一直停在 CPU 初帧。
    if tervo_gpu_any {
        // Slint's first WGPU present can happen after the first terminal frame
        // (PTY output/timer). An imported texture that has not been sampled by
        // a WGPU frame yet used to snapshot as an empty pane until the next
        // redraw, so nudge several times around renderer setup/first present.
        for delay_ms in [180u64, 800, 1500, 2100] {
            slint::Timer::single_shot(Duration::from_millis(delay_ms), || {
                if gpu_present::ready() || gpu_native::ready() {
                    redraw_all();
                }
            });
        }
    }
    // Slint 1.18 的 winit 后端不投递外部文件拖放，用 winit 事件桥接补齐。
    install_native_window_events(&ui, window_id);
    // 恢复其余窗口。产品恢复路径不依赖 TERVO_MULTI_WINDOW_INPROC：v2 快照里
    // 有几个可恢复窗口就建几个。每个窗口的 Tab 恢复失败只回退该窗口的新 Tab。
    //
    // 延迟 600ms 再建第二个原生窗口：Slint 1.18 的 WinitWindowAccessor 在
    // 多窗口启动阶段可能还没绑定到正确的 winit window；primary 的 maximize
    // 状态机（50ms 重试拿 X11 handle）必须先完成，否则会 attach 到错误的
    // xid。这个延迟只影响启动恢复，不影响单窗口路径。
    if restore_requested {
        let shared = shared.clone();
        let restore_windows = restore_windows.clone();
        slint::Timer::single_shot(Duration::from_millis(600), move || {
            let mut restored_extra = 0usize;
            for window in restore_windows.iter().skip(1) {
                match secondary::create_inproc_window_restored(window) {
                    Ok(id) => {
                        restored_extra += 1;
                        eprintln!("[session] 恢复窗口 {} -> {id}", window.id);
                    },
                    Err(error) => eprintln!(
                        "[session] 窗口 {} 创建失败（跳过，不整机失败）: {error}",
                        window.id
                    ),
                }
            }
            // 快照只消费一次：恢复成功就删掉，避免下次手动 --restore 又拉旧布局。
            // 恢复期间 autosave 被抑制；这里放开后 2s 内会用当前可用布局重写。
            if restored || restored_extra > 0 {
                session::clear();
            }
            shared.session.with_mut(|session| session.restoring = false);
            eprintln!(
                "[session] 多窗口恢复完成：{}/{} 个窗口",
                restored as usize + restored_extra,
                restore_windows.len()
            );
        });
    } else {
        shared.session.with_mut(|session| session.restoring = false);
    }
    // e2e/证据开关：feature flag + 显式 SECOND 时启动即建第二个窗口。
    // 同样延迟 600ms，让 primary 的 maximize 状态机先绑定正确的 X11 handle。
    if restore_windows.is_empty()
        && std::env::var_os("TERVO_MULTI_WINDOW_SECOND").is_some()
    {
        slint::Timer::single_shot(Duration::from_millis(600), move || {
            match secondary::create_inproc_window() {
                Ok(id) => eprintln!("[ui] startup second window id={id}"),
                Err(error) => eprintln!("[ui] startup second window failed: {error}"),
            }
        });    }
    // GPU 自检兜底：Skia 软件 surface 会“窗口正常但全黑”，而 Slint 的
    // set_rendering_notifier 在 software surface 上不会给出 NativeOpenGL/WGPU
    // 回调。给 GPU 首帧 1s 初始化时间，仍无 GPU API 就 exec 回退软件后端。
    if requested_backend != "winit-software" && std::env::var_os("TERVO_RENDER_FALLBACK").is_none()
    {
        let requested = requested_backend.clone();
        slint::Timer::single_shot(Duration::from_millis(1000), move || {
            // frames==0 才是“Skia software surface 黑屏”的特征；wgpu 可能已出帧但
            // 因未启用 unstable-wgpu-30 而拿不到 GraphicsAPI 回调。
            if render_probe::notifier_registered()
                && !render_probe::gpu_confirmed()
                && render_probe::frames() == 0
            {
                exec_software_fallback(&requested);
            }
        });
    }
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
                Some(xconn::CornerState {
                    maximized: false,
                    fullscreen: true,
                }),
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
        std::thread::Builder::new()
            .name("icon".into())
            .spawn(move || {
                // 等 Slint 完成首轮 window properties，避免被它的空 icon 覆盖
                std::thread::sleep(std::time::Duration::from_millis(800));
                match xconn::set_window_icon(ICON_RGBA, 64, &title_for_icon) {
                    Ok(win) => eprintln!("[ui] _NET_WM_ICON 已设置 (win=0x{win:x})"),
                    Err(e) => eprintln!("[ui] 设置窗口图标失败: {e}"),
                }
            })?;
    }

    redraw_all();
    diag::start_control_socket_with_control(live_diag_snapshot, live_control, shared.mcp.clone());
    schedule_mcp_status_refresh(window_id);
    // 之后 primary 窗口的生命周期只由 WindowRegistry/AppState 持有；这样
    // 关闭 primary 而 second 仍存活时，MainWindow/PTY/GPU 能真正 drop。
    drop(ui);
    let t_show = t_start.elapsed();

    if opts.exit_after_first {
        std::thread::Builder::new()
            .name("first-frame".into())
            .spawn(move || {
                let Some(watcher) = DamageWatcher::start(&opts.title, Duration::from_secs(30))
                else {
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
        let echo = with_app(|st| st.echo.clone()).unwrap_or_else(|| Arc::new(AtomicUsize::new(0)));
        std::thread::Builder::new()
            .name("latency-probe".into())
            .spawn(move || run_latency_probe(n, echo, TITLE.to_string()))?;
    }
    if let Some(secs) = opts.fps {
        std::thread::Builder::new()
            .name("fps-probe".into())
            .spawn(move || run_fps_probe(secs, TITLE.to_string()))?;
    }

    // 主流程不再持有 AppState/Timer 的所有权；它们归 WindowRegistry。
    drop(state);
    let run_result = slint::run_event_loop();
    // 正常退出（最后一个窗口关闭/最后一个 Tab 关闭）：写一次当前多窗口布局。
    // primary 提前关闭时 autosave 已由 SharedServices 继续运行，这里只做最终落盘。
    shared.stop_session_autosave();
    runtime::save_session_on_exit();
    let restarting = shared.session.with(|session| session.restarting);
    if !opts.window && !restarting {
        session::mark_clean();
    }
    stop_auto_watcher();
    // 显式停止并 join 轮询线程，避免退出阶段还有回调投递。
    shared.stop_config_watcher();
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

        let Some(t0) = inj.tap_char(MARKER) else {
            break;
        };
        match wait_echo(&watcher, &echo, 1, Duration::from_millis(1500)) {
            Some(t) => {
                if i >= WARMUP {
                    samples.push(t.duration_since(t0));
                }
            }
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

fn wait_echo(
    watcher: &DamageWatcher,
    echo: &AtomicUsize,
    target: usize,
    timeout: Duration,
) -> Option<Instant> {
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
    let fps = if stats.mean_us > 0.0 {
        1e6 / stats.mean_us
    } else {
        0.0
    };
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
    fn toggle_sidebar_math_roundtrips_and_keeps_window_width() {
        let mut cfg = Config::default();
        let start = content_rect(1000.0, 800.0, sidebar_width_of(&cfg));
        // 开：内容区让出 240，窗口逻辑宽度不变
        let open = toggle_sidebar_math(start, &mut cfg);
        assert!(cfg.sidebar_visible);
        assert_eq!((open.w, open.h), (760.0, 800.0 - TABBAR_H));
        // 关：回到原样
        let closed = toggle_sidebar_math(open, &mut cfg);
        assert!(!cfg.sidebar_visible);
        assert_eq!((closed.w, closed.h), (start.w, start.h));
    }

    #[test]
    fn content_rect_reserves_sidebar_only_when_visible() {
        let mut cfg = Config::default();
        assert_eq!(sidebar_width_of(&cfg), 0.0, "默认关闭时侧栏不占位");
        let r = content_rect(1000.0, 800.0, sidebar_width_of(&cfg));
        assert_eq!((r.w, r.h), (1000.0, 800.0 - TABBAR_H));

        cfg.sidebar_visible = true; // 默认宽度 240
        let r = content_rect(1000.0, 800.0, sidebar_width_of(&cfg));
        assert_eq!((r.w, r.h), (760.0, 800.0 - TABBAR_H));

        // 极窄窗口不能出现 0/负宽度
        let r = content_rect(100.0, 800.0, 240.0);
        assert_eq!(r.w, 1.0);
    }

    use super::*;

    #[test]
    fn config_reload_summary_separates_hot_new_pane_and_restart_fields() {
        let old = Config::default();
        let mut next = old.clone();
        next.theme = "latte".into();
        next.font_size = 18.0;
        next.background_opacity = 0.5;
        next.visual_bell = false;
        next.sixel = false;
        next.cursor_shape = "ibeam".into();
        next.bold_is_bright = false;
        next.right_click = "paste".into();
        next.tabbar_position = "bottom".into();
        let summary = reconfig_summary(&old, &next);
        for field in [
            "theme",
            "font_size",
            "background_opacity",
            "cursor_shape",
            "bold_is_bright",
            "right_click",
            "tabbar_position",
        ] {
            assert!(
                summary.hot.contains(&field),
                "missing hot field {field}: {summary:?}"
            );
        }
        assert!(summary.hot.contains(&"visual_bell"));
        assert!(summary.hot.contains(&"sixel"));
        assert!(summary.new_pane.is_empty());
        assert!(summary.restart.is_empty());
    }

    #[test]
    fn image_hit_survives_visible_texture_cache_eviction() {
        let pixels: Arc<[u8]> = Arc::from(vec![255, 0, 0, 255]);
        let source = ImageSource {
            source_id: 42,
            width: 1,
            height: 1,
            data: pixels,
        };
        let cache_slot = source.source_id;
        // 模拟 Slint 可见纹理淘汰；命中仍按 placement 找 source，
        // source 数据不再依赖 cache 是否仍持有它。
        let inline_cache: HashMap<u64, slint::Image> = HashMap::new();
        assert!(!inline_cache.contains_key(&cache_slot));
        assert_eq!(source.source_id, 42);
        assert_eq!(source.data.as_ref(), &[255, 0, 0, 255]);
    }

    #[test]
    fn config_reload_summary_marks_scrollback_as_new_pane_and_columns_as_restart() {
        let old = Config::default();
        let mut next = old.clone();
        next.scrollback = 5000;
        next.scrollback_unlimited = true;
        next.shell = Some("/bin/zsh".into());
        next.default_profile_id = 7;
        next.cols = 100;
        next.quake_hotkey = "F11".into();
        let summary = reconfig_summary(&old, &next);
        assert!(
            summary.hot.is_empty(),
            "scrollback/shell/cols 不能假装热更: {summary:?}"
        );
        assert_eq!(
            summary.new_pane,
            vec!["scrollback", "shell", "default_profile_id"],
            "这些字段只影响之后新建的 pane/Tab"
        );
        assert_eq!(summary.restart, vec!["cols/rows", "quake_hotkey"]);
        assert!(reconfig_summary(&old, &old).hot.is_empty());
    }

    #[test]
    fn config_reload_summary_covers_bell_scroll_sixel_and_hotkeys() {
        let old = Config::default();
        let mut next = old.clone();
        next.audible_bell = true;
        next.scroll_on_output = true;
        next.scroll_on_keystroke = false;
        next.scrollbar_policy = "never".into();
        next.sixel_scrolling = false;
        next.iip = false;
        next.desktop_notifications = false;
        next.hotkeys.push(commands::HotkeyOverride {
            command: "tab.new".into(),
            chord: "ctrl+shift+n".into(),
        });
        let summary = reconfig_summary(&old, &next);
        for field in [
            "audible_bell",
            "scroll_on_output",
            "scroll_on_keystroke",
            "scrollbar_policy",
            "sixel_scrolling",
            "iip",
            "desktop_notifications",
            "hotkeys",
        ] {
            assert!(summary.hot.contains(&field), "missing {field}: {summary:?}");
        }
    }

    #[test]
    fn mcp_settings_are_hot_reload_and_indexed_bilingually() {
        let old = Config::default();
        let mut next = old.clone();
        next.mcp_enabled = false;
        next.mcp_read = false;
        next.mcp_input = true;
        next.mcp_tabs = true;
        next.mcp_destructive = true;
        let summary = reconfig_summary(&old, &next);
        for field in [
            "mcp_enabled",
            "mcp_read",
            "mcp_input",
            "mcp_tabs",
            "mcp_destructive",
        ] {
            assert!(summary.hot.contains(&field), "missing {field}: {summary:?}");
        }
        assert!(summary.new_pane.is_empty(), "{summary:?}");
        assert!(summary.restart.is_empty(), "MCP 权限不能要求重启: {summary:?}");

        // 搜索索引：中英都命中 Privacy/MCP，且分类下标是 9。
        let en = settings_search_hits("mcp", "en");
        assert!(
            en.iter()
                .any(|hit| hit.cat == 9 && hit.line.contains("Privacy · MCP")),
            "{en:?}"
        );
        let zh = settings_search_hits("mcp", "zh");
        assert!(
            zh.iter()
                .any(|hit| hit.cat == 9 && hit.line.contains("隐私 · MCP")),
            "{zh:?}"
        );
        assert!(en.iter().all(|hit| hit.section == "mcp"));
        assert_eq!(settings_cat_label(9, "en"), "Privacy");
        assert_eq!(settings_cat_label(9, "zh"), "隐私");

        // 状态文案双语：关闭/开启都可读且不 panic。
        let off = McpPermissions {
            enabled: false,
            read: true,
            input: false,
            tabs: false,
            destructive: false,
        };
        assert!(mcp_status_text("en", &off, false, true, None, false, false).contains("Off"));
        assert!(mcp_status_text("zh", &off, false, true, None, false, false).contains("已关闭"));
        let read_only = McpPermissions::default();
        let status = mcp_status_text("zh", &read_only, true, true, None, false, false);
        assert!(status.contains("读取"), "{status}");
        assert!(status.contains("socket 已就绪"), "{status}");
    }

    #[test]
    fn wm_resize_watchdog_waits_for_first_configure() {
        let ms = Duration::from_millis;
        // 刚按下/不到首个 configure 的宽限：绝不回退。
        assert!(!wm_resize_timed_out(false, None, ms(0)));
        assert!(!wm_resize_timed_out(false, None, ms(399)));
        // 一直没 configure 才回退（真·WM 没接管）。
        assert!(wm_resize_timed_out(false, None, ms(400)));
        assert!(wm_resize_timed_out(false, Some(ms(900)), ms(900)));
    }

    #[test]
    fn wm_resize_watchdog_does_not_fire_while_configures_keep_coming() {
        let ms = Duration::from_millis;
        // 收到过 configure 后，即使按下已 500ms，只要最近 800ms 内还有 configure
        // 就认为 WM 仍在驱动（GPU 首帧排队时 configure 可能晚到）。
        assert!(!wm_resize_timed_out(true, Some(ms(0)), ms(500)));
        assert!(!wm_resize_timed_out(true, Some(ms(500)), ms(900)));
        assert!(!wm_resize_timed_out(true, Some(ms(799)), ms(2000)));
        // 真的停了（>800ms 没有 configure）才允许回退。
        assert!(wm_resize_timed_out(true, Some(ms(801)), ms(900)));
        assert!(wm_resize_timed_out(true, None, ms(900)));
    }

    #[test]
    fn resize_redraw_is_immediate_on_shrink_and_first_event() {
        // 缩小时旧帧右上角按钮会被裁，必须立即重绘。
        assert!(should_redraw_resize_immediately(1200.0, 1100.0, true));
        // 首次 configure 也要立即绘制。
        assert!(should_redraw_resize_immediately(1200.0, 1200.0, false));
        // 放大没有裁切风险，可合并到 16ms 定时器。
        assert!(!should_redraw_resize_immediately(1200.0, 1300.0, true));
    }

    #[test]
    fn color_scheme_has_priority() {
        // color-scheme 有明确答案时，忽略 gtk-theme 与 COLORFGBG。
        assert!(scheme_from_outputs("'prefer-dark'", "'Yaru'", Some("0;15")));
        assert!(!scheme_from_outputs(
            "'prefer-light'",
            "'Yaru-dark'",
            Some("15;0")
        ));
        assert!(!scheme_from_outputs(
            "'default'",
            "'Yaru-dark'",
            Some("15;0")
        ));
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
            Theme::ALL
                .iter()
                .copied()
                .map(crate::themes::builtin)
                .collect(),
            Vec::new(),
        );
        assert_eq!(
            resolve_auto(&reg, Variant::Dark, None, None).id,
            "yaru-dark"
        );
        assert_eq!(
            resolve_auto(&reg, Variant::Light, None, None).id,
            "yaru-light"
        );
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
        // \u{10} 是 Shift 伪字符，即使带 ctrl=true 也来自 Ctrl+Shift 组合；
        // 真正的 Ctrl+P 是 text="p" + ctrl=true，不能混为一谈。
        assert!(is_modifier_artifact("\u{10}", true, false, false));
        assert!(!is_modifier_artifact("p", true, false, true)); // Ctrl+P
        assert!(!is_modifier_artifact("\u{1b}", false, false, false)); // Esc
        assert!(!is_modifier_artifact("\n", true, false, false)); // Ctrl+Enter
        assert!(!is_modifier_artifact("你", false, false, false));
    }

    #[test]
    fn selection_autoscroll_triggers_only_on_vertical_edges() {
        assert_eq!(selection_autoscroll_dir(0.0, 240.0, 16.0), 1);
        assert_eq!(selection_autoscroll_dir(8.0, 240.0, 16.0), 1);
        assert_eq!(selection_autoscroll_dir(120.0, 240.0, 16.0), 0);
        assert_eq!(selection_autoscroll_dir(232.0, 240.0, 16.0), -1);
        assert_eq!(selection_autoscroll_dir(300.0, 240.0, 16.0), -1);
        assert_eq!(selection_autoscroll_dir(120.0, 0.0, 16.0), 0);
        // 指针拖出窗口后坐标可以为负/超出 pane；方向仍按最近的水平边判定。
        assert_eq!(selection_autoscroll_dir(-50.0, 240.0, 16.0), 1);
        assert_eq!(selection_autoscroll_dir(400.0, 240.0, 16.0), -1);
    }

    #[test]
    fn selection_autoscroll_step_clamps_at_history_limits() {
        // 历史中间：顶部向上滚一行、底部向下滚一行。
        assert_eq!(selection_autoscroll_step(1, 10, 100, 30), Some((1, 0)));
        assert_eq!(selection_autoscroll_step(-1, 10, 100, 30), Some((-1, 29)));
        // 已到历史顶：不再发 scroll，但仍把选区扩到第 0 行。
        assert_eq!(selection_autoscroll_step(1, 100, 100, 30), Some((0, 0)));
        assert_eq!(selection_autoscroll_step(1, 100, 100, 0), Some((0, 0)));
        // 已在最新：不再向下滚，但仍扩到最后一行。
        assert_eq!(selection_autoscroll_step(-1, 0, 100, 30), Some((0, 29)));
        assert_eq!(selection_autoscroll_step(-1, 0, 0, 0), Some((0, 0)));
        // 中间区域不滚动也不扩选区。
        assert_eq!(selection_autoscroll_step(0, 10, 100, 30), None);
    }

    #[test]
    fn autoscroll_only_for_local_moved_selection_drags() {
        // 本地选词且已移动：计时器应运行。
        assert!(selection_drag_autoscrolls(false, false, true));
        // 未移动：不滚（否则按下不动也会滚动）。
        assert!(!selection_drag_autoscrolls(false, false, false));
        // 终端鼠标上报转发、Ctrl 链接：都不允许本地自动滚动。
        assert!(!selection_drag_autoscrolls(true, false, true));
        assert!(!selection_drag_autoscrolls(false, true, true));
    }

    #[test]
    fn pointer_outside_window_uses_window_content_bounds() {
        // 单 pane：pane 原点 (0,0)，窗口内容 800x600，tab bar 32。
        // 边缘内/外必须区分，拖出窗口才能停表。
        assert!(!pointer_outside_window(0.0, 0.0, 10.0, 10.0, 800.0, 600.0));
        assert!(!pointer_outside_window(0.0, 0.0, 799.0, 599.0, 800.0, 600.0));
        assert!(pointer_outside_window(0.0, 0.0, -1.0, 10.0, 800.0, 600.0));
        assert!(pointer_outside_window(0.0, 0.0, 800.0, 10.0, 800.0, 600.0));
        assert!(pointer_outside_window(0.0, 0.0, 10.0, 634.0, 800.0, 600.0));
        // tab bar 上（窗口内）不算拖出窗口：拖到顶部边缘仍应自动滚动。
        assert!(!pointer_outside_window(0.0, 0.0, 10.0, -20.0, 800.0, 600.0));
        // 分屏右下 pane：pane 偏移要加到窗口坐标上。
        assert!(!pointer_outside_window(400.0, 300.0, 399.0, 299.0, 800.0, 600.0));
        assert!(pointer_outside_window(400.0, 300.0, 400.0, 10.0, 800.0, 600.0));
        // 尚未完成布局（窗口尺寸为 0）：不误判为拖出窗口。
        assert!(!pointer_outside_window(0.0, 0.0, 900.0, 900.0, 0.0, 0.0));
    }

    #[test]
    fn selection_normalization_trims_trailing_blank_lines() {
        assert_eq!(
            normalize_clipboard_text("a\nb\n\n\n".into()),
            Some("a\nb".to_string())
        );
        assert_eq!(
            normalize_clipboard_text("a\nb  ".into()),
            Some("a\nb  ".to_string())
        );
        assert_eq!(normalize_clipboard_text("\n\n\n".into()), None);
        assert_eq!(normalize_clipboard_text(String::new()), None);
        // 中间空行要保留
        assert_eq!(
            normalize_clipboard_text("a\n\nb".into()),
            Some("a\n\nb".to_string())
        );
    }

    #[test]
    fn language_change_is_hot_reload_and_search_follows_language() {
        // 语言是热更新字段：不应被归类为 new_pane/restart。
        let old = Config::default();
        let mut next = old.clone();
        next.language = "zh".into();
        let summary = reconfig_summary(&old, &next);
        assert!(summary.hot.contains(&"language"), "{summary:?}");
        assert!(summary.new_pane.is_empty(), "{summary:?}");
        assert!(summary.restart.is_empty(), "{summary:?}");

        // 搜索索引中英都命中；展示行跟随语言，中文行不再出现英文分类名。
        let en = settings_search_hits("sixel", "en");
        let en_line = &en.iter().find(|h| h.section == "images").expect("en sixel").line;
        assert!(en_line.contains("Terminal · Sixel"), "{en_line}");
        assert!(!en_line.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)), "{en_line}");
        let zh = settings_search_hits("sixel", "zh");
        let zh_line = &zh.iter().find(|h| h.section == "images").expect("zh sixel").line;
        assert!(zh_line.contains("终端 · Sixel"), "{zh_line}");
        assert!(!zh_line.contains("Terminal"), "{zh_line}");
        // 中文关键词也能搜到英文界面下的同一项。
        assert!(settings_search_hits("光标", "en").iter().any(|h| h.cat == 0));
        // 每个分类都有索引项，侧栏不会出现空页。
        for cat in 0..SETTINGS_CATS_EN.len() as i32 {
            assert!(
                SETTINGS_CATALOG.iter().any(|e| e.cat == cat),
                "settings 分类 {cat} 没有任何搜索索引项"
            );
        }
    }

    #[test]
    fn command_titles_have_chinese_names_for_settings_page() {
        assert_eq!(command_title("zh", "tab.new", "New Tab"), "新建标签");
        assert_eq!(command_title("zh", "terminal.find", "Find"), "查找");
        assert_eq!(command_title("en", "tab.new", "New Tab"), "New Tab");
        // 插件命令没有内置翻译时回退原名，不 panic/不丢标题。
        assert_eq!(command_title("zh", "plugin.demo.greet", "Greet"), "Greet");
    }

    /// 解析 Slint 源码里的 `Lang.t("en", "zh")` 对，供静态校验使用。
    fn parse_lang_pairs(source: &str) -> Vec<(String, String)> {
        fn quoted(s: &str, start: usize) -> Option<(String, usize)> {
            let bytes = s.as_bytes();
            if *bytes.get(start)? != b'"' {
                return None;
            }
            let mut out = String::new();
            let mut i = start + 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => {
                        let next = *bytes.get(i + 1)?;
                        match next {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'n' => out.push('\n'),
                            b't' => out.push('\t'),
                            b'{' => out.push_str("\\{"),
                            b'}' => out.push_str("\\}"),
                            other => out.push(other as char),
                        }
                        i += 2;
                    },
                    b'"' => return Some((out, i + 1)),
                    _ => {
                        let ch = s[i..].chars().next()?;
                        out.push(ch);
                        i += ch.len_utf8();
                    },
                }
            }
            None
        }
        let mut pairs = Vec::new();
        for (idx, _) in source.match_indices("Lang.t(") {
            let after = idx + "Lang.t(".len();
            let Some((en, pos)) = quoted(source, after) else { continue };
            let rest = &source[pos..];
            let Some(comma) = rest.find(',') else { continue };
            let zh_start = pos + comma + 1;
            let Some((zh, _)) = quoted(source, source[zh_start..].find('"').map(|p| zh_start + p).unwrap_or(zh_start)) else { continue };
            pairs.push((en, zh));
        }
        pairs
    }

    #[test]
    fn settings_i18n_translation_pairs_are_clean() {
        let source = include_str!("../ui/main.slint");
        let pairs = parse_lang_pairs(source);
        assert!(pairs.len() > 250, "Settings/菜单静态文案应全部走 Lang.t，实际 {} 对", pairs.len());
        let has_cjk = |s: &str| s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        for (en, zh) in &pairs {
            // en 文案里绝不能混入中文。
            assert!(!has_cjk(en), "英文文案混入中文: {en:?}");
            // zh 文案必须有中文；ASCII DEL/Control-H 这类纯技术选项例外。
            let technical = en == "ASCII DEL (0x7F)" || en == "Control-H (0x08)";
            assert!(has_cjk(zh) || technical, "中文文案没有中文: {en:?} -> {zh:?}");
        }
    }

    #[test]
    fn settings_static_text_is_always_wrapped_in_lang_t() {
        let source = include_str!("../ui/main.slint");
        let settings = source
            .split("// ================= Settings =================")
            .nth(1)
            .expect("Settings section");
        let settings = settings.split("// profiles-open 的遮罩").next().unwrap();
        let ctx = source
            .split("// ---------------- 右键菜单 ----------------")
            .nth(1)
            .expect("context menu")
            .split("// Profile 选择器菜单")
            .next()
            .unwrap();
        let tabmenu = source
            .split("if root.tabmenu-open : Rectangle {")
            .nth(1)
            .expect("tab menu")
            .split("// ================= Settings")
            .next()
            .unwrap();
        let profiles = source
            .split("// profiles-open 的遮罩")
            .nth(1)
            .expect("profiles panel")
            .split("// known_hosts 主机密钥预检：")
            .next()
            .unwrap();

        let allowed = |s: &str| {
            s.is_empty()
                || matches!(s, "×" | "✕" | "★" | "☆" | "✎")
                || s.starts_with("\\{")
                || s.starts_with("Ctrl+")
                || s.starts_with("ESC [")
                || matches!(
                    s,
                    "English" | "中文" | "SSH" | "Telnet" | "Shell" | "SFTP"
                        | "ASCII DEL" | "Control-H"
                )
                || s.starts_with('/')
        };
        for (tag, section) in [
            ("settings", settings),
            ("context-menu", ctx),
            ("tab-menu", tabmenu),
            ("profiles", profiles),
        ] {
            for (lineno, line) in section.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for prop in ["text: ", "label: ", "description: ", "placeholder: ", "title: "] {
                    let Some(pos) = line.find(prop) else { continue };
                    let value = line[pos + prop.len()..].trim_start();
                    let Some(value) = value.strip_prefix('"') else { continue };
                    let end = value.find('"').unwrap_or(value.len());
                    let literal = &value[..end];
                    assert!(
                        allowed(literal),
                        "{tag} 第 {} 行静态文案未走 Lang.t: {literal:?}",
                        lineno + 1
                    );
                }
            }
        }
    }

    #[test]
    fn settings_search_matches_labels_and_keywords() {
        let hits = settings_search("font");
        assert!(
            hits.iter()
                .any(|(cat, line)| *cat == 1 && line.contains("Font size")),
            "{hits:?}"
        );
        assert!(settings_search("copy")
            .iter()
            .any(|(_, l)| l.contains("Copy on select")));
        assert!(settings_search("  theme  ")
            .iter()
            .any(|(cat, _)| *cat == 0));
        assert!(settings_search("cursor")
            .iter()
            .any(|(cat, line)| *cat == 0 && line.contains("Cursor")));
        assert!(settings_search("bold").iter().any(|(cat, _)| *cat == 0));
        assert!(settings_search("zzzz").is_empty());
        assert!(settings_search("").is_empty());
        // 结果按目录顺序返回
        let cats: Vec<i32> = settings_search("e").iter().map(|(c, _)| *c).collect();
        assert!(cats.windows(2).all(|w| w[0] <= w[1]), "{cats:?}");
    }

    #[test]
    fn settings_help_category_is_indexed_in_both_languages() {
        let en = settings_search_hits("help", "en");
        assert_eq!(en.len(), 4, "en help hits={}", en.len());
        assert!(en.iter().all(|h| h.cat == 8 && h.line.contains("Help")));
        let zh = settings_search_hits("帮助", "zh");
        assert_eq!(zh.len(), 4, "zh help hits={}", zh.len());
        assert!(zh.iter().all(|h| h.cat == 8 && h.line.contains("帮助")));
        // 旧分类下标不因新增 Help 改变。
        assert_eq!(settings_cat_label(0, "en"), "Appearance");
        assert_eq!(settings_cat_label(7, "en"), "Profiles");
        assert_eq!(settings_cat_label(8, "en"), "Help");
        assert_eq!(settings_cat_label(8, "zh"), "帮助");

        let info = help_info();
        let text = help_diagnostics_text(&info, "zh");
        assert!(text.contains(BUILD_VERSION));
        assert!(text.contains(BUILD_COMMIT));
        assert!(text.contains(BUILD_TARGET));
        assert!(text.contains(&info.config_dir.display().to_string()));
        assert!(text.contains(&info.log_dir.display().to_string()));
        assert!(!text.to_lowercase().contains("password"));
        assert!(!text.to_lowercase().contains("secret"));
    }

    #[test]
    fn settings_search_finds_terminal_behavior_controls() {
        for (query, label) in [
            ("audible", "Audible bell"),
            ("scroll on output", "Scroll on output"),
            ("keystroke", "Scroll on keystroke"),
            ("unlimited", "Unlimited scrollback"),
            ("scrollbar", "Scrollbar policy"),
            ("restore session", "Restore session on startup"),
            ("backspace", "Backspace / Delete binding"),
        ] {
            let hits = settings_search(query);
            assert!(
                hits.iter()
                    .any(|(cat, line)| *cat == 1 && line.contains(label)),
                "query={query:?} 应命中 {label}: {hits:?}"
            );
        }
    }

    #[test]
    fn settings_search_hits_carry_jump_sections_and_new_categories() {
        let sixel = settings_search_hits("sixel", "en");
        assert!(
            sixel.iter().any(|h| h.cat == 1 && h.section == "images"),
            "sixel 应跳到 Terminal/Images: {:?}",
            sixel.iter().map(|h| (h.cat, h.section)).collect::<Vec<_>>()
        );
        let iip = settings_search_hits("iip", "en");
        assert!(
            iip.iter().any(|h| h.cat == 1 && h.section == "images"),
            "iip 应跳到 Terminal/Images: {:?}",
            iip.iter().map(|h| (h.cat, h.section)).collect::<Vec<_>>()
        );
        for query in ["hud", "fps", "render hud", "帧率"] {
            let hits = settings_search_hits(query, "en");
            assert!(
                hits.iter().any(|h| h.cat == 1 && h.section == "images"),
                "query={query:?} 应跳到 Terminal/Images: {:?}",
                hits.iter().map(|h| (h.cat, h.section)).collect::<Vec<_>>()
            );
        }
        let backspace = settings_search_hits("backspace", "en");
        assert!(backspace.iter().any(|h| h.cat == 1 && h.section == "keys"));
        let profiles = settings_search_hits("profile", "en");
        assert!(profiles.iter().any(|h| h.cat == 7 && h.section == "profiles"));
        let advanced = settings_search_hits("import", "en");
        assert!(advanced.iter().any(|h| h.cat == 3 && h.section == "config"));
        // 每个分类都必须有索引项，侧栏不会出现空页。
        for cat in 0..SETTINGS_CATS_EN.len() as i32 {
            assert!(
                SETTINGS_CATALOG.iter().any(|e| e.cat == cat),
                "settings 分类 {cat} 没有任何搜索索引项"
            );
        }
        assert!(SETTINGS_CATALOG.iter().all(|e| {
            e.cat >= 0
                && (e.cat as usize) < SETTINGS_CATS_EN.len()
                && !e.section.is_empty()
                && !e.en.is_empty()
                && !e.zh.is_empty()
        }));
    }

    #[test]
    fn key_scroll_policy_only_fires_when_scrolled_up_and_enabled() {
        let bottom = ScrollPosition {
            offset: 0,
            history: 100,
            rows: 20,
        };
        let up = ScrollPosition {
            offset: 7,
            history: 100,
            rows: 20,
        };
        assert!(!key_scrolls_to_bottom(true, bottom), "已在底部不重复滚动");
        assert!(key_scrolls_to_bottom(true, up));
        assert!(!key_scrolls_to_bottom(false, up), "配置关闭时不回底");
    }

    #[test]
    fn scrollbar_policy_controls_visibility_and_always_thumb() {
        let track = 200.0;
        let no_history = ScrollPosition {
            offset: 0,
            history: 0,
            rows: 20,
        };
        let scrollable = ScrollPosition {
            offset: 0,
            history: 80,
            rows: 20,
        };

        // auto：有历史才显示；thumb 在底部。
        let (visible, y, h) = scrollbar_geometry(ScrollbarPolicy::Auto, no_history, 0.1, track);
        assert!(!visible && y == 0.0 && h == 0.0);
        let (visible, y, h) = scrollbar_geometry(ScrollbarPolicy::Auto, scrollable, 0.1, track);
        assert!(visible);
        assert!(h >= 20.0 && h < track, "thumb 高度按比例: {h}");
        assert!(y > 0.0, "offset=0 时 thumb 应在底部: {y}");

        // always：没有历史也显示，thumb 占满轨道。
        let (visible, y, h) = scrollbar_geometry(ScrollbarPolicy::Always, no_history, 0.1, track);
        assert!(visible);
        assert_eq!((y, h), (0.0, track));

        // never：有历史也隐藏。
        let (visible, y, h) = scrollbar_geometry(ScrollbarPolicy::Never, scrollable, 0.1, track);
        assert!(!visible && y == 0.0 && h == 0.0);
    }

    #[test]
    fn hotkey_page_lists_bindings_and_conflicts() {
        let reg = commands::builtin();
        let (summary, rows, ids) = hotkey_rows(&reg, &[], "en");
        assert!(
            summary.contains(&format!("{} commands", reg.len())),
            "{summary}"
        );
        assert_eq!(rows.len(), reg.all().len());
        assert_eq!(ids.len(), rows.len());
        assert!(rows
            .iter()
            .any(|r| r.title == "New Tab" && r.chord == "Ctrl+T" && !r.custom));
        assert!(rows
            .iter()
            .any(|r| r.title == "Duplicate Tab" && r.chord == "(none)"));
        assert!(rows.iter().all(|r| !r.conflict));

        // 自定义标记：override 后 chord 生效并标 custom
        let mut overrides = vec![commands::HotkeyOverride {
            command: "tab.new".into(),
            chord: "ctrl+alt+n".into(),
        }];
        let (summary, rows, _) = hotkey_rows(&reg, &overrides, "en");
        assert!(summary.contains("1 custom"), "{summary}");
        let row = rows.iter().find(|r| r.title == "New Tab").unwrap();
        assert_eq!(row.chord, "Ctrl+Alt+N");
        assert!(row.custom);

        // 手工写入的冲突 override 会被计入行标记与冲突数
        overrides.push(commands::HotkeyOverride {
            command: "tab.close".into(),
            chord: "ctrl+alt+n".into(),
        });
        let (summary, rows, _) = hotkey_rows(&reg, &overrides, "en");
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
    fn clipboard_image_is_written_as_valid_png() {
        let image = arboard::ImageData {
            width: 2,
            height: 1,
            bytes: vec![255, 0, 0, 255, 0, 255, 0, 128].into(),
        };
        let path = write_clipboard_image(image).expect("encode clipboard image");
        let bytes = std::fs::read(&path).expect("read png");
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("png header");
        let mut out = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).expect("png frame");
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&out[..8], &[255, 0, 0, 255, 0, 255, 0, 128]);
        let _ = std::fs::remove_file(path);

        let bad = arboard::ImageData {
            width: 2,
            height: 2,
            bytes: vec![0; 4].into(),
        };
        assert!(write_clipboard_image(bad).is_err(), "short RGBA must be rejected");
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
    fn inline_image_fits_inside_pane() {
        assert_eq!(crate::render::fit_inline_image_size(100, 50, 200.0, 200.0), (100.0, 50.0));
        assert_eq!(crate::render::fit_inline_image_size(400, 200, 200.0, 200.0), (200.0, 100.0));
        assert_eq!(crate::render::fit_inline_image_size(100, 400, 200.0, 200.0), (50.0, 200.0));
        // 已经贴到右下角时只使用剩余空间，不越过 pane。
        assert_eq!(crate::render::fit_inline_image_size(400, 200, 80.0, 200.0), (80.0, 40.0));
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
    fn drag_target_respects_pinned_groups() {
        // 5 Tab（2 pinned + 3 unpinned），每槽 100px + 4px spacing + 8px padding。
        let strip = 5.0 * 100.0 + 4.0 * 4.0 + 8.0;
        let slot = strip / 5.0;
        let x_at = |slot_index: usize| slot_index as f32 * slot + 2.0;

        // pinned 拖动：只能落在 0..pinned_count-1；原地/跨组/越界都 None。
        assert_eq!(drag_target_in_group(x_at(0), strip, 5, 2, 0), None);
        assert_eq!(drag_target_in_group(x_at(1), strip, 5, 2, 0), Some(1));
        assert_eq!(drag_target_in_group(x_at(2), strip, 5, 2, 0), None);
        assert_eq!(drag_target_in_group(x_at(4), strip, 5, 2, 0), None);
        assert_eq!(drag_target_in_group(x_at(0), strip, 5, 2, 1), Some(0));
        assert_eq!(drag_target_in_group(x_at(2), strip, 5, 2, 1), None);

        // unpinned 拖动：只能落在 pinned_count..n-1。
        assert_eq!(drag_target_in_group(x_at(0), strip, 5, 2, 4), None);
        assert_eq!(drag_target_in_group(x_at(1), strip, 5, 2, 4), None);
        assert_eq!(drag_target_in_group(x_at(2), strip, 5, 2, 4), Some(2));
        assert_eq!(drag_target_in_group(x_at(3), strip, 5, 2, 4), Some(3));
        assert_eq!(drag_target_in_group(x_at(4), strip, 5, 2, 4), None);
        assert_eq!(drag_target_in_group(x_at(3), strip, 5, 2, 3), None);

        // 全 unpinned / 非法输入。
        assert_eq!(drag_target_in_group(x_at(3), strip, 5, 0, 0), Some(3));
        assert_eq!(drag_target_in_group(x_at(0), strip, 5, 5, 4), Some(0));
        assert_eq!(drag_target_in_group(x_at(0), strip, 0, 0, 0), None);
        assert_eq!(drag_target_in_group(x_at(0), strip, 5, 2, 9), None);
    }

    #[test]
    fn drag_gap_index_maps_insertion_boundary() {
        // 向右拖：最终下标 t 表示插在原 t 之后，视觉 gap = t+1。
        assert_eq!(drag_gap_index(0, 1), 2);
        assert_eq!(drag_gap_index(2, 3), 4);
        // 向左拖：插在原 t 之前，视觉 gap = t。
        assert_eq!(drag_gap_index(4, 2), 2);
        assert_eq!(drag_gap_index(1, 0), 0);
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
        let mut cfg = Config {
            default_profile_id: 7,
            ..Config::default()
        };
        // 删别的 Profile：默认保持不变
        assert!(!clear_default_after_remove(&mut cfg, 8));
        assert_eq!(cfg.default_profile_id, 7);
        // 删默认 Profile：回退 0（+ / Ctrl+T 走系统 shell）
        assert!(clear_default_after_remove(&mut cfg, 7));
        assert_eq!(cfg.default_profile_id, 0);
        // id=0 不可能是“默认 Profile”，不能误触发写盘
        assert!(!clear_default_after_remove(&mut cfg, 0));
    }

    fn put_frame_text(frame: &mut Frame, text: &str) {
        for (col, ch) in text.chars().enumerate() {
            let mut cell = tervo_core::CellData::default();
            cell.ch = ch as u32;
            frame.put(col, cell, None);
        }
    }

    #[test]
    fn frame_target_prefers_osc8_over_plain_url() {
        let mut f = Frame::new(40, 1);
        put_frame_text(&mut f, "https://plain.example/x");
        let mut first = tervo_core::CellData::default();
        first.ch = 'h' as u32;
        f.put(0, first, Some("https://osc.example/x"));

        assert_eq!(
            frame_target_at(&f, 0, 0),
            Some(LinkTarget::Uri("https://osc.example/x".to_string()))
        );
        // 同行的普通 URL 只在没有 OSC 8 的格子上兜底。
        assert_eq!(
            frame_target_at(&f, 0, 3),
            Some(LinkTarget::Uri("https://plain.example/x".to_string()))
        );
    }

    #[test]
    fn unsafe_osc8_is_rejected_and_plain_url_still_wins() {
        let mut f = Frame::new(40, 1);
        put_frame_text(&mut f, "https://plain.example/x");
        let mut first = tervo_core::CellData::default();
        first.ch = 'h' as u32;
        f.put(0, first, Some("javascript:alert(1)"));

        // 恶意 OSC 8 不得阻止同格可见 http URL 的兜底。
        assert_eq!(
            frame_target_at(&f, 0, 0),
            Some(LinkTarget::Uri("https://plain.example/x".to_string()))
        );
    }

    #[test]
    fn frame_target_rejects_unsafe_only_and_out_of_bounds() {
        let mut f = Frame::new(20, 1);
        put_frame_text(&mut f, "hello");
        let mut first = tervo_core::CellData::default();
        first.ch = 'h' as u32;
        f.put(0, first, Some("data:text/html,<script>alert(1)</script>"));
        assert_eq!(frame_target_at(&f, 0, 0), None);
        assert_eq!(frame_target_at(&f, 0, 9), None);
        assert_eq!(frame_target_at(&f, 5, 0), None, "越界行应该是 None");
        assert_eq!(frame_target_at(&f, 0, 99), None, "越界列应该是 None");
    }

    #[test]
    fn frame_target_opens_paths_but_not_bare_ipv4() {
        let mut f = Frame::new(64, 2);
        put_frame_text(&mut f, "see /tmp/tervo-file.txt");
        assert_eq!(
            frame_target_at(&f, 0, 6),
            Some(LinkTarget::Path("/tmp/tervo-file.txt".to_string()))
        );
        // hover 在远端 pane 不把路径伪装成可点；URL 不受影响。
        assert_eq!(hover_target_at(&f, 0, 6, true), None);
        assert_eq!(
            hover_target_at(&f, 0, 6, false),
            Some(LinkTarget::Path("/tmp/tervo-file.txt".to_string()))
        );

        let mut g = Frame::new(64, 2);
        put_frame_text(&mut g, "127.0.0.1:8080");
        assert_eq!(frame_target_at(&g, 0, 3), None, "裸 IP:port 不打开");
        let mut h = Frame::new(64, 1);
        put_frame_text(&mut h, "http://127.0.0.1:8080/health");
        assert_eq!(
            frame_target_at(&h, 0, 3),
            Some(LinkTarget::Uri("http://127.0.0.1:8080/health".to_string()))
        );
    }

    #[test]
    fn local_path_resolution_expands_home_and_uses_shell_cwd() {
        let home = PathBuf::from("/home/tervo");
        let cwd = PathBuf::from("/work/project");
        assert_eq!(
            resolve_local_path("/var/log/syslog", Some(&cwd), Some(&home)).unwrap(),
            PathBuf::from("/var/log/syslog")
        );
        assert_eq!(
            resolve_local_path("~/notes/a.txt", Some(&cwd), Some(&home)).unwrap(),
            home.join("notes/a.txt")
        );
        assert_eq!(
            resolve_local_path("./target/app", Some(&cwd), Some(&home)).unwrap(),
            cwd.join("./target/app")
        );
        assert_eq!(
            resolve_local_path("../sibling", Some(&cwd), Some(&home)).unwrap(),
            cwd.join("../sibling")
        );
        assert!(resolve_local_path("~/x", Some(&cwd), None)
            .unwrap_err()
            .contains("HOME"));
        assert!(resolve_local_path("./x", None, Some(&home))
            .unwrap_err()
            .contains("shell 目录"));
        assert!(resolve_local_path("/tmp/a b", Some(&cwd), Some(&home))
            .unwrap_err()
            .contains("空白"));
    }

    #[test]
    fn local_path_inspection_reports_missing_and_permission_denied() {
        let dir = std::env::temp_dir().join(format!("tervo-link-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let file = dir.join("ok.txt");
        std::fs::write(&file, "ok").unwrap();
        assert_eq!(inspect_local_path(&file), Ok(()));
        assert_eq!(inspect_local_path(&dir), Ok(()));

        let missing = dir.join("nope.txt");
        assert!(inspect_local_path(&missing).unwrap_err().contains("不存在"));

        // root 无视权限位，无法在 root 下构造 PermissionDenied；跳过该断言。
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.join("locked.txt");
            std::fs::write(&locked, "secret").unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            let err = inspect_local_path(&locked).unwrap_err();
            assert!(err.contains("无权限"), "实际错误: {err}");
            // 清理时先恢复权限，避免后续 remove_dir_all 失败。
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_text_has_commit_build_time_and_target() {
        let text = version_text();
        assert!(text.starts_with("Tervo "), "{text}");
        assert!(text.contains("\ncommit:  "), "{text}");
        assert!(text.contains("\nbuilt:   "), "{text}");
        assert!(text.contains("\ntarget:  "), "{text}");
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

    #[test]
    fn all_primary_callbacks_live_in_shared_registration() {
        let src = include_str!("main.rs");
        let start = src
            .find("fn register_window_callbacks(")
            .expect("shared registration fn must exist");
        let rest = &src[start..];
        let end = rest
            .find("\n/// 启动时对单实例锁的判定结果。")
            .expect("shared registration fn must be followed by instance code");
        let body = &rest[..end];
        // 全部 UI 回调都在共享注册函数里，不允许 secondary 另抄一份。
        assert_eq!(
            body.matches("\n    ui.on_").count(),
            src.matches("\n    ui.on_").count(),
            "some ui.on_* registration escaped register_window_callbacks"
        );
        assert!(
            !body.contains("with_app("),
            "shared callbacks must use runtime::with_window(window_id, ...), not unscoped with_app"
        );
        assert!(body.matches("runtime::with_window(window_id").count() >= 150);
    }

    #[test]
    fn secondary_window_reuses_shared_callback_registration() {
        let src = include_str!("secondary.rs");
        assert!(src.contains("crate::register_window_callbacks"));
        assert!(
            !src.contains("install_core_callbacks"),
            "secondary must not keep a private callback installer"
        );
        assert!(
            !src.contains("ui.on_"),
            "secondary must not duplicate any ui.on_* handler"
        );
    }

    #[test]
    fn close_strategy_removes_one_window_and_quits_on_last() {
        assert_eq!(window_close_action(3), WindowCloseAction::Remove);
        assert_eq!(window_close_action(2), WindowCloseAction::Remove);
        assert_eq!(
            window_close_action(1),
            WindowCloseAction::Quit,
            "关闭最后一个窗口必须交给主流程退出并清理 session marker"
        );
    }

    #[test]
    fn scrollback_budget_scales_with_pane_count() {
        assert_eq!(scrollback_budget(1, 2000), 2000, "单 pane 保持用户配置");
        assert_eq!(scrollback_budget(4, 2000), 2000, "少量 pane 不收缩");
        assert_eq!(scrollback_budget(25, 2000), 960, "22 Tab+4 pane 按预算收缩");
        assert_eq!(scrollback_budget(25, 500), 500, "Profile 更小配置不被放大");
        assert_eq!(scrollback_budget(1000, 2000), 200, "极多 pane 仍有 200 行下限");
    }

    #[test]
    fn drop_paths_are_shell_escaped_literally() {
        assert_eq!(
            drop_paths_text(&[PathBuf::from("/tmp/plain")]),
            "'/tmp/plain' "
        );
        assert_eq!(drop_paths_text(&[PathBuf::from("/tmp/a b")]), "'/tmp/a b' ");
        // 单引号在单引号字符串里写成 '\''，shell 解析后仍是字面单引号。
        assert_eq!(
            drop_paths_text(&[PathBuf::from("/tmp/it's")]),
            "'/tmp/it'\\''s' "
        );
        assert_eq!(
            drop_paths_text(&[PathBuf::from("/tmp/x"), PathBuf::from("目录")]),
            "'/tmp/x' '目录' "
        );
        assert_eq!(drop_paths_text(&[]), "");

        // 常见 shell 元字符必须被引号包住，不能被展开/执行。
        for raw in ["$HOME", "a;rm -rf /", "`id`", "$(id)", "*", "~", "\\", "\""] {
            assert_eq!(shell_escape_text(raw), format!("'{raw}'"));
        }
        assert_eq!(shell_escape_text(""), "''");
    }

    #[test]
    fn drop_payload_is_bracketed_with_trailing_space() {
        let paths = [PathBuf::from("/tmp/a b"), PathBuf::from("/tmp/c")];
        let text = drop_paths_text(&paths);
        assert_eq!(text, "'/tmp/a b' '/tmp/c' ");
        assert!(text.ends_with(' '), "末尾必须有空格: {text:?}");
        assert!(!text.contains('\n'));

        // 启用 bracketed paste 时包 200~/201~，光标不会把路径当命令执行。
        let bracketed = paste_payload(&text, true);
        assert!(bracketed.starts_with(b"\x1b[200~"));
        assert!(bracketed.ends_with(b"\x1b[201~"));
        assert_eq!(&bracketed[6..bracketed.len() - 6], text.as_bytes());

        // 未启用时退化为纯文本（由 shell 自己处理行编辑）。
        assert_eq!(paste_payload(&text, false), text.as_bytes());
    }

    #[test]
    fn shift_right_modifier_artifact_is_ignored_but_ctrl_u_is_not() {
        assert!(is_modifier_artifact("\u{15}", false, false, true));
        // Ctrl+Shift+P/C/V 时 Slint 会带 ctrl=true 上报 Shift 伪字符，也必须过滤，
        // 否则往 PTY 写 0x10（Ctrl+P）。真正的 Ctrl+U 是 text="u"，不受影响。
        assert!(is_modifier_artifact("\u{15}", true, false, true));
        assert!(is_modifier_artifact("\u{10}", true, false, true));
        // 即使 Slint 未上报 shift，Shift 伪字符也不能落进 PTY。
        assert!(is_modifier_artifact("\u{15}", true, false, false));
        assert!(!is_modifier_artifact("u", true, false, false));
    }

    #[test]
    fn save_contents_resolves_paths_and_writes_files() {
        let dir = std::env::temp_dir().join(format!("tervo-save-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).expect("create temp dir");
        let home = dir.join("home");

        // ~ 展开 + 写文件
        let path =
            save_contents_file("~/out.txt", "hello\nworld\n", Some(&home), &dir).expect("save");
        assert_eq!(path, home.join("out.txt"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");

        // 相对路径按 cwd 解析
        let path = save_contents_file("relative.txt", "x", Some(&home), &dir).expect("save");
        assert_eq!(path, dir.join("relative.txt"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");

        // 绝对路径保持
        let abs = dir.join("absolute.txt");
        let saved =
            save_contents_file(abs.to_str().unwrap(), "abs", Some(&home), &dir).expect("save");
        assert_eq!(saved, abs);

        // 明确错误：空路径 / 无 HOME 的 ~ / 不存在的父目录
        assert!(save_contents_file("  ", "x", Some(&home), &dir)
            .unwrap_err()
            .contains("为空"));
        assert!(resolve_save_path("~/x", None, &dir)
            .unwrap_err()
            .contains("HOME"));
        let bad = dir.join("missing-dir").join("x.txt");
        let err = save_contents_file(bad.to_str().unwrap(), "x", Some(&home), &dir)
            .expect_err("父目录不存在应报错");
        assert!(err.contains("写入"), "{err}");

        // 默认路径必须明确到文件名，便于 UI 显示/用户确认。
        let default = default_save_path(42);
        assert!(default.to_string_lossy().ends_with("tervo-save-42.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ime_spot_maps_click_anchor_to_window_pixels() {
        // 复刻 dsh /btw 的实测：120x40 终端、cell 9x22、顶部 tabbar 32px；
        // 右击记录到的 composer 格是 (37, 94)，候选点应落在 win=846,846。
        let rect = Rect::new(0.0, 0.0, 1080.0, 848.0);
        let a = ime_spot_key(1, rect, 9.0, 22.0, (37, 94), TABBAR_H, 1.0, true);
        assert_eq!((a.logical_x_milli, a.logical_y_milli), (846_000, 846_000));
        assert_eq!((a.physical_x, a.physical_y), (846, 846));
        assert_eq!((a.logical_w_milli, a.logical_h_milli), (9_000, 22_000));
        assert_eq!((a.physical_w, a.physical_h), (9, 22));
        assert_eq!(a.scale_milli, 1000);
        assert_eq!((a.pane, a.anchored), (1, true));

        // 同一格但 tabbar 在底部：y 少一个 tabbar 高度。
        let bottom = ime_spot_key(1, rect, 9.0, 22.0, (37, 94), 0.0, 1.0, true);
        assert_eq!(bottom.logical_y_milli, 814_000);

        // pane 切换 / 锚点来源 / 光标格变化都必须产生不同的键（否则会误去重）。
        assert_ne!(
            a,
            ime_spot_key(2, rect, 9.0, 22.0, (37, 94), TABBAR_H, 1.0, true)
        );
        assert_ne!(
            a,
            ime_spot_key(1, rect, 9.0, 22.0, (37, 94), TABBAR_H, 1.0, false)
        );
        assert_ne!(
            a,
            ime_spot_key(1, rect, 9.0, 22.0, (37, 93), TABBAR_H, 1.0, true)
        );
        assert_eq!(
            a,
            ime_spot_key(1, rect, 9.0, 22.0, (37, 94), TABBAR_H, 1.0, true)
        );
    }

    #[test]
    fn ime_spot_keys_include_dpr_and_physical_pixels() {
        let rect = Rect::new(0.0, 0.0, 1080.0, 848.0);
        let base = ime_spot_key(1, rect, 9.0, 22.0, (37, 94), TABBAR_H, 1.0, false);
        let hidpi = ime_spot_key(1, rect, 9.0, 22.0, (37, 94), TABBAR_H, 1.25, false);
        // winit 收逻辑坐标；物理像素按 DPR 四舍五入后只用于去重/日志。
        assert_eq!(
            (hidpi.logical_x_milli, hidpi.logical_y_milli),
            (846_000, 846_000)
        );
        assert_eq!((hidpi.physical_x, hidpi.physical_y), (1058, 1058));
        assert_eq!((hidpi.physical_w, hidpi.physical_h), (11, 28));
        assert_eq!(hidpi.scale_milli, 1250);
        assert_ne!(base, hidpi, "DPR 改变必须重发 spot");

        // 非法/零 DPR 回退 1.0，不能出现除零或饱和垃圾坐标。
        let bad = ime_spot_key(1, rect, 9.0, 22.0, (37, 94), TABBAR_H, 0.0, false);
        assert_eq!(bad.scale_milli, 1000);
        assert_eq!(bad, base);
    }

    #[test]
    fn ime_click_anchor_only_for_plain_left_click_on_mouse_reporting_tui() {
        assert!(
            ime_click_anchor(0, true, false),
            "dsh composer 左键应记录锚点"
        );
        assert!(!ime_click_anchor(0, false, false), "普通 shell 不记录锚点");
        assert!(
            !ime_click_anchor(0, true, true),
            "Shift+左键是本地选择，不锚定 IME"
        );
        assert!(!ime_click_anchor(2, true, false), "右键不改变候选点");
        assert!(!ime_click_anchor(1, true, false), "中键不改变候选点");
    }

    #[test]
    fn left_and_right_shift_artifacts_never_reach_pty() {
        // Slint: Shift_L=U+0010、Shift_R=U+0015；xterm 行编辑里 0x15 是 Ctrl+U。
        for artifact in ["\u{10}", "\u{15}"] {
            for (ctrl, alt) in [(false, false), (true, false), (false, true), (true, true)] {
                assert!(
                    is_modifier_artifact(artifact, ctrl, alt, true),
                    "{artifact:?} ctrl={ctrl} alt={alt} 必须被过滤"
                );
            }
        }
        // 真正的 Ctrl+U / 小写 u 不能被误杀。
        assert!(!is_modifier_artifact("u", true, false, false));
        assert!(!is_modifier_artifact("u", false, false, false));
    }

    #[test]
    fn profile_appearance_maps_overrides_and_inherits() {
        let cfg = Config::default();
        let custom = profile::Profile {
            font_size: Some(18.0),
            theme: Some("catppuccin-mocha".into()),
            cursor_shape: Some("beam".into()),
            cursor_blink: Some("off".into()),
            scrollback: Some(12_000),
            ..profile::Profile::default()
        };
        let a = PaneAppearance::from_profile(&custom);
        assert_eq!(a.font_size, Some(18.0));
        assert_eq!(a.theme.as_deref(), Some("catppuccin-mocha"));
        assert_eq!(a.cursor_shape.as_deref(), Some("ibeam"), "别名规范化");
        assert_eq!(a.cursor_blink, Some(false));
        assert_eq!(a.scrollback, Some(12_000));
        assert!(!a.cursor_blinks(true), "Profile off 覆盖全局 on");
        assert_eq!(a.cursor_shape(&cfg), tervo_core::CursorShape::Beam);

        let inherit = PaneAppearance::from_profile(&profile::Profile::default());
        assert_eq!(inherit.font_size, None);
        assert_eq!(inherit.theme, None);
        assert_eq!(inherit.cursor_shape, None);
        assert_eq!(inherit.cursor_blink, None);
        assert_eq!(inherit.scrollback, None);
        assert!(inherit.cursor_blinks(true), "空 = 继承全局闪烁设置");
        assert!(!inherit.cursor_blinks(false));
        assert_eq!(
            inherit.cursor_shape(&cfg),
            cfg.cursor_shape(),
            "空 = 继承全局光标形状"
        );
    }

    #[test]
    fn profile_appearance_blink_on_wakes_timer_without_global() {
        // 只验证继承/覆盖的布尔含义；计时器需要 AppState（GUI），不在单测里构造。
        let on = PaneAppearance { cursor_blink: Some(true), ..PaneAppearance::default() };
        let off = PaneAppearance { cursor_blink: Some(false), ..PaneAppearance::default() };
        assert!(on.cursor_blinks(false), "全局 off 时 Profile on 仍需闪烁");
        assert!(!off.cursor_blinks(true), "全局 on 时 Profile off 保持常亮");
    }

    #[test]
    fn search_count_label_formats_index_total_and_empty_results() {
        let hit = SearchHit { line: 1, start_col: 2, end_col: 4, index: 3, total: 12 };
        assert_eq!(search_count_label(Some(&hit)), "3/12");
        assert_eq!(search_count_label(None), "0/0", "无命中固定显示 0/0");
        let empty = SearchHit { index: 0, total: 0, ..hit };
        assert_eq!(search_count_label(Some(&empty)), "0/0");
        // 防御性：core 理论上不会给出 index=0/total>0；UI 至少不能显示 0/N。
        let partial = SearchHit { index: 0, total: 5, ..hit };
        assert_eq!(search_count_label(Some(&partial)), "1/5");
    }
}
