// SPDX-License-Identifier: GPL-3.0-only
//! 临时分配诊断探针（不发布）：按调用栈采样 malloc/free，统计 net live 增长。
//!
//! 只有显式设置 `TERVO_MEMPROBE=1` 才启用；默认路径只多一次 relaxed atomic
//! load，不记录任何数据。用法：
//!
//! ```bash
//! TERVO_MEMPROBE=1 TERVO_MEMPROBE_RATE=64 TERVO_MEMPROBE_EVERY=20 \
//!   ./target/release/tervo ...
//! ```
//!
//! - `TERVO_MEMPROBE_RATE`：大约每 N 次分配采样一次（默认 64；>=16KiB 的大块
//!   分配每次采样）。
//! - `TERVO_MEMPROBE_EVERY`：dump 间隔秒数（默认 30）。
//! - `TERVO_MEMPROBE_TOP`：每次 dump 的栈数量（默认 25）。
//!
//! 采样到的分配会记录返回地址；free 时按指针标记归属，所以每个栈的
//! `live_bytes` 是真实仍在使用的字节数（采样估计），`delta_alloc/free`
//! 是两次 dump 之间的流量。栈符号化用 glibc `backtrace_symbols`，输出
//! `binary(+0x...)` 偏移，退出后用 addr2line 还原行号。

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// 调用栈深度（返回地址个数）。
const DEPTH: usize = 24;
type StackKey = [usize; DEPTH];

#[derive(Clone)]
struct StackStats {
    live_bytes: u64,
    live_count: u64,
    alloc_bytes: u64,
    alloc_count: u64,
    free_bytes: u64,
    free_count: u64,
    /// 距上次 dump 的流量，dump 后清零。
    delta_alloc_bytes: u64,
    delta_alloc_count: u64,
    delta_free_bytes: u64,
    delta_free_count: u64,
    first_ms: u64,
    last_ms: u64,
}

impl Default for StackStats {
    fn default() -> Self {
        Self {
            live_bytes: 0,
            live_count: 0,
            alloc_bytes: 0,
            alloc_count: 0,
            free_bytes: 0,
            free_count: 0,
            delta_alloc_bytes: 0,
            delta_alloc_count: 0,
            delta_free_bytes: 0,
            delta_free_count: 0,
            first_ms: 0,
            last_ms: 0,
        }
    }
}

struct PtrSample {
    key: StackKey,
    size: u32,
}

struct ProbeState {
    ptrs: HashMap<usize, PtrSample>,
    stacks: HashMap<StackKey, StackStats>,
    last_dump: Instant,
    total_sampled_allocs: u64,
    total_sampled_frees: u64,
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static RATE: AtomicUsize = AtomicUsize::new(64);
static TOP: AtomicUsize = AtomicUsize::new(25);
static ALLOC_SEQ: AtomicUsize = AtomicUsize::new(0);
static SYMBOLS: AtomicBool = AtomicBool::new(false);
static ALL: AtomicBool = AtomicBool::new(false);
static BASE: AtomicUsize = AtomicUsize::new(0);
static START: Mutex<Option<Instant>> = Mutex::new(None);
static STATE: Mutex<Option<ProbeState>> = Mutex::new(None);

fn now_ms() -> u64 {
    let start = START.lock().unwrap_or_else(|e| e.into_inner());
    start.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0)
}

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// 释放一条采样记录的统计更新。`dealloc` 与 `alloc` 共用。
fn note_free(st: &mut ProbeState, sample: PtrSample, ts: u64) {
    st.total_sampled_frees += 1;
    if let Some(s) = st.stacks.get_mut(&sample.key) {
        s.live_bytes = s.live_bytes.saturating_sub(sample.size as u64);
        s.live_count = s.live_count.saturating_sub(1);
        s.free_bytes = s.free_bytes.saturating_add(sample.size as u64);
        s.free_count += 1;
        s.delta_free_bytes = s.delta_free_bytes.saturating_add(sample.size as u64);
        s.delta_free_count += 1;
        s.last_ms = ts;
    }
}

fn on_alloc(ptr: usize, size: usize) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let seq = ALLOC_SEQ.fetch_add(1, Ordering::Relaxed);
    // 大块分配全采样；小块按 1/rate 采样。
    let rate = RATE.load(Ordering::Relaxed).max(1);
    if size < 16 * 1024 && seq % rate != 0 {
        return;
    }
    // 采样 hook 自身（backtrace/HashMap）的分配不递归采样。
    if IN_HOOK.try_with(|c| c.get()).unwrap_or(true) {
        return;
    }
    let _ = IN_HOOK.try_with(|c| c.set(true));

    let mut frames = [0usize; DEPTH];
    let n = unsafe {
        libc::backtrace(frames.as_mut_ptr() as *mut *mut c_void, DEPTH as i32)
    };
    if n > 0 {
        let ts = now_ms();
        let mut guard = match STATE.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if let Some(st) = guard.as_mut() {
            st.total_sampled_allocs += 1;
            // 去掉 backtrace 内部帧，保留从 on_alloc 调用者开始的栈。
            let mut key: StackKey = [0; DEPTH];
            for (i, addr) in frames.iter().take(DEPTH).enumerate() {
                key[i] = *addr;
            }
            let entry = st.stacks.entry(key).or_insert_with(|| StackStats {
                first_ms: ts,
                ..StackStats::default()
            });
            entry.live_bytes = entry.live_bytes.saturating_add(size as u64);
            entry.live_count += 1;
            entry.alloc_bytes = entry.alloc_bytes.saturating_add(size as u64);
            entry.alloc_count += 1;
            entry.delta_alloc_bytes = entry.delta_alloc_bytes.saturating_add(size as u64);
            entry.delta_alloc_count += 1;
            entry.last_ms = ts;
            st.ptrs.insert(ptr, PtrSample { key, size: size.min(u32::MAX as usize) as u32 });
        }
    }
    let _ = IN_HOOK.try_with(|c| c.set(false));
}

