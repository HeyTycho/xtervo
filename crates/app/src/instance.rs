// SPDX-License-Identifier: GPL-3.0-only
//! 单实例 primary / secondary IPC 原型（多窗口 Phase 3 的基础设施）。
//!
//! 本模块被 `main.rs` 产品启动路径使用：primary 在 UI 平台初始化后启动
//! control server，secondary 转发 `open_window` 并在收到 ack 后退出。集成方负责：
//! - 在 UI 线程创建窗口（[`OpenWindowHandler`] 在 control 连接线程上被调用，
//!   需要用 `slint::invoke_from_event_loop` 或 channel 转给 UI 线程）；
//! - 决定 primary / secondary 后分别调用 [`PrimaryServer::start_with_lock`] 或
//!   [`forward_open_window`]。
//!
//! 协议：
//! - primary 持有 `$XDG_RUNTIME_DIR/xtervo/instance.lock` 上的
//!   `flock(LOCK_EX | LOCK_NB)`；锁随进程退出（fd 关闭）自动释放。
//! - primary 在 `control.sock`（0600）监听，token 在 `instance.token`（0600）。
//! - secondary 发送一行 JSON：
//!   `{"token":"…","op":"open_window","profile":?,"cwd":?}`；
//!   primary 回一行 JSON：`{"ok":true,"op":"open_window"}` 或
//!   `{"ok":false,"error":"…"}`。收到 ack 后 secondary 返回、由调用方退出。
//!
//! token 只存在于 0600 文件和进程内存中，本模块不会把它写进日志或错误信息。
//!
//! 没有 `XDG_RUNTIME_DIR` 时依次退回 `$XDG_STATE_HOME/xtervo`、
//! `~/.local/state/xtervo`、`$TMPDIR/xtervo-<uid>`。退回的目录会校验
//! owner/类型并收紧到 0700；symlink 或 owner 不符直接报错，而不是盲目使用
//! 可预测的临时路径。

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const CONTROL_DIR_NAME: &str = "xtervo";
pub const LOCK_FILE_NAME: &str = "instance.lock";
pub const SOCKET_FILE_NAME: &str = "control.sock";
pub const TOKEN_FILE_NAME: &str = "instance.token";

/// Unix socket path 的实际上限通常是 108 字节；与 MCP socket 一样留出余量。
const MAX_SOCKET_PATH_BYTES: usize = 100;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_ACK_BYTES: usize = 16 * 1024;
const MAX_PROFILE_BYTES: usize = 256;
const MAX_CWD_BYTES: usize = 4096;
const MAX_ERROR_CHARS: usize = 512;

/// secondary 等待 primary 连接/ack 的默认上限。
///
/// primary 可能刚拿到锁、还在初始化首个窗口/事件循环；secondary 会先按
/// [`forward_open_window_with_timeout`] 的连接重试策略等待，再在这个上限内等
/// UI 线程真正建完窗口后的 ack。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// primary 启动与 secondary 转发之间的竞态窗口：最多重试一次。
const RETRY_DELAY: Duration = Duration::from_millis(150);
/// accept 线程轮询间隔；Drop 时 join 最多等这么久。
const ACCEPT_POLL: Duration = Duration::from_millis(20);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);

/// 一次 `open_window` 请求的纯数据部分。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenWindowRequest {
    /// 可选的 profile id/name；由 primary 的 UI 层决定是否认识。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// 可选的工作目录；必须是绝对路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

impl OpenWindowRequest {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)] // 协议/测试保留；CLI 目前只转发 cwd。
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// 在发送前/调用 handler 前校验边界，避免把任意长度的字符串塞进 UI。
    pub fn validate(&self) -> Result<(), String> {
        if let Some(profile) = &self.profile {
            if profile.is_empty() {
                return Err("profile must not be empty".into());
            }
            if profile.len() > MAX_PROFILE_BYTES {
                return Err(format!("profile is longer than {MAX_PROFILE_BYTES} bytes"));
            }
            if profile.chars().any(char::is_control) {
                return Err("profile contains control characters".into());
            }
        }
        if let Some(cwd) = &self.cwd {
            let bytes = cwd.as_os_str().as_encoded_bytes();
            if bytes.is_empty() {
                return Err("cwd must not be empty".into());
            }
            if bytes.len() > MAX_CWD_BYTES {
                return Err(format!("cwd is longer than {MAX_CWD_BYTES} bytes"));
            }
            if bytes.contains(&0) {
                return Err("cwd contains a NUL byte".into());
            }
            if !cwd.is_absolute() {
                return Err("cwd must be an absolute path".into());
            }
        }
        Ok(())
    }
}

