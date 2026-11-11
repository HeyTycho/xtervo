// SPDX-License-Identifier: GPL-3.0-only
//! 单 pane 的 PTY 会话与终端 actor。
//!
//! 线程模型（严格对应冻结架构）：
//!
//! ```text
//!  UI 线程 ──input(直接推入有界字节 backlog)──┐
//!  actor 线程（鼠标编码/PtyWrite 回应）───────┤
//!                                            ├──► PTY 写线程 ◄── write backlog
//!  PTY 读线程 ──data(有界64)──► actor: Term + vte::Processor + damage
//!                                       │
//!                                       ├─ wake(容量1，可合并) ──► UI 重绘
//!                                       ├─ events(有界256) ──────► 标题/bell/退出
//!                                       │    └─ event_wake(容量1) ► UI 立即 drain
//!                                       └─ pull_req/pull_resp ──► UI 主动拉帧
//! ```
//!
//! 关键点：
//! - UI 的 `input()` 永不阻塞：直接进按字节数限界的写 backlog（16 MiB），
//!   慢 shell 也只会在 backlog 满后明确丢弃并计数，不会把 UI/actor 拖死；
//! - actor 只负责解析输出与编码鼠标，不再作为键盘输入的中转站；
//! - PTY 读线程只做 `read` + 有界 `send`，无任何 UI 调用；
//! - 内容变化走容量 1 的唤醒通道，输出洪峰只会被合并，不会堆积；
//! - 语义事件另有容量 1 的唤醒：UI 收到即 drain 事件队列；400ms timer 只是
//!   兜底/周期任务，不是语义事件的主延迟路径。
//! - 帧数据由 UI 侧持有 `Box<Frame>` 往返复用，稳态零分配；`pull` 硬超时。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Direction, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{Config as TermConfig, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Handler, Processor, Rgb};
use crossbeam_channel::{bounded, Receiver, SendTimeoutError, Sender, TrySendError};
use parking_lot::{Condvar, Mutex, RwLock};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtyPair, PtySize};

use crate::event::{ClipboardSelection, CoreEvent, EventSink, PaneStats, WakeSignal};
use crate::frame::{
    CellData, CursorShape, CursorState, Frame, ImageCrop, ImagePlacementMeta, InlineImage,
    LineDamage, Span,
};
use crate::iip::{IipEvent, IipFilter, IipImage};
use crate::keyboard::{KeyboardAction, KeyboardFlags, KeyboardScanner};
use crate::kitty_graphics::{
    CompositePolicy, CursorMovement, DeleteCommand, DeleteTarget, KittyCommand, KittyError,
    KittyEvent, KittyParser, PlacementCommand, SourceRect, TransmitCommand,
};
use crate::mouse::{self, MouseEvent};
use crate::osc::OscScanner;
use crate::palette::Palette;
use crate::sixel::{rewrite_da1, SixelEvent, SixelFilter, SixelImage};

const READ_CHUNK: usize = 32 * 1024;
const PTY_QUEUE_CHUNKS: usize = 64;
const CTRL_QUEUE: usize = 64;
const EVENT_QUEUE: usize = 256;

/// PTY 写 backlog 的硬上限：慢 shell 不读 stdin 时，最多在内存里压这么多
/// 尚未写进内核的字节；超过就丢弃新块并计数/限频日志。正常输入远小于此值。
///
/// 10 MiB 粘贴 + 后续按键也能完整落在 backlog 内，因此“慢 shell”不会立刻丢
/// 正常输入；只有持续灌入超过 16 MiB 才进入明确的丢弃策略。
const WRITE_BACKLOG_BYTES: usize = 16 * 1024 * 1024;
/// backlog 的块数上限：防止大量 1 字节按键把 VecDeque 撑爆（正常键盘远小于此）。
const WRITE_BACKLOG_CHUNKS: usize = 8192;
/// UI 拉帧的硬超时：actor 忙/异常时返回“跳过本帧”，绝不无限等。
const PULL_TIMEOUT: Duration = Duration::from_millis(100);
/// copy/search/contents 这类同步问 actor 的调用上限；超时返回空结果而不是卡 UI。
const CTRL_REPLY_TIMEOUT: Duration = Duration::from_millis(200);
/// ctrl 通道投递上限：actor 忙时 UI 最多等这么久，超时丢弃并计数（不无限阻塞）。
const CTRL_SEND_TIMEOUT: Duration = Duration::from_millis(50);

/// 搜索命中（视口坐标）。
///
/// `index`/`total` 来自 alacritty 的 `RegexSearch`/`RegexIter` 对整个
/// scrollback + 视口的扫描，**不读 UI 帧**；计数与命中在同一次 actor
/// 请求里完成，UI 不需要再拉帧/扫文本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub line: u16,
    pub start_col: u16,
    pub end_col: u16,
    /// 当前命中在全部匹配中的 1-based 序号；无命中时为 0。
    pub index: usize,
    /// scrollback + 视口内的匹配总数；无命中时为 0。
    pub total: usize,
}

/// 搜索选项：默认大小写不敏感、纯文本。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// true = 区分大小写；false = 大小写不敏感。
    pub match_case: bool,
    /// true = 把 query 当正则；false = 转义成纯文本。
    pub regex: bool,
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

/// PTY 写队列满/已关闭的两种结果。
///
/// `Full` 是真实背压（慢 shell 不读、backlog 到上限），必须计数并限频日志；
/// `Closed` 是 PTY 已结束，静默忽略即可，不应再刷“丢弃”。
enum PushError {
    Full,
    Closed,
}

/// PTY 写的独立有界 backlog：actor / EventListener 只做 O(1) push，永不阻塞。
///
/// 之前 actor 直接把输入 `send` 到有界(256)通道：慢 shell 不读 stdin 时
/// pty-writer 阻塞在 `write_all`，通道一满 actor 就卡住，接着 input 通道被灌满，
/// UI 线程的 `input()` 跟着无限阻塞。这里把“写不进内核”的字节留在**按字节数
/// 限界**的队列里，由 pty-writer 单独消费；生产者永不碰阻塞写。
struct WriteQueue {
    state: Mutex<WriteQueueState>,
    ready: Condvar,
}

struct WriteQueueState {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

impl WriteQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WriteQueueState {
                queue: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        })
    }

    /// 非阻塞入队；超过字节/块上限返回 `Full`（调用方计数+限频日志）。
    fn push(&self, bytes: Vec<u8>) -> Result<(), PushError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock();
        if state.closed {
            return Err(PushError::Closed);
        }
        if state.queue.len() >= WRITE_BACKLOG_CHUNKS
            || state.bytes.saturating_add(bytes.len()) > WRITE_BACKLOG_BYTES
        {
            return Err(PushError::Full);
        }
        state.bytes += bytes.len();
        state.queue.push_back(bytes);
        self.ready.notify_one();
        Ok(())
    }

    /// pty-writer 消费：队列空且未关闭时在 Condvar 上等待（不占 CPU）。
    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock();
        loop {
            if let Some(bytes) = state.queue.pop_front() {
                state.bytes -= bytes.len();
                return Some(bytes);
            }
            if state.closed {
                return None;
            }
            self.ready.wait(&mut state);
        }
    }

    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.queue.clear();
        state.bytes = 0;
        self.ready.notify_all();
    }
}

/// PTY 写 backlog 到上限时的限频计数+日志。
fn note_write_drop(stats: &PaneStats, bytes: usize) {
    let chunks = stats.write_dropped_chunks.fetch_add(1, Ordering::Relaxed) + 1;
    stats.write_dropped_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    if chunks == 1 || chunks % 64 == 0 {
        eprintln!(
            "[pty] 写 backlog 已满（{} MiB），丢弃 {bytes} bytes（累计 {chunks} 块）；慢 shell 背压"
            , WRITE_BACKLOG_BYTES / (1024 * 1024)
        );
    }
}

