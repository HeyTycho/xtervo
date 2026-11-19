// SPDX-License-Identifier: GPL-3.0-only
//! 每个 v3 插件一个独立 supervisor 线程：
//!
//! - spawn 子进程（独立进程，stdio 管道）；
//! - `initialize` / `initialized` 握手；
//! - 处理命令、事件、状态轮询、ping、声明式 UI；
//! - 所有宿主 -> 插件请求都带 deadline，超时只影响该请求；
//! - 崩溃/坏 JSON/输出超限按 backoff 自动重启，超过上限标记 Failed；
//! - 回复插件 -> 宿主 request，并对 terminal/clipboard 做 capability gate + ask。
//!
//! supervisor 从不阻塞 UI 线程：UI 只发消息/读共享快照。任何插件故障都只在
//! 自己的 supervisor 内处理，不会影响终端或其它插件。

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use serde_json::{json, Value};

use super::manifest::{Capability, CommandDecl, Contributes, PluginEvent};
use super::protocol::{
    self, error_code, format_command_result, parse_incoming, parse_notify_payload,
    parse_status_payload, parse_surface_doc, parse_terminal_read_params,
    validate_initialize_result, Incoming, RpcError, RpcId, SurfaceDoc, MAX_LINE_BYTES,
    MAX_STDERR_TAIL_BYTES,
};
use super::{
    record_activity, ActivityLog, ApprovalRequest, HostSnapshot, Notice, StatusPayload,
    SurfaceUpdate, WakeSlot,
};

// ---------------------------------------------------------------- 配置与运行时

/// 宿主 -> 插件 stdin 写队列长度。写线程独占 pipe，supervisor 只做非阻塞入队；
/// 队列满时命令保留 pending，由 deadline 报 timeout，而不是卡住 supervisor。
const STDIN_QUEUE_CAPACITY: usize = 16;
/// 每个插件最多保留的窗口专属 status 数；其余窗口回退到全局 status。
const MAX_STATUS_WINDOWS_PER_PLUGIN: usize = 2;
/// stderr 单行进入日志/UI 前保留的可见字符数。
const MAX_STDERR_LINE_CHARS: usize = 512;
/// 未知窗口/通知错误只记录前几条，避免坏插件刷爆宿主日志。
const MAX_BAD_WINDOW_LOGS: u32 = 3;

/// 生产超时/上限；测试通过 [`SupervisorOptions`] 缩短，不改变生产默认值。
#[derive(Clone, Debug)]
pub struct SupervisorOptions {
    pub handshake_timeout: Duration,
    pub command_timeout: Duration,
    pub status_interval: Duration,
    pub status_timeout: Duration,
    pub ping_interval: Duration,
    pub ping_timeout: Duration,
    pub surface_timeout: Duration,
    pub approval_timeout: Duration,
    pub max_restarts: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    pub max_bad_lines: u32,
    pub max_line_bytes: usize,
    pub max_output_bytes: u64,
    pub max_stderr_bytes: usize,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(10),
            status_interval: Duration::from_secs(2),
            status_timeout: Duration::from_secs(2),
            ping_interval: Duration::from_secs(30),
            ping_timeout: Duration::from_secs(5),
            surface_timeout: Duration::from_secs(5),
            approval_timeout: Duration::from_secs(60),
            max_restarts: 3,
            backoff_base: Duration::from_millis(200),
            backoff_max: Duration::from_secs(2),
            max_bad_lines: 5,
            max_line_bytes: MAX_LINE_BYTES,
            max_output_bytes: protocol::MAX_OUTPUT_BYTES,
            max_stderr_bytes: MAX_STDERR_TAIL_BYTES,
        }
    }
}

/// supervisor 启动所需的不可变插件信息；Reload 时整份替换。
#[derive(Clone, Debug)]
pub struct PluginRuntimeConfig {
    pub id: String,
    pub name: String,
    pub dir: std::path::PathBuf,
    pub entry: std::path::PathBuf,
    pub contributes: Contributes,
    pub permissions: super::manifest::Permissions,
    pub command_ids: Vec<String>,
    pub command_decls: Vec<CommandDecl>,
}

