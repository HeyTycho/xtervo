// SPDX-License-Identifier: GPL-3.0-only
//! 传输层 seam（ACT-052 POC，**尚未接线**）。
//!
//! 现在每个 pane 的后端是“PTY 里跑一个本地进程”；要把 SSH / Serial / Telnet
//! 做成官方可卸载插件，必须先让 pane 能被一条**任意字节通道**驱动：
//!
//! ```text
//! PTY（默认，零行为变化）  ─┐
//!                            ├─► PaneTransport ─► term-actor（vte 解析）► Frame
//! 插件提供的字节通道（未来）─┘
//! ```
//!
//! 本模块只定义 trait 和一个基于 `socketpair` 的参考实现，**不改任何现有路径**；
//! 后续接线顺序见 `docs/PLUGIN_TRANSPORT.md`：POC → 权限/审计/限流 → ssh 插件。
//!
//! 约定：
//! - `read` 允许短读，返回 `0` 表示对端关闭（actor 据此收尾）；
//! - `write_all` 必须要么写完、要么返回错误，不允许静默丢字节；
//! - `resize` 不阻塞，实现方自行决定是否需要发协议消息；
//! - `close` 幂等。

use std::io;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
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
}

/// POC 用参考实现：一条 `socketpair` 的一端。
///
/// 真实插件传输会由宿主代管 socket，并把另一端的 fd 交给插件进程；这里用它
/// 验证“trait 能不能跑通读写 + 尺寸 + 幂等关闭”，并作为 e2e 的测试替身。
pub struct SocketPairTransport {
    stream: UnixStream,
    size: Arc<AtomicBool>,
    last_resize: (u16, u16),
}

impl SocketPairTransport {
    /// 创建一对互相连接的端点：`(pane 侧, 对端模拟插件侧)`。
    pub fn pair() -> io::Result<(Self, UnixStream)> {
        let (ours, theirs) = UnixStream::pair()?;
        Ok((
            Self { stream: ours, size: Arc::new(AtomicBool::new(false)), last_resize: (0, 0) },
            theirs,
        ))
    }

    /// 最近一次 `resize` 的 (cols, rows)；测试与 HUD 用。
    pub fn last_resize(&self) -> (u16, u16) {
        self.last_resize
    }

    /// 是否收到过 resize（用于断言“协议真的下发到后端”）。
    pub fn saw_resize(&self) -> bool {
        self.size.load(Ordering::Relaxed)
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
        self.last_resize = (cols, rows);
        self.size.store(true, Ordering::Relaxed);
    }

    fn close(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
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
