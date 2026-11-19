// SPDX-License-Identifier: GPL-3.0-only
//! 传输层 seam（ACT-052 F1/F2 已接线）。
//!
//! 现在每个 pane 的后端是“PTY 里跑一个本地进程”；要把 SSH / Serial / Telnet
//! 做成官方可卸载插件，必须先让 pane 能被一条**任意字节通道**驱动：
//!
//! ```text
//! PTY（默认，零行为变化）  ─┐
//!                            ├─► PaneTransport ─► term-actor（vte 解析）► Frame
//! 插件提供的字节通道（Backend::Plugin）─┘
//! ```
//!
//! `PaneTransport` 是 pane 的后端抽象：`Backend::Pty` 仍走原来的 PTY 路径，
//! `Backend::Plugin` 用本模块的 trait 驱动同一个 `pump_reader` / writer backlog /
//! term-actor。后续顺序见 `docs/PLUGIN_TRANSPORT.md`：seam → 权限/审计/限流 → ssh 插件。
//!
//! 约定：
//! - `read` 允许短读，返回 `0` 表示对端关闭（pump_reader 据此发 `Ctrl::PtyClosed`）；
//! - `write_all` 必须要么写完、要么返回错误，不允许静默丢字节；
//! - `into_halves` 必须拆出独立的读/写/控制半边，避免读阻塞挡住写；
//! - `resize` 不阻塞，实现方自行决定是否需要发协议消息；
//! - `close` 幂等。

use std::io;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// pane 的字节后端。UI/actor 侧只依赖这个 trait，不关心背后是 PTY 还是插件。
pub trait PaneTransport: Send {
    /// 读一段字节；返回 `Ok(0)` 表示通道关闭。
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// 写入全部字节（背压由实现方负责，不能丢）。
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;
    /// 终端网格尺寸变化（列/行）。
    fn resize(&mut self, cols: u16, rows: u16);
    /// 关闭通道；必须幂等。
    fn close(&mut self);

    /// 拆成读/写/控制三个半边。
    ///
    /// pane 的读线程、写线程、actor 各持一份：读线程阻塞在 `read` 时不能把
    /// 写也一起挡住，所以这里必须真的拆开。`SocketPairTransport`/`SplitTransport`
    /// 分别用 `try_clone`/独立读写端实现；控制半边只负责 resize/close。
    fn into_halves(self: Box<Self>) -> io::Result<TransportHalves>;
}

/// transport 的控制半边：actor 线程用它下发 resize/close，不参与数据读写。
pub trait TransportControl: Send {
    fn resize(&mut self, cols: u16, rows: u16);
    fn close(&mut self);
}

/// 一条 transport 拆开后的三个半边。
pub struct TransportHalves {
    pub reader: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    pub control: Box<dyn TransportControl + Send>,
}

/// POC 用参考实现：一条 `socketpair` 的一端。
///
/// 真实插件传输会由宿主代管 socket，并把另一端的 fd 交给插件进程；这里用它
/// 验证“trait 能不能跑通读写 + 尺寸 + 幂等关闭”，并作为 e2e 的测试替身。
pub struct SocketPairTransport {
    stream: UnixStream,
    size: Arc<AtomicBool>,
    /// 打包的最近一次 resize：high 16 = cols，low 16 = rows。
    last_resize: Arc<AtomicU32>,
}

impl SocketPairTransport {
    /// 创建一对互相连接的端点：`(pane 侧, 对端模拟插件侧)`。
    pub fn pair() -> io::Result<(Self, UnixStream)> {
        let (ours, theirs) = UnixStream::pair()?;
        Ok((
            Self {
                stream: ours,
                size: Arc::new(AtomicBool::new(false)),
                last_resize: Arc::new(AtomicU32::new(0)),
            },
            theirs,
        ))
    }

    /// 最近一次 `resize` 的 (cols, rows)；测试与 HUD 用。
    pub fn last_resize(&self) -> (u16, u16) {
        unpack_resize(self.last_resize.load(Ordering::Relaxed))
    }

