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
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tervo_core::transport::{
    PaneTransport, TransportControl, TransportHalves, TransportShutdown,
};

use crate::profile::{Profile, ProfileStore};
use crate::transport_io::{decode_frame, encode_frame, MAX_FRAME_PAYLOAD};

/// 默认“多久没有任何远端输出就判定连接超时”。ssh 自己的 ConnectTimeout 更短，
/// 这里兜底 DNS/ProxyJump/认证阶段卡住的情况。
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// 单帧 payload（读 ssh stdout 的块大小）；远小于协议 256 KiB 上限。
const READ_CHUNK: usize = 32 * 1024;

/// stderr 环形缓冲只保留最后这么多字节；够看清 ssh 的错误，又不至于把 pane 灌满。
const STDERR_TAIL: usize = 8 * 1024;

/// `extra_args` 里不允许出现的 ssh 选项：它们会覆盖/绕过宿主的 known_hosts
/// 预检（StrictHostKeyChecking/UserKnownHostsFile/…）或在本地执行命令
/// （ProxyCommand/LocalCommand/…）。比较大小写不敏感。
const SENSITIVE_SSH_OPTIONS: &[&str] = &[
    "stricthostkeychecking",
    "userknownhostsfile",
    "globalknownhostsfile",
    "knownhostscommand",
    "hostkeyalias",
    "proxycommand",
    "proxyjump",
    "localcommand",
    "permitlocalcommand",
];

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
    /// 生命周期标志和 `phase` 的终态转移在同一把锁下决定，避免 watchdog
    /// “先检查后上报”在远端输出到达的瞬间误杀连接。
    lifecycle: Mutex<Lifecycle>,
    /// 帧写锁：pump 与 watchdog 都可能写同一个 socket，帧不能交错。
    write_lock: Mutex<()>,
}

#[derive(Default)]
struct Lifecycle {
    got_output: bool,
    notified: bool,
    closed: bool,
}

/// watchdog 是否可以在当前状态下宣布超时（调用者必须持有 `lifecycle` 锁）。
///
/// 抽成纯函数，测试覆盖“输出/关闭/已上报”与超时的各种交错顺序。
fn can_claim_timeout(got_output: bool, notified: bool, closed: bool) -> bool {
    !got_output && !notified && !closed
}

impl Shared {
    fn new() -> Self {
        Self {
            status: Mutex::new(SshConnectionStatus::new()),
            lifecycle: Mutex::new(Lifecycle::default()),
            write_lock: Mutex::new(()),
        }
    }