/// primary 的所有路径（同一目录下的 lock/token/socket）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstancePaths {
    dir: PathBuf,
    lock_path: PathBuf,
    socket_path: PathBuf,
    token_path: PathBuf,
}

impl InstancePaths {
    /// 根据显式输入解析路径，便于测试且不读环境变量。
    ///
    /// 候选目录按优先级选择；socket 路径超过 [`MAX_SOCKET_PATH_BYTES`] 时跳到
    /// 下一个候选。调用方应传绝对路径；相对候选会被忽略。
    pub fn resolve(
        runtime_dir: Option<&Path>,
        state_home: Option<&Path>,
        home: Option<&Path>,
        temp_dir: &Path,
        uid: u32,
    ) -> Self {
        let mut candidates = Vec::with_capacity(4);
        if let Some(dir) = runtime_dir.filter(|path| usable_base(path)) {
            candidates.push(dir.join(CONTROL_DIR_NAME));
        }
        if let Some(dir) = state_home.filter(|path| usable_base(path)) {
            candidates.push(dir.join(CONTROL_DIR_NAME));
        }
        if let Some(dir) = home.filter(|path| usable_base(path)) {
            candidates.push(dir.join(".local").join("state").join(CONTROL_DIR_NAME));
        }
        let temp_candidate = temp_dir.join(format!("{CONTROL_DIR_NAME}-{uid}"));
        candidates.push(temp_candidate.clone());
        let dir = candidates
            .into_iter()
            .find(|dir| socket_path_fits(dir))
            .unwrap_or(temp_candidate);
        Self::from_dir(dir)
    }

    /// 读取 `XTERVO_INSTANCE_DIR` / `XDG_RUNTIME_DIR` / `XDG_STATE_HOME` /
    /// `HOME` / `TMPDIR`。
    ///
    /// `XTERVO_INSTANCE_DIR` 是测试/高级隔离开关：显式指定整个 control 目录
    /// （lock/token/socket 都在其中）。相对路径按无效处理，继续走 fallback。
    /// 相对路径的 XDG/HOME 环境变量同样视为无效（XDG spec 要求绝对路径）。
    pub fn from_env() -> Self {
        if let Some(dir) = env_absolute("XTERVO_INSTANCE_DIR") {
            return Self::from_dir(dir);
        }
        let runtime_dir = env_absolute("XDG_RUNTIME_DIR");
        let state_home = env_absolute("XDG_STATE_HOME");
        let home = env_absolute("HOME");
        Self::resolve(
            runtime_dir.as_deref(),
            state_home.as_deref(),
            home.as_deref(),
            &std::env::temp_dir(),
            effective_uid(),
        )
    }

    fn from_dir(dir: PathBuf) -> Self {
        Self {
            lock_path: dir.join(LOCK_FILE_NAME),
            socket_path: dir.join(SOCKET_FILE_NAME),
            token_path: dir.join(TOKEN_FILE_NAME),
            dir,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn token_path(&self) -> &Path {
        &self.token_path
    }
}

/// primary 身份锁。持有期间其他进程/guard 的 `LOCK_NB` 会得到 `EWOULDBLOCK`。
pub struct InstanceLock {
    file: File,
    paths: InstancePaths,
}

impl InstanceLock {
    /// 尝试成为 primary。
    ///
    /// `Ok(None)` 表示另一个活着的 primary 持有锁；其他错误（目录被 symlink
    /// 占用、owner 不符、权限错误等）会原样返回，调用方**不应**静默降级。
    pub fn try_acquire(paths: &InstancePaths) -> io::Result<Option<Self>> {
        ensure_control_dir(paths.dir())?;
        let mut file = open_lock_file(paths.lock_path())?;
        // SAFETY: fd 来自仍然存活的 File；flock 不保留 fd 之外的指针。
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(io::Error::new(
                error.kind(),
                format!("flock {} failed: {error}", paths.lock_path().display()),
            ));
        }
        // PID 仅用于诊断；写失败不影响锁语义。
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Some(Self {
            file,
            paths: paths.clone(),
        }))
    }

