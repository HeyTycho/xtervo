// SPDX-License-Identifier: GPL-3.0-only
//! OSC / BEL 通知扫描与纯解析。
//!
//! `alacritty_terminal` 0.26 的 `vte::ansi::Handler` 没有 `osc_dispatch` 钩子
//! （OSC 9 / 777 在 vte 内部直接丢弃），所以 actor 在把字节喂给 `Term` 的同时，
//! 让本模块用一个小状态机扫一遍原始字节。它只关心通知/进度/cwd/shell integration，
//! 不参与网格解析。
//!
//! 所有解析函数只接收字节切片：非法/越界/超长输入一律忽略，绝不 panic。

use crate::event::{
    ClipboardRequest, ClipboardSelection, CoreEvent, Notification, Progress, ProgressState,
    ShellMark,
};

/// 单个 OSC 序列的缓冲上限（通知正文可能较长，但绝不允许无限增长）。
const MAX_OSC_BYTES: usize = 16 * 1024;

/// OSC 52 解码后的文本上限（1 MiB）：防止远端用一条序列撑爆内存/剪贴板。
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// 1 MiB 原文对应的 base64 编码长度（4 × ceil(n/3)）。
const MAX_OSC52_ENCODED_BYTES: usize = 4 * MAX_CLIPBOARD_BYTES.div_ceil(3);

