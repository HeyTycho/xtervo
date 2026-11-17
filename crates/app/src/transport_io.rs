// SPDX-License-Identifier: GPL-3.0-only
//! 插件 transport 的数据面帧（ACT-052 F3）。
//!
//! 宿主与插件进程之间用 AF_UNIX `SOCK_SEQPACKET`：一个 datagram 恰好是一个
//! 完整帧 `[u32 little-endian payload 长度][payload]`。控制面（open/resize/
//! close/错误）仍走现有 JSON-RPC；数据面不解析 JSON，也不做流式拼接。
//!
//! 约束：单帧 payload ≤ 256 KiB（读/写各一份上限）。超限或声明长度与
//! datagram 不一致一律返回错误，由调用方断开 transport；**绝不静默截断**。
//!
//! 本模块只有纯函数：不绑定 socket、不引入外部依赖，便于单独验证。
#![allow(dead_code)]

use std::fmt;

/// 单帧 payload 硬上限：256 KiB（与 `docs/PLUGIN_TRANSPORT.md` 一致）。
pub const MAX_FRAME_PAYLOAD: usize = 256 * 1024;

/// 帧头：little-endian u32 payload 长度。
pub const FRAME_HEADER_LEN: usize = 4;

/// 数据面帧错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// 声明的 payload 长度超过 256 KiB。
    Oversize { len: usize },
    /// datagram 连 4 字节帧头都不够。
    ShortDatagram { len: usize },
    /// SOCK_SEQPACKET 语义要求声明长度与 datagram 剩余字节完全相等。
    LengthMismatch { declared: usize, actual: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize { len } => {
                write!(f, "transport frame payload {len} > {MAX_FRAME_PAYLOAD} bytes")
            },
            Self::ShortDatagram { len } => {
                write!(f, "transport datagram {len} bytes < {FRAME_HEADER_LEN} byte header")
            },
            Self::LengthMismatch { declared, actual } => write!(
                f,
                "transport frame declares {declared} payload bytes but datagram carries {actual}"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

/// 编码一帧为 `[u32 LE len][payload]`。
///
/// payload 超过 [`MAX_FRAME_PAYLOAD`] 返回 `Oversize`，由调用方断开 transport
/// 并报 `transport.error`；不在这里截断（截断就是丢字节）。
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(FrameError::Oversize { len: payload.len() });
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// 解码一个完整 datagram，返回 payload 切片（零拷贝）。
///
/// SOCK_SEQPACKET 的 datagram 边界就是帧边界：多字节/少字节都按协议错误
/// 处理，绝不把相邻帧拼起来，也不接受“半帧等待后续”。
pub fn decode_frame(datagram: &[u8]) -> Result<&[u8], FrameError> {
    if datagram.len() < FRAME_HEADER_LEN {
        return Err(FrameError::ShortDatagram { len: datagram.len() });
    }
    let declared =
        u32::from_le_bytes([datagram[0], datagram[1], datagram[2], datagram[3]]) as usize;
    if declared > MAX_FRAME_PAYLOAD {
        return Err(FrameError::Oversize { len: declared });
    }
    let actual = datagram.len() - FRAME_HEADER_LEN;
    if declared != actual {
        return Err(FrameError::LengthMismatch { declared, actual });
    }
    Ok(&datagram[FRAME_HEADER_LEN..])
}

/// 只读帧头里的 payload 长度；datagram 不足 4 字节时返回 `None`。
///
/// 接收端先用它判断超限，再决定是否分配/读取整帧。
pub fn declared_payload_len(datagram: &[u8]) -> Option<usize> {
    if datagram.len() < FRAME_HEADER_LEN {
        return None;
    }
    Some(u32::from_le_bytes([datagram[0], datagram[1], datagram[2], datagram[3]]) as usize)
}
