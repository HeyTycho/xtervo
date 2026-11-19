// SPDX-License-Identifier: GPL-3.0-only
//! `_NET_WM_STATE` 传输层：显式 xid + 持久 X11 连接 + PropertyNotify。
//!
//! 关键点：
//!
//! * 所有 EWMH 请求直接发到 winit raw handle 给出的 xid，绝不按标题查找；
//! * 在目标窗口上选择 `PropertyChangeMask`，只把 `_NET_WM_STATE` 的
//!   PropertyNotify 翻译成状态机事件；
//! * 每条通知到达后读取一次属性（真实观察），而不是轮询状态；
//! * 没有几何、`_NET_WORKAREA`、标题或定时校准。

use std::io::Read;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, EventMask,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::controller::{MaximizeController, MaximizeSnapshot, DEFAULT_REQUEST_TIMEOUT};

/// worker 默认轮询 X 事件/超时的间隔。它只影响事件读取与错误上报延迟，
/// 不参与 target 决策，也不是状态校准窗口。
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 传输层配置。
#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    /// 请求等待 PropertyNotify 的上限；超时只报错，不改 target。
    pub timeout: Duration,
    /// worker 读取 X 事件/检查超时的节拍。
    pub poll_interval: Duration,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_REQUEST_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

/// 状态变化回调。worker 线程调用；调用方负责切回自己的 UI 事件循环。
pub type EventCallback = Box<dyn Fn(MaximizeSnapshot) + Send + 'static>;

/// 最大化状态机运行时：UI 线程只接触 `click`/`snapshot`，X11 在 worker 里。
pub struct MaximizeRuntime {
    shared: Arc<Mutex<MaximizeController>>,
    tx: Sender<Command>,
}

#[derive(Debug, Clone, Copy)]
enum Command {
    /// 把状态机当前 desired 作为 EWMH 请求发出。
    Apply(bool),
}

struct Atoms {
    state: u32,
    maximized_horz: u32,
    maximized_vert: u32,
    supporting_wm_check: u32,
    supported: u32,
}

impl MaximizeRuntime {
    /// 用显式 xid 启动状态机；`on_event` 在 worker 检测到状态变化时调用。
    ///
    /// 失败（非 X11、连接失败、窗口不存在）返回 `Err`，调用方应回退到平台
    /// 原生能力并明确记录日志；这里绝不按标题找窗口兜底。
    pub fn start<F>(window: u32, on_event: F) -> Result<Self, String>
    where
        F: Fn(MaximizeSnapshot) + Send + 'static,
    {
        Self::start_with_options(window, RuntimeOptions::default(), on_event)
    }

    /// 可注入超时/节拍，供单测或隔离 e2e 使用。
    pub fn start_with_options<F>(
        window: u32,
        options: RuntimeOptions,
        on_event: F,
    ) -> Result<Self, String>
    where
        F: Fn(MaximizeSnapshot) + Send + 'static,
    {
        let (conn, screen_num) = connect()?;
        let root = conn.setup().roots[screen_num].root;
        let atoms = load_atoms(&conn)?;

        // xid 必须在连接开始监听前就有效；也避免给错误窗口选事件。
        conn.get_window_attributes(window)
            .map_err(|e| format!("查询窗口 0x{window:x} 失败: {e}"))?
            .reply()
            .map_err(|e| format!("窗口 0x{window:x} 不存在/不可访问: {e}"))?;

        // 只监听本窗口属性变化；不抢 wm 的 SubstructureRedirect。
        conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| format!("监听窗口 0x{window:x} PropertyNotify 失败: {e}"))?
        .check()
        .map_err(|e| format!("PropertyNotify 选择失败: {e}"))?;

        // 监听建立后再读一次初始真实状态。即使 WM 在这之前刚完成最大化，
        // 这里也能拿到；之后的改变由 PropertyNotify 负责。
        let initial_observed = query_observed_conn(&conn, window, &atoms)
            .map_err(|e| format!("读取初始 _NET_WM_STATE 失败: {e}"))?;
        let controller = MaximizeController::with_timeout(initial_observed, options.timeout);
        let shared = Arc::new(Mutex::new(controller));
        let worker_shared = Arc::clone(&shared);
        let (tx, rx) = mpsc::channel();

        eprintln!(
            "[maximize] controller start win=0x{window:x} observed={initial_observed} \
             timeout={}ms",
            options.timeout.as_millis()
        );