    /// 是否收到过 resize（用于断言“协议真的下发到后端”）。
    pub fn saw_resize(&self) -> bool {
        self.size.load(Ordering::Relaxed)
    }
}

#[inline]
fn pack_resize(cols: u16, rows: u16) -> u32 {
    (u32::from(cols) << 16) | u32::from(rows)
}

#[inline]
fn unpack_resize(bits: u32) -> (u16, u16) {
    ((bits >> 16) as u16, bits as u16)
}

/// `SocketPairTransport` 拆开后的控制半边：resize 记到共享槽，close 关 socket。
struct SocketPairControl {
    stream: UnixStream,
    size: Arc<AtomicBool>,
    last_resize: Arc<AtomicU32>,
}

impl TransportControl for SocketPairControl {
    fn resize(&mut self, cols: u16, rows: u16) {
        self.last_resize.store(pack_resize(cols, rows), Ordering::Relaxed);
        self.size.store(true, Ordering::Relaxed);
    }

    fn close(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl PaneTransport for SocketPairTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use std::io::Read;
        self.stream.read(buf)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.stream.write_all(data)
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.last_resize.store(pack_resize(cols, rows), Ordering::Relaxed);
        self.size.store(true, Ordering::Relaxed);
    }

    fn close(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    fn into_halves(self: Box<Self>) -> io::Result<TransportHalves> {
        // 读半边与写半边各持一个 fd；控制半边再持一个用于 shutdown。
        let SocketPairTransport { stream, size, last_resize } = *self;
        let reader = stream.try_clone()?;
        let control_stream = stream.try_clone()?;
        Ok(TransportHalves {
            reader: Box::new(reader),
            writer: Box::new(stream),
            control: Box::new(SocketPairControl { stream: control_stream, size, last_resize }),
        })
    }
}

/// 关闭底层 fd 的回调；`SplitTransport::close` 用它唤醒阻塞中的 `read`。
type ShutdownFn = Arc<dyn Fn() + Send + Sync>;

/// 读写分离的实现：PTY 的读端/写端本来就是两条线程各持一个句柄，
/// 插件 socket 也常常是「一读一写」。泛型化后 PTY 与插件可以共用同一条 trait。
pub struct SplitTransport<R, W> {
    reader: R,
    writer: W,
    /// 打包的 (cols, rows)。
    size: Arc<AtomicU32>,
    closed: Arc<AtomicBool>,
    /// `close()` 时调用；不注册时只能置标志，无法唤醒已阻塞的读。
    shutdown: Option<ShutdownFn>,
}

impl<R, W> SplitTransport<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            size: Arc::new(AtomicU32::new(0)),
            closed: Arc::new(AtomicBool::new(false)),
            shutdown: None,
        }
    }

    /// 注册关闭句柄：`close()` 会调用它（典型实现：对底层 socket 做
    /// `shutdown(Both)`）。没有它时 `close()` 只能置 `closed` 标志，
    /// 已经阻塞在 `read` 上的线程不会被唤醒。
    pub fn with_shutdown(mut self, shutdown: impl Fn() + Send + Sync + 'static) -> Self {
        self.shutdown = Some(Arc::new(shutdown));
        self
    }

    /// 最近一次 `resize` 的 (cols, rows)。
    pub fn size(&self) -> (u16, u16) {
        unpack_resize(self.size.load(Ordering::Relaxed))
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn close_inner(&self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(shutdown) = &self.shutdown {
            shutdown();
        }
    }
}

/// `SplitTransport` 拆开后的控制半边。
struct SplitControl {
    size: Arc<AtomicU32>,
    closed: Arc<AtomicBool>,
    shutdown: Option<ShutdownFn>,
}

impl TransportControl for SplitControl {
    fn resize(&mut self, cols: u16, rows: u16) {
        self.size.store(pack_resize(cols, rows), Ordering::Relaxed);
    }

