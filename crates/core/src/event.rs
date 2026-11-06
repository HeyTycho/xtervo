// SPDX-License-Identifier: GPL-3.0-only
//! 跨线程事件与有界通道。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};

/// OSC 9;4 的进度状态（ConEmu / Windows Terminal 语义）。
///
/// 数值与协议里的 state 字段一致，UI 侧可直接转成 int。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProgressState {
    /// 0：清除进度。
    #[default]
    Clear,
    /// 1：进行中（可能带 0..=100 的百分比）。
    Normal,
    /// 2：出错。
    Error,
    /// 3：不确定进度（没有百分比）。
    Indeterminate,
    /// 4：暂停。
    Paused,
}

/// 一次进度更新。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub state: ProgressState,
    /// 0..=100；`None` = 协议没给或给了非法/越界值。
    pub percent: Option<u8>,
}

/// 终端主动发起的通知（OSC 9 / OSC 777 / BEL）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    /// OSC 777 带标题；OSC 9 没有，留空由 UI 回退到 Tab 标题。
    pub title: String,
    pub body: String,
}

/// OSC 52 的目标选择区。
///
/// xterm 的 `Pc` 参数分别是 `c`（系统剪贴板）、`p`（PRIMARY）、`s`（SELECTION）。
/// Linux/X11 上没有独立的 SELECTION 剪贴板，`p` 和 `s` 都落到 PRIMARY（中键粘贴）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardSelection {
    /// `c`：桌面剪贴板，Ctrl+V / 右键粘贴使用。
    Clipboard,
    /// `p` / `s`：X11 PRIMARY 选区。
    Primary,
}

/// 远端通过 OSC 52 请求的剪贴板操作，由 UI 线程实际执行。
///
/// 只传文本：控制序列的 base64 已在 core 解析时解码并校验；读写系统剪贴板
/// 需要 arboard/X11，属于 app 层职责。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardRequest {
    /// `OSC 52;Pc;BASE64`：写入文本。
    Store { selection: ClipboardSelection, text: String },
    /// `OSC 52;Pc;?`：读取剪贴板，UI 需用 `PaneHandle::clipboard_reply` 回写 PTY。
    Load { selection: ClipboardSelection },
    /// `OSC 52;Pc;`：清空。
    Clear { selection: ClipboardSelection },
}

/// 从终端核心发往 UI 的语义事件。高频的「内容变了」不在此列：
/// 它走 [`WakeSignal`]（容量 1，天然合并）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreEvent {
    Title(String),
    ResetTitle,
    Bell,
    /// OSC 9;4;[state];[percent]
    Progress(Progress),
    /// OSC 9;message / OSC 777;notify;title;body
    Notification(Notification),
    /// OSC 7;file://host/path（cwd）。MVP 只上报，不改变会话行为。
    Cwd(String),
    /// OSC 52：远端读写/清空剪贴板或 PRIMARY。
    Clipboard(ClipboardRequest),
    /// PTY 读到 EOF / 子进程退出。
    Exited,
    CursorBlinkChange,
}

/// 有界、可合并的唤醒信号：容量 1，重复唤醒直接丢弃。
#[derive(Clone)]
pub struct WakeSignal {
    tx: Sender<()>,
}

impl WakeSignal {
    pub fn new() -> (Self, Receiver<()>) {
        let (tx, rx) = bounded(1);
        (Self { tx }, rx)
    }

    /// 不阻塞；已有未消费唤醒时直接丢弃（合并）。
    #[inline]
    pub fn notify(&self) {
        match self.tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {},
            Err(TrySendError::Disconnected(())) => {},
        }
    }
}

/// 语义事件出口：有界、非阻塞；满则丢弃并计数（绝不为 UI 阻塞终端）。
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<CoreEvent>,
    dropped: Arc<AtomicU64>,
}

impl EventSink {
    pub fn new(capacity: usize) -> (Self, Receiver<CoreEvent>) {
        let (tx, rx) = bounded(capacity);
        (Self { tx, dropped: Arc::new(AtomicU64::new(0)) }, rx)
    }

    #[inline]
    pub fn send(&self, event: CoreEvent) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(event) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 轻量计数器，供基准/压测读取。
#[derive(Debug, Default)]
pub struct PaneStats {
    pub bytes_parsed: AtomicU64,
    pub parse_nanos: AtomicU64,
    pub parse_batches: AtomicU64,
    pub pulls: AtomicU64,
    pub fill_nanos: AtomicU64,
    pub cells_copied: AtomicU64,
    pub wakeups: AtomicU64,
    pub wakeups_coalesced: AtomicU64,
}

impl PaneStats {
    #[inline]
    pub fn bytes_per_sec(&self) -> f64 {
        let bytes = self.bytes_parsed.load(Ordering::Relaxed) as f64;
        let ns = self.parse_nanos.load(Ordering::Relaxed) as f64;
        if ns == 0.0 {
            0.0
        } else {
            bytes / (ns / 1e9)
        }
    }

    /// 解析线程平均忙占比（相对墙钟时间）。
    pub fn parse_utilization(&self, wall: std::time::Duration) -> f64 {
        self.parse_nanos.load(Ordering::Relaxed) as f64 / wall.as_nanos().max(1) as f64
    }
}

/// 一个可以「被唤醒」的 UI 侧句柄：把 core 的 WakeSignal 桥接到 Slint 事件循环。
pub struct WakeupBridge {
    rx: Receiver<()>,
    flag: Arc<AtomicBool>,
}

impl WakeupBridge {
    pub fn new(rx: Receiver<()>, flag: Arc<AtomicBool>) -> Self {
        Self { rx, flag }
    }

    /// 阻塞等待一次唤醒；返回前清除标志。
    pub fn wait(&self) {
        if self.rx.recv().is_ok() {
            self.flag.store(false, Ordering::Release);
        }
    }

    pub fn try_wait(&self) -> bool {
        if self.rx.try_recv().is_ok() {
            self.flag.store(false, Ordering::Release);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_signal_coalesces() {
        let (sig, rx) = WakeSignal::new();
        for _ in 0..1000 {
            sig.notify();
        }
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn event_sink_never_blocks() {
        let (sink, rx) = EventSink::new(2);
        for i in 0..100 {
            sink.send(CoreEvent::Bell);
            let _ = i;
        }
        assert_eq!(rx.try_recv(), Ok(CoreEvent::Bell));
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 98);
    }
}
