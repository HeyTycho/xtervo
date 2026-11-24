// SPDX-License-Identifier: GPL-3.0-only
//! pane 级会话录制（TERM-015）。
//!
//! 与 `PaneHandle::contents()`（保存**当前**回滚 + 视口的文本快照）不同，
//! 录制是在 pane 生命周期内**持续**累积 PTY 原始输出字节：开始 → 累计 →
//! 停止 → 导出。录制状态（是否录制、字节数、起止时间）全部在 core，UI/插件
//! 只通过 [`crate::PaneHandle`] 的 `*_recording` 方法调用，不自己攒 buffer。
//!
//! 设计约束：
//! - actor 热路径只做一次 `Mutex` + `Vec::extend_from_slice`；录制关闭时
//!   只读一个 bool，零分配；
//! - 内存有硬上限（[`DEFAULT_RECORDING_CAPACITY_BYTES`]），超限只丢**新**
//!   字节并计数，绝不 OOM，也不把半截 chunk 写坏（只追加放得下的前缀）；
//! - 导出拿快照后释放锁再写文件，不阻塞 actor；
//! - 停止后保留 buffer，允许“先停止、后导出”；再次开始会清空上一段录制。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

/// 单 pane 录制缓冲上限：64 MiB ≈ 数小时普通 shell 输出；反复录大文件时
/// 也不会把进程 RSS 顶爆。超过后新字节丢弃并累计 `dropped_bytes`。
pub const DEFAULT_RECORDING_CAPACITY_BYTES: usize = 64 * 1024 * 1024;

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 录制状态快照（UI/MCP/插件只读）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordingStatus {
    /// 当前是否在录制。
    pub active: bool,
    /// 已录制的原始字节数。
    pub bytes: usize,
    /// 缓冲区上限（字节）。
    pub capacity: usize,
    /// 因超限被丢弃的字节数（0 = 完整录制）。
    pub dropped_bytes: u64,
    /// 收到的输出块数（含空块不计）。
    pub chunks: u64,
    /// 本次开始录制的 epoch 秒；从未开始为 None。
    pub started_at: Option<u64>,
    /// 最近一次停止的 epoch 秒；未停止为 None。
    pub stopped_at: Option<u64>,
}

impl RecordingStatus {
    /// 已录制时长（秒）：录制中取当前时间，停止后取停止时间。
    pub fn duration_secs(&self) -> Option<u64> {
        let start = self.started_at?;
        let end = if self.active {
            now_epoch_secs()
        } else {
            self.stopped_at?
        };
        Some(end.saturating_sub(start))
    }
}

/// 导出结果：写入的绝对/原样路径 + 当时快照的字节数 + 导出时是否仍在录制。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingExport {
    pub path: PathBuf,
    pub bytes: usize,
    pub active: bool,
}

#[derive(Debug)]
struct Inner {
    active: bool,
    bytes: Vec<u8>,
    capacity: usize,
    dropped_bytes: u64,
    chunks: u64,
    started_at: Option<u64>,
    stopped_at: Option<u64>,
}

impl Inner {
    fn status(&self) -> RecordingStatus {
        RecordingStatus {
            active: self.active,
            bytes: self.bytes.len(),
            capacity: self.capacity,
            dropped_bytes: self.dropped_bytes,
            chunks: self.chunks,
            started_at: self.started_at,
            stopped_at: self.stopped_at,
        }
    }
}

/// 单 pane 的录制器；由 [`crate::PaneHandle`] 持有，actor 与 UI 共享。
///
/// 用 `Arc` 是为了让 actor 线程写、UI 线程读；`Recorder` 本身不暴露可变
/// 引用，所有操作经内部 `Mutex`，没有 `&mut self` 要求。
#[derive(Debug)]
pub struct Recorder {
    inner: Mutex<Inner>,
    /// 无锁快路径：未录制时 `record` 只读这个 bool，不碰 `Mutex`。
    /// 与 `Inner.active` 的写入顺序见 `start`/`stop`：两个标志短暂不一致时
    /// `record` 的锁内二次检查会丢弃或照常追加，不会写坏 buffer。
    recording: AtomicBool,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new(DEFAULT_RECORDING_CAPACITY_BYTES)
    }
}

