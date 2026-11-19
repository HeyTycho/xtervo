// SPDX-License-Identifier: GPL-3.0-only
//! 进程内串口后端（`serialport` crate）：不再依赖系统 socat。
//!
//! 与 `Backend::Plugin` 共用 pane 的读线程/写 backlog/actor；`read` 用 100ms
//! 超时轮询 stop 标志，`into_halves` 拆成读/写/控制三个 clone。

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use tervo_core::transport::{
    PaneTransport, TransportControl, TransportHalves, TransportShutdown,
};

use crate::profile::Profile;

const READ_TIMEOUT: Duration = Duration::from_millis(100);

pub struct SerialTransport {
    reader: Box<dyn SerialPort>,
    writer: Box<dyn SerialPort>,
    control: Box<dyn SerialPort>,
    stop: Arc<AtomicBool>,
}

impl SerialTransport {
    pub fn open(profile: &Profile) -> io::Result<Self> {
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
        let reader = builder.open().map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("打开串口 {port}（{baud} {}{}/{}/{}）失败: {e}", profile.data_bits, profile.parity, profile.stop_bits, profile.flow_control),
            )
        })?;
        let writer = reader.try_clone().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("复制串口写句柄 {port} 失败: {e}"))
        })?;
        let control = reader.try_clone().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("复制串口控制句柄 {port} 失败: {e}"))
        })?;
        Ok(Self { reader, writer, control, stop: Arc::new(AtomicBool::new(false)) })
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
                    if io::ErrorKind::Other == io::ErrorKind::TimedOut
                        || io::ErrorKind::Other == io::ErrorKind::WouldBlock =>
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
                    if io::ErrorKind::Other == io::ErrorKind::TimedOut
                        || io::ErrorKind::Other == io::ErrorKind::WouldBlock =>
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
        let mut transport = SerialTransport::open(&p).expect("open pty via serialport");
        let msg = b"NATIVE-SERIAL";
        let written = unsafe {
            libc::write(master, msg.as_ptr().cast(), msg.len())
        };
        assert_eq!(written, msg.len() as isize);
        let mut buf = [0u8; 32];
        let n = transport.read(&mut buf).expect("read native serial");
        assert_eq!(&buf[..n], msg);

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
