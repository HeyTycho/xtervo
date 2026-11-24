// SPDX-License-Identifier: GPL-3.0-only
//! 输入延迟测量：X11 XTest 注入真实按键 + 应用内渲染打点。
//!
//! 测量链路（不含合成器扫描输出）：
//!   XTest fake_input → X server → winit → Slint key callback → PTY → Term →
//!   唤醒 → 拉帧 → 光栅化 → Image 属性 → Skia 渲染完成（AfterRendering）
//!
//! 之所以要 XTest 而不是在应用内伪造事件：只有前者能覆盖 X server 投递与
//! winit 事件循环唤醒这两段真实开销。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as XprotoExt, InputFocus, Window};
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

pub struct Injector {
    conn: RustConnection,
    win: Window,
    root: Window,
    keycodes: HashMap<char, u8>,
    ctrl: u8,
}

fn read_prop(conn: &RustConnection, win: Window, atom: u32) -> Option<String> {
    let reply = conn
        .get_property(false, win, atom, AtomEnum::ANY, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&reply.value).to_string())
    }
}

fn find_window(conn: &RustConnection, win: Window, title: &str, depth: usize) -> Option<Window> {
    if depth > 8 {
        return None;
    }
    let wm_name = AtomEnum::WM_NAME.into();
    if let Some(name) = read_prop(conn, win, wm_name) {
        if name.contains(title) {
            return Some(win);
        }
    }
    if let Ok(cookie) = conn.intern_atom(false, b"_NET_WM_NAME") {
        if let Ok(atom) = cookie.reply() {
            if let Some(name) = read_prop(conn, win, atom.atom) {
                if name.contains(title) {
                    return Some(win);
                }
            }
        }
    }
    let tree = conn.query_tree(win).ok()?.reply().ok()?;
    for child in tree.children {
        if let Some(found) = find_window(conn, child, title, depth + 1) {
            return Some(found);
        }
    }
    None
}

impl Injector {
    pub fn connect(title: &str, timeout: Duration) -> Option<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(inj) = Self::try_connect(title) {
                return Some(inj);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn try_connect(title: &str) -> Option<Self> {
        let (conn, screen_num) = crate::xconn::connect().ok()?;
        let setup = conn.setup();
        let root = setup.roots.get(screen_num)?.root;
        let win = find_window(&conn, root, title, 0)?;

        conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
            .ok()?
            .check()
            .ok()?;
        conn.flush().ok()?;

        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max.wrapping_sub(min).wrapping_add(1);
        let reply = conn.get_keyboard_mapping(min, count).ok()?.reply().ok()?;
        let per = reply.keysyms_per_keycode as usize;
        let mut keycodes = HashMap::new();
        for (i, sym) in reply.keysyms.iter().enumerate() {
            if i % per != 0 || *sym == 0 {
                continue;
            }
            if let Some(ch) = char::from_u32(*sym) {
                keycodes.entry(ch).or_insert(min + (i / per) as u8);
            }
        }
        // Control_L 的 keysym = 0xFFE3
        let mut ctrl_kc = None;
        for (i, sym) in reply.keysyms.iter().enumerate() {
            if *sym == 0xFFE3 && i % per == 0 {
                ctrl_kc = Some(min + (i / per) as u8);
                break;
            }
        }
        let ctrl = ctrl_kc?;

        Some(Self {
            conn,
            win,
            root,
            keycodes,
            ctrl,
        })
    }

    #[inline]
    fn fake(&self, type_: u8, detail: u8) -> Result<(), x11rb::errors::ConnectionError> {
        xtest::fake_input(
            &self.conn,
            type_,
            detail,
            x11rb::CURRENT_TIME,
            self.root,
            0,
            0,
            0,
        )?;
        Ok(())
    }

    fn keycode(&self, ch: char) -> Option<u8> {
        self.keycodes.get(&ch).copied()
    }

    /// 注入一个可见字符，返回注入前的时间戳。
    pub fn tap_char(&mut self, ch: char) -> Option<Instant> {
        let kc = self.keycode(ch)?;
        let t0 = Instant::now();
        self.fake(KEY_PRESS, kc).ok()?;
        self.fake(KEY_RELEASE, kc).ok()?;
        self.conn.flush().ok()?;
        // 往返一次，确保 release 真的被服务器处理；否则退出时可能把修饰键留在按下状态。
        let _ = self.conn.sync();
        Some(t0)
    }

    /// 注入 Ctrl-U（清行），用于下一次测量。
    pub fn ctrl_u(&mut self) -> bool {
        let Some(kc) = self.keycode('u') else {
            return false;
        };
        let _ = self.fake(KEY_PRESS, self.ctrl);
        let _ = self.fake(KEY_PRESS, kc);
        let _ = self.fake(KEY_RELEASE, kc);
        let _ = self.fake(KEY_RELEASE, self.ctrl);
        if self.conn.flush().is_err() {
            return false;
        }
        // 同上：避免 Ctrl 卡在 XTEST 键盘上。
        self.conn.sync().is_ok()
    }

    pub fn focus(&self) -> bool {
        match self
            .conn
            .set_input_focus(InputFocus::PARENT, self.win, x11rb::CURRENT_TIME)
        {
            Ok(cookie) => cookie.check().is_ok(),
            Err(_) => false,
        }
    }

    #[allow(dead_code)]
    pub fn root(&self) -> Window {
        self.root
    }
}

/// 分位数统计（微秒）。
#[derive(Debug, Clone, Copy)]
pub struct LatencyStats {
    pub n: usize,
    pub min_us: f64,
    pub mean_us: f64,
    pub median_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
}

impl LatencyStats {
    pub fn from_samples(samples: &[Duration]) -> Self {
        let mut v: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1e6).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        let pick = |q: f64| -> f64 {
            if n == 0 {
                return 0.0;
            }
            let idx = ((n as f64 - 1.0) * q).round() as usize;
            v[idx.min(n - 1)]
        };
        Self {
            n,
            min_us: v.first().copied().unwrap_or(0.0),
            mean_us: if n == 0 {
                0.0
            } else {
                v.iter().sum::<f64>() / n as f64
            },
            median_us: pick(0.5),
            p95_us: pick(0.95),
            p99_us: pick(0.99),
            max_us: v.last().copied().unwrap_or(0.0),
        }
    }

