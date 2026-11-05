// SPDX-License-Identifier: GPL-3.0-only
//! 跨线程事件与有界通道。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};

/// 从终端核心发往 UI 的语义事件。高频的「内容变了」不在此列：
/// 它走 [`WakeSignal`]（容量 1，天然合并）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreEvent {
    Title(String),
    ResetTitle,
    Bell,
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
