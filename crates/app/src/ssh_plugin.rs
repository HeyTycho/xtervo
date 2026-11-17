// SPDX-License-Identifier: GPL-3.0-only
//! 官方 SSH 连接插件 POC（ACT-052 F5）：把系统 `ssh` 客户端包成 `PaneTransport`。
//!
//! 目的不是再做一个“PTY 里跑 ssh”的连接：pane 的字节后端直接是
//! [`tervo_core::transport::PaneTransport`]（`Backend::Plugin`），核心的 vte
//! 解析、写队列、EOF/背压路径与本地 PTY 完全共用。
//!
//! ```text
//! ssh 子进程 stdout ──读线程──► [u32 len][payload] 帧 ──► FrameReader ──► pane
//! ssh 子进程 stdin  ◄──PaneTransport::write_all─────────────── 终端按键
//! ```
//!
//! 数据面复用 [`crate::transport_io`] 的帧协议（256 KiB 上限、长度必须精确
//! 匹配）；子进程 stderr 由独立线程收集，连接失败/超时时作为一条红色文本
//! 送进 pane 再 EOF，所以“连不上”在终端里可见，而不是只打日志。
//!
//! POC 边界：
//! - 认证只支持 agent / 私钥（`BatchMode=yes`）；`auth=password` 直接拒绝，
//!   避免 ssh 在无 tty 的管道里静默等待密码；
//! - 远端 PTY 用 `ssh -tt` 强制分配；窗口 resize 目前只记录、不转发
//!   （管道没有 winsize，OpenSSH 也不会对非 tty stdin 发 window-change）；
//! - 不做 Connection Manager / 重连 UI：pane 关闭 = transport close = kill ssh。

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tervo_core::transport::{PaneTransport, TransportControl, TransportHalves};

use crate::profile::{Profile, ProfileStore};
use crate::transport_io::{decode_frame, encode_frame, MAX_FRAME_PAYLOAD};

/// 默认“多久没有任何远端输出就判定连接超时”。ssh 自己的 ConnectTimeout 更短，
/// 这里兜底 DNS/ProxyJump/认证阶段卡住的情况。
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// 单帧 payload（读 ssh stdout 的块大小）；远小于协议 256 KiB 上限。
const READ_CHUNK: usize = 32 * 1024;

/// stderr 只保留尾部这么多字节；够看清 ssh 的错误，又不至于把 pane 灌满。
const STDERR_TAIL: usize = 8 * 1024;

/// 连接阶段（可观测状态，测试/日志用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 子进程已启动，还没收到任何远端字节。
    Connecting,
    /// 已收到远端输出（banner/提示符等）。
    Connected,
    /// 正常关闭（远端退出或用户关 pane）。
    Closed,
    /// 连接超时（watchdog 杀掉 ssh）。
    TimedOut,
    /// 启动/认证/网络失败（stderr 已送进 pane）。
    Failed,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Closed => "closed",
            Self::TimedOut => "timeout",
            Self::Failed => "failed",
        }
    }
}

/// `SshPluginTransport` 的可观测状态快照。
#[derive(Clone, Debug)]
pub struct SshConnectionStatus {
    pub phase: Phase,
    /// 简短原因；不含 payload、密码或密钥内容。
    pub detail: String,
}

impl SshConnectionStatus {
    fn new() -> Self {
        Self { phase: Phase::Connecting, detail: String::new() }
    }
}

/// pump 线程、watchdog、控制半边共享的状态。
struct Shared {
    status: Mutex<SshConnectionStatus>,
    /// 用户主动关闭：EOF 时不再报“连接失败”。
    closed: AtomicBool,
    /// 是否已收到远端输出（watchdog 据此取消超时）。
    got_output: AtomicBool,
    /// 是否已经发送过“终止性”错误文本（超时 vs EOF 只报一次）。
    notified: AtomicBool,
    /// 帧写锁：pump 与 watchdog 都可能写同一个 socket，帧不能交错。
    write_lock: Mutex<()>,
}

impl Shared {
    fn new() -> Self {
        Self {
            status: Mutex::new(SshConnectionStatus::new()),
            closed: AtomicBool::new(false),
            got_output: AtomicBool::new(false),
            notified: AtomicBool::new(false),
            write_lock: Mutex::new(()),
        }
    }

