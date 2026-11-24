// SPDX-License-Identifier: GPL-3.0-only
//! 进程内串口后端（`serialport` crate）：不再依赖系统 socat。
//!
//! 与 `Backend::Plugin` 共用 pane 的读线程/写 backlog/actor；`read` 用 100ms
//! 超时轮询 stop 标志，`into_halves` 拆成读/写/控制三个 clone。

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits, TTYPort};
use xtervo_core::transport::{
    PaneTransport, TransportControl, TransportHalves, TransportShutdown,
};

use crate::profile::Profile;

const READ_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Text,
    Hexdump,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NewlineMode {
    None,
    Lf,
    Cr,
    Crlf,
}

/// 串口输入模式（SERIAL-004）。
///
/// `Text` / `Readline` / `Byte` 都是**原样发送**：应用不做本地行编辑
/// （readline 语义交给设备/远端），也不在发送前改写字节；只有 `Hex` 会把
/// 用户键入的十六进制字符解析成字节。换行转换只作用于非 Hex 模式——Hex
/// 模式下用户显式写了 `0d 0a`，再转换会重复/破坏字节。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
    Text,
    Readline,
    Byte,
    Hex,
}

fn parse_output(value: &str) -> OutputMode {
    if value.eq_ignore_ascii_case("hexdump") {
        OutputMode::Hexdump
    } else {
        OutputMode::Text
    }
}

fn parse_input(value: &str) -> InputMode {
    match value.to_ascii_lowercase().as_str() {
        "readline" => InputMode::Readline,
        "byte" => InputMode::Byte,
        "hex" => InputMode::Hex,
        _ => InputMode::Text,
    }
}

fn parse_newline(value: &str) -> NewlineMode {
    match value.to_ascii_lowercase().as_str() {
        "lf" => NewlineMode::Lf,
        "cr" => NewlineMode::Cr,
        "crlf" => NewlineMode::Crlf,
        _ => NewlineMode::None,
    }
}

/// 出站换行转换；CRLF 输入按一个换行处理，避免发出两个终止符。
fn convert_newlines(data: &[u8], mode: NewlineMode) -> Vec<u8> {
    let target: &[u8] = match mode {
        NewlineMode::None => return data.to_vec(),
        NewlineMode::Lf => b"\n",
        NewlineMode::Cr => b"\r",
        NewlineMode::Crlf => b"\r\n",
    };
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\r' => {
                if i + 1 < data.len() && data[i + 1] == b'\n' {
                    i += 1;
                }
                out.extend_from_slice(target);
            },
            b'\n' => out.extend_from_slice(target),
            byte => out.push(byte),
        }
        i += 1;
    }
    out
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 有状态的输入编码器：Hex 模式下奇数个半字节跨 `write` 调用暂存。
#[derive(Debug, Default)]
struct InputEncoder {
    pending_nibble: Option<u8>,
}

impl InputEncoder {
    /// 纯函数入口（除 `pending_nibble` 状态外无副作用），供单测直接覆盖。
    fn encode(&mut self, data: &[u8], mode: InputMode, newline: NewlineMode) -> Vec<u8> {
        match mode {
            InputMode::Text | InputMode::Readline | InputMode::Byte => {
                convert_newlines(data, newline)
            },
            InputMode::Hex => self.encode_hex(data),
        }
    }

    /// 十六进制对 → 字节：忽略空白（含跨调用暂存的半字节）与非法字符。
    ///
    /// 非法字符不发送也不报错：用户用键盘逐字输入 `4`、`1` 时中间可能夹
    /// 空格或分隔符；把非法字符原样发出去会破坏设备协议。
    fn encode_hex(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() / 2 + 1);
        for &byte in data {
            let Some(nibble) = hex_nibble(byte) else {
                continue;
            };
            match self.pending_nibble.take() {
                Some(high) => out.push((high << 4) | nibble),
                None => self.pending_nibble = Some(nibble),
            }
        }
        out
    }
}

