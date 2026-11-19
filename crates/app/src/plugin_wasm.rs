// SPDX-License-Identifier: GPL-3.0-only
//! Plugin v3 WASM hot-path runtime (`runtime = "wasm"`).
//!
//! 这是一个**窄 ABI、无导入**的终端输出过滤器宿主：
//!
//! ```text
//! PTY bytes
//!   └─ core pane actor（非 UI 线程）
//!        └─ WasmOutputHost::filter
//!             ├─ alloc(len) -> ptr        输入写到 ptr（ptr 必须 >= 256 KiB）
//!             ├─ filter(ptr, len) -> i32  输出写到 memory[0..i32]
//!             └─ 宿主读回有界输出
//!   └─ Sixel → IIP → Kitty → vte
//! ```
//!
//! 安全边界：
//! - 模块没有任何 import（无 WASI、文件、网络、进程、时钟、随机数）；
//! - 每个实例最多 16 MiB 线性内存，单个模块最多 8 MiB；
//! - 每次调用重置 fuel（16M）与 epoch deadline（约 20 ms）；
//! - 输入/输出各 256 KiB；`alloc` 指针、越界、陷阱、超时、输出超限都回退到
//!   原始 bytes，并把该插件标记为 faulted（后续调用直接 passthrough，直到
//!   Reload/Re-enable）；
//! - 任何错误都不会 panic/abort Tervo；调用发生在 pane actor，不阻塞 UI。
//!
//! 这不是“信任插件”模型：WASM 是内存/执行沙箱，但宿主仍然只加载用户显式
//! 启用并确认过 `terminal.output_filter = true` 的插件；没有市场/签名。
//!
//! 详见 `docs/PLUGIN_V3.md` 的 WASM 章节。

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tervo_core::{OutputFilter, MAX_OUTPUT_FILTER_BYTES};
use wasmtime::{
    Config, Engine, ExternType, Instance, Memory, Module, OptLevel, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc, ValType,
};

/// 单次过滤输入上限（与 core `MAX_OUTPUT_FILTER_BYTES` 相同）。
pub const MAX_INPUT_BYTES: usize = 256 * 1024;
/// 单次过滤输出上限。
pub const MAX_OUTPUT_BYTES: usize = MAX_OUTPUT_FILTER_BYTES;
/// 插件 `.wasm` 文件上限；避免把巨大模块读进 UI 线程。
pub const MAX_WASM_BYTES: usize = 8 * 1024 * 1024;
/// 单个 WASM 实例线性内存上限（256 pages × 64 KiB）。
pub const MAX_MEMORY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MEMORY_PAGES: u64 = (MAX_MEMORY_BYTES / 65_536) as u64;

/// ABI：输出固定写回线性内存 offset 0；输入指针必须 >= 该值，保证输出区与
/// 输入区不重叠（输出最多 256 KiB，输入从 256 KiB 开始）。
pub const OUTPUT_OFFSET: usize = 0;
pub const MIN_INPUT_OFFSET: usize = MAX_OUTPUT_BYTES;

/// 每次调用最多消耗的 WASM 指令燃料。
pub const FILTER_FUEL: u64 = 16_000_000;
/// epoch ticker 周期与每次调用允许的 tick 数（约 20 ms 墙钟）。
pub const EPOCH_TICK: Duration = Duration::from_millis(5);
pub const FILTER_EPOCH_TICKS: u64 = 4;
/// 错误文本上限，避免陷阱 backtrace 撑爆设置页。
pub const MAX_ERROR_CHARS: usize = 512;

const WASM_STACK_BYTES: usize = 512 * 1024;

/// 一个插件的热路径计数器。UI/soak 只读原子值，不与 actor 争锁。
#[derive(Debug, Default)]
pub struct WasmFilterStats {
    pub calls: AtomicU64,
    pub successes: AtomicU64,
    pub failures: AtomicU64,
    pub timeouts: AtomicU64,
    pub input_bytes: AtomicU64,
    pub output_bytes: AtomicU64,
    pub total_nanos: AtomicU64,
    pub max_nanos: AtomicU64,
}