    fn set(&self, phase: Phase, detail: impl Into<String>) {
        if let Ok(mut st) = self.status.lock() {
            st.phase = phase;
            st.detail = detail.into();
        }
    }

    fn snapshot(&self) -> SshConnectionStatus {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(SshConnectionStatus { phase: Phase::Failed, detail: "status poisoned".into() })
    }

    /// 只有第一个调用者能拿到错误上报权（避免超时 + EOF 重复打印）。
    fn claim_error(&self) -> bool {
        !self.notified.swap(true, Ordering::SeqCst)
    }

    /// 用户主动关闭：状态只写一次，不覆盖已经上报的 timeout/failed。
    fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if !self.notified.swap(true, Ordering::SeqCst) {
            self.set(Phase::Closed, "closed by user");
        }
    }

    fn send(&self, sink: &mut UnixStream, text: &str) -> io::Result<()> {
        self.send_bytes(sink, text.as_bytes())
    }

    fn send_bytes(&self, sink: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
        let frame = encode_frame(payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let _guard = self.write_lock.lock().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "transport write lock poisoned")
        })?;
        sink.write_all(&frame)
    }
}

/// 系统 `ssh` 子进程 + 帧化 stdout 的 pane 后端。
pub struct SshPluginTransport {
    reader: FrameReader,
    stdin: ChildStdin,
    child: Arc<Mutex<Child>>,
    shared: Arc<Shared>,
    /// 最近一次 resize（POC 只记录，不转发；见模块注释）。
    resize: Arc<AtomicU32>,
}

impl SshPluginTransport {
    /// 用 Profile 启动 SSH transport：默认超时 20s，可用
    /// `TERVO_PLUGIN_SSH_CONNECT_TIMEOUT_MS` 覆盖。
    pub fn start(profile: &Profile) -> io::Result<Self> {
        Self::start_with_timeout(profile, configured_timeout())
    }

    /// 可注入超时的版本（测试用；避免进程级 env 在并行测试里互相干扰）。
    pub fn start_with_timeout(profile: &Profile, timeout: Duration) -> io::Result<Self> {
        if profile.kind != "ssh" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("plugin-ssh 只支持 ssh Profile，实际 kind={}", profile.kind),
            ));
        }
        if profile.host.trim().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "SSH 主机不能为空"));
        }
        if profile.auth == "password" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin-ssh POC 暂不支持密码认证：请改用 agent 或私钥（BatchMode=yes）",
            ));
        }

        let label = remote_label(profile);
        let (program, args) = plugin_ssh_command(profile);
        eprintln!(
            "[plugin-ssh] spawn program={program} host={} auth={} timeout={}ms",
            label,
            profile.auth,
            timeout.as_millis()
        );
        let mut command = Command::new(&program);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 远端 shell 的 TERM 继承宿主；宿主没有时给 xterm-256color，
        // 否则远端 vi/top 之类会退化成 dumb。
        if std::env::var_os("TERM").is_none() {
            command.env("TERM", "xterm-256color");
        }
        let mut child = command.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("启动 {program} 失败（plugin-ssh）: {e}"),
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ssh stdin 不可用")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ssh stdout 不可用")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ssh stderr 不可用")
        })?;
        let child = Arc::new(Mutex::new(child));

        let (tx, rx) = UnixStream::pair()?;
        let mut sink = tx.try_clone()?;
        let shared = Arc::new(Shared::new());
        // 先给用户一个“正在连接”的反馈；错的时候后面还有红色错误文本。
        let _ = shared.send(&mut sink, &connecting_message(&label));

        let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr_thread = spawn_stderr(stderr, stderr_buf.clone());
        spawn_pump(
            stdout,
            sink,
            child.clone(),
            shared.clone(),
            stderr_buf,
            stderr_thread,
            label.clone(),
        );
        spawn_watchdog(
            tx,
            child.clone(),
            shared.clone(),
            timeout,
            label,
        );

        Ok(Self {
            reader: FrameReader::new(rx),
            stdin,
            child,
            shared,
            resize: Arc::new(AtomicU32::new(0)),
        })
    }

    /// 当前连接状态快照。
    pub fn status(&self) -> SshConnectionStatus {
        self.shared.snapshot()
    }

    /// 测试用：最近一次 resize 是否真的到达后端（POC 只记录，不转发）。
    #[cfg(test)]
    pub fn last_resize(&self) -> (u16, u16) {
        let bits = self.resize.load(Ordering::Relaxed);
        ((bits >> 16) as u16, bits as u16)
    }

    /// 读超时（probe 用；pane 路径保持阻塞读）。
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.reader.stream.set_read_timeout(timeout)
    }

    fn close_inner(&self) {
        self.shared.mark_closed();
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.reader.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl PaneTransport for SshPluginTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.stdin.write_all(data)?;
        self.stdin.flush()
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.resize.store(pack_resize(cols, rows), Ordering::Relaxed);
    }

    fn close(&mut self) {
        self.close_inner();
    }

    fn into_halves(self: Box<Self>) -> io::Result<TransportHalves> {
        let SshPluginTransport { reader, stdin, child, shared, resize } = *self;
        let control_socket = reader.stream.try_clone()?;
        Ok(TransportHalves {
            reader: Box::new(reader),
            writer: Box::new(stdin),
            control: Box::new(SshControl { child, socket: control_socket, shared, resize }),
        })
    }
}