    /// 生命周期锁：状态机不允许因为某个线程 panic 而永久卡死，poison 后继续用。
    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, Lifecycle> {
        self.lifecycle.lock().unwrap_or_else(|e| e.into_inner())
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

    /// 收到远端输出：先置 `got_output`（在生命周期锁内），再提升 phase。
    /// watchdog 若在此之后才判定超时，会在同一把锁下看到 `got_output` 而放弃。
    fn note_output(&self) {
        let connected = {
            let mut lc = self.lock_lifecycle();
            lc.got_output = true;
            !lc.notified && !lc.closed
        };
        if !connected {
            return;
        }
        if let Ok(mut st) = self.status.lock() {
            if st.phase == Phase::Connecting {
                st.phase = Phase::Connected;
                st.detail = "remote output received".into();
            }
        }
    }

    fn has_output(&self) -> bool {
        self.lock_lifecycle().got_output
    }

    fn is_closed(&self) -> bool {
        self.lock_lifecycle().closed
    }

    fn was_notified(&self) -> bool {
        self.lock_lifecycle().notified
    }

    /// 原子地“检查并认领超时”：只有未收到输出、未关闭、未上报过时成功。
    /// 检查与置位在同一把锁内，输出线程不可能在两者之间插入。
    fn claim_timeout(&self, detail: impl Into<String>) -> bool {
        {
            let mut lc = self.lock_lifecycle();
            if !can_claim_timeout(lc.got_output, lc.notified, lc.closed) {
                return false;
            }
            lc.notified = true;
        }
        self.set(Phase::TimedOut, detail);
        true
    }

    /// 正常关闭（远端 EOF）：只有没有 timeout/failed 认领过时才写状态。
    fn claim_closed(&self, detail: impl Into<String>) -> bool {
        {
            let mut lc = self.lock_lifecycle();
            if lc.notified {
                return false;
            }
            lc.notified = true;
        }
        self.set(Phase::Closed, detail);
        true
    }

    /// 只有第一个调用者能拿到错误上报权（避免超时 + EOF 重复打印）。
    fn claim_error(&self) -> bool {
        let mut lc = self.lock_lifecycle();
        if lc.notified || lc.closed {
            return false;
        }
        lc.notified = true;
        true
    }

    /// 用户主动关闭：状态只写一次，不覆盖已经上报的 timeout/failed。
    fn mark_closed(&self) {
        let first = {
            let mut lc = self.lock_lifecycle();
            lc.closed = true;
            if lc.notified {
                false
            } else {
                lc.notified = true;
                true
            }
        };
        if first {
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

/// spawn 后、transport 构造完成前的 RAII 收尸守卫：任何提前返回（取 stdio
/// 失败、socket 创建失败、线程 spawn 失败）都会 kill+wait，不留 ssh 子进程。
struct ChildGuard {
    child: Arc<Mutex<Child>>,
    armed: bool,
}

impl ChildGuard {
    fn new(child: Arc<Mutex<Child>>) -> Self {
        Self { child, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    /// 立刻收尸（错误路径用）；之后 Drop 不再重复。
    fn cleanup(&mut self) {
        if self.armed {
            self.armed = false;
            kill_and_wait(&self.child);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn kill_and_wait(child: &Arc<Mutex<Child>>) {
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 关闭 ssh transport：标记状态、收尸、shutdown 数据 socket。
///
/// 控制半边 `close` 与 `TransportHalves::shutdown` 共用；必须幂等。
fn shutdown_ssh(child: &Arc<Mutex<Child>>, socket: &UnixStream, shared: &Shared) {
    shared.mark_closed();
    kill_and_wait(child);
    let _ = socket.shutdown(std::net::Shutdown::Both);
}

/// 系统 `ssh` 子进程 + 帧化 stdout 的 pane 后端。
pub struct SshPluginTransport {
    /// `into_halves()` 会把读/写半边移交给 pane，所以这里有 `Option` 供 `take`。
    reader: Option<FrameReader>,
    stdin: Option<ChildStdin>,
    child: Arc<Mutex<Child>>,
    shared: Arc<Shared>,
    /// 最近一次 resize（POC 只记录，不转发；见模块注释）。
    resize: Arc<AtomicU32>,
    /// `into_halves` 之后由控制半边负责收尸，本结构 Drop 不再 kill。
    owns_child: bool,
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
        let (program, args) = plugin_ssh_command(profile)?;
        // 安全门禁：真实 ssh 在 spawn 之前必须已经在本机 known_hosts 里确认过
        // 主机密钥，避免 plugin 入口绕过 UI 的 known_hosts 预检。
        host_key_precheck(profile, &program)?;
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
        let child = command.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("启动 {program} 失败（plugin-ssh）: {e}"),
            )
        })?;
        let child = Arc::new(Mutex::new(child));
        // 从这里开始任何提前返回（包括取 stdio 失败）都必须收尸。
        let mut guard = ChildGuard::new(child.clone());
        let (stdin, stdout, stderr) = {
            let mut child = child.lock().map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "ssh child lock poisoned")
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
            (stdin, stdout, stderr)
        };

        let (tx, rx) = UnixStream::pair()?;
        let mut sink = tx.try_clone()?;
        let shared = Arc::new(Shared::new());
        // 先给用户一个“正在连接”的反馈；错的时候后面还有红色错误文本。
        let _ = shared.send(&mut sink, &connecting_message(&label));

        let stderr_buf = Arc::new(Mutex::new(StderrTail::new(STDERR_TAIL)));
        let stderr_thread = spawn_stderr(stderr, stderr_buf.clone())?;
        let _pump = spawn_pump(
            stdout,
            sink,
            child.clone(),
            shared.clone(),
            stderr_buf,
            stderr_thread,
            label.clone(),
        )?;
        if let Err(e) = spawn_watchdog(tx, child.clone(), shared.clone(), timeout, label) {
            // pump/stderr 线程会因 child 被 kill 而自行退出；这里只保证不泄漏进程。
            guard.cleanup();
            return Err(e);
        }
        guard.disarm();

        Ok(Self {
            reader: Some(FrameReader::new(rx)),
            stdin: Some(stdin),
            child,
            shared,
            resize: Arc::new(AtomicU32::new(0)),
            owns_child: true,
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
        self.reader
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "transport 已拆分"))?
            .stream
            .set_read_timeout(timeout)
    }

    fn close_inner(&mut self) {
        self.shared.mark_closed();
        kill_and_wait(&self.child);
        if let Some(reader) = self.reader.as_ref() {
            let _ = reader.stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for SshPluginTransport {
    fn drop(&mut self) {
        if self.owns_child {
            self.close_inner();
        }
    }
}

impl PaneTransport for SshPluginTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.reader.as_mut() {
            Some(reader) => reader.read(buf),
            None => Ok(0),
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "transport 已拆分")
        })?;
        stdin.write_all(data)?;
        stdin.flush()
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.resize.store(pack_resize(cols, rows), Ordering::Relaxed);
    }

    fn close(&mut self) {
        self.close_inner();
    }

    fn into_halves(self: Box<Self>) -> io::Result<TransportHalves> {
        let mut this = *self;
        let reader = this.reader.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "transport 已拆分")
        })?;
        let stdin = this.stdin.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "transport 已拆分")
        })?;
        let control_socket = match reader.stream.try_clone() {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                // 放回半边，让 `this` 的 Drop 仍然收尸并 shutdown。
                this.reader = Some(reader);
                this.stdin = Some(stdin);
                return Err(e);
            },
        };
        // 从此刻起控制半边/独立 shutdown 负责 kill；`this` 的 Drop 不能重复收尸。
        this.owns_child = false;
        let shutdown_socket = control_socket.clone();
        let shutdown_child = this.child.clone();
        let shutdown_shared = this.shared.clone();
        Ok(TransportHalves {
            reader: Box::new(reader),
            writer: Box::new(stdin),
            control: Box::new(SshControl {
                child: this.child.clone(),
                socket: control_socket,
                shared: this.shared.clone(),
                resize: this.resize.clone(),
            }),
            // PaneHandle::drop 直接调用：即使 actor 来不及处理 Ctrl::Shutdown，
            // 也能 shutdown socket / 杀掉 ssh，唤醒阻塞中的 FrameReader。
            shutdown: TransportShutdown::new(move || {
                shutdown_ssh(&shutdown_child, &shutdown_socket, &shutdown_shared);
            }),
        })
    }
}