/// 事件监听器：只做「转发到有界通道」，绝不阻塞解析线程。
#[derive(Clone)]
struct Listener {
    wake: WakeSignal,
    events: EventSink,
    write_queue: Arc<WriteQueue>,
    stats: Arc<PaneStats>,
    /// 当前调色板：OSC 4/10/11/12 查询要按主题回真实颜色。
    palette: Arc<RwLock<Palette>>,
    /// Sixel 开关：开启时把 alacritty 的 Primary DA 回应加上参数 4。
    sixel: Arc<AtomicBool>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => self.events.send(CoreEvent::Title(title)),
            Event::ResetTitle => self.events.send(CoreEvent::ResetTitle),
            // Bell 由 actor 里的 OscScanner 统一上报：alacritty 的 Event::Bell 与它
            // 会重复，且 OSC 9/777 本来就不经过 vte 的 EventListener。
            Event::CursorBlinkingChange => self.events.send(CoreEvent::CursorBlinkChange),
            Event::PtyWrite(mut text) => {
                // alacritty 固定回 `CSI ?6c`。开启 Sixel 时替换为带参数 4 的
                // Primary DA（dsh-tui 等程序据此自动选择 Sixel）；关闭时原样
                // 回 `?6c`，绝不宣告 Sixel，也不触碰二级 DA。
                if let Some(da1) = rewrite_da1(&text, self.sixel.load(Ordering::Relaxed)) {
                    text = da1;
                }
                if !text.is_empty() {
                    let len = text.len();
                    if let Err(PushError::Full) = self.write_queue.push(text.into_bytes()) {
                        note_write_drop(&self.stats, len);
                    }
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
                let len = reply.len();
                if let Err(PushError::Full) = self.write_queue.push(reply.into_bytes()) {
                    note_write_drop(&self.stats, len);
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
    /// 启用 kitty keyboard protocol / xterm modifyOtherKeys 的解析与增强编码。
    ///
    /// false（配置 `keyboard_protocol = "off"`）时：不响应 `CSI ? u`、不统计
    /// modifyOtherKeys、UI 也只会走与旧版本完全一致的字节编码。
    pub keyboard_protocol: bool,
    /// 启用 Sixel 内联图片（解析/解码/渲染 + DA1 宣告参数 4）。
    pub sixel: bool,
    /// Sixel 图片是否跟随滚动历史（默认 true）；false 时锚定在创建时的屏幕行。
    pub sixel_scrolling: bool,
    /// 启用 iTerm2 IIP（OSC 1337 File）内联图片。关闭后不解析/解码 IIP，
    /// 已显示的 IIP 图片立即清除；不影响 Sixel。
    pub iip: bool,
    /// 终端 cell 像素宽度：IIP 的 `width=/height=` 以 cell 为单位时换算像素。
    pub cell_width: u16,
    /// 终端 cell 像素高度：判断图片是否仍与视口相交，并换算 IIP cell 尺寸。
    pub cell_height: u16,
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
            keyboard_protocol: true,
            sixel: true,
            sixel_scrolling: true,
            iip: true,
            cell_width: 8,
            cell_height: 16,
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
    /// 运行时开关 Sixel（设置页改动立即生效）；关闭时清除已有 Sixel 图片。
    SetSixel(bool),
    /// 运行时切换 Sixel 图片是否随滚动历史移动。
    SetSixelScrolling(bool),
    /// 按全局内存预算动态调整本 pane 的 scrollback 行数。
    SetScrollback(usize),
    /// 运行时开关 iTerm2 IIP；关闭时清除已有 IIP 图片（不影响 Sixel）。
    SetIip(bool),
    /// 字号变化后更新 cell 像素尺寸（IIP cell 尺寸换算 + 图片可见性判断）。
    SetCellSize { width: u16, height: u16 },
    /// 按源 ID 取回不可变 RGBA（灯箱与点击命中不依赖可见缓存）。
    ImageRgba { source_id: u64, reply: Sender<Option<(u32, u32, Arc<[u8]>)>> },
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
    /// 复位终端状态（RIS 语义：模式/光标/字符集/滚动区/标题栈等）。
    Reset,
    /// 复位终端状态并显式再清一次回滚缓冲（与 Reset 在 alacritty 下等价）。
    ResetAndClear,
    /// 鼠标上报：终端启用 1000/1002/1003 时转成转义序列写进 PTY。
    Mouse(MouseEvent),
    CopySelection(Sender<Option<String>>),
    Search {
        query: String,
        forward: bool,
        options: SearchOptions,
        reply: Sender<Result<Option<SearchHit>, String>>,
    },
    SearchClear,
    /// 取全部内容（回滚 + 视口），保存到文件用。
    Contents(Sender<Option<String>>),
    /// UI 拉帧：请求 ID 用于丢弃“上一帧超时后才回来”的迟到响应。
    Pull {
        id: u64,
        frame: Box<Frame>,
        /// 上一帧超时/跳帧后强制整屏重取，避免迟到响应消费 damage 造成漏画。
        force_full: bool,
    },
    PtyClosed,
    Shutdown,
}

/// UI 侧持有的单 pane 句柄。
pub struct PaneHandle {
    /// 与 pty-writer 共享的有界字节 backlog：键盘/粘贴直接进入，不经过 actor，
    /// 因此 actor 再慢也不会反压 UI 的输入。
    write_queue: Arc<WriteQueue>,
    ctrl_tx: Sender<Ctrl>,
    pull_rx: Receiver<(u64, Box<Frame>)>,
    events_rx: Receiver<CoreEvent>,
    /// 语义事件唤醒（容量 1，合并）：有事件时 UI 立即 drain，400ms 仅兜底。
    events_wake_rx: Receiver<()>,
    wake_rx: Receiver<()>,
    pub stats: Arc<PaneStats>,
    /// actor 每次喂数据后刷新的模式位快照：UI 需要在**不阻塞**的前提下判断
    /// 当前是否处于鼠标上报模式（决定「转发」还是「本地选词」）。
    mode_bits: Arc<std::sync::atomic::AtomicU32>,
    /// actor 旁路扫描 `CSI > 4 ; n m` 后刷新的 xterm modifyOtherKeys 等级。
    modify_other_keys: Arc<std::sync::atomic::AtomicU8>,
    /// actor 维护的 kitty flags 快照（main/alternate screen 各自独立）。
    keyboard_flags: Arc<std::sync::atomic::AtomicU8>,
    /// 当前终端光标（行 u32 << 32 | 列），actor 每次解析后刷新；不依赖重绘。
    cursor_bits: Arc<std::sync::atomic::AtomicU64>,
    /// 滚动位置快照（打包 offset/history），见 [`PaneHandle::scroll_position`]。
    scroll: Arc<std::sync::atomic::AtomicU64>,
    /// Read Only：true 时 UI 输入/粘贴/鼠标上报都不写 PTY（选区/滚动仍走 ctrl）。
    read_only: Arc<std::sync::atomic::AtomicBool>,
    /// 网格行数快照（high 32 = history_size，low 32 = total_lines），诊断用。
    grid_lines: Arc<std::sync::atomic::AtomicU64>,
    /// 下一次拉帧的请求 ID（只由 UI 线程递增）。
    pull_seq: AtomicU64,
    /// 上一帧是否超时/跳帧；下一次 pull 用它强制整屏，保证不因迟到帧漏画。
    pull_needs_full: AtomicBool,
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

    /// 送键盘输入。**永不阻塞 UI**：直接进 16 MiB 有界写 backlog；只有 backlog
    /// 满（极端慢 shell 持续不读）才丢弃新块并计数/限频日志。
    ///
    /// 正常按键/粘贴（单次 10 MiB 也只是一个队列块）不会丢。顺序保证：同一
    /// 调用成块入队，跨调用按 UI 调用顺序 FIFO；保留下来的块顺序不乱。
    /// Read Only 时直接丢弃，连队列都不进。
    pub fn input(&self, bytes: &[u8]) {
        if self.read_only() {
            return;
        }
        self.enqueue_input(bytes);
    }

    /// 入队一帧输入；返回 false = backlog 满（已计数）或 PTY 已关闭。
    fn enqueue_input(&self, bytes: &[u8]) -> bool {
        match self.write_queue.push(bytes.to_vec()) {
            Ok(()) => true,
            Err(PushError::Full) => {
                note_write_drop(&self.stats, bytes.len());
                false
            },
            Err(PushError::Closed) => false,
        }
    }

    /// 非阻塞输入（满则返回 false），供高频粘贴等场景降级。
    pub fn try_input(&self, bytes: &[u8]) -> bool {
        if self.read_only() {
            return false;
        }
        self.enqueue_input(bytes)
    }

    /// 控制消息投递：最多等 [`CTRL_SEND_TIMEOUT`]，actor 忙/退出时丢弃并计数。
    ///
    /// 返回 false = 未投递。调用方对 copy/search/contents 直接走“超时=空结果”
    /// 分支；滚动/选区等高频操作也可以安全丢弃（下一次事件会覆盖状态）。
    fn send_ctrl(&self, ctrl: Ctrl) -> bool {
        match self.ctrl_tx.send_timeout(ctrl, CTRL_SEND_TIMEOUT) {
            Ok(()) => true,
            Err(SendTimeoutError::Timeout(_)) | Err(SendTimeoutError::Disconnected(_)) => {
                let n = self.stats.ctrl_send_timeouts.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n % 128 == 0 {
                    eprintln!(
                        "[pty] ctrl 队列投递超时（{}ms，累计 {} 次）；UI 不阻塞",
                        CTRL_SEND_TIMEOUT.as_millis(),
                        n
                    );
                }
                false
            },
        }
    }

    /// 切换 Read Only。actor 侧也会检查同一个原子标志，已入队但未写的输入同样丢弃。
    pub fn set_read_only(&self, on: bool) {
        self.read_only.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// 当前是否 Read Only（无锁）。
    pub fn read_only(&self) -> bool {
        self.read_only.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 诊断用：`(history_size, total_lines)`（当前网格；备用屏只含可见行）。
    pub fn grid_lines(&self) -> (usize, usize) {
        let v = self.grid_lines.load(std::sync::atomic::Ordering::Relaxed);
        ((v >> 32) as usize, (v & 0xffff_ffff) as usize)
    }

    /// 滚轮回滚：正数向上翻历史。
    pub fn scroll(&self, lines: i32) {
        self.send_ctrl(Ctrl::Scroll(lines));
    }

    /// 绝对滚动位置（0 = 底部/最新，`history` = 顶部/最旧）。
    ///
    /// 异步：发送后由 actor 在下一轮处理；UI 通过 [`Self::scroll_position`]
    /// 读取新位置并在唤醒后重绘。
    pub fn scroll_to(&self, offset: usize) {
        self.send_ctrl(Ctrl::ScrollTo(offset));
    }

    /// 回到底部（最新内容）；`scroll_on_keystroke` 的按键路径使用。
    ///
    /// 与 [`Self::scroll_to`] 一样只投递异步消息，不阻塞 UI；是否已经在底部
    /// 由调用方读 [`Self::scroll_position`] 判断。
    pub fn scroll_to_bottom(&self) {
        self.send_ctrl(Ctrl::ScrollTo(0));
    }

    /// 运行时切换「输出即回底」策略（异步投递，不阻塞 UI）。
    pub fn set_scroll_on_output(&self, enabled: bool) {
        self.send_ctrl(Ctrl::SetScrollOnOutput(enabled));
    }

    /// 运行时开关 Sixel（异步）。关闭后 actor 不再解码/渲染图片，并清除已有图；
    /// 同时 Listener 的 DA1 回应不再包含参数 4。
    pub fn set_sixel(&self, enabled: bool) {
        self.send_ctrl(Ctrl::SetSixel(enabled));
    }

    /// 运行时切换 Sixel 图片是否跟随滚动历史（异步）。
    pub fn set_sixel_scrolling(&self, scrolling: bool) {
        self.send_ctrl(Ctrl::SetSixelScrolling(scrolling));
    }

    /// 按全局预算动态调整 scrollback 行数（只缩容时立即释放旧行；扩容不恢复已丢历史）。
    pub fn set_scrollback(&self, lines: usize) {
        self.send_ctrl(Ctrl::SetScrollback(lines));
    }

    /// 运行时开关 iTerm2 IIP（异步）。关闭后 actor 不再解码 IIP，并清除
    /// 已有 IIP 图片；Sixel 图片与 DA1 宣告不受影响。
    pub fn set_iip(&self, enabled: bool) {
        self.send_ctrl(Ctrl::SetIip(enabled));
    }

    /// 字号变化后同步 cell 像素尺寸（异步）。IIP 的 cell 宽高换算与图片
    /// 可见性判断都依赖它；不影响已经解码图片的像素尺寸。
    pub fn set_cell_size(&self, width: u16, height: u16) {
        self.send_ctrl(Ctrl::SetCellSize { width, height });
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
    /// Read Only 时返回 false：鼠标点击改为本地选区/滚动，不写 PTY。
    pub fn wants_mouse(&self) -> bool {
        !self.read_only() && mouse::reporting(self.mode())
    }

    /// 是否要求上报「无按键悬停」（1003）。Read Only 时不上报。
    pub fn wants_motion(&self) -> bool {
        !self.read_only() && self.mode().contains(TermMode::MOUSE_MOTION)
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
        self.keyboard_flags().intersects(
            KeyboardFlags::DISAMBIGUATE_ESC_CODES | KeyboardFlags::REPORT_ALL_KEYS_AS_ESC,
        )
    }

    /// 当前 kitty progressive enhancement flags（无锁快照）。
    ///
    /// 只包含本终端实际实现的 5 个 bit；由 actor 的 [`KeyboardScanner`] 维护，
    /// 包含 main/alternate screen 各自的 set/push/pop 状态。
    pub fn keyboard_flags(&self) -> KeyboardFlags {
        KeyboardFlags::from_bits_truncate(self.keyboard_flags.load(Ordering::Relaxed))
    }

    /// 当前 xterm modifyOtherKeys 等级（0 = 关闭，1/2/3）。
    ///
    /// 由 actor 旁路扫描 `CSI > 4 ; n m` 维护；配置
    /// `keyboard_protocol = "off"` 时恒为 0。
    pub fn modify_other_keys(&self) -> u8 {
        self.modify_other_keys.load(Ordering::Relaxed)
    }

    /// 上报一次鼠标事件。未启用鼠标模式 / Read Only 的 pane 自动忽略。
    pub fn mouse(&self, ev: MouseEvent) {
        if self.read_only() {
            return;
        }
        self.send_ctrl(Ctrl::Mouse(ev));
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
        self.send_ctrl(Ctrl::SelectionStart { line, col });
    }

    pub fn selection_extend(&self, line: u16, col: u16) {
        self.send_ctrl(Ctrl::SelectionExtend { line, col });
    }

    pub fn selection_clear(&self) {
        self.send_ctrl(Ctrl::SelectionClear);
    }

    /// 清除选区。
    pub fn clear_selection(&self) {
        self.send_ctrl(Ctrl::ClearSelection);
    }

    /// 选中当前视口（不含回滚历史）。
    pub fn select_all(&self) {
        self.send_ctrl(Ctrl::SelectAll);
    }

    /// 选中视口内第 `line` 行（双击选行；`line` 是视口坐标，0 = 顶行）。
    pub fn select_line(&self, line: i32) {
        self.send_ctrl(Ctrl::SelectLine(line));
    }

    /// 清屏 + 清回滚；下一次拉帧强制整屏。
    pub fn clear_screen(&self) {
        self.send_ctrl(Ctrl::ClearScreen);
    }

    /// 复位终端状态（等同 RIS）：清屏、清回滚、重置模式/光标/字符集/滚动区等。
    ///
    /// alacritty 0.26 的 `Handler::reset_state()` 会同时清空回滚历史，因此本方法
    /// 与 [`Self::reset_and_clear`] 在当前上游版本等价；保留两个入口是为了菜单/
    /// 命令语义与未来上游 API 对齐。
    pub fn reset(&self) {
        self.send_ctrl(Ctrl::Reset);
    }

    /// 复位并清空回滚缓冲（显式语义；当前与 [`Self::reset`] 等价）。
    pub fn reset_and_clear(&self) {
        self.send_ctrl(Ctrl::ResetAndClear);
    }

    /// 取当前选区文本（最多等 actor 200ms；超时返回 None，不阻塞 UI）。
    pub fn copy_selection(&self) -> Option<String> {
        let (tx, rx) = bounded(1);
        if !self.send_ctrl(Ctrl::CopySelection(tx)) {
            return None;
        }
        match rx.recv_timeout(CTRL_REPLY_TIMEOUT) {
            Ok(text) => text,
            Err(_) => {
                self.stats.ctrl_reply_timeouts.fetch_add(1, Ordering::Relaxed);
                eprintln!("[pty] copy_selection 回复超时（{}ms），本次跳过", CTRL_REPLY_TIMEOUT.as_millis());
                None
            },
        }
    }

    /// 按源 ID 取不可变 RGBA。UI 的可见纹理缓存淘汰不影响点击/灯箱；
    /// actor 忙/超时返回 None，不阻塞 UI。
    pub fn image_rgba(&self, source_id: u64) -> Option<(u32, u32, Arc<[u8]>)> {
        let (tx, rx) = bounded(1);
        if !self.send_ctrl(Ctrl::ImageRgba { source_id, reply: tx }) {
            return None;
        }
        match rx.recv_timeout(CTRL_REPLY_TIMEOUT) {
            Ok(value) => value,
            Err(_) => {
                self.stats.ctrl_reply_timeouts.fetch_add(1, Ordering::Relaxed);
                None
            },
        }
    }

    /// 清除搜索高亮。
    pub fn search_clear(&self) {
        self.send_ctrl(Ctrl::SearchClear);
    }

    /// 搜索下一个/上一个命中；命中会滚动到可见并返回视口内高亮区间。
    ///
    /// 兼容旧调用：纯文本、大小写不敏感；非法正则返回 `Err` 请用
    /// [`Self::search_with`]。
    pub fn search(&self, query: &str, forward: bool) -> Option<SearchHit> {
        self.search_with(query, forward, SearchOptions::default()).ok().flatten()
    }

    /// 带选项搜索；非法 regex 返回可展示的错误字符串（不 panic）。
    pub fn search_with(
        &self,
        query: &str,
        forward: bool,
        options: SearchOptions,
    ) -> Result<Option<SearchHit>, String> {
        let (tx, rx) = bounded(1);
        if !self.send_ctrl(Ctrl::Search {
            query: query.to_string(),
            forward,
            options,
            reply: tx,
        }) {
            return Ok(None);
        }
        match rx.recv_timeout(CTRL_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                self.stats.ctrl_reply_timeouts.fetch_add(1, Ordering::Relaxed);
                eprintln!("[pty] search 回复超时（{}ms），本次无结果", CTRL_REPLY_TIMEOUT.as_millis());
                Ok(None)
            },
        }
    }

    /// 取回滚 + 视口全部文本（最多等 actor 200ms；超时返回 None）。
    pub fn contents(&self) -> Option<String> {
        let (tx, rx) = bounded(1);
        if !self.send_ctrl(Ctrl::Contents(tx)) {
            return None;
        }
        match rx.recv_timeout(CTRL_REPLY_TIMEOUT) {
            Ok(text) => text,
            Err(_) => {
                self.stats.ctrl_reply_timeouts.fetch_add(1, Ordering::Relaxed);
                eprintln!("[pty] contents 回复超时（{}ms），本次跳过", CTRL_REPLY_TIMEOUT.as_millis());
                None
            },
        }
    }

    /// 切换配色：actor 侧置 full damage，下一次拉帧整屏重取。
    pub fn set_theme(&self, palette: Palette) {
        self.send_ctrl(Ctrl::SetTheme(palette));
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid = GridSize::new(cols as usize, rows as usize);
        self.send_ctrl(Ctrl::Resize { cols, rows });
    }

    /// 主动拉取一帧：把旧缓冲送进去，拿回填充后的同一块内存。
    ///
    /// 最多等 [`PULL_TIMEOUT`]；actor 忙/异常时返回空帧并标记 [`Frame::skipped`]，
    /// 由 app 跳过本帧重绘（旧画面留在屏幕上）。下一次 pull 强制整屏，
    /// 迟到响应消费掉的 damage 不会导致漏画。UI 线程永不无限等 PTY。
    pub fn pull(&self, frame: Box<Frame>) -> Box<Frame> {
        let id = self.pull_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let force_full = self.pull_needs_full.swap(false, Ordering::SeqCst);
        if !self.send_ctrl(Ctrl::Pull {
            id,
            frame,
            force_full,
        }) {
            // actor 已退出/ctrl 队列堵住：返回跳过帧，UI 不阻塞。
            self.stats.pull_timeouts.fetch_add(1, Ordering::Relaxed);
            return self.skipped_frame();
        }

        let deadline = Instant::now() + PULL_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.pull_rx.recv_timeout(remaining) {
                Ok((rid, mut filled)) => {
                    if rid == id {
                        filled.skipped = false;
                        return filled;
                    }
                    // 上一帧超时后才回来的响应：丢弃，它的 damage 由 force_full 补回。
                    self.stats.pull_stale_responses.fetch_add(1, Ordering::Relaxed);
                },
                Err(_) => break,
            }
        }

        self.pull_needs_full.store(true, Ordering::SeqCst);
        let timeouts = self.stats.pull_timeouts.fetch_add(1, Ordering::Relaxed) + 1;
        self.stats.pull_skipped_frames.fetch_add(1, Ordering::Relaxed);
        if timeouts == 1 || timeouts % 128 == 0 {
            eprintln!(
                "[pty] 拉帧超时 {}ms，跳过本帧（累计 {} 次）；UI 不阻塞",
                PULL_TIMEOUT.as_millis(),
                timeouts
            );
        }
        self.skipped_frame()
    }

    /// 超时/actor 退出时交给 UI 的占位帧：`skipped = true`，app 不应重绘。
    fn skipped_frame(&self) -> Box<Frame> {
        let mut empty = Box::new(Frame::new(self.grid.cols, self.grid.rows));
        empty.skipped = true;
        empty.full = true;
        // 帧内容不可用，但模式位是 UI 的异步快照：保留它，避免 skip 期间
        // 粘贴误判 bracketed paste 等模式。
        empty.mode = self.mode_bits.load(Ordering::Relaxed);
        empty
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

    /// 语义事件唤醒通道的克隆。
    ///
    /// 与 [`Self::wake_receiver`] 分开：内容变化走拉帧 + 重绘，标题/Bell/OSC52
    /// 等语义事件走 UI 事件处理，两者不互相冒充。容量 1，洪峰只合并通知。
    pub fn event_wake_receiver(&self) -> Receiver<()> {
        self.events_wake_rx.clone()
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// 子进程退出码/信号（非阻塞）。进程仍在运行/无法取到返回 `None`。
    ///
    /// `Exited` 事件只表示 PTY 关闭，UI 需要退出状态给用户看（SSH 断线时
    /// 常见 255）；这里直接 `try_wait` 拿 OS 状态，不阻塞事件循环。
    pub fn try_exit_code(&mut self) -> Option<(u32, Option<String>)> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exited = true;
                Some((status.exit_code(), status.signal().map(str::to_string)))
            },
            _ => None,
        }
    }

    pub fn kill(&mut self) {
        self.send_ctrl(Ctrl::Shutdown);
        let _ = self.child.kill();
    }
}

impl Drop for PaneHandle {
    fn drop(&mut self) {
        self.send_ctrl(Ctrl::Shutdown);
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
            pixel_width: config.cols.saturating_mul(config.cell_width.max(1)),
            pixel_height: config.rows.saturating_mul(config.cell_height.max(1)),
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
    let (pty_tx, pty_rx) = bounded::<Vec<u8>>(PTY_QUEUE_CHUNKS);
    let (ctrl_tx, ctrl_rx) = bounded::<Ctrl>(CTRL_QUEUE);
    let (pull_tx, pull_rx) = bounded::<(u64, Box<Frame>)>(1);
    let (events_sink, events_rx, events_wake_rx) = EventSink::new_with_wake(EVENT_QUEUE);
    let (wake_signal, wake_rx) = WakeSignal::new();

    let stats = Arc::new(PaneStats::default());
    let write_queue = WriteQueue::new();

    let shared_palette = Arc::new(RwLock::new(config.palette.clone()));
    let sixel_enabled = Arc::new(AtomicBool::new(config.sixel));
    let iip_enabled = Arc::new(AtomicBool::new(config.iip));
    let listener = Listener {
        wake: wake_signal,
        events: events_sink,
        write_queue: write_queue.clone(),
        stats: stats.clone(),
        palette: shared_palette.clone(),
        sixel: sixel_enabled.clone(),
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
    let writer_queue = write_queue.clone();
    let writer_thread = thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || {
            while let Some(bytes) = writer_queue.pop() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
            // 写端结束：后续 push 直接 Closed（不再堆积/刷日志）。
            writer_queue.close();
        })?;

    // ---- actor 线程 ----
    let term_config = TermConfig {
        scrolling_history: config.scrollback,
        // alacritty 0.26 的 kitty 键盘栈在“set 不 push 后 query / set-push-pop”
        // 两处不符合 kitty 规范，因此由 core 的 KeyboardScanner 维护状态；
        // 这里关掉 alacritty 的实现，避免重复回包。
        kitty_keyboard: false,
        // OSC 52 由 core 的 OscScanner 统一处理（带 1 MiB 上限并转成 CoreEvent）；
        // 关掉 alacritty 内建实现，避免同一条序列被写两次。
        osc52: Osc52::Disabled,
        ..TermConfig::default()
    };
    let term = Term::new(term_config.clone(), &size, listener.clone());
    let palette = shared_palette.clone();
    let stats_actor = stats.clone();
    let mode_bits = Arc::new(std::sync::atomic::AtomicU32::new(term.mode().bits()));
    let keyboard_flags = Arc::new(AtomicU8::new(0));
    let modify_other_keys = Arc::new(AtomicU8::new(0));
    let c0 = term.grid().cursor.point;
    let cursor_bits = Arc::new(std::sync::atomic::AtomicU64::new(
        ((c0.line.0 as u64) << 32) | (c0.column.0 as u64 & 0xffff),
    ));
    let scroll = Arc::new(std::sync::atomic::AtomicU64::new(pack_scroll(
        term.grid().display_offset(),
        term.grid().history_size(),
    )));
    let read_only = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let grid_lines = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut iip = IipFilter::new();
    iip.set_cell_size(config.cell_width.max(1), config.cell_height.max(1));

    let actor = Actor {
        term,
        term_config,
        processor: Processor::new(),
        osc: OscScanner::new(),
        keyboard: KeyboardScanner::new(config.keyboard_protocol),
        sixel: SixelFilter::new(),
        sixel_enabled: sixel_enabled.clone(),
        sixel_scrolling: config.sixel_scrolling,
        iip,
        iip_enabled: iip_enabled.clone(),
        cell_width: config.cell_width.max(1),
        cell_height: config.cell_height.max(1),
        images: ImageStore::default(),
        kitty: KittyParser::new(),
        kitty_store: KittyStore::default(),
        query_tail: Vec::new(),
        image_clear: ImageClearScanner::default(),
        pty_rx,
        ctrl_rx,
        pull_tx,
        pull_rx: pull_rx.clone(),
        palette,
        stats: stats_actor,
        master,
        size,
        wake: listener.wake.clone(),
        events: listener.events.clone(),
        write_queue: write_queue.clone(),
        mode_bits: mode_bits.clone(),
        modify_other_keys: modify_other_keys.clone(),
        keyboard_flags: keyboard_flags.clone(),
        cursor_bits: cursor_bits.clone(),
        scroll: scroll.clone(),
        read_only: read_only.clone(),
        sel_anchor: None,
        pty_closed: false,
        force_full: false,
        refresh_zerowidth: false,
        search_span: None,
        scroll_on_output: config.scroll_on_output,
        grid_lines: grid_lines.clone(),
    };
    let actor_thread = thread::Builder::new()
        .name("term-actor".into())
        .spawn(move || actor.run())?;

    Ok(PaneHandle {
        write_queue: write_queue.clone(),
        ctrl_tx,
        pull_rx,
        events_rx,
        events_wake_rx,
        wake_rx,
        stats,
        mode_bits,
        modify_other_keys,
        keyboard_flags,
        cursor_bits,
        scroll,
        read_only,
        grid_lines: grid_lines.clone(),
        pull_seq: AtomicU64::new(0),
        pull_needs_full: AtomicBool::new(false),
        child,
        threads: vec![reader_thread, writer_thread, actor_thread],
        grid: size,
        exited: false,
    })
}

/// 搜索 query → alacritty RegexSearch 模式。
///
/// - 纯文本模式先转义元字符；
/// - `match_case` 用内联组强制（alacritty 默认按“是否含大写”启发式决定，
///   不能表达用户显式开关，所以这里始终写 `(?i:...)` / `(?-i:...)`）。
fn search_pattern(query: &str, options: SearchOptions) -> String {
    let body = if options.regex { query.to_string() } else { regex_escape(query) };
    if options.match_case {
        format!("(?-i:{body})")
    } else {
        format!("(?i:{body})")
    }
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

/// 从命中起点/终点前进或后退一格，用于下一次搜索的 exclusive origin。
///
/// 正向搜索必须越过当前命中（+1 格），反向搜索必须退到命中起点前一格
/// （-1 格）；否则 alacritty 的 `search_next` 会把当前命中再次算进来，
/// Shift+Enter 会“原地不动”。越过最后/第一格时按 grid 环绕到另一端。
fn step_search_point<T: EventListener>(term: &Term<T>, point: Point, forward: bool) -> Point {
    let grid = term.grid();
    let cols = term.columns();
    let last_col = Column(cols.saturating_sub(1));
    if forward {
        if point.column.0 + 1 < cols {
            Point::new(point.line, Column(point.column.0 + 1))
        } else if point.line < grid.bottommost_line() {
            Point::new(point.line + 1, Column(0))
        } else {
            Point::new(grid.topmost_line(), Column(0))
        }
    } else if point.column.0 > 0 {
        Point::new(point.line, Column(point.column.0 - 1))
    } else if point.line > grid.topmost_line() {
        Point::new(point.line - 1, last_col)
    } else {
        Point::new(grid.bottommost_line(), last_col)
    }
}

/// 用 alacritty `RegexIter` 扫描整个 grid（scrollback + 视口），返回
/// `(当前命中 1-based 序号, 匹配总数)`。
///
/// 与 `Term::search_next` 共用同一套 regex/grid 语义，因此换行、宽字符、
/// 大小写内联组的结果与高亮一致；不经过 `Frame`，也不在 UI 线程扫文本。
/// `found` 一定来自同一 `term`+`regex` 的 `search_next`，正常都能在迭代中
/// 找到；万一坐标未命中（例如极端并发滚动），序号回退为 1。
fn count_search_matches<T: EventListener>(
    term: &Term<T>,
    regex: &mut RegexSearch,
    found: &std::ops::RangeInclusive<Point>,
) -> (usize, usize) {
    let grid = term.grid();
    let start = Point::new(grid.topmost_line(), Column(0));
    let end = Point::new(
        grid.bottommost_line(),
        Column(term.columns().saturating_sub(1)),
    );
    let mut total = 0usize;
    let mut index = 0usize;
    for m in RegexIter::new(start, end, Direction::Right, term, regex) {
        total += 1;
        if index == 0 && m.start() == found.start() && m.end() == found.end() {
            index = total;
        }
    }
    if total > 0 && index == 0 {
        index = 1;
    }
    (index, total)
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

/// 把一块真正交给 vte 的字节喂给 parser，然后刷新 grid 行数诊断快照。
///
/// RowId 改造后 Tervo 不再自行累计 `scroll_total`：图片锚点只认 alacritty
/// `RowId`，滚动/历史淘汰由 `Grid::find_row_id` 直接判定。
fn advance_and_publish_grid_lines<T: EventListener>(
    term: &mut Term<T>,
    processor: &mut Processor,
    grid_lines_slot: Option<&std::sync::atomic::AtomicU64>,
    chunk: &[u8],
) {
    processor.advance(term, chunk);
    let snap = ((term.grid().history_size() as u64) << 32)
        | (term.grid().total_lines() as u64 & 0xffff_ffff);
    if let Some(slot) = grid_lines_slot {
        slot.store(snap, std::sync::atomic::Ordering::Relaxed);
    }
}

struct Actor {
    term: Term<Listener>,
    /// 当前生效的 alacritty 配置（动态 scrollback 预算用）。
    term_config: TermConfig,
    processor: Processor,
    /// 通知/进度/OSC 7 扫描：alacritty 不暴露 OSC 9/777，需旁路扫原始字节。
    osc: OscScanner,
    /// kitty flags 栈 + xterm modifyOtherKeys 扫描（alacritty 栈语义不完整）。
    keyboard: KeyboardScanner,
    /// Sixel DCS 过滤器：把图片字节从 vte 输入里摘掉。
    sixel: SixelFilter,
    /// 与 Listener 共享的开关（DA1 判定 + 是否解码/渲染图片）。
    sixel_enabled: Arc<AtomicBool>,
    /// 图片是否跟随滚动历史。
    sixel_scrolling: bool,
    /// iTerm2 IIP 流式过滤器（OSC 1337 File）。
    iip: IipFilter,
    /// IIP 开关：关闭时只吞序列，不解码/渲染。
    iip_enabled: Arc<AtomicBool>,
    /// 终端 cell 像素宽/高（图片 cell 覆盖范围与文本擦除判定）。
    cell_width: u16,
    cell_height: u16,
    /// 已解码的 Sixel/IIP 图片（按创建顺序，超出预算淘汰最旧）。
    images: ImageStore,
    /// Kitty graphics APC 过滤器/解析器。
    kitty: KittyParser,
    /// Kitty 图片资源 + placement（协议 id 与稳定 RowId 分离）。
    kitty_store: KittyStore,
    /// XTWINOPS (CSI 16t/14t) 跨 chunk 扫描尾部。
    query_tail: Vec<u8>,
    /// 清屏/备用屏切换检测：这些序列让 alacritty 清屏，图片也必须一起清。
    image_clear: ImageClearScanner,
    pty_rx: Receiver<Vec<u8>>,
    ctrl_rx: Receiver<Ctrl>,
    pull_tx: Sender<(u64, Box<Frame>)>,
    /// 拉帧响应通道的接收端克隆：仅在容量 1 被迟到响应占住时由 actor 自行排空，
    /// 保证 actor 永远不因 UI 超时而阻塞。
    pull_rx: Receiver<(u64, Box<Frame>)>,
    palette: Arc<RwLock<Palette>>,
    stats: Arc<PaneStats>,
    master: Box<dyn MasterPty + Send>,
    size: GridSize,
    wake: WakeSignal,
    events: EventSink,
    /// 与 Listener 共享的 PTY 写 backlog（actor 只 push，永不阻塞）。
    write_queue: Arc<WriteQueue>,
    /// 模式位快照，供 UI 无阻塞查询（见 [`PaneHandle::mode`]）。
    mode_bits: Arc<std::sync::atomic::AtomicU32>,
    /// modifyOtherKeys 等级快照（0 关闭；见 [`PaneHandle::modify_other_keys`]）。
    modify_other_keys: Arc<AtomicU8>,
    /// kitty flags 快照（含 main/alternate screen 栈；见 [`PaneHandle::keyboard_flags`]）。
    keyboard_flags: Arc<AtomicU8>,
    /// 光标快照，供 IME/焦点同步（不依赖 UI 拉帧）。
    cursor_bits: Arc<std::sync::atomic::AtomicU64>,
    /// 滚动位置快照（打包 offset/history），供 UI 无阻塞查询。
    scroll: Arc<std::sync::atomic::AtomicU64>,
    /// Read Only 原子标志：actor 写 PTY 前再检查一次，丢弃已入队的输入。
    read_only: Arc<std::sync::atomic::AtomicBool>,
    /// 选区锚点（网格坐标）；反向选择时用来保持两端 side 一致。
    sel_anchor: Option<Point>,
    pty_closed: bool,
    force_full: bool,
    /// 零宽字符到达后要求下一次整帧重取时扫描组合字符（见 feed）。
    refresh_zerowidth: bool,
    search_span: Option<Span>,
    /// 收到输出后是否强制回到底部（GNOME scroll-on-output）。
    scroll_on_output: bool,
    /// 网格行数快照（high 32 = history_size，low 32 = total_lines）。
    /// 诊断用：scrollback 内存 ≈ total_lines × columns × size_of::<Cell>()。
    grid_lines: Arc<std::sync::atomic::AtomicU64>,
}

/// 单图 + 全部图片的像素预算：UI 侧缓存 Slint Image 还会再占一份，所以这里
/// 限制得比“单图 16 MP（64 MiB）”宽不了多少。
/// alacritty 单元格字节数：`total_lines × columns × CELL_BYTES` 可作为
/// scrollback 存储（含 alacritty 的 1000 行 cache）的内存估计。
pub const CELL_BYTES: usize = std::mem::size_of::<alacritty_terminal::term::cell::Cell>();

const MAX_STORED_IMAGES: usize = 64;
const MAX_STORED_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// 检测 `CSI Ps X`（ECH，擦除字符）并返回参数。dsh-tui 用 ECH 释放旧图；
/// 图片像素不在 cell 中，必须显式擦除对应矩形。
fn erase_char_params(bytes: &[u8], out: &mut Vec<Option<u16>>) -> bool {
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && matches!(bytes[j], 0x30..=0x3f) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'X' {
                let params = std::str::from_utf8(&bytes[i + 2..j]).unwrap_or("");
                // ECH 可以有多个参数，按每个参数各擦一次当前光标位置。
                for value in params.split(';').map(str::trim).filter(|v| !v.is_empty()) {
                    out.push(value.parse::<u16>().ok());
                }
                if params.is_empty() {
                    out.push(None);
                }
            }
        }
        i += 1;
    }
    !out.is_empty()
}

/// 会清空整屏/切换备用屏、因而必须清除内联图片的转义序列。
///
/// 粗粒度清屏（Escape 2J/3J、进入/离开备用屏、RIS）会清全部图片。
const IMAGE_CLEAR_SEQUENCES: [&[u8]; 8] = [
    b"\x1b[2J",
    b"\x1b[3J",
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?1047h",
    b"\x1b[?1047l",
    b"\x1b[?47h",
    b"\x1b[?47l",
];

/// 跨 chunk 检测清屏序列；最多保留 8 字节尾巴，不复制整块 PTY 数据。
#[derive(Default)]
struct ImageClearScanner {
    tail: Vec<u8>,
    /// 本 chunk/尾部拼接里的 ECH 宽度；由 actor 取 vte 处理后光标左侧的矩形。
    ech: Vec<Option<u16>>,
}

impl ImageClearScanner {
    fn feed(&mut self, bytes: &[u8]) -> bool {
        // 把上一块尾部拼进来，统一做 chunk 内扫描；旧实现逐 pattern 处理跨边界，
        // 这里同时覆盖 ECH 这类参数长度不固定的序列。
        let mut buf = Vec::with_capacity(self.tail.len() + bytes.len());
        buf.extend_from_slice(&self.tail);
        buf.extend_from_slice(bytes);
        let hit = IMAGE_CLEAR_SEQUENCES
            .iter()
            .any(|pattern| buf.windows(pattern.len()).any(|w| w == *pattern));
        self.ech.clear();
        erase_char_params(&buf, &mut self.ech);
        // 保留末尾 32 字节：ECH 参数/跨 chunk 边界都足够。
        let keep = 32.min(buf.len());
        self.tail.clear();
        if keep > 0 {
            self.tail.extend_from_slice(&buf[buf.len() - keep..]);
        }
        hit
    }
}

/// 图片来自哪种协议；两种图片共用一个 ID 空间与预算，但可以各自开关/清除。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageKind {
    Sixel,
    Iip,
}

/// actor 内保存的一张已解码内联图片（Sixel 或 IIP）。
struct StoredImage {
    /// 与 terminal placement 同生命周期；仅用于可访问/擦除判断。
    id: u64,
    /// 源像素生命周期标识。UI 以它缓存纹理和灯箱 source。
    source_id: u64,
    kind: ImageKind,
    /// 创建时光标所在 grid 行的稳定身份（`Grid::row_id`）。
    ///
    /// placement 的位置只认它：`find_row_id` 找不到说明该逻辑行已被
    /// reset/清屏/历史淘汰/切到备用屏，必须显式删除，绝不回退到
    /// `scroll_total` 或绝对行号推算。
    row_id: u64,
    /// `sixel_scrolling=false` 时使用：创建（或切到固定模式）时的视口行，
    /// 滚动时固定在屏幕上；`row_id` 仍负责判定逻辑行是否还存在。
    fixed_row: Option<i32>,
    col: u16,
    width: u32,
    height: u32,
    data: Arc<[u8]>,
    bytes: usize,
}

#[derive(Default)]
struct ImageStore {
    images: VecDeque<StoredImage>,
    bytes: usize,
    next_id: u64,
    next_source_id: u64,
}

impl ImageStore {
    fn clear(&mut self) {
        self.images.clear();
        self.bytes = 0;
    }

    /// 只清除某一种协议的图片（关闭 Sixel 时不影响 IIP，反之亦然）。
    fn clear_kind(&mut self, kind: ImageKind) {
        let mut removed = 0usize;
        self.images.retain(|image| {
            if image.kind == kind {
                removed += image.bytes;
                false
            } else {
                true
            }
        });
        self.bytes = self.bytes.saturating_sub(removed);
    }

    /// 锚定到光标处的 Sixel 图片。`row_id` 由调用方从当前光标 grid 行读取
    /// （`Grid::row_id(cursor.line)`）。
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: ImageKind,
        image: SixelImage,
        cursor: Point,
        row_id: u64,
        display_offset: usize,
        scrolling: bool,
        cols: usize,
    ) {
        self.push_rgba(
            kind,
            image.width,
            image.height,
            image.rgba,
            cursor,
            row_id,
            display_offset,
            scrolling,
            cols,
        );
    }

    /// 锚定到光标处的一张 RGBA 图片。`cursor.line` 是 alacritty grid 坐标
    /// （视口底行为 `screen_lines-1`，历史为负）。
    #[allow(clippy::too_many_arguments)]
    fn push_rgba(
        &mut self,
        kind: ImageKind,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        cursor: Point,
        row_id: u64,
        display_offset: usize,
        scrolling: bool,
        cols: usize,
    ) {
        let expected = width as usize * height as usize * 4;
        if width == 0 || height == 0 || rgba.len() != expected || expected > MAX_STORED_IMAGE_BYTES
        {
            return;
        }
        let bytes = rgba.len();
        // 按创建顺序淘汰最旧图片，给新图腾预算。
        while !self.images.is_empty()
            && (self.images.len() >= MAX_STORED_IMAGES
                || self.bytes.saturating_add(bytes) > MAX_STORED_IMAGE_BYTES)
        {
            if let Some(old) = self.images.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.bytes);
            }
        }
        if self.bytes.saturating_add(bytes) > MAX_STORED_IMAGE_BYTES {
            return;
        }
        let col = if cols == 0 { 0 } else { (cursor.column.0).min(cols - 1) as u16 };
        let fixed_row = if scrolling {
            None
        } else {
            Some(cursor.line.0.saturating_add(display_offset as i32))
        };
        self.next_id = self.next_id.wrapping_add(1);
        self.next_source_id = self.next_source_id.wrapping_add(1);
        self.bytes += bytes;
        self.images.push_back(StoredImage {
            id: self.next_id,
            source_id: self.next_source_id,
            kind,
            row_id,
            fixed_row,
            col,
            width,
            height,
            data: Arc::from(rgba.into_boxed_slice()),
            bytes,
        });
    }

