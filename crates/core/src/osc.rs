// SPDX-License-Identifier: GPL-3.0-only
//! OSC / BEL 通知扫描与纯解析。
//!
//! `alacritty_terminal` 0.26 的 `vte::ansi::Handler` 没有 `osc_dispatch` 钩子
//! （OSC 9 / 777 在 vte 内部直接丢弃），所以 actor 在把字节喂给 `Term` 的同时，
//! 让本模块用一个小状态机扫一遍原始字节。它只关心通知/进度/cwd，不参与网格解析。
//!
//! 所有解析函数只接收字节切片：非法/越界/超长输入一律忽略，绝不 panic。

use crate::event::{CoreEvent, Notification, Progress, ProgressState};

/// 单个 OSC 序列的缓冲上限（通知正文可能较长，但绝不允许无限增长）。
const MAX_OSC_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    /// 已看到 `ESC ]` 或 C1 0x9D，正在收集 OSC 内容。
    Osc,
    /// OSC 内看到 ESC，等待 `\` 组成 ST（ESC `\`）。
    OscEscape,
    /// CSI：等到 final byte 才回 Ground，参数里的 BEL 不算铃声。
    Csi,
    /// DCS / SOS / PM / APC：以 ST 结束，内容里的 BEL 不算铃声。
    String,
    StringEscape,
}

/// 流式 OSC/BEL 扫描器；跨 PTY chunk 的序列由内部状态拼接。
pub struct OscScanner {
    state: State,
    buf: Vec<u8>,
    overflow: bool,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self { state: State::Ground, buf: Vec::new(), overflow: false }
    }

    /// 扫描一段 PTY 字节，把识别出的通知类事件追加到 `out`。
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<CoreEvent>) {
        for &b in bytes {
            self.state = match self.state {
                State::Ground => match b {
                    0x07 => {
                        out.push(CoreEvent::Bell);
                        State::Ground
                    },
                    0x1b => State::Escape,
                    0x9d => {
                        self.begin_osc();
                        State::Osc
                    },
                    0x90 | 0x98 | 0x9e | 0x9f => State::String,
                    0x9b => State::Csi,
                    _ => State::Ground,
                },
                State::Escape => match b {
                    b']' => {
                        self.begin_osc();
                        State::Osc
                    },
                    b'[' => State::Csi,
                    b'P' | b'X' | b'^' | b'_' => State::String,
                    0x1b => State::Escape,
                    _ => State::Ground,
                },
                State::Osc => match b {
                    0x07 => {
                        self.finish_osc(out);
                        State::Ground
                    },
                    0x1b => State::OscEscape,
                    0x9c => {
                        self.finish_osc(out);
                        State::Ground
                    },
                    _ => {
                        self.push(b);
                        State::Osc
                    },
                },
                State::OscEscape => match b {
                    b'\\' => {
                        self.finish_osc(out);
                        State::Ground
                    },
                    0x1b => State::OscEscape,
                    // ESC 后面不是 `\`：整条 OSC 视为损坏并丢弃。
                    _ => {
                        self.buf.clear();
                        self.overflow = false;
                        State::Ground
                    },
                },
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        State::Ground
                    } else {
                        State::Csi
                    }
                },
                State::String => match b {
                    0x1b => State::StringEscape,
                    0x9c => State::Ground,
                    _ => State::String,
                },
                State::StringEscape => match b {
                    b'\\' => State::Ground,
                    0x1b => State::StringEscape,
                    _ => State::String,
                },
            };
        }
    }

    fn begin_osc(&mut self) {
        self.buf.clear();
        self.overflow = false;
    }

    fn push(&mut self, b: u8) {
        if self.overflow {
            return;
        }
        if self.buf.len() >= MAX_OSC_BYTES {
            // 超长序列直接丢弃，但仍继续吃到 terminator，避免污染下一条。
            self.overflow = true;
            self.buf.clear();
            return;
        }
        self.buf.push(b);
    }

    fn finish_osc(&mut self, out: &mut Vec<CoreEvent>) {
        if !self.overflow {
            let params: Vec<&[u8]> = self.buf.split(|&b| b == b';').collect();
            if let Some(event) = parse_osc(&params) {
                out.push(event);
            }
        }
        self.buf.clear();
        self.overflow = false;
    }
}

/// 解析一条 OSC 的全部参数（已按 `;` 切分，第 0 项是 OSC 编号）。
///
/// 只处理通知相关编号：7（cwd）、9（iTerm2 通知/进度）、777（notify）。
/// 其他 OSC（0/2 标题、4 颜色、8 超链接、52 剪贴板…）交给 alacritty，返回 None。
pub fn parse_osc(params: &[&[u8]]) -> Option<CoreEvent> {
    match *params.first()? {
        b"9" => parse_osc9(params),
        b"777" => parse_osc777(params),
        b"7" => params.get(1).and_then(|uri| parse_cwd(uri)).map(CoreEvent::Cwd),
        _ => None,
    }
}