impl WasmFilterStats {
    pub fn snapshot(&self) -> WasmFilterStatsSnapshot {
        WasmFilterStatsSnapshot {
            calls: self.calls.load(Ordering::Relaxed),
            successes: self.successes.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            input_bytes: self.input_bytes.load(Ordering::Relaxed),
            output_bytes: self.output_bytes.load(Ordering::Relaxed),
            total_nanos: self.total_nanos.load(Ordering::Relaxed),
            max_nanos: self.max_nanos.load(Ordering::Relaxed),
        }
    }

    fn record(&self, elapsed: Duration, input: usize, output: Option<usize>) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.input_bytes.fetch_add(input as u64, Ordering::Relaxed);
        let nanos = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        self.total_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
        match output {
            Some(len) => {
                self.successes.fetch_add(1, Ordering::Relaxed);
                self.output_bytes.fetch_add(len as u64, Ordering::Relaxed);
            }
            None => {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WasmFilterStatsSnapshot {
    pub calls: u64,
    pub successes: u64,
    pub failures: u64,
    pub timeouts: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub total_nanos: u64,
    pub max_nanos: u64,
}

impl WasmFilterStatsSnapshot {
    /// 平均单次过滤耗时（微秒）；无调用时为 0。
    pub fn average_micros(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.total_nanos as f64 / self.calls as f64 / 1_000.0
        }
    }
}

/// epoch 时钟：每 [`EPOCH_TICK`] 递增一次，让死循环在 fuel 之外还有墙钟兜底。
struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine, plugin_id: &str) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::Builder::new()
            .name(format!("wasm-epoch-{plugin_id}"))
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    thread::sleep(EPOCH_TICK);
                    engine.increment_epoch();
                }
            })
            .map_err(|e| format!("无法创建 WASM epoch 线程: {e}"))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct StoreState {
    limits: StoreLimits,
}

struct WasmInstance {
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    filter: TypedFunc<(i32, i32), i32>,
    store: Store<StoreState>,
}

/// 一个已编译、已实例化的插件过滤器。
///
/// 实例用 Mutex 串行化（多个 pane actor 可能并发）。锁竞争只发生在 actor
/// 线程，UI/渲染线程从不等待；单次持锁时间由 fuel/epoch 硬上限约束。
pub struct WasmFilterRuntime {
    id: String,
    state: Mutex<WasmInstance>,
    faulted: AtomicBool,
    fault_error: Mutex<Option<String>>,
    stats: WasmFilterStats,
    _ticker: EpochTicker,
}