impl Recorder {
    /// 新建录制器；`capacity == 0` 视为 1，避免录制永远空转。
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                active: false,
                bytes: Vec::new(),
                capacity: capacity.max(1),
                dropped_bytes: 0,
                chunks: 0,
                started_at: None,
                stopped_at: None,
            }),
            recording: AtomicBool::new(false),
        }
    }

    /// 开始录制：清空上一段 buffer（含停止后保留的内容），状态置为 active。
    pub fn start(&self) -> RecordingStatus {
        let mut inner = self.inner.lock();
        inner.bytes.clear();
        inner.active = true;
        inner.dropped_bytes = 0;
        inner.chunks = 0;
        inner.started_at = Some(now_epoch_secs());
        inner.stopped_at = None;
        self.recording.store(true, Ordering::Relaxed);
        inner.status()
    }

    /// 停止录制；保留已录 buffer 供导出。
    pub fn stop(&self) -> RecordingStatus {
        let mut inner = self.inner.lock();
        if inner.active {
            inner.active = false;
            inner.stopped_at = Some(now_epoch_secs());
        }
        self.recording.store(false, Ordering::Relaxed);
        inner.status()
    }

    /// 清空 buffer 并回到未录制状态（等价于丢弃上一段录制）。
    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.bytes.clear();
        inner.active = false;
        inner.dropped_bytes = 0;
        inner.chunks = 0;
        inner.started_at = None;
        inner.stopped_at = None;
        self.recording.store(false, Ordering::Relaxed);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    /// 只读状态快照。
    pub fn status(&self) -> RecordingStatus {
        self.inner.lock().status()
    }

    /// actor 热路径：只有 active 时复制字节。
    ///
    /// 超限时只追加放得下的前缀（buffer 始终是 PTY 字节流的连续前缀），
    /// 剩余部分计入 `dropped_bytes`，不会产生“半截转义序列”之外的错位。
    pub fn record(&self, bytes: &[u8]) {
        // 关闭录制的常见路径：一次 relaxed load，不加锁、不分配。
        if bytes.is_empty() || !self.recording.load(Ordering::Relaxed) {
            return;
        }
        let mut inner = self.inner.lock();
        if !inner.active {
            return;
        }
        inner.chunks += 1;
        let room = inner.capacity.saturating_sub(inner.bytes.len());
        let take = room.min(bytes.len());
        if take > 0 {
            inner.bytes.extend_from_slice(&bytes[..take]);
        }
        if take < bytes.len() {
            inner.dropped_bytes += (bytes.len() - take) as u64;
        }
    }

    /// 当前 buffer 的克隆（导出/测试/MCP 上报）。
    pub fn snapshot(&self) -> Vec<u8> {
        self.inner.lock().bytes.clone()
    }

    /// 导出当前快照到 `path`（父目录必须已存在）。
    ///
    /// 录制中也可导出（快照语义，不改变录制状态）；锁只在克隆时持有。
    pub fn export(&self, path: &Path) -> std::io::Result<RecordingExport> {
        let (bytes, active) = {
            let inner = self.inner.lock();
            (inner.bytes.clone(), inner.active)
        };
        // 先写同目录唯一临时文件再 rename：目标已存在时，写失败不能把旧录制
        // 截断；with_extension 可能让 temp 与目标同名（目标本身以 .tmp 结尾）。
        let (tmp, mut file) = loop {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let seq = EXPORT_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp =
                path.with_file_name(format!(".{name}.{}.{seq}.export.tmp", std::process::id()));
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&tmp) {
                Ok(file) => break (tmp, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        if let Err(e) = file.write_all(&bytes).and_then(|()| file.flush()) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        drop(file);
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(RecordingExport {
            path: path.to_path_buf(),
            bytes: bytes.len(),
            active,
        })
    }
}

/// 共享录制器句柄（[`crate::PaneHandle`] 内部使用）。
pub type RecorderHandle = Arc<Recorder>;

static EXPORT_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_record_stop_export_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "xtervo-recording-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.raw");
        let recorder = Recorder::new(1024);

        // 未开始时 record 必须丢弃，导出为空文件。
        recorder.record(b"ignored");
        assert_eq!(recorder.status().bytes, 0);
        assert!(!recorder.status().active);

        let started = recorder.start();
        assert!(started.active);
        assert_eq!(started.bytes, 0);
        assert!(started.started_at.is_some());

        recorder.record(b"hello ");
        recorder.record(b"\x1b[31mred\x1b[0m\r\n");
        let status = recorder.status();
        assert_eq!(status.bytes, b"hello \x1b[31mred\x1b[0m\r\n".len());
        assert_eq!(status.chunks, 2);

        let stopped = recorder.stop();
        assert!(!stopped.active);
        assert_eq!(stopped.bytes, status.bytes);
        assert!(stopped.stopped_at.is_some());

        let exported = recorder.export(&path).expect("export");
        assert_eq!(exported.bytes, status.bytes);
        assert!(!exported.active);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"hello \x1b[31mred\x1b[0m\r\n"
        );

        // 再次开始清空上一段录制；导出可发生在录制中（快照）。
        recorder.start();
        recorder.record(b"next");
        assert_eq!(recorder.status().chunks, 1);
        let exported = recorder.export(&path).expect("export while recording");
        assert!(exported.active);
        assert_eq!(std::fs::read(&path).unwrap(), b"next");
        recorder.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capacity_drops_new_bytes_but_keeps_prefix() {
        let recorder = Recorder::new(4);
        recorder.start();
        recorder.record(b"abcd");
        recorder.record(b"ef");
        recorder.record(b"gh");
        let status = recorder.status();
        assert_eq!(status.bytes, 4);
        assert_eq!(status.dropped_bytes, 4);
        assert_eq!(recorder.snapshot(), b"abcd");
    }
}

#[cfg(unix)]
#[test]
fn export_preserves_private_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "xtervo-recording-mode-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.raw");
    std::fs::write(&path, b"old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let recorder = Recorder::new(1024);
    recorder.start();
    recorder.record(b"new");
    recorder.export(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "export must not widen permissions, got {mode:o}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