/// transport 的控制半边：resize 记录 + close 杀 ssh。
struct SshControl {
    child: Arc<Mutex<Child>>,
    socket: Arc<UnixStream>,
    shared: Arc<Shared>,
    resize: Arc<AtomicU32>,
}

impl SshControl {
    fn close_inner(&mut self) {
        shutdown_ssh(&self.child, &self.socket, &self.shared);
    }
}

impl TransportControl for SshControl {
    fn resize(&mut self, cols: u16, rows: u16) {
        self.resize.store(pack_resize(cols, rows), Ordering::Relaxed);
    }

    fn close(&mut self) {
        self.close_inner();
    }
}

impl Drop for SshControl {
    fn drop(&mut self) {
        self.close_inner();
    }
}

/// 系统 ssh 命令。`profile.args` 若已由 `ssh_command()` 生成就直接复用，否则
/// 现场构造。
///
/// 安全选项插在参数最前面：OpenSSH 对同一 `-o` 取**第一次**出现的值，
/// 放在 extra_args 之后会被它们降级（例如 `StrictHostKeyChecking=no`）。
pub fn plugin_ssh_command(profile: &Profile) -> io::Result<(String, Vec<String>)> {
    let (program, mut args) = if !profile.shell.trim().is_empty() && profile.shell != "ssh" {
        // 测试/自定义后端：shell 脚本自己忽略这些参数。
        (profile.shell.trim().to_string(), profile.args.clone())
    } else if !profile.args.is_empty() {
        ("ssh".to_string(), profile.args.clone())
    } else {
        profile.ssh_command()
    };
    reject_unsafe_extra_args(&args)?;
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
    args.splice(0..0, options);
    Ok((program, args))
}