impl fmt::Debug for WasmFilterRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmFilterRuntime")
            .field("id", &self.id)
            .field("faulted", &self.faulted.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl WasmFilterRuntime {
    /// 编译并实例化一个 `.wasm`。生产不解析 WAT；WAT 只在测试/构建示例时用
    /// `wat` 汇编成字节。
    pub fn load(plugin_id: &str, wasm: &[u8]) -> Result<Self, String> {
        if wasm.is_empty() {
            return Err("WASM 模块为空".to_string());
        }
        if wasm.len() > MAX_WASM_BYTES {
            return Err(format!(
                "WASM 模块过大（{} bytes，上限 {}）",
                wasm.len(),
                MAX_WASM_BYTES
            ));
        }
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.max_wasm_stack(WASM_STACK_BYTES);
        config.cranelift_opt_level(OptLevel::Speed);
        // 过滤器不需要线程/多内存/64-bit 内存；这些关闭后 ABI 面更窄。
        config.wasm_multi_memory(false);
        config.wasm_memory64(false);
        // 不启用 WASI/组件模型/cache；没有 import 就没有宿主能力。
        let engine = Engine::new(&config).map_err(|e| format!("创建 WASM engine 失败: {e}"))?;
        let module = Module::new(&engine, wasm)
            .map_err(|e| format!("WASM 编译失败: {}", clip_error(&format!("{e:#}"))))?;
        validate_module(&module)?;

        // epoch ticker 必须在实例化之前启动：恶意 start 函数也要被超时截断。
        let ticker = EpochTicker::start(engine.clone(), plugin_id)?;
        let limits = StoreLimitsBuilder::new()
            .memory_size(MAX_MEMORY_BYTES)
            .instances(1)
            .tables(1)
            .table_elements(1024)
            .memories(1)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(&engine, StoreState { limits });
        store.limiter(|state| &mut state.limits);
        reset_store_limits(&mut store)?;
        let instance = Instance::new(&mut store, &module, &[])
            .map_err(|e| format!("WASM 实例化失败: {}", clip_error(&format!("{e:#}"))))?;
        // 实例化后 start 函数可能已经消耗燃料；调用前会再次重置。
        reset_store_limits(&mut store)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| "WASM 缺少导出的 memory".to_string())?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| format!("导出 alloc 签名必须为 (i32)->i32: {e}"))?;
        let filter = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "filter")
            .map_err(|e| format!("导出 filter 签名必须为 (i32,i32)->i32: {e}"))?;
        Ok(Self {
            id: plugin_id.to_string(),
            state: Mutex::new(WasmInstance {
                memory,
                alloc,
                filter,
                store,
            }),
            faulted: AtomicBool::new(false),
            fault_error: Mutex::new(None),
            stats: WasmFilterStats::default(),
            _ticker: ticker,
        })
    }

    pub fn plugin_id(&self) -> &str {
        &self.id
    }

    pub fn is_faulted(&self) -> bool {
        self.faulted.load(Ordering::Acquire)
    }

    pub fn fault_error(&self) -> Option<String> {
        self.fault_error.lock().ok().and_then(|error| error.clone())
    }

    pub fn stats_snapshot(&self) -> WasmFilterStatsSnapshot {
        self.stats.snapshot()
    }

    fn fail(&self, reason: &str) {
        let reason = clip_error(reason);
        if !self.faulted.swap(true, Ordering::AcqRel) {
            if let Ok(mut error) = self.fault_error.lock() {
                *error = Some(reason.clone());
            }
            eprintln!(
                "[plugin-wasm] {}: filter 已熔断，后续输出原样放行: {reason}",
                self.id
            );
        }
    }

    fn is_timeout(error: &str) -> bool {
        let lower = error.to_ascii_lowercase();
        lower.contains("interrupt")
            || lower.contains("fuel")
            || lower.contains("epoch")
            || lower.contains("deadline")
    }
}

/// 对 WASM 线性内存执行一次输入复制 → filter → 有界输出读回。
///
/// 所有地址都由宿主重新校验，不信任模块返回值。
impl WasmInstance {
    fn call(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        if input.len() > MAX_INPUT_BYTES {
            return Err(format!(
                "输入 {} bytes 超过 {} 上限",
                input.len(),
                MAX_INPUT_BYTES
            ));
        }
        if input.is_empty() {
            return Ok(Vec::new());
        }
        reset_store_limits(&mut self.store)?;
        let len = i32::try_from(input.len()).map_err(|_| "输入长度无法放入 i32".to_string())?;
        let ptr = self
            .alloc
            .call(&mut self.store, len)
            .map_err(|e| format!("alloc 陷阱: {}", clip_error(&format!("{e:#}"))))?;
        if ptr < 0 {
            return Err(format!("alloc 返回非法负指针 {ptr}"));
        }
        let ptr_i32 = ptr;
        let ptr = ptr as usize;
        // 输出固定写 offset 0；输入必须在输出区之后，避免覆盖尚未读取的输入。
        if ptr < MIN_INPUT_OFFSET {
            return Err(format!(
                "alloc 指针 {ptr} 落在输出保留区（必须 >= {MIN_INPUT_OFFSET}）"
            ));
        }
        let end = ptr
            .checked_add(input.len())
            .ok_or_else(|| "alloc 指针 + 输入长度溢出".to_string())?;
        let size = self.memory.data_size(&self.store);
        if end > size {
            return Err(format!("alloc 指针越界: {end} > memory {size}"));
        }
        self.memory
            .write(&mut self.store, ptr, input)
            .map_err(|e| format!("写入 WASM memory 失败: {e}"))?;
        let out_len = self
            .filter
            .call(&mut self.store, (ptr_i32, len))
            .map_err(|e| format!("filter 陷阱: {}", clip_error(&format!("{e:#}"))))?;
        if out_len < 0 {
            return Err(format!("filter 返回非法负长度 {out_len}"));
        }
        let out_len = out_len as usize;
        if out_len > MAX_OUTPUT_BYTES {
            return Err(format!(
                "filter 输出 {out_len} bytes 超过 {MAX_OUTPUT_BYTES} 上限"
            ));
        }
        let data = self.memory.data(&self.store);
        let capacity = data.len().saturating_sub(OUTPUT_OFFSET);
        if out_len > capacity {
            return Err(format!(
                "filter 输出越界: {out_len} > memory 可用 {capacity}"
            ));
        }
        Ok(data[OUTPUT_OFFSET..OUTPUT_OFFSET + out_len].to_vec())
    }
}