    /// 运行时切换“图片跟随滚动”。
    ///
    /// 滚动模式只由 `row_id` 解析；切到固定模式时记录**当前**视口行，让
    /// 图片停在切换那一刻的屏幕位置。切回滚动模式直接丢掉 fixed_row，
    /// 逻辑行身份本来就在，不需要重新换算绝对行号。
    fn set_scrolling(&mut self, scrolling: bool, grid: &Grid<Cell>, display_offset: usize) {
        for image in &mut self.images {
            if scrolling {
                image.fixed_row = None;
            } else if image.fixed_row.is_none() {
                if let Some(line) = grid.find_row_id(image.row_id) {
                    image.fixed_row = Some(line.0.saturating_add(display_offset as i32));
                }
            }
        }
    }

    /// 擦除与视口 cell 矩形相交的图片。图片跨多行时整张 drop，等 TUI 重画；
    /// 同行的另一张图片不受影响。
    ///
    /// 图片当前视口行由 `row_id` + `Grid::find_row_id` 解析；固定模式用
    /// `fixed_row`。`row_id` 已找不到的图片交给 [`ImageStore::place`] 显式
    /// drop，这里不做猜测性擦除。
    #[allow(clippy::too_many_arguments)]
    fn erase_viewport_rectangle(
        &mut self,
        grid: &Grid<Cell>,
        row: i64,
        left: usize,
        right: usize,
        display_offset: usize,
        cols: usize,
        rows: usize,
        cell_width: u16,
        cell_height: u16,
    ) {
        if self.images.is_empty() || rows == 0 {
            return;
        }
        let off = display_offset as i64;
        let cell_w = cell_width.max(1) as i64;
        let cell_h = cell_height.max(1) as i64;
        let left = left.min(cols) as i64;
        let right = right.min(cols) as i64;
        if left >= right {
            return;
        }
        let mut removed = 0usize;
        self.images.retain(|image| {
            let top = match image.fixed_row {
                Some(r) => r as i64,
                None => match grid.find_row_id(image.row_id) {
                    Some(line) => line.0 as i64 + off,
                    // 行已消失：交给 place 统一显式删除。
                    None => return true,
                },
            };
            let cell_rows = (image.height as i64 + cell_h - 1) / cell_h;
            let cell_cols = (image.width as i64 + cell_w - 1) / cell_w;
            let image_left = image.col as i64;
            let image_right = image_left + cell_cols.max(1);
            let intersects = row >= top
                && row < top + cell_rows.max(1)
                && left < image_right
                && right > image_left;
            if intersects {
                removed += image.bytes;
                false
            } else {
                true
            }
        });
        self.bytes = self.bytes.saturating_sub(removed);
    }