    pub fn paths(&self) -> &InstancePaths {
        &self.paths
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // SAFETY: fd 在 self.file 之后才关闭，这里仍然有效。
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
        // 绝不删除 lock 文件：删除后另一个进程会创建新 inode，两个 primary
        // 会各自持有不同 inode 的锁。
    }
}

/// 在 control 连接线程上执行的 `open_window` 处理器。
pub type OpenWindowHandler =
    dyn Fn(OpenWindowRequest) -> Result<(), String> + Send + Sync + 'static;

/// primary 的 control socket 服务器。Drop 时停止 accept 线程并清理 socket。
pub struct PrimaryServer {
    _lock: InstanceLock,
    paths: InstancePaths,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    socket: Option<SocketCleanup>,
}

impl PrimaryServer {
    /// 获取 primary 锁、准备 token、绑定 control socket 并启动 accept 线程。
    ///
    /// 锁被占用时返回 [`io::ErrorKind::AlreadyExists`]，调用方应作为 secondary
    /// 转发请求。`handler` 在连接线程上运行，不能直接触碰 UI 状态。
    ///
    /// 需要**精确**区分 primary/secondary 时，先调用
    /// [`InstanceLock::try_acquire`]，再调用 [`Self::start_with_lock`]。
    #[allow(dead_code)] // 测试/脚本便捷入口；产品启动用 start_with_lock。
    pub fn start<H>(paths: InstancePaths, handler: H) -> io::Result<Self>
    where
        H: Fn(OpenWindowRequest) -> Result<(), String> + Send + Sync + 'static,
    {
        let lock = InstanceLock::try_acquire(&paths)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "another xtervo primary already holds {}",
                    paths.lock_path().display()
                ),
            )
        })?;
        Self::start_with_lock(lock, handler)
    }

    /// 用已经获取的 primary 锁启动 control server。
    ///
    /// 这让集成方可以先 `InstanceLock::try_acquire`：`Some(lock)` → primary，
    /// `None` → secondary；避免把 socket 路径异常误判成“已有 primary”。
    pub fn start_with_lock<H>(lock: InstanceLock, handler: H) -> io::Result<Self>
    where
        H: Fn(OpenWindowRequest) -> Result<(), String> + Send + Sync + 'static,
    {
        let paths = lock.paths().clone();
        let token = prepare_instance_token(paths.token_path())?;
        let bound = bind_control_socket(&paths)?;
        let cleanup = SocketCleanup {
            path: bound.path.clone(),
            id: bound.id,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handler: Arc<OpenWindowHandler> = Arc::new(handler);
        let accept_cleanup = cleanup.clone();
        let join = std::thread::Builder::new()
            .name("xtervo-instance".into())
            .spawn(move || {
                accept_loop(bound.listener, thread_stop, token, handler, accept_cleanup)
            });
        match join {
            Ok(join) => Ok(Self {
                _lock: lock,
                paths,
                stop,
                accept_thread: Some(join),
                socket: Some(cleanup),
            }),
            Err(error) => {
                // listener 已随闭包 drop；只需清掉路径。
                cleanup.remove();
                Err(error)
            }
        }
    }

    pub fn paths(&self) -> &InstancePaths {
        &self.paths
    }
}

impl Drop for PrimaryServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.accept_thread.take() {
            let _ = join.join();
        }
        if let Some(cleanup) = self.socket.take() {
            cleanup.remove();
        }
        // _lock 最后 drop，释放 flock。
    }
}

/// secondary 客户端。连接 primary、发送请求、等待一行 ack。
pub struct SecondaryClient {
    paths: InstancePaths,
    timeout: Duration,
}