/// transport 的控制半边：resize 记录 + close 杀 ssh。
struct SshControl {
    child: Arc<Mutex<Child>>,
    socket: UnixStream,
    shared: Arc<Shared>,
    resize: Arc<AtomicU32>,
}

impl TransportControl for SshControl {
    fn resize(&mut self, cols: u16, rows: u16) {
        self.resize.store(pack_resize(cols, rows), Ordering::Relaxed);
    }

    fn close(&mut self) {
        self.shared.mark_closed();
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

/// 系统 ssh 命令。`profile.args` 若已由 `ssh_command()` 生成就直接复用，否则
/// 现场构造；我们的 POC 选项插在 host 之前（ssh 只接受 host 之后的远端命令）。
pub fn plugin_ssh_command(profile: &Profile) -> (String, Vec<String>) {
    let (program, mut args) = if !profile.shell.trim().is_empty() && profile.shell != "ssh" {
        // 测试/自定义后端：shell 脚本自己忽略这些参数。
        (profile.shell.trim().to_string(), profile.args.clone())
    } else if !profile.args.is_empty() {
        ("ssh".to_string(), profile.args.clone())
    } else {
        profile.ssh_command()
    };
    let options = vec![
        "-tt".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", connect_timeout_secs()),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
    ];
    let host_last = args.last().is_some_and(|a| a == profile.host.trim());
    let at = if host_last { args.len() - 1 } else { args.len() };
    args.splice(at..at, options);
    (program, args)
}

fn connect_timeout_secs() -> u64 {
    configured_timeout().as_secs().max(1)
}

fn configured_timeout() -> Duration {
    std::env::var("TERVO_PLUGIN_SSH_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
}

fn remote_label(profile: &Profile) -> String {
    let user = profile.user.trim();
    let host = profile.host.trim();
    if user.is_empty() {
        format!("{host}:{}", profile.port)
    } else {
        format!("{user}@{host}:{}", profile.port)
    }
}

fn connecting_message(label: &str) -> String {
    format!("\x1b[2m[plugin-ssh] 正在连接 {label} …\x1b[0m\r\n")
}

fn closed_message(detail: &str) -> String {
    format!("\r\n\x1b[2m[plugin-ssh] 连接已关闭（{detail}）\x1b[0m\r\n")
}

fn timeout_message(label: &str, timeout: Duration) -> String {
    format!(
        "\r\n\x1b[1;33m[plugin-ssh] 连接超时：{label} 在 {}s 内没有任何远端输出；已断开。\x1b[0m\r\n",
        timeout.as_secs_f32()
    )
}

fn failure_message(label: &str, reason: &str, stderr: &str) -> String {
    let mut text = format!(
        "\r\n\x1b[1;31m[plugin-ssh] 连接失败：{label}（{reason}）\x1b[0m\r\n"
    );
    let tail = sanitize_stderr(stderr);
    if !tail.is_empty() {
        text.push_str("\x1b[31m");
        text.push_str(&tail);
        text.push_str("\x1b[0m\r\n");
    }
    text
}

/// stderr → 终端文本：统一 CRLF、去掉多余空行、限制行数。
fn sanitize_stderr(stderr: &str) -> String {
    stderr
        .replace('\r', "")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(12)
        .map(|line| format!("{line}\r\n"))
        .collect()
}

fn spawn_stderr(mut stderr: std::process::ChildStderr, buf: Arc<Mutex<Vec<u8>>>) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("plugin-ssh-err".into())
        .spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut out) = buf.lock() {
                            if out.len() < STDERR_TAIL {
                                let take = (STDERR_TAIL - out.len()).min(n);
                                out.extend_from_slice(&chunk[..take]);
                            }
                        }
                    },
                    Err(_) => break,
                }
            }
        })
        .ok()
}