impl PluginRuntimeConfig {
    pub fn allows(&self, capability: Capability) -> bool {
        self.permissions.allows(capability)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Running,
    Restarting,
    Stopped,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::Restarting => "restarting",
            Phase::Stopped => "stopped",
            Phase::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeInfo {
    pub phase: Option<Phase>,
    pub error: String,
    pub restart_count: u32,
    pub pid: Option<u32>,
}

impl RuntimeInfo {
    pub fn phase(&self) -> Phase {
        self.phase.unwrap_or(Phase::Stopped)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    Ok(String),
    Error(String),
    Timeout,
    Cancelled,
    Unavailable(String),
}

impl CommandOutcome {
    pub fn ok(&self) -> bool {
        matches!(self, CommandOutcome::Ok(_))
    }

    pub fn label(&self) -> &'static str {
        match self {
            CommandOutcome::Ok(_) => "ok",
            CommandOutcome::Error(_) => "error",
            CommandOutcome::Timeout => "timeout",
            CommandOutcome::Cancelled => "cancelled",
            CommandOutcome::Unavailable(_) => "unavailable",
        }
    }

    pub fn text(&self) -> &str {
        match self {
            CommandOutcome::Ok(text)
            | CommandOutcome::Error(text)
            | CommandOutcome::Unavailable(text) => text,
            CommandOutcome::Timeout => "命令超时",
            CommandOutcome::Cancelled => "已取消",
        }
    }
}

/// UI 线程 -> supervisor 的控制消息。
pub enum SupervisorMsg {
    Command {
        command: String,
        args: Vec<String>,
        /// 发起命令的窗口；写入 `command.invoke.window_id`。
        window_id: Option<u64>,
        reply: Sender<CommandOutcome>,
    },
    Render {
        surface: String,
        panel: Option<String>,
        /// 打开 surface 的窗口；写入 `surface.render.window_id`。
        window_id: Option<u64>,
        reply: Sender<Result<SurfaceDoc, String>>,
    },
    Event {
        event: PluginEvent,
        payload: Value,
    },
    Action {
        surface: String,
        panel: Option<String>,
        /// 发起动作的窗口；写入 `ui.action.window_id`。
        window_id: Option<u64>,
        element: String,
        action: String,
        value: Option<String>,
    },
    /// UI 对 terminal/clipboard ask 的答复。
    HostReply {
        id: u64,
        outcome: HostOutcome,
    },
    Reload(Box<PluginRuntimeConfig>),
    Restart,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostOutcome {
    Ok(Value),
    Denied,
    Unavailable(String),
    Error(String),
}

/// supervisor 与 UI 共享的只读/消息队列状态。
#[derive(Clone)]
pub struct SupervisorShared {
    pub activity: Arc<Mutex<ActivityLog>>,
    pub notices: Arc<Mutex<VecDeque<Notice>>>,
    /// key = (plugin_id, window_id)；window_id=None 表示全局状态。
    pub statuses: Arc<Mutex<HashMap<(String, Option<u64>), StatusPayload>>>,
    /// UI 线程同步的真实窗口集合；插件显式 window_id 必须命中，防止伪造 id
    /// 让 status/审批表无界增长。
    pub windows: Arc<Mutex<HashSet<u64>>>,
    pub surfaces: Arc<Mutex<VecDeque<SurfaceUpdate>>>,
    pub approvals: Arc<Mutex<VecDeque<ApprovalRequest>>>,
    pub host: Arc<Mutex<HostSnapshot>>,
    pub status_revision: Arc<AtomicU64>,
    pub runtime_revision: Arc<AtomicU64>,
    pub wake: WakeSlot,
}

/// supervisor 句柄；store 持有并用于发消息/读运行时快照。
pub struct SupervisorHandle {
    pub id: String,
    pub tx: Sender<SupervisorMsg>,
    pub runtime: Arc<Mutex<RuntimeInfo>>,
    pub stop: Arc<AtomicBool>,
}

impl SupervisorHandle {
    pub fn runtime(&self) -> RuntimeInfo {
        self.runtime
            .lock()
            .map(|info| info.clone())
            .unwrap_or_default()
    }

    pub fn phase(&self) -> Phase {
        self.runtime().phase()
    }

    pub fn is_running(&self) -> bool {
        self.phase() == Phase::Running
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(SupervisorMsg::Shutdown);
    }

    pub fn restart(&self) {
        let _ = self.tx.send(SupervisorMsg::Restart);
    }

    pub fn reload(&self, config: Box<PluginRuntimeConfig>) {
        let _ = self.tx.send(SupervisorMsg::Reload(config));
    }
}

// ---------------------------------------------------------------- 内部类型

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstanceExit {
    Shutdown,
    Restart,
    Reload,
    Crash,
    Protocol,
    CommandTimeout,
    SpawnError,
}

impl InstanceExit {
    fn retryable(self) -> bool {
        matches!(
            self,
            InstanceExit::Crash
                | InstanceExit::Protocol
                | InstanceExit::CommandTimeout
        )
    }
}

enum PendingKind {
    Initialize,
    Command(Sender<CommandOutcome>),
    Status,
    Ping,
    Surface(Sender<Result<SurfaceDoc, String>>),
    Shutdown,
}

struct Pending {
    kind: PendingKind,
    deadline: Instant,
}

struct PendingHost {
    rpc_id: RpcId,
    deadline: Instant,
    method: String,
    /// 该 ask 的目标窗口；同一窗口同方法只允许一个未决 ask（多窗口可并行）。
    window_id: Option<u64>,
}

enum ReaderEvent {
    Line { line: String, truncated: bool },
    Eof,
    OutputLimit,
    ReadError(String),
}

#[derive(Debug)]
enum StdinSendError {
    /// 有界队列已满：插件没在读 stdin，消息被丢弃（命令等 deadline 超时）。
    QueueFull,
    /// 写线程已退出（pipe 断开）；调用方应按 crash 处理。
    Closed,
    /// 单行超过协议上限 / 序列化失败（宿主侧错误）。
    TooLarge(String),
}

impl StdinSendError {
    fn message(&self, plugin_id: &str) -> String {
        match self {
            Self::QueueFull => format!(
                "插件 {plugin_id} 未读取 stdin，写队列已满（{STDIN_QUEUE_CAPACITY} 条）"
            ),
            Self::Closed => format!("插件 {plugin_id} stdin 写线程已退出"),
            Self::TooLarge(reason) => reason.clone(),
        }
    }

    fn fatal(&self) -> bool {
        !matches!(self, Self::QueueFull)
    }
}

/// 非阻塞 stdin 写入句柄。
///
/// 旧实现直接在 supervisor 线程 `write_all`：插件停读后 pipe 满，supervisor 永久
/// 阻塞，命令 deadline/Shutdown 都无法处理。现在写线程单独阻塞，supervisor 只
/// 做 `try_send`，并且先登记 pending 再入队。
struct StdinWriter {
    tx: Sender<Vec<u8>>,
}

impl StdinWriter {
    fn spawn(stdin: ChildStdin, plugin_id: &str) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(STDIN_QUEUE_CAPACITY);
        let thread_id = plugin_id.to_string();
        std::thread::Builder::new()
            .name(format!("plugin-v3-in-{plugin_id}"))
            .spawn(move || {
                let mut writer = BufWriter::new(stdin);
                while let Ok(line) = rx.recv() {
                    // 写失败通常意味着子进程已退出/关闭 stdin：直接结束写线程，
                    // 读线程会看到 EOF 并让 supervisor 走 crash 路径。
                    if writer
                        .write_all(&line)
                        .and_then(|_| writer.write_all(b"\n"))
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        eprintln!("[plugin-v3] {thread_id}: stdin 写入失败，写线程退出");
                        break;
                    }
                }
            })
            .ok();
        // `stdin` 已 move 进线程；这里无需保留 ChildStdin。
        Self { tx }
    }

    fn send(&self, value: &Value, max_line_bytes: usize) -> Result<(), StdinSendError> {
        let line = serde_json::to_vec(value)
            .map_err(|e| StdinSendError::TooLarge(format!("序列化 JSON 失败: {e}")))?;
        if line.len() > max_line_bytes {
            return Err(StdinSendError::TooLarge(format!(
                "单条消息 {} 字节超过协议上限 {max_line_bytes}",
                line.len()
            )));
        }
        match self.tx.try_send(line) {
            Ok(()) => Ok(()),
            Err(crossbeam_channel::TrySendError::Full(_)) => Err(StdinSendError::QueueFull),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => Err(StdinSendError::Closed),
        }
    }
}

struct ApprovalDraft {
    method: &'static str,
    summary: String,
    details: String,
    params: Value,
    window_id: Option<u64>,
}

// ---------------------------------------------------------------- 启动/主循环

/// 启动 supervisor 线程；返回句柄。线程名带插件 id 便于 `top`/`ps` 排查。
pub fn spawn_supervisor(
    config: PluginRuntimeConfig,
    shared: SupervisorShared,
    options: SupervisorOptions,
) -> Result<SupervisorHandle, String> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let runtime = Arc::new(Mutex::new(RuntimeInfo {
        phase: Some(Phase::Starting),
        error: String::new(),
        restart_count: 0,
        pid: None,
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let runtime_thread = runtime.clone();
    let stop_thread = stop.clone();
    let thread_name = format!("plugin-v3-{}", config.id);
    let id = config.id.clone();
    let shared_for_thread = shared.clone();
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            run_supervisor(config, rx, runtime_thread, stop_thread, shared_for_thread, options);
        })
        .map_err(|e| format!("无法创建插件 supervisor 线程: {e}"))?;
    Ok(SupervisorHandle { id, tx, runtime, stop })
}

fn run_supervisor(
    mut config: PluginRuntimeConfig,
    rx: Receiver<SupervisorMsg>,
    runtime: Arc<Mutex<RuntimeInfo>>,
    stop: Arc<AtomicBool>,
    shared: SupervisorShared,
    options: SupervisorOptions,
) {
    let mut restart_count = 0u32;
    let mut last_error = String::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            publish_runtime(&runtime, &shared, Some(Phase::Stopped), "", restart_count, None);
            break;
        }
        publish_runtime(
            &runtime,
            &shared,
            Some(if restart_count == 0 { Phase::Starting } else { Phase::Restarting }),
            if restart_count == 0 { "" } else { &last_error },
            restart_count,
            None,
        );
        let exit = run_instance(
            &mut config,
            &rx,
            &runtime,
            &stop,
            &shared,
            &options,
            restart_count,
        );
        match exit {
            InstanceExit::Shutdown => {
                publish_runtime(&runtime, &shared, Some(Phase::Stopped), "", restart_count, None);
                break;
            },
            InstanceExit::Reload => {
                restart_count = 0;
                last_error.clear();
                eprintln!("[plugin-v3] {}: reload，重启插件进程", config.id);
                continue;
            },
            InstanceExit::Restart => {
                restart_count = 0;
                last_error.clear();
                eprintln!("[plugin-v3] {}: 手动 restart", config.id);
                continue;
            },
            InstanceExit::SpawnError => {
                let error = take_runtime_error(&runtime);
                publish_runtime(
                    &runtime,
                    &shared,
                    Some(Phase::Failed),
                    &error,
                    restart_count,
                    None,
                );
                drain_unavailable(&rx, "插件启动失败");
                break;
            },
            exit if exit.retryable() => {
                let error = take_runtime_error(&runtime);
                last_error = error.clone();
                restart_count = restart_count.saturating_add(1);
                if restart_count > options.max_restarts {
                    let message = format!(
                        "{}；连续失败，已停止自动重启（上限 {}）。可点 Restart 或 Reload 重试。",
                        error, options.max_restarts
                    );
                    eprintln!("[plugin-v3] {}: {message}", config.id);
                    record_activity(
                        &shared,
                        &config.id,
                        "supervisor",
                        "failed",
                        &message,
                    );
                    publish_runtime(
                        &runtime,
                        &shared,
                        Some(Phase::Failed),
                        &message,
                        restart_count,
                        None,
                    );
                    drain_unavailable(&rx, "插件已失败");
                    break;
                }
                let backoff = backoff_delay(&options, restart_count);
                eprintln!(
                    "[plugin-v3] {}: {}；{}ms 后重启（{}/{}）",
                    config.id,
                    error,
                    backoff.as_millis(),
                    restart_count,
                    options.max_restarts
                );
                record_activity(
                    &shared,
                    &config.id,
                    "supervisor",
                    "restart",
                    &format!("{}（{restart_count}/{}）", clip(&error, 120), options.max_restarts),
                );
                publish_runtime(
                    &runtime,
                    &shared,
                    Some(Phase::Restarting),
                    &error,
                    restart_count,
                    None,
                );
                if !backoff_or_control(
                    backoff,
                    &mut config,
                    &mut restart_count,
                    &rx,
                    &stop,
                    &runtime,
                    &shared,
                ) {
                    publish_runtime(
                        &runtime,
                        &shared,
                        Some(Phase::Stopped),
                        "",
                        restart_count,
                        None,
                    );
                    break;
                }
                continue;
            },
            _ => {
                publish_runtime(&runtime, &shared, Some(Phase::Stopped), "", restart_count, None);
                break;
            },
        }
    }
    let _ = rx; // 线程退出后 sender 自然断开
}

fn backoff_delay(options: &SupervisorOptions, restart_count: u32) -> Duration {
    let factor = 1u32 << restart_count.saturating_sub(1).min(5);
    (options.backoff_base * factor).min(options.backoff_max)
}