impl SecondaryClient {
    pub fn new(paths: InstancePaths) -> Self {
        Self {
            paths,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// 设置连接后读写 ack 的超时（至少 1ms）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    pub fn open_window(&self, request: &OpenWindowRequest) -> Result<(), ForwardError> {
        request.validate().map_err(ForwardError::InvalidRequest)?;
        // 先 connect 再读 token：没有 primary 时返回明确的 Unavailable（而不是
        // “token 缺失”误导用户以为 primary 在运行但坏了）；连接成功但 token
        // 缺失/不安全时才报 Token。
        let socket_path = self.paths.socket_path().to_path_buf();
        let mut stream = UnixStream::connect(&socket_path).map_err(|source| {
            if is_timeout(&source) {
                ForwardError::Timeout { stage: "connect" }
            } else {
                ForwardError::Unavailable {
                    path: socket_path.clone(),
                    source,
                }
            }
        })?;
        let token = read_instance_token(self.paths.token_path()).map_err(ForwardError::Token)?;
        let _ = stream.set_read_timeout(Some(self.timeout));
        let _ = stream.set_write_timeout(Some(self.timeout));

        let wire = ClientRequest {
            token: &token,
            op: "open_window",
            request,
        };
        let mut encoded = serde_json::to_vec(&wire)
            .map_err(|error| ForwardError::Protocol(format!("encode request: {error}")))?;
        encoded.push(b'\n');
        stream
            .write_all(&encoded)
            .map_err(|error| map_io_timeout("write", error))?;
        stream
            .flush()
            .map_err(|error| map_io_timeout("write", error))?;

        let mut reader = BufReader::new(stream).take((MAX_ACK_BYTES + 1) as u64);
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| map_io_timeout("ack", error))?;
        if read == 0 {
            return Err(ForwardError::Protocol(
                "primary closed the connection without an ack".into(),
            ));
        }
        if line.len() > MAX_ACK_BYTES {
            return Err(ForwardError::Protocol("primary ack is too large".into()));
        }
        let ack: ServerAck = serde_json::from_str(line.trim())
            .map_err(|error| ForwardError::Protocol(format!("parse ack: {error}")))?;
        if ack.ok {
            if ack.op.as_deref() != Some("open_window") {
                return Err(ForwardError::Protocol(
                    "primary ack does not confirm open_window".into(),
                ));
            }
            Ok(())
        } else {
            Err(ForwardError::Rejected(
                ack.error
                    .unwrap_or_else(|| "primary rejected open_window".into()),
            ))
        }
    }
}

/// 把 `open_window` 转发给 primary；连接失败时最多重试一次（primary 启动竞态），
/// 但发送后**绝不**重试，避免重复开窗。
pub fn forward_open_window(
    paths: &InstancePaths,
    request: &OpenWindowRequest,
) -> Result<(), ForwardError> {
    forward_open_window_with_timeout(paths, request, DEFAULT_TIMEOUT)
}

/// 同 [`forward_open_window`]，但可指定等待 ack 的 timeout。
pub fn forward_open_window_with_timeout(
    paths: &InstancePaths,
    request: &OpenWindowRequest,
    timeout: Duration,
) -> Result<(), ForwardError> {
    let client = SecondaryClient::new(paths.clone()).with_timeout(timeout);
    match client.open_window(request) {
        Err(ForwardError::Unavailable { .. }) | Err(ForwardError::Token(_)) => {
            // primary 可能刚拿到锁、尚未写完 token/绑定 socket；按架构文档重试一次。
            std::thread::sleep(RETRY_DELAY);
            client.open_window(request)
        }
        result => result,
    }
}

/// secondary 转发的明确错误。任何变体都不包含 token。
#[derive(Debug)]
pub enum ForwardError {
    InvalidRequest(String),
    /// token 文件缺失、权限不安全或内容非法。
    Token(io::Error),
    /// control.sock 不存在/拒绝连接（primary 未运行或正在启动）。
    Unavailable {
        path: PathBuf,
        source: io::Error,
    },
    Timeout {
        stage: &'static str,
    },
    /// primary 明确拒绝了请求（token 错误、handler 失败等）。
    Rejected(String),
    Protocol(String),
    Io {
        stage: &'static str,
        source: io::Error,
    },
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => {
                write!(f, "invalid open_window request: {message}")
            }
            Self::Token(error) => write!(f, "xtervo instance token is unavailable: {error}"),
            Self::Unavailable { path, source } => write!(
                f,
                "xtervo primary is not running or its control socket is unavailable at {}: {source}",
                path.display()
            ),
            Self::Timeout { stage } => {
                write!(f, "timed out waiting for xtervo primary during {stage}")
            }
            Self::Rejected(message) => write!(f, "xtervo primary rejected open_window: {message}"),
            Self::Protocol(message) => write!(f, "invalid response from xtervo primary: {message}"),
            Self::Io { stage, source } => {
                write!(f, "xtervo instance IPC {stage} failed: {source}")
            }
        }
    }
}