/// 行首时间戳状态机（SERIAL-010）。
///
/// 每行（含空行）第一个字节前加前缀；`\r\n` 不会在两个终止符之间重复插入，
/// 跨 read 调用的半行也不会被拆出新前缀。
#[derive(Debug)]
struct LineTimestamper {
    at_line_start: bool,
    last_was_cr: bool,
}

impl LineTimestamper {
    fn new() -> Self {
        Self { at_line_start: true, last_was_cr: false }
    }

    fn prefix(&mut self, data: &[u8], prefix: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + prefix.len());
        for &byte in data {
            // `\r\n` 里的 `\n` 是同一个行终止符的一部分，不重复加前缀。
            let crlf_tail = byte == b'\n' && self.last_was_cr;
            if self.at_line_start && !crlf_tail {
                out.extend_from_slice(prefix.as_bytes());
            }
            out.push(byte);
            self.last_was_cr = byte == b'\r';
            self.at_line_start = byte == b'\n' || byte == b'\r';
        }
        out
    }
}

/// 把“当天毫秒数”格式化成 `[HH:MM:SS.mmm] `；按 24 小时回绕。
fn format_timestamp_prefix(millis_of_day: u64) -> String {
    let ms = millis_of_day % 86_400_000;
    let hour = ms / 3_600_000;
    let minute = (ms / 60_000) % 60;
    let second = (ms / 1000) % 60;
    let milli = ms % 1000;
    format!("[{hour:02}:{minute:02}:{second:02}.{milli:03}] ")
}

/// 当前本地时间到“当天毫秒数”；非 Unix 平台退回 UTC。
fn local_millis_of_day(now: SystemTime) -> u64 {
    let since_epoch = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    #[cfg(unix)]
    {
        let secs = since_epoch.as_secs() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // `localtime_r` 是线程安全的；失败（极少数 TZ 损坏）时退回 UTC。
        if !unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
            let hour = tm.tm_hour.max(0) as u64;
            let minute = tm.tm_min.max(0) as u64;
            let second = tm.tm_sec.max(0) as u64;
            return ((hour * 3600 + minute * 60 + second) * 1000)
                + u64::from(since_epoch.subsec_millis());
        }
    }
    since_epoch.as_secs() % 86_400 * 1000 + u64::from(since_epoch.subsec_millis())
}

fn timestamp_prefix_now() -> String {
    format_timestamp_prefix(local_millis_of_day(SystemTime::now()))
}

/// 出站（设备→UI）格式化：text/hexdump + 可选行时间戳。
///
/// transport 的直连 `read`（测试/未拆半时）与 pane 用的 [`SerialReader`] 共用
/// 同一份状态机，避免两条路径对同一 Profile 产生不同输出。
struct OutputFormatter {
    output: OutputMode,
    timestamp: bool,
    hex_offset: u64,
    timestamper: LineTimestamper,
}

impl OutputFormatter {
    fn new(output: OutputMode, timestamp: bool) -> Self {
        Self { output, timestamp, hex_offset: 0, timestamper: LineTimestamper::new() }
    }

    fn format(&mut self, raw: &[u8]) -> Vec<u8> {
        let mut bytes = match self.output {
            OutputMode::Text => raw.to_vec(),
            OutputMode::Hexdump => format_hexdump(raw, &mut self.hex_offset),
        };
        if self.timestamp {
            let prefix = timestamp_prefix_now();
            bytes = self.timestamper.prefix(&bytes, &prefix);
        }
        bytes
    }
}

/// 从格式化后的 pending 缓冲拷贝一段；返回 `Some(n)` = pending 仍有数据。
fn take_pending(pending: &[u8], pos: &mut usize, buf: &mut [u8]) -> Option<usize> {
    if *pos >= pending.len() {
        return None;
    }
    let n = (pending.len() - *pos).min(buf.len());
    buf[..n].copy_from_slice(&pending[*pos..*pos + n]);
    *pos += n;
    Some(n)
}