/// 返回 false = 收到 Shutdown/stop；true = 继续重启（可能已 Reload 新配置）。
fn backoff_or_control(
    duration: Duration,
    config: &mut PluginRuntimeConfig,
    restart_count: &mut u32,
    rx: &Receiver<SupervisorMsg>,
    stop: &AtomicBool,
    runtime: &Arc<Mutex<RuntimeInfo>>,
    shared: &SupervisorShared,
) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        match rx.try_recv() {
            Ok(SupervisorMsg::Shutdown) => return false,
            Ok(SupervisorMsg::Reload(new_config)) => {
                *config = *new_config;
                *restart_count = 0;
                return true;
            },
            Ok(SupervisorMsg::Restart) => {
                *restart_count = 0;
                return true;
            },
            Ok(SupervisorMsg::Command { reply, .. }) => {
                let _ = reply.send(CommandOutcome::Unavailable(
                    "插件正在重启，命令未执行".to_string(),
                ));
            },
            Ok(SupervisorMsg::Render { reply, .. }) => {
                let _ = reply.send(Err("插件正在重启".to_string()));
            },
            Ok(SupervisorMsg::Event { .. })
            | Ok(SupervisorMsg::Action { .. })
            | Ok(SupervisorMsg::HostReply { .. }) => {},
            Err(crossbeam_channel::TryRecvError::Empty) => {},
            Err(crossbeam_channel::TryRecvError::Disconnected) => return false,
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = shared;
    let _ = runtime;
    true
}

fn drain_unavailable(rx: &Receiver<SupervisorMsg>, reason: &str) {
    while let Ok(message) = rx.try_recv() {
        match message {
            SupervisorMsg::Command { reply, .. } => {
                let _ = reply.send(CommandOutcome::Unavailable(reason.to_string()));
            },
            SupervisorMsg::Render { reply, .. } => {
                let _ = reply.send(Err(reason.to_string()));
            },
            _ => {},
        }
    }
}

fn take_runtime_error(runtime: &Arc<Mutex<RuntimeInfo>>) -> String {
    runtime
        .lock()
        .ok()
        .map(|info| info.error.clone())
        .filter(|error| !error.trim().is_empty())
        .unwrap_or_else(|| "插件进程异常退出".to_string())
}

// ---------------------------------------------------------------- 单次进程生命周期