    /// 将图片锚点展开为当前视口内的 layer placement。
    ///
    /// `row_id` 是唯一位置来源：`find_row_id` 找不到 = 逻辑行已 reset /
    /// 清屏 / 历史淘汰 / 属于非活动屏幕，必须从 store 显式删除，绝不回退
    /// 到 `scroll_total` 或绝对行号。完全在视口外但行仍存在的图片保留，
    /// 以便滚回时重新显示。
    fn place(
        &mut self,
        grid: &Grid<Cell>,
        display_offset: usize,
        rows: usize,
        cols: usize,
        cell_height: u16,
        out: &mut Vec<InlineImage>,
    ) {
        if rows == 0 || cols == 0 {
            return;
        }
        let off = display_offset as i64;
        let screen_rows = rows as i64;
        let cell_h = cell_height.max(1) as i64;
        let mut removed = 0usize;
        self.images.retain(|image| {
            let Some(line) = grid.find_row_id(image.row_id) else {
                removed += image.bytes;
                return false;
            };
            let row = match image.fixed_row {
                Some(row) => row as i64,
                None => line.0 as i64 + off,
            };
            if row >= screen_rows {
                return true;
            }
            let cell_rows = (image.height as i64 + cell_h - 1) / cell_h;
            if row + cell_rows.max(1) <= 0 {
                return true;
            }
            if image.col as usize >= cols {
                return true;
            }
            out.push(InlineImage {
                id: image.id,
                source_id: image.source_id,
                row_id: image.row_id,
                row: row.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                col: image.col,
                width: image.width,
                height: image.height,
                data: image.data.clone(),
                z_index: -1,
                above_text: false,
            });
            true
        });
        self.bytes = self.bytes.saturating_sub(removed);
    }
}

