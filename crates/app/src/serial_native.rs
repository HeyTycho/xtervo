// SPDX-License-Identifier: GPL-3.0-only
//! 进程内串口后端（`serialport` crate）：不再依赖系统 socat。
//!
//! 与 `Backend::Plugin` 共用 pane 的读线程/写 backlog/actor；`read` 用 100ms
//! 超时轮询 stop 标志，`into_halves` 拆成读/写/控制三个 clone。

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits, TTYPort};
use tervo_core::transport::{
    PaneTransport, TransportControl, TransportHalves, TransportShutdown,
};

use crate::profile::Profile;

const READ_TIMEOUT: Duration = Duration::from_millis(100);

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

fn start_control(mut port: TTYPort, dtr: bool, rts: bool) -> SerialControlHandle {
    let (tx, rx) = mpsc::channel();
    let status = Arc::new(Mutex::new(SerialStatus { dtr, rts, ..Default::default() }));
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
        let ui_control = start_control(control_port, profile.dtr, profile.rts);
        let control = native.try_clone().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("复制串口控制句柄 {port} 失败: {e}"))
        })?;
        Ok((
            Self { reader, writer, control, stop: Arc::new(AtomicBool::new(false)) },
            ui_control,
        ))
    }
}

impl PaneTransport for SerialTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(0);
            }
            match self.reader.read(buf) {
                Ok(n) => return Ok(n),
                Err(e)
                    if e.kind() == io::ErrorKind::TimedOut
                        || e.kind() == io::ErrorKind::WouldBlock =>
                {
                    continue
                },
                Err(e) => return Err(e),
            }
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)
    }

    fn resize(&mut self, _cols: u16, _rows: u16) {}

    fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
    }

    fn into_halves(self: Box<Self>) -> io::Result<TransportHalves> {
        let SerialTransport { reader, writer, control, stop } = *self;
        let shutdown_stop = stop.clone();
        Ok(TransportHalves {
            reader: Box::new(SerialReader { port: reader, stop: stop.clone() }),
            writer: Box::new(SerialWriter { port: writer }),
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
}

impl Read for SerialReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(0);
            }
            match self.port.read(buf) {
                Ok(n) => return Ok(n),
                Err(e)
                    if e.kind() == io::ErrorKind::TimedOut
                        || e.kind() == io::ErrorKind::WouldBlock =>
                {
                    continue
                },
                Err(e) => return Err(e),
            }
        }
    }
}

struct SerialWriter {
    port: Box<dyn SerialPort>,
}

impl Write for SerialWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.port.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.port.flush()
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
}
