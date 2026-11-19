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

use crate::known_hosts::{self, KnownKey};
use crate::profile::Profile;

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

/// 当前 native 后端支持的最小 Profile 子集。端口转发/ProxyJump/agent
/// forwarding/extra_args 需要系统 ssh 或后续 russh 实现；命中时回退系统 ssh，
/// 而不是静默忽略用户配置。
pub fn can_use(profile: &Profile) -> bool {
    let basic = profile.is_native_ssh()
        && profile.proxy_jump.trim().is_empty()
        && profile.local_forwards.is_empty()
        && profile.remote_forwards.is_empty()
        && profile.dynamic_forwards.is_empty()
        && !profile.agent_forward
        && profile.extra_args.iter().all(|arg| arg.trim().is_empty());
    if !basic {
        return false;
    }
    // 加密私钥需要 passphrase UI；native 暂不支持，回退系统 ssh 让它在
    // 终端里提示。auth=password 的 Vault 可用性由 create_pane 再判断。
    if profile.auth == "key" {
        let path = profile.key_path.trim();
        if path.is_empty() || load_secret_key(Path::new(path), None).is_err() {
            return false;
        }
    }
    // 首次连接没有 known_hosts 条目时回退系统 ssh，让用户在终端确认指纹；
    // native 不在没有确认的情况下 TOFU 信任。
    let effective = known_hosts::effective_ssh(profile);
    if !effective.proxy_jump.is_empty() || !effective.proxy_command.is_empty() {
        return false;
    }
    // ssh_config 的 HostName/Port/User 别名：native 现在会用 effective 值连接，
    // 但仍可能有 IdentityFile 等 native 未解析的选项；保守地要求 profile 与
    // effective 一致，避免静默绕过用户配置。
    if effective.hostname != profile.host.trim()
        || effective.port != profile.port
        || (!effective.user.is_empty() && effective.user != profile.user.trim())
    {
        return false;
    }
    if profile.auth == "agent"
        && std::env::var_os("SSH_AUTH_SOCK").is_none()
    {
        return false;
    }
    if !known_hosts::revoked_keys(profile).is_empty() {
        // 有 revoked 条目时交给系统 ssh 做完整 host-pattern 判断。
        return false;
    }
    !known_hosts::local_known_entries(profile)
        .unwrap_or_default()
        .is_empty()
}

impl SshNativeTransport {
    /// 立即返回 transport；连接/认证失败会以文本 + EOF 出现在 pane 里。
    pub fn start(profile: &Profile, password: Option<String>, cols: u16, rows: u16) -> io::Result<Self> {
        let profile = profile.clone();
        if profile.host.trim().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "SSH 主机不能为空"));
        }
        let effective = known_hosts::effective_ssh(&profile);
        if !effective.proxy_jump.is_empty() || !effective.proxy_command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native SSH 不支持 ProxyJump/ProxyCommand；请用 transport=system",
            ));
        }
        let known = known_hosts::local_known_entries(&profile).unwrap_or_default();
        let revoked = known_hosts::revoked_keys(&profile);
        if known.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                " known_hosts 中没有此主机；请先用普通 SSH 确认主机密钥（或设置 transport=system）",
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
                    known,
                    revoked,
                    effective,
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

struct HostKeyHandler {
    known: Vec<KnownKey>,
    revoked: Vec<KnownKey>,
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
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    profile: Profile,
    password: Option<String>,
    known: Vec<KnownKey>,
    revoked: Vec<KnownKey>,
    effective: known_hosts::EffectiveSsh,
    resize: Arc<AtomicU32>,
    cols: u16,
    rows: u16,
    out: tokio::sync::mpsc::Sender<Option<Vec<u8>>>,
    mut commands: tokio::sync::mpsc::Receiver<Cmd>,
    closed: Arc<AtomicBool>,
) -> Result<(), String> {
    let host = effective.hostname.clone();
    let port = effective.port.max(1);
    let user = if effective.user.trim().is_empty() {
        std::env::var("USER").unwrap_or_else(|_| "root".into())
    } else {
        effective.user.clone()
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(INACTIVITY_TIMEOUT),
        ..<_>::default()
    });
    let handler = HostKeyHandler { known, revoked };
    let connect = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(config, (host.as_str(), port), handler),
    );
    let mut session = connect
        .await
        .map_err(|_| format!("连接 {host}:{port} 超时（{}s）", CONNECT_TIMEOUT.as_secs()))?
        .map_err(|error| format!("连接 {host}:{port} 失败: {error}"))?;

    authenticate(&mut session, &profile, &user, password).await?;

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
    channel
        .request_shell(true)
        .await
        .map_err(|error| format!("请求远端 shell 失败: {error}"))?;
    await_request(&mut channel, &out, &closed).await?;
    let mut last_resize = unpack_resize(resize.load(Ordering::Acquire));

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
                        let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
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
            let key = load_secret_key(Path::new(path), None)
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

    #[test]
    fn proxy_or_forwarding_falls_back_to_system_ssh() {
        let mut p = Profile { kind: "ssh".into(), ..Profile::default() };
        // 默认没有 known_hosts 条目时也会回退；这里只验证配置门槛。
        p.proxy_jump = "jump.example".into();
        assert!(!can_use(&p));
        p.proxy_jump.clear();
        p.local_forwards.push("8080:localhost:80".into());
        assert!(!can_use(&p));
    }

    #[test]
    fn missing_known_host_is_rejected_before_connect() {
        let p = Profile {
            kind: "ssh".into(),
            host: "example.invalid".into(),
            user: "nobody".into(),
            ..Profile::default()
        };
        let err = match SshNativeTransport::start(&p, None, 80, 24) {
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

    struct TestServer;

    impl server::Server for TestServer {
        type Handler = TestHandler;
        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
            TestHandler
        }
    }

    struct TestHandler;

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
        let dir = std::env::temp_dir().join(format!(
            "tervo-native-ssh-{}-{}",
            std::process::id(),
            crate::logging::now_ms()
        ));
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
            let _ = TestServer.run_on_socket(config, &listener).await;
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
        let transport = SshNativeTransport::start(&profile, Some("pw".into()), 80, 24)
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
}