/// Kitty placement/source ID 下界：与 Sixel/IIP 的本地 ID 空间隔开，UI 的
/// `inline_hits`/纹理缓存用 u64 source_id 才不会跨协议碰撞。
const KITTY_ID_BASE: u64 = 1 << 62;
const MAX_KITTY_IMAGES: usize = 256;
const MAX_KITTY_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// 一张 Kitty 图片的不可变像素资源。
struct KittyResource {
    source_id: u64,
    width: u32,
    height: u32,
    data: Arc<[u8]>,
    bytes: usize,
}

/// 一次 Kitty placement。`row_id` 是唯一位置来源；`fixed_row` 预留给
/// 固定滚动策略（当前 Kitty 一律跟随 RowId）。
struct KittyPlacement {
    /// 本地 placement ID，与 Sixel/IIP 共用 Frame 命名空间但使用高位段。
    id: u64,
    /// 协议 `p`（0 = 无显式 placement id）。
    client_id: u32,
    image_id: u32,
    source_id: u64,
    row_id: u64,
    fixed_row: Option<i32>,
    col: u16,
    source_rect: Option<SourceRect>,
    /// 协议 `c/r`；0 表示按源像素与 cell 尺寸推导。
    columns: u16,
    rows: u16,
    cells_w: u16,
    cells_h: u16,
    z_index: i32,
    above_text: bool,
}

#[derive(Default)]
struct KittyStore {
    resources: HashMap<u32, KittyResource>,
    placements: Vec<KittyPlacement>,
    bytes: usize,
    next_placement_id: u64,
    next_source_id: u64,
}

impl KittyStore {
    fn clear(&mut self) {
        self.resources.clear();
        self.placements.clear();
        self.bytes = 0;
    }

    fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.placements.is_empty()
    }

    fn resource_by_source(&self, source_id: u64) -> Option<(u32, u32, Arc<[u8]>)> {
        self.resources
            .values()
            .find(|resource| resource.source_id == source_id)
            .map(|resource| (resource.width, resource.height, resource.data.clone()))
    }

    /// 存入一张已解码的 Kitty 图片，返回被预算淘汰的协议 image id 与是否成功。
    ///
    /// 同一协议 image id 再次传输按 Kitty 规范替换旧资源和全部旧 placement。
    fn insert(&mut self, transmit: TransmitCommand) -> (Vec<u32>, bool) {
        let image = transmit.image;
        let image_id = image.id;
        let bytes = image.rgba.len();
        if bytes == 0 || bytes > MAX_KITTY_IMAGE_BYTES {
            return (Vec::new(), false);
        }

        // 规范要求重传同一 id 时先删除旧图及其 placement。
        self.remove_image(image_id);
        let evicted = self.make_room(bytes);
        if self.bytes.saturating_add(bytes) > MAX_KITTY_IMAGE_BYTES
            || self.resources.len() >= MAX_KITTY_IMAGES
        {
            return (evicted, false);
        }

        self.next_source_id = if self.next_source_id < KITTY_ID_BASE {
            KITTY_ID_BASE + 1
        } else {
            self.next_source_id.wrapping_add(1).max(KITTY_ID_BASE + 1)
        };
        let source_id = self.next_source_id;
        self.resources.insert(
            image_id,
            KittyResource {
                source_id,
                width: image.width,
                height: image.height,
                data: Arc::from(image.rgba.into_boxed_slice()),
                bytes,
            },
        );
        self.bytes = self.bytes.saturating_add(bytes);
        (evicted, true)
    }

    /// 在预算内腾出 `needed` 字节；优先淘汰没有 placement 的资源，其次最旧资源。
    fn make_room(&mut self, needed: usize) -> Vec<u32> {
        let mut evicted = Vec::new();
        while self.bytes.saturating_add(needed) > MAX_KITTY_IMAGE_BYTES
            || self.resources.len() >= MAX_KITTY_IMAGES
        {
            let referenced = |id: &u32, placements: &[KittyPlacement]| {
                placements.iter().any(|placement| placement.image_id == *id)
            };
            let victim = self
                .resources
                .keys()
                .copied()
                .find(|id| !referenced(id, &self.placements))
                .or_else(|| {
                    self.resources
                        .iter()
                        .min_by_key(|(_, resource)| resource.source_id)
                        .map(|(id, _)| *id)
                });
            let Some(victim) = victim else {
                break;
            };
            if self.remove_image(victim).is_none() {
                break;
            }
            evicted.push(victim);
        }
        evicted
    }

    fn remove_image(&mut self, image_id: u32) -> Option<KittyResource> {
        let resource = self.resources.remove(&image_id);
        if let Some(resource) = &resource {
            self.bytes = self.bytes.saturating_sub(resource.bytes);
        }
        self.placements
            .retain(|placement| placement.image_id != image_id);
        resource
    }

    fn remove_unreferenced(&mut self) -> Vec<u32> {
        let ids: Vec<u32> = self
            .resources
            .keys()
            .copied()
            .filter(|id| {
                !self
                    .placements
                    .iter()
                    .any(|placement| placement.image_id == *id)
            })
            .collect();
        let mut removed = Vec::new();
        for id in ids {
            if self.remove_image(id).is_some() {
                removed.push(id);
            }
        }
        removed
    }

    /// 放置一张已传输的图片；返回实际目标 cell 宽高（供 C=0 光标移动）。
    fn place(
        &mut self,
        place: PlacementCommand,
        cursor: Point,
        row_id: u64,
        cols: usize,
        cell_width: u16,
        cell_height: u16,
    ) -> Option<(u16, u16)> {
        let resource = self.resources.get(&place.image_id)?;
        let (source_w, source_h) = place
            .source_rect
            .map_or((resource.width, resource.height), |rect| (rect.w, rect.h));
        let (cells_w, cells_h) = kitty_cells(
            place.columns,
            place.rows,
            source_w,
            source_h,
            cell_width,
            cell_height,
        );

        if place.placement_id != 0 {
            self.placements.retain(|placement| {
                !(placement.image_id == place.image_id
                    && placement.client_id == place.placement_id)
            });
        }

        self.next_placement_id = if self.next_placement_id < KITTY_ID_BASE {
            KITTY_ID_BASE + 1
        } else {
            self.next_placement_id
                .wrapping_add(1)
                .max(KITTY_ID_BASE + 1)
        };
        let col = if cols == 0 {
            0
        } else {
            (cursor.column.0).min(cols - 1) as u16
        };
        let fixed_row = None;
        self.placements.push(KittyPlacement {
            id: self.next_placement_id,
            client_id: place.placement_id,
            image_id: place.image_id,
            source_id: resource.source_id,
            row_id,
            fixed_row,
            col,
            source_rect: place.source_rect,
            columns: place.columns,
            rows: place.rows,
            cells_w,
            cells_h,
            z_index: place.z_index,
            above_text: matches!(place.composite, CompositePolicy::AboveText),
        });
        Some((cells_w, cells_h))
    }

    /// cell 尺寸变化后按协议 c/r 重新推导目标 cell 矩形。
    fn retune_cells(&mut self, cell_width: u16, cell_height: u16) {
        let resources = &self.resources;
        for placement in &mut self.placements {
            let Some(resource) = resources.get(&placement.image_id) else {
                continue;
            };
            let (source_w, source_h) = placement
                .source_rect
                .map_or((resource.width, resource.height), |rect| (rect.w, rect.h));
            let (cells_w, cells_h) = kitty_cells(
                placement.columns,
                placement.rows,
                source_w,
                source_h,
                cell_width,
                cell_height,
            );
            placement.cells_w = cells_w;
            placement.cells_h = cells_h;
        }
    }

    /// 把当前视口内的 Kitty placement 展开成 Frame 图片和元数据。
    #[allow(clippy::too_many_arguments)]
    fn place_frame(
        &mut self,
        grid: &Grid<Cell>,
        display_offset: usize,
        rows: usize,
        cols: usize,
        images: &mut Vec<InlineImage>,
        metas: &mut Vec<ImagePlacementMeta>,
    ) {
        if self.placements.is_empty() || rows == 0 || cols == 0 {
            return;
        }
        let off = display_offset as i64;
        let screen_rows = rows as i64;
        let old = std::mem::take(&mut self.placements);
        let mut kept = Vec::with_capacity(old.len());
        let mut visible = Vec::new();

        for placement in old {
            let Some(resource) = self.resources.get(&placement.image_id) else {
                continue;
            };
            let Some(line) = grid.find_row_id(placement.row_id) else {
                // RowId 消失 = 逻辑行被 reset/淘汰，显式丢弃 placement。
                continue;
            };
            let row = match placement.fixed_row {
                Some(row) => row as i64,
                None => line.0 as i64 + off,
            };
            if row >= screen_rows
                || row + placement.cells_h.max(1) as i64 <= 0
                || placement.col as usize >= cols
            {
                kept.push(placement);
                continue;
            }
            visible.push((placement, resource.data.clone(), resource.width, resource.height));
        }

        visible.sort_by(|a, b| {
            a.0.z_index
                .cmp(&b.0.z_index)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });

        for (placement, data, width, height) in visible {
            let meta = ImagePlacementMeta {
                placement_id: placement.id,
                image_id: u64::from(placement.image_id),
                row_id: placement.row_id,
                source_rect: placement.source_rect.map(|rect| ImageCrop {
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                }),
                cells_w: placement.cells_w,
                cells_h: placement.cells_h,
                z_index: placement.z_index,
                above_text: placement.above_text,
            };
            let Some(line) = grid.find_row_id(placement.row_id) else {
                continue;
            };
            let row = match placement.fixed_row {
                Some(row) => row as i64,
                None => line.0 as i64 + off,
            };
            images.push(InlineImage {
                id: placement.id,
                source_id: placement.source_id,
                row_id: placement.row_id,
                row: row.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                col: placement.col,
                width,
                height,
                data,
                z_index: placement.z_index,
                above_text: placement.above_text,
            });
            metas.push(meta);
            kept.push(placement);
        }
        self.placements = kept;
    }

    fn delete(&mut self, target: DeleteTarget, ctx: &KittyDeleteCtx) -> Vec<u32> {
        let free_data = target_frees_data(&target);
        let mut removed = Vec::new();
        match target {
            DeleteTarget::All | DeleteTarget::AllPlacements { .. } => {
                self.placements.clear();
                if free_data {
                    removed.extend(self.resources.keys().copied());
                    self.resources.clear();
                    self.bytes = 0;
                }
            },
            DeleteTarget::Image {
                image_id,
                placement_id,
                free_data,
            } => match placement_id {
                Some(client_id) => {
                    self.placements.retain(|placement| {
                        !(placement.image_id == image_id && placement.client_id == client_id)
                    });
                    if free_data
                        && !self
                            .placements
                            .iter()
                            .any(|placement| placement.image_id == image_id)
                        && self.remove_image(image_id).is_some()
                    {
                        removed.push(image_id);
                    }
                },
                None => {
                    if free_data {
                        if self.remove_image(image_id).is_some() {
                            removed.push(image_id);
                        }
                    } else {
                        self.placements
                            .retain(|placement| placement.image_id != image_id);
                    }
                },
            },
            DeleteTarget::NewestImage { .. } => {
                // 解析器保留 I/n 语法，但本 slice 不维护 image-number 索引。
            },
            DeleteTarget::Cursor { .. } => {
                self.placements.retain(|placement| {
                    !(placement.row_id == ctx.cursor_row_id
                        && kitty_span_contains(placement, ctx.cursor_col))
                });
                if free_data {
                    removed.extend(self.remove_unreferenced());
                }
            },
            DeleteTarget::Cell {
                x,
                y,
                z_index,
                free_data,
            } => {
                let row_id = y
                    .checked_sub(1)
                    .and_then(|row| ctx.viewport_row_ids.get(row as usize))
                    .copied();
                let col = x.saturating_sub(1);
                if let Some(row_id) = row_id {
                    self.placements.retain(|placement| {
                        if placement.row_id != row_id {
                            return true;
                        }
                        if z_index.is_some_and(|z| z != placement.z_index) {
                            return true;
                        }
                        !kitty_span_contains(placement, col)
                    });
                }
                if free_data {
                    removed.extend(self.remove_unreferenced());
                }
            },
            DeleteTarget::Column { x, .. } => {
                let col = x.saturating_sub(1);
                self.placements
                    .retain(|placement| !kitty_span_contains(placement, col));
                if free_data {
                    removed.extend(self.remove_unreferenced());
                }
            },
            DeleteTarget::Row { y, .. } => {
                if let Some(row_id) = y
                    .checked_sub(1)
                    .and_then(|row| ctx.viewport_row_ids.get(row as usize))
                    .copied()
                {
                    self.placements
                        .retain(|placement| placement.row_id != row_id);
                }
                if free_data {
                    removed.extend(self.remove_unreferenced());
                }
            },
            DeleteTarget::ZIndex { z_index, .. } => {
                self.placements
                    .retain(|placement| placement.z_index != z_index);
                if free_data {
                    removed.extend(self.remove_unreferenced());
                }
            },
            DeleteTarget::ImageRange {
                min_image_id,
                max_image_id,
                free_data,
            } => {
                if free_data {
                    let ids: Vec<u32> = self
                        .resources
                        .keys()
                        .copied()
                        .filter(|id| *id >= min_image_id && *id <= max_image_id)
                        .collect();
                    for id in ids {
                        if self.remove_image(id).is_some() {
                            removed.push(id);
                        }
                    }
                } else {
                    self.placements.retain(|placement| {
                        placement.image_id < min_image_id
                            || placement.image_id > max_image_id
                    });
                }
            },
        }
        removed
    }
}

/// 一次删除/擦除命令需要的视口快照，避免在 retain 闭包里再借 `Term`。
struct KittyDeleteCtx {
    cursor_row_id: u64,
    cursor_col: u32,
    viewport_row_ids: Vec<u64>,
}