/// `OSC 9;4;state;percent` 或 `OSC 9;message`。
fn parse_osc9(params: &[&[u8]]) -> Option<CoreEvent> {
    if params.get(1).is_some_and(|p| *p == b"4".as_slice()) {
        return parse_progress(params).map(CoreEvent::Progress);
    }
    let body = join_lossy(params.get(1..)?);
    if body.trim().is_empty() {
        return None;
    }
    Some(CoreEvent::Notification(Notification { title: String::new(), body }))
}

/// 解析进度参数，要求完整形态 `[9, 4, state, (percent)]`。
fn parse_progress(params: &[&[u8]]) -> Option<Progress> {
    if params.len() < 3 || params[0] != b"9".as_slice() || params[1] != b"4".as_slice() {
        return None;
    }
    let state = match parse_u8(params[2])? {
        0 => ProgressState::Clear,
        1 => ProgressState::Normal,
        2 => ProgressState::Error,
        3 => ProgressState::Indeterminate,
        4 => ProgressState::Paused,
        _ => return None,
    };
    // state 0 / 3 没有有意义的值；非法百分比只丢百分比，不丢状态。
    let percent = match state {
        ProgressState::Clear | ProgressState::Indeterminate => None,
        _ => params.get(3).and_then(|p| parse_u8(p)).filter(|v| *v <= 100),
    };
    Some(Progress { state, percent })
}

/// `OSC 777;notify;title;body`；标题/正文可含 `;`，UTF-8 非法时用替换字符。
fn parse_osc777(params: &[&[u8]]) -> Option<CoreEvent> {
    if !params.get(1).is_some_and(|p| *p == b"notify".as_slice()) {
        return None;
    }
    let title = params.get(2).map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or_default();
    let body = params.get(3..).map(join_lossy).unwrap_or_default();
    if title.trim().is_empty() && body.trim().is_empty() {
        return None;
    }
    Some(CoreEvent::Notification(Notification { title, body }))
}

/// `file://host/path` → `path`；host 忽略，简单解码 `%XX`。
fn parse_cwd(uri: &[u8]) -> Option<String> {
    let uri = std::str::from_utf8(uri).ok()?;
    let rest = uri.strip_prefix("file://")?;
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) => &rest[i..],
        None => return None,
    };
    if path.is_empty() {
        return None;
    }
    Some(percent_decode(path))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_u8(bytes: &[u8]) -> Option<u8> {
    std::str::from_utf8(bytes).ok()?.parse::<u8>().ok()
}

