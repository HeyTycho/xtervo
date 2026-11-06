// SPDX-License-Identifier: GPL-3.0-only
//! 单 pane 的 PTY 会话与终端 actor。
//!
//! 线程模型（严格对应冻结架构）：
//!
//! ```text
//!  UI 线程 ──input(有界64)──┐
//!                            ├──► actor 线程: Term + vte::Processor + damage
//!  PTY 读线程 ──data(有界64)─┘         │
//!                                       ├─ wake(容量1，可合并) ──► UI 重绘
//!                                       ├─ events(有界256) ──────► 标题/bell/退出
//!                                       └─ pull_req/pull_resp ──► UI 主动拉帧
//!  PTY 写线程 ◄──write(有界256)── actor/PtyWrite 事件
//! ```
//!
//! 关键点：
//! - PTY 读线程只做 `read` + 有界 `send`，无任何 UI 调用；
//! - UI 事件走容量 1 的唤醒通道，输出洪峰只会被合并，不会堆积；
//! - 帧数据由 UI 侧持有 `Box<Frame>` 往返复用，稳态零分配。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Direction, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{Config as TermConfig, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Handler, Processor, Rgb};
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::RwLock;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtyPair, PtySize};

use crate::event::{ClipboardSelection, CoreEvent, EventSink, PaneStats, WakeSignal};
use crate::frame::{CellData, CursorShape, CursorState, Frame, LineDamage, Span};
use crate::mouse::{self, MouseEvent};
use crate::osc::OscScanner;
use crate::palette::Palette;

const READ_CHUNK: usize = 32 * 1024;
const PTY_QUEUE_CHUNKS: usize = 64;
const INPUT_QUEUE: usize = 64;
const CTRL_QUEUE: usize = 64;
const WRITE_QUEUE: usize = 256;
const EVENT_QUEUE: usize = 256;

/// 搜索命中（视口坐标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub line: u16,
    pub start_col: u16,
    pub end_col: u16,
}

/// 非阻塞滚动位置快照（actor 在输出/滚动/resize 后刷新）。
///
/// UI 线程只读原子值，绝不向 actor 发同步请求，因此即使 PTY 输出洪峰
/// 也不会因为画滚动条而卡住。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollPosition {
    /// 视口距历史底部的偏移；0 = 底部（最新内容），`history` = 顶部。
    pub offset: usize,
    /// 可回滚历史行数（不含视口内的 `rows` 行）。
    pub history: usize,
    /// 当前视口行数。
    pub rows: usize,
}

impl ScrollPosition {
    /// 历史 + 视口总行数。
    #[inline]
    pub fn total(&self) -> usize {
        self.history.saturating_add(self.rows)
    }

    /// 是否有可滚动历史；false 时 UI 不显示滚动条。
    #[inline]
    pub fn scrollable(&self) -> bool {
        self.history > 0 && self.rows > 0
    }

    /// 滚动条 thumb 在 track 中的归一化 `(top, height)`，取值 0.0..=1.0。
    ///
    /// `min_thumb` 是 thumb 的最小高度（归一化，防止长历史时 thumb 细到点不中）。
    /// 没有历史时返回 `None`。
    pub fn thumb(&self, min_thumb: f32) -> Option<(f32, f32)> {
        if !self.scrollable() {
            return None;
        }
        let total = self.total() as f32;
        let min_thumb = if min_thumb.is_finite() { min_thumb.clamp(0.0, 1.0) } else { 0.0 };
        let height = (self.rows as f32 / total).max(min_thumb).min(1.0);
        let travel = (1.0 - height).max(0.0);
        // offset=history（顶部）→ top=0；offset=0（底部）→ top=travel。
        let progress = if self.history == 0 {
            1.0
        } else {
            1.0 - self.offset.min(self.history) as f32 / self.history as f32
        };
        Some((progress * travel, height))
    }

    /// 由 thumb 顶部归一化位置反推目标 offset（拖动时用）。
    pub fn offset_for_thumb_top(&self, thumb_top: f32, min_thumb: f32) -> usize {
        if self.history == 0 {
            return 0;
        }
        let Some((_, height)) = self.thumb(min_thumb) else { return 0 };
        let travel = (1.0 - height).max(0.0);
        if travel <= f32::EPSILON {
            return 0;
        }
        let top = if thumb_top.is_finite() { thumb_top.clamp(0.0, travel) } else { 0.0 };
        let progress = 1.0 - top / travel;
        ((progress * self.history as f32).round() as usize).min(self.history)
    }

    /// 点击/拖动 track 的归一化位置（0 = 顶部，1 = 底部）→ 目标 offset。
    ///
    /// 语义是「让 thumb 中心跟到指针」；因此点 track 两端会直接跳到顶/底。
    pub fn offset_for_track_fraction(&self, fraction: f32, min_thumb: f32) -> usize {
        if self.history == 0 {
            return 0;
        }
        let Some((_, height)) = self.thumb(min_thumb) else { return 0 };
        let travel = (1.0 - height).max(0.0);
        let fraction =
            if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 1.0 };
        let top = (fraction - height * 0.5).clamp(0.0, travel);
        self.offset_for_thumb_top(top, min_thumb)
    }
}

const SCROLL_OFFSET_MASK: u64 = u32::MAX as u64;

/// 把 offset/history 打包进一个 `AtomicU64`：一次 load 就能拿到一致的一组值。
#[inline]
fn pack_scroll(offset: usize, history: usize) -> u64 {
    let offset = offset.min(u32::MAX as usize) as u64;
    let history = history.min(u32::MAX as usize) as u64;
    (history << 32) | offset
}

#[inline]
fn unpack_scroll(bits: u64) -> (usize, usize) {
    ((bits & SCROLL_OFFSET_MASK) as usize, (bits >> 32) as usize)
}

/// 终端网格尺寸（实现 alacritty 的 `Dimensions`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
}