impl std::error::Error for ForwardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Token(error) => Some(error),
            Self::Unavailable { source, .. } | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl ForwardError {
    /// 连接尚未建立（primary 还在启动 / token 尚未写好）时可以安全重试；
    /// 请求一旦发出去（Timeout/Protocol/Rejected）绝不能重试，避免重复开窗。
    pub fn is_connect_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. } | Self::Token(_))
    }
}

#[derive(Serialize)]
struct ClientRequest<'a> {
    token: &'a str,
    op: &'static str,
    #[serde(flatten)]
    request: &'a OpenWindowRequest,
}

#[derive(Deserialize)]
struct ServerRequest {
    token: String,
    op: String,
    #[serde(flatten)]
    request: OpenWindowRequest,
}

#[derive(Deserialize)]
struct ServerAck {
    ok: bool,
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Serialize)]
struct WireAck<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    op: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

#[derive(Clone)]
struct SocketCleanup {
    path: PathBuf,
    id: (u64, u64),
}

impl SocketCleanup {
    fn remove(&self) {
        remove_socket_if_same(&self.path, self.id);
    }
}

struct BoundSocket {
    listener: UnixListener,
    path: PathBuf,
    id: (u64, u64),
}

fn accept_loop(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    token: String,
    handler: Arc<OpenWindowHandler>,
    cleanup: SocketCleanup,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let token = token.clone();
                let handler = Arc::clone(&handler);
                let _ = std::thread::Builder::new()
                    .name("xtervo-instance-conn".into())
                    .spawn(move || handle_connection(stream, &token, handler.as_ref()));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                eprintln!("[instance] control socket accept failed: {error}");
                // 避免留下一个“可连接但永远不 ack”的 socket；让 secondary 得到
                // 明确的 connection refused，而不是超时。
                cleanup.remove();
                break;
            }
        }
    }
}

fn handle_connection(mut stream: UnixStream, expected_token: &str, handler: &OpenWindowHandler) {
    let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));

    let mut line = String::new();
    let read = {
        let Ok(reader_stream) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reader_stream).take((MAX_REQUEST_BYTES + 1) as u64);
        reader.read_line(&mut line)
    };
    match read {
        Ok(0) => return,
        Ok(_) => {}
        Err(error) => {
            let _ = write_ack(&mut stream, false, Some(&format!("read request: {error}")));
            return;
        }
    }
    if line.len() > MAX_REQUEST_BYTES {
        let _ = write_ack(&mut stream, false, Some("request too large"));
        return;
    }

    let request: ServerRequest = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(_) => {
            let _ = write_ack(&mut stream, false, Some("bad request: invalid JSON"));
            return;
        }
    };

    // 先认证再解释 op/参数，避免向未授权连接泄露行为细节。
    if !constant_time_eq(request.token.as_bytes(), expected_token.as_bytes()) {
        let _ = write_ack(&mut stream, false, Some("unauthorized"));
        return;
    }
    if request.op != "open_window" {
        let _ = write_ack(&mut stream, false, Some("unsupported op"));
        return;
    }
    if let Err(message) = request.request.validate() {
        let _ = write_ack(&mut stream, false, Some(&message));
        return;
    }

    match handler(request.request) {
        Ok(()) => {
            let _ = write_ack(&mut stream, true, None);
        }
        Err(message) => {
            let message = sanitize_error(&message);
            let _ = write_ack(&mut stream, false, Some(&message));
        }
    }
}

fn write_ack(stream: &mut UnixStream, ok: bool, error: Option<&str>) -> io::Result<()> {
    let ack = WireAck {
        ok,
        op: ok.then_some("open_window"),
        error,
    };
    let mut encoded = serde_json::to_vec(&ack)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    encoded.push(b'\n');
    stream.write_all(&encoded)?;
    stream.flush()
}

