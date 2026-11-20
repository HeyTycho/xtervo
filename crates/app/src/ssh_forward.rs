// SPDX-License-Identifier: GPL-3.0-only
//! native SSH 端口转发（本地/动态/远程）与运行状态句柄。
//!
//! 所有 listener 都绑定 127.0.0.1（本地/动态）或按 `-R` 规格绑定（远程）。
//! 每个连接通过 russh `direct-tcpip` / forwarded channel 桥接；任务在会话
//! 结束时由调用方 abort。
//!
//! [`ForwardController`] 是给 UI 的可查询句柄：规则/状态放在共享 Mutex 里，
//! 启停通过 tokio mpsc 发回 SSH 会话 runtime；UI 线程不需要也不能直接
//! 操作 russh 连接。只有 native 当前会话的 local/dynamic listener 支持运行时
//! 启停；remote 只报告服务器分配的端口/状态，不伪造取消能力。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use russh::client;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::profile::Profile;
use crate::ssh_native::HostKeyHandler;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardKind {
    Local,
    Remote,
    Dynamic,
}

impl ForwardKind {
    /// 单字母前缀，与 `ssh -L/-R/-D` 一致。
    pub fn short(self) -> &'static str {
        match self {
            Self::Local => "L",
            Self::Remote => "R",
            Self::Dynamic => "D",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardState {
    /// Profile 里有规则，但当前会话还没开始监听（连接中/未连接）。
    Pending,
    /// listener 已绑定（remote 已获服务器确认）。
    Active,
    /// 用户在当前会话里停止了 listener（remote 不使用此状态）。
    Stopped,
    /// 绑定/请求失败；`ForwardRule::error` 带原因。
    Failed,
}

impl ForwardState {
    pub fn is_active(self) -> bool {
        self == Self::Active
    }
}

/// 一条转发的当前状态。由 [`ForwardController`] 共享给 UI 与会话 runtime。
#[derive(Clone, Debug)]
pub struct ForwardRule {
    pub id: u64,
    pub kind: ForwardKind,
    /// Profile 中保存的原始规格（不含 `-L/-R/-D`）。
    pub spec: String,
    pub bind: String,
    /// 请求的监听端口（remote port 0 = 由服务器分配）。
    pub port: u16,
    /// 实际使用的端口；remote port=0 后是服务器分配的端口。
    pub assigned_port: Option<u16>,
    /// 本机/远端目标主机；remote 规则里是本地目标。
    pub target_host: String,
    pub target_port: u16,
    pub state: ForwardState,
    /// true = 当前会话可运行时启停（native local/dynamic）。
    pub controllable: bool,
    pub error: Option<String>,
}

impl ForwardRule {
    pub fn effective_port(&self) -> u16 {
        self.assigned_port.unwrap_or(self.port)
    }

    /// `bind:port`（remote 显示分配后的端口）。
    pub fn listen(&self) -> String {
        format!("{}:{}", self.bind, self.effective_port())
    }

    /// `target_host:target_port`；动态转发没有目标。
    pub fn target(&self) -> Option<String> {
        (self.kind != ForwardKind::Dynamic)
            .then(|| format!("{}:{}", self.target_host, self.target_port))
    }
}

/// UI → SSH 会话 runtime 的控制命令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardCommand {
    Start(u64),
    Stop(u64),
}

struct ForwardInner {
    rules: Mutex<Vec<ForwardRule>>,
    commands: tokio::sync::mpsc::UnboundedSender<ForwardCommand>,
}

/// 转发状态/控制句柄；克隆廉价，可跨线程共享。
#[derive(Clone)]
pub struct ForwardController {
    inner: Arc<ForwardInner>,
}

impl ForwardController {
    /// 从 Profile 生成初始规则（全部 Pending/Failed），并返回控制命令接收端。
    ///
    /// 规则解析失败不 panic：保留一条 Failed 状态，UI 能看到具体规则与原因。
    pub fn new(profile: &Profile) -> (Self, tokio::sync::mpsc::UnboundedReceiver<ForwardCommand>) {
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut rules = Vec::new();
        let mut id = 0u64;

        for spec in &profile.local_forwards {
            id += 1;
            let mut rule = ForwardRule {
                id,
                kind: ForwardKind::Local,
                spec: spec.clone(),
                bind: String::new(),
                port: 0,
                assigned_port: None,
                target_host: String::new(),
                target_port: 0,
                state: ForwardState::Failed,
                controllable: false,
                error: None,
            };
            match parse_local(spec) {
                Some(spec) => {
                    rule.bind = spec.bind;
                    rule.port = spec.port;
                    rule.target_host = spec.target_host;
                    rule.target_port = spec.target_port;
                    rule.state = ForwardState::Pending;
                    rule.controllable = true;
                },
                None => rule.error = Some("本地转发格式无效".into()),
            }
            rules.push(rule);
        }
        for spec in &profile.remote_forwards {
            id += 1;
            let mut rule = ForwardRule {
                id,
                kind: ForwardKind::Remote,
                spec: spec.clone(),
                bind: String::new(),
                port: 0,
                assigned_port: None,
                target_host: String::new(),
                target_port: 0,
                state: ForwardState::Failed,
                controllable: false,
                error: None,
            };
            match parse_remote(spec) {
                Some(spec) => {
                    rule.bind = spec.bind;
                    rule.port = spec.port;
                    rule.target_host = spec.target_host;
                    rule.target_port = spec.target_port;
                    rule.state = ForwardState::Pending;
                },
                None => rule.error = Some("远程转发格式无效".into()),
            }
            rules.push(rule);
        }
        for spec in &profile.dynamic_forwards {
            id += 1;
            let mut rule = ForwardRule {
                id,
                kind: ForwardKind::Dynamic,
                spec: spec.clone(),
                bind: String::new(),
                port: 0,
                assigned_port: None,
                target_host: String::new(),
                target_port: 0,
                state: ForwardState::Failed,
                controllable: false,
                error: None,
            };
            match parse_dynamic(spec) {
                Some(spec) => {
                    rule.bind = spec.bind;
                    rule.port = spec.port;
                    rule.state = ForwardState::Pending;
                    rule.controllable = true;
                },
                None => rule.error = Some("动态转发格式无效".into()),
            }
            rules.push(rule);
        }

        (
            Self { inner: Arc::new(ForwardInner { rules: Mutex::new(rules), commands }) },
            receiver,
        )
    }