/// 每 16 字节一行：偏移、十六进制、ASCII。
fn format_hexdump(data: &[u8], offset: &mut u64) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(16) {
        out.extend_from_slice(format!("{:08x}  ", *offset).as_bytes());
        for i in 0..16 {
            if let Some(byte) = chunk.get(i) {
                out.extend_from_slice(format!("{byte:02x} ").as_bytes());
            } else {
                out.extend_from_slice(b"   ");
            }
        }
        out.extend_from_slice(b" |");
        for byte in chunk {
            out.push(if byte.is_ascii_graphic() || *byte == b' ' { *byte } else { b'.' });
        }
        out.extend_from_slice(b"|\r\n");
        *offset += chunk.len() as u64;
    }
    out
}

/// 枚举系统串口；用于设置页的“串口”列表/热插拔提示。
pub fn list_ports() -> Vec<String> {
    match serialport::available_ports() {
        Ok(ports) => ports
            .into_iter()
            .map(|port| match port.port_type {
                serialport::SerialPortType::UsbPort(info) => format!(
                    "{}  USB {:04x}:{:04x}{}",
                    port.port_name,
                    info.vid,
                    info.pid,
                    info.product.map(|p| format!(" {p}")).unwrap_or_default()
                ),
                serialport::SerialPortType::PciPort => format!("{}  PCI", port.port_name),
                serialport::SerialPortType::BluetoothPort => {
                    format!("{}  Bluetooth", port.port_name)
                },
                serialport::SerialPortType::Unknown => port.port_name,
            })
            .collect(),
        Err(error) => {
            eprintln!("[serial-native] 枚举串口失败: {error}");
            Vec::new()
        },
    }
}

