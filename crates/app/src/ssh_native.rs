// SPDX-License-Identifier: GPL-3.0-only
//! 进程内 SSH 后端（russh）：默认 SSH Profile 不再启动系统 `ssh` 子进程。
//!
//! 数据面：russh channel <-> tokio unbounded command queue <-> PaneTransport。
//! 连接/认证在专用 current-thread runtime 里执行；失败文本会作为红色输出
//! 写进 pane，然后 EOF，和旧 SSH 路径的可见性一致。

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// UI 可查询/控制本会话转发的句柄（状态共享，不持有 russh 连接）。
    forward: crate::ssh_forward::ForwardController,
}

/// 私钥是否无需口令即可加载（供 create_pane 判断是否需要 Vault 口令）。
pub fn key_loads_without_passphrase(profile: &Profile) -> bool {
    let path = profile.key_path.trim();
    !path.is_empty() && load_secret_key(Path::new(path), None).is_ok()
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

    fn start_resolved(
        profile: &Profile,
        password: Option<String>,
        key_passphrase: Option<String>,
        cols: u16,
        rows: u16,
        resolved: ResolvedSsh,
    ) -> io::Result<Self> {
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
        let SshNativeTransport { output, commands, closed, resize, pending, forward: _ } = *self;
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
            Ok(()) => agent_forward_enabled.store(true, Ordering::Release),
            Err(error) if error == "远端拒绝了 PTY/shell 请求" => {
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
            Some(ChannelMsg::Failure) => return Err("远端拒绝了 PTY/shell 请求".into()),
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

async fn try_public_key<H: Handler + Send>(
    session: &mut client::Handle<H>,
    user: &str,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<bool, String> {
    let key = match load_secret_key(path, passphrase) {
        Ok(key) => key,
        Err(error) => return Err(format!("加载私钥 {} 失败: {error}", path.display())),
    };
    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|error| format!("协商 RSA hash 失败: {error}"))?
        .flatten();
    let result = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await
        .map_err(|error| format!("公钥认证 {} 失败: {error}", path.display()))?;
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
    for path in candidates {
        match try_public_key(session, user, &path, key_passphrase).await {
            Ok(true) => return Ok(()),
            Ok(false) => errors.push(format!("{} 被服务器拒绝", path.display())),
            Err(error) => errors.push(error),
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
                record: self.record.clone(),
                prompt: self.prompt.clone(),
            }
        }
    }

    struct TestHandler {
        forward_to: Option<u16>,
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
            _channel: russh::ChannelId,
            _session: &mut Session,
        ) -> Result<bool, Self::Error> {
            Ok(false)
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
            let _ = TestServer { forward_to: None, record: None, prompt: None }
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
        start_test_server_with(rt, forward_to, None, None)
    }

    fn start_test_server_with(
        rt: &tokio::runtime::Runtime,
        forward_to: Option<u16>,
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
            let _ = TestServer { forward_to, record, prompt }
                .run_on_socket(config, &listener)
                .await;
        });
        (addr, public)
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


}