fn target_frees_data(target: &DeleteTarget) -> bool {
    match target {
        DeleteTarget::All => false,
        DeleteTarget::AllPlacements { free_data }
        | DeleteTarget::Image { free_data, .. }
        | DeleteTarget::NewestImage { free_data, .. }
        | DeleteTarget::Cursor { free_data }
        | DeleteTarget::Cell { free_data, .. }
        | DeleteTarget::Column { free_data, .. }
        | DeleteTarget::Row { free_data, .. }
        | DeleteTarget::ZIndex { free_data, .. }
        | DeleteTarget::ImageRange { free_data, .. } => *free_data,
    }
}

fn kitty_span_contains(placement: &KittyPlacement, col: u32) -> bool {
    let left = u32::from(placement.col);
    let right = left.saturating_add(u32::from(placement.cells_w.max(1)));
    col >= left && col < right
}

/// 由协议 `c/r` 与源尺寸推导目标 cell 矩形；0 表示按源像素/cell 尺寸推导，
/// 只有一边时按源宽高比补齐另一边。
fn kitty_cells(
    columns: u16,
    rows: u16,
    source_w: u32,
    source_h: u32,
    cell_width: u16,
    cell_height: u16,
) -> (u16, u16) {
    let cell_w = u64::from(cell_width.max(1));
    let cell_h = u64::from(cell_height.max(1));
    let source_w = u64::from(source_w.max(1));
    let source_h = u64::from(source_h.max(1));
    let mut columns = u64::from(columns);
    let mut rows = u64::from(rows);
    if columns == 0 && rows == 0 {
        columns = source_w.div_ceil(cell_w);
        rows = source_h.div_ceil(cell_h);
    } else if columns == 0 {
        let dest_w = rows.saturating_mul(cell_h).saturating_mul(source_w) / source_h;
        columns = dest_w.div_ceil(cell_w);
    } else if rows == 0 {
        let dest_h = columns.saturating_mul(cell_w).saturating_mul(source_h) / source_w;
        rows = dest_h.div_ceil(cell_h);
    }
    (
        columns.clamp(1, u64::from(u16::MAX)) as u16,
        rows.clamp(1, u64::from(u16::MAX)) as u16,
    )
}

impl Drop for Actor {
    fn drop(&mut self) {
        // actor 退出（Shutdown / ctrl 通道断开）时唤醒并结束 pty-writer，避免
        // 慢 shell 场景下 writer 永远等在 Condvar 上。
        self.write_queue.close();
    }
}

