// SPDX-License-Identifier: GPL-3.0-only
//! 进程内 SSH 后端（russh）：默认 SSH Profile 不再启动系统 `ssh` 子进程。
//!
//! 数据面：russh channel <-> tokio unbounded command queue <-> PaneTransport。
//! 连接/认证在专用 current-thread runtime 里执行；失败文本会作为红色输出
//! 写进 pane，然后 EOF，和旧 SSH 路径的可见性一致。

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handler};
use russh::keys::agent::client::AgentClient;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use tervo_core::transport::{
    PaneTransport, TransportControl, TransportHalves, TransportShutdown,
};

use crate::known_hosts::KnownKey;
use crate::known_hosts_rs;
use crate::profile::Profile;
use crate::ssh_config::{self, ResolvedSsh};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(60);

/// 远端用 SSH_MSG_CHANNEL_FAILURE 明确拒绝了 PTY/agent-forward 请求。
/// 单独用常量避免 agent forwarding 误匹配其它错误文本。
const REMOTE_REQUEST_REJECTED: &str = "远端拒绝了 PTY/shell 请求";

#[inline]
fn pack_resize(cols: u16, rows: u16) -> u32 {
    (u32::from(cols) << 16) | u32::from(rows)
}

#[inline]
fn unpack_resize(bits: u32) -> (u16, u16) {
    ((bits >> 16) as u16, bits as u16)
}

#[derive(Default)]
struct ForwardGuard(std::collections::HashMap<u64, tokio::task::JoinHandle<()>>);

impl ForwardGuard {
    fn insert(&mut self, id: u64, task: tokio::task::JoinHandle<()>) {
        self.0.insert(id, task);
    }
    fn contains(&self, id: u64) -> bool {
        self.0.contains_key(&id)
    }
    fn remove(&mut self, id: u64) -> Option<tokio::task::JoinHandle<()>> {
        self.0.remove(&id)
    }
}

impl Drop for ForwardGuard {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}

enum Cmd {
    Data(Vec<u8>),
    Resize(u16, u16),
    Close,
}

pub struct SshNativeTransport {
    output: tokio::sync::mpsc::Receiver<Option<Vec<u8>>>,
    commands: tokio::sync::mpsc::Sender<Cmd>,
    closed: Arc<AtomicBool>,
    resize: Arc<AtomicU32>,
    pending: Vec<u8>,
    status: Arc<SshNativeStatus>,
    /// UI 可查询/控制本会话转发的句柄（状态共享，不持有 russh 连接）。
    forward: crate::ssh_forward::ForwardController,
}

/// 口令弹窗/自动连接共用的纯判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPassphraseState {
    /// 非 key 认证、没有显式密钥、或密钥本身未加密。
    NotNeeded,
    /// 密钥加密且没有可用口令。
    Missing,
    /// 有口令但加载失败（口令错误或密钥格式不受支持）。
    Invalid,
    /// 有口令且能加载；可以继续 native 连接。
    Ready,
}

impl KeyPassphraseState {
    pub fn needs_prompt(self) -> bool {
        matches!(self, Self::Missing | Self::Invalid)
    }
}

/// 私钥是否无需口令即可加载（供 create_pane 判断是否需要 Vault 口令）。
pub fn key_loads_without_passphrase(profile: &Profile) -> bool {
    let path = profile.key_path.trim();
    !path.is_empty() && load_secret_key(Path::new(path), None).is_ok()
}

/// 显式 `key_path` 是否需要口令。只看 profile 自己指定的私钥；ssh_config
/// 里的 IdentityFile/默认密钥可能各有不同口令，不能用一个输入框猜。
pub fn key_needs_passphrase(profile: &Profile) -> bool {
    profile.auth == "key"
        && !profile.key_path.trim().is_empty()
        && !key_loads_without_passphrase(profile)
}

/// 有/没有 Vault 口令时私钥处于什么状态。纯函数，不读配置、不写日志。
pub fn key_passphrase_state(profile: &Profile, saved: Option<&str>) -> KeyPassphraseState {
    if !key_needs_passphrase(profile) {
        return KeyPassphraseState::NotNeeded;
    }
    match saved {
        None | Some("") => KeyPassphraseState::Missing,
        Some(passphrase) => {
            if load_secret_key(Path::new(profile.key_path.trim()), Some(passphrase)).is_ok() {
                KeyPassphraseState::Ready
            } else {
                KeyPassphraseState::Invalid
            }
        },
    }
}

/// 校验用户在弹窗里输入的口令；错误文本固定，不回显口令/密钥内容。
pub fn validate_key_passphrase(profile: &Profile, passphrase: &str) -> Result<(), String> {
    let path = profile.key_path.trim();
    if path.is_empty() {
        return Err("Profile 没有指定私钥路径".into());
    }
    if passphrase.is_empty() {
        return Err("口令不能为空".into());
    }
    load_secret_key(Path::new(path), Some(passphrase))
        .map(|_| ())
        .map_err(|_| "无法用该口令解密私钥（口令错误或私钥格式不受支持）".to_string())
}

/// 连接内可观测状态：agent forwarding 协商结果 + 私钥口令失败。
///
/// 所有字段都是原子量，UI 线程可直接读，不把口令/密钥内容带出连接线程。
#[derive(Default)]
pub struct SshNativeStatus {
    agent_forward: AtomicU8,
    key_passphrase_failed: AtomicBool,
}

/// `agent forwarding` 的协商状态。数值稳定，供 UI/日志使用。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentForwardState {
    /// Profile 没开 agent forwarding。
    #[default]
    NotRequested,
    /// 服务器接受了请求且本机有 SSH_AUTH_SOCK，agent channel 可以工作。
    Active,
    /// 服务器拒绝了 agent forwarding；连接继续，但不接受 agent channel。
    Denied,
    /// 服务器接受请求，但本机没有 SSH_AUTH_SOCK，无法真正转发。
    Unavailable,
}

impl AgentForwardState {
    pub fn code(self) -> i32 {
        match self {
            Self::NotRequested => 0,
            Self::Active => 1,
            Self::Denied => 2,
            Self::Unavailable => 3,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Active,
            2 => Self::Denied,
            3 => Self::Unavailable,
            _ => Self::NotRequested,
        }
    }
}

impl SshNativeStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn agent_forward(&self) -> AgentForwardState {
        AgentForwardState::from_code(self.agent_forward.load(Ordering::Acquire))
    }

    fn set_agent_forward(&self, state: AgentForwardState) {
        self.agent_forward.store(state.code() as u8, Ordering::Release);
    }

    /// 私钥在连接线程里加载失败（通常是口令错误）。UI 可据此再次弹口令框。
    pub fn key_passphrase_failed(&self) -> bool {
        self.key_passphrase_failed.load(Ordering::Acquire)
    }

    fn mark_key_passphrase_failed(&self) {
        self.key_passphrase_failed.store(true, Ordering::Release);
    }
}

/// 当前 native 后端支持的最小 Profile 子集。端口转发/ProxyJump/agent
/// forwarding/extra_args 需要系统 ssh 或后续 russh 实现；命中时回退系统 ssh，
/// 而不是静默忽略用户配置。
pub fn can_use(profile: &Profile) -> bool {
    if !profile.is_native_ssh() {
        return false;
    }
    if profile.x11_forward {
        return false;
    }
    if !profile.extra_args.iter().all(|arg| arg.trim().is_empty()) {
        return false;
    }
    let resolved = ssh_config::resolve(profile);
    if resolved.unsupported.is_some() || !resolved.proxy_command.is_empty() {
        return false;
    }
    if !matches!(resolved.strict.as_str(), "ask" | "yes" | "true") {
        return false;
    }
    if profile.auth == "key" && profile.key_path.trim().is_empty() {
        return false;
    }
    let has_agent = std::env::var_os("SSH_AUTH_SOCK").is_some();
    let has_keys = !identity_candidates(profile, &resolved).is_empty();
    if profile.auth == "agent" && !has_agent && !has_keys {
        return false;
    }
    if profile.auth == "key" && !has_keys {
        return false;
    }
    for jump in resolved
        .proxy_jump
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let jump_profile = jump_profile(profile, jump);
        let jump_resolved = ssh_config::resolve_jump(&jump_profile);
        if jump_resolved.unsupported.is_some() || !jump_resolved.proxy_command.is_empty() {
            return false;
        }
    }
    let lookup = known_hosts_rs::lookup(&resolved);
    !lookup.known.is_empty() && lookup.revoked.is_empty()
}

impl SshNativeTransport {
    /// 立即返回 transport；连接/认证失败会以文本 + EOF 出现在 pane 里。
    pub fn start(
        profile: &Profile,
        password: Option<String>,
        key_passphrase: Option<String>,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        let resolved = ssh_config::resolve(profile);
        Self::start_resolved(profile, password, key_passphrase, cols, rows, resolved)
    }

    /// 连接线程写入、UI 线程只读的状态句柄。必须在 box 成 transport 之前取。
    pub fn status(&self) -> Arc<SshNativeStatus> {
        self.status.clone()
    }