/// 串口控制线/运行时状态（UI 面板用）。
#[derive(Clone, Debug, Default)]
pub struct SerialStatus {
    pub dtr: bool,
    pub rts: bool,
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum SerialCmd {
    Dtr(bool),
    Rts(bool),
    Break,
}

/// UI → 串口控制线程的句柄；可在连接期间切 DTR/RTS、发 Break、读状态。
#[derive(Clone)]
pub struct SerialControlHandle {
    tx: mpsc::Sender<SerialCmd>,
    status: Arc<Mutex<SerialStatus>>,
}

impl SerialControlHandle {
    pub fn set_dtr(&self, on: bool) {
        let _ = self.tx.send(SerialCmd::Dtr(on));
    }
    pub fn set_rts(&self, on: bool) {
        let _ = self.tx.send(SerialCmd::Rts(on));
    }
    pub fn send_break(&self) {
        let _ = self.tx.send(SerialCmd::Break);
    }
    pub fn status(&self) -> SerialStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// 串口读/写失败 = 设备拔出或链路断开：把错误写到面板状态里。
///
/// 只保留第一个错误（后续重试错误不覆盖根因）；控制线程每 50ms 刷新状态时
/// 会先读取已有 `error` 再写回，避免把 reader/writer 标记的断开信息抹掉。
fn note_disconnected(status: &Mutex<SerialStatus>, action: &str, error: &io::Error) {
    if let Ok(mut current) = status.lock() {
        if current.error.is_none() {
            current.error = Some(format!("设备断开（{action}）: {error}"));
        }
    }
}

fn start_control(
    mut port: TTYPort,
    dtr: bool,
    rts: bool,
    status: Arc<Mutex<SerialStatus>>,
) -> SerialControlHandle {
    let (tx, rx) = mpsc::channel();
    let shared = status.clone();
    let _ = std::thread::Builder::new()
        .name("serial-control".into())
        .spawn(move || {
            let mut current = SerialStatus { dtr, rts, ..Default::default() };
            let _ = port.write_data_terminal_ready(current.dtr);
            let _ = port.write_request_to_send(current.rts);
            loop {
                let mut disconnected = false;
                loop {
                    match rx.try_recv() {
                        Ok(SerialCmd::Dtr(on)) => current.dtr = on,
                        Ok(SerialCmd::Rts(on)) => current.rts = on,
                        Ok(SerialCmd::Break) => {
                            if let Err(error) = port.send_break(serialport::BreakDuration::Short) {
                                current.error = Some(format!("send break: {error}"));
                            }
                        },
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        },
                    }
                }
                if disconnected {
                    break;
                }
                let _ = port.write_data_terminal_ready(current.dtr);
                let _ = port.write_request_to_send(current.rts);
                current.cts = port.read_clear_to_send().unwrap_or(false);
                current.dsr = port.read_data_set_ready().unwrap_or(false);
                current.dcd = port.read_carrier_detect().unwrap_or(false);
                if let Ok(mut slot) = shared.lock() {
                    // reader/writer 可能已经先一步标记断开；保留它。
                    current.error = slot.error.clone().or(current.error.take());
                    *slot = current.clone();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    SerialControlHandle { tx, status }
}

pub struct SerialTransport {
    reader: Box<dyn SerialPort>,
    writer: Box<dyn SerialPort>,
    control: Box<dyn SerialPort>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<SerialStatus>>,
    formatter: OutputFormatter,
    pending: Vec<u8>,
    pos: usize,
    newline: NewlineMode,
    input: InputMode,
    encoder: InputEncoder,
}

impl SerialTransport {
    pub fn open(profile: &Profile) -> io::Result<(Self, SerialControlHandle)> {
        let port = profile.serial_port.trim();
        if port.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "串口设备不能为空"));
        }
        let baud = profile.baud.max(1);
        let data_bits = match profile.data_bits {
            5 => DataBits::Five,
            6 => DataBits::Six,
            7 => DataBits::Seven,
            _ => DataBits::Eight,
        };
        let parity = match profile.parity.as_str() {
            "even" => Parity::Even,
            "odd" => Parity::Odd,
            _ => Parity::None,
        };
        let stop_bits = if profile.stop_bits == 2 { StopBits::Two } else { StopBits::One };
        let flow = match profile.flow_control.as_str() {
            "software" => FlowControl::Software,
            "hardware" => FlowControl::Hardware,
            _ => FlowControl::None,
        };
        let builder = serialport::new(port, baud)
            .data_bits(data_bits)
            .parity(parity)
            .stop_bits(stop_bits)
            .flow_control(flow)
            .timeout(READ_TIMEOUT);
        let native = builder.open_native().map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("打开串口 {port}（{baud} {}{}/{}/{}）失败: {e}", profile.data_bits, profile.parity, profile.stop_bits, profile.flow_control),
            )
        })?;
        let reader = native.try_clone().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("复制串口读句柄 {port} 失败: {e}"))
        })?;
        let writer = native.try_clone().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("复制串口写句柄 {port} 失败: {e}"))
        })?;
        let control_port = native.try_clone_native().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("复制串口控制句柄 {port} 失败: {e}"))
        })?;
        let status = Arc::new(Mutex::new(SerialStatus {
            dtr: profile.dtr,
            rts: profile.rts,
            ..Default::default()
        }));
        let ui_control = start_control(control_port, profile.dtr, profile.rts, status.clone());
        let control = native.try_clone().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("复制串口控制句柄 {port} 失败: {e}"))
        })?;
        Ok((
            Self {
                reader,
                writer,
                control,
                stop: Arc::new(AtomicBool::new(false)),
                status,
                formatter: OutputFormatter::new(
                    parse_output(&profile.serial_output),
                    profile.serial_timestamp,
                ),
                pending: Vec::new(),
                pos: 0,
                newline: parse_newline(&profile.serial_newline),
                input: parse_input(&profile.serial_input),
                encoder: InputEncoder::default(),
            },
            ui_control,
        ))
    }
}