fn on_dealloc(ptr: usize) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if IN_HOOK.try_with(|c| c.get()).unwrap_or(true) {
        return;
    }
    // 先做一次廉价探测，避免每次 free 都抢锁（绝大多数指针未采样）。
    // 这里仍然需要锁，但采样率低时 free 命中率也低；用 try_lock 快速跳过。
    let mut guard = match STATE.try_lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let Some(st) = guard.as_mut() else { return };
    if let Some(sample) = st.ptrs.remove(&ptr) {
        let ts = now_ms();
        note_free(st, sample, ts);
    }
}

pub struct SamplingAllocator;

unsafe impl GlobalAlloc for SamplingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            on_alloc(ptr as usize, layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc_zeroed(layout);
        if !ptr.is_null() {
            on_alloc(ptr as usize, layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        on_dealloc(ptr as usize);
        System.dealloc(ptr, layout);
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// 在 `main` 最前面调用：读取环境变量；未设置时保持关闭。
pub fn init() {
    if std::env::var_os("TERVO_MEMPROBE").is_none() {
        return;
    }
    RATE.store(env_usize("TERVO_MEMPROBE_RATE", 64).max(1), Ordering::Relaxed);
    TOP.store(env_usize("TERVO_MEMPROBE_TOP", 25).max(1), Ordering::Relaxed);
    SYMBOLS.store(std::env::var_os("TERVO_MEMPROBE_SYMBOLS").is_some(), Ordering::Relaxed);
    ALL.store(std::env::var_os("TERVO_MEMPROBE_ALL").is_some(), Ordering::Relaxed);
    // PIE 主程序加载基址：离线 addr2line/nm 需要 `pc - base`。
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(init as *const libc::c_void, &mut info) != 0 {
            BASE.store(info.dli_fbase as usize, Ordering::Relaxed);
        }
    }
    *START.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(ProbeState {
        ptrs: HashMap::new(),
        stacks: HashMap::new(),
        last_dump: Instant::now(),
        total_sampled_allocs: 0,
        total_sampled_frees: 0,
    });
    ENABLED.store(true, Ordering::Relaxed);
    eprintln!(
        "[memprobe] enabled rate=1/{} top={} every={}s",
        RATE.load(Ordering::Relaxed),
        TOP.load(Ordering::Relaxed),
        env_usize("TERVO_MEMPROBE_EVERY", 30).max(1)
    );
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn dump_interval_secs() -> u64 {
    env_usize("TERVO_MEMPROBE_EVERY", 30).max(1) as u64
}

fn proc_status_kb(field: &str) -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else { return 0 };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .parse::<u64>()
                .unwrap_or(0);
        }
    }
    0
}

/// 读取 /proc/self/statm 的 RSS（页）。
fn rss_kb() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/statm") else { return 0 };
    let pages = text
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    pages * 4
}

#[repr(C)]
struct MallInfo2 {
    arena: usize,
    ordblks: usize,
    smblks: usize,
    hblks: usize,
    hblkhd: usize,
    usmblks: usize,
    fsmblks: usize,
    uordblks: usize,
    fordblks: usize,
    keepcost: usize,
}

extern "C" {
    fn mallinfo2() -> MallInfo2;
}

fn symbolize(frames: &StackKey) -> Vec<String> {
    unsafe {
        let ptrs: Vec<*mut c_void> = frames.iter().map(|&a| a as *mut c_void).collect();
        let syms = libc::backtrace_symbols(ptrs.as_ptr(), ptrs.len() as i32);
        if syms.is_null() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(ptrs.len());
        for i in 0..ptrs.len() {
            let p = *syms.add(i);
            if p.is_null() || frames[i] == 0 {
                continue;
            }
            let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
            out.push(s);
        }
        libc::free(syms as *mut c_void);
        out
    }
}

/// 打印当前 live 最大的调用栈。UI 线程 timer 调用。
pub fn dump() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // dump 自身会分配（克隆统计、符号化、eprintln）。必须屏蔽采样，
    // 否则持锁期间的分配会重入 on_alloc 的 STATE.lock 而死锁。
    if IN_HOOK.try_with(|c| c.get()).unwrap_or(true) {
        return;
    }
    let _ = IN_HOOK.try_with(|c| c.set(true));
    dump_inner();
    let _ = IN_HOOK.try_with(|c| c.set(false));
}