fn run_instance(
    config: &mut PluginRuntimeConfig,
    rx: &Receiver<SupervisorMsg>,
    runtime: &Arc<Mutex<RuntimeInfo>>,
    stop: &AtomicBool,
    shared: &SupervisorShared,
    options: &SupervisorOptions,
    restart_count: u32,
) -> InstanceExit {
    let mut child = match spawn_child(config) {
        Ok(child) => child,
        Err(e) => {
            let message = format!("启动插件失败: {e}");
            set_runtime_error(runtime, &message);
            shared.runtime_revision.fetch_add(1, Ordering::SeqCst);
            super::notify_wake(&shared.wake);
            record_activity(shared, &config.id, "supervisor", "spawn-error", &message);
            return InstanceExit::SpawnError;
        },
    };
    let pid = child.id();
    publish_runtime(runtime, shared, Some(Phase::Starting), "", restart_count, Some(pid));

    let Some(stdin) = child.stdin.take() else {
        let message = "无法获取插件 stdin".to_string();
        kill_child(&mut child);
        set_runtime_error(runtime, &message);
        return InstanceExit::SpawnError;
    };
    let Some(stdout) = child.stdout.take() else {
        let message = "无法获取插件 stdout".to_string();
        kill_child(&mut child);
        set_runtime_error(runtime, &message);
        return InstanceExit::SpawnError;
    };
    let Some(stderr) = child.stderr.take() else {
        let message = "无法获取插件 stderr".to_string();
        kill_child(&mut child);
        set_runtime_error(runtime, &message);
        return InstanceExit::SpawnError;
    };

    let writer = StdinWriter::spawn(stdin, &config.id);
    let (reader_tx, reader_rx) = crossbeam_channel::unbounded();
    let max_line = options.max_line_bytes;
    let max_output = options.max_output_bytes;
    std::thread::Builder::new()
        .name(format!("plugin-v3-out-{}", config.id))
        .spawn(move || read_stdout(stdout, reader_tx, max_line, max_output))
        .ok();
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let stderr_tail_thread = stderr_tail.clone();
    let stderr_cap = options.max_stderr_bytes;
    let stderr_id = config.id.clone();
    std::thread::Builder::new()
        .name(format!("plugin-v3-err-{}", config.id))
        .spawn(move || read_stderr(stderr, stderr_tail_thread, stderr_cap, &stderr_id))
        .ok();

    // 握手：initialize request(id=1)。
    let init_id = 1u64;
    let next_id = Arc::new(AtomicU64::new(2));
    let initialize_params = json!({
        "protocol_version": protocol::HOST_PROTOCOL_VERSION,
        "host": {
            "name": "tervo",
            "version": env!("CARGO_PKG_VERSION"),
            "abi": super::manifest::HOST_ABI,
        },
        "plugin": { "id": config.id, "name": config.name },
        "permissions": {
            "granted": config.permissions.names(),
            "terminal_read": "ask",
            "terminal_write": "ask",
            "clipboard": if config.permissions.clipboard { "ask" } else { "denied" },
            "credentials": "never-host-provided",
        },
        "contributes": {
            "commands": config.command_ids,
            "status": config.contributes.status,
            "events": config.contributes.events,
            "panels": config.contributes.panels,
            "settings": config.contributes.settings,
            "assistant": config.contributes.assistant_enabled(),
        },
    });
    // pending 必须先登记再入队：写线程/队列任一环节卡住时，命令 deadline 仍然生效。
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    pending.insert(
        init_id,
        Pending {
            kind: PendingKind::Initialize,
            deadline: Instant::now() + options.handshake_timeout,
        },
    );
    if let Err(e) = writer.send(
        &protocol::request(init_id, protocol::M_INITIALIZE, initialize_params),
        options.max_line_bytes,
    ) {
        let message = format!("写入 initialize 失败: {}", e.message(&config.id));
        set_runtime_error(runtime, &message);
        kill_child(&mut child);
        return InstanceExit::Crash;
    }

    let mut pending_host: HashMap<u64, PendingHost> = HashMap::new();
    let mut initialized = false;
    let mut shutting_down: Option<Instant> = None;
    let mut bad_lines = 0u32;
    let mut bad_plugin_logs = 0u32;
    let mut status_failures = 0u32;
    let mut ping_failures = 0u32;
    let mut next_status = Instant::now() + options.status_interval;
    let mut next_ping = Instant::now() + options.ping_interval;
    let mut notify_window: VecDeque<Instant> = VecDeque::new();

    let exit = loop {
        if stop.load(Ordering::SeqCst) && shutting_down.is_none() {
            if !initialized {
                break InstanceExit::Shutdown;
            }
            begin_shutdown(
                &writer,
                &mut pending,
                &next_id,
                options.max_line_bytes,
                &mut shutting_down,
            );
        }
        let now = Instant::now();
        if let Some(deadline) = shutting_down {
            if now >= deadline {
                break InstanceExit::Shutdown;
            }
        }

        // 计算最近 deadline，最多等 50ms，保证 stop/timer 响应及时。
        let mut next_deadline = now + Duration::from_millis(50);
        for p in pending.values() {
            next_deadline = next_deadline.min(p.deadline);
        }
        for p in pending_host.values() {
            next_deadline = next_deadline.min(p.deadline);
        }
        if initialized && shutting_down.is_none() {
            if config.contributes.status {
                next_deadline = next_deadline.min(next_status);
            }
            next_deadline = next_deadline.min(next_ping);
        }
        let timeout = next_deadline.saturating_duration_since(now);

        crossbeam_channel::select! {
            recv(rx) -> message => {
                match message {
                    Ok(SupervisorMsg::Shutdown) => {
                        if initialized {
                            begin_shutdown(
                                &writer,
                                &mut pending,
                                &next_id,
                                options.max_line_bytes,
                                &mut shutting_down,
                            );
                        } else {
                            break InstanceExit::Shutdown;
                        }
                    },
                    Ok(SupervisorMsg::Restart) => break InstanceExit::Restart,
                    Ok(SupervisorMsg::Reload(new_config)) => {
                        *config = *new_config;
                        break InstanceExit::Reload;
                    },
                    Ok(SupervisorMsg::Command { command, args, window_id, reply }) => {
                        if !initialized || shutting_down.is_some() {
                            let _ = reply.send(CommandOutcome::Unavailable(
                                "插件尚未就绪或正在关闭".to_string(),
                            ));
                            continue;
                        }
                        let id = next_request_id(&next_id);
                        let params = json!({
                            "command": command,
                            "args": args,
                            "window_id": window_id,
                            "context": {
                                "host_version": env!("CARGO_PKG_VERSION"),
                                "api": super::manifest::PLUGIN_API_VERSION,
                                "window_id": window_id,
                            },
                        });
                        // pending 先登记，再入队；队列满时保留 pending 让 deadline
                        // 报 timeout，而不是让 supervisor 等写线程。
                        pending.insert(id, Pending {
                            kind: PendingKind::Command(reply),
                            deadline: Instant::now() + options.command_timeout,
                        });
                        if let Err(e) = writer.send(
                            &protocol::request(id, protocol::M_COMMAND_INVOKE, params),
                            options.max_line_bytes,
                        ) {
                            eprintln!(
                                "[plugin-v3] {}: 命令入队失败: {}",
                                config.id,
                                e.message(&config.id)
                            );
                            if e.fatal() {
                                break InstanceExit::Crash;
                            }
                        }
                    },
                    Ok(SupervisorMsg::Event { event, payload }) => {
                        if !initialized || shutting_down.is_some() {
                            continue;
                        }
                        let params = protocol::event_params(event, &payload);
                        if let Err(e) = writer.send(
                            &protocol::notification(protocol::M_EVENT_DELIVER, params),
                            options.max_line_bytes,
                        ) {
                            // 事件是可丢的：队列满时直接放弃，不能让宿主阻塞。
                            if e.fatal() {
                                break InstanceExit::Crash;
                            }
                        }
                    },
                    Ok(SupervisorMsg::Action { surface, panel, window_id, element, action, value }) => {
                        if !initialized || shutting_down.is_some() { continue; }
                        let mut params = json!({
                            "surface": surface,
                            "window_id": window_id,
                            "element": element,
                            "action": action,
                        });
                        if let Some(panel) = panel { params["panel"] = json!(panel); }
                        if let Some(value) = value { params["value"] = json!(value); }
                        if let Err(e) = writer.send(
                            &protocol::notification(protocol::M_UI_ACTION, params),
                            options.max_line_bytes,
                        ) {
                            if e.fatal() {
                                break InstanceExit::Crash;
                            }
                        }
                    },
                    Ok(SupervisorMsg::Render { surface, panel, window_id, reply }) => {
                        if !initialized || shutting_down.is_some() {
                            let _ = reply.send(Err("插件尚未就绪或正在关闭".to_string()));
                            continue;
                        }
                        let id = next_request_id(&next_id);
                        let mut params = json!({ "surface": surface, "window_id": window_id });
                        if let Some(panel) = panel.clone() { params["panel"] = json!(panel); }
                        pending.insert(id, Pending {
                            kind: PendingKind::Surface(reply),
                            deadline: Instant::now() + options.surface_timeout,
                        });
                        if let Err(e) = writer.send(
                            &protocol::request(id, protocol::M_SURFACE_RENDER, params),
                            options.max_line_bytes,
                        ) {
                            eprintln!(
                                "[plugin-v3] {}: surface.render 入队失败: {}",
                                config.id,
                                e.message(&config.id)
                            );
                            if e.fatal() {
                                break InstanceExit::Crash;
                            }
                        }
                    },
                    Ok(SupervisorMsg::HostReply { id, outcome }) => {
                        let Some(host) = pending_host.remove(&id) else {
                            eprintln!("[plugin-v3] {}: 忽略未知 host reply id={id}", config.id);
                            continue;
                        };
                        let response = match outcome {
                            HostOutcome::Ok(value) => protocol::result(&host.rpc_id, value),
                            HostOutcome::Denied => protocol::error(
                                &host.rpc_id,
                                &RpcError::new(error_code::DENIED_BY_USER, "denied by user"),
                            ),
                            HostOutcome::Unavailable(message) => protocol::error(
                                &host.rpc_id,
                                &RpcError::new(error_code::UNAVAILABLE, message),
                            ),
                            HostOutcome::Error(message) => protocol::error(
                                &host.rpc_id,
                                &RpcError::new(error_code::INTERNAL_ERROR, message),
                            ),
                        };
                        remove_approval(shared, &config.id, id);
                        if let Err(e) = writer.send(&response, options.max_line_bytes) {
                            eprintln!(
                                "[plugin-v3] {}: host reply 入队失败: {}",
                                config.id,
                                e.message(&config.id)
                            );
                            if e.fatal() {
                                break InstanceExit::Crash;
                            }
                        }
                    },
                    Err(_) => {
                        break InstanceExit::Shutdown;
                    },
                }
            },
            recv(reader_rx) -> event => {
                match event {
                    Ok(ReaderEvent::Line { line, truncated }) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if truncated {
                            bad_lines += 1;
                            let message = format!("插件输出超长行（上限 {} 字节）", options.max_line_bytes);
                            set_runtime_error(runtime, &message);
                            eprintln!("[plugin-v3] {}: {message}", config.id);
                            if bad_lines > options.max_bad_lines {
                                break InstanceExit::Protocol;
                            }
                            continue;
                        }
                        let incoming = parse_incoming(&line);
                        match incoming {
                            Incoming::Invalid(reason) => {
                                bad_lines += 1;
                                let message = format!("坏 JSON/协议行（{bad_lines}/{}）: {reason}", options.max_bad_lines);
                                set_runtime_error(runtime, &message);
                                eprintln!("[plugin-v3] {}: {message}", config.id);
                                if bad_lines > options.max_bad_lines {
                                    break InstanceExit::Protocol;
                                }
                            },
                            Incoming::Response { id, result, error } => {
                                let Some(request) = pending.remove(&id) else {
                                    eprintln!("[plugin-v3] {}: 忽略未知 response id={id}", config.id);
                                    continue;
                                };
                                match request.kind {
                                    PendingKind::Initialize => {
                                        if let Some(error) = error {
                                            let message = format!(
                                                "initialize 失败: {} ({})",
                                                sanitize_plugin_text(&error.message, 400),
                                                error.code
                                            );
                                            set_runtime_error(runtime, &message);
                                            break InstanceExit::Protocol;
                                        }
                                        let result = result.unwrap_or(Value::Null);
                                        if let Err(message) = validate_initialize_result(&result) {
                                            set_runtime_error(runtime, &message);
                                            break InstanceExit::Protocol;
                                        }
                                        initialized = true;
                                        if let Err(e) = writer.send(
                                            &protocol::notification(protocol::M_INITIALIZED, json!({})),
                                            options.max_line_bytes,
                                        ) {
                                            eprintln!(
                                                "[plugin-v3] {}: 写入 initialized 失败: {}",
                                                config.id,
                                                e.message(&config.id)
                                            );
                                            break InstanceExit::Crash;
                                        }
                                        publish_runtime(
                                            runtime,
                                            shared,
                                            Some(Phase::Running),
                                            "",
                                            restart_count,
                                            Some(pid),
                                        );
                                        eprintln!(
                                            "[plugin-v3] {}: initialize/initialized 完成（pid={pid}）",
                                            config.id
                                        );
                                        record_activity(shared, &config.id, "lifecycle", "running", "initialize/initialized 完成");
                                        next_status = Instant::now();
                                        next_ping = Instant::now() + options.ping_interval;
                                    },
                                    PendingKind::Command(reply) => {
                                        let outcome = match (result, error) {
                                            (_, Some(error)) => CommandOutcome::Error(format!(
                                                "{} (code {})",
                                                sanitize_plugin_text(&error.message, 400),
                                                error.code
                                            )),
                                            (Some(result), None) => {
                                                let (ok, text) = format_command_result(&result);
                                                if ok { CommandOutcome::Ok(text) } else { CommandOutcome::Error(text) }
                                            },
                                            (None, None) => CommandOutcome::Error("插件返回空响应".into()),
                                        };
                                        let _ = reply.send(outcome);
                                    },
                                    PendingKind::Status => {
                                        match result.as_ref().map(parse_status_payload) {
                                            Some(Ok(status)) => {
                                                status_failures = 0;
                                                // status.get 是宿主轮询：没有显式 window_id 时
                                                // 必须保持全局（None），不能回退 active。
                                                let window = match status.window_id {
                                                    Some(raw) => match resolve_window_id(shared, Some(raw)) {
                                                        Ok(window) => window,
                                                        Err(error) => {
                                                            if bad_plugin_logs < MAX_BAD_WINDOW_LOGS {
                                                                eprintln!(
                                                                    "[plugin-v3] {}: 丢弃 status.get 结果: {error}",
                                                                    config.id
                                                                );
                                                                bad_plugin_logs += 1;
                                                            }
                                                            continue;
                                                        },
                                                    },
                                                    None => None,
                                                };
                                                publish_status(shared, &config.id, window, Some(status));
                                            },
                                            Some(Err(e)) => {
                                                eprintln!(
                                                    "[plugin-v3] {}: status.get 结果非法: {}",
                                                    config.id,
                                                    sanitize_plugin_text(&e, 400)
                                                );
                                            },
                                            None => {
                                                let message = error.map(|e| e.message).unwrap_or_else(|| "空响应".into());
                                                eprintln!(
                                                    "[plugin-v3] {}: status.get 失败: {}",
                                                    config.id,
                                                    sanitize_plugin_text(&message, 400)
                                                );
                                            },
                                        }
                                    },
                                    PendingKind::Ping => {
                                        if let Some(error) = error {
                                            ping_failures += 1;
                                            eprintln!(
                                                "[plugin-v3] {}: ping 失败: {} ({ping_failures}/2)",
                                                config.id,
                                                sanitize_plugin_text(&error.message, 400)
                                            );
                                        } else {
                                            ping_failures = 0;
                                        }
                                    },
                                    PendingKind::Surface(reply) => {
                                        let parsed = match (result, error) {
                                            (_, Some(error)) => Err(format!(
                                                "{} ({})",
                                                sanitize_plugin_text(&error.message, 400),
                                                error.code
                                            )),
                                            (Some(result), None) => parse_surface_doc(&result),
                                            (None, None) => Err("插件返回空 UI 文档".into()),
                                        };
                                        let _ = reply.send(parsed);
                                    },
                                    PendingKind::Shutdown => {
                                        break InstanceExit::Shutdown;
                                    },
                                }
                            },
                            Incoming::Request { id, method, params } => {
                                let Some(id_num) = id.as_u64() else {
                                    // 需要异步 ask 的协议要求数字 id；同步应答仍回原 id。
                                    let response = protocol::error(
                                        &id,
                                        &RpcError::new(error_code::INVALID_REQUEST, "plugin -> host request id 必须是 u64"),
                                    );
                                    if writer.send(&response, options.max_line_bytes).is_err() {
                                        break InstanceExit::Crash;
                                    }
                                    continue;
                                };
                                match handle_host_request(
                                    id_num,
                                    id,
                                    &method,
                                    &params,
                                    config,
                                    shared,
                                    options,
                                    &mut pending_host,
                                    &mut notify_window,
                                    initialized,
                                ) {
                                    HostRequestAction::Respond(value) => {
                                        if writer.send(&value, options.max_line_bytes).is_err() {
                                            break InstanceExit::Crash;
                                        }
                                    },
                                    HostRequestAction::Pending => {},
                                    HostRequestAction::Crash => break InstanceExit::Crash,
                                }
                            },
                            Incoming::Notification { method, params } => {
                                if !initialized {
                                    continue;
                                }
                                if let Err(e) = handle_host_notification(
                                    &method,
                                    &params,
                                    config,
                                    shared,
                                    &mut notify_window,
                                ) {
                                    if bad_plugin_logs < MAX_BAD_WINDOW_LOGS {
                                        eprintln!(
                                            "[plugin-v3] {}: 忽略通知 {method}: {e}",
                                            config.id
                                        );
                                        bad_plugin_logs += 1;
                                    }
                                }
                            },
                        }
                    },
                    Ok(ReaderEvent::Eof) => {
                        let code = child.try_wait().ok().flatten().and_then(|s| s.code());
                        let message = format!(
                            "插件进程退出（{}）",
                            code.map(|code| format!("code={code}"))
                                .unwrap_or_else(|| "EOF/无退出码".to_string())
                        );
                        set_runtime_error(runtime, &message);
                        break InstanceExit::Crash;
                    },
                    Ok(ReaderEvent::OutputLimit) => {
                        let message = format!(
                            "插件 stdout 超过输出上限（{} MiB），进程已停止",
                            options.max_output_bytes / (1024 * 1024)
                        );
                        set_runtime_error(runtime, &message);
                        break InstanceExit::Protocol;
                    },
                    Ok(ReaderEvent::ReadError(e)) => {
                        set_runtime_error(runtime, &format!("读取插件 stdout 失败: {e}"));
                        break InstanceExit::Crash;
                    },
                    Err(_) => {
                        break InstanceExit::Crash;
                    },
                }
            },
            default(timeout) => {},
        }

        // ---- 定时器 ----
        let now = Instant::now();
        if initialized && shutting_down.is_none() {
            if config.contributes.status
                && now >= next_status
                && !pending.values().any(|p| matches!(p.kind, PendingKind::Status))
            {
                next_status = now + options.status_interval;
                let id = next_request_id(&next_id);
                pending.insert(id, Pending {
                    kind: PendingKind::Status,
                    deadline: now + options.status_timeout,
                });
                if let Err(e) = writer.send(
                    &protocol::request(id, protocol::M_STATUS_GET, json!({})),
                    options.max_line_bytes,
                ) {
                    eprintln!(
                        "[plugin-v3] {}: status.get 入队失败: {}",
                        config.id,
                        e.message(&config.id)
                    );
                    if e.fatal() {
                        break InstanceExit::Crash;
                    }
                }
            }
            if now >= next_ping {
                next_ping = now + options.ping_interval;
                let id = next_request_id(&next_id);
                pending.insert(id, Pending {
                    kind: PendingKind::Ping,
                    deadline: now + options.ping_timeout,
                });
                if let Err(e) = writer.send(
                    &protocol::request(id, protocol::M_PING, json!({ "seq": id })),
                    options.max_line_bytes,
                ) {
                    eprintln!(
                        "[plugin-v3] {}: ping 入队失败: {}",
                        config.id,
                        e.message(&config.id)
                    );
                    if e.fatal() {
                        break InstanceExit::Crash;
                    }
                }
            }
        }

        // pending deadline
        let mut timed_out_command = false;
        let mut timed_out_surface: Vec<Sender<Result<SurfaceDoc, String>>> = Vec::new();
        let mut timed_out_host: Vec<u64> = Vec::new();
        let mut expired_status = false;
        let mut expired_init = false;
        let now = Instant::now();
        let expired_ids: Vec<u64> = pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired_ids {
            let Some(request) = pending.remove(&id) else { continue };
            match request.kind {
                PendingKind::Initialize => {
                    expired_init = true;
                },
                PendingKind::Command(reply) => {
                    timed_out_command = true;
                    let _ = reply.send(CommandOutcome::Timeout);
                },
                PendingKind::Status => {
                    expired_status = true;
                    status_failures += 1;
                },
                PendingKind::Ping => {
                    ping_failures += 1;
                },
                PendingKind::Surface(reply) => timed_out_surface.push(reply),
                PendingKind::Shutdown => {},
            }
        }
        for reply in timed_out_surface {
            let _ = reply.send(Err("surface.render 超时".to_string()));
        }
        for (id, host) in pending_host.iter() {
            if host.deadline <= now {
                timed_out_host.push(*id);
            }
        }
        let mut host_write_failed = false;
        for id in timed_out_host {
            if let Some(host) = pending_host.remove(&id) {
                remove_approval(shared, &config.id, id);
                let response = protocol::error(
                    &host.rpc_id,
                    &RpcError::new(error_code::REQUEST_TIMEOUT, "等待用户确认超时"),
                );
                if writer.send(&response, options.max_line_bytes).is_err() {
                    host_write_failed = true;
                    break;
                }
            }
        }
        if host_write_failed {
            break InstanceExit::Crash;
        }
        if expired_status {
            eprintln!(
                "[plugin-v3] {}: status.get 超时（{status_failures} 次；不终止进程）",
                config.id
            );
        }
        if expired_init {
            let message = format!(
                "initialize 握手超时（{}ms）",
                options.handshake_timeout.as_millis()
            );
            set_runtime_error(runtime, &message);
            break InstanceExit::Protocol;
        }
        if ping_failures >= 2 {
            let message = "连续 ping 超时，插件无响应".to_string();
            set_runtime_error(runtime, &message);
            break InstanceExit::Protocol;
        }
        if timed_out_command {
            let message = format!(
                "命令超时（{}s），重启插件进程",
                options.command_timeout.as_secs()
            );
            set_runtime_error(runtime, &message);
            break InstanceExit::CommandTimeout;
        }
        if shutting_down.is_some() && now >= shutting_down.unwrap() {
            break InstanceExit::Shutdown;
        }
    };

    // 清理本次实例：回复所有未完成请求，移除审批，杀掉子进程。
    fail_pending(&mut pending, "插件进程已退出");
    for (id, host) in pending_host.drain() {
        remove_approval(shared, &config.id, id);
        // 关闭路径只做非阻塞入队；写线程可能仍卡在旧 pipe 上，不能等它。
        let _ = writer.send(
            &protocol::error(
                &host.rpc_id,
                &RpcError::new(error_code::UNAVAILABLE, "插件进程已退出"),
            ),
            options.max_line_bytes,
        );
    }
    clear_plugin_status(shared, &config.id);
    kill_child(&mut child);
    let tail = stderr_tail.lock().map(|tail| tail.clone()).unwrap_or_default();
    if !tail.trim().is_empty() {
        append_runtime_error(runtime, &format!("stderr: {}", clip(&tail, 600)));
    }
    exit
}