impl PaneTransport for SerialTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(n) = take_pending(&self.pending, &mut self.pos, buf) {
            return Ok(n);
        }
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(0);
            }
            let mut raw = [0u8; 512];
            match self.reader.read(&mut raw) {
                Ok(0) => {
                    // tty 对端消失（PTY master 关闭 / 设备移除）时可能返回 EOF
                    // 而不是 EIO；同样按断开处理，让 pump_reader 收尾。
                    note_disconnected(
                        &self.status,
                        "读取结束",
                        &io::Error::new(io::ErrorKind::UnexpectedEof, "serial eof"),
                    );
                    return Ok(0);
                },
                Ok(n) => {
                    self.pending = self.formatter.format(&raw[..n]);
                    self.pos = 0;
                    return self.read(buf);
                },
                Err(e)
                    if e.kind() == io::ErrorKind::TimedOut
                        || e.kind() == io::ErrorKind::WouldBlock =>
                {
                    continue
                },
                Err(e) => {
                    note_disconnected(&self.status, "读取失败", &e);
                    return Err(e);
                },
            }
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let encoded = self.encoder.encode(data, self.input, self.newline);
        if let Err(error) = self.writer.write_all(&encoded) {
            note_disconnected(&self.status, "写入失败", &error);
            return Err(error);
        }
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) {}

    fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
    }

    fn into_halves(self: Box<Self>) -> io::Result<TransportHalves> {
        let SerialTransport {
            reader,
            writer,
            control,
            stop,
            status,
            formatter,
            pending,
            pos,
            newline,
            input,
            encoder,
        } = *self;
        let shutdown_stop = stop.clone();
        Ok(TransportHalves {
            reader: Box::new(SerialReader {
                port: reader,
                stop: stop.clone(),
                status: status.clone(),
                formatter,
                pending,
                pos,
            }),
            writer: Box::new(SerialWriter {
                port: writer,
                status,
                newline,
                input,
                encoder,
            }),
            control: Box::new(SerialControl { port: control, stop: stop.clone() }),
            shutdown: TransportShutdown::new(move || {
                shutdown_stop.store(true, Ordering::Release);
            }),
        })
    }
}

struct SerialReader {
    port: Box<dyn SerialPort>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<SerialStatus>>,
    formatter: OutputFormatter,
    pending: Vec<u8>,
    pos: usize,
}

impl Read for SerialReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(n) = take_pending(&self.pending, &mut self.pos, buf) {
            return Ok(n);
        }
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(0);
            }
            let mut raw = [0u8; 512];
            match self.port.read(&mut raw) {
                Ok(0) => {
                    // EOF 必须原样返回 0：继续递归重读会空转（旧实现会在
                    // 设备消失后 stack overflow）。pump_reader 收到 0 即收尾。
                    note_disconnected(
                        &self.status,
                        "读取结束",
                        &io::Error::new(io::ErrorKind::UnexpectedEof, "serial eof"),
                    );
                    return Ok(0);
                },
                Ok(n) => {
                    self.pending = self.formatter.format(&raw[..n]);
                    self.pos = 0;
                    return self.read(buf);
                },
                Err(e)
                    if e.kind() == io::ErrorKind::TimedOut
                        || e.kind() == io::ErrorKind::WouldBlock =>
                {
                    continue
                },
                Err(e) => {
                    note_disconnected(&self.status, "读取失败", &e);
                    return Err(e);
                },
            }
        }
    }
}

struct SerialWriter {
    port: Box<dyn SerialPort>,
    status: Arc<Mutex<SerialStatus>>,
    newline: NewlineMode,
    input: InputMode,
    encoder: InputEncoder,
}

impl Write for SerialWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let data = self.encoder.encode(buf, self.input, self.newline);
        if let Err(error) = self.port.write_all(&data) {
            note_disconnected(&self.status, "写入失败", &error);
            return Err(error);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Err(error) = self.port.flush() {
            note_disconnected(&self.status, "刷新失败", &error);
            return Err(error);
        }
        Ok(())
    }
}

struct SerialControl {
    port: Box<dyn SerialPort>,
    stop: Arc<AtomicBool>,
}