impl OutputFilter for WasmFilterRuntime {
    fn filter(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        if self.is_faulted() {
            return None;
        }
        if bytes.len() > MAX_INPUT_BYTES {
            self.fail(&format!(
                "输入 {} bytes 超过 {} 上限",
                bytes.len(),
                MAX_INPUT_BYTES
            ));
            return None;
        }
        let start = Instant::now();
        let mut instance = match self.state.lock() {
            Ok(instance) => instance,
            Err(poisoned) => poisoned.into_inner(),
        };
        // 等锁期间另一个 pane 可能已经熔断；不要在故障实例上再跑一次。
        if self.is_faulted() {
            return None;
        }
        let result = instance.call(bytes);
        let elapsed = start.elapsed();
        match result {
            Ok(output) => {
                self.stats.record(elapsed, bytes.len(), Some(output.len()));
                Some(output)
            }
            Err(error) => {
                if Self::is_timeout(&error) {
                    self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
                }
                self.stats.record(elapsed, bytes.len(), None);
                self.fail(&error);
                None
            }
        }
    }
}

fn reset_store_limits(store: &mut Store<StoreState>) -> Result<(), String> {
    store
        .set_fuel(FILTER_FUEL)
        .map_err(|e| format!("设置 WASM fuel 失败: {e}"))?;
    store.set_epoch_deadline(FILTER_EPOCH_TICKS);
    Ok(())
}

fn validate_module(module: &Module) -> Result<(), String> {
    let mut imports = module.imports();
    if let Some(import) = imports.next() {
        return Err(format!(
            "WASM 插件不允许 import（发现 {}.{}）；没有 WASI/文件/网络/进程宿主",
            import.module(),
            import.name()
        ));
    }

    // `ExportType::ty()` 返回临时 `ExternType`；按名字取出并立即保存 owned 值。
    let mut memory_ty = None;
    let mut alloc_ty = None;
    let mut filter_ty = None;
    for export in module.exports() {
        match export.name() {
            "memory" => memory_ty = Some(export.ty()),
            "alloc" => alloc_ty = Some(export.ty()),
            "filter" => filter_ty = Some(export.ty()),
            _ => {}
        }
    }

    let ExternType::Memory(memory) =
        memory_ty.ok_or_else(|| "WASM 缺少导出的 memory".to_string())?
    else {
        return Err("导出 memory 不是线性内存".to_string());
    };
    if memory.is_64() {
        return Err("不支持 64-bit WASM memory".to_string());
    }
    if memory.is_shared() {
        return Err("不支持 shared WASM memory".to_string());
    }
    let page_size = memory.page_size();
    if memory.minimum().saturating_mul(page_size) > MAX_MEMORY_BYTES as u64 {
        return Err(format!(
            "WASM memory 初始大小超过 {} MiB",
            MAX_MEMORY_BYTES / (1024 * 1024)
        ));
    }
    match memory.maximum() {
        Some(max) if max.saturating_mul(page_size) <= MAX_MEMORY_BYTES as u64 => {}
        Some(max) => {
            return Err(format!(
                "WASM memory 上限 {} pages 超过 {} pages",
                max, MAX_MEMORY_PAGES
            ))
        }
        None => {
            return Err(format!(
                "WASM memory 必须声明最大 {} pages 的上限",
                MAX_MEMORY_PAGES
            ))
        }
    }

    let alloc = alloc_ty.ok_or_else(|| "WASM 缺少导出的 alloc".to_string())?;
    let ExternType::Func(alloc) = alloc else {
        return Err("导出 alloc 不是函数".to_string());
    };
    let alloc_params: Vec<_> = alloc.params().collect();
    let alloc_results: Vec<_> = alloc.results().collect();
    if !matches!(alloc_params.as_slice(), [ValType::I32])
        || !matches!(alloc_results.as_slice(), [ValType::I32])
    {
        return Err("alloc 签名必须是 (i32)->i32".to_string());
    }

    let filter = filter_ty.ok_or_else(|| "WASM 缺少导出的 filter".to_string())?;
    let ExternType::Func(filter) = filter else {
        return Err("导出 filter 不是函数".to_string());
    };
    let filter_params: Vec<_> = filter.params().collect();
    let filter_results: Vec<_> = filter.results().collect();
    if !matches!(filter_params.as_slice(), [ValType::I32, ValType::I32])
        || !matches!(filter_results.as_slice(), [ValType::I32])
    {
        return Err("filter 签名必须是 (i32,i32)->i32".to_string());
    }
    Ok(())
}