fn join_lossy(parts: &[&[u8]]) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(';');
        }
        out.push_str(&String::from_utf8_lossy(part));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8]) -> Vec<CoreEvent> {
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        scanner.feed(bytes, &mut out);
        out
    }

    fn progress(state: ProgressState, percent: Option<u8>) -> CoreEvent {
        CoreEvent::Progress(Progress { state, percent })
    }

    #[test]
    fn osc94_all_states() {
        assert_eq!(
            scan(b"\x1b]9;4;0;99\x07"),
            vec![progress(ProgressState::Clear, None)],
            "state 0 清除，百分比无意义"
        );
        assert_eq!(
            scan(b"\x1b]9;4;1;42\x07"),
            vec![progress(ProgressState::Normal, Some(42))]
        );
        assert_eq!(
            scan(b"\x1b]9;4;2;100\x07"),
            vec![progress(ProgressState::Error, Some(100))]
        );
        assert_eq!(
            scan(b"\x1b]9;4;3;77\x07"),
            vec![progress(ProgressState::Indeterminate, None)],
            "不确定进度忽略百分比"
        );
        assert_eq!(
            scan(b"\x1b]9;4;4;7\x07"),
            vec![progress(ProgressState::Paused, Some(7))]
        );
    }

    #[test]
    fn osc94_missing_and_bad_percent() {
        assert_eq!(scan(b"\x1b]9;4;1\x07"), vec![progress(ProgressState::Normal, None)]);
        assert_eq!(scan(b"\x1b]9;4;1;\x07"), vec![progress(ProgressState::Normal, None)]);
        assert_eq!(scan(b"\x1b]9;4;1;101\x07"), vec![progress(ProgressState::Normal, None)]);
        assert_eq!(scan(b"\x1b]9;4;1;-1\x07"), vec![progress(ProgressState::Normal, None)]);
        assert_eq!(scan(b"\x1b]9;4;1;abc\x07"), vec![progress(ProgressState::Normal, None)]);
        assert_eq!(scan(b"\x1b]9;4;1;1;2;3\x07"), vec![progress(ProgressState::Normal, Some(1))]);
    }

    #[test]
    fn osc94_bad_state_or_shape_is_ignored() {
        assert!(scan(b"\x1b]9;4;5\x07").is_empty());
        assert!(scan(b"\x1b]9;4;255\x07").is_empty());
        assert!(scan(b"\x1b]9;4\x07").is_empty());
        assert!(scan(b"\x1b]9;4;\x07").is_empty());
        // 多余参数忽略，进度本身仍有效
        assert_eq!(
            scan(b"\x1b]9;4;1;50;extra\x07"),
            vec![progress(ProgressState::Normal, Some(50))]
        );
    }

    #[test]
    fn osc9_message() {
        assert_eq!(
            scan(b"\x1b]9;hello world\x07"),
            vec![CoreEvent::Notification(Notification {
                title: String::new(),
                body: "hello world".to_string(),
            })]
        );
        // ST 结尾 + 含分号
        assert_eq!(
            scan(b"\x1b]9;build;done\x1b\\"),
            vec![CoreEvent::Notification(Notification {
                title: String::new(),
                body: "build;done".to_string(),
            })]
        );
        assert!(scan(b"\x1b]9;\x07").is_empty());
    }

    #[test]
    fn osc777_notify() {
        assert_eq!(
            scan(b"\x1b]777;notify;Build;finished OK\x07"),
            vec![CoreEvent::Notification(Notification {
                title: "Build".to_string(),
                body: "finished OK".to_string(),
            })]
        );
        // 正文里可以有分号
        assert_eq!(
            scan(b"\x1b]777;notify;t;a;b;c\x1b\\"),
            vec![CoreEvent::Notification(Notification {
                title: "t".to_string(),
                body: "a;b;c".to_string(),
            })]
        );
        // 非 notify 的 777 不处理
        assert!(scan(b"\x1b]777;other;t;b\x07").is_empty());
        assert!(scan(b"\x1b]777;notify;;\x07").is_empty());
    }

    #[test]
    fn bel_and_osc_terminator_bel_are_distinct() {
        assert_eq!(scan(b"\x07"), vec![CoreEvent::Bell]);
        // OSC 内部的 BEL 是终止符，不应额外产生铃声
        assert_eq!(
            scan(b"\x1b]9;hi\x07\x07"),
            vec![
                CoreEvent::Notification(Notification {
                    title: String::new(),
                    body: "hi".to_string(),
                }),
                CoreEvent::Bell,
            ]
        );
    }

    #[test]
    fn csi_and_dcs_do_not_ring_bell() {
        // CSI 参数里出现 BEL（非法输入）不应误判；final byte 后恢复
        assert!(scan(b"\x1b[1;\x07m").is_empty());
        // DCS payload 里的 BEL 不是铃声
        assert!(scan(b"\x1bPq#0;2;0;0;0\x07\x1b\\").is_empty());
    }

    #[test]
    fn osc_split_across_chunks() {
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        scanner.feed(b"\x1b]9;4;1;5", &mut out);
        assert!(out.is_empty(), "未终止的 OSC 不应提前上报");
        scanner.feed(b"0\x07", &mut out);
        assert_eq!(out, vec![progress(ProgressState::Normal, Some(50))]);

        // ESC 和 ] 也被拆开
        let mut out = Vec::new();
        scanner.feed(b"\x1b", &mut out);
        scanner.feed(b"]777;notify;t;b\x07", &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn osc7_cwd() {
        assert_eq!(
            scan(b"\x1b]7;file://host/home/user/project\x07"),
            vec![CoreEvent::Cwd("/home/user/project".to_string())]
        );
        assert_eq!(
            scan(b"\x1b]7;file:///tmp/a%20b\x07"),
            vec![CoreEvent::Cwd("/tmp/a b".to_string())]
        );
        assert!(scan(b"\x1b]7;file://host\x07").is_empty());
        assert!(scan(b"\x1b]7;http://x/y\x07").is_empty());
    }

    #[test]
    fn unrelated_osc_and_invalid_utf8_are_ignored() {
        assert!(scan(b"\x1b]0;window title\x07").is_empty());
        assert!(scan(b"\x1b]52;c;aGVsbG8=\x07").is_empty());
        // 非 UTF-8 正文用替换字符，不 panic
        assert_eq!(
            scan(b"\x1b]9;ok\xff\x07"),
            vec![CoreEvent::Notification(Notification {
                title: String::new(),
                body: "ok\u{fffd}".to_string(),
            })]
        );
    }

    #[test]
    fn oversized_osc_is_dropped() {
        let mut data = Vec::new();
        data.extend_from_slice(b"\x1b]9;");
        data.extend(std::iter::repeat(b'a').take(MAX_OSC_BYTES + 10));
        data.extend_from_slice(b"\x07");
        assert!(scan(&data).is_empty());
        // 超长序列之后的新序列仍然正常
        data.extend_from_slice(b"\x07");
        assert_eq!(scan(&data), vec![CoreEvent::Bell]);
    }

    #[test]
    fn incomplete_sequences_never_panic() {
        assert!(scan(b"\x1b]9;4;1;50").is_empty());
        assert!(scan(b"\x1b]777;notify").is_empty());
        assert!(scan(b"\x1b").is_empty());
        assert!(scan(b"\x1b]").is_empty());
        assert!(scan(b"\x1b]9;hi\x1b").is_empty());
    }
}