fn begin_shutdown(
    writer: &StdinWriter,
    pending: &mut HashMap<u64, Pending>,
    next_id: &Arc<AtomicU64>,
    max_line: usize,
    shutting_down: &mut Option<Instant>,
) {
    if shutting_down.is_some() {
        return;
    }
    let id = next_request_id(next_id);
    // 关闭请求也只入队，不等写线程；1s 后无论是否写成功都走 kill 路径。
    pending.insert(id, Pending {
        kind: PendingKind::Shutdown,
        deadline: Instant::now() + Duration::from_secs(1),
    });
    let _ = writer.send(
        &protocol::request(id, protocol::M_SHUTDOWN, json!({})),
        max_line,
    );
    *shutting_down = Some(Instant::now() + Duration::from_secs(1));
}

fn fail_pending(pending: &mut HashMap<u64, Pending>, reason: &str) {
    for (_, request) in pending.drain() {
        match request.kind {
            PendingKind::Command(reply) => {
                let _ = reply.send(CommandOutcome::Unavailable(reason.to_string()));
            },
            PendingKind::Surface(reply) => {
                let _ = reply.send(Err(reason.to_string()));
            },
            PendingKind::Initialize
            | PendingKind::Status
            | PendingKind::Ping
            | PendingKind::Shutdown => {},
        }
    }
}