impl Actor {
    fn run(mut self) {
        let pty = self.pty_rx.clone();
        let ctrl = self.ctrl_rx.clone();

        // 键盘/粘贴不再经过 actor：UI 直接推写 backlog，因此这里没有 input 分支。
        loop {
            if self.pty_closed {
                crossbeam_channel::select! {
                    recv(ctrl) -> msg => match msg {
                        Ok(c) => if !self.handle(c) { break },
                        Err(_) => break,
                    },
                }
            } else {
                crossbeam_channel::select! {
                    recv(ctrl) -> msg => match msg {
                        Ok(c) => if !self.handle(c) { break },
                        Err(_) => break,
                    },
                    recv(pty) -> msg => match msg {
                        // Sixel payload 分块到达时 feed 返回 false：没有新内容，
                        // 不应为每 32 KiB 图片数据触发一次整帧重绘。
                        Ok(b) => { if self.feed(&b) { self.wake.notify(); } },
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

    /// actor 侧编码后的输入（鼠标上报等）写进 PTY；绝不能喂给 VT 解析器。
    ///
    /// 只做有界 backlog 的 O(1) push：慢 shell 时最多压 16 MiB，超过则丢弃新块
    /// 并计数/限频日志。actor 绝不在这里阻塞；键盘输入由 UI 直接推同一队列。
    fn write_input(&self, bytes: &[u8]) {
        if self.read_only.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if let Err(PushError::Full) = self.write_queue.push(bytes.to_vec()) {
            note_write_drop(&self.stats, bytes.len());
        }
    }

    /// 旁路扫描键盘协议 CSI；只在真正交给 vte 的字节上调用。
    ///
    /// - kitty set/push/pop/query：core 自己维护 current flags + 屏幕栈；
    /// - xterm XTMODKEYS：alacritty 的 vte Handler 只有空实现，core 自己记状态。
    fn scan_keyboard_modes(&mut self, bytes: &[u8]) -> Vec<KeyboardAction> {
        let mut actions = Vec::new();
        self.keyboard.feed(bytes, &mut actions);
        actions
    }

    /// 应用扫描器的状态更新/回包。回包在 vte 之后写，和旧实现一样保持输入
    /// 响应顺序：同一批字节里的 query 能立即得到答案。
    fn apply_keyboard_actions(&mut self, actions: Vec<KeyboardAction>) {
        for action in actions {
            match action {
                KeyboardAction::SetKeyboardFlags(flags) => {
                    self.keyboard_flags.store(flags, Ordering::Relaxed);
                },
                KeyboardAction::SetModifyOtherKeys(level) => {
                    self.modify_other_keys.store(level, Ordering::Relaxed);
                },
                KeyboardAction::Reply(reply) => self.write_input(&reply),
            }
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

    /// 解析一批 PTY 字节。返回 false 表示这批字节全是 Sixel 中间数据、
    /// 没有产生可见变化，调用方无需唤醒 UI。
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let start = Instant::now();
        use std::sync::atomic::Ordering::Relaxed;
        // 组合音标/ZWJ/VS 是零宽字符，alacritty 把它们挂到前一个格子上但**不标
        // damage**（见 Term::input 的 width == 0 分支）。如果它们单独成批到达，
        // 按脏行拉帧会永远看不到；这里检测到零宽码位就下一帧整屏重取。
        if has_zero_width_chars(bytes) {
            self.force_full = true;
            // 组合字符不产生 damage，必须整帧重取并扫描零宽表，才能发现它挂到哪一格。
            self.refresh_zerowidth = true;
        }

        // 先摘出 Sixel DCS：既避免图片字节进入网格，也避免 BEL 终止的
        // DCS 让 vte 卡在字符串状态吞掉后续输出。
        let decode_sixel = self.sixel_enabled.load(Ordering::Relaxed);
        let decode_iip = self.iip_enabled.load(Ordering::Relaxed);
        let filtered = self.sixel.feed(bytes, decode_sixel);
        let mut parsed = 0usize;
        let mut changed = false;
        if let Some(events) = filtered {
            for event in events {
                match event {
                    SixelEvent::Pass(chunk) => {
                        let (chunk_changed, chunk_parsed) = self.feed_iip(&chunk, decode_iip);
                        changed |= chunk_changed;
                        parsed += chunk_parsed;
                    },
                    SixelEvent::Image(image) => {
                        self.add_sixel_image(image);
                        // 图片把字节流分成两段，清屏序列不可能跨过它。
                        self.image_clear.tail.clear();
                        changed = true;
                    },
                }
            }
        } else {
            let (chunk_changed, chunk_parsed) = self.feed_iip(bytes, decode_iip);
            changed |= chunk_changed;
            parsed += chunk_parsed;
        }

        if changed {
            // GNOME scroll-on-output：开启时任何输出都把视口拉回最新（即使已开启
            // scroll_on_output 的用户明确选择“输出即跟随”）。默认 false 时交给
            // alacritty：用户上滚后 `Grid::scroll_up` 会递增 display_offset，视图
            // 保持在原内容上，不会被新行抢走。
            if self.scroll_on_output {
                self.term.scroll_display(Scroll::Bottom);
            }
            // 鼠标模式（1000/1002/1003）常随 TUI 启动一起打开，UI 需要马上看到，
            // 不能等下一次拉帧，所以每次解析后刷新快照。
            self.mode_bits.store(self.term.mode().bits(), Relaxed);
            self.store_cursor();
        }
        let nanos = start.elapsed().as_nanos() as u64;
        self.stats.bytes_parsed.fetch_add(parsed as u64, Relaxed);
        self.stats.parse_nanos.fetch_add(nanos, Relaxed);
        self.stats.parse_batches.fetch_add(1, Relaxed);
        if changed {
            self.stats.wakeups.fetch_add(1, Relaxed);
        }
        changed
    }

    /// 二级过滤：从 Sixel 放行的普通字节里再摘出 `OSC 1337;File=`。
    ///
    /// 返回 `(是否有可见变化, 真正交给 vte 的字节数)`；`Pass`/`Image` 事件
    /// 严格按输入顺序处理，保证图片锚点就是该序列结束时的光标。
    fn feed_iip(&mut self, bytes: &[u8], decode: bool) -> (bool, usize) {
        let Some(events) = self.iip.feed(bytes, decode) else {
            // None = 本段没有 IIP；原始字节直接交给 Kitty APC 过滤 + vte。
            return self.process_vte_chunk(bytes);
        };
        let mut changed = false;
        let mut parsed = 0usize;
        for event in events {
            match event {
                IipEvent::Pass(chunk) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    let (chunk_changed, chunk_parsed) = self.process_vte_chunk(&chunk);
                    changed |= chunk_changed;
                    parsed += chunk_parsed;
                },
                IipEvent::Image(image) => {
                    self.add_iip_image(image);
                    // 图片把字节流分成两段，清屏序列不可能跨过它。
                    self.image_clear.tail.clear();
                    changed = true;
                },
            }
        }
        (changed, parsed)
    }

    /// 回应 XTWINOPS：`CSI 16 t` 单元格像素、`CSI 14 t` 窗口像素。
    ///
    /// dsh-tui 等 TUI 用这两个查询决定图像编码分辨率；不回就只能用 8×16
    /// 默认值，图片会明显低清。响应格式与 xterm 一致：`CSI 6;h;w t` /
    /// `CSI 4;h;w t`。
    fn scan_pixel_size_queries(&mut self, bytes: &[u8]) {
        let mut buf = Vec::with_capacity(self.query_tail.len() + bytes.len());
        buf.extend_from_slice(&self.query_tail);
        buf.extend_from_slice(bytes);
        let mut i = 0;
        while i + 2 < buf.len() {
            if buf[i] == 0x1b && buf[i + 1] == b'[' {
                let mut j = i + 2;
                while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                    j += 1;
                }
                if j < buf.len() && buf[j] == b't' {
                    let param = std::str::from_utf8(&buf[i + 2..j]).unwrap_or("");
                    let (code, reply_h, reply_w) = match param {
                        "16" => (
                            6,
                            self.cell_height.max(1),
                            self.cell_width.max(1),
                        ),
                        "14" => (
                            4,
                            self.size
                                .rows
                                .saturating_mul(self.cell_height.max(1) as usize)
                                .min(u16::MAX as usize) as u16,
                            self.size
                                .cols
                                .saturating_mul(self.cell_width.max(1) as usize)
                                .min(u16::MAX as usize) as u16,
                        ),
                        _ => {
                            i = j + 1;
                            continue;
                        }
                    };
                    let reply = format!("\x1b[{code};{reply_h};{reply_w}t");
                    let len = reply.len();
                    if let Err(PushError::Full) = self.write_queue.push(reply.into_bytes()) {
                        note_write_drop(&self.stats, len);
                    }
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
        let keep = 24.min(buf.len());
        self.query_tail.clear();
        if keep > 0 {
            self.query_tail.extend_from_slice(&buf[buf.len() - keep..]);
        }
    }

    /// Kitty APC 三级过滤：APC 解析成图片命令，其余字节按原顺序走
    /// XTWINOPS/OSC/键盘/vte。
    ///
    /// 返回 `(是否有可见变化, 真正交给 vte 的字节数)`。Kitty APC 本身不再
    /// 计入 `bytes_parsed`。
    fn process_vte_chunk(&mut self, chunk: &[u8]) -> (bool, usize) {
        if chunk.is_empty() {
            return (false, 0);
        }
        if !self.kitty.has_partial_sequence() && !chunk.contains(&0x1b) {
            self.process_vte_bytes(chunk);
            return (true, chunk.len());
        }
        let events = self.kitty.feed(chunk);
        let mut changed = false;
        let mut parsed = 0usize;
        for event in events {
            match event {
                KittyEvent::Pass(bytes) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    changed = true;
                    parsed += bytes.len();
                    self.process_vte_bytes(&bytes);
                },
                KittyEvent::Command(command) => {
                    changed |= self.handle_kitty_command(command);
                },
                KittyEvent::Error(error) => {
                    self.handle_kitty_error(error);
                },
            }
        }
        (changed, parsed)
    }

    /// 通知扫描/清屏检测/键盘协议扫描/vte 解析都只处理真正交给 vte 的字节。
    fn process_vte_bytes(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.scan_pixel_size_queries(chunk);
        let full_clear = self.image_clear.feed(chunk);
        let had_ech = !self.image_clear.ech.is_empty();
        if full_clear {
            // 2J/3J、备用屏切换、RIS 会清空整屏：所有协议的内联图片都要清。
            self.clear_all_images();
        }
        let mut notif_events = Vec::new();
        self.osc.feed(chunk, &mut notif_events);
        for event in notif_events {
            self.events.send(event);
        }
        let keyboard_actions = self.scan_keyboard_modes(chunk);
        self.advance_vte(chunk);
        let cursor = self.term.grid().cursor.point;
        // ECH 已经过 vte。常见场景一次性发出：`CUP -> ECH`，此时光标仍在
        // 被擦矩形的右端；按 ECH 宽度取光标左侧区域，不删除同行其他图片。
        let samples = self.image_clear.ech.clone();
        if had_ech && !full_clear {
            let grid = self.term.grid();
            let off = grid.display_offset() as i64;
            for erase in samples.into_iter().rev() {
                let cells = (erase.unwrap_or(1).max(1) as usize).min(self.term.columns());
                let end = cursor.column.0.saturating_add(1);
                let start = end.saturating_sub(cells);
                self.images.erase_viewport_rectangle(
                    grid,
                    cursor.line.0 as i64 + off,
                    start,
                    end,
                    off as usize,
                    self.term.columns(),
                    self.term.screen_lines(),
                    self.cell_width,
                    self.cell_height,
                );
                // 同一 chunk 中连续的 ECH 都作用于同一光标位置。
            }
        }
        self.apply_keyboard_actions(keyboard_actions);
    }

    fn store_grid_lines(&self) {
        let hist = self.term.grid().history_size() as u64;
        let total = self.term.grid().total_lines() as u64 & 0xffff_ffff;
        self.grid_lines
            .store((hist << 32) | total, std::sync::atomic::Ordering::Relaxed);
    }

    /// 把字节交给 vte，并刷新 grid 行数诊断快照（RowId 锚点不再需要 Tervo
    /// 侧累计滚动行数）。
    fn advance_vte(&mut self, chunk: &[u8]) {
        advance_and_publish_grid_lines(
            &mut self.term,
            &mut self.processor,
            Some(&self.grid_lines),
            chunk,
        );
    }

    /// 处理一条 Kitty 图形命令。返回是否产生可见变化。
    fn handle_kitty_command(&mut self, command: KittyCommand) -> bool {
        match command {
            KittyCommand::Response(bytes) => {
                self.write_input(&bytes);
                false
            },
            KittyCommand::Transmit(transmit) => {
                let image_id = transmit.image.id;
                let (evicted, stored) = self.kitty_store.insert(transmit);
                for id in evicted {
                    self.kitty.forget_image(id);
                }
                if !stored {
                    // 资源预算拒绝：让解析器的 image 索引一起失效，后续 a=p
                    // 会得到 ENOENT，客户端可重传。
                    self.kitty.forget_image(image_id);
                    log::debug!("kitty: image {image_id} rejected by image budget");
                }
                stored
            },
            KittyCommand::Place(place) => self.handle_kitty_place(place),
            KittyCommand::Delete(delete) => self.handle_kitty_delete(delete),
        }
    }

    fn handle_kitty_place(&mut self, place: PlacementCommand) -> bool {
        let cursor_movement = place.cursor_movement;
        let (cursor, row_id, cols) = {
            let grid = self.term.grid();
            let cursor = grid.cursor.point;
            (
                cursor,
                grid.row_id(cursor.line),
                self.term.columns(),
            )
        };
        let Some((cells_w, cells_h)) = self.kitty_store.place(
            place,
            cursor,
            row_id,
            cols,
            self.cell_width,
            self.cell_height,
        ) else {
            log::debug!("kitty: placement for unknown/evicted image ignored");
            return false;
        };
        if cursor_movement == CursorMovement::Move {
            self.move_kitty_cursor(cursor, cells_w, cells_h);
        }
        true
    }

    /// Kitty 默认 `C=0`：放置后光标右移 columns、下移 rows。`C=1` 由调用方
    /// 直接跳过。越界按屏幕边界截断（协议对越界位置未定义）。
    fn move_kitty_cursor(&mut self, cursor: Point, cells_w: u16, cells_h: u16) {
        let rows = self.term.screen_lines().saturating_sub(1) as i32;
        let cols = self.term.columns().saturating_sub(1) as i32;
        let line = (cursor.line.0 + i32::from(cells_h)).clamp(0, rows);
        let col = (cursor.column.0 as i32 + i32::from(cells_w)).clamp(0, cols);
        let seq = format!("\x1b[{};{}H", line + 1, col + 1);
        // 直接喂 vte；不经过图片清屏扫描器，避免移动光标被误判成擦图。
        self.advance_vte(seq.as_bytes());
    }

    fn handle_kitty_delete(&mut self, delete: DeleteCommand) -> bool {
        let ctx = {
            let grid = self.term.grid();
            let cursor = grid.cursor.point;
            let display_offset = grid.display_offset();
            let rows = self.term.screen_lines();
            let mut viewport_row_ids = Vec::with_capacity(rows);
            for row in 0..rows {
                viewport_row_ids.push(grid.row_id(Line(row as i32 - display_offset as i32)));
            }
            KittyDeleteCtx {
                cursor_row_id: grid.row_id(cursor.line),
                cursor_col: cursor.column.0 as u32,
                viewport_row_ids,
            }
        };
        for id in self.kitty_store.delete(delete.target, &ctx) {
            self.kitty.forget_image(id);
        }
        true
    }

    fn handle_kitty_error(&mut self, error: KittyError) {
        if let Some(response) = error.response {
            self.write_input(&response);
        }
        log::debug!(
            "kitty: {}:{} image={:?} placement={:?}",
            error.code.as_str(),
            error.detail,
            error.image_id,
            error.placement_id
        );
    }

    fn clear_all_images(&mut self) {
        self.images.clear();
        self.kitty_store.clear();
        self.kitty.reset();
    }

    /// 把一张 Sixel 图片锚定到**当前**光标（可能刚解析完同一 chunk 里图片
    /// 之前的位置控制序列，因此必须逐个事件读取，不能等整批结束）。
    ///
    /// 锚点是光标所在 grid 行的稳定 RowId，而不是易变的行号。
    fn add_sixel_image(&mut self, image: SixelImage) {
        if !self.sixel_enabled.load(Ordering::Relaxed) {
            return;
        }
        let grid = self.term.grid();
        let cursor = grid.cursor.point;
        let row_id = grid.row_id(cursor.line);
        let display_offset = grid.display_offset();
        let cols = self.term.columns();
        self.images.push(
            ImageKind::Sixel,
            image,
            cursor,
            row_id,
            display_offset,
            self.sixel_scrolling,
            cols,
        );
    }

    /// IIP 图片与 Sixel 共用锚点/滚动/store 预算，只是解码来源不同。
    fn add_iip_image(&mut self, image: IipImage) {
        if !self.iip_enabled.load(Ordering::Relaxed) {
            return;
        }
        let grid = self.term.grid();
        let cursor = grid.cursor.point;
        let row_id = grid.row_id(cursor.line);
        let display_offset = grid.display_offset();
        let cols = self.term.columns();
        self.images.push_rgba(
            ImageKind::Iip,
            image.width,
            image.height,
            image.rgba,
            cursor,
            row_id,
            display_offset,
            self.sixel_scrolling,
            cols,
        );
    }

    fn viewport_point(&self, line: u16, col: u16) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        let rows = self.term.screen_lines() as i32;
        let cols = self.term.columns();
        let line = (line as i32).min(rows - 1);
        let col = (col as usize).min(cols.saturating_sub(1));
        Point::new(Line(line - offset), Column(col))
    }

    fn search(
        &mut self,
        query: &str,
        forward: bool,
        options: SearchOptions,
    ) -> Result<Option<SearchHit>, String> {
        if query.is_empty() {
            self.search_span = None;
            self.wake.notify();
            return Ok(None);
        }
        // 非法 regex 必须把错误交回 UI，而不是静默返回 None（否则看起来像“没命中”）。
        let mut regex = match RegexSearch::new(&search_pattern(query, options)) {
            Ok(regex) => regex,
            Err(e) => {
                self.search_span = None;
                self.wake.notify();
                return Err(format!("invalid regex: {e}"));
            },
        };
        let direction = if forward { Direction::Right } else { Direction::Left };
        let offset = self.term.grid().display_offset() as i32;
        let origin = match self.search_span {
            Some(span) => {
                let point = if forward {
                    Point::new(
                        Line(span.end_line as i32 - offset),
                        Column(span.end_col as usize),
                    )
                } else {
                    Point::new(
                        Line(span.start_line as i32 - offset),
                        Column(span.start_col as usize),
                    )
                };
                step_search_point(&self.term, point, forward)
            },
            None => self.term.grid().cursor.point,
        };

        let Some(found) =
            self.term.search_next(&mut regex, origin, direction, Side::Left, None)
        else {
            return Ok(None);
        };
        // 计数与命中在同一次 actor 请求里做完：扫的是 alacritty grid，
        // 不是 UI 帧；失败/空查询不会走到这里。
        let (index, total) = count_search_matches(&self.term, &mut regex, &found);
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
            return Ok(None);
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
        Ok(Some(SearchHit {
            line: span.start_line,
            start_col: span.start_col,
            end_col: span.end_col,
            index,
            total,
        }))
    }

    /// 复位终端状态（RIS）。`clear_scrollback` 为 true 时再显式清一次回滚。
    fn reset(&mut self, clear_scrollback: bool) {
        // alacritty 的 Handler::reset_state() 会清屏、清回滚，并重置模式/光标/
        // 字符集/滚动区/标题栈等，等价于 RIS。
        self.term.reset_state();
        if clear_scrollback {
            self.term.clear_screen(ClearMode::Saved);
        }
        self.term.selection = None;
        self.sel_anchor = None;
        self.search_span = None;
        self.clear_all_images();
        self.force_full = true;
        self.refresh_zerowidth = false;
        // RIS 同样复位 kitty 键盘栈与 xterm modifyOtherKeys。
        self.keyboard.reset();
        use std::sync::atomic::Ordering::Relaxed;
        self.keyboard_flags.store(0, Relaxed);
        self.modify_other_keys.store(0, Relaxed);
        self.mode_bits.store(self.term.mode().bits(), Relaxed);
        self.store_cursor();
        self.sync_scroll();
        self.wake.notify();
    }

    /// 全部内容（回滚 + 视口）转成文本；末尾补一个换行（与常见抓取工具一致）。
    fn contents(&self) -> String {
        let text = self.term.bounds_to_string(
            Point::new(self.term.topmost_line(), Column(0)),
            Point::new(self.term.bottommost_line(), self.term.last_column()),
        );
        let text = text.trim_end();
        if text.is_empty() {
            String::new()
        } else {
            format!("{text}\n")
        }
    }

    fn handle(&mut self, ctrl: Ctrl) -> bool {
        match ctrl {
            Ctrl::Pull {
                id,
                mut frame,
                force_full,
            } => {
                // 上一帧超时后必须整屏重取：迟到的 Pull 可能已经消费了 damage。
                if force_full {
                    self.force_full = true;
                }
                self.fill(&mut frame);
                // UI 正在等这一帧；先发布同一时刻的滚动快照，避免 UI 拿到帧后
                // 读到上一轮的 offset（滚动条会滞后一帧）。容量 1，不会堆积。
                self.sync_scroll();
                let mut frame = frame;
                loop {
                    match self.pull_tx.try_send((id, frame)) {
                        Ok(()) => break,
                        Err(TrySendError::Full(returned)) => {
                            // UI 拉帧超时后旧响应还占着容量 1；丢弃旧帧，保留最新帧。
                            frame = returned.1;
                            let _ = self.pull_rx.try_recv();
                            self.stats.pull_stale_responses.fetch_add(1, Ordering::Relaxed);
                        },
                        // UI 已退出：没人收帧，直接丢弃，绝不在 actor 里阻塞。
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
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
            Ctrl::SetSixel(enabled) => {
                self.sixel_enabled.store(enabled, Ordering::Relaxed);
                if !enabled {
                    // 关闭后已有 Sixel 立即清除；IIP 图片不受影响。
                    self.images.clear_kind(ImageKind::Sixel);
                }
                self.wake.notify();
                true
            },
            Ctrl::SetSixelScrolling(scrolling) => {
                self.sixel_scrolling = scrolling;
                let grid = self.term.grid();
                let offset = grid.display_offset();
                self.images.set_scrolling(scrolling, grid, offset);
                self.wake.notify();
                true
            },
            Ctrl::SetScrollback(lines) => {
                let lines = lines.clamp(1, 1_000_000);
                if self.term_config.scrolling_history != lines {
                    self.term_config.scrolling_history = lines;
                    self.term.set_options(self.term_config.clone());
                    self.store_grid_lines();
                    self.force_full = true;
                    self.wake.notify();
                }
                true
            },
            Ctrl::SetIip(enabled) => {
                self.iip_enabled.store(enabled, Ordering::Relaxed);
                if !enabled {
                    // 只清 IIP；Sixel 与 DA1 宣告保持不变。
                    self.images.clear_kind(ImageKind::Iip);
                }
                self.wake.notify();
                true
            },
            Ctrl::SetCellSize { width, height } => {
                self.cell_width = width.max(1);
                self.cell_height = height.max(1);
                self.kitty_store
                    .retune_cells(self.cell_width, self.cell_height);
                self.iip.set_cell_size(width, height);
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
                self.store_grid_lines();
                self.term.selection = None;
                self.sel_anchor = None;
                self.clear_all_images();
                self.force_full = true;
                self.wake.notify();
                true
            },
            Ctrl::Reset => {
                self.reset(false);
                true
            },
            Ctrl::ResetAndClear => {
                self.reset(true);
                true
            },
            Ctrl::ImageRgba { source_id, reply } => {
                let value = self
                    .images
                    .images
                    .iter()
                    .find(|image| image.source_id == source_id)
                    .map(|image| (image.width, image.height, image.data.clone()))
                    .or_else(|| self.kitty_store.resource_by_source(source_id));
                let _ = reply.send(value);
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
            Ctrl::Search { query, forward, options, reply } => {
                let hit = self.search(&query, forward, options);
                let _ = reply.send(hit);
                true
            },
            Ctrl::Contents(reply) => {
                let _ = reply.send(Some(self.contents()));
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
                self.kitty_store
                    .retune_cells(self.cell_width, self.cell_height);
                self.store_grid_lines();
                let _ = self.master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: cols.saturating_mul(self.cell_width.max(1)),
                    pixel_height: rows.saturating_mul(self.cell_height.max(1)),
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
        self.fill_images(frame);
        let nanos = start.elapsed().as_nanos() as u64;
        self.stats.pulls.fetch_add(1, Relaxed);
        self.stats.fill_nanos.fetch_add(nanos, Relaxed);
        self.stats.cells_copied.fetch_add(frame.copied_cells as u64, Relaxed);
    }

    /// 按当前滚动位置把图片锚点展开成视口行，写进帧。
    ///
    /// 位置解析与生命周期淘汰都在 [`ImageStore::place`] 里按 `RowId` 完成：
    /// 行身份找不到就 drop，不再有 `scroll_total` 时间轴。
    fn fill_images(&mut self, frame: &mut Frame) {
        frame.images.clear();
        frame.image_placements.clear();
        let sixel = self.sixel_enabled.load(Ordering::Relaxed);
        let iip = self.iip_enabled.load(Ordering::Relaxed);
        if !sixel && !iip && self.kitty_store.is_empty() {
            return;
        }
        let grid = self.term.grid();
        let display_offset = grid.display_offset();
        let rows = self.term.screen_lines();
        let cols = self.term.columns();

        if sixel || iip {
            let start = frame.images.len();
            self.images.place(
                grid,
                display_offset,
                rows,
                cols,
                self.cell_height,
                &mut frame.images,
            );
            let cell_w = u32::from(self.cell_width.max(1));
            let cell_h = u32::from(self.cell_height.max(1));
            for image in &frame.images[start..] {
                let cells_w = image.width.div_ceil(cell_w).clamp(1, u32::from(u16::MAX)) as u16;
                let cells_h = image.height.div_ceil(cell_h).clamp(1, u32::from(u16::MAX)) as u16;
                frame.image_placements.push(ImagePlacementMeta {
                    placement_id: image.id,
                    image_id: image.id,
                    row_id: image.row_id,
                    source_rect: None,
                    cells_w,
                    cells_h,
                    z_index: -1,
                    above_text: false,
                });
            }
        }

        self.kitty_store.place_frame(
            grid,
            display_offset,
            rows,
            cols,
            &mut frame.images,
            &mut frame.image_placements,
        );
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
    frame.skipped = false;
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
            // 一帧的 renderer 按整行绘制；Frame 也必须整行与 Term 一致。
            // 若只合并 damage 的 left..right，damage 之外仍是旧 cell，renderer
            // 绘制整行时就会把旧字符重新画出来（dsh 整屏重写时非常明显）。
            for i in 0..frame.damage.len() {
                let d = frame.damage[i];
                let line = Line(d.line as i32);
                let base = d.line as usize * cols;
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
                frame.copied_cells += cols as u32;
            }
        }

        // 视口行身份：无论整屏还是脏行路径都全量刷新（rows 次 u64 写入），
        // 这样滚动/行移动后不会留下旧 RowId。图片 placement 只认这里。
        frame.row_ids.resize(rows, 0);
        let origin = -(display_offset as i32);
        for row in 0..rows {
            frame.row_ids[row] = grid.row_id(Line(row as i32 + origin));
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

    fn test_image(w: u32, h: u32) -> SixelImage {
        SixelImage { width: w, height: h, rgba: vec![255; w as usize * h as usize * 4] }
    }

    /// 5 行视口的测试终端；scrollback 可配置。
    fn test_term(rows: usize, scrollback: usize) -> Term<alacritty_terminal::event::VoidListener> {
        use alacritty_terminal::event::VoidListener;
        let size = GridSize::new(80, rows);
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        Term::new(config, &size, VoidListener)
    }

    /// 输出若干行，制造可回滚历史（视口 5 行时第 5 个 LF 起开始滚动）。
    fn feed_lines(term: &mut Term<alacritty_terminal::event::VoidListener>, count: usize) {
        let mut processor: Processor = Processor::new();
        for i in 0..count {
            processor.advance(term, format!("L{i}\r\n").as_bytes());
        }
    }

    #[test]
    fn printable_writes_damage_every_written_cell() {
        use alacritty_terminal::term::TermDamage;
        let mut term = test_term(2, 0);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, b"ABCDE");
        term.reset_damage();
        processor.advance(&mut term, b"\x1b[1;1Hxy");
        let mut bounds = None;
        match term.damage() {
            TermDamage::Full => panic!("expected partial damage"),
            TermDamage::Partial(iter) => {
                for d in iter {
                    if d.line == 0 {
                        bounds = Some((d.left, d.right));
                    }
                }
            },
        }
        let (left, right) = bounds.expect("line 0 must be damaged");
        assert!(left <= 0 && right >= 1, "writes at cols 0..=1 must expand damage: {left}..={right}");
    }

    #[test]
    fn damaged_line_refreshes_the_whole_frame_row() {
        let mut term = test_term(2, 0);
        let mut processor: Processor = Processor::new();
        let mut frame = Box::new(Frame::default());
        let palette = Palette::default();
        processor.advance(&mut term, b"ABCDE");
        fill_frame(&mut term, &mut frame, &palette, false, None, false);
        // 人为把 damage 范围之外的 cell 弄脏，模拟旧 Frame 残留。
        frame.cells[10].ch = 'Z' as u32;
        processor.advance(&mut term, b"\\x1b[1;1Hxy");
        fill_frame(&mut term, &mut frame, &palette, false, None, false);
        assert!(!frame.full, "本用例需要 partial damage 才有区分度");
        assert_ne!(frame.cell(0, 10).ch, 'Z' as u32, "damage 行必须整行刷新");
    }

    #[test]
    fn image_placement_follows_row_id_across_scroll() {
        let mut term = test_term(5, 10);
        feed_lines(&mut term, 10);
        assert!(term.grid().history_size() >= 2, "需要可回滚历史");

        let line = Line(2);
        let row_id = term.grid().row_id(line);
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(line, Column(2)),
            row_id,
            0,
            true,
            80,
        );

        let mut out = Vec::new();
        store.place(term.grid(), 0, 5, 80, 16, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].row, out[0].col), (2, 2));
        assert_eq!(out[0].row_id, row_id);

        // 视口上滚 2 行：同一逻辑行在屏幕上下移 2 行，RowId 不变。
        term.scroll_display(Scroll::Delta(2));
        let offset = term.grid().display_offset();
        out.clear();
        store.place(term.grid(), offset, 5, 80, 16, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].row, 4, "视口行必须随 display_offset 移动");
        assert_eq!(out[0].row_id, row_id, "滚动只改变视口投影，不改变 RowId");
    }

    #[test]
    fn image_is_dropped_when_row_id_is_reset() {
        let mut term = test_term(5, 10);
        let row_id = term.grid().row_id(Line(1));
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Iip,
            test_image(16, 16),
            Point::new(Line(1), Column(0)),
            row_id,
            0,
            true,
            80,
        );

        // CSI 1J：光标移到第 5 行后清上方，被清的 Row 会拿到全新 RowId。
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, b"\x1b[5;1H\x1b[1J");
        assert_eq!(term.grid().find_row_id(row_id), None, "reset 后旧 RowId 必须消失");

        let mut out = Vec::new();
        store.place(term.grid(), 0, 5, 80, 16, &mut out);
        assert!(out.is_empty());
        assert!(store.images.is_empty(), "RowId 消失的 placement 必须显式 drop");
        assert_eq!(store.bytes, 0);
    }

    #[test]
    fn image_above_viewport_is_kept_until_row_is_evicted() {
        let mut term = test_term(5, 3);
        feed_lines(&mut term, 8);
        assert_eq!(term.grid().history_size(), 3, "history 应到上限");

        let oldest = term.grid().topmost_line();
        let row_id = term.grid().row_id(oldest);
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Iip,
            test_image(16, 16),
            Point::new(oldest, Column(0)),
            row_id,
            0,
            true,
            80,
        );

        // 完全在视口上方但仍在 scrollback 里：保留以便滚回，不输出 placement。
        let mut out = Vec::new();
        store.place(term.grid(), 0, 5, 80, 16, &mut out);
        assert!(out.is_empty());
        assert_eq!(store.images.len(), 1, "仍可达的历史行不能提前释放");

        // 继续输出把最旧行挤出 scrollback：RowId 消失后必须显式释放。
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, b"more\r\nmore\r\nmore\r\n");
        assert_eq!(term.grid().find_row_id(row_id), None, "最旧行必须被淘汰");
        out.clear();
        store.place(term.grid(), 0, 5, 80, 16, &mut out);
        assert!(out.is_empty());
        assert!(store.images.is_empty(), "RowId 淘汰后 placement 必须释放");
        assert_eq!(store.bytes, 0);
    }

    #[test]
    fn fixed_image_keeps_screen_row_but_is_owned_by_row_id() {
        let mut term = test_term(5, 10);
        feed_lines(&mut term, 10);
        let line = Line(2);
        let row_id = term.grid().row_id(line);
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(line, Column(2)),
            row_id,
            0,
            false,
            80,
        );

        let mut out = Vec::new();
        store.place(term.grid(), 0, 5, 80, 16, &mut out);
        assert_eq!(out[0].row, 2);

        // sixel_scrolling=false：屏幕行固定，不随视口滚动下移。
        term.scroll_display(Scroll::Delta(2));
        out.clear();
        store.place(term.grid(), 2, 5, 80, 16, &mut out);
        assert_eq!(out[0].row, 2, "固定图片锚定屏幕行");
        assert_eq!(out[0].row_id, row_id);

        // 逻辑行被 reset 后，固定图片也不能继续泄漏。
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, b"\x1b[5;1H\x1b[1J");
        assert_eq!(term.grid().find_row_id(row_id), None);
        out.clear();
        store.place(term.grid(), 2, 5, 80, 16, &mut out);
        assert!(out.is_empty());
        assert!(store.images.is_empty(), "固定图片的 RowId 消失后必须 drop");
    }

    #[test]
    fn toggling_scrolling_reanchors_with_row_id() {
        let mut term = test_term(5, 10);
        feed_lines(&mut term, 10);
        let line = Line(2);
        let row_id = term.grid().row_id(line);
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(line, Column(2)),
            row_id,
            0,
            true,
            80,
        );

        let mut out = Vec::new();
        term.scroll_display(Scroll::Delta(2));
        store.place(term.grid(), 2, 5, 80, 16, &mut out);
        assert_eq!(out[0].row, 4);

        // 切到固定模式：记录当前视口行，之后滚动不再移动。
        store.set_scrolling(false, term.grid(), 2);
        out.clear();
        store.place(term.grid(), 2, 5, 80, 16, &mut out);
        assert_eq!(out[0].row, 4);

        // 切回滚动模式：RowId 本身没变，按当前 offset 重新解析即可。
        store.set_scrolling(true, term.grid(), 2);
        out.clear();
        store.place(term.grid(), 0, 5, 80, 16, &mut out);
        assert_eq!(out[0].row, 2, "回到底部后图片回到逻辑行");
        assert_eq!(out[0].row_id, row_id);
    }