    /// 当前全部规则（按 Profile 顺序 L → R → D）。
    pub fn rules(&self) -> Vec<ForwardRule> {
        self.inner.rules.lock().map(|rules| rules.clone()).unwrap_or_default()
    }

    pub fn rule(&self, id: u64) -> Option<ForwardRule> {
        self.inner
            .rules
            .lock()
            .ok()
            .and_then(|rules| rules.iter().find(|rule| rule.id == id).cloned())
    }

    /// 请求启动当前会话的一条 local/dynamic 规则。
    ///
    /// 状态先置 Pending；SSH runtime 绑定成功后置 Active，失败置 Failed。
    pub fn start(&self, id: u64) -> Result<(), String> {
        self.request(id, true)
    }

    /// 请求停止当前会话的一条 local/dynamic 规则。
    pub fn stop(&self, id: u64) -> Result<(), String> {
        self.request(id, false)
    }

    fn request(&self, id: u64, start: bool) -> Result<(), String> {
        let mut rules = self.inner.rules.lock().map_err(|_| "转发表锁损坏".to_string())?;
        let Some(rule) = rules.iter_mut().find(|rule| rule.id == id) else {
            return Err(format!("转发规则不存在: {id}"));
        };
        if !rule.controllable {
            return Err(format!(
                "-{} {} 不支持当前会话运行时启停",
                rule.kind.short(),
                rule.spec
            ));
        }
        if start {
            if rule.state == ForwardState::Active {
                return Ok(());
            }
            rule.state = ForwardState::Pending;
            rule.error = None;
        }
        self.inner
            .commands
            .send(if start {
                ForwardCommand::Start(id)
            } else {
                ForwardCommand::Stop(id)
            })
            .map_err(|_| "SSH 会话已结束，无法控制转发".to_string())
    }