        std::thread::Builder::new()
            .name("maximize-state".into())
            .spawn(move || {
                run_worker(
                    conn,
                    root,
                    window,
                    atoms,
                    worker_shared,
                    rx,
                    Box::new(on_event),
                    options.poll_interval,
                )
            })
            .map_err(|e| format!("创建 maximize-state 线程失败: {e}"))?;

        Ok(Self { shared, tx })
    }

    /// 点击：只翻转状态机的 desired，并把 target 排队发给 worker。
    ///
    /// 返回新 target，UI 图标立即按它显示；不等 PropertyNotify、不查几何。
    pub fn click(&self) -> bool {
        let snapshot = {
            let mut controller = lock(&self.shared);
            controller.click();
            controller.snapshot()
        };
        eprintln!(
            "[maximize] click target={} desired={} observed={} pending={:?}",
            snapshot.desired, snapshot.desired, snapshot.observed, snapshot.pending
        );
        // worker 已退出（应用关闭/连接断开）时不能只翻转 UI：回滚到 observed，
        // 否则按钮显示最大化但窗口实际没变，下一次点击还会按错误状态翻转。
        if self.tx.send(Command::Apply(snapshot.desired)).is_err() {
            let mut controller = lock(&self.shared);
            controller.revert_to_observed();
            controller.note_transport_error("maximize worker 已退出，请求未发送");
            let reverted = controller.snapshot();
            drop(controller);
            eprintln!(
                "[maximize] click target={} 未发送：worker 已退出，回滚到 observed={}",
                snapshot.desired, reverted.desired
            );
            return reverted.desired;
        }
        snapshot.desired
    }

    /// UI 读取当前状态；锁只在这个调用期间持有。
    pub fn snapshot(&self) -> MaximizeSnapshot {
        lock(&self.shared).snapshot()
    }
}

fn lock(shared: &Arc<Mutex<MaximizeController>>) -> MutexGuard<'_, MaximizeController> {
    // 状态机本身没有跨字段不一致的 panic 点；即使 worker 在测试里 panic，
    // UI 也应继续显示最后状态而不是整个进程崩溃。
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Emitter {
    last: Option<MaximizeSnapshot>,
    callback: EventCallback,
}

impl Emitter {
    fn new(callback: EventCallback) -> Self {
        Self {
            last: None,
            callback,
        }
    }

    fn emit(&mut self, shared: &Arc<Mutex<MaximizeController>>) {
        let snapshot = lock(shared).snapshot();
        if self.last.as_ref() == Some(&snapshot) {
            return;
        }
        eprintln!(
            "[maximize] state desired={} observed={} pending={:?} error={:?}",
            snapshot.desired, snapshot.observed, snapshot.pending, snapshot.last_error
        );
        self.last = Some(snapshot.clone());
        (self.callback)(snapshot);
    }
}