fn clip_error(error: &str) -> String {
    let mut out = String::with_capacity(error.len().min(MAX_ERROR_CHARS));
    for ch in error.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if out.chars().count() >= MAX_ERROR_CHARS {
            break;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// 读取 `.wasm` 模块：先 stat，超过上限直接拒绝；文件最多读 `max + 1` 字节，
/// 避免 Enable 时把任意大的文件整块读进 UI 线程。
pub(crate) fn read_wasm_module(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!("读取 WASM 元数据失败 {}: {error}", path.display())
    })?;
    if metadata.len() > max as u64 {
        return Err(format!(
            "WASM 模块过大（{} bytes，上限 {max}）: {}",
            metadata.len(),
            path.display()
        ));
    }
    let file = std::fs::File::open(path)
        .map_err(|error| format!("读取 WASM 失败 {}: {error}", path.display()))?;
    read_capped(file, max)
        .map_err(|error| format!("读取 WASM 失败 {}: {error}", path.display()))
}

/// 从 reader 最多读 `max + 1` 字节；多出的 1 字节只用于判断是否超限。
fn read_capped<R: Read>(reader: R, max: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity((max + 1).min(64 * 1024));
    reader
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > max {
        return Err(format!("WASM 模块过大（超过 {max} bytes 上限）"));
    }
    Ok(bytes)
}

/// 已启用 WASM 输出过滤器的有序链（按插件 id 排序，保证确定性）。
///
/// `Arc<WasmOutputHost>` 在创建 pane 时交给 core；Enable/Disable/Reload 只替换
/// 内部 runtime，不重建 pane，也不重启 Tervo。
#[derive(Default)]
pub struct WasmOutputHost {
    filters: RwLock<BTreeMap<String, Arc<WasmFilterRuntime>>>,
    /// 已激活过滤器数量。0 时热路径只做一次 relaxed load，不拿锁。
    active: AtomicUsize,
}

impl fmt::Debug for WasmOutputHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmOutputHost")
            .field("active", &self.active_ids())
            .finish()
    }
}

impl WasmOutputHost {
    pub fn activate(&self, runtime: Arc<WasmFilterRuntime>) {
        let id = runtime.plugin_id().to_string();
        if let Ok(mut filters) = self.filters.write() {
            if filters.insert(id, runtime).is_none() {
                self.active.fetch_add(1, Ordering::Release);
            }
        }
    }

    pub fn remove(&self, id: &str) -> Option<Arc<WasmFilterRuntime>> {
        let mut filters = match self.filters.write() {
            Ok(filters) => filters,
            Err(poisoned) => poisoned.into_inner(),
        };
        let removed = filters.remove(id);
        if removed.is_some() {
            self.active.fetch_sub(1, Ordering::Release);
        }
        removed
    }

    pub fn clear(&self) {
        if let Ok(mut filters) = self.filters.write() {
            filters.clear();
            self.active.store(0, Ordering::Release);
        }
    }