impl TransportControl for SerialControl {
    fn resize(&mut self, _cols: u16, _rows: u16) {}
    fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.port.clear(serialport::ClearBuffer::All);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    #[test]
    fn newline_and_hexdump_helpers() {
        assert_eq!(convert_newlines(b"a\r\nb", NewlineMode::Lf), b"a\nb");
        assert_eq!(convert_newlines(b"a\rb", NewlineMode::Crlf), b"a\r\nb");
        assert_eq!(convert_newlines(b"a\nb", NewlineMode::Cr), b"a\rb");
        let mut offset = 0;
        let dump = format_hexdump(b"AB", &mut offset);
        let text = String::from_utf8_lossy(&dump);
        assert!(text.contains("00000000"), "{text}");
        assert!(text.contains("41 42"), "{text}");
        assert!(text.contains("|AB|"), "{text}");
        assert_eq!(offset, 2);
    }

    #[test]
    fn input_modes_are_raw_except_hex() {
        // text/readline/byte 都是原样发送，只做显式换行转换；readline 不做本地行编辑。
        for mode in [InputMode::Text, InputMode::Readline, InputMode::Byte] {
            let mut encoder = InputEncoder::default();
            assert_eq!(encoder.encode(b"hello\r\n", mode, NewlineMode::None), b"hello\r\n");
            assert_eq!(encoder.encode(b"a\r\nb", mode, NewlineMode::Lf), b"a\nb");
            assert_eq!(encoder.encode(b"a\nb", mode, NewlineMode::Crlf), b"a\r\nb");
        }

        // Hex：空白可有可无，大小写都接受。
        let mut encoder = InputEncoder::default();
        assert_eq!(encoder.encode(b"48 65 6C 6c 6f", InputMode::Hex, NewlineMode::Crlf), b"Hello");
        assert_eq!(
            encoder.encode(b"0d0A", InputMode::Hex, NewlineMode::Lf),
            b"\r\n",
            "hex 模式不能再做换行转换"
        );
        // 非法字符忽略；不做 0x 前缀特判（0 与 4 会组成 0x04）。
        assert_eq!(encoder.encode(b"41 zz 42", InputMode::Hex, NewlineMode::None), b"AB");
    }

    #[test]
    fn hex_input_holds_odd_nibble_across_writes() {
        let mut encoder = InputEncoder::default();
        // 终端逐键输入：每一次 write 只有一个字节。
        assert!(encoder.encode(b"4", InputMode::Hex, NewlineMode::None).is_empty());
        assert!(encoder.encode(b" ", InputMode::Hex, NewlineMode::None).is_empty());
        assert_eq!(encoder.encode(b"1", InputMode::Hex, NewlineMode::None), b"A");
        // 新一次写入的单半字节继续暂存，下一 write 补齐。
        assert_eq!(encoder.encode(b"f", InputMode::Hex, NewlineMode::None), b"");
        assert_eq!(encoder.encode(b"f", InputMode::Hex, NewlineMode::None), b"\xff");
        // 奇数半字节保留到下一次，不会在空白处丢失。
        assert_eq!(encoder.encode(b"a", InputMode::Hex, NewlineMode::None), b"");
        assert_eq!(encoder.encode(b"\n", InputMode::Hex, NewlineMode::None), b"");
        assert_eq!(encoder.encode(b"b", InputMode::Hex, NewlineMode::None), b"\xab");
    }

    #[test]
    fn timestamp_format_is_fixed_width_and_wraps() {
        assert_eq!(format_timestamp_prefix(0), "[00:00:00.000] ");
        assert_eq!(format_timestamp_prefix(3_661_123), "[01:01:01.123] ");
        assert_eq!(format_timestamp_prefix(86_399_999), "[23:59:59.999] ");
        assert_eq!(format_timestamp_prefix(86_400_000), "[00:00:00.000] ");
    }