fn next_request_id(next_id: &Arc<AtomicU64>) -> u64 {
    next_id.fetch_add(1, Ordering::SeqCst)
}

// ---------------------------------------------------------------- 插件 -> 宿主

enum HostRequestAction {
    Respond(Value),
    Pending,
    Crash,
}

#[allow(clippy::too_many_arguments)]
fn handle_host_request(
    id_num: u64,
    rpc_id: RpcId,
    method: &str,
    params: &Value,
    config: &PluginRuntimeConfig,
    shared: &SupervisorShared,
    options: &SupervisorOptions,
    pending_host: &mut HashMap<u64, PendingHost>,
    notify_window: &mut VecDeque<Instant>,
    initialized: bool,
) -> HostRequestAction {
    macro_rules! deny {
        ($code:expr, $message:expr $(,)?) => {{
            let message: &str = $message;
            record_activity(shared, &config.id, method, "denied", message);
            eprintln!("[plugin-v3] {}: 拒绝 {method}: {message}", config.id);
            HostRequestAction::Respond(protocol::error(
                &rpc_id,
                &RpcError::new($code, message.to_string()),
            ))
        }};
    }
    if !initialized
        && !matches!(
            method,
            protocol::M_CONTEXT_VERSION | protocol::M_CONTEXT_CONFIG_METADATA
        )
    {
        return deny!(
            error_code::UNAVAILABLE,
            "插件尚未完成 initialize/initialized",
        );
    }
    // additive：任何插件 -> 宿主方法都可带 window_id；缺省回退 active。
    // 显式 id 必须命中 UI 线程同步过来的真实窗口集合，否则立即拒绝：不能让伪造
    // id 占用审批队列或 status map。
    let window_id = match parse_and_resolve_window(shared, params) {
        Ok(raw) => raw,
        Err(error) => {
            record_activity(shared, &config.id, method, "denied", &error);
            return HostRequestAction::Respond(protocol::error(
                &rpc_id,
                &RpcError::new(error_code::INVALID_PARAMS, format!("{method}.{error}")),
            ));
        },
    };
    match method {
        protocol::M_UI_NOTIFY => {
            let payload = match parse_notify_payload(params) {
                Ok(payload) => payload,
                Err(e) => {
                    return HostRequestAction::Respond(protocol::error(
                        &rpc_id,
                        &RpcError::new(error_code::INVALID_PARAMS, e),
                    ))
                },
            };
            if !allow_notify(notify_window) {
                return HostRequestAction::Respond(protocol::error(
                    &rpc_id,
                    &RpcError::new(error_code::RATE_LIMITED, "ui.notify 频率过高"),
                ));
            }
            push_notice(shared, Notice {
                plugin_id: config.id.clone(),
                plugin_name: config.name.clone(),
                text: payload.text.clone(),
                level: payload.level,
                window_id,
            });
            record_activity(
                shared,
                &config.id,
                "ui.notify",
                "ok",
                &format!("window={window_id:?} {}", payload.text),
            );
            HostRequestAction::Respond(protocol::result(&rpc_id, json!({})))
        },
        protocol::M_UI_STATUS => {
            if !config.contributes.status {
                return deny!(
                    error_code::PERMISSION_DENIED,
                    "未声明 contributes.status，不能推送 ui.status",
                );
            }
            let status = match parse_status_payload(params) {
                Ok(status) => status,
                Err(e) => {
                    return HostRequestAction::Respond(protocol::error(
                        &rpc_id,
                        &RpcError::new(error_code::INVALID_PARAMS, e),
                    ))
                },
            };
            if let Some(command) = &status.command {
                if !config.command_ids.iter().any(|id| id == command) {
                    return HostRequestAction::Respond(protocol::error(
                        &rpc_id,
                        &RpcError::new(
                            error_code::INVALID_PARAMS,
                            format!("status.command {command:?} 未在 [[commands]] 声明"),
                        ),
                    ));
                }
            }
            publish_status(shared, &config.id, window_id, Some(status));
            record_activity(
                shared,
                &config.id,
                "ui.status",
                "ok",
                &format!("window={window_id:?}"),
            );
            HostRequestAction::Respond(protocol::result(&rpc_id, json!({})))
        },
        protocol::M_UI_RENDER => {
            let (surface_kind, requested_panel, doc_value) = match parse_ui_render_params(params) {
                Ok(parsed) => parsed,
                Err(e) => {
                    return HostRequestAction::Respond(protocol::error(
                        &rpc_id,
                        &RpcError::new(error_code::INVALID_PARAMS, e),
                    ))
                },
            };
            let (surface, panel) = match surface_kind.as_str() {
                "settings" => {
                    if !config.contributes.settings {
                        return deny!(
                            error_code::PERMISSION_DENIED,
                            "未声明 contributes.settings，不能推送 settings UI",
                        );
                    }
                    ("settings".to_string(), None)
                },
                "panel" => {
                    let panel = requested_panel
                        .or_else(|| config.contributes.panels.first().cloned());
                    let Some(panel) = panel else {
                        return deny!(
                            error_code::PERMISSION_DENIED,
                            "未声明 contributes.panels，不能推送 panel UI",
                        );
                    };
                    if !config.contributes.panels.iter().any(|id| id == &panel) {
                        return deny!(
                            error_code::PERMISSION_DENIED,
                            &format!("panel {panel:?} 未在 contributes.panels 声明"),
                        );
                    }
                    (format!("panel:{panel}"), Some(panel))
                },
                other => {
                    return HostRequestAction::Respond(protocol::error(
                        &rpc_id,
                        &RpcError::new(
                            error_code::INVALID_PARAMS,
                            format!("不支持的 surface {other:?}"),
                        ),
                    ))
                },
            };
            let doc = match parse_surface_doc(&doc_value) {
                Ok(doc) => doc,
                Err(e) => {
                    return HostRequestAction::Respond(protocol::error(
                        &rpc_id,
                        &RpcError::new(error_code::INVALID_PARAMS, e),
                    ))
                },
            };
            push_surface(
                shared,
                SurfaceUpdate {
                    plugin_id: config.id.clone(),
                    plugin_name: config.name.clone(),
                    surface: surface.clone(),
                    panel: panel.clone(),
                    doc,
                    window_id,
                    pushed: true,
                },
            );
            record_activity(
                shared,
                &config.id,
                "ui.render",
                "ok",
                &format!("window={window_id:?} surface={surface}"),
            );
            eprintln!(
                "[plugin-v3] {}: ui.render window={window_id:?} surface={surface} panel={panel:?}",
                config.id
            );
            HostRequestAction::Respond(protocol::result(&rpc_id, json!({"accepted": true})))
        },
        protocol::M_CONTEXT_VERSION => {
            let host = shared.host.lock().map(|host| host.clone()).unwrap_or_default();
            HostRequestAction::Respond(protocol::result(
                &rpc_id,
                json!({
                    "api": super::manifest::PLUGIN_API_VERSION,
                    "host_abi": super::manifest::HOST_ABI,
                    "protocol_version": protocol::HOST_PROTOCOL_VERSION,
                    "jsonrpc": protocol::JSONRPC_VERSION,
                    "host_version": host.host_version,
                    "transport": "stdio-line-delimited",
                }),
            ))
        },
        protocol::M_CONTEXT_CONFIG_METADATA => {
            let metadata = shared
                .host
                .lock()
                .map(|host| host.config_metadata.clone())
                .unwrap_or_else(|_| json!({}));
            HostRequestAction::Respond(protocol::result(&rpc_id, metadata))
        },
        protocol::M_TERMINAL_READ => {
            if !config.allows(Capability::TerminalContext) {
                return deny!(
                    error_code::PERMISSION_DENIED,
                    "缺少 permissions.terminal_context（capability gate）",
                );
            }
            let limits = parse_terminal_read_params(params);
            if pending_host
                .values()
                .any(|p| p.method == protocol::M_TERMINAL_READ && p.window_id == window_id)
            {
                return HostRequestAction::Respond(protocol::error(
                    &rpc_id,
                    &RpcError::new(error_code::RATE_LIMITED, "该窗口已有 terminal.read 等待确认"),
                ));
            }
            let draft = ApprovalDraft {
                method: protocol::M_TERMINAL_READ,
                summary: format!(
                    "Read up to {} lines / {} bytes from the focused terminal",
                    limits.max_lines, limits.max_bytes
                ),
                details: "The host returns only the requested tail of the focused terminal. CWD, environment variables, Vault contents and credentials are never included.".to_string(),
                params: json!({
                    "max_lines": limits.max_lines,
                    "max_bytes": limits.max_bytes,
                }),
                window_id,
            };
            queue_approval(
                shared,
                config,
                id_num,
                rpc_id,
                pending_host,
                options,
                draft,
            )
        },
        protocol::M_TERMINAL_WRITE => {
            if !config.allows(Capability::TerminalWrite) {
                return deny!(
                    error_code::PERMISSION_DENIED,
                    "缺少 permissions.terminal_write（capability gate）",
                );
            }
            let text = match protocol::parse_terminal_write_params(params) {
                Ok(text) => text,
                Err(e) => {
                    return HostRequestAction::Respond(protocol::error(
                        &rpc_id,
                        &RpcError::new(error_code::INVALID_PARAMS, e),
                    ))
                },
            };
            let draft = ApprovalDraft {
                method: protocol::M_TERMINAL_WRITE,
                summary: format!("Write {} bytes to the focused terminal", text.len()),
                details: format!("Preview (control characters escaped): {}", escape_preview(&text, 300)),
                params: json!({ "text": text }),
                window_id,
            };
            queue_approval(shared, config, id_num, rpc_id, pending_host, options, draft)
        },
        protocol::M_CLIPBOARD_READ => {
            if !config.allows(Capability::Clipboard) {
                return deny!(
                    error_code::PERMISSION_DENIED,
                    "缺少 permissions.clipboard（capability gate）",
                );
            }
            let draft = ApprovalDraft {
                method: protocol::M_CLIPBOARD_READ,
                summary: "Read the system clipboard".to_string(),
                details: "The host returns clipboard text only; images are not exposed.".to_string(),
                params: json!({}),
                window_id,
            };
            queue_approval(shared, config, id_num, rpc_id, pending_host, options, draft)
        },
        protocol::M_CLIPBOARD_WRITE => {
            if !config.allows(Capability::Clipboard) {
                return deny!(
                    error_code::PERMISSION_DENIED,
                    "缺少 permissions.clipboard（capability gate）",
                );
            }
            let text = match protocol::parse_clipboard_write_params(params) {
                Ok(text) => text,
                Err(e) => {
                    return HostRequestAction::Respond(protocol::error(
                        &rpc_id,
                        &RpcError::new(error_code::INVALID_PARAMS, e),
                    ))
                },
            };
            let draft = ApprovalDraft {
                method: protocol::M_CLIPBOARD_WRITE,
                summary: format!("Write {} characters to the system clipboard", text.chars().count()),
                details: format!("Preview (control characters escaped): {}", escape_preview(&text, 300)),
                params: json!({ "text": text }),
                window_id,
            };
            queue_approval(shared, config, id_num, rpc_id, pending_host, options, draft)
        },
        protocol::M_CREDENTIALS_GET => deny!(
            error_code::UNSUPPORTED,
            "宿主永不提供 Vault/环境变量中的凭据明文；插件必须自行管理凭据"
        ),
        other => {
            record_activity(shared, &config.id, other, "unsupported", "未知宿主方法");
            HostRequestAction::Respond(protocol::error(
                &rpc_id,
                &RpcError::new(
                    error_code::METHOD_NOT_FOUND,
                    format!("宿主不支持方法 {other:?}"),
                ),
            ))
        },
    }
}