/// OSC 52 整条序列的缓冲上限：留出 `52;c;` 前缀的余量。
/// 非 52 的 OSC 仍按 [`MAX_OSC_BYTES`] 截断，避免标题等序列白占 1.4 MB。
const MAX_OSC52_OSC_BYTES: usize = MAX_OSC52_ENCODED_BYTES + 8;

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
        // OSC 52 的内容（base64）允许到 ~1.4 MB；其余 OSC 仍限 16 KiB。
        let limit =
            if self.buf.starts_with(b"52;") { MAX_OSC52_OSC_BYTES } else { MAX_OSC_BYTES };
        if self.buf.len() >= limit {
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
/// 只处理通知相关编号：7（cwd）、9（iTerm2 通知/进度）、52（剪贴板）、
/// 133（shell integration）、777（notify）。
/// 其他 OSC（0/2 标题、4 颜色、8 超链接…）交给 alacritty，返回 None。
pub fn parse_osc(params: &[&[u8]]) -> Option<CoreEvent> {
    match *params.first()? {
        b"9" => parse_osc9(params),
        b"52" => parse_osc52(params),
        b"777" => parse_osc777(params),
        b"7" => params.get(1).and_then(|uri| parse_cwd(uri)).map(CoreEvent::Cwd),
        b"133" => parse_shell_mark(params).map(CoreEvent::Shell),
        _ => None,
    }
}

/// `OSC 133;A|B|C|D[;exit_code]`。
///
/// 未知子命令（`P` 等扩展）忽略；`D` 的退出码只接受有界整数，
/// 缺省/非法都按 `None` 上报（命令完成但退出码未知）。
fn parse_shell_mark(params: &[&[u8]]) -> Option<ShellMark> {
    match *params.get(1)? {
        b"A" => Some(ShellMark::PromptStart),
        b"B" => Some(ShellMark::InputStart),
        b"C" => Some(ShellMark::OutputStart),
        b"D" => Some(ShellMark::CommandFinished {
            exit_code: params.get(2).and_then(|raw| parse_exit_code(raw)),
        }),
        _ => None,
    }
}

/// OSC 133;D 的退出码：只接受 -255..=255 的十进制整数，非法值回退 `None`。
fn parse_exit_code(raw: &[u8]) -> Option<i32> {
    let text = std::str::from_utf8(raw).ok()?;
    let code: i32 = text.trim().parse().ok()?;
    (-255..=255).contains(&code).then_some(code)
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

/// `OSC 52;Pc;Pd`：剪贴板写入、查询与清空。
///
/// - `Pd = ?`：查询，由 app 读系统剪贴板后经 [`clipboard_reply`] 回写 PTY；
/// - `Pd` 为空：清空；
/// - 其他：base64 文本。非法 base64、非 UTF-8、超过 [`MAX_CLIPBOARD_BYTES`]
///   的输入整条忽略（绝不 panic）。
///
/// `Pc` 省略时按 xterm 默认取 `s`（PRIMARY）；`p`/`s` 都映射到 PRIMARY。
/// base64 字符集不含 `;`，所以参数多于 3 段一定是坏输入，直接丢弃。
fn parse_osc52(params: &[&[u8]]) -> Option<CoreEvent> {
    if params.len() > 3 {
        return None;
    }
    let selection = parse_clipboard_selection(*params.get(1)?)?;
    let data = *params.get(2)?;
    let request = if data == b"?" {
        ClipboardRequest::Load { selection }
    } else if data.is_empty() {
        ClipboardRequest::Clear { selection }
    } else {
        let bytes = decode_base64(data)?;
        let text = String::from_utf8(bytes).ok()?;
        ClipboardRequest::Store { selection, text }
    };
    Some(CoreEvent::Clipboard(request))
}

fn parse_clipboard_selection(raw: &[u8]) -> Option<ClipboardSelection> {
    match raw {
        // xterm 规范：Pc 缺省是 s；空参数与 s 等价。
        b"" | b"s" | b"p" => Some(ClipboardSelection::Primary),
        b"c" => Some(ClipboardSelection::Clipboard),
        _ => None,
    }
}

/// 把剪贴板内容编码成 OSC 52 回写序列（查询响应）。
///
/// PRIMARY 统一回 `p`：X11 上 `p` 与 `s` 是同一个选区，回 `p` 更接近事实。
pub fn clipboard_reply(selection: ClipboardSelection, text: &str) -> Vec<u8> {
    let pc = match selection {
        ClipboardSelection::Clipboard => b'c',
        ClipboardSelection::Primary => b'p',
    };
    // 查询回写与写入共用 1 MiB 上限：本地剪贴板可能被别的程序塞进超大文本，
    // 不能让一条 OSC 52 回包无限膨胀（也会阻塞有界输入队列）。
    let encoded = encode_base64(truncate_utf8(text, MAX_CLIPBOARD_BYTES).as_bytes());
    let mut out = Vec::with_capacity(encoded.len() + 8);
    out.extend_from_slice(b"\x1b]52;");
    out.push(pc);
    out.push(b';');
    out.extend_from_slice(encoded.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

fn base64_value(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 标准 base64 解码；支持带或不带 `=` 补位，但拒绝非法字符、错误长度与
/// 中间补位。解码结果超过 [`MAX_CLIPBOARD_BYTES`] 时返回 None。
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    decode_base64_bounded(input, MAX_CLIPBOARD_BYTES, false)
}

/// 带输出上限的严格 base64 解码；OSC 52 与 iTerm2 IIP 共用。
///
/// `canonical = true` 时额外拒绝补位字节里非零的“尾部未用位”（IIP 要求严格
/// 解码）；OSC 52 保持宽松，兼容历史上外部工具发出的非规范补位。
pub(crate) fn decode_base64_bounded(
    input: &[u8],
    max_bytes: usize,
    canonical: bool,
) -> Option<Vec<u8>> {
    if input.is_empty() {
        return Some(Vec::new());
    }
    let max_encoded = 4 * max_bytes.div_ceil(3);
    if input.len() > max_encoded {
        return None;
    }
    let mut end = input.len();
    let mut pad = 0;
    while end > 0 && input[end - 1] == b'=' {
        pad += 1;
        end -= 1;
        if pad > 2 {
            return None;
        }
    }
    // "====" / "==" 全是补位，没有数据。
    if end == 0 {
        return None;
    }
    // 有补位时总长必须是 4 的倍数（如 "YQ=="）；无补位允许 2/3 的尾巴。
    if pad > 0 && input.len() % 4 != 0 {
        return None;
    }
    if end % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity((end / 4) * 3 + 2);
    let mut i = 0;
    while i + 4 <= end {
        let a = base64_value(input[i])?;
        let b = base64_value(input[i + 1])?;
        let c = base64_value(input[i + 2])?;
        let d = base64_value(input[i + 3])?;
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        out.push((c << 6) | d);
        i += 4;
    }
    match end - i {
        0 => {},
        2 => {
            let a = base64_value(input[i])?;
            let b = base64_value(input[i + 1])?;
            if canonical && b & 0x0f != 0 {
                return None;
            }
            out.push((a << 2) | (b >> 4));
        },
        3 => {
            let a = base64_value(input[i])?;
            let b = base64_value(input[i + 1])?;
            let c = base64_value(input[i + 2])?;
            if canonical && c & 0x03 != 0 {
                return None;
            }
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
        },
        _ => return None,
    }
    if out.len() > max_bytes {
        return None;
    }
    Some(out)
}

/// 按字节上限截断，且不切断 UTF-8 字符边界。
fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 标准 base64 编码（带 `=` 补位），仅用于 OSC 52 查询响应与测试。
pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let bits = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((bits >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((bits >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((bits >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(bits & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
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
    fn osc133_shell_integration_marks() {
        assert_eq!(scan(b"\x1b]133;A\x07"), vec![CoreEvent::Shell(ShellMark::PromptStart)]);
        assert_eq!(scan(b"\x1b]133;B\x07"), vec![CoreEvent::Shell(ShellMark::InputStart)]);
        assert_eq!(scan(b"\x1b]133;C\x07"), vec![CoreEvent::Shell(ShellMark::OutputStart)]);
        assert_eq!(
            scan(b"\x1b]133;D;0\x07"),
            vec![CoreEvent::Shell(ShellMark::CommandFinished { exit_code: Some(0) })]
        );
        assert_eq!(
            scan(b"\x1b]133;D;130\x1b\\"),
            vec![CoreEvent::Shell(ShellMark::CommandFinished { exit_code: Some(130) })]
        );
        // 没有退出码 / 非法退出码 / 未知子命令：要么 None 要么忽略。
        assert_eq!(
            scan(b"\x1b]133;D\x07"),
            vec![CoreEvent::Shell(ShellMark::CommandFinished { exit_code: None })]
        );
        assert_eq!(
            scan(b"\x1b]133;D;abc\x07"),
            vec![CoreEvent::Shell(ShellMark::CommandFinished { exit_code: None })]
        );
        assert_eq!(
            scan(b"\x1b]133;D;9999\x07"),
            vec![CoreEvent::Shell(ShellMark::CommandFinished { exit_code: None })]
        );
        assert!(scan(b"\x1b]133;P;k=v\x07").is_empty());
        // 跨 chunk 的 133 标记也要拼起来。
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        scanner.feed(b"\x1b]133;D;", &mut out);
        assert!(out.is_empty());
        scanner.feed(b"7\x07", &mut out);
        assert_eq!(out, vec![CoreEvent::Shell(ShellMark::CommandFinished { exit_code: Some(7) })]);
    }

    #[test]
    fn unrelated_osc_and_invalid_utf8_are_ignored() {
        assert!(scan(b"\x1b]0;window title\x07").is_empty());
        // OSC 133 已接入 shell integration 事件流（本轮行为变更，旧断言为 is_empty）。
        assert_eq!(scan(b"\x1b]133;A\x07"), vec![CoreEvent::Shell(ShellMark::PromptStart)]);
        // 非 UTF-8 正文用替换字符，不 panic
        assert_eq!(
            scan(b"\x1b]9;ok\xff\x07"),
            vec![CoreEvent::Notification(Notification {
                title: String::new(),
                body: "ok\u{fffd}".to_string(),
            })]
        );
    }

    fn clipboard(request: ClipboardRequest) -> CoreEvent {
        CoreEvent::Clipboard(request)
    }

    #[test]
    fn osc52_store_clipboard_and_primary() {
        assert_eq!(
            scan(b"\x1b]52;c;aGVsbG8=\x07"),
            vec![clipboard(ClipboardRequest::Store {
                selection: ClipboardSelection::Clipboard,
                text: "hello".to_string(),
            })]
        );
        // p / s 都是 PRIMARY；ST 结尾同样支持。
        for pc in ["p", "s"] {
            let mut seq = format!("\x1b]52;{pc};").into_bytes();
            seq.extend_from_slice(b"5LiA5Liq5rWL6K+V");
            seq.extend_from_slice(b"\x1b\\");
            assert_eq!(
                scan(&seq),
                vec![clipboard(ClipboardRequest::Store {
                    selection: ClipboardSelection::Primary,
                    text: "一个测试".to_string(),
                })],
                "Pc={pc}"
            );
        }
        // 无补位 base64 也接受。
        assert_eq!(
            scan(b"\x1b]52;c;aGVsbG8\x07"),
            vec![clipboard(ClipboardRequest::Store {
                selection: ClipboardSelection::Clipboard,
                text: "hello".to_string(),
            })]
        );
    }

    #[test]
    fn osc52_default_selection_is_primary() {
        // xterm：Pc 缺省为 s（PRIMARY）。
        assert_eq!(
            scan(b"\x1b]52;;aGk=\x07"),
            vec![clipboard(ClipboardRequest::Store {
                selection: ClipboardSelection::Primary,
                text: "hi".to_string(),
            })]
        );
    }

    #[test]
    fn osc52_query_and_clear() {
        assert_eq!(
            scan(b"\x1b]52;c;?\x07"),
            vec![clipboard(ClipboardRequest::Load { selection: ClipboardSelection::Clipboard })]
        );
        assert_eq!(
            scan(b"\x1b]52;p;?\x1b\\"),
            vec![clipboard(ClipboardRequest::Load { selection: ClipboardSelection::Primary })]
        );
        assert_eq!(
            scan(b"\x1b]52;c;\x07"),
            vec![clipboard(ClipboardRequest::Clear { selection: ClipboardSelection::Clipboard })]
        );
        assert_eq!(
            scan(b"\x1b]52;s;\x07"),
            vec![clipboard(ClipboardRequest::Clear { selection: ClipboardSelection::Primary })]
        );
    }

    #[test]
    fn osc52_bad_selection_or_shape_is_ignored() {
        assert!(scan(b"\x1b]52;x;aGk=\x07").is_empty(), "未知 Pc");
        assert!(scan(b"\x1b]52;cc;aGk=\x07").is_empty(), "多字符 Pc");
        assert!(scan(b"\x1b]52;c;aGk=;extra\x07").is_empty(), "多于 3 段");
        assert!(scan(b"\x1b]52\x07").is_empty(), "缺 Pc/Pd");
        assert!(scan(b"\x1b]52;c\x07").is_empty(), "缺 Pd（应写成空段）");
        // 单独的 0 是合法 base64（一个 NUL 字节，也是合法 UTF-8）。
        assert_eq!(
            scan(b"\x1b]52;c;AA==\x07"),
            vec![clipboard(ClipboardRequest::Store {
                selection: ClipboardSelection::Clipboard,
                text: "\0".to_string(),
            })]
        );
    }

    #[test]
    fn osc52_invalid_base64_and_utf8_are_ignored() {
        for bad in [
            &b"!!!!"[..],
            b"A",
            b"AB=C",
            b"====",
            b"aGVsbG8===",
            b"aGVs bG8=",
            b"aGVsbG8=\xff",
        ] {
            let mut seq = b"\x1b]52;c;".to_vec();
            seq.extend_from_slice(bad);
            seq.push(0x07);
            assert!(scan(&seq).is_empty(), "bad base64 {bad:?} 必须忽略");
        }
    }

    #[test]
    fn base64_decode_and_encode_roundtrip() {
        assert_eq!(decode_base64(b""), Some(Vec::new()));
        assert_eq!(decode_base64(b"aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64(b"aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode_base64(b"YQ==").unwrap(), b"a");
        assert_eq!(decode_base64(b"YWI=").unwrap(), b"ab");
        assert_eq!(decode_base64(b"YWJj").unwrap(), b"abc");
        for len in 0..=16 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let encoded = encode_base64(&data);
            assert_eq!(decode_base64(encoded.as_bytes()), Some(data), "len={len}");
        }
        assert!(decode_base64(b"a").is_none());
        assert!(decode_base64(b"A===").is_none());
        assert!(decode_base64(b"AB=C").is_none());
        assert!(decode_base64(b"====").is_none());
        assert!(decode_base64(b"YQ==YQ==").is_none());
    }

    #[test]
    fn clipboard_reply_is_valid_osc52() {
        let reply = clipboard_reply(ClipboardSelection::Clipboard, "hello");
        assert_eq!(reply, b"\x1b]52;c;aGVsbG8=\x1b\\");
        let reply = clipboard_reply(ClipboardSelection::Primary, "hi");
        assert_eq!(reply, b"\x1b]52;p;aGk=\x1b\\");
        // 回写序列自己也能被扫描器解析回同样的文本。
        assert_eq!(
            scan(&reply),
            vec![clipboard(ClipboardRequest::Store {
                selection: ClipboardSelection::Primary,
                text: "hi".to_string(),
            })]
        );

        // 超大剪贴板回包截到 1 MiB，且不切坏多字节字符。
        let big = "界".repeat(MAX_CLIPBOARD_BYTES / 3 + 10);
        let reply = clipboard_reply(ClipboardSelection::Clipboard, &big);
        let payload = &reply[7..reply.len() - 2];
        let decoded = decode_base64(payload).unwrap();
        assert!(decoded.len() <= MAX_CLIPBOARD_BYTES);
        assert!(std::str::from_utf8(&decoded).is_ok());
    }

    #[test]
    fn osc52_size_limit() {
        // 恰好 1 MiB 可以接受。
        let max = vec![b'a'; MAX_CLIPBOARD_BYTES];
        let mut seq = b"\x1b]52;c;".to_vec();
        seq.extend_from_slice(encode_base64(&max).as_bytes());
        seq.push(0x07);
        assert_eq!(
            scan(&seq),
            vec![clipboard(ClipboardRequest::Store {
                selection: ClipboardSelection::Clipboard,
                text: "a".repeat(MAX_CLIPBOARD_BYTES),
            })],
            "上限内应接受"
        );

        // 超过 1 MiB 解码后长度，整条忽略。
        let over = vec![b'a'; MAX_CLIPBOARD_BYTES + 1];
        let mut seq = b"\x1b]52;c;".to_vec();
        seq.extend_from_slice(encode_base64(&over).as_bytes());
        seq.push(0x07);
        assert!(scan(&seq).is_empty(), "超过 1 MiB 必须忽略");

        // 超长（超过扫描器缓冲）同样忽略，且后续 BEL 仍正常。
        let mut seq = b"\x1b]52;c;".to_vec();
        seq.extend(std::iter::repeat(b'A').take(MAX_OSC52_OSC_BYTES + 10));
        seq.push(0x07);
        assert!(scan(&seq).is_empty());
        seq.push(0x07);
        assert_eq!(scan(&seq), vec![CoreEvent::Bell]);
    }

    #[test]
    fn osc52_split_across_chunks() {
        // 跨 chunk 的 payload 在扫描器里拼接后仍能解析。
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        let encoded = encode_base64(b"split-me");
        let mut seq = b"\x1b]52;c;".to_vec();
        seq.extend_from_slice(encoded.as_bytes());
        seq.push(0x07);
        let (a, b) = seq.split_at(12);
        scanner.feed(a, &mut out);
        assert!(out.is_empty());
        scanner.feed(b, &mut out);
        assert_eq!(
            out,
            vec![clipboard(ClipboardRequest::Store {
                selection: ClipboardSelection::Clipboard,
                text: "split-me".to_string(),
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
    fn every_split_point_of_known_osc_is_equivalent() {
        // 分包边界可以落在任意字节：引导符 / ESC / ] / 正文 / ST 两字节之间。
        let cases: [&[u8]; 6] = [
            b"\x1b]9;4;1;42\x07",
            b"\x1b]9;hi\x1b\\",
            b"\x1b]52;c;aGk=\x07",
            b"\x1b]133;D;3\x07",
            b"\x1b]777;notify;t;b\x07",
            b"\x1b]7;file:///tmp/x%20y\x07",
        ];
        for seq in cases {
            let want = scan(seq);
            assert!(!want.is_empty(), "基线序列应产生事件: {seq:?}");
            for split in 0..=seq.len() {
                let mut scanner = OscScanner::new();
                let mut out = Vec::new();
                scanner.feed(&seq[..split], &mut out);
                scanner.feed(&seq[split..], &mut out);
                assert_eq!(out, want, "split={split} seq={seq:?}");
                // 分三片也必须一致（正文中间再切一刀）。
                let mut scanner = OscScanner::new();
                let mut out = Vec::new();
                let mid = split + (seq.len() - split) / 2;
                scanner.feed(&seq[..split], &mut out);
                scanner.feed(&seq[split..mid.max(split)], &mut out);
                scanner.feed(&seq[mid.max(split)..], &mut out);
                assert_eq!(out, want, "three-way split={split} seq={seq:?}");
            }
        }
    }

    #[test]
    fn oversized_osc_recovers_and_emits_only_later_events() {
        // 16 KiB 上限：超长正文整条丢弃，不产生半截事件；结束后下一条正常上报。
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        scanner.feed(b"\x1b]9;", &mut out);
        let chunk = vec![b'a'; 4096];
        for _ in 0..(MAX_OSC_BYTES / 4096 + 2) {
            scanner.feed(&chunk, &mut out);
        }
        assert!(out.is_empty(), "超长 OSC 不应提前上报");
        scanner.feed(b"\x07\x1b]9;after\x07", &mut out);
        assert_eq!(
            out,
            vec![CoreEvent::Notification(Notification {
                title: String::new(),
                body: "after".to_string(),
            })],
            "超长序列之后的 OSC 必须恢复正常"
        );
    }

    #[test]
    fn osc_aborted_by_escape_never_reports_partial_body() {
        // OSC 内 ESC + 非 `\\`：整条丢弃（包括已收集的正文）。
        assert!(scan(b"\x1b]9;partial\x1bq").is_empty());
        // 连续 ESC（等待是否构成 ST）不进入正文；最后 `\\` 仍按 ST 正常结束，
        // 正文只保留 ESC 之前的字节。
        assert_eq!(
            scan(b"\x1b]9;x\x1b\x1b\\"),
            vec![CoreEvent::Notification(Notification {
                title: String::new(),
                body: "x".to_string(),
            })],
            "ESC ESC 不应污染正文"
        );
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