fn spawn_pump(
    mut stdout: std::process::ChildStdout,
    mut sink: UnixStream,
    child: Arc<Mutex<Child>>,
    shared: Arc<Shared>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
    label: String,
) {
    let _ = std::thread::Builder::new().name("plugin-ssh-out".into()).spawn(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut read_error: Option<io::Error> = None;
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if !shared.got_output.swap(true, Ordering::SeqCst) {
                        shared.set(Phase::Connected, "remote output received");
                    }
                    if shared.send_bytes(&mut sink, &buf[..n]).is_err() {
                        // 帧写失败说明 pane 侧已关闭；退出并杀掉 ssh。
                        read_error = Some(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "pane transport closed",
                        ));
                        break;
                    }
                },
                Err(e) => {
                    read_error = Some(e);
                    break;
                },
            }
        }

        let exit = wait_for_exit(&child, Duration::from_secs(2));
        if let Some(thread) = stderr_thread {
            let _ = thread.join();
        }
        let stderr = stderr_buf
            .lock()
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default();

        if shared.closed.load(Ordering::SeqCst) {
            shared.set(Phase::Closed, "closed by user");
            let _ = shared.send(&mut sink, &closed_message("用户关闭"));
            return;
        }
        // watchdog 已经发过超时文本：不重复。
        if shared.notified.load(Ordering::SeqCst) {
            return;
        }

        match (&exit, &read_error) {
            (Some(status), _) if status.success() => {
                shared.set(Phase::Closed, "remote closed");
                let _ = shared.send(&mut sink, &closed_message("远端关闭"));
            },
            _ => {
                let reason = failure_reason(exit.as_ref(), read_error.as_ref());
                if !shared.claim_error() {
                    return;
                }
                shared.set(Phase::Failed, reason.clone());
                eprintln!("[plugin-ssh] failed host={label} reason={reason}");
                let _ = shared.send(&mut sink, &failure_message(&label, &reason, &stderr));
            },
        }
        let _ = sink.shutdown(std::net::Shutdown::Write);
    });
}

fn failure_reason(exit: Option<&ExitStatus>, read_error: Option<&io::Error>) -> String {
    let mut reason = match exit {
        Some(status) => match status.code() {
            Some(code) => format!("ssh 退出码 {code}"),
            None => {
                use std::os::unix::process::ExitStatusExt;
                match status.signal() {
                    Some(sig) => format!("ssh 被信号 {sig} 终止"),
                    None => "ssh 异常退出".to_string(),
                }
            },
        },
        None => "ssh 未在 2s 内退出（已 kill）".to_string(),
    };
    if let Some(e) = read_error {
        reason.push_str(&format!("；读取远端输出失败: {e}"));
    }
    reason
}

fn spawn_watchdog(
    mut sink: UnixStream,
    child: Arc<Mutex<Child>>,
    shared: Arc<Shared>,
    timeout: Duration,
    label: String,
) {
    let _ = std::thread::Builder::new().name("plugin-ssh-watchdog".into()).spawn(move || {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if shared.closed.load(Ordering::SeqCst) || shared.got_output.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if shared.closed.load(Ordering::SeqCst) || shared.got_output.load(Ordering::SeqCst) {
            return;
        }
        if !shared.claim_error() {
            return;
        }
        shared.set(Phase::TimedOut, format!("{}s 无远端输出", timeout.as_secs_f32()));
        eprintln!("[plugin-ssh] timeout host={label} after={}ms", timeout.as_millis());
        let _ = shared.send(&mut sink, &timeout_message(&label, timeout));
        let _ = sink.shutdown(std::net::Shutdown::Write);
        if let Ok(mut child) = child.lock() {
            let _ = child.kill();
        }
    });
}