fn queue_approval(
    shared: &SupervisorShared,
    config: &PluginRuntimeConfig,
    id_num: u64,
    rpc_id: RpcId,
    pending_host: &mut HashMap<u64, PendingHost>,
    options: &SupervisorOptions,
    draft: ApprovalDraft,
) -> HostRequestAction {
    let now = Instant::now();
    let request = ApprovalRequest {
        plugin_id: config.id.clone(),
        plugin_name: config.name.clone(),
        request_id: id_num,
        method: draft.method.to_string(),
        summary: draft.summary,
        details: draft.details,
        params: draft.params,
        deadline: now + options.approval_timeout,
        window_id: draft.window_id,
    };
    eprintln!(
        "[plugin-v3] {}: ask {} window={:?} request={id_num}",
        config.id,
        draft.method,
        draft.window_id
    );
    let queued = {
        match shared.approvals.lock() {
            Ok(mut approvals) => {
                if approvals.iter().any(|a| {
                    a.plugin_id == config.id
                        && a.method == request.method
                        && a.window_id == request.window_id
                }) || approvals.len() >= 16 {
                    false
                } else {
                    approvals.push_back(request.clone());
                    true
                }
            },
            Err(_) => false,
        }
    };
    if !queued {
        return HostRequestAction::Respond(protocol::error(
            &rpc_id,
            &RpcError::new(error_code::RATE_LIMITED, "已有待确认请求或队列已满"),
        ));
    }
    if pending_host.contains_key(&id_num) {
        remove_approval(shared, &config.id, id_num);
        return HostRequestAction::Respond(protocol::error(
            &rpc_id,
            &RpcError::new(error_code::INVALID_REQUEST, "request id 已被占用"),
        ));
    }
    pending_host.insert(id_num, PendingHost {
        rpc_id,
        deadline: request.deadline,
        method: request.method,
        window_id: request.window_id,
    });
    super::notify_wake(&shared.wake);
    HostRequestAction::Pending
}

fn remove_approval(shared: &SupervisorShared, plugin_id: &str, request_id: u64) {
    if let Ok(mut approvals) = shared.approvals.lock() {
        approvals.retain(|approval| {
            !(approval.plugin_id == plugin_id && approval.request_id == request_id)
        });
    }
}

fn handle_host_notification(
    method: &str,
    params: &Value,
    config: &PluginRuntimeConfig,
    shared: &SupervisorShared,
    notify_window: &mut VecDeque<Instant>,
) -> Result<(), String> {
    // 未知窗口的 notification 直接丢弃，防止 status/notify 表被伪造 id 撑大。
    let window_id = parse_and_resolve_window(shared, params)?;
    match method {
        protocol::M_UI_NOTIFY => {
            let payload = parse_notify_payload(params)?;
            if !allow_notify(notify_window) {
                return Err("ui.notify 频率过高".to_string());
            }
            push_notice(shared, Notice {
                plugin_id: config.id.clone(),
                plugin_name: config.name.clone(),
                text: payload.text,
                level: payload.level,
                window_id,
            });
            Ok(())
        },
        protocol::M_UI_STATUS => {
            if !config.contributes.status {
                return Err("未声明 contributes.status".to_string());
            }
            let status = parse_status_payload(params)?;
            if let Some(command) = &status.command {
                if !config.command_ids.iter().any(|id| id == command) {
                    return Err(format!("status.command {command:?} 未声明"));
                }
            }
            publish_status(shared, &config.id, window_id, Some(status));
            Ok(())
        },
        protocol::M_UI_RENDER => Err("ui.render 必须用 request（带 id）发送".to_string()),
        other => {
            eprintln!("[plugin-v3] {}: 未知 notification {other}", config.id);
            Ok(())
        },
    }
}

fn parse_ui_render_params(params: &Value) -> Result<(String, Option<String>, Value), String> {
    let object = params.as_object().ok_or("ui.render params 必须是 object")?;
    let surface = object
        .get("surface")
        .and_then(Value::as_str)
        .ok_or("ui.render.surface 必须是 panel/settings")?
        .to_string();
    let panel = object.get("panel").and_then(Value::as_str).map(str::to_string);
    let doc = if object.get("elements").is_some() {
        params.clone()
    } else {
        object
            .get("doc")
            .cloned()
            .ok_or("ui.render 缺少 elements/doc")?
    };
    Ok((surface, panel, doc))
}

fn allow_notify(window: &mut VecDeque<Instant>) -> bool {
    let now = Instant::now();
    while window
        .front()
        .is_some_and(|time| now.duration_since(*time) > Duration::from_secs(10))
    {
        window.pop_front();
    }
    if window.len() >= 20 {
        return false;
    }
    window.push_back(now);
    true
}