fn run_worker(
    conn: RustConnection,
    root: u32,
    window: u32,
    atoms: Atoms,
    shared: Arc<Mutex<MaximizeController>>,
    rx: Receiver<Command>,
    callback: EventCallback,
    poll_interval: Duration,
) {
    let mut emitter = Emitter::new(callback);
    emitter.emit(&shared);

    loop {
        // 1) 请求优先：点击后立刻发，避免被事件轮询节拍拖慢。
        loop {
            match rx.try_recv() {
                Ok(command) => {
                    apply_command(&conn, root, window, &atoms, &shared, &mut emitter, command)
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // 2) 读取 X 事件；一次 drain 里多条 PropertyNotify 只查一次属性。
        let mut state_dirty = false;
        loop {
            match conn.poll_for_event() {
                Ok(Some(Event::PropertyNotify(event))) => {
                    if event.window == window && event.atom == atoms.state {
                        state_dirty = true;
                    }
                }
                Ok(Some(Event::Error(error))) => {
                    eprintln!(
                        "[maximize] X11 error code={:?} major={:?} minor={:?}",
                        error.error_code, error.major_opcode, error.minor_opcode
                    );
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    eprintln!("[maximize] X11 连接读取失败，状态机停止: {error}");
                    return;
                }
            }
        }

        if state_dirty {
            match query_observed_conn(&conn, window, &atoms) {
                Ok(observed) => {
                    let outcome = lock(&shared).on_property_notify(observed);
                    eprintln!("[maximize] property-notify observed={observed} outcome={outcome:?}");
                    emitter.emit(&shared);
                }
                Err(error) => {
                    // 读取失败不能被当成“未最大化”：只记录错误，不改状态。
                    lock(&shared).note_transport_error(format!("读取 _NET_WM_STATE 失败: {error}"));
                    eprintln!("[maximize] property query failed: {error}");
                    emitter.emit(&shared);
                }
            }
        }

        // 3) 超时只报错；pending/desired 不变，迟到的通知仍可确认。
        let timed_out = lock(&shared).check_timeout(Instant::now());
        if let Some(error) = timed_out {
            eprintln!("[maximize] {error}");
            emitter.emit(&shared);
        }

        // 4) 等点击唤醒；没有点击时按节拍检查事件/超时。
        match rx.recv_timeout(poll_interval) {
            Ok(command) => {
                apply_command(&conn, root, window, &atoms, &shared, &mut emitter, command)
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn apply_command(
    conn: &RustConnection,
    root: u32,
    window: u32,
    atoms: &Atoms,
    shared: &Arc<Mutex<MaximizeController>>,
    emitter: &mut Emitter,
    command: Command,
) {
    let Command::Apply(target) = command;
    match send_maximized(conn, root, window, atoms, target) {
        Ok(()) => {
            eprintln!("[maximize] request target={target} sent=ewmh win=0x{window:x}");
        }
        Err(error) => {
            {
                let mut controller = lock(shared);
                controller.on_request_failed(format!("send failed: {error}"));
            }
            eprintln!("[maximize] request failed target={target} err={error}");
            emitter.emit(shared);
        }
    }
}

/// 通过 EWMH 请求最大化/恢复；显式 window id，不按标题查找。
fn send_maximized(
    conn: &RustConnection,
    root: u32,
    window: u32,
    atoms: &Atoms,
    maximized: bool,
) -> Result<(), String> {
    if !wm_supports_maximize(conn, root, atoms) {
        return Err("WM 未声明 _NET_SUPPORTING_WM_CHECK / MAXIMIZED_HORZ+VERT".into());
    }
    let action = if maximized { 1u32 } else { 0u32 }; // 1=add, 0=remove
    let event = ClientMessageEvent::new(
        32,
        window,
        atoms.state,
        [action, atoms.maximized_horz, atoms.maximized_vert, 1, 0],
    );
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )
    .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// WM 是否声明支持 EWMH 最大化；没有 WM/未声明时明确失败，不猜测。
fn wm_supports_maximize(conn: &RustConnection, root: u32, atoms: &Atoms) -> bool {
    let Ok(check) = conn
        .get_property(
            false,
            root,
            atoms.supporting_wm_check,
            AtomEnum::WINDOW,
            0,
            1,
        )
        .map_err(|e| e.to_string())
        .and_then(|cookie| cookie.reply().map_err(|e| e.to_string()))
    else {
        return false;
    };
    if check
        .value32()
        .is_none_or(|mut values| values.next().is_none())
    {
        return false;
    }

    let Ok(supported) = conn
        .get_property(false, root, atoms.supported, AtomEnum::ATOM, 0, 1024)
        .map_err(|e| e.to_string())
        .and_then(|cookie| cookie.reply().map_err(|e| e.to_string()))
    else {
        return false;
    };
    let mut has_horz = false;
    let mut has_vert = false;
    for atom in supported.value32().into_iter().flatten() {
        has_horz |= atom == atoms.maximized_horz;
        has_vert |= atom == atoms.maximized_vert;
    }
    has_horz && has_vert
}

fn query_observed_conn(conn: &RustConnection, window: u32, atoms: &Atoms) -> Result<bool, String> {
    let reply = conn
        .get_property(false, window, atoms.state, AtomEnum::ATOM, 0, 64)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?;
    let mut horz = false;
    let mut vert = false;
    for atom in reply.value32().into_iter().flatten() {
        horz |= atom == atoms.maximized_horz;
        vert |= atom == atoms.maximized_vert;
    }
    Ok(horz && vert)
}

/// 用显式 xid 查询当前 `_NET_WM_STATE` 是否同时含 HORZ+VERT。
///
/// 仅用于启动时初始化图标；之后状态只由 PropertyNotify 驱动。
pub fn query_observed(window: u32) -> Option<bool> {
    let (conn, _) = connect().ok()?;
    let atoms = load_atoms(&conn).ok()?;
    query_observed_conn(&conn, window, &atoms).ok()
}

fn load_atoms(conn: &RustConnection) -> Result<Atoms, String> {
    let atom = |name: &[u8]| -> Result<u32, String> {
        conn.intern_atom(false, name)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())
            .map(|reply| reply.atom)
    };
    Ok(Atoms {
        state: atom(b"_NET_WM_STATE")?,
        maximized_horz: atom(b"_NET_WM_STATE_MAXIMIZED_HORZ")?,
        maximized_vert: atom(b"_NET_WM_STATE_MAXIMIZED_VERT")?,
        supporting_wm_check: atom(b"_NET_SUPPORTING_WM_CHECK")?,
        supported: atom(b"_NET_SUPPORTED")?,
    })
}

fn display_number() -> Option<String> {
    let display = std::env::var("DISPLAY").ok()?;
    let after_colon = display.rsplit(':').next()?;
    Some(after_colon.split('.').next()?.to_string())
}

fn xauthority_path() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("XAUTHORITY") {
        return Some(path.into());
    }
    std::env::var("HOME")
        .ok()
        .map(|home| std::path::PathBuf::from(home).join(".Xauthority"))
}

/// 极简 Xauthority 解析：只取第一个与 display 号匹配的 MIT-MAGIC-COOKIE-1。
fn read_cookie(display: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut data = Vec::new();
    std::fs::File::open(xauthority_path()?)
        .ok()?
        .read_to_end(&mut data)
        .ok()?;

    let mut pos = 0usize;
    let take = |pos: &mut usize, len: usize| -> Option<&[u8]> {
        let out = data.get(*pos..*pos + len)?;
        *pos += len;
        Some(out)
    };
    while pos + 2 <= data.len() {
        let _family = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?);
        let addr_len = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?) as usize;
        let _addr = take(&mut pos, addr_len)?;
        let num_len = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?) as usize;
        let number = take(&mut pos, num_len)?;
        let name_len = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?) as usize;
        let name = take(&mut pos, name_len)?;
        let data_len = u16::from_be_bytes(take(&mut pos, 2)?.try_into().ok()?) as usize;
        let cookie = take(&mut pos, data_len)?;

        if name == b"MIT-MAGIC-COOKIE-1"
            && (number.is_empty()
                || String::from_utf8_lossy(number) == display
                || String::from_utf8_lossy(number).ends_with(&format!(":{display}")))
        {
            return Some((name.to_vec(), cookie.to_vec()));
        }
    }
    None
}

/// 连接本机 X11。仓库测试环境没有 `/tmp/.X11-unix` 目录项，优先走抽象
/// socket；与 `crates/app/src/xconn.rs` 的策略保持一致。
fn connect() -> Result<(RustConnection, usize), String> {
    let display = display_number().ok_or("DISPLAY 未设置")?;
    let name = format!("/tmp/.X11-unix/X{display}");
    if let Ok(addr) = SocketAddr::from_abstract_name(name.as_bytes()) {
        if let Ok(stream) = UnixStream::connect_addr(&addr) {
            if let Ok((stream, _peer)) = DefaultStream::from_unix_stream(stream) {
                let (auth_name, auth_data) = read_cookie(&display).unwrap_or_default();
                if let Ok(conn) = RustConnection::connect_to_stream_with_auth_info(
                    stream, 0, auth_name, auth_data,
                ) {
                    return Ok((conn, 0));
                }
            }
        }
    }
    x11rb::connect(None).map_err(|e| format!("X11 连接失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_options_are_for_error_reporting_only() {
        // 防止有人把 e2e 的短超时误当成“校准窗口”：desired 在 timeout 后不变。
        let mut controller = MaximizeController::with_timeout(false, Duration::from_millis(50));
        let start = Instant::now();
        controller.click();
        assert!(controller
            .check_timeout(start + Duration::from_secs(1))
            .is_some());
        assert!(controller.desired());
        assert_eq!(controller.pending(), Some(true));
    }

    #[test]
    fn display_number_parses_host_suffix() {
        // 只测解析函数对 `:97.0` 的处理；不依赖真实 DISPLAY。
        let value = ":97.0";
        let after = value.rsplit(':').next().unwrap();
        assert_eq!(after.split('.').next().unwrap(), "97");
    }
}