    fn close(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(shutdown) = &self.shutdown {
            shutdown();
        }
    }
}

impl<R: std::io::Read + Send + 'static, W: std::io::Write + Send + 'static> PaneTransport
    for SplitTransport<R, W>
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.size.store(pack_resize(cols, rows), Ordering::Relaxed);
    }

    fn close(&mut self) {
        self.close_inner();
    }

    fn into_halves(self: Box<Self>) -> io::Result<TransportHalves> {
        let SplitTransport { reader, writer, size, closed, shutdown } = *self;
        Ok(TransportHalves {
            reader: Box::new(reader),
            writer: Box::new(writer),
            control: Box::new(SplitControl { size, closed, shutdown }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    #[test]
    fn socketpair_transport_roundtrips_bytes_and_tracks_resize() {
        let (mut pane, mut plugin) = SocketPairTransport::pair().unwrap();
        assert!(!pane.saw_resize());

        plugin.write_all(b"hello\x1b[31m").unwrap();
        let mut buf = [0u8; 32];
        let n = pane.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello\x1b[31m");

        pane.write_all(b"\x03").unwrap();
        let mut back = [0u8; 8];
        let n = plugin.read(&mut back).unwrap();
        assert_eq!(&back[..n], b"\x03");

        pane.resize(120, 40);
        assert!(pane.saw_resize());
        assert_eq!(pane.last_resize(), (120, 40));

        // 关闭是幂等的，且关闭后对端读到 EOF
        pane.close();
        pane.close();
        let mut tail = [0u8; 8];
        plugin
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        assert_eq!(plugin.read(&mut tail).unwrap_or(0), 0);
    }

    #[test]
    fn split_transport_works_with_independent_read_and_write_halves() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut t = SplitTransport::new(ours.try_clone().unwrap(), ours);
        assert!(!t.is_closed());
        assert_eq!(t.size(), (0, 0));

        // 下游写 -> pane 读
        (&theirs).write_all(b"out\n").unwrap();
        let mut buf = [0u8; 16];
        let n = t.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"out\n");

        // pane 写 -> 下游读
        t.write_all(b"in").unwrap();
        let mut back = [0u8; 8];
        let n = (&theirs).read(&mut back).unwrap();
        assert_eq!(&back[..n], b"in");

        t.resize(80, 24);
        assert_eq!(t.size(), (80, 24));
        t.close();
        assert!(t.is_closed());
    }

    #[test]
    fn split_control_close_wakes_blocked_read() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let wake = ours.try_clone().unwrap();
        let t = SplitTransport::new(ours.try_clone().unwrap(), ours)
            .with_shutdown(move || {
                let _ = wake.shutdown(std::net::Shutdown::Both);
            });
        let TransportHalves { reader, writer, mut control } = Box::new(t).into_halves().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 16];
            let _ = tx.send(reader.read(&mut buf));
        });

        // 读线程应已阻塞在 read 上；此时只剩 control.close() 能唤醒它。
        std::thread::sleep(Duration::from_millis(100));
        control.close();
        let result = rx.recv_timeout(Duration::from_secs(1));
        // 保持 writer / 对端存活，排除“关 fd 导致的 EOF”这种假通过。
        drop(writer);
        drop(theirs);
        match result {
            Ok(Ok(0)) | Ok(Err(_)) => {},
            Ok(Ok(n)) => panic!("阻塞读意外返回 {n} 字节"),
            Err(e) => panic!("control.close() 没有唤醒阻塞中的 read（1s 超时）: {e}"),
        }
        reader_thread.join().expect("reader thread");
    }

    #[test]
    fn read_returns_zero_at_eof_so_actor_can_finish() {
        let (mut pane, plugin) = SocketPairTransport::pair().unwrap();
        drop(plugin);
        let mut buf = [0u8; 8];
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match pane.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "EOF 应在超时前到达");
                    continue;
                },
                Err(e) => panic!("unexpected read error: {e}"),
            }
        }
    }
}
