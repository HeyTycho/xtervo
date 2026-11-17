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

// ---------------------------------------------------------------- 权限骨架（F4）

/// `permissions.terminal_transport` 门禁错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportGateError {
    /// 用户尚未确认“允许插件提供终端字节流”。
    NotConfirmed,
    /// 同一套 transport 同时只能服务一个 pane（不能跨 pane 读写）。
    PaneLimit { max: usize },
    /// 单 pane 读/写缓冲超过 256 KiB。
    BufferLimit { read: usize, write: usize, max: usize },
}

impl fmt::Display for TransportGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfirmed => f.write_str("terminal_transport permission not confirmed"),
            Self::PaneLimit { max } => {
                write!(f, "terminal_transport pane limit reached (max {max})")
            },
            Self::BufferLimit { read, write, max } => write!(
                f,
                "terminal_transport buffer limit: read {read} / write {write} > {max} bytes"
            ),
        }
    }
}

impl std::error::Error for TransportGateError {}

/// 审计动作；只记元数据，不记 payload / 凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportAuditAction {
    Confirm,
    Revoke,
    Open,
    Close,
    Reject,
}

/// 一条 transport 审计记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportAuditEntry {
    /// 单调递增序号；首次记录为 1。
    pub seq: u64,
    /// 关联 pane；确认/撤销等全局动作记 0。
    pub pane_id: u64,
    pub action: TransportAuditAction,
    pub allowed: bool,
    /// 简短原因，不含任何用户数据。
    pub detail: &'static str,
}

/// `permissions.terminal_transport` 的权限骨架：确认 + 审计 + 单 pane 上限。
///
/// 只做数据结构与门禁函数，**不改插件启用流程**。真正接线时：
/// 1. 用户首次启用 transport 插件 → [`Self::confirm`]；
/// 2. `transport.open` 前 → [`Self::authorize_open`]；
/// 3. 数据面每帧 → [`Self::check_buffers`]，超限断开并报 `transport.error`；
/// 4. `transport.close` / pane 关闭 → [`Self::close`]。
#[derive(Debug, Default)]
pub struct TerminalTransportPermission {
    confirmed: bool,
    pane: Option<u64>,
    audit: Vec<TransportAuditEntry>,
    next_seq: u64,
}

impl TerminalTransportPermission {
    /// 同一套 transport 同时最多服务 1 个 pane（不能跨 pane）。
    pub const MAX_OPEN_PANES: usize = 1;
    /// 单 pane 读/写缓冲各 ≤ 256 KiB。
    pub const MAX_BUFFER_BYTES: usize = MAX_FRAME_PAYLOAD;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_confirmed(&self) -> bool {
        self.confirmed
    }

    /// 当前被授权的 pane（未打开时为 `None`）。
    pub fn open_pane(&self) -> Option<u64> {
        self.pane
    }

    /// 用户首次启用时确认。重复确认幂等，但每次都会留审计。
    pub fn confirm(&mut self) {
        self.confirmed = true;
        self.record(0, TransportAuditAction::Confirm, true, "user confirmed");
    }

    /// 撤销授权：清空已打开 pane，后续 `authorize_open` 全部拒绝。
    pub fn revoke(&mut self) {
        self.confirmed = false;
        self.pane = None;
        self.record(0, TransportAuditAction::Revoke, true, "permission revoked");
    }

    /// `transport.open` 门禁：未确认 / 超单 pane 上限都拒绝并记审计。
    ///
    /// 同一个 pane 重复 open 视为幂等（插件重连自己的 pane），不占第二个名额。
    pub fn authorize_open(&mut self, pane_id: u64) -> Result<(), TransportGateError> {
        if !self.confirmed {
            self.record(
                pane_id,
                TransportAuditAction::Reject,
                false,
                "permission not confirmed",
            );
            return Err(TransportGateError::NotConfirmed);
        }
        if self.pane.is_some_and(|open| open != pane_id) {
            self.record(pane_id, TransportAuditAction::Reject, false, "pane limit");
            return Err(TransportGateError::PaneLimit { max: Self::MAX_OPEN_PANES });
        }
        self.pane = Some(pane_id);
        self.record(pane_id, TransportAuditAction::Open, true, "transport open");
        Ok(())
    }

    /// 关闭当前 pane 的授权（幂等；非当前 pane 不会误清空别人）。
    pub fn close(&mut self, pane_id: u64) {
        if self.pane == Some(pane_id) {
            self.pane = None;
            self.record(pane_id, TransportAuditAction::Close, true, "transport close");
        }
    }

    /// 数据面缓冲门禁：读/写各不得超过 [`Self::MAX_BUFFER_BYTES`]。
    ///
    /// 超限不是“截断后继续”，调用方必须断开 transport 并报 `transport.error`。
    pub fn check_buffers(
        &mut self,
        read_bytes: usize,
        write_bytes: usize,
    ) -> Result<(), TransportGateError> {
        if read_bytes > Self::MAX_BUFFER_BYTES || write_bytes > Self::MAX_BUFFER_BYTES {
            self.record(0, TransportAuditAction::Reject, false, "buffer limit");
            return Err(TransportGateError::BufferLimit {
                read: read_bytes,
                write: write_bytes,
                max: Self::MAX_BUFFER_BYTES,
            });
        }
        Ok(())
    }

    /// 已有审计记录（按发生顺序）。
    pub fn audit(&self) -> &[TransportAuditEntry] {
        &self.audit
    }

    fn record(
        &mut self,
        pane_id: u64,
        action: TransportAuditAction,
        allowed: bool,
        detail: &'static str,
    ) {
        self.next_seq = self.next_seq.wrapping_add(1);
        self.audit.push(TransportAuditEntry {
            seq: self.next_seq,
            pane_id,
            action,
            allowed,
            detail,
        });
    }
}