fn wait_for_exit(child: &Arc<Mutex<Child>>, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(mut child) = child.lock() {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {},
                Err(_) => return None,
            }
        }
        if Instant::now() >= deadline {
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
            }
            // kill 之后最多再等 500ms 收尸，避免留下僵尸进程。
            let grace = Instant::now() + Duration::from_millis(500);
            while Instant::now() < grace {
                if let Ok(mut child) = child.lock() {
                    if let Ok(Some(status)) = child.try_wait() {
                        return Some(status);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[inline]
fn pack_resize(cols: u16, rows: u16) -> u32 {
    (u32::from(cols) << 16) | u32::from(rows)
}

/// 从 Unix socket 读 `[u32 LE len][payload]` 帧的读半边。
///
/// `read` 允许短读；非阻塞/超时读不会丢半帧（状态机保留已读的 header/payload）。
struct FrameReader {
    stream: UnixStream,
    header: [u8; 4],
    header_filled: usize,
    payload: Vec<u8>,
    payload_filled: usize,
    payload_read: usize,
    eof: bool,
}

impl FrameReader {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            header: [0u8; 4],
            header_filled: 0,
            payload: Vec::new(),
            payload_filled: 0,
            payload_read: 0,
            eof: false,
        }
    }

    fn fill_payload(&mut self) -> io::Result<()> {
        let len = self.payload.len();
        while self.payload_filled < len {
            match self.stream.read(&mut self.payload[self.payload_filled..]) {
                Ok(0) => {
                    self.eof = true;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "transport frame truncated",
                    ));
                },
                Ok(n) => self.payload_filled += n,
                Err(e) => return Err(e),
            }
        }
        // 用 transport_io 的协议校验（长度/上限），不自己另写一套规则。
        let mut frame = Vec::with_capacity(4 + len);
        frame.extend_from_slice(&self.header);
        frame.extend_from_slice(&self.payload);
        decode_frame(&frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.header_filled = 0;
        self.payload_read = 0;
        Ok(())
    }
}

impl Read for FrameReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            // 只在整帧 payload 都已读入后才交付；超时/短读留下的半帧不能当数据。
            if !self.payload.is_empty() && self.payload_filled == self.payload.len() {
                if self.payload_read < self.payload.len() {
                    let n = (self.payload.len() - self.payload_read).min(buf.len());
                    buf[..n]
                        .copy_from_slice(&self.payload[self.payload_read..self.payload_read + n]);
                    self.payload_read += n;
                    return Ok(n);
                }
                // 上一帧交付完：清空，继续读下一帧 header。
                self.payload.clear();
                self.payload_filled = 0;
            }
            if self.payload_filled < self.payload.len() {
                // 上一轮超时/短读留下的半帧：先补齐再交付。
                self.fill_payload()?;
                continue;
            }
            if self.eof {
                return Ok(0);
            }
            while self.header_filled < 4 {
                match self.stream.read(&mut self.header[self.header_filled..]) {
                    Ok(0) => {
                        self.eof = true;
                        if self.header_filled == 0 {
                            return Ok(0);
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "transport frame header truncated",
                        ));
                    },
                    Ok(n) => self.header_filled += n,
                    Err(e) => return Err(e),
                }
            }
            let len = u32::from_le_bytes(self.header) as usize;
            if len > MAX_FRAME_PAYLOAD {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("transport frame payload {len} > {MAX_FRAME_PAYLOAD} bytes"),
                ));
            }
            self.payload = vec![0u8; len];
            self.payload_filled = 0;
            self.payload_read = 0;
            if len == 0 {
                // 空帧合法但无数据：读下一帧。
                self.payload.clear();
                self.header_filled = 0;
                continue;
            }
            self.fill_payload()?;
        }
    }
}