/// 从 `-o key=value` / `-okey=value` 里取小写 key；不是选项赋值时返回 `None`。
fn ssh_option_key(value: &str) -> String {
    value
        .trim()
        .split(['=', ' ', '\t'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// 校验 `extra_args`：拒绝会绕过 known_hosts/本地命令策略的选项。
fn reject_unsafe_extra_args(args: &[String]) -> io::Result<()> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].trim();
        // `-F` 指定另一份配置：配置文件里的 StrictHostKeyChecking no 无法
        // 被上面的 key 检查看到，直接拒绝。
        if arg == "-F" || (arg.starts_with("-F") && arg.len() > 2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin-ssh 不允许 extra_args 用 -F 指定 ssh 配置文件",
            ));
        }
        let option = if arg == "-o" || arg == "--option" {
            i += 1;
            args.get(i).map(String::as_str)
        } else {
            arg.strip_prefix("-o").filter(|rest| !rest.is_empty())
        };
        if let Some(option) = option {
            let key = ssh_option_key(option);
            if let Some(sensitive) =
                SENSITIVE_SSH_OPTIONS.iter().copied().find(|opt| key == *opt)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "plugin-ssh 不允许 extra_args 覆盖安全选项 {sensitive}；\
                         主机密钥/KnownHosts/Proxy 配置请走 Profile 字段或普通 ssh 路径"
                    ),
                ));
            }
        }
        i += 1;
    }
    Ok(())
}

/// 是否是真正的 OpenSSH 客户端（自定义 shell/测试脚本不查 known_hosts）。
fn is_real_ssh(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy() == "ssh")
        .unwrap_or(false)
}

/// spawn 前的 known_hosts 门禁：真实 ssh 必须已经在本机确认过主机密钥，
/// 避免 `ssh.connect`/插件入口绕过 UI 的 known_hosts 预检。
fn host_key_precheck(profile: &Profile, program: &str) -> io::Result<()> {
    host_key_precheck_with(profile, program, || {
        crate::known_hosts::has_known_entry(profile)
    })
}

fn host_key_precheck_with(
    profile: &Profile,
    program: &str,
    has_entry: impl FnOnce() -> bool,
) -> io::Result<()> {
    if !is_real_ssh(program) {
        return Ok(());
    }
    if has_entry() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "主机密钥未在 known_hosts 中确认：请先通过普通 SSH 连接确认 {} 的主机密钥\
             （或手动写入 known_hosts）后再使用 plugin transport",
            remote_label(profile)
        ),
    ))
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
    let user = sanitize_control(profile.user.trim());
    let host = sanitize_control(profile.host.trim());
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

/// 不可信文本（host/user/stderr）→ 可安全放进终端文本。
///
/// 保留 `\n`/`\t`（stderr 多行），删除其余 C0/DEL/C1；ESC 转成可见的 `␛`，
/// 这样 `\x1b]52;...` 之类的 OSC/CSI 注入只会显示成普通字符。
fn sanitize_control(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' | '\t' => out.push(ch),
            '\u{1b}' => out.push('␛'),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {},
            c if ('\u{80}'..='\u{9f}').contains(&c) => {},
            c => out.push(c),
        }
    }
    out
}

/// stderr → 终端文本：统一 CRLF、去掉空行、只留**最后** 12 行。
///
/// 与 [`StderrTail`] 的“保留最后 8KiB”保持一致；限制头部行数会把最后一条
/// 错误挤掉，所以这里取尾部。
fn sanitize_stderr(stderr: &str) -> String {
    let cleaned = stderr.replace('\r', "");
    let lines: Vec<&str> = cleaned
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(12);
    lines[start..]
        .iter()
        .map(|line| format!("{}\r\n", sanitize_control(line)))
        .collect()
}