    fn start_resolved(
        profile: &Profile,
        password: Option<String>,
        key_passphrase: Option<String>,
        cols: u16,
        rows: u16,
        resolved: ResolvedSsh,
    ) -> io::Result<Self> {
        let status = Arc::new(SshNativeStatus::new());
        let profile = profile.clone();
        if profile.host.trim().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "SSH 主机不能为空"));
        }
        if let Some(reason) = &resolved.unsupported {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("native SSH 不支持该 ssh_config 选项: {reason}"),
            ));
        }
        if !resolved.proxy_command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native SSH 不支持 ProxyCommand；请用 transport=system",
            ));
        }
        let lookup = known_hosts_rs::lookup(&resolved);
        let known = lookup.known;
        let revoked = lookup.revoked;
        if known.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "known_hosts 中没有此主机；请先用普通 SSH 确认主机密钥（或设置 transport=system）",
            ));
        }
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Option<Vec<u8>>>(1024);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(256);
        let closed = Arc::new(AtomicBool::new(false));
        let resize = Arc::new(AtomicU32::new(pack_resize(cols, rows)));
        // 先建立可查询的转发表（全部 Pending/Failed）；连接成功后由会话线程
        // 启动 listener 并写回 Active/assigned port。UI 在连接期间也能看到规则。
        let (forward, forward_rx) = crate::ssh_forward::ForwardController::new(&profile);
        let forward_thread = forward.clone();
        let closed_thread = closed.clone();
        let resize_thread = resize.clone();
        let status_thread = status.clone();
        std::thread::Builder::new()
            .name("ssh-native".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(error) => {
                        let _ = out_tx.blocking_send(Some(
                            format!("\r\n[ssh-native] 创建 runtime 失败: {error}\r\n").into_bytes(),
                        ));
                        let _ = out_tx.blocking_send(None);
                        return;
                    },
                };
                let result = rt.block_on(run_session(
                    profile,
                    password,
                    key_passphrase,
                    known,
                    revoked,
                    resolved,
                    status_thread,
                    resize_thread,
                    cols,
                    rows,
                    out_tx.clone(),
                    cmd_rx,
                    closed_thread,
                    forward_thread,
                    forward_rx,
                ));
                if let Err(error) = result {
                    let _ = out_tx.blocking_send(Some(
                        format!("\r\n[ssh-native] 连接失败: {error}\r\n").into_bytes(),
                    ));
                }
                let _ = out_tx.blocking_send(None);
            })
            .map_err(|error| io::Error::other(format!("创建 ssh-native 线程失败: {error}")))?;
        Ok(Self {
            output: out_rx,
            commands: cmd_tx,
            closed,
            resize,
            pending: Vec::new(),
            status,
            forward,
        })
    }

    /// UI 查询/控制转发的句柄；不暴露 russh session。
    pub fn forward_control(&self) -> crate::ssh_forward::ForwardController {
        self.forward.clone()
    }
}

impl PaneTransport for SshNativeTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            match self.output.blocking_recv() {
                Some(Some(data)) => self.pending = data,
                _ => return Ok(0),
            }
        }
        let n = self.pending.len().min(buf.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.commands
            .blocking_send(Cmd::Data(data.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "SSH 通道已关闭"))
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.resize.store(pack_resize(cols, rows), Ordering::Release);
        let _ = self.commands.try_send(Cmd::Resize(cols, rows));
    }

    fn close(&mut self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.commands.try_send(Cmd::Close);
    }

    fn into_halves(self: Box<Self>) -> io::Result<TransportHalves> {
        let SshNativeTransport {
            output,
            commands,
            closed,
            resize,
            pending,
            status: _,
            forward: _,
        } = *self;
        let shutdown_cmd = commands.clone();
        let shutdown_closed = closed.clone();
        Ok(TransportHalves {
            reader: Box::new(SshReader { output, pending, pos: 0 }),
            writer: Box::new(SshWriter { commands: commands.clone() }),
            control: Box::new(SshControl {
                commands,
                closed: closed.clone(),
                resize: resize.clone(),
            }),
            shutdown: TransportShutdown::new(move || {
                shutdown_closed.store(true, Ordering::Release);
                let _ = shutdown_cmd.try_send(Cmd::Close);
            }),
        })
    }
}

struct SshReader {
    output: tokio::sync::mpsc::Receiver<Option<Vec<u8>>>,
    pending: Vec<u8>,
    pos: usize,
}

impl Read for SshReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.pending.len() {
            match self.output.blocking_recv() {
                Some(Some(data)) => {
                    self.pending = data;
                    self.pos = 0;
                },
                _ => return Ok(0),
            }
        }
        let n = (self.pending.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct SshWriter {
    commands: tokio::sync::mpsc::Sender<Cmd>,
}

impl Write for SshWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.commands
            .blocking_send(Cmd::Data(buf.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "SSH 通道已关闭"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct SshControl {
    commands: tokio::sync::mpsc::Sender<Cmd>,
    closed: Arc<AtomicBool>,
    resize: Arc<AtomicU32>,
}

impl TransportControl for SshControl {
    fn resize(&mut self, cols: u16, rows: u16) {
        self.resize.store(pack_resize(cols, rows), Ordering::Release);
        let _ = self.commands.try_send(Cmd::Resize(cols, rows));
    }
    fn close(&mut self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.commands.try_send(Cmd::Close);
    }
}

pub(crate) struct HostKeyHandler {
    known: Vec<KnownKey>,
    revoked: Vec<KnownKey>,
    remote_targets: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<(String, u16), (String, u16)>>,
    >,
    agent_forward: Arc<AtomicBool>,
}

impl Handler for HostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(_) => return Ok(false),
        };
        let text = key.to_openssh().unwrap_or_default();
        let mut parts = text.split_whitespace();
        let (Some(key_type), Some(key_b64)) = (parts.next(), parts.next()) else {
            return Ok(false);
        };
        if self
            .revoked
            .iter()
            .any(|revoked| revoked.key_type == key_type && revoked.key == key_b64)
        {
            return Ok(false);
        }
        Ok(self
            .known
            .iter()
            .any(|known| known.key_type == key_type && known.key == key_b64))
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::client::ChannelOpenHandle,
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        let target = self
            .remote_targets
            .lock()
            .ok()
            .and_then(|map| {
                let key = (connected_address.to_string(), connected_port as u16);
                map.get(&key)
                    .or_else(|| map.get(&(connected_address.to_string(), 0)))
                    .cloned()
            });
        let Some(target) = target else { return Ok(()) };
        reply.accept().await;
        tokio::spawn(crate::ssh_forward::bridge_forwarded(channel, target));
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        reply: russh::client::ChannelOpenHandle,
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        if !self.agent_forward.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(path) = std::env::var_os("SSH_AUTH_SOCK") else { return Ok(()) };
        let Ok(mut agent) = tokio::net::UnixStream::connect(path).await else { return Ok(()) };
        reply.accept().await;
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut agent, &mut stream).await;
        });
        Ok(())
    }
}

fn jump_profile(profile: &Profile, spec: &str) -> Profile {
    let (user, host, port) = parse_jump_spec(spec);
    let mut p = profile.clone();
    p.host = host;
    p.port = port.unwrap_or(0);
    p.user = user.unwrap_or_default();
    p.proxy_jump.clear();
    p.agent_forward = false;
    p
}