    pub(crate) fn mark_active(&self, id: u64, assigned_port: u16) {
        self.update(id, |rule| {
            rule.state = ForwardState::Active;
            rule.assigned_port = Some(assigned_port);
            rule.error = None;
        });
    }

    pub(crate) fn mark_stopped(&self, id: u64) {
        self.update(id, |rule| {
            rule.state = ForwardState::Stopped;
            rule.error = None;
        });
    }

    pub(crate) fn mark_failed(&self, id: u64, error: impl Into<String>) {
        let error = error.into();
        self.update(id, |rule| {
            rule.state = ForwardState::Failed;
            rule.error = Some(error);
        });
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut ForwardRule)) {
        if let Ok(mut rules) = self.inner.rules.lock() {
            if let Some(rule) = rules.iter_mut().find(|rule| rule.id == id) {
                f(rule);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalSpec {
    pub bind: String,
    pub port: u16,
    pub target_host: String,
    pub target_port: u16,
}

pub fn parse_local(spec: &str) -> Option<LocalSpec> {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() == 3 {
        Some(LocalSpec {
            bind: "127.0.0.1".into(),
            port: parts[0].parse().ok()?,
            target_host: parts[1].into(),
            target_port: parts[2].parse().ok()?,
        })
    } else if parts.len() == 4 {
        Some(LocalSpec {
            bind: parts[0].into(),
            port: parts[1].parse().ok()?,
            target_host: parts[2].into(),
            target_port: parts[3].parse().ok()?,
        })
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicSpec {
    pub bind: String,
    pub port: u16,
}

pub fn parse_dynamic(spec: &str) -> Option<DynamicSpec> {
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.as_slice() {
        [port] => Some(DynamicSpec { bind: "127.0.0.1".into(), port: port.parse().ok()? }),
        [bind, port] => Some(DynamicSpec { bind: (*bind).into(), port: port.parse().ok()? }),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSpec {
    pub bind: String,
    pub port: u16,
    pub target_host: String,
    pub target_port: u16,
}

pub fn parse_remote(spec: &str) -> Option<RemoteSpec> {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() == 3 {
        Some(RemoteSpec {
            bind: "127.0.0.1".into(),
            port: parts[0].parse().ok()?,
            target_host: parts[1].into(),
            target_port: parts[2].parse().ok()?,
        })
    } else if parts.len() == 4 {
        Some(RemoteSpec {
            bind: parts[0].into(),
            port: parts[1].parse().ok()?,
            target_host: parts[2].into(),
            target_port: parts[3].parse().ok()?,
        })
    } else {
        None
    }
}

/// 启动一条本地转发 listener；返回任务句柄与真实绑定端口。
pub async fn start_local(
    handle: Arc<client::Handle<HostKeyHandler>>,
    bind: &str,
    port: u16,
    target_host: &str,
    target_port: u16,
) -> Result<(JoinHandle<()>, u16), String> {
    let listener = TcpListener::bind((bind, port))
        .await
        .map_err(|error| format!("绑定本地转发 {bind}:{port} 失败: {error}"))?;
    let bound = listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|error| format!("读取本地转发 {bind}:{port} 监听地址失败: {error}"))?;
    let target_host = target_host.to_string();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut tcp, origin)) = listener.accept().await else { break };
            let session = handle.clone();
            let target_host = target_host.clone();
            tokio::spawn(async move {
                let Ok(channel) = session
                    .channel_open_direct_tcpip(
                        &target_host,
                        target_port as u32,
                        origin.ip().to_string(),
                        origin.port() as u32,
                    )
                    .await
                else {
                    return;
                };
                let mut stream = channel.into_stream();
                let _ = copy_bidirectional(&mut tcp, &mut stream).await;
            });
        }
    });
    Ok((task, bound))
}

/// 启动一条 SOCKS5 动态转发 listener；返回任务句柄与真实绑定端口。
pub async fn start_dynamic(
    handle: Arc<client::Handle<HostKeyHandler>>,
    bind: &str,
    port: u16,
) -> Result<(JoinHandle<()>, u16), String> {
    let listener = TcpListener::bind((bind, port))
        .await
        .map_err(|error| format!("绑定 SOCKS {bind}:{port} 失败: {error}"))?;
    let bound = listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|error| format!("读取 SOCKS {bind}:{port} 监听地址失败: {error}"))?;
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else { break };
            let session = handle.clone();
            tokio::spawn(async move {
                let Ok((host, port)) = socks5_handshake(&mut tcp).await else { return };
                let Ok(channel) = session
                    .channel_open_direct_tcpip(&host, port as u32, "127.0.0.1", 0)
                    .await
                else {
                    let _ = socks5_reply(&mut tcp, 0x01).await;
                    return;
                };
                if socks5_reply(&mut tcp, 0x00).await.is_err() {
                    return;
                }
                let mut stream = channel.into_stream();
                let _ = copy_bidirectional(&mut tcp, &mut stream).await;
            });
        }
    });
    Ok((task, bound))
}