fn sanitize_error(message: &str) -> String {
    let cleaned: String = message
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_ERROR_CHARS)
        .collect();
    if cleaned.is_empty() {
        "open_window failed".into()
    } else {
        cleaned
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn map_io_timeout(stage: &'static str, source: io::Error) -> ForwardError {
    if is_timeout(&source) {
        ForwardError::Timeout { stage }
    } else {
        ForwardError::Io { stage, source }
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn usable_base(path: &Path) -> bool {
    path.is_absolute() && !path.as_os_str().is_empty()
}

fn socket_path_fits(dir: &Path) -> bool {
    dir.join(SOCKET_FILE_NAME).as_os_str().len() <= MAX_SOCKET_PATH_BYTES
}

fn env_absolute(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid 无参数、无副作用，只读取当前进程的 effective uid。
    unsafe { libc::geteuid() as u32 }
}

/// 创建/校验 control 目录：必须是当前 uid 拥有、非 symlink 的目录，并收紧到 0700。
fn ensure_control_dir(dir: &Path) -> io::Result<()> {
    if !dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("instance dir must be absolute: {}", dir.display()),
        ));
    }
    match fs::symlink_metadata(dir) {
        Ok(_) => secure_control_dir(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dir)?;
            secure_control_dir(dir)
        }
        Err(error) => Err(error),
    }
}

fn secure_control_dir(dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "instance dir {} is not a directory (symlinks rejected)",
                dir.display()
            ),
        ));
    }
    if metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "instance dir {} is owned by uid {}, expected {}",
                dir.display(),
                metadata.uid(),
                effective_uid()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    // chmod 后再校验一次，避免把错误目标收紧/继续使用。
    let secured = fs::symlink_metadata(dir)?;
    if !secured.file_type().is_dir()
        || secured.uid() != effective_uid()
        || secured.permissions().mode() & 0o777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("instance dir {} could not be secured", dir.display()),
        ));
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("instance lock {} is not a regular file", path.display()),
        ));
    }
    if metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "instance lock {} is owned by uid {}, expected {}",
                path.display(),
                metadata.uid(),
                effective_uid()
            ),
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn prepare_instance_token(path: &Path) -> io::Result<String> {
    let token = crate::mcp::ensure_token_file(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("create instance token {}: {error}", path.display()),
        )
    })?;
    verify_private_token_file(path)?;
    Ok(token)
}

fn read_instance_token(path: &Path) -> io::Result<String> {
    verify_private_token_file(path)?;
    crate::mcp::read_token_file(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("instance token {} is empty or malformed", path.display()),
        )
    })
}

fn verify_private_token_file(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("instance token {} is not a regular file", path.display()),
        ));
    }
    if metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "instance token {} is owned by uid {}, expected {}",
                path.display(),
                metadata.uid(),
                effective_uid()
            ),
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "instance token {} is accessible by group/other (mode {mode:o})",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn bind_control_socket(paths: &InstancePaths) -> io::Result<BoundSocket> {
    let path = paths.socket_path();
    if path.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "control socket path is longer than {MAX_SOCKET_PATH_BYTES} bytes: {}",
                path.display()
            ),
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                // 不删除非 socket 文件；可能是用户文件或被攻击者放置的 symlink。
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "refusing to replace non-socket control path {}",
                        path.display()
                    ),
                ));
            }
            match UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another process is already listening on {}", path.display()),
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    // SIGKILL 留下的 stale socket；锁已在我们手里，可以清理。
                    fs::remove_file(path)?;
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let listener = UnixListener::bind(path)?;
    let prepared = (|| -> io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("control path {} is not a socket", path.display()),
            ));
        }
        if metadata.uid() != effective_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "control socket {} is owned by uid {}, expected {}",
                    path.display(),
                    metadata.uid(),
                    effective_uid()
                ),
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("control socket {} is not 0600", path.display()),
            ));
        }
        Ok(())
    })();
    match prepared {
        Ok(()) => {
            let metadata = fs::symlink_metadata(path)?;
            Ok(BoundSocket {
                listener,
                path: path.to_path_buf(),
                id: (metadata.dev(), metadata.ino()),
            })
        }
        Err(error) => {
            drop(listener);
            let _ = fs::remove_file(path);
            Err(error)
        }
    }
}