impl GridSize {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// 事件监听器：只做「转发到有界通道」，绝不阻塞解析线程。
#[derive(Clone)]
struct Listener {
    wake: WakeSignal,
    events: EventSink,
    pty_write: Sender<Vec<u8>>,
    write_dropped: Arc<std::sync::atomic::AtomicU64>,
    /// 当前调色板：OSC 4/10/11/12 查询要按主题回真实颜色。
    palette: Arc<RwLock<Palette>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => self.events.send(CoreEvent::Title(title)),
            Event::ResetTitle => self.events.send(CoreEvent::ResetTitle),
            // Bell 由 actor 里的 OscScanner 统一上报：alacritty 的 Event::Bell 与它
            // 会重复，且 OSC 9/777 本来就不经过 vte 的 EventListener。
            Event::CursorBlinkingChange => self.events.send(CoreEvent::CursorBlinkChange),
            Event::PtyWrite(text) => {
                if self.pty_write.try_send(text.into_bytes()).is_err() {
                    self.write_dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            },
            Event::Exit | Event::ChildExit(_) => self.events.send(CoreEvent::Exited),
            Event::Wakeup => self.wake.notify(),
            // OSC 4/10/11/12 查询：把当前主题颜色按 xterm 格式回写给 PTY。
            Event::ColorRequest(index, format) => {
                let palette = self.palette.read();
                let Some(rgb) = palette.at(index) else { return };
                let reply = format(Rgb {
                    r: (rgb >> 16) as u8,
                    g: (rgb >> 8) as u8,
                    b: rgb as u8,
                });
                if self.pty_write.try_send(reply.into_bytes()).is_err() {
                    self.write_dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            },
            _ => {},
        }
    }
}

/// pane 启动参数。
#[derive(Clone, Debug)]
pub struct PaneConfig {
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
    pub shell: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// 终端配色（内置主题或用户自定义主题都已归一化成 Palette）。
    pub palette: Palette,
    /// 输出时是否始终跟随到最底部（GNOME `scroll-on-output`）。
    ///
    /// false（默认）时用户上滚查看历史，新输出只增长历史、不抢视图；
    /// true 时每次收到输出都把视口拉回底部。
    pub scroll_on_output: bool,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            cols: 120,
            rows: 32,
            // 注意：alacritty 的历史缓冲是**预分配**的，直接决定空闲 RSS。
            // 2000 行 × 120 列 × 16 B ≈ 3.8 MB；10000 行则 ≈ 19 MB。
            scrollback: 2000,
            shell: None,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            palette: Palette::default(),
            scroll_on_output: false,
        }
    }
}

enum Ctrl {
    Resize { cols: u16, rows: u16 },
    SetTheme(Palette),
    Scroll(i32),
    /// 绝对滚动位置（0 = 底部）；滚动条点击/拖拽用。
    ScrollTo(usize),
    /// 运行时切换 scroll-on-output（设置页改动立即生效）。
    SetScrollOnOutput(bool),
    SelectionStart { line: u16, col: u16 },
    SelectionExtend { line: u16, col: u16 },
    SelectionClear,
    /// 选中当前视口全部内容。
    SelectAll,
    /// 清除当前选区（复制后清除，Tabby 的行为）。
    ClearSelection,
    /// 选中视口内某一行（双击选行）。
    SelectLine(i32),
    /// 清屏并清回滚（终端“Clear”语义）。
    ClearScreen,
    /// 鼠标上报：终端启用 1000/1002/1003 时转成转义序列写进 PTY。
    Mouse(MouseEvent),
    CopySelection(Sender<Option<String>>),
    Search { query: String, forward: bool, reply: Sender<Option<SearchHit>> },
    SearchClear,
    Pull(Box<Frame>),
    PtyClosed,
    Shutdown,
}

/// UI 侧持有的单 pane 句柄。
pub struct PaneHandle {
    input_tx: Sender<Vec<u8>>,
    ctrl_tx: Sender<Ctrl>,
    pull_rx: Receiver<Box<Frame>>,
    events_rx: Receiver<CoreEvent>,
    wake_rx: Receiver<()>,
    pub stats: Arc<PaneStats>,
    /// actor 每次喂数据后刷新的模式位快照：UI 需要在**不阻塞**的前提下判断
    /// 当前是否处于鼠标上报模式（决定「转发」还是「本地选词」）。
    mode_bits: Arc<std::sync::atomic::AtomicU32>,
    /// 当前终端光标（行 u32 << 32 | 列），actor 每次解析后刷新；不依赖重绘。
    cursor_bits: Arc<std::sync::atomic::AtomicU64>,
    /// 滚动位置快照（打包 offset/history），见 [`PaneHandle::scroll_position`]。
    scroll: Arc<std::sync::atomic::AtomicU64>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    #[allow(dead_code)]
    threads: Vec<JoinHandle<()>>,
    pub grid: GridSize,
    exited: bool,
}

impl PaneHandle {
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// 送键盘输入。有界阻塞：宁可短暂背压，也不丢按键。
    pub fn input(&self, bytes: &[u8]) {
        let _ = self.input_tx.send(bytes.to_vec());
    }

    /// 非阻塞输入（满则返回 false），供高频粘贴等场景降级。
    pub fn try_input(&self, bytes: &[u8]) -> bool {
        self.input_tx.try_send(bytes.to_vec()).is_ok()
    }

    /// 滚轮回滚：正数向上翻历史。
    pub fn scroll(&self, lines: i32) {
        let _ = self.ctrl_tx.send(Ctrl::Scroll(lines));
    }

    /// 绝对滚动位置（0 = 底部/最新，`history` = 顶部/最旧）。
    ///
    /// 异步：发送后由 actor 在下一轮处理；UI 通过 [`Self::scroll_position`]
    /// 读取新位置并在唤醒后重绘。
    pub fn scroll_to(&self, offset: usize) {
        let _ = self.ctrl_tx.send(Ctrl::ScrollTo(offset));
    }

    /// 回到底部（最新内容）；`scroll_on_keystroke` 的按键路径使用。
    ///
    /// 与 [`Self::scroll_to`] 一样只投递异步消息，不阻塞 UI；是否已经在底部
    /// 由调用方读 [`Self::scroll_position`] 判断。
    pub fn scroll_to_bottom(&self) {
        let _ = self.ctrl_tx.send(Ctrl::ScrollTo(0));
    }

    /// 运行时切换「输出即回底」策略（异步投递，不阻塞 UI）。
    pub fn set_scroll_on_output(&self, enabled: bool) {
        let _ = self.ctrl_tx.send(Ctrl::SetScrollOnOutput(enabled));
    }