    pub fn to_json(&self, extra: &str) -> String {
        format!(
            "{{\"n\":{},\"min_us\":{:.1},\"mean_us\":{:.1},\"median_us\":{:.1},\"p95_us\":{:.1},\"p99_us\":{:.1},\"max_us\":{:.1}{}}}",
            self.n, self.min_us, self.mean_us, self.median_us, self.p95_us, self.p99_us, self.max_us, extra
        )
    }
}

// ---------------------------------------------------------------------------
// XDamage：把「像素真的进了 X server」当作上屏时间戳。
//
// 相比 set_rendering_notifier，它对任何 Slint 渲染后端都成立；
// 并且在软件光栅化路径下能覆盖「拷贝到窗口」这一步。
// ---------------------------------------------------------------------------

use x11rb::protocol::damage::{self, ConnectionExt as DamageExt, ReportLevel};

pub struct DamageWatcher {
    rx: crossbeam_channel::Receiver<Instant>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DamageWatcher {
    /// 在后台线程里阻塞监听窗口 damage，每个事件的时间戳送到有界通道。
    pub fn start(title: &str, timeout: Duration) -> Option<Self> {
        let (tx, rx) = crossbeam_channel::bounded::<Instant>(4096);
        let title = title.to_string();

        // 先同步找到窗口，保证返回时监听已建立
        let (conn, screen_num) = crate::xconn::connect().ok()?;
        let root = conn.setup().roots[screen_num].root;
        let deadline = Instant::now() + timeout;
        let win = loop {
            if let Some(w) = find_window(&conn, root, &title, 0) {
                break w;
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        conn.damage_query_version(1, 1).ok()?.reply().ok()?;
        let damage_id = conn.generate_id().ok()?;
        conn.damage_create(damage_id, win, ReportLevel::NON_EMPTY)
            .ok()?
            .check()
            .ok()?;
        conn.flush().ok()?;

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = stop.clone();
        let thread = std::thread::Builder::new()
            .name("xdamage".into())
            .spawn(move || {
                use std::sync::atomic::Ordering;
                loop {
                    if stop_thread.load(Ordering::Acquire) {
                        break;
                    }
                    match conn.poll_for_event() {
                        Ok(Some(event)) => match event {
                            x11rb::protocol::Event::DamageNotify(e) => {
                                let _ = conn.damage_subtract(damage_id, 0u32, 0u32);
                                let _ = conn.flush();
                                let now = Instant::now();
                                if tx.try_send(now).is_err() {
                                    // 通道满：丢掉最旧的语义由接收端兜底，这里直接跳过
                                }
                                let _ = e;
                            }
                            _ => {}
                        },
                        Ok(None) => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
                let _ = conn.damage_destroy(damage_id);
            })
            .ok()?;

        Some(Self {
            rx,
            stop,
            thread: Some(thread),
        })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<Instant> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// 清空积压（用于每次测量前对齐）。
    pub fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }

    #[allow(dead_code)]
    pub fn rx(&self) -> &crossbeam_channel::Receiver<Instant> {
        &self.rx
    }
}

impl Drop for DamageWatcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[allow(unused)]
fn _assert_damage_traits<C: damage::ConnectionExt>(_: &C) {}