    #[test]
    fn timestamp_prefixes_each_line_across_chunks_without_doubling_crlf() {
        let mut stamper = LineTimestamper::new();
        let t = "[T] ";
        assert_eq!(stamper.prefix(b"hello\nwor", t), b"[T] hello\n[T] wor");
        assert_eq!(stamper.prefix(b"ld\n", t), b"ld\n");
        // 上一 chunk 以 \n 结束：新 chunk 是下一行，需要前缀。
        assert_eq!(stamper.prefix(b"next", t), b"[T] next");
        // \r\n 不重复加；CR-only 行在下一个非 \n 字节前加。
        assert_eq!(stamper.prefix(b"x\r\ny", t), b"x\r\n[T] y");
        assert_eq!(stamper.prefix(b"\rz", t), b"\r[T] z");
        // 每行都有前缀，空行也不例外。
        let mut stamper = LineTimestamper::new();
        assert_eq!(stamper.prefix(b"\nq", t), b"[T] \n[T] q");
        // 跨 chunk 的 \r\n 不能在中间插前缀。
        let mut stamper = LineTimestamper::new();
        assert_eq!(stamper.prefix(b"x\r", t), b"[T] x\r");
        assert_eq!(stamper.prefix(b"\nq", t), b"\n[T] q");
    }

    #[test]
    fn missing_device_is_a_clean_error() {
        let p = Profile {
            kind: "serial".into(),
            serial_port: "/definitely/not/a/serial/device".into(),
            baud: 115200,
            ..Profile::default()
        };
        let err = match SerialTransport::open(&p) {
            Ok(_) => panic!("missing serial device must fail"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("打开串口"), "{err}");
    }
}

#[cfg(all(test, unix))]
mod pty_tests {
    use super::*;
    use std::os::fd::FromRawFd;

    #[test]
    fn native_serial_roundtrips_through_a_pty() {
        let mut master = 0i32;
        let mut slave = 0i32;
        let ok = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(ok, 0, "openpty failed: {}", io::Error::last_os_error());
        let path = format!("/proc/self/fd/{slave}");
        let p = Profile {
            kind: "serial".into(),
            serial_port: path,
            baud: 115200,
            ..Profile::default()
        };
        let (mut transport, control) =
            SerialTransport::open(&p).expect("open pty via serialport");
        control.set_dtr(false);
        control.set_rts(false);
        std::thread::sleep(Duration::from_millis(120));
        let status = control.status();
        assert!(!status.dtr && !status.rts);
        control.send_break();
        std::thread::sleep(Duration::from_millis(120));
        // 读先开始，另一个线程在超过 READ_TIMEOUT 后才写；确保实际走到
        // TimedOut 重试分支，而不是数据已经就绪。
        let master_w = unsafe { libc::dup(master) };
        assert!(master_w >= 0);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(READ_TIMEOUT + Duration::from_millis(150));
            let msg = b"NATIVE-SERIAL";
            let written = unsafe { libc::write(master_w, msg.as_ptr().cast(), msg.len()) };
            unsafe { libc::close(master_w) };
            assert_eq!(written, msg.len() as isize);
        });
        let mut buf = [0u8; 32];
        let n = transport.read(&mut buf).expect("read native serial after idle timeout");
        writer.join().unwrap();
        assert_eq!(&buf[..n], b"NATIVE-SERIAL");