    /// 非阻塞滚动位置查询：只读 actor 维护的原子快照。
    ///
    /// `rows` 取 UI 侧已同步的网格行数，`offset/history` 来自同一份打包值，
    /// 不会出现「新 history 配旧 offset」的撕裂。
    pub fn scroll_position(&self) -> ScrollPosition {
        let (offset, history) = unpack_scroll(self.scroll.load(std::sync::atomic::Ordering::Relaxed));
        ScrollPosition { offset, history, rows: self.grid.rows }
    }

    /// 当前终端模式位（actor 刷新，无阻塞）。
    pub fn mode(&self) -> TermMode {
        TermMode::from_bits_truncate(
            self.mode_bits.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// 当前终端光标 (行, 列)，不阻塞；actor 刷新。
    pub fn cursor_pos(&self) -> (u16, u16) {
        let bits = self.cursor_bits.load(std::sync::atomic::Ordering::Relaxed);
        ((bits >> 32) as u16, bits as u16)
    }

    /// 终端是否要求接收鼠标事件（1000/1002/1003）。
    pub fn wants_mouse(&self) -> bool {
        mouse::reporting(self.mode())
    }

    /// 是否要求上报「无按键悬停」（1003）。
    pub fn wants_motion(&self) -> bool {
        self.mode().contains(TermMode::MOUSE_MOTION)
    }

    /// 是否启用 DECSET 1004 focus in/out 上报。
    pub fn wants_focus(&self) -> bool {
        self.mode().contains(TermMode::FOCUS_IN_OUT)
    }

    /// 向 PTY 发送 focus in/out（仅当应用启用了 1004）。
    pub fn focus_event(&self, focused: bool) {
        if self.wants_focus() {
            self.input(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// 是否启用了 kitty keyboard protocol 的 CSI-u 编码。
    pub fn wants_kitty_keyboard(&self) -> bool {
        self.mode().intersects(
            TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC,
        )
    }

    /// 上报一次鼠标事件。未启用鼠标模式的 pane 会自动忽略（actor 侧门禁）。
    pub fn mouse(&self, ev: MouseEvent) {
        let _ = self.ctrl_tx.send(Ctrl::Mouse(ev));
    }

    /// 回写 OSC 52 查询结果（`OSC 52;Pc;<base64> ST`）到 PTY。
    ///
    /// app 收到 [`CoreEvent::Clipboard`] 的 `Load` 后调用；文本由 core 统一编码，
    /// 避免 UI 层再拼一遍控制序列。
    pub fn clipboard_reply(&self, selection: ClipboardSelection, text: &str) {
        let reply = crate::osc::clipboard_reply(selection, text);
        self.input(&reply);
    }

    /// 鼠标按下：在视口坐标 (line, col) 开始选区。
    pub fn selection_start(&self, line: u16, col: u16) {
        let _ = self.ctrl_tx.send(Ctrl::SelectionStart { line, col });
    }

    pub fn selection_extend(&self, line: u16, col: u16) {
        let _ = self.ctrl_tx.send(Ctrl::SelectionExtend { line, col });
    }

    pub fn selection_clear(&self) {
        let _ = self.ctrl_tx.send(Ctrl::SelectionClear);
    }

    /// 清除选区。
    pub fn clear_selection(&self) {
        let _ = self.ctrl_tx.send(Ctrl::ClearSelection);
    }

    /// 选中当前视口（不含回滚历史）。
    pub fn select_all(&self) {
        let _ = self.ctrl_tx.send(Ctrl::SelectAll);
    }

    /// 选中视口内第 `line` 行（双击选行；`line` 是视口坐标，0 = 顶行）。
    pub fn select_line(&self, line: i32) {
        let _ = self.ctrl_tx.send(Ctrl::SelectLine(line));
    }

    /// 清屏 + 清回滚；下一次拉帧强制整屏。
    pub fn clear_screen(&self) {
        let _ = self.ctrl_tx.send(Ctrl::ClearScreen);
    }

    /// 取当前选区文本（阻塞等 actor 回复）。
    pub fn copy_selection(&self) -> Option<String> {
        let (tx, rx) = bounded(1);
        if self.ctrl_tx.send(Ctrl::CopySelection(tx)).is_err() {
            return None;
        }
        rx.recv_timeout(std::time::Duration::from_millis(500)).ok().flatten()
    }

    /// 清除搜索高亮。
    pub fn search_clear(&self) {
        let _ = self.ctrl_tx.send(Ctrl::SearchClear);
    }

    /// 搜索下一个/上一个命中；命中会滚动到可见并返回视口内高亮区间。
    pub fn search(&self, query: &str, forward: bool) -> Option<SearchHit> {
        let (tx, rx) = bounded(1);
        if self
            .ctrl_tx
            .send(Ctrl::Search { query: query.to_string(), forward, reply: tx })
            .is_err()
        {
            return None;
        }
        rx.recv_timeout(std::time::Duration::from_millis(500)).ok().flatten()
    }

    /// 切换配色：actor 侧置 full damage，下一次拉帧整屏重取。
    pub fn set_theme(&self, palette: Palette) {
        let _ = self.ctrl_tx.send(Ctrl::SetTheme(palette));
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid = GridSize::new(cols as usize, rows as usize);
        let _ = self.ctrl_tx.send(Ctrl::Resize { cols, rows });
    }

    /// 主动拉取一帧：把旧缓冲送进去，拿回填充后的同一块内存。
    pub fn pull(&self, frame: Box<Frame>) -> Box<Frame> {
        if self.ctrl_tx.send(Ctrl::Pull(frame)).is_err() {
            // actor 已退出：返回空帧，避免 UI 卡死
            return Box::new(Frame::new(self.grid.cols, self.grid.rows));
        }
        self.pull_rx.recv().unwrap_or_else(|_| Box::new(Frame::new(self.grid.cols, self.grid.rows)))
    }

    pub fn new_frame(&self) -> Box<Frame> {
        Box::new(Frame::new(self.grid.cols, self.grid.rows))
    }

    pub fn try_event(&self) -> Option<CoreEvent> {
        self.events_rx.try_recv().ok()
    }

    pub fn drain_events(&self, out: &mut Vec<CoreEvent>) {
        while let Ok(ev) = self.events_rx.try_recv() {
            out.push(ev);
        }
    }

    /// 等待终端内容变化（阻塞）。用于把 core 唤醒桥接到 UI 事件循环。
    pub fn wait_wake(&self) {
        let _ = self.wake_rx.recv();
    }

    pub fn try_wake(&self) -> bool {
        self.wake_rx.try_recv().is_ok()
    }

    /// 唤醒通道的克隆，供桥接线程持有。
    pub fn wake_receiver(&self) -> Receiver<()> {
        self.wake_rx.clone()
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    pub fn kill(&mut self) {
        let _ = self.ctrl_tx.send(Ctrl::Shutdown);
        let _ = self.child.kill();
    }
}

impl Drop for PaneHandle {
    fn drop(&mut self) {
        let _ = self.ctrl_tx.send(Ctrl::Shutdown);
        let _ = self.child.kill();
    }
}

/// 启动一个 pane：PTY + shell + 专属 actor 线程。
pub fn spawn(config: PaneConfig) -> std::io::Result<PaneHandle> {
    let size = GridSize::new(config.cols as usize, config.rows as usize);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let PtyPair { master, slave } = pair;

    let shell = config
        .shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/bash".to_string());
    let mut cmd = CommandBuilder::new(&shell);
    for arg in &config.args {
        cmd.arg(arg);
    }
    if let Some(cwd) = &config.cwd {
        cmd.cwd(cwd);
    }
    for (k, v) in &config.env {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let child = slave
        .spawn_command(cmd)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // slave 必须立刻关闭，否则 PTY 永远不会 EOF。
    drop(slave);

    let mut reader = master
        .try_clone_reader()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut writer = master
        .take_writer()
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // ---- 有界通道 ----
    let (input_tx, input_rx) = bounded::<Vec<u8>>(INPUT_QUEUE);
    let (pty_tx, pty_rx) = bounded::<Vec<u8>>(PTY_QUEUE_CHUNKS);
    let (ctrl_tx, ctrl_rx) = bounded::<Ctrl>(CTRL_QUEUE);
    let (pull_tx, pull_rx) = bounded::<Box<Frame>>(1);
    let (write_tx, write_rx) = bounded::<Vec<u8>>(WRITE_QUEUE);
    let (events_sink, events_rx) = EventSink::new(EVENT_QUEUE);
    let (wake_signal, wake_rx) = WakeSignal::new();

    let stats = Arc::new(PaneStats::default());
    let write_dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let shared_palette = Arc::new(RwLock::new(config.palette.clone()));
    let listener = Listener {
        wake: wake_signal,
        events: events_sink,
        pty_write: write_tx.clone(),
        write_dropped: write_dropped.clone(),
        palette: shared_palette.clone(),
    };

    // ---- PTY 读线程：只 read + 有界 send ----
    let ctrl_tx_reader = ctrl_tx.clone();
    let reader_thread = thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // 有界阻塞：背压回内核，绝不丢字节、绝不做 UI 工作。
                        if pty_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = ctrl_tx_reader.send(Ctrl::PtyClosed);
        })?;

    // ---- PTY 写线程 ----
    let writer_thread = thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || {
            while let Ok(bytes) = write_rx.recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        })?;

    // ---- actor 线程 ----
    let term_config = TermConfig {
        scrolling_history: config.scrollback,
        // OSC 52 由 core 的 OscScanner 统一处理（带 1 MiB 上限并转成 CoreEvent）；
        // 关掉 alacritty 内建实现，避免同一条序列被写两次。
        osc52: Osc52::Disabled,
        ..TermConfig::default()
    };
    let term = Term::new(term_config, &size, listener.clone());
    let palette = shared_palette.clone();
    let stats_actor = stats.clone();
    let mode_bits = Arc::new(std::sync::atomic::AtomicU32::new(term.mode().bits()));
    let c0 = term.grid().cursor.point;
    let cursor_bits = Arc::new(std::sync::atomic::AtomicU64::new(
        ((c0.line.0 as u64) << 32) | (c0.column.0 as u64 & 0xffff),
    ));
    let scroll = Arc::new(std::sync::atomic::AtomicU64::new(pack_scroll(
        term.grid().display_offset(),
        term.grid().history_size(),
    )));

    let actor = Actor {
        term,
        processor: Processor::new(),
        osc: OscScanner::new(),
        input_rx,
        pty_rx,
        ctrl_rx,
        pull_tx,
        palette,
        stats: stats_actor,
        master,
        size,
        wake: listener.wake.clone(),
        events: listener.events.clone(),
        write_tx: write_tx.clone(),
        mode_bits: mode_bits.clone(),
        cursor_bits: cursor_bits.clone(),
        scroll: scroll.clone(),
        sel_anchor: None,
        pty_closed: false,
        force_full: false,
        refresh_zerowidth: false,
        search_span: None,
        scroll_on_output: config.scroll_on_output,
    };
    let actor_thread = thread::Builder::new()
        .name("term-actor".into())
        .spawn(move || actor.run())?;

    Ok(PaneHandle {
        input_tx,
        ctrl_tx,
        pull_rx,
        events_rx,
        wake_rx,
        stats,
        mode_bits,
        cursor_bits,
        scroll,
        child,
        threads: vec![reader_thread, writer_thread, actor_thread],
        grid: size,
        exited: false,
    })
}

/// 转义正则元字符：搜索框按「纯文本」语义处理。
fn regex_escape(query: &str) -> String {
    const SPECIAL: &str = "\\^$.|?*+()[]{}";
    let mut out = String::with_capacity(query.len() + 8);
    for c in query.chars() {
        if SPECIAL.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn selection_span<T: EventListener>(term: &Term<T>, rows: usize, offset: usize) -> Option<Span> {
    let range = term.selection.as_ref()?.to_range(term)?;
    let off = offset as i32;
    let start_line = range.start.line.0 + off;
    let end_line = range.end.line.0 + off;
    if end_line < 0 || start_line >= rows as i32 {
        return None;
    }
    let clamp = |l: i32| l.clamp(0, rows as i32 - 1) as u16;
    Some(Span {
        start_line: clamp(start_line),
        start_col: range.start.column.0 as u16,
        end_line: clamp(end_line),
        end_col: range.end.column.0 as u16,
    })
}

struct Actor {
    term: Term<Listener>,
    processor: Processor,
    /// 通知/进度/OSC 7 扫描：alacritty 不暴露 OSC 9/777，需旁路扫原始字节。
    osc: OscScanner,
    input_rx: Receiver<Vec<u8>>,
    pty_rx: Receiver<Vec<u8>>,
    ctrl_rx: Receiver<Ctrl>,
    pull_tx: Sender<Box<Frame>>,
    palette: Arc<RwLock<Palette>>,
    stats: Arc<PaneStats>,
    master: Box<dyn MasterPty + Send>,
    size: GridSize,
    wake: WakeSignal,
    events: EventSink,
    write_tx: Sender<Vec<u8>>,
    /// 模式位快照，供 UI 无阻塞查询（见 [`PaneHandle::mode`]）。
    mode_bits: Arc<std::sync::atomic::AtomicU32>,
    /// 光标快照，供 IME/焦点同步（不依赖 UI 拉帧）。
    cursor_bits: Arc<std::sync::atomic::AtomicU64>,
    /// 滚动位置快照（打包 offset/history），供 UI 无阻塞查询。
    scroll: Arc<std::sync::atomic::AtomicU64>,
    /// 选区锚点（网格坐标）；反向选择时用来保持两端 side 一致。
    sel_anchor: Option<Point>,
    pty_closed: bool,
    force_full: bool,
    /// 零宽字符到达后要求下一次整帧重取时扫描组合字符（见 feed）。
    refresh_zerowidth: bool,
    search_span: Option<Span>,
    /// 收到输出后是否强制回到底部（GNOME scroll-on-output）。
    scroll_on_output: bool,
}

impl Actor {
    fn run(mut self) {
        let input = self.input_rx.clone();
        let pty = self.pty_rx.clone();
        let ctrl = self.ctrl_rx.clone();

        loop {
            // 输入优先：任何洪峰下按键都不排在几 MB 数据后面。
            while let Ok(bytes) = input.try_recv() {
                self.write_input(&bytes);
            }

            if self.pty_closed {
                crossbeam_channel::select! {
                    recv(input) -> msg => if let Ok(b) = msg { self.write_input(&b); },
                    recv(ctrl) -> msg => match msg {
                        Ok(c) => if !self.handle(c) { break },
                        Err(_) => break,
                    },
                }
            } else {
                crossbeam_channel::select! {
                    recv(input) -> msg => if let Ok(b) = msg { self.write_input(&b); },
                    recv(ctrl) -> msg => match msg {
                        Ok(c) => if !self.handle(c) { break },
                        Err(_) => break,
                    },
                    recv(pty) -> msg => match msg {
                        Ok(b) => { self.feed(&b); self.wake.notify(); },
                        Err(_) => self.pty_closed = true,
                    },
                }
            }
            self.store_cursor();
            // 每轮处理完刷新一次滚动快照：输出增长历史、用户滚动、搜索跳转、
            // resize 改变历史长度都会走到这里；一次 relaxed store 极便宜。
            self.sync_scroll();
        }
    }

    /// 把 actor 里的真实滚动状态发布给 UI 的无锁快照。
    #[inline]
    fn sync_scroll(&self) {
        self.scroll.store(
            pack_scroll(
                self.term.grid().display_offset(),
                self.term.grid().history_size(),
            ),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// 用户输入必须写进 PTY（交给 shell 处理），绝不能喂给 VT 解析器。
    fn write_input(&self, bytes: &[u8]) {
        if self.write_tx.try_send(bytes.to_vec()).is_err() {
            // 有界队列满：按键不能丢，退化为阻塞发送
            let _ = self.write_tx.send(bytes.to_vec());
        }
    }

    #[inline]
    fn store_cursor(&self) {
        let c = self.term.grid().cursor.point;
        self.cursor_bits.store(
            ((c.line.0 as u64) << 32) | (c.column.0 as u64 & 0xffff),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    #[inline]
    fn feed(&mut self, bytes: &[u8]) {
        let start = Instant::now();
        // 组合音标/ZWJ/VS 是零宽字符，alacritty 把它们挂到前一个格子上但**不标
        // damage**（见 Term::input 的 width == 0 分支）。如果它们单独成批到达，
        // 按脏行拉帧会永远看不到；这里检测到零宽码位就下一帧整屏重取。
        if has_zero_width_chars(bytes) {
            self.force_full = true;
            // 组合字符不产生 damage，必须整帧重取并扫描零宽表，才能发现它挂到哪一格。
            self.refresh_zerowidth = true;
        }
        // 先扫通知类 OSC/BEL：这些序列 vte 内部不转发给 EventListener。
        let mut notif_events = Vec::new();
        self.osc.feed(bytes, &mut notif_events);
        for event in notif_events {
            self.events.send(event);
        }
        self.processor.advance(&mut self.term, bytes);
        // GNOME scroll-on-output：开启时任何输出都把视口拉回最新（即使已开启
        // scroll_on_output 的用户明确选择“输出即跟随”）。默认 false 时交给
        // alacritty：用户上滚后 `Grid::scroll_up` 会递增 display_offset，视图
        // 保持在原内容上，不会被新行抢走。
        if self.scroll_on_output {
            self.term.scroll_display(Scroll::Bottom);
        }
        let nanos = start.elapsed().as_nanos() as u64;
        use std::sync::atomic::Ordering::Relaxed;
        self.stats.bytes_parsed.fetch_add(bytes.len() as u64, Relaxed);
        self.stats.parse_nanos.fetch_add(nanos, Relaxed);
        self.stats.parse_batches.fetch_add(1, Relaxed);
        self.stats.wakeups.fetch_add(1, Relaxed);
        // 鼠标模式（1000/1002/1003）常随 TUI 启动一起打开，UI 需要马上看到，
        // 不能等下一次拉帧，所以每次解析后刷新快照。
        self.mode_bits.store(self.term.mode().bits(), Relaxed);
        self.store_cursor();
    }

    fn viewport_point(&self, line: u16, col: u16) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        let rows = self.term.screen_lines() as i32;
        let cols = self.term.columns();
        let line = (line as i32).min(rows - 1);
        let col = (col as usize).min(cols.saturating_sub(1));
        Point::new(Line(line - offset), Column(col))
    }

    fn search(&mut self, query: &str, forward: bool) -> Option<SearchHit> {
        if query.is_empty() {
            self.search_span = None;
            self.wake.notify();
            return None;
        }
        let mut regex = RegexSearch::new(&regex_escape(query)).ok()?;
        let direction = if forward { Direction::Right } else { Direction::Left };
        let offset = self.term.grid().display_offset() as i32;
        let origin = match self.search_span {
            Some(span) if forward => Point::new(
                Line(span.end_line as i32 - offset),
                Column(span.end_col as usize + 1),
            ),
            Some(span) => Point::new(
                Line(span.start_line as i32 - offset),
                Column(span.start_col as usize),
            ),
            None => self.term.grid().cursor.point,
        };

        let found = self.term.search_next(&mut regex, origin, direction, Side::Left, None)?;
        let (start, end) = (*found.start(), *found.end());

        // 命中不在视口内就滚动过去
        let rows = self.term.screen_lines() as i32;
        let mut line = start.line.0 + self.term.grid().display_offset() as i32;
        if line < 0 {
            self.term.scroll_display(Scroll::Delta(-line));
        } else if line >= rows {
            self.term.scroll_display(Scroll::Delta(-(line - rows + 1)));
        }
        let offset = self.term.grid().display_offset() as i32;
        line = start.line.0 + offset;
        if !(0..rows).contains(&line) {
            return None;
        }

        let end_line = (end.line.0 + offset).clamp(0, rows - 1) as u16;
        let span = Span {
            start_line: line as u16,
            start_col: start.column.0 as u16,
            end_line,
            end_col: end.column.0 as u16,
        };
        self.search_span = Some(span);
        self.wake.notify();
        Some(SearchHit { line: span.start_line, start_col: span.start_col, end_col: span.end_col })
    }

    fn handle(&mut self, ctrl: Ctrl) -> bool {
        match ctrl {
            Ctrl::Pull(mut frame) => {
                self.fill(&mut frame);
                // UI 正在等这一帧；先发布同一时刻的滚动快照，避免 UI 拿到帧后
                // 读到上一轮的 offset（滚动条会滞后一帧）。容量 1，不会堆积。
                self.sync_scroll();
                let _ = self.pull_tx.send(frame);
                true
            },
            Ctrl::Scroll(lines) => {
                if lines != 0 {
                    self.term.scroll_display(Scroll::Delta(lines));
                    self.wake.notify();
                }
                true
            },
            Ctrl::ScrollTo(offset) => {
                let current = self.term.grid().display_offset() as i64;
                let target = offset.min(self.term.grid().history_size()) as i64;
                let delta = (target - current).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                if delta != 0 {
                    self.term.scroll_display(Scroll::Delta(delta));
                    self.wake.notify();
                }
                true
            },
            Ctrl::SetScrollOnOutput(enabled) => {
                self.scroll_on_output = enabled;
                true
            },
            Ctrl::SelectionStart { line, col } => {
                let point = self.viewport_point(line, col);
                self.sel_anchor = Some(point);
                self.term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
                self.wake.notify();
                true
            },
            Ctrl::SelectionExtend { line, col } => {
                let point = self.viewport_point(line, col);
                let anchor = *self.sel_anchor.get_or_insert(point);
                // 选区两端都用「包含该格」的 side：起点固定 Side::Left、终点固定 Side::Right，
                // 谁先谁后由坐标决定。alacritty 的 to_range 在「起点是 Right / 终点是 Left」时
                // 会把两端各内缩一格——若按拖动方向固定传 Right，反向拖选会同时丢掉行首和锚点格，
                // 最左边的字母就永远选不上。
                let (start, end) =
                    if point <= anchor { (point, anchor) } else { (anchor, point) };
                let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
                if start != end {
                    sel.update(end, Side::Right);
                }
                self.term.selection = Some(sel);
                self.wake.notify();
                true
            },
            Ctrl::SelectionClear => {
                self.sel_anchor = None;
                if self.term.selection.is_some() {
                    self.term.selection = None;
                    self.wake.notify();
                }
                true
            },
            Ctrl::Mouse(ev) => {
                // 门禁在 actor 里：UI 可能因为模式刚变化而多转发一次，
                // 这里以解析器最新的模式为准，未启用则静默丢弃（不写 PTY）。
                if let Some(bytes) = mouse::encode(ev, *self.term.mode()) {
                    self.write_input(&bytes);
                }
                true
            },
            Ctrl::SelectLine(line) => {
                let rows = self.term.screen_lines();
                let cols = self.term.columns();
                if line < 0 || line as usize >= rows || cols == 0 {
                    return true;
                }
                let offset = self.term.grid().display_offset() as i32;
                let grid_line = Line(line - offset);
                // 行尾空白不纳入选区：从后往前找最后一个非空格。
                let mut end_col = 0usize;
                {
                    let row = &self.term.grid()[grid_line];
                    for c in 0..cols {
                        if row[Column(c)].c != ' ' {
                            end_col = c;
                        }
                    }
                }
                let start = Point::new(grid_line, Column(0));
                let end = Point::new(grid_line, Column(end_col));
                let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
                if start != end {
                    sel.update(end, Side::Right);
                }
                self.term.selection = Some(sel);
                self.sel_anchor = Some(start);
                // 选区高亮要立刻重绘
                self.force_full = true;
                self.wake.notify();
                true
            },
            Ctrl::ClearSelection => {
                self.term.selection = None;
                self.force_full = true;
                self.wake.notify();
                true
            },
            Ctrl::SelectAll => {
                let rows = self.term.screen_lines();
                let cols = self.term.columns();
                if rows == 0 || cols == 0 {
                    return true;
                }
                // 视口坐标 → 网格坐标（滚动历史可见时 display_offset != 0）
                let offset = self.term.grid().display_offset() as i32;
                let start = Point::new(Line(-offset), Column(0));
                let end = Point::new(Line(rows as i32 - 1 - offset), Column(cols - 1));
                let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
                if start != end {
                    sel.update(end, Side::Right);
                }
                self.term.selection = Some(sel);
                self.sel_anchor = Some(start);
                self.force_full = true;
                self.wake.notify();
                true
            },
            Ctrl::ClearScreen => {
                // All = 清屏，Saved = 清回滚；两刀下去等价于终端里的 Clear
                self.term.clear_screen(ClearMode::All);
                self.term.clear_screen(ClearMode::Saved);
                self.term.selection = None;
                self.sel_anchor = None;
                self.force_full = true;
                self.wake.notify();
                true
            },
            Ctrl::CopySelection(reply) => {
                let text = self.term.selection_to_string().filter(|s| !s.is_empty());
                let _ = reply.send(text);
                true
            },
            Ctrl::SearchClear => {
                self.search_span = None;
                self.wake.notify();
                true
            },
            Ctrl::Search { query, forward, reply } => {
                let hit = self.search(&query, forward);
                let _ = reply.send(hit);
                true
            },
            Ctrl::SetTheme(palette) => {
                *self.palette.write() = palette;
                self.force_full = true;
                self.wake.notify();
                true
            },
            Ctrl::Resize { cols, rows } => {
                self.size = GridSize::new(cols as usize, rows as usize);
                self.term.resize(self.size);
                let _ = self.master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                self.wake.notify();
                true
            },
            Ctrl::PtyClosed => {
                // 关键：EOF 之后 PTY 队列里可能还压着若干块数据，必须排空再收尾。
                while let Ok(bytes) = self.pty_rx.try_recv() {
                    self.feed(&bytes);
                }
                // 不要立刻退出：子进程退出后 UI 仍可能拉取最后一帧并展示退出状态。
                self.pty_closed = true;
                self.events.send(CoreEvent::Exited);
                true
            },
            Ctrl::Shutdown => false,
        }
    }

    /// 把 `Term` 的 damage 转成 UI 可消费的帧。
    fn fill(&mut self, frame: &mut Frame) {
        use std::sync::atomic::Ordering::Relaxed;
        let start = Instant::now();
        fill_frame(
            &mut self.term,
            frame,
            &self.palette.read(),
            self.force_full,
            self.search_span,
            self.refresh_zerowidth,
        );
        self.force_full = false;
        self.refresh_zerowidth = false;
        let nanos = start.elapsed().as_nanos() as u64;
        self.stats.pulls.fetch_add(1, Relaxed);
        self.stats.fill_nanos.fetch_add(nanos, Relaxed);
        self.stats.cells_copied.fetch_add(frame.copied_cells as u64, Relaxed);
    }
}

/// 本批字节里是否有零宽码位（组合音标、ZWJ、变体选择符等）。
///
/// alacritty 的 `width() == 0` 分支只把字符挂到前一格，不产生 damage；调用方
/// 据此强制一次整帧刷新，避免后到的组合字符丢失。宽度判定用与 alacritty 相同
/// 的 unicode-width 表，避免两边对“零宽”的定义不一致。
pub(crate) fn has_zero_width_chars(bytes: &[u8]) -> bool {
    use unicode_width::UnicodeWidthChar;

    if bytes.is_ascii() {
        return false;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        // 只有合法前缀能解码；PTY 在 UTF-8 中间切块时尾部的半个字符交给下一批，
        // 这里不能因为“尾部不完整”就整帧刷新（否则大段中文输出每块都全刷）。
        Err(e) => match std::str::from_utf8(&bytes[..e.valid_up_to()]) {
            Ok(text) => text,
            Err(_) => return false,
        },
    };
    text.chars().any(|c| c.width() == Some(0))
}

/// 把 `Term` 的 damage 转成 UI 可消费的帧（actor 与 bench 共用）。
pub fn fill_frame<T: EventListener>(
    term: &mut Term<T>,
    frame: &mut Frame,
    palette: &Palette,
    force_full: bool,
    search: Option<Span>,
    refresh_zerowidth: bool,
) {
    let cols = term.columns();
    let rows = term.screen_lines();
    // 组合字符是极少数：没有零宽输入、且帧里本来就没有记录时，整屏拷贝保持
    // 无分支热路径（否则 200x50 整屏 pull 会慢一倍）。只有确实可能有组合字符时
    // 才多读每格的 `Cell::zerowidth()`。
    let had_zerowidth = frame.has_zerowidth();
    frame.ensure_size(cols, rows);

    frame.generation += 1;
    frame.damage.clear();
    frame.full = false;

    let display_offset = term.grid().display_offset();
    // 滚动历史可见时行号与视口不再一一对应；配色切换等场景要求强制整屏。
    let mut full = force_full || display_offset != 0;

    if !full {
        {
            match term.damage() {
                TermDamage::Full => full = true,
                TermDamage::Partial(iter) => {
                    for d in iter {
                        if d.line < rows {
                            let left = d.left.min(cols.saturating_sub(1));
                            let right = d.right.min(cols.saturating_sub(1));
                            if left <= right {
                                frame.damage.push(LineDamage {
                                    line: d.line as u16,
                                    left: left as u16,
                                    right: right as u16,
                                });
                            }
                        }
                    }
                },
            }
        }
        term.reset_damage();
    } else {
        let _ = term.damage();
        term.reset_damage();
    }

    frame.full = full;
    frame.display_offset = display_offset;
    frame.mode = term.mode().bits();
    frame.copied_cells = 0;

    {
        let grid = term.grid();
        if full {
            frame.damage.clear();
            frame.reset_links();
            frame.clear_zerowidth();
            // 视口行 -> grid 行：display_offset > 0 时顶部是 Line(-offset)
            let origin = -(display_offset as i32);
            let scan_zerowidth = refresh_zerowidth || had_zerowidth;
            if scan_zerowidth {
                for row in 0..rows {
                    let line = Line(row as i32 + origin);
                    let base = row * cols;
                    for col in 0..cols {
                        let cell = &grid[Point::new(line, Column(col))];
                        let data = CellData::from_cell(cell, palette);
                        let link = cell.hyperlink();
                        let zerowidth = cell.zerowidth().unwrap_or(&[]);
                        if zerowidth.is_empty() {
                            frame.put(base + col, data, link.as_ref().map(|h| h.uri()));
                        } else {
                            frame.put_zerowidth(
                                base + col,
                                data,
                                link.as_ref().map(|h| h.uri()),
                                zerowidth,
                            );
                        }
                    }
                }
            } else {
                // 热路径：与加入零宽支持前完全一致，避免为罕见组合字符拖慢整屏。
                for row in 0..rows {
                    let line = Line(row as i32 + origin);
                    let base = row * cols;
                    for col in 0..cols {
                        let cell = &grid[Point::new(line, Column(col))];
                        let link = cell.hyperlink();
                        frame.put(
                            base + col,
                            CellData::from_cell(cell, palette),
                            link.as_ref().map(|h| h.uri()),
                        );
                    }
                }
            }
            frame.copied_cells = (rows * cols) as u32;
        } else {
            for i in 0..frame.damage.len() {
                let d = frame.damage[i];
                let line = Line(d.line as i32);
                let base = d.line as usize * cols;
                for col in d.left as usize..=d.right as usize {
                    let cell = &grid[Point::new(line, Column(col))];
                    let data = CellData::from_cell(cell, palette);
                    let link = cell.hyperlink();
                    let zerowidth = cell.zerowidth().unwrap_or(&[]);
                    if zerowidth.is_empty() {
                        frame.put(base + col, data, link.as_ref().map(|h| h.uri()));
                    } else {
                        frame.put_zerowidth(
                            base + col,
                            data,
                            link.as_ref().map(|h| h.uri()),
                            zerowidth,
                        );
                    }
                }
                frame.copied_cells += d.right as u32 - d.left as u32 + 1;
            }
        }
    }

    let offset = display_offset;
    frame.selection = selection_span(term, rows, offset);
    frame.search = search;

    let cursor = term.grid().cursor.point;
    let cursor_row = cursor.line.0 + display_offset as i32;
    frame.cursor = CursorState {
        line: cursor_row.clamp(0, rows.saturating_sub(1) as i32) as u16,
        col: (cursor.column.0 as u16).min(cols.saturating_sub(1) as u16),
        shape: CursorShape::from_vte(term.cursor_style().shape),
        visible: term.mode().contains(TermMode::SHOW_CURSOR),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_position_pack_roundtrip() {
        for &(offset, history) in &[(0usize, 0usize), (3, 100), (100, 100), (0, 12345)] {
            let (o, h) = unpack_scroll(pack_scroll(offset, history));
            assert_eq!((o, h), (offset, history));
        }
        // 超大值饱和而不是回绕（实际 scrollback 远小于 u32::MAX）
        let bits = pack_scroll(u32::MAX as usize + 9, u32::MAX as usize + 7);
        assert_eq!(unpack_scroll(bits), (u32::MAX as usize, u32::MAX as usize));
    }

    #[test]
    fn scroll_position_requires_history() {
        let none = ScrollPosition { offset: 0, history: 0, rows: 24 };
        assert!(!none.scrollable());
        assert_eq!(none.thumb(0.1), None);
        assert_eq!(none.total(), 24);
        assert_eq!(none.offset_for_track_fraction(0.5, 0.1), 0);

        let empty_view = ScrollPosition { offset: 0, history: 100, rows: 0 };
        assert!(!empty_view.scrollable());
        assert_eq!(empty_view.thumb(0.1), None);
    }

    #[test]
    fn scroll_thumb_geometry_endpoints_and_min_size() {
        let pos = ScrollPosition { offset: 0, history: 100, rows: 20 };
        let (top_bottom, h) = pos.thumb(0.0).expect("scrollable");
        // 视口占 20/120，thumb 高 1/6；在底部时 thumb 顶 = 1 - h。
        assert!((h - 20.0 / 120.0).abs() < 1e-6, "thumb height={h}");
        assert!((top_bottom - (1.0 - h)).abs() < 1e-6, "bottom top={top_bottom}");

        let (top_top, _) = ScrollPosition { offset: 100, ..pos }.thumb(0.0).unwrap();
        assert!(top_top.abs() < 1e-6, "顶部 thumb 顶必须为 0: {top_top}");

        // 长历史下 thumb 被最小高度托底。
        let long = ScrollPosition { offset: 0, history: 100_000, rows: 20 };
        let (_, min_h) = long.thumb(0.08).unwrap();
        assert!((min_h - 0.08).abs() < 1e-6, "min thumb={min_h}");
        let (_, tiny) = long.thumb(0.0).unwrap();
        assert!(tiny < 0.001, "没有 min 时应非常小: {tiny}");
    }

    #[test]
    fn scroll_track_fraction_maps_to_top_and_bottom() {
        let pos = ScrollPosition { offset: 10, history: 100, rows: 20 };
        let min = 0.08;
        assert_eq!(pos.offset_for_track_fraction(0.0, min), 100, "点顶部 = 最旧");
        assert_eq!(pos.offset_for_track_fraction(1.0, min), 0, "点底部 = 最新");
        assert_eq!(pos.offset_for_track_fraction(-5.0, min), 100, "越界夹取");
        assert_eq!(pos.offset_for_track_fraction(5.0, min), 0, "越界夹取");

        // thumb 顶 ↔ offset 往返：thumb(top) 后反推应回到原 offset。
        let (top, _) = pos.thumb(min).unwrap();
        assert_eq!(pos.offset_for_thumb_top(top, min), 10, "thumb 顶 ↔ offset 往返");
    }

    #[test]
    fn scroll_thumb_offset_roundtrip_is_monotonic() {
        let pos = ScrollPosition { offset: 0, history: 500, rows: 30 };
        let min = 0.05;
        let mut last = usize::MAX;
        for i in 0..=20 {
            let top = i as f32 / 20.0;
            let offset = pos.offset_for_thumb_top(top, min);
            assert!(offset <= last, "top 增大时 offset 必须单调下降: {offset} > {last}");
            last = offset;
        }
        assert_eq!(pos.offset_for_thumb_top(0.0, min), 500);
        assert_eq!(pos.offset_for_thumb_top(1.0, min), 0);
    }
}
