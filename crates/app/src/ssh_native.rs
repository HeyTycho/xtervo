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

struct ForwardGuard(Vec<tokio::task::JoinHandle<()>>);
impl Drop for ForwardGuard {
    fn drop(&mut self) {
        for task in &self.0 {
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
    if profile.auth == "agent" && std::env::var_os("SSH_AUTH_SOCK").is_none() {
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
                ));
                if let Err(error) = result {
                    let _ = out_tx.blocking_send(Some(
                        format!("\r\n[ssh-native] 连接失败: {error}\r\n").into_bytes(),
                    ));
                }
                let _ = out_tx.blocking_send(None);
            })
            .map_err(|error| io::Error::other(format!("创建 ssh-native 线程失败: {error}")))?;
        Ok(Self { output: out_rx, commands: cmd_tx, closed, resize, pending: Vec::new() })
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
        let SshNativeTransport { output, commands, closed, resize, pending } = *self;
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
                map.get(&key).cloned()
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
    let mut forward_tasks = Vec::new();
    if !profile.local_forwards.is_empty() {
        forward_tasks.extend(
            crate::ssh_forward::spawn_local(session.clone(), &profile.local_forwards)
                .await
                .map_err(|error| format!("本地转发启动失败: {error}"))?,
        );
    }
    if !profile.dynamic_forwards.is_empty() {
        forward_tasks.extend(
            crate::ssh_forward::spawn_dynamic(session.clone(), &profile.dynamic_forwards)
                .await
                .map_err(|error| format!("动态转发启动失败: {error}"))?,
        );
    }
    if !profile.remote_forwards.is_empty() {
        crate::ssh_forward::setup_remote(
            &session,
            &profile.remote_forwards,
            &remote_targets,
        )
        .await?;
    }
    let mut last_resize = unpack_resize(resize.load(Ordering::Acquire));

    let _forward_guard = ForwardGuard(forward_tasks);
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

async fn authenticate<H: Handler + Send>(
    session: &mut client::Handle<H>,
    profile: &Profile,
    user: &str,
    password: Option<String>,
    key_passphrase: Option<&str>,
) -> Result<(), String> {
    let ok = match profile.auth.as_str() {
        "password" => {
            let Some(password) = password else {
                return Err(
                    "native SSH 密码认证需要把密码保存到 Vault（或设置 transport=system 使用系统 ssh）"
                        .into(),
                );
            };
            session
                .authenticate_password(user, password)
                .await
                .map_err(|error| format!("密码认证失败: {error}"))?
                .success()
        },
        "key" => {
            let path = profile.key_path.trim();
            if path.is_empty() {
                return Err("auth=key 但 key_path 为空".into());
            }
            let key = load_secret_key(Path::new(path), key_passphrase)
                .map_err(|error| format!("加载私钥 {path} 失败: {error}"))?;
            let hash = session
                .best_supported_rsa_hash()
                .await
                .map_err(|error| format!("协商 RSA hash 失败: {error}"))?
                .flatten();
            session
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|error| format!("公钥认证失败: {error}"))?
                .success()
        },
        _ => {
            let mut agent = AgentClient::connect_env()
                .await
                .map_err(|error| format!("连接 ssh-agent 失败: {error}"))?;
            let identities = agent
                .request_identities()
                .await
                .map_err(|error| format!("读取 ssh-agent identities 失败: {error}"))?;
            let mut success = false;
            for identity in identities {
                if let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity {
                    if session
                        .authenticate_publickey_with(user, key, None, &mut agent)
                        .await
                        .map(|result| result.success())
                        .unwrap_or(false)
                    {
                        success = true;
                        break;
                    }
                }
            }
            success
        },
    };
    if !ok {
        return Err(format!("SSH 认证失败（auth={}）", profile.auth));
    }
    Ok(())
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
    }

    impl server::Server for TestServer {
        type Handler = TestHandler;
        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
            TestHandler { forward_to: self.forward_to }
        }
    }

    struct TestHandler {
        forward_to: Option<u16>,
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
            let _ = TestServer { forward_to: None }
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
            let _ = TestServer { forward_to }
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

}