/// `tervo --plugin-ssh-probe <profile-id|name> [--duration <secs>]`
///
/// 无 GUI 的复现工具：走与 pane 完全相同的 transport，把远端字节原样打到
/// stdout，最后打印状态。成功显示 banner 时退出码 0，失败/超时为 1。
pub fn run_probe(args: &[String]) -> i32 {
    let mut selector: Option<&str> = None;
    let mut duration = Duration::from_secs(8);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--duration" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                    duration = Duration::from_secs(v.clamp(1, 120));
                }
                i += 2;
                continue;
            },
            other if selector.is_none() => selector = Some(other),
            other => {
                eprintln!("[plugin-ssh-probe] 忽略未知参数: {other}");
            },
        }
        i += 1;
    }
    let Some(selector) = selector else {
        eprintln!("用法: tervo --plugin-ssh-probe <profile-id|name> [--duration <secs>]");
        return 2;
    };
    let store = ProfileStore::load();
    let Some(profile) = find_profile(&store, selector) else {
        eprintln!("[plugin-ssh-probe] 找不到 Profile: {selector}");
        return 2;
    };
    println!(
        "[plugin-ssh-probe] profile={} id={} host={} user={} auth={} transport={}",
        profile.name,
        profile.id,
        profile.host,
        profile.user,
        profile.auth,
        profile.transport
    );
    let mut transport = match SshPluginTransport::start(profile) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[plugin-ssh-probe] 启动失败: {e}");
            return 1;
        },
    };
    if let Err(e) = transport.set_read_timeout(Some(Duration::from_millis(200))) {
        eprintln!("[plugin-ssh-probe] set_read_timeout 失败: {e}");
        return 1;
    }
    let deadline = Instant::now() + duration;
    let mut bytes = 0usize;
    let mut buf = [0u8; 8192];
    let mut out = io::stdout();
    while Instant::now() < deadline {
        match transport.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes += n;
                if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                    break;
                }
            },
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue
            },
            Err(e) => {
                eprintln!("\n[plugin-ssh-probe] 读取失败: {e}");
                break;
            },
        }
    }
    let status = transport.status();
    transport.close();
    println!(
        "\n[plugin-ssh-probe] bytes={bytes} phase={} detail={}",
        status.phase.label(),
        status.detail
    );
    if bytes > 0 && status.phase == Phase::Connected {
        0
    } else {
        1
    }
}