fn dump_inner() {
    let ts = now_ms();
    let (by_bytes, by_delta_bytes, by_delta_objs, totals) = {
        let mut guard = match STATE.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let Some(st) = guard.as_mut() else { return };
        let rows: Vec<(StackKey, StackStats)> =
            st.stacks.iter().map(|(k, v)| (*k, v.clone())).collect();
        let mut by_bytes = rows.clone();
        by_bytes.sort_by_key(|(_, v)| std::cmp::Reverse(v.live_bytes));
        let mut by_delta_bytes = rows;
        by_delta_bytes.sort_by_key(|(_, v)| {
            std::cmp::Reverse(v.delta_alloc_bytes.saturating_sub(v.delta_free_bytes))
        });
        let mut by_delta_objs = by_delta_bytes.clone();
        by_delta_objs.sort_by_key(|(_, v)| {
            std::cmp::Reverse(
                v.delta_alloc_count
                    .saturating_sub(v.delta_free_count),
            )
        });
        let total_live: u64 = st.stacks.values().map(|s| s.live_bytes).sum();
        let total_count: u64 = st.stacks.values().map(|s| s.live_count).sum();
        let delta_live: i64 = st
            .stacks
            .values()
            .map(|s| s.delta_alloc_bytes as i64 - s.delta_free_bytes as i64)
            .sum();
        let delta_objs: i64 = st
            .stacks
            .values()
            .map(|s| s.delta_alloc_count as i64 - s.delta_free_count as i64)
            .sum();
        let sampled_allocs = st.total_sampled_allocs;
        let sampled_frees = st.total_sampled_frees;
        let stack_count = st.stacks.len();
        st.last_dump = Instant::now();
        // 清零区间流量，下一轮 dump 只看增量。
        for s in st.stacks.values_mut() {
            s.delta_alloc_bytes = 0;
            s.delta_alloc_count = 0;
            s.delta_free_bytes = 0;
            s.delta_free_count = 0;
        }
        (
            by_bytes,
            by_delta_bytes,
            by_delta_objs,
            (
                total_live,
                total_count,
                delta_live,
                delta_objs,
                sampled_allocs,
                sampled_frees,
                stack_count,
            ),
        )
    };
    let (total_live, total_count, delta_live, delta_objs, sampled_allocs, sampled_frees, stacks) =
        totals;
    let mi = unsafe { mallinfo2() };
    eprintln!(
        "[memprobe] t={}ms base=0x{:x} rss={}KiB heap_inuse={}KiB heap_arena={}KiB live_sample={}KiB \
         objs={} delta_live={}KiB delta_objs={} sampled={} freed={} rate=1/{} stacks={}",
        ts,
        BASE.load(Ordering::Relaxed),
        rss_kb(),
        mi.uordblks / 1024,
        mi.arena / 1024,
        total_live / 1024,
        total_count,
        delta_live / 1024,
        delta_objs,
        sampled_allocs,
        sampled_frees,
        RATE.load(Ordering::Relaxed),
        stacks,
    );
    let top = TOP.load(Ordering::Relaxed);
    let header = |tag: &str, rank: usize, stats: &StackStats, key: &StackKey| {
        eprintln!(
            "[memprobe] {tag} #{rank} live={}KiB objs={} d_alloc={}KiB/{} d_free={}KiB/{} \
             alloc={}KiB/{} free={}KiB/{} first={}ms last={}ms",
            stats.live_bytes / 1024,
            stats.live_count,
            stats.delta_alloc_bytes / 1024,
            stats.delta_alloc_count,
            stats.delta_free_bytes / 1024,
            stats.delta_free_count,
            stats.alloc_bytes / 1024,
            stats.alloc_count,
            stats.free_bytes / 1024,
            stats.free_count,
            stats.first_ms,
            stats.last_ms,
        );
        let raw: Vec<String> = key
            .iter()
            .take_while(|a| **a != 0)
            .map(|a| format!("{a:x}"))
            .collect();
        eprintln!("[memprobe-raw] {tag} #{rank} {}", raw.join(","));
        if !SYMBOLS.load(Ordering::Relaxed) {
            return;
        }
    };
    for (rank, (key, stats)) in by_bytes.iter().take(top).enumerate() {
        header("bytes", rank, stats, key);
        for line in symbolize(key) {
            eprintln!("    {line}");
        }
    }
    for (rank, (key, stats)) in by_delta_bytes.iter().take(top).enumerate() {
        if stats.delta_alloc_bytes == 0 && stats.delta_free_bytes == 0 {
            break;
        }
        header("dbytes", rank, stats, key);
        for line in symbolize(key) {
            eprintln!("    {line}");
        }
    }
    for (rank, (key, stats)) in by_delta_objs.iter().take(top).enumerate() {
        if stats.delta_alloc_count == 0 && stats.delta_free_count == 0 {
            break;
        }
        header("dobjs", rank, stats, key);
        for line in symbolize(key) {
            eprintln!("    {line}");
        }
    }
}