    pub fn active_ids(&self) -> Vec<String> {
        self.filters
            .read()
            .map(|filters| filters.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<Arc<WasmFilterRuntime>> {
        self.filters
            .read()
            .ok()
            .and_then(|filters| filters.get(id).cloned())
    }

    pub fn stats(&self, id: &str) -> Option<WasmFilterStatsSnapshot> {
        self.get(id).map(|runtime| runtime.stats_snapshot())
    }
}

impl OutputFilter for WasmOutputHost {
    fn filter(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.is_empty() || self.active.load(Ordering::Acquire) == 0 {
            return None;
        }
        let filters = match self.filters.read() {
            Ok(filters) => filters,
            Err(poisoned) => poisoned.into_inner(),
        };
        if filters.is_empty() {
            return None;
        }
        let mut current: Option<Vec<u8>> = None;
        let mut applied = false;
        for runtime in filters.values() {
            if runtime.is_faulted() {
                continue;
            }
            let input = current.as_deref().unwrap_or(bytes);
            if let Some(output) = runtime.filter(input) {
                if output.len() > MAX_OUTPUT_BYTES {
                    // runtime 已经拒绝；这里防御性兜底，保持整个链失败即原样。
                    return None;
                }
                current = Some(output);
                applied = true;
            }
        }
        if !applied {
            return None;
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPPERCASE_WAT: &str = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param $len i32) (result i32)
    (local $need i32)
    (local.set $need
      (i32.div_u
        (i32.add (i32.add (local.get $len) (i32.const 262144)) (i32.const 65535))
        (i32.const 65536)))
    (if (i32.gt_u (local.get $need) (memory.size))
      (then (drop (memory.grow (i32.sub (local.get $need) (memory.size))))))
    (i32.const 262144))
  (func (export "filter") (param $ptr i32) (param $len i32) (result i32)
    (local $i i32)
    (block $done
      (loop $copy
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (i32.store8
          (local.get $i)
          (i32.xor
            (i32.load8_u (i32.add (local.get $ptr) (local.get $i)))
            (i32.const 32)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy)))
    (local.get $len))
)
"#;

    fn load_runtime(wat: &str) -> Result<WasmFilterRuntime, String> {
        let wasm = wat::parse_str(wat).expect("test WAT parses");
        WasmFilterRuntime::load("test.plugin", &wasm)
    }

    fn wat_filter(wat: &str, input: &[u8]) -> Option<Vec<u8>> {
        load_runtime(wat).expect("runtime loads").filter(input)
    }

    #[test]
    fn abi_uppercases_and_uses_output_offset_zero() {
        let out = wat_filter(UPPERCASE_WAT, b"hellowasm").expect("filter succeeds");
        assert_eq!(out, b"HELLOWASM");
        assert_eq!(OUTPUT_OFFSET, 0);
        assert!(MIN_INPUT_OFFSET >= MAX_OUTPUT_BYTES);
    }

    #[test]
    fn empty_output_is_a_valid_filter_result() {
        let wat = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const 0)))
"#;
        let runtime = load_runtime(wat).expect("loads");
        assert_eq!(runtime.filter(b"drop me"), Some(Vec::new()));
        assert!(!runtime.is_faulted());
    }

    #[test]
    fn trap_falls_back_and_circuit_breaks() {
        let wat = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) unreachable))
"#;
        let runtime = load_runtime(wat).expect("loads");
        assert_eq!(runtime.filter(b"original bytes"), None);
        assert!(runtime.is_faulted());
        let error = runtime.fault_error().expect("error recorded");
        assert!(
            error.contains("unreachable") || error.contains("陷阱"),
            "{error}"
        );
        // 熔断后不再进入实例。
        assert_eq!(runtime.filter(b"original bytes"), None);
        assert_eq!(runtime.stats_snapshot().calls, 1);
        assert_eq!(runtime.stats_snapshot().failures, 1);
    }

    #[test]
    fn infinite_loop_is_stopped_by_fuel_or_epoch() {
        let wat = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32)
    (loop $forever (br $forever))
    (i32.const 0)))
"#;
        let runtime = load_runtime(wat).expect("loads");
        let start = Instant::now();
        assert_eq!(runtime.filter(b"never returns"), None);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(1), "超时未生效: {elapsed:?}");
        assert!(runtime.is_faulted());
        let err = runtime.fault_error().unwrap_or_default();
        assert!(
            WasmFilterRuntime::is_timeout(&err),
            "应识别为 fuel/epoch 超时: {err}"
        );
        assert!(runtime.stats_snapshot().timeouts >= 1);
    }

    #[test]
    fn negative_output_length_faults() {
        let wat = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const -1)))
"#;
        let runtime = load_runtime(wat).expect("loads");
        assert_eq!(runtime.filter(b"abc"), None);
        assert!(runtime.fault_error().unwrap_or_default().contains("负长度"));
    }

    #[test]
    fn oversize_output_faults() {
        let wat = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const 300000)))