fn escape_preview(text: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max) {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------- 共享状态发布

/// 插件进程退出/重启：清掉它在所有窗口的 status，避免陈旧窗口状态一直显示。
fn clear_plugin_status(shared: &SupervisorShared, plugin_id: &str) {
    let changed = match shared.statuses.lock() {
        Ok(mut statuses) => {
            let before = statuses.len();
            statuses.retain(|(id, _), _| id != plugin_id);
            statuses.len() != before
        },
        Err(_) => false,
    };
    if changed {
        shared.status_revision.fetch_add(1, Ordering::SeqCst);
        super::notify_wake(&shared.wake);
    }
}

/// 该窗口是否是 UI 线程当前真实存在的窗口。
fn window_known(shared: &SupervisorShared, window_id: u64) -> bool {
    shared
        .windows
        .lock()
        .map(|windows| windows.contains(&window_id))
        .unwrap_or(false)
}

/// 解析并校验插件给的 window_id（JSON 类型错误与未知 id 都拒绝）。
fn parse_and_resolve_window(
    shared: &SupervisorShared,
    params: &Value,
) -> Result<Option<u64>, String> {
    resolve_window_id(shared, protocol::parse_window_id(params)?)
}

/// 解析 window_id：显式值必须命中窗口注册表，缺省回退 active window。
///
/// supervisor 线程不直接持有 `WindowRegistry`；UI 线程通过
/// [`super::PluginStore::set_known_windows`] 同步窗口集合，所以插件伪造的 id
/// 在这里被拒绝，而不是进入 status/审批表。
fn resolve_window_id(
    shared: &SupervisorShared,
    raw: Option<u64>,
) -> Result<Option<u64>, String> {
    match raw {
        Some(window_id) => {
            if window_known(shared, window_id) {
                Ok(Some(window_id))
            } else {
                Err(format!("未知窗口 window_id={window_id}"))
            }
        },
        None => {
            let active = shared
                .host
                .lock()
                .ok()
                .and_then(|host| host.active_window_id);
            Ok(active.filter(|window_id| window_known(shared, *window_id)))
        },
    }
}

/// 状态 key 是 `(plugin_id, window_id)`：`None` = 全局（status.get 轮询），
/// `Some(id)` = 该窗口专属 ui.status。
///
/// 防御性上限（即使调用方漏了 registry 检查）：
/// - 未知窗口不插入；
/// - 每个插件最多保留 [`MAX_STATUS_WINDOWS_PER_PLUGIN`] 个窗口专属状态，
///   超出的按窗口 id 升序驱逐，保证 status map 不随插件输入无界增长。
fn publish_status(
    shared: &SupervisorShared,
    plugin_id: &str,
    window_id: Option<u64>,
    status: Option<StatusPayload>,
) {
    if let Some(window_id) = window_id {
        if !window_known(shared, window_id) {
            return;
        }
    }
    let key = (plugin_id.to_string(), window_id);
    let changed = match shared.statuses.lock() {
        Ok(mut statuses) => {
            if status.is_some() && window_id.is_some() && !statuses.contains_key(&key) {
                let mut explicit: Vec<u64> = statuses
                    .keys()
                    .filter(|(id, window)| id == plugin_id && window.is_some())
                    .filter_map(|(_, window)| *window)
                    .collect();
                if explicit.len() >= MAX_STATUS_WINDOWS_PER_PLUGIN {
                    explicit.sort_unstable();
                    if let Some(evict) = explicit.first() {
                        statuses.remove(&(plugin_id.to_string(), Some(*evict)));
                    }
                }
            }
            let previous = statuses.get(&key).cloned();
            match status {
                Some(mut status) => {
                    status.window_id = window_id;
                    status.text = sanitize_plugin_text(&status.text, protocol::MAX_STATUS_CHARS);
                    status.tooltip =
                        sanitize_plugin_text(&status.tooltip, protocol::MAX_TOOLTIP_CHARS);
                    statuses.insert(key.clone(), status);
                },
                None => {
                    statuses.remove(&key);
                },
            }
            previous != statuses.get(&key).cloned()
        },
        Err(_) => false,
    };
    if changed {
        shared.status_revision.fetch_add(1, Ordering::SeqCst);
        super::notify_wake(&shared.wake);
    }
}

fn push_notice(shared: &SupervisorShared, notice: Notice) {
    if let Ok(mut notices) = shared.notices.lock() {
        if notices.len() >= 32 {
            notices.pop_front();
        }
        notices.push_back(notice);
    }
    super::notify_wake(&shared.wake);
}

fn push_surface(shared: &SupervisorShared, update: SurfaceUpdate) {
    if let Ok(mut surfaces) = shared.surfaces.lock() {
        if surfaces.len() >= 32 {
            surfaces.pop_front();
        }
        surfaces.push_back(update);
    }
    super::notify_wake(&shared.wake);
}

fn publish_runtime(
    runtime: &Arc<Mutex<RuntimeInfo>>,
    shared: &SupervisorShared,
    phase: Option<Phase>,
    error: &str,
    restart_count: u32,
    pid: Option<u32>,
) {
    if let Ok(mut info) = runtime.lock() {
        info.phase = phase;
        info.restart_count = restart_count;
        info.pid = pid;
        info.error = error.to_string();
    }
    shared.runtime_revision.fetch_add(1, Ordering::SeqCst);
    super::notify_wake(&shared.wake);
}

fn set_runtime_error(runtime: &Arc<Mutex<RuntimeInfo>>, message: &str) {
    if let Ok(mut info) = runtime.lock() {
        info.error = sanitize_plugin_text(message, 800);
    }
}

fn append_runtime_error(runtime: &Arc<Mutex<RuntimeInfo>>, extra: &str) {
    if let Ok(mut info) = runtime.lock() {
        if info.error.trim().is_empty() {
            info.error = extra.to_string();
        } else {
            info.error = format!("{}; {extra}", info.error);
        }
        info.error = sanitize_plugin_text(&info.error, 800);
    }
}

fn clip(text: &str, max: usize) -> String {
    protocol::clip_chars(text, max)
}

/// 插件提供的文本（stderr / error.message）进入日志或 UI 前统一清洗：
/// 先剥掉 ANSI/VT 转义序列（CSI/OSC/两字节 ESC），再把剩余控制字符替换为空格
/// 并折叠空白。否则 ESC/CR 可以改写终端日志或伪造 UI 行。
fn sanitize_plugin_text(text: &str, max: usize) -> String {
    let mut visible = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            visible.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: 参数/中间字节，直到 0x40..=0x7e 的终止字节。
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            },
            // OSC: 直到 BEL 或 ESC \。
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            },
            // 其它 ESC + 单字节序列。
            Some(_) => {},
            None => {},
        }
    }
    super::sanitize_text(&visible, max)
}

// ---------------------------------------------------------------- 子进程 IO

fn spawn_child(config: &PluginRuntimeConfig) -> Result<Child, String> {
    let mut command = Command::new(&config.entry);
    command
        .current_dir(&config.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 不清空环境会把用户的 API key / token 泄漏给插件；这里只透传运行所需
    // 的非敏感变量。注意：这不是 OS 沙箱，插件仍可读取它能访问的文件。
    command.env_clear();
    for key in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("TERVO_PLUGIN_ID", &config.id);
    command.env("TERVO_PLUGIN_API", super::manifest::PLUGIN_API_VERSION.to_string());
    command.env("TERVO_HOST_VERSION", env!("CARGO_PKG_VERSION"));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        // 宿主被 SIGKILL 时内核也回收插件进程，避免孤儿；不是权限沙箱。
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(|e| {
        format!(
            "无法执行 {}（cwd {}）: {e}",
            config.entry.display(),
            config.dir.display()
        )
    })
}

/// 子进程独立 process group；先 SIGKILL 整组再 wait，避免插件子进程残留。
fn kill_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        if pid > 0 {
            // SAFETY: kill 只接受整数 pid/signal；负 pid = 该进程 group。
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_stdout(
    stdout: std::process::ChildStdout,
    tx: Sender<ReaderEvent>,
    max_line_bytes: usize,
    max_output_bytes: u64,
) {
    let mut reader = BufReader::new(stdout);
    let mut total = 0u64;
    loop {
        match read_line_capped(&mut reader, max_line_bytes) {
            Ok(Some((line, truncated, bytes))) => {
                total = total.saturating_add(bytes as u64);
                if total > max_output_bytes {
                    let _ = tx.send(ReaderEvent::OutputLimit);
                    return;
                }
                if tx.send(ReaderEvent::Line { line, truncated }).is_err() {
                    return;
                }
            },
            Ok(None) => {
                let _ = tx.send(ReaderEvent::Eof);
                return;
            },
            Err(e) => {
                let _ = tx.send(ReaderEvent::ReadError(e.to_string()));
                return;
            },
        }
    }
}

/// 读取一行并限制内存；返回 (UTF-8 文本, 是否截断, 原始字节数)。
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<(String, bool, usize)>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            if buf.is_empty() && !truncated && total == 0 {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map(|pos| pos + 1).unwrap_or(available.len());
        let remaining = cap.saturating_sub(buf.len());
        if remaining == 0 {
            truncated = true;
        } else {
            let copy = take.min(remaining);
            buf.extend_from_slice(&available[..copy]);
            if copy < take {
                truncated = true;
            }
        }
        total = total.saturating_add(take);
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf)
        .trim_end_matches(['\n', '\r'])
        .to_string();
    Ok(Some((text, truncated, total)))
}

fn read_stderr(
    stderr: std::process::ChildStderr,
    tail: Arc<Mutex<String>>,
    cap: usize,
    plugin_id: &str,
) {
    let mut reader = BufReader::new(stderr);
    let mut lines = 0u64;
    loop {
        match read_line_capped(&mut reader, 16 * 1024) {
            Ok(Some((line, _, _))) => {
                // 先清洗再做任何记录/保存：ESC/CR 不能进裸日志或 runtime error。
                let line = sanitize_plugin_text(&line, MAX_STDERR_LINE_CHARS);
                if line.is_empty() {
                    continue;
                }
                lines += 1;
                // 防止坏插件用 stderr 刷爆日志：前 50 行全记，之后每 100 行记一次。
                if lines <= 50 || lines % 100 == 0 {
                    eprintln!("[plugin-v3] {plugin_id} stderr: {line}");
                }
                if let Ok(mut tail) = tail.lock() {
                    if tail.len() + line.len() + 1 > cap {
                        let drain = (tail.len() + line.len() + 1).saturating_sub(cap);
                        let mut cut = 0;
                        for (index, _) in tail.char_indices() {
                            if index >= drain {
                                cut = index;
                                break;
                            }
                        }
                        tail.drain(..cut);
                    }
                    tail.push_str(&line);
                    tail.push('\n');
                }
            },
            Ok(None) => return,
            Err(_) => return,
        }
    }
}