/// 请求一条远程转发，返回服务器确认/分配的端口。
pub async fn start_remote(
    handle: &Arc<client::Handle<HostKeyHandler>>,
    bind: &str,
    port: u16,
    target_host: &str,
    target_port: u16,
    targets: &Arc<Mutex<HashMap<(String, u16), (String, u16)>>>,
) -> Result<u16, String> {
    // port=0 用 (bind,0) 占位，收到服务器分配端口后再补精确 key。
    // 同一 bind 只允许一个 pending port=0，避免把服务器开的任意通道
    // 误桥接到某个目标。
    if port == 0 {
        let map = targets.lock().map_err(|_| "远程转发表锁损坏".to_string())?;
        if map.contains_key(&(bind.to_string(), 0)) {
            return Err(format!(
                "远程转发 {bind} 已有待分配端口，暂不支持同一 bind 多个 port 0"
            ));
        }
    }
    {
        let mut map = targets.lock().map_err(|_| "远程转发表锁损坏".to_string())?;
        map.insert((bind.to_string(), port), (target_host.to_string(), target_port));
    }
    let assigned = match handle.tcpip_forward(bind, port as u32).await {
        Ok(assigned) => assigned,
        Err(error) => {
            if let Ok(mut map) = targets.lock() {
                map.remove(&(bind.to_string(), port));
            }
            return Err(format!("请求远程转发 {bind}:{port} 失败: {error}"));
        },
    };
    let assigned = assigned as u16;
    if assigned != port {
        let mut map = targets.lock().map_err(|_| "远程转发表锁损坏".to_string())?;
        map.insert(
            (bind.to_string(), assigned),
            (target_host.to_string(), target_port),
        );
        map.remove(&(bind.to_string(), 0));
    }
    eprintln!(
        "[ssh-forward] remote bind={} requested={} assigned={}",
        bind, port, assigned
    );
    Ok(assigned)
}

async fn socks5_handshake(stream: &mut TcpStream) -> std::io::Result<(String, u16)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(std::io::Error::other("SOCKS5 only"));
    }
    let mut methods = vec![0u8; head[1] as usize];
    stream.read_exact(&mut methods).await?;
    stream.write_all(&[5, 0]).await?;
    let mut req = [0u8; 4];
    stream.read_exact(&mut req).await?;
    if req[0] != 5 || req[1] != 1 {
        return Err(std::io::Error::other("SOCKS5 CONNECT only"));
    }
    let host = match req[3] {
        1 => {
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        },
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            stream.read_exact(&mut name).await?;
            String::from_utf8_lossy(&name).into_owned()
        },
        4 => {
            let mut ip = [0u8; 16];
            stream.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        },
        _ => return Err(std::io::Error::other("SOCKS5 atyp")),
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await?;
    Ok((host, u16::from_be_bytes(port)))
}