/// 只保留最后 `limit` 字节的 stderr 缓冲（环形语义；绝不保留头部）。
struct StderrTail {
    buf: Vec<u8>,
    limit: usize,
}

impl StderrTail {
    fn new(limit: usize) -> Self {
        Self { buf: Vec::with_capacity(limit.min(4096)), limit }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.limit == 0 {
            return;
        }
        if chunk.len() >= self.limit {
            self.buf.clear();
            self.buf.extend_from_slice(&chunk[chunk.len() - self.limit..]);
            return;
        }
        let overflow = self
            .buf
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.limit);
        if overflow > 0 {
            self.buf.drain(..overflow);
        }
        self.buf.extend_from_slice(chunk);
    }

    fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buf.len()
    }
}

fn spawn_stderr(
    mut stderr: std::process::ChildStderr,
    buf: Arc<Mutex<StderrTail>>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("plugin-ssh-err".into())
        .spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut out) = buf.lock() {
                            out.push(&chunk[..n]);
                        }
                    },
                    Err(_) => break,
                }
            }
        })
}

fn spawn_pump(
    mut stdout: std::process::ChildStdout,
    mut sink: UnixStream,
    child: Arc<Mutex<Child>>,
    shared: Arc<Shared>,
    stderr_buf: Arc<Mutex<StderrTail>>,
    stderr_thread: JoinHandle<()>,
    label: String,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name("plugin-ssh-out".into()).spawn(move || {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut read_error: Option<io::Error> = None;
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    shared.note_output();
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
        let _ = stderr_thread.join();
        let stderr = stderr_buf
            .lock()
            .map(|b| b.as_string())
            .unwrap_or_default();

        if shared.is_closed() {
            shared.set(Phase::Closed, "closed by user");
            let _ = shared.send(&mut sink, &closed_message("用户关闭"));
            return;
        }
        // watchdog 已经发过超时文本：不重复。
        if shared.was_notified() {
            return;
        }

        match (&exit, &read_error) {
            (Some(status), _) if status.success() => {
                if !shared.claim_closed("remote closed") {
                    return;
                }
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
    })
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
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name("plugin-ssh-watchdog".into()).spawn(move || {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if shared.is_closed() || shared.has_output() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // 最终判定与输出线程的 note_output 在同一把锁下排序：如果输出在
        // 临界区之前到达，这里必然看到 got_output 并放弃；不存在
        // “检查时还没输出、认领时输出刚到”而被误杀的窗口。
        let detail = format!("{}s 无远端输出", timeout.as_secs_f32());
        if !shared.claim_timeout(detail) {
            return;
        }
        eprintln!("[plugin-ssh] timeout host={label} after={}ms", timeout.as_millis());
        let _ = shared.send(&mut sink, &timeout_message(&label, timeout));
        let _ = sink.shutdown(std::net::Shutdown::Write);
        kill_and_wait(&child);
    })
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

    /// 临时假 ssh 脚本（独立目录，脚本名可指定）；Drop 时整目录清理。
    struct FakeSsh {
        dir: std::path::PathBuf,
        path: std::path::PathBuf,
    }

    impl FakeSsh {
        fn new(body: &str) -> Self {
            Self::named("fake-ssh.sh", body)
        }

        /// 指定脚本文件名：`ssh` 用来验证真实 ssh 的 known_hosts 预检分支。
        fn named(name: &str, body: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "tervo-fake-ssh-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            Self { dir, path }
        }

        fn profile(&self) -> Profile {
            Profile {
                kind: "ssh".into(),
                host: "fake.test".into(),
                port: 22,
                user: "u".into(),
                auth: "agent".into(),
                shell: self.path.to_string_lossy().into_owned(),
                ..Profile::default()
            }
        }
    }

    impl Drop for FakeSsh {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// 每次测试独立临时文件路径；调用方负责删除。
    fn temp_path(name: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "tervo-ssh-test-{}-{}-{name}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn wait_for_file(path: &std::path::Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.is_file() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        path.is_file()
    }

    fn read_pid(path: &std::path::Path) -> u32 {
        std::fs::read_to_string(path)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid")
    }

    fn process_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    fn wait_until_gone(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !process_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        !process_alive(pid)
    }

    /// 测试环境里并行 fork/exec 可能短暂继承另一个测试“正在写脚本”的 fd，
    /// 触发 ETXTBSY；这是测试进程的 fork 竞态，不是 transport 行为，重试即可。
    fn start_fake(profile: &Profile, timeout: Duration) -> io::Result<SshPluginTransport> {
        for _ in 0..5 {
            match SshPluginTransport::start_with_timeout(profile, timeout) {
                Err(e) if e.to_string().contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(25));
                },
                other => return other,
            }
        }
        SshPluginTransport::start_with_timeout(profile, timeout)
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
        let (program, args) = plugin_ssh_command(&p).expect("valid ssh profile");
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
        let err = start_fake(&p, Duration::from_millis(200))
            .err()
            .expect("password auth must be rejected");
        assert!(err.to_string().contains("密码认证"), "{err}");
    }

    #[test]
    fn fake_ssh_banner_reaches_pane_and_eof_closes_cleanly() {
        let fake = FakeSsh::new("printf 'Welcome to fake ssh\\r\\n'; sleep 0.1");
        let mut t = start_fake(
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
        let mut t = start_fake(
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
        let t = start_fake(
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
        let mut t = start_fake(
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
        let mut t = start_fake(
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

    #[test]
    fn dropping_transport_without_close_kills_ssh() {
        let pidfile = temp_path("drop.pid");
        let fake = FakeSsh::new(&format!(
            "echo $$ > '{}'; exec sleep 30",
            pidfile.display()
        ));
        let t = start_fake(&fake.profile(), Duration::from_secs(30))
            .expect("transport should start");
        assert!(wait_for_file(&pidfile, Duration::from_secs(2)), "fake ssh 没有起来");
        let pid = read_pid(&pidfile);
        drop(t);
        assert!(
            wait_until_gone(pid, Duration::from_millis(500)),
            "drop transport 后 ssh 子进程仍然存活 (pid={pid})"
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn dropping_control_half_kills_ssh() {
        let pidfile = temp_path("halves.pid");
        let fake = FakeSsh::new(&format!(
            "echo $$ > '{}'; exec sleep 30",
            pidfile.display()
        ));
        let t = start_fake(&fake.profile(), Duration::from_secs(30))
            .expect("transport should start");
        assert!(wait_for_file(&pidfile, Duration::from_secs(2)), "fake ssh 没有起来");
        let pid = read_pid(&pidfile);
        let halves = Box::new(t).into_halves().expect("into_halves");
        drop(halves);
        assert!(
            wait_until_gone(pid, Duration::from_millis(500)),
            "drop control 半边后 ssh 子进程仍然存活 (pid={pid})"
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn stderr_ring_keeps_last_bytes_not_first() {
        let fake = FakeSsh::new(concat!(
            "i=0; ",
            "while [ \"$i\" -lt 200 ]; do ",
            "printf 'line-%03d %s\\n' \"$i\" 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' >&2; ",
            "i=$((i+1)); ",
            "done; ",
            "printf 'LAST-LINE\\n' >&2; exit 255"
        ));
        let mut t = start_fake(&fake.profile(), Duration::from_secs(5))
            .expect("transport should start");
        let bytes = read_all(&mut t);
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("LAST-LINE"),
            "stderr 尾部丢了最后一行（实现了只留头部？）: len={}",
            text.len()
        );
        assert!(
            !text.contains("line-000"),
            "8KiB 环形缓冲不应该保留最早的行: len={}",
            text.len()
        );
    }

    #[test]
    fn stderr_control_sequences_are_sanitized_in_pane() {
        let fake = FakeSsh::new(
            "printf 'pre\\033]52;c;Zm9v\\007post\\n' >&2; exit 255",
        );
        let mut t = start_fake(&fake.profile(), Duration::from_secs(5))
            .expect("transport should start");
        let bytes = read_all(&mut t);
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("\x1b]52"), "OSC 52 不得原样进入 pane: {text:?}");
        assert!(!text.contains('\x07'), "BEL 不得原样进入 pane: {text:?}");
        assert!(
            text.contains("pre␛]52;c;Zm9vpost"),
            "ESC 应转成可见字符且文本保留: {text:?}"
        );
    }

    #[test]
    fn remote_label_sanitizes_control_characters() {
        let mut p = ssh_profile();
        p.host = "evil\x1b]52;c;Zm9v\x07.test".into();
        p.user = "u\x1b[31m".into();
        let label = remote_label(&p);
        assert!(!label.contains('\x1b'), "label 里不能有裸 ESC: {label:?}");
        assert!(!label.contains('\x07'), "label 里不能有 BEL: {label:?}");
        assert!(label.contains("␛]52;c;Zm9v"), "ESC 应转成可见字符: {label:?}");
        assert!(label.contains("u␛[31m@"), "user 也要 sanitize: {label:?}");
    }

    #[test]
    fn fake_ssh_sees_security_options_before_extra_args() {
        let argvfile = temp_path("argv");
        let fake = FakeSsh::new(&format!(
            "printf '%s\\n' \"$@\" > '{}'",
            argvfile.display()
        ));
        let mut p = fake.profile();
        p.args = vec![
            "-o".into(),
            "SendEnv=TERVO_TEST".into(),
            "fake.test".into(),
        ];
        let mut t = start_fake(&p, Duration::from_secs(5))
            .expect("transport should start");
        let _ = read_all(&mut t);
        let recorded = std::fs::read_to_string(&argvfile).expect("argv file");
        let args: Vec<&str> = recorded.lines().collect();
        let _ = std::fs::remove_file(&argvfile);
        assert_eq!(args.last().copied(), Some("fake.test"), "host 必须最后: {args:?}");
        assert!(args.contains(&"-tt"), "缺少 -tt: {args:?}");
        let batch = args
            .iter()
            .position(|a| *a == "BatchMode=yes")
            .unwrap_or_else(|| panic!("缺少 BatchMode=yes: {args:?}"));
        let extra = args
            .iter()
            .position(|a| *a == "SendEnv=TERVO_TEST")
            .unwrap_or_else(|| panic!("缺少 extra_args: {args:?}"));
        assert!(
            batch < extra,
            "安全 -o 必须排在 extra_args 之前: {args:?}"
        );
    }

    #[test]
    fn sensitive_extra_args_are_rejected() {
        for extra in [
            "StrictHostKeyChecking=no",
            "UserKnownHostsFile=/tmp/evil-known-hosts",
            "GlobalKnownHostsFile=/dev/null",
            "ProxyCommand=/bin/sh -c true",
            "KnownHostsCommand=/bin/false",
            "HostKeyAlias=evil",
            "LocalCommand=/bin/true",
            "PermitLocalCommand=yes",
        ] {
            let mut p = ssh_profile();
            p.extra_args = vec!["-o".into(), extra.into()];
            let err = plugin_ssh_command(&p)
                .err()
                .unwrap_or_else(|| panic!("敏感选项 {extra} 必须被拒绝"));
            assert!(
                err.to_string().contains("extra_args"),
                "{extra}: 错误信息应指明 extra_args: {err}"
            );
        }

        // `-oKEY=VALUE` 连写与大小写混用同样要拦。
        let mut p = ssh_profile();
        p.extra_args = vec!["-ostricthostkeychecking=no".into()];
        assert!(plugin_ssh_command(&p).is_err(), "attached -o 形式必须被拒绝");

        // 换配置文件会绕过 -o 检查。
        let mut p = ssh_profile();
        p.extra_args = vec!["-F".into(), "/tmp/evil-ssh.conf".into()];
        assert!(plugin_ssh_command(&p).is_err(), "-F 必须被拒绝");
    }

    #[test]
    fn host_key_precheck_gates_real_ssh_only() {
        let p = ssh_profile();
        assert!(host_key_precheck_with(&p, "/usr/bin/ssh", || true).is_ok());
        let err = host_key_precheck_with(&p, "/usr/bin/ssh", || false)
            .expect_err("known_hosts 未确认时必须拒绝");
        assert!(err.to_string().contains("known_hosts"), "{err}");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        // 自定义 shell/测试后端不查 known_hosts。
        assert!(host_key_precheck_with(&p, "/tmp/fake-ssh.sh", || false).is_ok());
    }

    #[test]
    fn start_refuses_unverified_host_key_before_spawning() {
        let marker = temp_path("spawned");
        let fake = FakeSsh::named("ssh", &format!("touch '{}'", marker.display()));
        let mut p = fake.profile();
        p.host = format!("unverified-{}.invalid", std::process::id());
        let err = start_fake(&p, Duration::from_millis(200))
            .err()
            .expect("known_hosts 未确认时必须在 spawn 前拒绝");
        assert!(err.to_string().contains("known_hosts"), "{err}");
        assert!(!marker.exists(), "预检失败但 ssh 脚本仍被 spawn 了");
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn child_guard_kills_and_reaps_on_early_return() {
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let guard = ChildGuard::new(child.clone());
        drop(guard);
        assert!(
            wait_until_gone(pid, Duration::from_millis(500)),
            "ChildGuard drop 后子进程仍然存活 (pid={pid})"
        );
    }

    #[test]
    fn watchdog_timeout_claim_is_atomic_with_output_and_close() {
        assert!(can_claim_timeout(false, false, false));
        for (got_output, notified, closed) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, false, true),
            (false, true, true),
        ] {
            assert!(
                !can_claim_timeout(got_output, notified, closed),
                "got_output={got_output} notified={notified} closed={closed} 不应判超时"
            );
        }

        // 交错 1：输出先到 -> watchdog 不能再判超时。
        let shared = Shared::new();
        shared.note_output();
        assert!(!shared.claim_timeout("timeout"), "收到输出后不能判超时");
        assert_eq!(shared.snapshot().phase, Phase::Connected);

        // 交错 2：超时先认领 -> 迟到输出不能把 TimedOut 改回 Connected。
        let shared = Shared::new();
        assert!(shared.claim_timeout("timeout"));
        shared.note_output();
        assert_eq!(shared.snapshot().phase, Phase::TimedOut);

        // 交错 3：用户关闭 -> 不再判超时。
        let shared = Shared::new();
        shared.mark_closed();
        assert!(!shared.claim_timeout("timeout"));
        assert_eq!(shared.snapshot().phase, Phase::Closed);
    }

    #[test]
    fn watchdog_does_not_kill_connection_after_banner() {
        let fake = FakeSsh::new("printf 'banner\\n'; exec sleep 30");
        let mut t = start_fake(
            &fake.profile(),
            Duration::from_millis(250),
        )
        .expect("transport should start");
        let mut text = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(2);
        while !String::from_utf8_lossy(&text).contains("banner") {
            assert!(Instant::now() < deadline, "没等到 banner");
            match t.read(&mut buf) {
                Ok(0) => panic!("unexpected EOF before banner"),
                Ok(n) => text.extend_from_slice(&buf[..n]),
                Err(e) => panic!("read failed: {e}"),
            }
        }
        // 超过 watchdog 的 250ms 仍未退出；收到过输出的连接不能被误杀。
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            t.status().phase,
            Phase::Connected,
            "有远端输出后 watchdog 不应判定超时: {}",
            t.status().detail
        );
        t.close();
    }

    #[test]
    fn stderr_tail_ring_keeps_last_bytes_across_chunks() {
        let mut tail = StderrTail::new(STDERR_TAIL);
        let mut last_line = String::new();
        for i in 0..180 {
            let line = format!("line-{i:03} {}\n", "y".repeat(60));
            if i == 179 {
                last_line = line.clone();
            }
            tail.push(line.as_bytes());
        }
        assert_eq!(tail.len(), STDERR_TAIL, "环形缓冲必须稳定在 limit");
        let text = tail.as_string();
        assert!(text.contains("line-179"), "最后一行必须保留");
        assert!(!text.contains("line-000"), "最早的行必须被挤掉");
        assert!(text.ends_with(&last_line), "保留的必须是最新字节: {text:?}");

        // 单次 push 超过 limit：只留最后 limit 字节。
        let mut tail = StderrTail::new(16);
        tail.push(b"0123456789abcdefXYZ");
        assert_eq!(tail.as_string(), "3456789abcdefXYZ");
    }
}