        transport.write_all(b"REPLY").unwrap();
        let mut reply = [0u8; 16];
        let n = unsafe { libc::read(master, reply.as_mut_ptr().cast(), reply.len()) };
        assert_eq!(n, 5);
        assert_eq!(&reply[..5], b"REPLY");
        transport.close();
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }

    fn open_pty_pair() -> (i32, i32) {
        let mut master = 0i32;
        let mut slave = 0i32;
        let ok = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(ok, 0, "openpty failed: {}", io::Error::last_os_error());
        (master, slave)
    }

    fn serial_profile(slave: i32) -> Profile {
        Profile {
            kind: "serial".into(),
            serial_port: format!("/proc/self/fd/{slave}"),
            baud: 115200,
            ..Profile::default()
        }
    }

    /// 从 PTY master 读满 `want` 字节；5s 无数据直接失败，避免测试挂死。
    fn read_master_exact(master: i32, want: usize) -> Vec<u8> {
        let mut out = vec![0u8; want];
        let mut off = 0usize;
        while off < want {
            let mut pfd = libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 };
            let rc = unsafe { libc::poll(&mut pfd, 1, 5000) };
            assert!(rc > 0, "master read timeout: {off}/{want}");
            let n = unsafe { libc::read(master, out[off..].as_mut_ptr().cast(), want - off) };
            assert!(n > 0, "master read failed: {}", io::Error::last_os_error());
            off += n as usize;
        }
        out
    }

    /// SERIAL-004 端到端：Profile → transport 写入（与 pane writer 共用 encoder）。
    #[test]
    fn pty_hex_and_readline_input_modes_reach_the_wire() {
        {
            let (master, slave) = open_pty_pair();
            let mut profile = serial_profile(slave);
            profile.serial_input = "hex".into();
            let (mut transport, control) =
                SerialTransport::open(&profile).expect("open pty via serialport");
            // 一次写多半字节对 + 空白。
            transport.write_all(b"41 42 43").expect("hex write 1");
            // 奇数半字节跨 write 调用暂存。
            transport.write_all(b"4").expect("hex write 2");
            transport.write_all(b"4").expect("hex write 3");
            let wire = read_master_exact(master, 4);
            assert_eq!(&wire, b"ABCD", "hex pairs must become bytes");
            transport.close();
            drop(transport);
            drop(control);
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
        }
        {
            let (master, slave) = open_pty_pair();
            let mut profile = serial_profile(slave);
            profile.serial_input = "readline".into();
            let (mut transport, control) =
                SerialTransport::open(&profile).expect("open pty via serialport");
            // readline 不做本地行编辑：CR、退格等原样交给设备。
            transport.write_all(b"ab\x7f\r").expect("readline write");
            let wire = read_master_exact(master, 4);
            assert_eq!(&wire, b"ab\x7f\r");
            transport.close();
            drop(transport);
            drop(control);
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
        }
    }

    #[test]
    fn serial_eof_and_write_error_mark_device_disconnected() {
        // 读方向：PTY master 关闭 -> slave read 返回 EOF（不是 EIO）。
        let (master, slave) = open_pty_pair();
        let (mut transport, control) =
            SerialTransport::open(&serial_profile(slave)).expect("open pty via serialport");
        unsafe { libc::close(master) };
        let mut buf = [0u8; 16];
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "PTY EOF 未在 5s 内上报"
            );
            match transport.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                // Linux 上 master 关闭后 slave read 可能报 EPIPE 而不是 EOF；
                // pump_reader 对二者都按 transport 关闭处理。
                Err(_) => break,
            }
        }
        let status = control.status();
        let error = status.error.expect("EOF/EPIPE 必须标记串口断开");
        assert!(error.contains("设备断开"), "status.error={error:?}");
        assert!(
            error.contains("读取结束") || error.contains("读取失败"),
            "status.error={error:?}"
        );
        transport.close();
        unsafe { libc::close(slave) };

        // 写方向：master 关闭后 write 返回 EIO，同样必须标记断开。
        let (master, slave) = open_pty_pair();
        let (mut transport, control) =
            SerialTransport::open(&serial_profile(slave)).expect("open pty via serialport");
        unsafe { libc::close(master) };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let write_error = loop {
            assert!(
                std::time::Instant::now() < deadline,
                "PTY write 未在 5s 内报错"
            );
            match transport.write_all(b"x") {
                Ok(()) => continue,
                Err(error) => break error,
            }
        };
        // PTY master 关闭后写 slave：Linux/serialport 可能给 EIO/EPIPE，
        // serialport 也可能把 EPIPE 包成不带 raw_os_error 的 BrokenPipe。
        assert!(
            matches!(write_error.raw_os_error(), Some(libc::EIO))
                || write_error.kind() == io::ErrorKind::BrokenPipe,
            "{write_error}"
        );
        let error = control.status().error.expect("写失败必须标记串口断开");
        assert!(error.contains("设备断开"), "status.error={error:?}");
        assert!(error.contains("写入失败"), "status.error={error:?}");
        transport.close();
        unsafe { libc::close(slave) };
    }

}