async fn socks5_reply(stream: &mut TcpStream, code: u8) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    stream.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    stream.flush().await
}

pub async fn bridge_forwarded(
    channel: russh::Channel<russh::client::Msg>,
    target: (String, u16),
) {
    let Ok(mut tcp) = TcpStream::connect((target.0.as_str(), target.1)).await else { return };
    let mut stream = channel.into_stream();
    let _ = copy_bidirectional(&mut tcp, &mut stream).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    #[test]
    fn parses_forward_specs() {
        assert_eq!(
            parse_local("8080:localhost:80"),
            Some(LocalSpec {
                bind: "127.0.0.1".into(),
                port: 8080,
                target_host: "localhost".into(),
                target_port: 80
            })
        );
        assert_eq!(parse_dynamic("1080").unwrap().port, 1080);
        assert_eq!(parse_dynamic("127.0.0.1:1080").unwrap().bind, "127.0.0.1");
        assert_eq!(parse_remote("0.0.0.0:8080:localhost:80").unwrap().port, 8080);
        assert!(parse_local("bad").is_none());
    }

    #[test]
    fn controller_exposes_profile_rules_and_control_capability() {
        let profile = Profile {
            local_forwards: vec!["8080:localhost:80".into()],
            remote_forwards: vec!["0:localhost:80".into()],
            dynamic_forwards: vec!["1080".into()],
            ..Profile::default()
        };
        let (controller, mut commands) = ForwardController::new(&profile);
        let rules = controller.rules();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].kind, ForwardKind::Local);
        assert!(rules[0].controllable);
        assert_eq!(rules[0].state, ForwardState::Pending);
        assert_eq!(rules[1].kind, ForwardKind::Remote);
        assert!(!rules[1].controllable, "remote 不支持运行时启停");
        assert_eq!(rules[2].kind, ForwardKind::Dynamic);
        assert!(rules[2].controllable);

        controller.start(rules[0].id).unwrap();
        assert_eq!(controller.rule(rules[0].id).unwrap().state, ForwardState::Pending);
        assert_eq!(commands.try_recv(), Ok(ForwardCommand::Start(rules[0].id)));
        controller.mark_active(rules[0].id, 8080);
        assert_eq!(controller.rule(rules[0].id).unwrap().state, ForwardState::Active);
        assert_eq!(controller.rule(rules[0].id).unwrap().listen(), "127.0.0.1:8080");
        controller.stop(rules[0].id).unwrap();
        assert_eq!(commands.try_recv(), Ok(ForwardCommand::Stop(rules[0].id)));
        assert!(controller.start(rules[1].id).is_err());
    }

    #[test]
    fn invalid_profile_specs_become_failed_rules_without_panic() {
        let profile = Profile {
            local_forwards: vec!["bad".into()],
            remote_forwards: vec!["also-bad".into()],
            dynamic_forwards: vec!["nope".into()],
            ..Profile::default()
        };
        let (controller, _commands) = ForwardController::new(&profile);
        let rules = controller.rules();
        assert_eq!(rules.len(), 3);
        for rule in &rules {
            assert_eq!(rule.state, ForwardState::Failed);
            assert!(rule.error.is_some());
            assert!(!rule.controllable);
        }
    }

    #[test]
    fn remote_port_zero_uses_assigned_port_for_display() {
        let profile = Profile {
            remote_forwards: vec!["0:localhost:80".into()],
            ..Profile::default()
        };
        let (controller, _commands) = ForwardController::new(&profile);
        let rule = &controller.rules()[0];
        assert_eq!(rule.port, 0);
        controller.mark_active(rule.id, 49152);
        let rule = controller.rule(rule.id).unwrap();
        assert_eq!(rule.effective_port(), 49152);
        assert_eq!(rule.listen(), "127.0.0.1:49152");
        assert_eq!(rule.target().as_deref(), Some("localhost:80"));
    }
}