    #[test]
    fn image_store_clear_kind_keeps_other_protocol() {
        let term = test_term(5, 10);
        let row_id = term.grid().row_id(Line(0));
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(Line(0), Column(0)),
            row_id,
            0,
            true,
            80,
        );
        store.push_rgba(
            ImageKind::Iip,
            8,
            8,
            vec![7; 8 * 8 * 4],
            Point::new(Line(1), Column(1)),
            term.grid().row_id(Line(1)),
            0,
            true,
            80,
        );
        assert_eq!(store.images.len(), 2);
        assert!(store.bytes > 0);
        store.clear_kind(ImageKind::Iip);
        assert_eq!(store.images.len(), 1);
        assert_eq!(store.images[0].kind, ImageKind::Sixel);
        let mut out = Vec::new();
        store.place(term.grid(), 0, 5, 80, 16, &mut out);
        assert_eq!(out.len(), 1, "清除 IIP 不应带走 Sixel");
        store.clear_kind(ImageKind::Sixel);
        assert!(store.images.is_empty());
        assert_eq!(store.bytes, 0);
    }

    #[test]
    fn image_store_clears_and_drops_empty() {
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(Line(0), Column(0)),
            0,
            0,
            true,
            80,
        );
        store.push(
            ImageKind::Sixel,
            test_image(0, 16),
            Point::new(Line(0), Column(0)),
            0,
            0,
            true,
            80,
        );
        assert_eq!(store.images.len(), 1);
        assert!(store.bytes > 0);
        store.clear();
        assert!(store.images.is_empty());
        assert_eq!(store.bytes, 0);
    }

    #[test]
    fn image_ids_are_generation_safe_after_clear() {
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(Line(0), Column(0)),
            0,
            0,
            true,
            80,
        );
        let old = (store.images[0].id, store.images[0].source_id);
        store.clear();
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(Line(0), Column(0)),
            0,
            0,
            true,
            80,
        );
        let new = (store.images[0].id, store.images[0].source_id);
        assert!(new.0 > old.0, "placement ID 在 clear 后仍必须单调");
        assert!(new.1 > old.1, "source ID 在 clear 后仍必须单调");
    }

    #[test]
    fn ech_rectangle_removes_only_intersecting_image() {
        let term = test_term(5, 10);
        let row_id = term.grid().row_id(Line(2));
        let mut store = ImageStore::default();
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(Line(2), Column(0)),
            row_id,
            0,
            true,
            80,
        );
        store.push(
            ImageKind::Sixel,
            test_image(16, 16),
            Point::new(Line(2), Column(8)),
            row_id,
            0,
            true,
            80,
        );
        store.erase_viewport_rectangle(term.grid(), 2, 1, 3, 0, 80, 5, 8, 16);
        assert_eq!(store.images.len(), 1);
        assert_eq!(store.images[0].col, 8);
    }

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

    #[test]
    fn write_queue_is_fifo_bounded_and_close_wakes_consumer() {
        let queue = WriteQueue::new();
        assert!(queue.push(b"first".to_vec()).is_ok());
        assert!(queue.push(b"second".to_vec()).is_ok());
        assert_eq!(queue.pop().as_deref(), Some(&b"first"[..]));
        assert_eq!(queue.pop().as_deref(), Some(&b"second"[..]));

        // 字节上限：塞满 16 MiB 后下一条必须明确 Full，而不是无限增长/阻塞。
        assert!(queue.push(vec![1u8; WRITE_BACKLOG_BYTES]).is_ok());
        assert!(matches!(queue.push(vec![2u8]), Err(PushError::Full)));
        // 关闭后消费者立即被唤醒；生产者得到 Closed（静默，不刷丢弃日志）。
        queue.close();
        assert!(queue.pop().is_none());
        assert!(matches!(queue.push(vec![3u8]), Err(PushError::Closed)));
    }

    #[test]
    fn write_queue_chunk_cap_protects_against_tiny_message_flood() {
        let queue = WriteQueue::new();
        for _ in 0..WRITE_BACKLOG_CHUNKS {
            assert!(queue.push(vec![1u8]).is_ok());
        }
        assert!(matches!(queue.push(vec![2u8]), Err(PushError::Full)));
        queue.close();
    }
}