"#;
        let runtime = load_runtime(wat).expect("loads");
        assert_eq!(runtime.filter(b"abc"), None);
        assert!(runtime.fault_error().unwrap_or_default().contains("超过"));
    }

    #[test]
    fn low_or_out_of_bounds_alloc_pointer_faults() {
        let low = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const 1)))
"#;
        let runtime = load_runtime(low).expect("loads");
        assert_eq!(runtime.filter(b"abc"), None);
        assert!(runtime
            .fault_error()
            .unwrap_or_default()
            .contains("输出保留区"));

        let oob = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 536870000))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const 1)))
"#;
        let runtime = load_runtime(oob).expect("loads");
        assert_eq!(runtime.filter(b"abc"), None);
        assert!(runtime.fault_error().unwrap_or_default().contains("越界"));
    }

    #[test]
    fn memory_growth_beyond_limit_faults() {
        let wat = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32)
    (drop (memory.grow (i32.const 4096)))
    (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const 1)))
"#;
        let runtime = load_runtime(wat).expect("loads");
        assert_eq!(runtime.filter(b"abc"), None);
        assert!(runtime.is_faulted());
        let err = runtime.fault_error().unwrap_or_default();
        assert!(
            err.contains("memory") || err.contains("grow") || err.contains("陷阱"),
            "{err}"
        );
    }

    #[test]
    fn imports_are_rejected() {
        let wat = r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const 0)))
"#;
        let err = load_runtime(wat).unwrap_err();
        assert!(err.contains("不允许 import"), "{err}");
    }

    #[test]
    fn missing_exports_and_wrong_signatures_are_rejected() {
        let missing = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144)))
"#;
        assert!(load_runtime(missing).unwrap_err().contains("filter"));

        let wrong = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i64) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const 0)))
"#;
        assert!(load_runtime(wrong).unwrap_err().contains("alloc 签名"));

        let no_max = r#"
(module
  (memory (export "memory") 8)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) (i32.const 0)))
"#;
        assert!(load_runtime(no_max).unwrap_err().contains("必须声明最大"));
    }

    #[test]
    fn host_chain_runs_in_plugin_id_order_and_skips_faulted() {
        let host = WasmOutputHost::default();
        let upper = Arc::new(load_runtime(UPPERCASE_WAT).expect("loads"));
        host.activate(upper);
        assert_eq!(host.filter(b"abc"), Some(b"ABC".to_vec()));

        // 第二个过滤器 trap：链保留第一个过滤器的输出，而不是丢字节。
        let trap = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) unreachable))
