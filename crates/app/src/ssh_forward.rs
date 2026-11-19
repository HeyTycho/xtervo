// SPDX-License-Identifier: GPL-3.0-only
//! native SSH 端口转发（本地/动态/远程）。
//!
//! 所有 listener 都绑定 127.0.0.1（本地/动态）或按 `-R` 规格绑定（远程）。
//! 每个连接通过 russh `direct-tcpip` / forwarded channel 桥接；任务在会话
//! 结束时由调用方 abort。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use russh::client;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::ssh_native::HostKeyHandler;

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

pub async fn spawn_local(
    handle: Arc<client::Handle<HostKeyHandler>>,
    specs: &[String],
) -> Result<Vec<JoinHandle<()>>, String> {
    let mut tasks = Vec::new();
    for raw in specs {
        let spec = parse_local(raw).ok_or_else(|| format!("本地转发格式无效: {raw}"))?;
        let listener = TcpListener::bind((spec.bind.as_str(), spec.port))
            .await
            .map_err(|error| format!("绑定本地转发 {}:{} 失败: {error}", spec.bind, spec.port))?;
        let session = handle.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((mut tcp, origin)) = listener.accept().await else { break };
                let session = session.clone();
                let target_host = spec.target_host.clone();
                let target_port = spec.target_port;
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
        }));
    }
    Ok(tasks)
}

pub async fn spawn_dynamic(
    handle: Arc<client::Handle<HostKeyHandler>>,
    specs: &[String],
) -> Result<Vec<JoinHandle<()>>, String> {
    let mut tasks = Vec::new();
    for raw in specs {
        let spec = parse_dynamic(raw).ok_or_else(|| format!("动态转发格式无效: {raw}"))?;
        let listener = TcpListener::bind((spec.bind.as_str(), spec.port))
            .await
            .map_err(|error| format!("绑定 SOCKS {}:{} 失败: {error}", spec.bind, spec.port))?;
        let session = handle.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((mut tcp, _)) = listener.accept().await else { break };
                let session = session.clone();
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
        }));
    }
    Ok(tasks)
}

pub async fn setup_remote(
    handle: &Arc<client::Handle<HostKeyHandler>>,
    specs: &[String],
    targets: &Arc<Mutex<HashMap<(String, u16), (String, u16)>>>,
) -> Result<(), String> {
    for raw in specs {
        let spec = parse_remote(raw).ok_or_else(|| format!("远程转发格式无效: {raw}"))?;
        // 固定端口先注册，避免服务器在 tcpip_forward 回复后立刻打开通道时
        // handler 查不到目标。port=0 先注册 (bind,0) 作为占位，收到实际端口后
        // 再补精确 key（handler 同时查 0 占位兜底）。
        {
            let mut map = targets.lock().map_err(|_| "远程转发表锁损坏".to_string())?;
            map.insert(
                (spec.bind.clone(), spec.port),
                (spec.target_host.clone(), spec.target_port),
            );
        }
        let assigned = match handle.tcpip_forward(&spec.bind, spec.port as u32).await {
            Ok(assigned) => assigned,
            Err(error) => {
                if let Ok(mut map) = targets.lock() {
                    map.remove(&(spec.bind.clone(), spec.port));
                }
                return Err(format!(
                    "请求远程转发 {}:{} 失败: {error}",
                    spec.bind, spec.port
                ));
            }
        };
        if assigned as u16 != spec.port {
            let mut map = targets.lock().map_err(|_| "远程转发表锁损坏".to_string())?;
            map.insert(
                (spec.bind.clone(), assigned as u16),
                (spec.target_host.clone(), spec.target_port),
            );
        }
    }
    Ok(())
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
}