fn find_profile<'a>(store: &'a ProfileStore, selector: &str) -> Option<&'a Profile> {
    let selector = selector.trim();
    if let Ok(id) = selector.parse::<u64>() {
        if let Some(p) = store.get(id) {
            return Some(p);
        }
    }
    store
        .profiles
        .iter()
        .find(|p| p.name == selector || p.name.eq_ignore_ascii_case(selector))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    fn ssh_profile() -> Profile {
        Profile {
            kind: "ssh".into(),
            host: "example.test".into(),
            port: 2222,
            user: "alice".into(),
            auth: "agent".into(),
            ..Profile::default()
        }
    }

    /// 临时假 ssh 脚本；返回 (路径, 清理用 TempPath 守卫)。
    struct FakeSsh(std::path::PathBuf);

    impl FakeSsh {
        fn new(body: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tervo-fake-ssh-{}-{}.sh",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            Self(path)
        }

        fn profile(&self) -> Profile {
            Profile {
                kind: "ssh".into(),
                host: "fake.test".into(),
                port: 22,
                user: "u".into(),
                auth: "agent".into(),
                shell: self.0.to_string_lossy().into_owned(),
                ..Profile::default()
            }
        }
    }

    impl Drop for FakeSsh {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn read_all(t: &mut SshPluginTransport) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match t.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => panic!("read failed: {e}"),
            }
        }
        out
    }

    #[test]
    fn ssh_args_force_batchmode_tt_and_keep_host_last() {
        let p = ssh_profile();
        let (program, args) = plugin_ssh_command(&p);
        assert_eq!(program, "ssh");
        assert_eq!(args.last().map(String::as_str), Some("example.test"));
        for needle in [
            "-tt",
            "BatchMode=yes",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
        ] {
            assert!(args.iter().any(|a| a == needle), "missing {needle}: {args:?}");
        }
        assert!(args.iter().any(|a| a.starts_with("ConnectTimeout=")));
        // -p/-l 仍在 host 之前，顺序不被 POC 选项破坏。
        let port = args.iter().position(|a| a == "-p").unwrap();
        assert!(port < args.len() - 1);
    }

    #[test]
    fn password_auth_is_rejected_before_spawning() {
        let mut p = ssh_profile();
        p.auth = "password".into();
        let err = SshPluginTransport::start_with_timeout(&p, Duration::from_millis(200))
            .err()
            .expect("password auth must be rejected");
        assert!(err.to_string().contains("密码认证"), "{err}");
    }

    #[test]
    fn fake_ssh_banner_reaches_pane_and_eof_closes_cleanly() {
        let fake = FakeSsh::new("printf 'Welcome to fake ssh\\r\\n'; sleep 0.1");
        let mut t = SshPluginTransport::start_with_timeout(
            &fake.profile(),
            Duration::from_secs(5),
        )
        .expect("transport should start");
        let bytes = read_all(&mut t);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("正在连接"), "missing connecting banner: {text:?}");
        assert!(text.contains("Welcome to fake ssh"), "missing remote banner: {text:?}");
        assert!(text.contains("连接已关闭"), "missing close notice: {text:?}");
        assert_eq!(t.status().phase, Phase::Closed);
    }

    #[test]
    fn failed_ssh_exit_and_stderr_are_visible_in_pane() {
        let fake = FakeSsh::new("echo 'Permission denied (publickey).' >&2; exit 255");
        let mut t = SshPluginTransport::start_with_timeout(
            &fake.profile(),
            Duration::from_secs(5),
        )
        .expect("transport should start");
        let bytes = read_all(&mut t);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("连接失败"), "missing failure banner: {text:?}");
        assert!(text.contains("255"), "missing exit code: {text:?}");
        assert!(text.contains("Permission denied"), "missing stderr: {text:?}");
        assert_eq!(t.status().phase, Phase::Failed);
    }

    #[test]
    fn silent_connection_times_out_and_reports_visible_error() {
        let fake = FakeSsh::new("exec sleep 30");
        let t = SshPluginTransport::start_with_timeout(
            &fake.profile(),
            Duration::from_millis(300),
        )
        .expect("transport should start");
        let status = std::thread::spawn(move || {
            let mut t = Box::new(t);
            let bytes = read_all(&mut t);
            let status = t.status();
            (String::from_utf8_lossy(&bytes).to_string(), status)
        });
        let (text, status) = status.join().expect("timeout thread");
        assert!(text.contains("连接超时"), "missing timeout banner: {text:?}");
        assert_eq!(status.phase, Phase::TimedOut, "detail={}", status.detail);
    }

    #[test]
    fn resize_reaches_transport_backend() {
        let fake = FakeSsh::new("exec sleep 30");
        let mut t = SshPluginTransport::start_with_timeout(
            &fake.profile(),
            Duration::from_secs(30),
        )
        .expect("transport should start");
        t.resize(120, 40);
        assert_eq!(t.last_resize(), (120, 40));
        t.close();
    }

    #[test]
    fn close_kills_ssh_and_is_idempotent() {
        let fake = FakeSsh::new("exec sleep 30");
        let mut t = SshPluginTransport::start_with_timeout(
            &fake.profile(),
            Duration::from_secs(30),
        )
        .expect("transport should start");
        t.close();
        t.close();
        // 关闭后读端应很快 EOF；不依赖 30s 的 ssh。
        let mut buf = [0u8; 64];
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(Instant::now() < deadline, "close 后读端没有 EOF");
            match t.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) => panic!("unexpected read error after close: {e}"),
            }
        }
        assert_eq!(t.status().phase, Phase::Closed);
    }

    #[test]
    fn frame_reader_reassembles_across_short_reads() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut reader = FrameReader::new(ours);
        let frame = encode_frame(b"hello world").unwrap();
        for b in &frame {
            (&mut theirs).write_all(&[*b]).unwrap();
        }
        theirs.shutdown(std::net::Shutdown::Write).unwrap();

        let mut out = Vec::new();
        let mut buf = [0u8; 4];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn frame_reader_rejects_oversize_declared_length() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut reader = FrameReader::new(ours);
        let too_big = (MAX_FRAME_PAYLOAD as u32 + 1).to_le_bytes();
        (&mut theirs).write_all(&too_big).unwrap();
        let mut buf = [0u8; 8];
        let err = reader.read(&mut buf).expect_err("oversize frame must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn plugin_ssh_kind_alias_is_normalized() {
        let mut p = Profile { kind: "plugin-ssh".into(), ..Profile::default() };
        p.normalize_plugin_kind();
        assert_eq!(p.kind, "ssh");
        assert_eq!(p.transport, "plugin");
        assert!(p.is_plugin_ssh());
    }
}