fn remove_socket_if_same(path: &Path, id: (u64, u64)) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if (metadata.dev(), metadata.ino()) == id {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;
    use tempfile::TempDir;

    fn test_paths(root: &TempDir) -> InstancePaths {
        InstancePaths::resolve(Some(root.path()), None, None, root.path(), effective_uid())
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn two_guards_compete_and_lock_releases_on_drop() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(&root);

        let primary = InstanceLock::try_acquire(&paths)
            .expect("first acquire")
            .expect("first guard must become primary");
        assert_eq!(primary.paths(), &paths);
        assert!(paths.lock_path().exists());
        assert_eq!(mode(paths.lock_path()), 0o600);

        let secondary = InstanceLock::try_acquire(&paths).expect("second acquire");
        assert!(
            secondary.is_none(),
            "second guard must not become primary while lock is held"
        );

        drop(primary);
        let reacquired = InstanceLock::try_acquire(&paths)
            .expect("third acquire")
            .expect("lock must be released when the guard drops");
        // lock 文件保留（不 unlink），只释放 flock。
        assert!(paths.lock_path().exists());
        drop(reacquired);
    }

    #[test]
    fn secondary_forwards_open_window_and_receives_ack() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(&root);
        let (tx, rx) = mpsc::channel();
        let lock = InstanceLock::try_acquire(&paths)
            .expect("acquire primary lock")
            .expect("must become primary");
        let server = PrimaryServer::start_with_lock(lock, move |request| {
            tx.send(request)
                .map_err(|_| "test receiver gone".to_string())
        })
        .expect("start primary");

        let request = OpenWindowRequest::new()
            .with_profile("work")
            .with_cwd("/tmp");
        forward_open_window(&paths, &request).expect("forward must receive ack");
        let received = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("handler must run before ack");
        assert_eq!(received, request);
        drop(server);
    }

    #[test]
    fn wrong_token_is_rejected_without_invoking_handler() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(&root);
        let (tx, rx) = mpsc::channel();
        let server = PrimaryServer::start(paths.clone(), move |request| {
            tx.send(request)
                .map_err(|_| "test receiver gone".to_string())
        })
        .expect("start primary");

        let original = fs::read_to_string(paths.token_path()).expect("read token");
        assert!(!original.is_empty());
        let replacement = "0123456789abcdef".repeat(4);
        assert_ne!(original, replacement);
        fs::write(paths.token_path(), format!("{replacement}\n")).expect("rotate token");
        fs::set_permissions(paths.token_path(), fs::Permissions::from_mode(0o600)).unwrap();

        let error = SecondaryClient::new(paths.clone())
            .open_window(&OpenWindowRequest::default())
            .expect_err("wrong token must be rejected");
        match error {
            ForwardError::Rejected(message) => assert_eq!(message, "unauthorized"),
            other => panic!("expected unauthorized rejection, got {other:?}"),
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "handler must not run for an unauthorized request"
        );
        drop(server);
    }

    #[test]
    fn socket_is_private_and_removed_on_drop() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(&root);
        let server = PrimaryServer::start(paths.clone(), |_| Ok(())).expect("start primary");

        assert_eq!(mode(paths.dir()), 0o700);
        let socket_meta = fs::symlink_metadata(paths.socket_path()).expect("socket metadata");
        assert!(socket_meta.file_type().is_socket());
        assert_eq!(socket_meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(mode(paths.token_path()), 0o600);

        drop(server);
        assert!(
            !paths.socket_path().exists(),
            "primary Drop must remove control.sock promptly"
        );

        // 模拟 SIGKILL 后的 stale socket：bind 后直接丢 listener，不删文件。
        {
            let stale = UnixListener::bind(paths.socket_path()).expect("stale bind");
            drop(stale);
        }
        assert!(paths.socket_path().exists());
        let server = PrimaryServer::start(paths.clone(), |_| Ok(())).expect("restart primary");
        assert!(paths.socket_path().exists());
        drop(server);
        assert!(!paths.socket_path().exists());
    }

    #[test]
    fn missing_primary_reports_unavailable_without_side_effects() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(&root);
        let error = forward_open_window(&paths, &OpenWindowRequest::default())
            .expect_err("no primary must fail");
        assert!(
            error.is_connect_retryable(),
            "missing primary must be a retryable connect failure, got {error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("primary is not running"), "{message}");
        assert!(
            !paths.socket_path().exists(),
            "forwarding must not create control.sock"
        );
    }

    #[test]
    fn handler_failure_is_rejected_and_ack_is_not_ok() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(&root);
        let server = PrimaryServer::start(paths.clone(), |_request| {
            Err("window creation exploded".to_string())
        })
        .expect("start primary");

        let error = forward_open_window(&paths, &OpenWindowRequest::default())
            .expect_err("handler failure must not ack success");
        match error {
            ForwardError::Rejected(message) => {
                assert_eq!(message, "window creation exploded")
            }
            other => panic!("expected rejected handler error, got {other:?}"),
        }
        drop(server);
    }

    #[test]
    fn secondary_times_out_with_clear_error() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(&root);
        ensure_control_dir(paths.dir()).expect("control dir");
        let _token = prepare_instance_token(paths.token_path()).expect("token");
        // 接受连接但永远不 ack：connect/write 成功，read 必须超时。
        let listener = UnixListener::bind(paths.socket_path()).expect("bind");
        let client = SecondaryClient::new(paths.clone()).with_timeout(Duration::from_millis(100));
        let started = Instant::now();
        let error = client
            .open_window(&OpenWindowRequest::default())
            .expect_err("must time out");
        match error {
            ForwardError::Timeout { stage } => assert_eq!(stage, "ack"),
            other => panic!("expected timeout, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout must be bounded"
        );
        drop(listener);
    }

    #[test]
    fn paths_fall_back_without_xdg_runtime_dir() {
        let root = TempDir::new().unwrap();
        let state = root.path().join("state");
        let home = root.path().join("home");
        let temp = root.path().join("tmp");
        fs::create_dir_all(&temp).unwrap();
        let uid = 4242;

        let state_paths = InstancePaths::resolve(None, Some(&state), Some(&home), &temp, uid);
        assert_eq!(state_paths.dir(), state.join(CONTROL_DIR_NAME));
        assert_eq!(
            state_paths.socket_path(),
            state.join(CONTROL_DIR_NAME).join(SOCKET_FILE_NAME)
        );
        assert_eq!(
            state_paths.lock_path(),
            state.join(CONTROL_DIR_NAME).join(LOCK_FILE_NAME)
        );
        assert_eq!(
            state_paths.token_path(),
            state.join(CONTROL_DIR_NAME).join(TOKEN_FILE_NAME)
        );

        let home_paths = InstancePaths::resolve(None, None, Some(&home), &temp, uid);
        assert_eq!(
            home_paths.dir(),
            home.join(".local").join("state").join(CONTROL_DIR_NAME)
        );

        let temp_paths = InstancePaths::resolve(None, None, None, &temp, uid);
        assert_eq!(
            temp_paths.dir(),
            temp.join(format!("{CONTROL_DIR_NAME}-{uid}"))
        );
        assert!(temp_paths.dir().starts_with(&temp));

        // XDG_RUNTIME_DIR 存在但 socket 路径过长时，不能硬用 runtime path。
        let long_runtime = root.path().join("r".repeat(90));
        let long_paths =
            InstancePaths::resolve(Some(&long_runtime), Some(&state), Some(&home), &temp, uid);
        assert_eq!(long_paths.dir(), state.join(CONTROL_DIR_NAME));
    }

    #[test]
    fn fallback_dir_is_private_and_rejects_symlink() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join(format!("{CONTROL_DIR_NAME}-4242"));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

        ensure_control_dir(&dir).expect("existing dir must be tightened");
        assert_eq!(mode(&dir), 0o700);

        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join(format!("{CONTROL_DIR_NAME}-9999"));
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let error = ensure_control_dir(&link).expect_err("symlink must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            mode(&target),
            0o755,
            "chmod must never follow the rejected symlink"
        );
    }

    #[test]
    fn refuses_to_replace_non_socket_control_path() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(&root);
        ensure_control_dir(paths.dir()).expect("control dir");
        fs::write(paths.socket_path(), b"not a socket").expect("regular file at socket path");
        let error = match PrimaryServer::start(paths.clone(), |_| Ok(())) {
            Ok(_) => panic!("must not delete non-socket path"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(paths.socket_path().exists());
        assert_eq!(
            fs::read(paths.socket_path()).expect("user file intact"),
            b"not a socket"
        );
    }
}