fn parse_jump_spec(spec: &str) -> (Option<String>, String, Option<u16>) {
    let (user, rest) = match spec.rsplit_once('@') {
        Some((user, rest)) => (Some(user.to_string()), rest),
        None => (None, spec),
    };
    // 简单 IPv4/hostname；IPv6 用 [addr]:port 交给系统 ssh。
    let (host, port) = if let Some((host, port)) = rest.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            (host.trim_matches(['[', ']']).to_string(), Some(port))
        } else {
            (rest.to_string(), None)
        }
    } else {
        (rest.to_string(), None)
    };
    (user, host, port)
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    profile: Profile,
    password: Option<String>,
    key_passphrase: Option<String>,
    known: Vec<KnownKey>,
    revoked: Vec<KnownKey>,
    resolved: ResolvedSsh,
    status: Arc<SshNativeStatus>,
    resize: Arc<AtomicU32>,
    cols: u16,
    rows: u16,
    out: tokio::sync::mpsc::Sender<Option<Vec<u8>>>,
    mut commands: tokio::sync::mpsc::Receiver<Cmd>,
    closed: Arc<AtomicBool>,
    forward: crate::ssh_forward::ForwardController,
    mut forward_commands: tokio::sync::mpsc::UnboundedReceiver<
        crate::ssh_forward::ForwardCommand,
    >,
) -> Result<(), String> {
    let host = resolved.hostname.clone();
    let port = resolved.port.max(1);
    let user = if resolved.user.trim().is_empty() {
        std::env::var("USER").unwrap_or_else(|_| "root".into())
    } else {
        resolved.user.clone()
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(INACTIVITY_TIMEOUT),
        keepalive_interval: resolved.keepalive_interval,
        keepalive_max: resolved.keepalive_max,
        ..<_>::default()
    });
    let remote_targets = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::HashMap::new(),
    ));
    let mut keep_sessions: Vec<client::Handle<HostKeyHandler>> = Vec::new();
    let mut jump_session: Option<client::Handle<HostKeyHandler>> = None;
    let jump_specs: Vec<&str> = resolved
        .proxy_jump
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    for jump in jump_specs {
        let jump_profile = jump_profile(&profile, jump);
        let jump_resolved = ssh_config::resolve_jump(&jump_profile);
        if let Some(reason) = &jump_resolved.unsupported {
            return Err(format!("ProxyJump 配置不受 native 支持: {reason}"));
        }
        if !jump_resolved.proxy_command.is_empty() {
            return Err("native ProxyJump 的 ProxyCommand 未实现".into());
        }
        let jump_lookup = known_hosts_rs::lookup(&jump_resolved);
        if jump_lookup.known.is_empty() {
            return Err(format!(
                "known_hosts 中没有跳板机 {}；请先用普通 SSH 确认",
                jump_resolved.hostname
            ));
        }
        let jump_handler = HostKeyHandler {
            known: jump_lookup.known,
            revoked: jump_lookup.revoked,
            remote_targets: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            agent_forward: Arc::new(AtomicBool::new(false)),
        };
        let mut next = if let Some(prev) = jump_session.take() {
            let channel = prev
                .channel_open_direct_tcpip(
                    &jump_resolved.hostname,
                    jump_resolved.port.max(1) as u32,
                    "127.0.0.1",
                    0,
                )
                .await
                .map_err(|error| format!("ProxyJump 打开跳板通道失败: {error}"))?;
            keep_sessions.push(prev);
            tokio::time::timeout(
                CONNECT_TIMEOUT,
                client::connect_stream(config.clone(), channel.into_stream(), jump_handler),
            )
            .await
            .map_err(|_| format!("ProxyJump {} 超时", jump_resolved.hostname))?
            .map_err(|error| format!("ProxyJump {} 失败: {error}", jump_resolved.hostname))?
        } else {
            tokio::time::timeout(
                CONNECT_TIMEOUT,
                client::connect(
                    config.clone(),
                    (jump_resolved.hostname.as_str(), jump_resolved.port.max(1)),
                    jump_handler,
                ),
            )
            .await
            .map_err(|_| format!("ProxyJump {} 超时", jump_resolved.hostname))?
            .map_err(|error| format!("ProxyJump {} 失败: {error}", jump_resolved.hostname))?
        };
        authenticate(
            &mut next,
            &jump_profile,
            &jump_resolved,
            &jump_resolved.user,
            password.clone(),
            key_passphrase.as_deref(),
            None,
        )
        .await?;
        jump_session = Some(next);
    }
    let agent_forward_enabled = Arc::new(AtomicBool::new(false));
    let handler = HostKeyHandler {
        known,
        revoked,
        remote_targets: remote_targets.clone(),
        agent_forward: agent_forward_enabled.clone(),
    };
    let mut session = if let Some(prev) = jump_session.take() {
        let channel = prev
            .channel_open_direct_tcpip(&host, port as u32, "127.0.0.1", 0)
            .await
            .map_err(|error| format!("经 ProxyJump 打开目标通道失败: {error}"))?;
        keep_sessions.push(prev);
        tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect_stream(config, channel.into_stream(), handler),
        )
        .await
        .map_err(|_| format!("经 ProxyJump 连接 {host}:{port} 超时"))?
        .map_err(|error| format!("经 ProxyJump 连接 {host}:{port} 失败: {error}"))?
    } else {
        tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect(config, (host.as_str(), port), handler),
        )
        .await
        .map_err(|_| format!("连接 {host}:{port} 超时（{}s）", CONNECT_TIMEOUT.as_secs()))?
        .map_err(|error| format!("连接 {host}:{port} 失败: {error}"))?
    };
    authenticate(
        &mut session,
        &profile,
        &resolved,
        &user,
        password,
        key_passphrase.as_deref(),
        Some(&status),
    )
    .await?;
    let _keep_sessions = keep_sessions;
    let session = Arc::new(session);

    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|error| format!("打开 SSH session 失败: {error}"))?;
    channel
        .request_pty(
            true,
            &std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
            cols.max(1) as u32,
            rows.max(1) as u32,
            0,
            0,
            &[],
        )
        .await
        .map_err(|error| format!("请求远端 PTY 失败: {error}"))?;
    await_request(&mut channel, &out, &closed).await?;
    if profile.agent_forward {
        channel
            .agent_forward(true)
            .await
            .map_err(|error| format!("请求 agent forwarding 失败: {error}"))?;
        match await_request(&mut channel, &out, &closed).await {
            Ok(()) if std::env::var_os("SSH_AUTH_SOCK").is_some() => {
                // 只有服务器接受且本机真的有 agent 时才允许后续 agent channel。
                agent_forward_enabled.store(true, Ordering::Release);
                status.set_agent_forward(AgentForwardState::Active);
                eprintln!("[ssh-native] agent forwarding: active");
            },
            Ok(()) => {
                status.set_agent_forward(AgentForwardState::Unavailable);
                eprintln!(
                    "[ssh-native] agent forwarding: 服务器已接受，但本机没有 SSH_AUTH_SOCK；继续无转发连接"
                );
            },
            Err(error) if error == REMOTE_REQUEST_REJECTED => {
                // 服务器拒绝：连接继续，但 handler 的 gate 保持 false，绝不接受
                // 服务器后续打开的 agent channel。
                status.set_agent_forward(AgentForwardState::Denied);
                eprintln!("[ssh-native] agent forwarding 被拒绝，继续无转发连接");
            },
            Err(error) => return Err(error),
        }
    }
    channel
        .request_shell(true)
        .await
        .map_err(|error| format!("请求远端 shell 失败: {error}"))?;
    await_request(&mut channel, &out, &closed).await?;
    // 端口转发：逐条启动并写入共享状态。单条失败只标记 Failed + pane 警告，
    // 不再整条 SSH 会话退出（用户可在 Port Forwards 面板看到原因并停/启）。
    let mut forward_guard = ForwardGuard::default();
    for rule in forward.rules() {
        if rule.error.is_some() {
            let message = format!(
                "-{} {} 未启动: {}",
                rule.kind.short(),
                rule.spec,
                rule.error.as_deref().unwrap_or("规则无效")
            );
            emit_forward_warning(&out, &message).await;
            forward.mark_failed(rule.id, rule.error.clone().unwrap_or_default());
            continue;
        }
        let result = match rule.kind {
            crate::ssh_forward::ForwardKind::Local => {
                crate::ssh_forward::start_local(
                    session.clone(),
                    &rule.bind,
                    rule.port,
                    &rule.target_host,
                    rule.target_port,
                )
                .await
                .map(|(task, bound)| {
                    forward_guard.insert(rule.id, task);
                    bound
                })
            },
            crate::ssh_forward::ForwardKind::Dynamic => {
                crate::ssh_forward::start_dynamic(
                    session.clone(),
                    &rule.bind,
                    rule.port,
                )
                .await
                .map(|(task, bound)| {
                    forward_guard.insert(rule.id, task);
                    bound
                })
            },
            crate::ssh_forward::ForwardKind::Remote => crate::ssh_forward::start_remote(
                &session,
                &rule.bind,
                rule.port,
                &rule.target_host,
                rule.target_port,
                &remote_targets,
            )
            .await,
        };
        match result {
            Ok(bound) => forward.mark_active(rule.id, bound),
            Err(error) => {
                let message = format!(
                    "-{} {} 启动失败: {error}",
                    rule.kind.short(),
                    rule.spec
                );
                eprintln!("[ssh-forward] {message}");
                emit_forward_warning(&out, &message).await;
                forward.mark_failed(rule.id, error);
            },
        }
    }
    let mut last_resize = unpack_resize(resize.load(Ordering::Acquire));

    // Login Scripts（SSH-013）：远端 shell 已确认，按 Profile 顺序发送。
    // 只写 SSH channel，不经过本地 shell。
    run_login_scripts(&mut channel, &out, &closed, &profile.login_scripts).await?;

    let mut forward_commands_open = true;
    loop {
        if closed.load(Ordering::Acquire) {
            let _ = session.disconnect(Disconnect::ByApplication, "", "closed").await;
            return Ok(());
        }
        tokio::select! {
            cmd = commands.recv() => {
                match cmd {
                    Some(Cmd::Data(data)) => {
                        let mut sending = Box::pin(channel.data(&data[..]));
                        loop {
                            tokio::select! {
                                result = &mut sending => {
                                    if let Err(error) = result {
                                        return Err(format!("写 SSH 通道失败: {error}"));
                                    }
                                    break;
                                }
                                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                                    if closed.load(Ordering::Acquire) {
                                        let _ = session.disconnect(
                                            Disconnect::ByApplication, "", "closed"
                                        ).await;
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                    Some(Cmd::Resize(cols, rows)) => {
                        if channel
                            .window_change(cols as u32, rows as u32, 0, 0)
                            .await
                            .is_ok()
                        {
                            last_resize = (cols, rows);
                        }
                    }
                    Some(Cmd::Close) | None => {
                        closed.store(true, Ordering::Release);
                        let _ = channel.eof().await;
                        let _ = session.disconnect(Disconnect::ByApplication, "", "closed").await;
                        return Ok(());
                    }
                }
            }
            command = forward_commands.recv(), if forward_commands_open => {
                match command {
                    Some(command) => {
                        handle_forward_command(
                            &session,
                            &forward,
                            &mut forward_guard,
                            command,
                            &out,
                        )
                        .await;
                    }
                    None => forward_commands_open = false,
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        if out.send(Some(data.to_vec())).await.is_err() {
                            return Ok(());
                        }
                    }
                    Some(ChannelMsg::ExtendedData { .. }) => {}
                    Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                        let _ = session.disconnect(Disconnect::ByApplication, "", "eof").await;
                        return Ok(());
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if closed.load(Ordering::Acquire) {
                    let _ = session.disconnect(Disconnect::ByApplication, "", "closed").await;
                    return Ok(());
                }
                let (cols, rows) = unpack_resize(resize.load(Ordering::Acquire));
                if (cols, rows) != last_resize {
                    let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                    last_resize = (cols, rows);
                }
            }
        }
    }
}

async fn emit_forward_warning(
    out: &tokio::sync::mpsc::Sender<Option<Vec<u8>>>,
    message: &str,
) {
    let text = format!("\r\n[ssh-forward] {message}\r\n");
    let _ = out.send(Some(text.into_bytes())).await;
}

/// 处理 UI 发来的 local/dynamic 启停命令；remote 不支持运行时启停。
async fn handle_forward_command(
    session: &Arc<client::Handle<HostKeyHandler>>,
    forward: &crate::ssh_forward::ForwardController,
    guard: &mut ForwardGuard,
    command: crate::ssh_forward::ForwardCommand,
    out: &tokio::sync::mpsc::Sender<Option<Vec<u8>>>,
) {
    use crate::ssh_forward::{ForwardCommand, ForwardKind};

    match command {
        ForwardCommand::Start(id) => {
            let Some(rule) = forward.rule(id) else {
                return;
            };
            if guard.contains(id) {
                let port = rule.assigned_port.unwrap_or(rule.port);
                forward.mark_active(id, port);
                return;
            }
            let started = match rule.kind {
                ForwardKind::Local => crate::ssh_forward::start_local(
                    session.clone(),
                    &rule.bind,
                    rule.port,
                    &rule.target_host,
                    rule.target_port,
                )
                .await
                .map(|(task, bound)| {
                    guard.insert(rule.id, task);
                    bound
                }),
                ForwardKind::Dynamic => crate::ssh_forward::start_dynamic(
                    session.clone(),
                    &rule.bind,
                    rule.port,
                )
                .await
                .map(|(task, bound)| {
                    guard.insert(rule.id, task);
                    bound
                }),
                ForwardKind::Remote => Err("远程转发不支持当前会话运行时启动".into()),
            };
            match started {
                Ok(bound) => {
                    eprintln!(
                        "[ssh-forward] {} {} → {} 已启动",
                        rule.kind.short(),
                        rule.spec,
                        bound
                    );
                    forward.mark_active(id, bound);
                },
                Err(error) => {
                    emit_forward_warning(
                        out,
                        &format!("-{} {} 启动失败: {error}", rule.kind.short(), rule.spec),
                    )
                    .await;
                    forward.mark_failed(id, error);
                },
            }
        },
        ForwardCommand::Stop(id) => {
            let Some(rule) = forward.rule(id) else {
                return;
            };
            if let Some(task) = guard.remove(id) {
                task.abort();
                // 等 abort 真正 drop listener 再返回，避免立刻 Start 时端口仍被占。
                let _ = task.await;
            }
            forward.mark_stopped(id);
            eprintln!("[ssh-forward] {} {} 已停止", rule.kind.short(), rule.spec);
        },
    }
}

/// 等待 PTY/shell 请求的 Success/Failure；期间到达的远端数据先送 pane。
async fn await_request(
    channel: &mut russh::Channel<russh::client::Msg>,
    out: &tokio::sync::mpsc::Sender<Option<Vec<u8>>>,
    closed: &AtomicBool,
) -> Result<(), String> {
    loop {
        if closed.load(Ordering::Acquire) {
            return Err("pane 已关闭".into());
        }
        tokio::select! {
            message = channel.wait() => match message {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => return Err(REMOTE_REQUEST_REJECTED.into()),
            Some(ChannelMsg::Data { data }) => {
                if out.send(Some(data.to_vec())).await.is_err() {
                    return Err("pane 已关闭".into());
                }
            },
            Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                return Err("远端在 PTY/shell 确认前关闭了通道".into())
            },
            _ => {},
            },
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if closed.load(Ordering::Acquire) {
                    return Err("pane 已关闭".into());
                }
            }
        }
    }
}

/// 连接 shell 建立后按序把 Login Scripts 发给**远端** PTY。
///
/// 命令只写入 SSH channel；这里不启动本地 shell/`sh -c`。等待提示符时收到的
/// 远端数据照常送 pane，用户能看到脚本执行过程。等待超时默认停止后续脚本，
/// 但不断开 SSH 会话；`on_error=continue` 时记录警告并继续。
async fn run_login_scripts(
    channel: &mut russh::Channel<russh::client::Msg>,
    out: &tokio::sync::mpsc::Sender<Option<Vec<u8>>>,
    closed: &AtomicBool,
    raw_lines: &[String],
) -> Result<(), String> {
    let steps = match crate::login_script::parse_lines(raw_lines) {
        Ok(steps) => steps,
        Err(error) => {
            // Profile 保存时已校验；这里兜底（例如手写 profiles.toml）。
            emit_login_warning(out, &error).await;
            return Ok(());
        },
    };
    if steps.is_empty() {
        return Ok(());
    }
    eprintln!("[ssh-native] login scripts: {} 条", steps.len());
    for (index, step) in steps.iter().enumerate() {
        if closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut payload = step.command.clone().into_bytes();
        // PTY 里的 Enter 就是 CR；追加 LF 会让部分 shell 多执行一次空行。
        payload.push(b'\r');
        if let Err(error) = channel.data(&payload[..]).await {
            return Err(format!("登录脚本发送命令失败（第 {} 条）: {error}", index + 1));
        }
        if let Some(wait_for) = &step.wait_for {
            let mut seen = String::new();
            let deadline = tokio::time::Instant::now() + step.timeout;
            let mut matched = false;
            while !closed.load(Ordering::Acquire) {
                let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
                else {
                    break;
                };
                match tokio::time::timeout(remaining, channel.wait()).await {
                    Ok(Some(ChannelMsg::Data { data })) => {
                        if out.send(Some(data.to_vec())).await.is_err() {
                            return Ok(());
                        }
                        seen.push_str(&String::from_utf8_lossy(&data));
                        if seen.contains(wait_for) {
                            matched = true;
                            break;
                        }
                        // 提示符匹配窗口只需覆盖最近一段输出；限制内存，避免
                        // 脚本等待期间远端洪水把本地缓冲吃满。
                        if seen.len() > 64 * 1024 {
                            let keep = seen.len() - 32 * 1024;
                            seen.drain(..keep);
                        }
                    },
                    Ok(Some(ChannelMsg::Eof | ChannelMsg::Close)) | Ok(None) => return Ok(()),
                    Ok(Some(_)) => {},
                    Err(_) => break,
                }
            }
            if !matched && !closed.load(Ordering::Acquire) {
                let message = format!(
                    "第 {} 条脚本等待 {} 超时（{}ms）",
                    index + 1,
                    quote_for_log(wait_for),
                    step.timeout.as_millis()
                );
                eprintln!("[ssh-native] {message}");
                emit_login_warning(out, &message).await;
                if step.continue_on_error {
                    continue;
                }
                emit_login_warning(out, "已停止后续登录脚本，SSH 会话保持连接").await;
                return Ok(());
            }
            // 会话在等待期间关闭：主循环无需继续。
            if closed.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        if !step.delay.is_zero() {
            tokio::time::sleep(step.delay).await;
        }
    }
    Ok(())
}

fn quote_for_log(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

async fn emit_login_warning(
    out: &tokio::sync::mpsc::Sender<Option<Vec<u8>>>,
    message: &str,
) {
    let text = format!("\r\n[login-script] {message}\r\n");
    let _ = out.send(Some(text.into_bytes())).await;
}

fn identity_candidates(profile: &Profile, resolved: &ResolvedSsh) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let explicit = profile.key_path.trim();
    if !explicit.is_empty() {
        out.push(std::path::PathBuf::from(explicit));
    }
    for path in &resolved.identity_files {
        if !path.trim().is_empty() {
            out.push(std::path::PathBuf::from(path));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let ssh = std::path::PathBuf::from(home).join(".ssh");
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            let path = ssh.join(name);
            if path.is_file() {
                out.push(path);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|path| seen.insert(path.clone()));
    out
}

enum PublicKeyAttemptError {
    /// 本地解密/读取私钥失败（最典型的是口令错误）。
    Load(String),
    /// 连接/协议层失败。
    Auth(String),
}

async fn try_public_key<H: Handler + Send>(
    session: &mut client::Handle<H>,
    user: &str,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<bool, PublicKeyAttemptError> {
    let key = match load_secret_key(path, passphrase) {
        Ok(key) => key,
        Err(error) => {
            return Err(PublicKeyAttemptError::Load(format!(
                "加载私钥 {} 失败: {error}",
                path.display()
            )))
        },
    };
    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|error| PublicKeyAttemptError::Auth(format!("协商 RSA hash 失败: {error}")))?
        .flatten();
    let result = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await
        .map_err(|error| {
            PublicKeyAttemptError::Auth(format!("公钥认证 {} 失败: {error}", path.display()))
        })?;
    Ok(result.success())
}

async fn try_agent<H: Handler + Send>(
    session: &mut client::Handle<H>,
    user: &str,
) -> Result<bool, String> {
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|error| format!("连接 ssh-agent 失败: {error}"))?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|error| format!("读取 ssh-agent identities 失败: {error}"))?;
    for identity in identities {
        if let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity {
            if session
                .authenticate_publickey_with(user, key, None, &mut agent)
                .await
                .map(|result| result.success())
                .unwrap_or(false)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn authenticate<H: Handler + Send>(
    session: &mut client::Handle<H>,
    profile: &Profile,
    resolved: &ResolvedSsh,
    user: &str,
    password: Option<String>,
    key_passphrase: Option<&str>,
    status: Option<&SshNativeStatus>,
) -> Result<(), String> {
    if profile.auth == "password" {
        let Some(password) = password else {
            return Err(
                "native SSH 密码认证需要把密码保存到 Vault（或设置 transport=system 使用系统 ssh）"
                    .into(),
            );
        };
        let ok = session
            .authenticate_password(user, password)
            .await
            .map_err(|error| format!("密码认证失败: {error}"))?
            .success();
        if ok {
            return Ok(());
        }
        return Err("SSH 密码认证失败".into());
    }

    let mut last_error = None;
    if profile.auth == "agent" && std::env::var_os("SSH_AUTH_SOCK").is_some() {
        match try_agent(session, user).await {
            Ok(true) => return Ok(()),
            Ok(false) => last_error = Some("ssh-agent 没有可用身份".to_string()),
            Err(error) => last_error = Some(error),
        }
    }
    let candidates = identity_candidates(profile, resolved);
    if candidates.is_empty() && last_error.is_none() {
        last_error = Some("没有可用的私钥/agent 身份".to_string());
    }
    let mut errors = Vec::new();
    let mut passphrase_load_failed = false;
    for path in candidates {
        match try_public_key(session, user, &path, key_passphrase).await {
            Ok(true) => return Ok(()),
            Ok(false) => errors.push(format!("{} 被服务器拒绝", path.display())),
            Err(PublicKeyAttemptError::Load(error)) => {
                // 有口令但本地解密失败：连接线程把状态交回 UI，后者可以再次
                // 弹出口令框（Vault 里的口令可能已经过期/被外部改过）。
                if key_passphrase.is_some() {
                    passphrase_load_failed = true;
                }
                errors.push(error);
            },
            Err(PublicKeyAttemptError::Auth(error)) => errors.push(error),
        }
    }
    if passphrase_load_failed {
        if let Some(status) = status {
            status.mark_key_passphrase_failed();
        }
    }
    if let Some(error) = last_error {
        errors.push(error);
    }
    Err(format!(
        "SSH 认证失败（auth={}）：{}",
        profile.auth,
        errors.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    fn test_path(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            crate::logging::now_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn parses_jump_spec() {
        assert_eq!(
            parse_jump_spec("alice@jump.example:2222"),
            (Some("alice".into()), "jump.example".into(), Some(2222))
        );
        assert_eq!(
            parse_jump_spec("jump.example"),
            (None, "jump.example".into(), None)
        );
    }

    #[test]
    fn no_known_host_disables_native() {
        let p = Profile {
            kind: "ssh".into(),
            host: "no-such-host.invalid".into(),
            ..Profile::default()
        };
        assert!(!can_use(&p));
    }

    #[test]
    fn detects_encrypted_key_and_loads_it_with_passphrase() {
        let path = test_path("tervo-enc-key");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "s3cret", "-f"])
            .arg(&path)
            .status()
            .expect("ssh-keygen for encrypted key test");
        assert!(status.success());
        let p = Profile {
            kind: "ssh".into(),
            auth: "key".into(),
            key_path: path.display().to_string(),
            ..Profile::default()
        };
        assert!(!key_loads_without_passphrase(&p));
        assert!(load_secret_key(&path, Some("s3cret")).is_ok());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("pub"));
    }

    #[test]
    fn key_passphrase_state_distinguishes_missing_invalid_and_ready() {
        let path = test_path("tervo-enc-key-state");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "s3cret", "-f"])
            .arg(&path)
            .status()
            .expect("ssh-keygen for encrypted key state test");
        assert!(status.success());
        let p = Profile {
            kind: "ssh".into(),
            auth: "key".into(),
            key_path: path.display().to_string(),
            ..Profile::default()
        };
        assert!(key_needs_passphrase(&p));
        assert_eq!(key_passphrase_state(&p, None), KeyPassphraseState::Missing);
        assert_eq!(
            key_passphrase_state(&p, Some("wrong")),
            KeyPassphraseState::Invalid
        );
        assert_eq!(
            key_passphrase_state(&p, Some("s3cret")),
            KeyPassphraseState::Ready
        );
        assert!(validate_key_passphrase(&p, "wrong").is_err());
        assert!(validate_key_passphrase(&p, "s3cret").is_ok());
        // 未加密的私钥不应弹窗；空口令也不应被误判为可用。
        let plain = test_path("tervo-plain-key-state");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&plain)
            .status()
            .expect("ssh-keygen for plain key state test");
        assert!(status.success());
        let plain_profile = Profile {
            auth: "key".into(),
            key_path: plain.display().to_string(),
            ..p.clone()
        };
        assert!(!key_needs_passphrase(&plain_profile));
        assert_eq!(
            key_passphrase_state(&plain_profile, None),
            KeyPassphraseState::NotNeeded
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("pub"));
        let _ = std::fs::remove_file(&plain);
        let _ = std::fs::remove_file(plain.with_extension("pub"));
    }

    #[test]
    fn agent_forward_state_codes_are_stable() {
        let status = SshNativeStatus::new();
        assert_eq!(status.agent_forward(), AgentForwardState::NotRequested);
        assert!(!status.key_passphrase_failed());
        status.set_agent_forward(AgentForwardState::Denied);
        assert_eq!(status.agent_forward(), AgentForwardState::Denied);
        status.set_agent_forward(AgentForwardState::Active);
        assert_eq!(status.agent_forward(), AgentForwardState::Active);
        status.mark_key_passphrase_failed();
        assert!(status.key_passphrase_failed());
        assert_eq!(AgentForwardState::Active.code(), 1);
        assert_eq!(AgentForwardState::Denied.code(), 2);
        assert_eq!(AgentForwardState::Unavailable.code(), 3);
    }

    #[test]
    fn missing_known_host_is_rejected_before_connect() {
        let p = Profile {
            kind: "ssh".into(),
            host: "example.invalid".into(),
            user: "nobody".into(),
            ..Profile::default()
        };
        let err = match SshNativeTransport::start(&p, None, None, 80, 24) {
            Ok(_) => panic!("missing known_hosts entry must fail"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("known_hosts"), "{err}");
    }

}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use russh::keys::PrivateKey;
    use russh::server::{self, Auth, Msg, Server as _, Session};
    use std::sync::mpsc;
    use std::time::Duration;

    struct TestServer {
        forward_to: Option<u16>,
        agent_accept: bool,
        /// 服务器主动打开 agent channel 的结果（true=客户端接受了通道）。
        /// 用来验证“协商被拒绝后 handler 不得接受 agent channel”。
        agent_probe: Option<tokio::sync::mpsc::Sender<bool>>,
        /// Login Scripts 测试：记录客户端通过 session channel 发来的原始字节。
        record: Option<Arc<std::sync::Mutex<Vec<u8>>>>,
        /// 非空时每条命令后回复该提示符（供 `wait=` 匹配）。
        prompt: Option<String>,
    }

    impl server::Server for TestServer {
        type Handler = TestHandler;
        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
            TestHandler {
                forward_to: self.forward_to,
                agent_accept: self.agent_accept,
                agent_probe: self.agent_probe.clone(),
                record: self.record.clone(),
                prompt: self.prompt.clone(),
            }
        }
    }

    struct TestHandler {
        forward_to: Option<u16>,
        agent_accept: bool,
        agent_probe: Option<tokio::sync::mpsc::Sender<bool>>,
        record: Option<Arc<std::sync::Mutex<Vec<u8>>>>,
        prompt: Option<String>,
    }

    impl server::Handler for TestHandler {
        type Error = russh::Error;

        async fn channel_open_session(
            &mut self,
            _channel: russh::Channel<Msg>,
            reply: server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn auth_publickey(
            &mut self,
            _user: &str,
            _key: &russh::keys::ssh_key::PublicKey,
        ) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn agent_request(
            &mut self,
            channel: russh::ChannelId,
            session: &mut Session,
        ) -> Result<bool, Self::Error> {
            // russh 0.63 的 server/session.rs 在 agent_request 分支里，无论
            // handler 返回 true/false 都会调用 **全局** request_success/failure；
            // 但 auth-agent-req 是 channel request，客户端等的是 CHANNEL_SUCCESS/
            // FAILURE。返回 Ok(false) 会发出一个协议层 REQUEST_FAILURE，客户端
            // 视为异常并断开（这就是旧测试 server 的“拒绝路径异常关闭”）。
            // 这里自己回 channel 级应答，并返回 Ok(true) 让 crate 不再发错包。
            if self.agent_accept {
                session.channel_success(channel)?;
            } else {
                session.channel_failure(channel)?;
            }
            Ok(true)
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: russh::Channel<Msg>,
            _host: &str,
            _port: u32,
            _originator_address: &str,
            _originator_port: u32,
            reply: server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            let Some(port) = self.forward_to else { return Ok(()) };
            reply.accept().await;
            tokio::spawn(async move {
                let Ok(mut tcp) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await
                else {
                    return;
                };
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
            });
            Ok(())
        }

        async fn tcpip_forward(
            &mut self,
            address: &str,
            port: &mut u32,
            session: &mut Session,
        ) -> Result<bool, Self::Error> {
            let listener = tokio::net::TcpListener::bind((address, *port as u16)).await?;
            let assigned = listener.local_addr()?.port();
            *port = assigned as u32;
            let handle = session.handle();
            let bind = address.to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut tcp, origin)) = listener.accept().await else { break };
                    let handle = handle.clone();
                    let bind = bind.clone();
                    tokio::spawn(async move {
                        let Ok(channel) = handle
                            .channel_open_forwarded_tcpip(
                                bind,
                                assigned as u32,
                                origin.ip().to_string(),
                                origin.port() as u32,
                            )
                            .await
                        else {
                            return;
                        };
                        let mut stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                    });
                }
            });
            Ok(true)
        }

        async fn pty_request(
            &mut self,
            channel: russh::ChannelId,
            _term: &str,
            _cols: u32,
            _rows: u32,
            _pw: u32,
            _ph: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel);
            Ok(())
        }

        async fn shell_request(
            &mut self,
            channel: russh::ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel);
            session.data(channel, b"READY\r\n".to_vec())?;
            // 模拟“服务器在 shell 起来后主动打开 agent channel”。真实服务器只有
            // 在远端程序请求 agent 转发时才这样做；这里主动探测客户端 gate。
            if let Some(probe) = self.agent_probe.take() {
                let handle = session.handle();
                tokio::spawn(async move {
                    let accepted = tokio::time::timeout(
                        Duration::from_secs(3),
                        handle.channel_open_agent(),
                    )
                    .await
                    .map(|result| result.is_ok())
                    .unwrap_or(false);
                    let _ = probe.send(accepted).await;
                });
            }
            Ok(())
        }

        async fn data(
            &mut self,
            channel: russh::ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            if let Some(record) = &self.record {
                record.lock().unwrap().extend_from_slice(data);
            }
            if let Some(prompt) = &self.prompt {
                // 命令本身不回显（真实 PTY 会回显，但这里只验证顺序/等待）。
                session.data(channel, prompt.as_bytes().to_vec())?;
                return Ok(());
            }
            // 跳板机的 direct-tcpip 通道由 copy_bidirectional 处理，不能再走 echo。
            if self.forward_to.is_some() {
                return Ok(());
            }
            if data == b"quit" {
                session.close(channel)?;
            } else {
                session.data(channel, data.to_vec())?;
            }
            Ok(())
        }
    }

    #[test]
    fn native_ssh_connects_password_and_echoes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = test_path("tervo-native-ssh");
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("hostkey");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .expect("ssh-keygen for test host key");
        assert!(status.success(), "ssh-keygen failed");
        let key = load_secret_key(&key_path, None).expect("load test host key");
        let public = key.public_key().to_openssh().unwrap();
        let mut parts = public.split_whitespace();
        let key_type = parts.next().unwrap();
        let key_b64 = parts.next().unwrap();
        let known = dir.join("known_hosts");
        std::fs::write(&known, format!("[127.0.0.1]:{} {key_type} {key_b64}\n", addr.port()))
            .unwrap();
        let config = Arc::new(server::Config {
            keys: vec![key],
            inactivity_timeout: Some(Duration::from_secs(30)),
            ..<_>::default()
        });
        rt.spawn(async move {
            let _ = TestServer {
                forward_to: None,
                agent_accept: false,
                agent_probe: None,
                record: None,
                prompt: None,
            }
            .run_on_socket(config, &listener)
            .await;
        });

        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            auth: "password".into(),
            extra_args: vec![
                "-o".into(),
                format!("UserKnownHostsFile={}", known.display()),
                "-o".into(),
                "StrictHostKeyChecking=yes".into(),
            ],
            ..Profile::default()
        };
        let resolved = ResolvedSsh {
            hostname: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            proxy_jump: String::new(),
            proxy_command: String::new(),
            keepalive_interval: None,
            keepalive_max: 3,
            strict: "ask".into(),
            user_known_hosts: vec![known.clone()],
            global_known_hosts: Vec::new(),
            host_key_alias: None,
            forward_agent: false,
            identity_files: Vec::new(),
            unsupported: None,
        };
        let transport = SshNativeTransport::start_resolved(
            &profile,
            Some("pw".into()),
            None,
            80,
            24,
            resolved,
        )
        .expect("start native ssh");
        let (mut tx, mut rx) = (transport, ());
        let _ = &mut rx;
        let mut buf = [0u8; 256];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while !got.windows(5).any(|w| w == b"READY") {
            assert!(std::time::Instant::now() < deadline, "no READY from test sshd");
            let n = tx.read(&mut buf).expect("read ssh");
            assert!(n > 0, "ssh EOF before READY; got={:?}", String::from_utf8_lossy(&got));
            got.extend_from_slice(&buf[..n]);
        }
        tx.write_all(b"hello").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut echo = Vec::new();
        while !echo.windows(5).any(|w| w == b"hello") {
            assert!(std::time::Instant::now() < deadline, "no echo from test sshd");
            let n = tx.read(&mut buf).expect("read ssh echo");
            assert!(n > 0, "ssh EOF before echo");
            echo.extend_from_slice(&buf[..n]);
        }
        tx.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 测试期间临时改环境变量，Drop 时恢复（panic 时也恢复，避免污染其它测试）。
    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.old.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn test_path(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            crate::logging::now_ms(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn start_test_server(
        rt: &tokio::runtime::Runtime,
        forward_to: Option<u16>,
    ) -> (std::net::SocketAddr, String) {
        start_test_server_all(rt, forward_to, false, None, None, None)
    }

    fn start_test_server_full(
        rt: &tokio::runtime::Runtime,
        forward_to: Option<u16>,
        agent_accept: bool,
        agent_probe: Option<tokio::sync::mpsc::Sender<bool>>,
    ) -> (std::net::SocketAddr, String) {
        start_test_server_all(rt, forward_to, agent_accept, agent_probe, None, None)
    }

    fn start_test_server_with(
        rt: &tokio::runtime::Runtime,
        forward_to: Option<u16>,
        record: Option<Arc<std::sync::Mutex<Vec<u8>>>>,
        prompt: Option<String>,
    ) -> (std::net::SocketAddr, String) {
        start_test_server_all(rt, forward_to, false, None, record, prompt)
    }

    fn start_test_server_all(
        rt: &tokio::runtime::Runtime,
        forward_to: Option<u16>,
        agent_accept: bool,
        agent_probe: Option<tokio::sync::mpsc::Sender<bool>>,
        record: Option<Arc<std::sync::Mutex<Vec<u8>>>>,
        prompt: Option<String>,
    ) -> (std::net::SocketAddr, String) {
        let listener = rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = test_path("tervo-native-ssh-srv");
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("hostkey");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .expect("ssh-keygen for test host key");
        assert!(status.success());
        let key = load_secret_key(&key_path, None).expect("load test host key");
        let public = key.public_key().to_openssh().unwrap();
        let config = Arc::new(server::Config {
            keys: vec![key],
            inactivity_timeout: Some(Duration::from_secs(30)),
            ..<_>::default()
        });
        rt.spawn(async move {
            let _ = TestServer {
                forward_to,
                agent_accept,
                agent_probe,
                record,
                prompt,
            }
                .run_on_socket(config, &listener)
                .await;
        });
        (addr, public)
    }

    fn write_known_hosts(dir: &Path, addr: std::net::SocketAddr, public: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let known = dir.join("known_hosts");
        let key = public.split_whitespace().nth(1).unwrap();
        std::fs::write(
            &known,
            format!("[127.0.0.1]:{} ssh-ed25519 {key}\n", addr.port()),
        )
        .unwrap();
        known
    }

    fn resolved_for(addr: std::net::SocketAddr, known: std::path::PathBuf) -> ResolvedSsh {
        ResolvedSsh {
            hostname: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            proxy_jump: String::new(),
            proxy_command: String::new(),
            keepalive_interval: None,
            keepalive_max: 3,
            strict: "ask".into(),
            user_known_hosts: vec![known],
            global_known_hosts: Vec::new(),
            host_key_alias: None,
            forward_agent: false,
            identity_files: Vec::new(),
            unsupported: None,
        }
    }

    fn read_until_ready(transport: &mut SshNativeTransport) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while !got.windows(5).any(|w| w == b"READY") {
            assert!(
                std::time::Instant::now() < deadline,
                "no READY; got={:?}",
                String::from_utf8_lossy(&got)
            );
            let n = transport.read(&mut buf).expect("read test ssh");
            assert!(n > 0, "EOF before READY: {:?}", String::from_utf8_lossy(&got));
            got.extend_from_slice(&buf[..n]);
        }
        got
    }

    #[test]
    fn native_ssh_proxy_jump_reaches_target() {
        let _guard = ENV_LOCK.lock().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (target_addr, target_pub) = start_test_server(&rt, None);
        let (jump_addr, jump_pub) = start_test_server(&rt, Some(target_addr.port()));
        let dir = test_path("tervo-native-ssh-proxy");
        std::fs::create_dir_all(&dir).unwrap();
        let known = dir.join("known_hosts");
        let target_key = target_pub.split_whitespace().nth(1).unwrap();
        let jump_key = jump_pub.split_whitespace().nth(1).unwrap();
        std::fs::write(
            &known,
            format!(
                "[127.0.0.1]:{} ssh-ed25519 {target_key}\n[127.0.0.1]:{} ssh-ed25519 {jump_key}\n",
                target_addr.port(),
                jump_addr.port()
            ),
        )
        .unwrap();
        let config = dir.join("config");
        std::fs::write(
            &config,
            format!(
                "Host *\n UserKnownHostsFile {}\n StrictHostKeyChecking yes\n",
                known.display()
            ),
        )
        .unwrap();
        let old = std::env::var_os("TERVO_SSH_CONFIG");
        std::env::set_var("TERVO_SSH_CONFIG", &config);
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: target_addr.port(),
            user: "tester".into(),
            auth: "password".into(),
            proxy_jump: format!("tester@127.0.0.1:{}", jump_addr.port()),
            ..Profile::default()
        };
        let resolved = ResolvedSsh {
            hostname: "127.0.0.1".into(),
            port: target_addr.port(),
            user: "tester".into(),
            proxy_jump: format!("tester@127.0.0.1:{}", jump_addr.port()),
            proxy_command: String::new(),
            keepalive_interval: None,
            keepalive_max: 3,
            strict: "ask".into(),
            user_known_hosts: vec![known.clone()],
            global_known_hosts: Vec::new(),
            host_key_alias: None,
            forward_agent: false,
            identity_files: Vec::new(),
            unsupported: None,
        };
        let transport = SshNativeTransport::start_resolved(
            &profile,
            Some("pw".into()),
            None,
            80,
            24,
            resolved,
        )
        .expect("start proxy jump");
        let mut transport = transport;
        let mut buf = [0u8; 256];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while !got.windows(5).any(|w| w == b"READY") {
            assert!(std::time::Instant::now() < deadline, "no READY via jump");
            let n = transport.read(&mut buf).expect("read via jump");
            assert!(n > 0, "EOF via jump: {:?}", String::from_utf8_lossy(&got));
            got.extend_from_slice(&buf[..n]);
        }
        transport.write_all(b"jump-ok").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut echo = Vec::new();
        while !echo.windows(7).any(|w| w == b"jump-ok") {
            assert!(std::time::Instant::now() < deadline, "no echo via jump");
            let n = transport.read(&mut buf).expect("read echo via jump");
            assert!(n > 0, "EOF before echo via jump");
            echo.extend_from_slice(&buf[..n]);
        }
        transport.close();
        match old {
            Some(value) => std::env::set_var("TERVO_SSH_CONFIG", value),
            None => std::env::remove_var("TERVO_SSH_CONFIG"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_ssh_login_scripts_run_in_order_on_remote_shell() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let record = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let (addr, public) = start_test_server_with(
            &rt,
            None,
            Some(record.clone()),
            Some("PROMPT> ".into()),
        );
        let dir = test_path("tervo-native-ssh-login");
        std::fs::create_dir_all(&dir).unwrap();
        let known = dir.join("known_hosts");
        let key = public.split_whitespace().nth(1).unwrap();
        std::fs::write(
            &known,
            format!("[127.0.0.1]:{} ssh-ed25519 {key}\n", addr.port()),
        )
        .unwrap();
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            auth: "password".into(),
            login_scripts: vec![
                "echo first ; wait=PROMPT> ; timeout=3000".into(),
                "echo second ; delay=10 ; wait=PROMPT> ; timeout=3000".into(),
            ],
            ..Profile::default()
        };
        let resolved = ResolvedSsh {
            hostname: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            proxy_jump: String::new(),
            proxy_command: String::new(),
            keepalive_interval: None,
            keepalive_max: 3,
            strict: "ask".into(),
            user_known_hosts: vec![known],
            global_known_hosts: Vec::new(),
            host_key_alias: None,
            forward_agent: false,
            identity_files: Vec::new(),
            unsupported: None,
        };
        let mut transport = SshNativeTransport::start_resolved(
            &profile,
            Some("pw".into()),
            None,
            80,
            24,
            resolved,
        )
        .expect("start login-script ssh");
        let mut buf = [0u8; 256];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while !got.windows(5).any(|w| w == b"READY") {
            assert!(std::time::Instant::now() < deadline, "no READY login-script ssh");
            let n = transport.read(&mut buf).expect("read READY");
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let text = String::from_utf8_lossy(&record.lock().unwrap()).to_string();
            if text.contains("echo first") && text.contains("echo second") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "server did not receive both login commands; got={text:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = String::from_utf8_lossy(&record.lock().unwrap()).to_string();
        let first = text.find("echo first").expect("first command recorded");
        let second = text.find("echo second").expect("second command recorded");
        assert!(first < second, "commands out of order: {text:?}");
        assert!(text.contains("echo first\r"), "missing PTY Enter after first: {text:?}");
        assert!(text.contains("echo second\r"), "missing PTY Enter after second: {text:?}");
        transport.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_ssh_local_forward_reaches_local_echo() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let echo_listener = rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0))).unwrap();
        let echo_port = echo_listener.local_addr().unwrap().port();
        rt.spawn(async move {
            loop {
                let Ok((mut tcp, _)) = echo_listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = [0u8; 256];
                    loop {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let Ok(n) = tcp.read(&mut buf).await else { break };
                        if n == 0 || tcp.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let (addr, public) = start_test_server(&rt, Some(echo_port));
        let dir = test_path("tervo-native-ssh-lf");
        std::fs::create_dir_all(&dir).unwrap();
        let known = dir.join("known_hosts");
        let key = public.split_whitespace().nth(1).unwrap();
        std::fs::write(
            &known,
            format!("[127.0.0.1]:{} ssh-ed25519 {key}\n", addr.port()),
        )
        .unwrap();
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let local_port = probe.local_addr().unwrap().port();
        drop(probe);
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            auth: "password".into(),
            local_forwards: vec![format!("127.0.0.1:{local_port}:127.0.0.1:{echo_port}")],
            ..Profile::default()
        };
        let resolved = ResolvedSsh {
            hostname: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            proxy_jump: String::new(),
            proxy_command: String::new(),
            keepalive_interval: None,
            keepalive_max: 3,
            strict: "ask".into(),
            user_known_hosts: vec![known],
            global_known_hosts: Vec::new(),
            host_key_alias: None,
            forward_agent: false,
            identity_files: Vec::new(),
            unsupported: None,
        };
        let mut transport = SshNativeTransport::start_resolved(
            &profile,
            Some("pw".into()),
            None,
            80,
            24,
            resolved,
        )
        .expect("start local forward");
        let mut buf = [0u8; 256];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while !got.windows(5).any(|w| w == b"READY") {
            assert!(std::time::Instant::now() < deadline, "no READY local forward");
            let n = transport.read(&mut buf).expect("read READY");
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        let local = std::net::TcpStream::connect(("127.0.0.1", local_port))
            .expect("connect local forward");
        local.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        use std::io::{Read, Write};
        (&local).write_all(b"local-fwd").unwrap();
        let mut echoed = [0u8; 9];
        (&local).read_exact(&mut echoed).expect("read local forward echo");
        assert_eq!(&echoed, b"local-fwd");
        transport.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_ssh_remote_forward_reaches_local_echo() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let echo_listener = rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0))).unwrap();
        let echo_port = echo_listener.local_addr().unwrap().port();
        rt.spawn(async move {
            loop {
                let Ok((mut tcp, _)) = echo_listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = [0u8; 256];
                    loop {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let Ok(n) = tcp.read(&mut buf).await else { break };
                        if n == 0 || tcp.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let (addr, public) = start_test_server(&rt, Some(0));
        let dir = test_path("tervo-native-ssh-rf");
        std::fs::create_dir_all(&dir).unwrap();
        let known = dir.join("known_hosts");
        let key = public.split_whitespace().nth(1).unwrap();
        std::fs::write(
            &known,
            format!("[127.0.0.1]:{} ssh-ed25519 {key}\n", addr.port()),
        )
        .unwrap();
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let remote_port = probe.local_addr().unwrap().port();
        drop(probe);
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            auth: "password".into(),
            remote_forwards: vec![format!("127.0.0.1:{remote_port}:127.0.0.1:{echo_port}")],
            ..Profile::default()
        };
        let resolved = ResolvedSsh {
            hostname: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            proxy_jump: String::new(),
            proxy_command: String::new(),
            keepalive_interval: None,
            keepalive_max: 3,
            strict: "ask".into(),
            user_known_hosts: vec![known],
            global_known_hosts: Vec::new(),
            host_key_alias: None,
            forward_agent: false,
            identity_files: Vec::new(),
            unsupported: None,
        };
        let mut transport = SshNativeTransport::start_resolved(
            &profile,
            Some("pw".into()),
            None,
            80,
            24,
            resolved,
        )
        .expect("start remote forward");
        let mut buf = [0u8; 256];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while !got.windows(5).any(|w| w == b"READY") {
            assert!(std::time::Instant::now() < deadline, "no READY remote forward");
            let n = transport.read(&mut buf).expect("read READY");
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        let mut remote = std::net::TcpStream::connect(("127.0.0.1", remote_port))
            .expect("connect remote forward");
        remote.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        use std::io::{Read, Write};
        remote.write_all(b"remote-fwd").unwrap();
        let mut echoed = [0u8; 10];
        remote.read_exact(&mut echoed).expect("read remote forward echo");
        assert_eq!(&echoed, b"remote-fwd");
        transport.close();
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn native_ssh_uses_identity_file_from_config() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (addr, public) = start_test_server(&rt, None);
        let dir = test_path("tervo-native-ssh-idfile");
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("client_key");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .expect("ssh-keygen for client key");
        assert!(status.success());
        let known = dir.join("known_hosts");
        let host_key = public.split_whitespace().nth(1).unwrap();
        std::fs::write(
            &known,
            format!("[127.0.0.1]:{} ssh-ed25519 {host_key}\n", addr.port()),
        )
        .unwrap();
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            auth: "agent".into(),
            ..Profile::default()
        };
        let resolved = ResolvedSsh {
            hostname: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            proxy_jump: String::new(),
            proxy_command: String::new(),
            keepalive_interval: None,
            keepalive_max: 3,
            strict: "ask".into(),
            user_known_hosts: vec![known],
            global_known_hosts: Vec::new(),
            host_key_alias: None,
            forward_agent: false,
            identity_files: vec![key_path.display().to_string()],
            unsupported: None,
        };
        let mut transport = SshNativeTransport::start_resolved(
            &profile,
            None,
            None,
            80,
            24,
            resolved,
        )
        .expect("start identity-file ssh");
        let mut buf = [0u8; 64];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while !got.windows(5).any(|w| w == b"READY") {
            assert!(
                std::time::Instant::now() < deadline,
                "no READY with identity file; got={:?}",
                String::from_utf8_lossy(&got)
            );
            let n = transport.read(&mut buf).expect("read identity-file ssh");
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        transport.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_until(transport: &mut SshNativeTransport, needle: &[u8]) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while !got.windows(needle.len()).any(|w| w == needle) {
            assert!(
                std::time::Instant::now() < deadline,
                "no {:?}; got={:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&got)
            );
            let n = transport.read(&mut buf).expect("read test ssh");
            assert!(n > 0, "EOF before {:?}", String::from_utf8_lossy(needle));
            got.extend_from_slice(&buf[..n]);
        }
        got
    }

    fn encrypted_client_key(dir: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("client_key");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "s3cret", "-f"])
            .arg(&path)
            .status()
            .expect("ssh-keygen for encrypted client key");
        assert!(status.success());
        path
    }

    #[test]
    fn native_ssh_accepts_passphrase_from_prompt_and_reports_wrong_one() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (addr, public) = start_test_server(&rt, None);
        let dir = test_path("tervo-native-ssh-passphrase");
        let _home = EnvGuard::set("HOME", &dir);
        // 隔离默认密钥：错误口令必须真的失败，不能碰巧用 ~/.ssh 里的无口令
        // 默认私钥通过测试 server 的“接受任意公钥”。
        let key_path = encrypted_client_key(&dir);
        let known = write_known_hosts(&dir, addr, &public);
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            auth: "key".into(),
            key_path: key_path.display().to_string(),
            ..Profile::default()
        };
        let mut transport = SshNativeTransport::start_resolved(
            &profile,
            None,
            Some("s3cret".into()),
            80,
            24,
            resolved_for(addr, known.clone()),
        )
        .expect("start with prompt passphrase");
        read_until_ready(&mut transport);
        transport.close();

        let mut wrong = SshNativeTransport::start_resolved(
            &profile,
            None,
            Some("definitely-wrong".into()),
            80,
            24,
            resolved_for(addr, known),
        )
        .expect("start with wrong passphrase");
        let wrong_status = wrong.status();
        let mut buf = [0u8; 256];
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(std::time::Instant::now() < deadline, "wrong pass did not EOF");
            let n = wrong.read(&mut buf).expect("read wrong-pass ssh");
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8_lossy(&got);
        assert!(
            text.contains("加载私钥") || text.contains("认证失败"),
            "unexpected wrong-pass output: {text}"
        );
        // 口令本身绝不能进日志/终端输出。
        assert!(!text.contains("definitely-wrong"), "{text}");
        assert!(!text.contains("s3cret"), "{text}");
        assert!(wrong_status.key_passphrase_failed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_ssh_agent_forward_rejection_keeps_session_and_gates_channel() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (probe_tx, mut probe_rx) = tokio::sync::mpsc::channel(1);
        let (addr, public) = start_test_server_full(&rt, None, false, Some(probe_tx));
        let dir = test_path("tervo-native-ssh-agent-denied");
        let known = write_known_hosts(&dir, addr, &public);
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            auth: "password".into(),
            agent_forward: true,
            ..Profile::default()
        };
        let mut transport = SshNativeTransport::start_resolved(
            &profile,
            Some("pw".into()),
            None,
            80,
            24,
            resolved_for(addr, known),
        )
        .expect("start agent-reject ssh");
        let status = transport.status();
        read_until_ready(&mut transport);
        assert_eq!(status.agent_forward(), AgentForwardState::Denied);
        let accepted = rt
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), probe_rx.recv()).await
            })
            .expect("agent probe timed out");
        assert_eq!(accepted, Some(false), "denied negotiation must not accept agent channel");
        // 拒绝只影响 agent，shell 仍可用。
        transport.write_all(b"still-alive").unwrap();
        read_until(&mut transport, b"still-alive");
        transport.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_ssh_agent_forward_success_is_reported_and_accepts_channel() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (probe_tx, mut probe_rx) = tokio::sync::mpsc::channel(1);
        let (addr, public) = start_test_server_full(&rt, None, true, Some(probe_tx));
        let dir = test_path("tervo-native-ssh-agent-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                drop(stream);
            }
        });
        let _sock = EnvGuard::set("SSH_AUTH_SOCK", &sock);
        let known = write_known_hosts(&dir, addr, &public);
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: "tester".into(),
            auth: "password".into(),
            agent_forward: true,
            ..Profile::default()
        };
        let mut transport = SshNativeTransport::start_resolved(
            &profile,
            Some("pw".into()),
            None,
            80,
            24,
            resolved_for(addr, known),
        )
        .expect("start agent-accept ssh");
        let status = transport.status();
        read_until_ready(&mut transport);
        assert_eq!(status.agent_forward(), AgentForwardState::Active);
        let accepted = rt
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), probe_rx.recv()).await
            })
            .expect("agent probe timed out");
        assert_eq!(accepted, Some(true), "accepted negotiation should accept agent channel");
        transport.close();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