"#;
        let mut trapped = load_runtime(trap).expect("loads");
        trapped.id = "test.zzz".to_string();
        host.activate(Arc::new(trapped));
        assert_eq!(host.active_ids(), vec!["test.plugin", "test.zzz"]);
        assert_eq!(host.filter(b"abc"), Some(b"ABC".to_vec()));
        assert!(host.get("test.zzz").expect("runtime").is_faulted());

        // 链中所有过滤器都不可用时返回 None，core 原样放行。
        host.remove("test.plugin");
        assert_eq!(host.filter(b"abc"), None);
    }

    #[test]
    fn input_over_limit_faults_and_passthroughs() {
        let runtime = load_runtime(UPPERCASE_WAT).expect("loads");
        let too_big = vec![b'a'; MAX_INPUT_BYTES + 1];
        assert_eq!(runtime.filter(&too_big), None);
        assert!(runtime.is_faulted());
        let err = runtime.fault_error().unwrap_or_default();
        assert!(err.contains("超过"), "{err}");
    }

    #[test]
    fn exact_max_input_is_accepted() {
        let runtime = load_runtime(UPPERCASE_WAT).expect("loads");
        let input = vec![b'a'; MAX_INPUT_BYTES];
        let output = runtime.filter(&input).expect("exact cap accepted");
        assert_eq!(output.len(), MAX_INPUT_BYTES);
        assert!(output.iter().all(|byte| *byte == b'A'));
        assert!(!runtime.is_faulted());
    }

    #[test]
    fn module_read_is_capped_at_max_plus_one_byte() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct CountingReader<R> {
            inner: R,
            count: Rc<Cell<usize>>,
        }
        impl<R: Read> Read for CountingReader<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let read = self.inner.read(buf)?;
                self.count.set(self.count.get() + read);
                Ok(read)
            }
        }

        let count = Rc::new(Cell::new(0));
        let reader = CountingReader {
            inner: std::io::repeat(b'x').take(64 * 1024 * 1024),
            count: count.clone(),
        };
        let error = read_capped(reader, MAX_WASM_BYTES).expect_err("超限模块必须被拒绝");
        assert!(error.contains("过大"), "{error}");
        assert_eq!(
            count.get(),
            MAX_WASM_BYTES + 1,
            "读取必须封顶 MAX+1 字节，不能把超限文件整块读完"
        );
    }

    #[test]
    fn module_size_over_limit_is_rejected_before_compile() {
        let oversized = vec![0u8; MAX_WASM_BYTES + 1];
        let err = WasmFilterRuntime::load("test.too-big", &oversized).unwrap_err();
        assert!(err.contains("过大"), "{err}");
    }

    #[test]
    fn filter_latency_is_bounded_for_terminal_sized_chunks() {
        let wasm = wat::parse_str(UPPERCASE_WAT).unwrap();
        let runtime = WasmFilterRuntime::load("test.latency", &wasm).expect("loads");
        let input = vec![b'a'; 32 * 1024];
        let calls = 500;
        let start = Instant::now();
        for _ in 0..calls {
            assert_eq!(runtime.filter(&input).expect("filter").len(), input.len());
        }
        let elapsed = start.elapsed();
        let stats = runtime.stats_snapshot();
        assert_eq!(stats.calls, calls);
        assert_eq!(stats.input_bytes, (input.len() * calls as usize) as u64);
        assert!(stats.max_nanos > 0);
        eprintln!(
            "WASM filter 32 KiB: avg={:.1}us max={:.1}us total={elapsed:?}",
            stats.average_micros(),
            stats.max_nanos as f64 / 1_000.0
        );
        // 32 KiB 走一遍内存复制即使在 debug 下也应远低于 50ms；这里只做冒烟
        // 上限，真正 soak 性能证据在 report 中记录。
        assert!(stats.max_nanos < 50_000_000, "单次过滤延迟过高: {stats:?}");
    }

    #[test]
    fn docs_example_wat_transforms_the_documented_pattern() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/examples/plugin-wasm-filter/filter.wat");
        let text = std::fs::read_to_string(&path).expect("example WAT exists");
        let wasm = wat::parse_str(&text).expect("example WAT parses");
        let runtime = WasmFilterRuntime::load("dev.tervo.wasm-filter", &wasm).expect("loads");
        assert_eq!(
            runtime.filter(b"hello [tervo-wasm] world\n"),
            Some(b"hello WASM-FILTER-OK world\n".to_vec())
        );
        assert_eq!(
            runtime.filter(b"no marker\n"),
            Some(b"no marker\n".to_vec())
        );
        assert!(!runtime.is_faulted());
    }

    #[test]
    fn committed_example_wasm_matches_the_wat() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/examples/plugin-wasm-filter");
        let wasm = std::fs::read(dir.join("filter.wasm")).expect("committed filter.wasm exists");
        let runtime = WasmFilterRuntime::load("dev.tervo.wasm-filter", &wasm).expect("loads");
        assert_eq!(
            runtime.filter(b"[tervo-wasm]"),
            Some(b"WASM-FILTER-OK".to_vec())
        );
        // 相同 WAT 汇编结果只要求行为一致，不要求字节完全一致（工具版本不同）。
        let wat_text = std::fs::read_to_string(dir.join("filter.wat")).unwrap();
        let from_wat = wat::parse_str(&wat_text).unwrap();
        let wat_runtime = WasmFilterRuntime::load("dev.tervo.wasm-filter", &from_wat).unwrap();
        assert_eq!(
            wat_runtime.filter(b"[tervo-wasm]"),
            Some(b"WASM-FILTER-OK".to_vec())
        );
    }

    #[test]
    fn module_has_no_imports() {
        let wasm = wat::parse_str(UPPERCASE_WAT).unwrap();
        let module = Module::new(&Engine::default(), &wasm).unwrap();
        assert_eq!(module.imports().count(), 0);
    }
}
