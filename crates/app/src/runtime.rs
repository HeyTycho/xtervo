// SPDX-License-Identifier: GPL-3.0-only
//! 单进程多窗口的运行时边界。
//!
//! 本模块只负责**所有权与路由**，不实现终端/UI：
//!
//! ```text
//! WindowRegistry
//!   WindowId(1) -> WindowRuntime { MainWindow, Rc<RefCell<AppState>> }
//!   WindowId(2) -> WindowRuntime { ... }
//!   SharedServices { Config / Theme / PluginHost / MCP / Session / Vault }
//! ```
//!
//! 线程模型：`WindowRegistry`、`WindowRuntime`、`SharedServices` 都只在 Slint UI
//! 线程创建和访问。PTY、插件 supervisor、MCP socket 线程不直接访问这里；它们
//! 通过既有 channel / `invoke_from_event_loop` 回到 UI 线程。
//!
//! `window_id` 是**显式**的：新代码应使用 [`with_window`]，旧代码的
//! [`crate::with_app`] 只在回调作用域内回退到“当前窗口”，不再假设进程里只有
//! 一个全局 `AppState`。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::sync::Arc;

use crate::config::Config;
use crate::config_watch::ConfigWatcher;
use crate::mcp::McpRuntime;
use crate::plugin_v3::PluginStore;
use crate::profile::ProfileStore;
use crate::session;
use crate::session::{SnapshotV2, WindowGeometry, WindowSnapshot};
use crate::themes::ThemeRegistry;
use crate::vault::Vault;
use slint::ComponentHandle;

use crate::AppState;

/// 进程内窗口身份。primary 从 1 开始单调递增，关闭后不复用。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(u64);

impl WindowId {
    /// WindowRegistry 的首个窗口。
    pub const FIRST: Self = Self(1);

    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "window-{}", self.0)
    }
}

/// 单调递增的 WindowId 分配器；不回收 id，避免旧回调/插件消息误命中新窗口。
#[derive(Debug, Default)]
pub struct WindowIdAllocator {
    next: u64,
}

impl WindowIdAllocator {
    pub fn new() -> Self {
        Self { next: 1 }
    }

    pub fn take(&mut self) -> WindowId {
        let id = WindowId(self.next.max(1));
        self.next = id.0.saturating_add(1);
        id
    }

    /// 测试/恢复路径：保证后续 `take` 不会低于已知 id。
    pub fn observe(&mut self, id: WindowId) {
        self.next = self.next.max(id.0.saturating_add(1));
    }
}

/// UI 线程内的共享服务句柄。
///
/// 使用 `Rc<RefCell<T>>` 保证同一份状态；`Deref/DerefMut` 让既有 `st.cfg.*`
/// 形式的调用点不需要改成 `borrow()` 链。安全约束：`Shared<T>` 只能在 Slint
/// UI 线程访问，且不允许在 `&mut T` 的生命周期里再取同一 `T` 的引用；Rust
/// 借用检查器在 `st.field` 层面仍然生效，`RefCell` 只用于持有所有权。
///
/// 这不是 `Send/Sync` 类型：后台线程必须走 channel / UI 回调。
pub struct Shared<T> {
    inner: Rc<RefCell<T>>,
}

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: Rc::new(RefCell::new(value)),
        }
    }

    pub fn clone_handle(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
        }
    }

    pub fn replace(&self, value: T) -> T {
        std::mem::replace(&mut *self.inner.borrow_mut(), value)
    }

    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.inner.borrow())
    }

    pub fn with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.inner.borrow_mut())
    }

    /// 诊断用地址，不暴露内容。
    pub fn identity(&self) -> usize {
        Rc::as_ptr(&self.inner) as usize
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        self.clone_handle()
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: 见类型文档。Shared 只在 UI 线程使用；返回的引用生命周期受
        // `&self` 约束，调用方无法在 `&mut` 借用期间同时持有它。
        unsafe { &*self.inner.as_ptr() }
    }
}

impl<T> DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 同上；`&mut self` 保证调用点对句柄本身是独占的。
        unsafe { &mut *self.inner.as_ptr() }
    }
}

impl<T: fmt::Debug> fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.borrow().fmt(f)
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// 会话写盘状态：由 SharedServices 唯一持有，所有窗口共享。
#[derive(Default)]
pub struct SessionService {
    /// 最近一次成功写盘的 v2 快照；用于避免每 2s 重复写盘。
    pub last_snapshot: Option<SnapshotV2>,
    /// Restart Session 已写好快照并拉起子进程：退出钩子不要覆盖。
    pub restarting: bool,
    /// 快照写入者是否 primary（子窗口进程不写 marker/文件）。
    pub writer: bool,
    /// 启动恢复期间抑制 autosave，避免半恢复布局覆盖磁盘上的原快照。
    pub restoring: bool,
}

impl SessionService {
    pub fn new(writer: bool) -> Self {
        Self {
            last_snapshot: None,
            restarting: false,
            writer,
            restoring: false,
        }
    }
}

/// 每用户只应有一份的服务。窗口只持有句柄，不拥有生命周期。
pub struct SharedServices {
    /// 唯一配置对象；外部 config.toml 热更新直接替换它，所有窗口立即可见。
    pub cfg: Shared<Config>,
    pub themes: Shared<ThemeRegistry>,
    /// 插件宿主：每个插件一个子进程，与窗口数无关。
    pub plugins: Shared<PluginStore>,
    /// Vault 解锁状态：一次解锁，所有窗口共享。
    pub vault: Shared<Vault>,
    /// profile 列表：同一份数据，避免两个窗口各自 save 覆盖。
    pub profiles: Shared<ProfileStore>,
    /// MCP 只有一个 runtime/socket/token；`Arc` 供 socket 线程持有。
    pub mcp: Arc<McpRuntime>,
    pub session: Shared<SessionService>,
    /// 外部 config.toml 监听器。生命周期属于服务层而不是某个窗口：primary
    /// 关闭后只要还有窗口存活，热更新就必须继续工作。
    config_watcher: RefCell<Option<ConfigWatcher>>,
    /// 2s autosave 计时器。同样属于服务层：primary 的 AppState 被 drop 后，
    /// 剩余窗口仍要持续写多窗口布局。
    session_timer: RefCell<Option<slint::Timer>>,
    /// 全进程唯一 PaneId 分配器。插件/MCP/session 都靠 PaneId 路由，不能让
    /// 每个窗口各自从 1 开始（否则 window_id+pane_id 仍然撞车）。
    next_pane_id: Cell<u64>,
}

impl SharedServices {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        themes: ThemeRegistry,
        plugins: PluginStore,
        vault: Vault,
        profiles: ProfileStore,
        mcp: Arc<McpRuntime>,
        session: SessionService,
    ) -> Self {
        Self {
            cfg: Shared::new(cfg),
            themes: Shared::new(themes),
            plugins: Shared::new(plugins),
            vault: Shared::new(vault),
            profiles: Shared::new(profiles),
            mcp,
            session: Shared::new(session),
            config_watcher: RefCell::new(None),
            session_timer: RefCell::new(None),
            next_pane_id: Cell::new(1),
        }
    }

    /// 启动 2s 会话 autosave。计时器由 SharedServices 持有，因此 primary
    /// 窗口关闭、只剩 second 时仍然继续写盘。
    pub fn start_session_autosave(&self) {
        if self.session_timer.borrow().is_some() {
            return;
        }
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(2),
            autosave_session_tick,
        );
        *self.session_timer.borrow_mut() = Some(timer);
        eprintln!("[session] autosave 已启动（SharedServices，2s）");
    }

    pub fn stop_session_autosave(&self) {
        self.session_timer.borrow_mut().take();
    }

    /// config.toml 监听器归服务层所有；primary 的 AppState 不持有它。
    pub fn set_config_watcher(&self, watcher: ConfigWatcher) {
        *self.config_watcher.borrow_mut() = Some(watcher);
    }

    pub fn stop_config_watcher(&self) {
        self.config_watcher.borrow_mut().take();
    }

    /// 分配全进程唯一的 PaneId；不复用，避免旧事件/插件消息命中新 pane。
    pub fn alloc_pane_id(&self) -> u64 {
        let id = self.next_pane_id.get().max(1);
        self.next_pane_id.set(id.saturating_add(1));
        id
    }

    /// 诊断身份：确认两个窗口拿到的是同一份服务，而不是各自 load 的副本。
    pub fn identities(&self) -> serde_json::Value {
        serde_json::json!({
            "cfg": self.cfg.identity(),
            "themes": self.themes.identity(),
            "plugins": self.plugins.identity(),
            "vault": self.vault.identity(),
            "profiles": self.profiles.identity(),
            "mcp": Arc::as_ptr(&self.mcp) as usize,
            "session": self.session.identity(),
        })
    }
}

/// 一个窗口拥有的运行时。关闭窗口时 drop 它，不得 drop SharedServices。
pub struct WindowRuntime {
    pub id: WindowId,
    pub ui: crate::MainWindow,
    pub state: Rc<RefCell<AppState>>,
    /// 该窗口的 Slint Timer。必须和窗口同生命周期，不能只放在 main 的局部变量里。
    pub timers: RefCell<Vec<slint::Timer>>,
    /// 窗口标题（session/插件路由用；不依赖 winit title 读回）。
    pub title: RefCell<String>,
    /// 最近一次聚焦的 pane，供 `pane-focus` 事件/MCP 缺省窗口使用。
    pub last_focused_pane: Cell<Option<crate::workspace::PaneId>>,
}

impl WindowRuntime {
    pub fn new(
        id: WindowId,
        ui: crate::MainWindow,
        state: Rc<RefCell<AppState>>,
        title: impl Into<String>,
    ) -> Self {
        Self {
            id,
            ui,
            state,
            timers: RefCell::new(Vec::new()),
            title: RefCell::new(title.into()),
            last_focused_pane: Cell::new(None),
        }
    }

    pub fn keep_timer(&self, timer: slint::Timer) {
        self.timers.borrow_mut().push(timer);
    }

    pub fn geometry(&self) -> WindowGeometry {
        let size = self.ui.window().size();
        let position = self.ui.window().position();
        WindowGeometry {
            x: position.x,
            y: position.y,
            w: size.width,
            h: size.height,
        }
    }

    /// 收集该窗口的 session 快照纯数据部分。
    pub fn snapshot(&self, state: &AppState) -> WindowSnapshot {
        WindowSnapshot {
            id: self.id.get(),
            geometry: self.geometry(),
            maximized: state.window_maximized,
            active_tab: state.ws.active,
            tabs: crate::capture_tabs_snapshot(state),
        }
    }
}

/// 窗口注册表。只有 primary 进程/UI 线程持有一份。
pub struct WindowRegistry {
    ids: WindowIdAllocator,
    windows: HashMap<WindowId, Rc<WindowRuntime>>,
    /// 最近聚焦/最近创建的窗口；旧回调的缺省路由。
    active: Option<WindowId>,
    /// 创建顺序，session 恢复/诊断输出稳定。
    order: Vec<WindowId>,
    pub shared: Rc<SharedServices>,
}

impl WindowRegistry {
    pub fn new(shared: Rc<SharedServices>) -> Self {
        Self {
            ids: WindowIdAllocator::new(),
            windows: HashMap::new(),
            active: None,
            order: Vec::new(),
            shared,
        }
    }

    pub fn allocate_id(&mut self) -> WindowId {
        self.ids.take()
    }

    pub fn insert(&mut self, runtime: WindowRuntime) -> Result<WindowId, String> {
        let id = runtime.id;
        if self.windows.contains_key(&id) {
            return Err(format!("duplicate {id}"));
        }
        self.ids.observe(id);
        self.windows.insert(id, Rc::new(runtime));
        self.order.push(id);
        if self.active.is_none() {
            self.active = Some(id);
        }
        Ok(id)
    }

    pub fn get(&self, id: WindowId) -> Option<Rc<WindowRuntime>> {
        self.windows.get(&id).cloned()
    }

    pub fn remove(&mut self, id: WindowId) -> Option<Rc<WindowRuntime>> {
        let runtime = self.windows.remove(&id)?;
        self.order.retain(|other| *other != id);
        if self.active == Some(id) {
            self.active = self.order.last().copied();
        }
        Some(runtime)
    }

    pub fn set_active(&mut self, id: WindowId) -> bool {
        if !self.windows.contains_key(&id) {
            return false;
        }
        self.active = Some(id);
        true
    }

    pub fn active(&self) -> Option<WindowId> {
        self.active
    }

    /// 创建顺序中的第一个窗口（primary）。旧 `with_app` 在没有回调作用域时
    /// 固定回退到它，而不是猜“当前活动窗口”。
    pub fn primary_id(&self) -> Option<WindowId> {
        self.order.first().copied()
    }

    pub fn len(&self) -> usize {
        self.windows.len()
    }

    #[allow(dead_code)] // registry 完整性 API；测试/后续控制面使用
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    pub fn ids(&self) -> Vec<WindowId> {
        self.order.clone()
    }

    pub fn runtimes(&self) -> Vec<Rc<WindowRuntime>> {
        self.order
            .iter()
            .filter_map(|id| self.windows.get(id).cloned())
            .collect()
    }

    pub fn shared(&self) -> Rc<SharedServices> {
        Rc::clone(&self.shared)
    }
}

thread_local! {
    static REGISTRY: RefCell<Option<Rc<RefCell<WindowRegistry>>>> =
        const { RefCell::new(None) };
    /// 当前 UI 回调所属窗口。`with_state` 进入时设置，退出时恢复，
    /// 让回调内部调用的旧 `with_app` 也路由到正确窗口。
    static CURRENT_WINDOW: Cell<Option<WindowId>> = const { Cell::new(None) };
}

/// 安装/替换本线程的 registry。正常启动只调用一次。
pub fn install_registry(registry: Rc<RefCell<WindowRegistry>>) {
    REGISTRY.with(|slot| {
        *slot.borrow_mut() = Some(registry);
    });
}

pub fn registry() -> Option<Rc<RefCell<WindowRegistry>>> {
    REGISTRY.with(|slot| slot.borrow().clone())
}

/// 当前回调所属窗口（仅 UI 线程）。
pub fn current_window() -> Option<WindowId> {
    CURRENT_WINDOW.with(Cell::get)
}

fn enter_current_window(id: Option<WindowId>) -> WindowScope {
    let previous = CURRENT_WINDOW.with(|slot| slot.replace(id));
    WindowScope { previous }
}

/// 进入/退出窗口作用域；`with_app` 在作用域内优先使用显式窗口。
pub struct WindowScope {
    previous: Option<WindowId>,
}

impl Drop for WindowScope {
    fn drop(&mut self) {
        CURRENT_WINDOW.with(|slot| slot.set(self.previous));
    }
}

/// 显式按 WindowId 路由。`f` 执行期间当前窗口作用域被设为 `id`。
pub fn with_window<R>(id: WindowId, f: impl FnOnce(&mut AppState) -> R) -> Option<R> {
    let registry = registry()?;
    let runtime = {
        let guard = registry.borrow();
        guard.get(id)?
    };
    let _scope = enter_current_window(Some(id));
    let state = runtime.state.clone();
    let mut st = match state.try_borrow_mut() {
        Ok(st) => st,
        Err(_) => {
            eprintln!("[runtime] with_window({id}) 重入（AppState 已被借用），忽略本次操作");
            return None;
        },
    };
    Some(f(&mut st))
}

/// 直接按 state 句柄路由（窗口构建/回调注册阶段 registry 尚未查表时使用）。
/// 与 [`with_window`] 一样会设置当前窗口作用域。
pub fn with_state<R>(
    state: &Rc<RefCell<AppState>>,
    f: impl FnOnce(&mut AppState) -> R,
) -> Option<R> {
    let id = state.borrow().window_id;
    let _scope = enter_current_window(Some(id));
    let mut st = match state.try_borrow_mut() {
        Ok(st) => st,
        Err(_) => {
            eprintln!("[runtime] with_state({id}) 重入（AppState 已被借用），忽略本次操作");
            return None;
        },
    };
    Some(f(&mut st))
}

/// 当前/活动窗口；没有显式作用域时回退到 registry.active。
///
/// MCP 的“缺省 window_id = active”用这个；旧 UI 回调不要用它猜窗口。
pub fn with_active_window<R>(f: impl FnOnce(&mut AppState) -> R) -> Option<R> {
    let id = current_window().or_else(|| registry()?.borrow().active())?;
    with_window(id, f)
}

/// 旧 `with_app` 的确定性回退：回调作用域优先，否则 primary；primary 已关闭
/// 时才落到 active。这样 primary 的旧回调不会在 B 聚焦时误改 B。
pub fn with_primary_window<R>(f: impl FnOnce(&mut AppState) -> R) -> Option<R> {
    let id = match current_window() {
        Some(id) => id,
        None => {
            let registry = registry()?;
            let guard = registry.borrow();
            guard.primary_id().or_else(|| guard.active())?
        },
    };
    with_window(id, f)
}

/// 对 registry 里每个窗口执行一次；快照/配置广播用。
pub fn with_all_windows(mut f: impl FnMut(WindowId, &mut AppState)) {
    let Some(registry) = registry() else {
        return;
    };
    let ids = registry.borrow().ids();
    for id in ids {
        let _ = with_window(id, |st| f(id, st));
    }
}

/// 按全局唯一 PaneId 找窗口。PTY 后台线程只带 pane id，不能靠“当前活动窗口”。
pub fn window_for_pane(pane: u64) -> Option<WindowId> {
    let registry = registry()?;
    let runtimes = registry.borrow().runtimes();
    for runtime in runtimes {
        let state = runtime.state.clone();
        if state.try_borrow().ok().is_some_and(|st| st.panes.contains_key(&pane)) {
            return Some(runtime.id);
        }
    }
    None
}

/// 记录窗口最近聚焦的 pane（插件 pane-focus / 后续 per-window scope 用）。
pub fn note_pane_focus(id: WindowId, pane: crate::workspace::PaneId) {
    if let Some(registry) = registry() {
        if let Some(runtime) = registry.borrow().get(id) {
            runtime.last_focused_pane.set(Some(pane));
        }
    }
}

/// 设置活动窗口（focus/点击时调用）；返回是否仍存在。
pub fn set_active_window(id: WindowId) -> bool {
    let Some(registry) = registry() else {
        return false;
    };
    let ok = registry.borrow_mut().set_active(id);
    if ok {
        eprintln!("[runtime] active window={id}");
    }
    ok
}

pub fn window_count() -> usize {
    registry().map_or(0, |registry| registry.borrow().len())
}

/// 从 registry 移除窗口并返回其 runtime；调用方负责 hide/drop 顺序。
pub fn remove_window(id: WindowId) -> Option<Rc<WindowRuntime>> {
    let registry = registry()?;
    let runtime = registry.borrow_mut().remove(id);
    if runtime.is_some() {
        eprintln!("[runtime] closed {id}（remaining={}）", window_count());
    }
    runtime
}

/// 所有窗口的 pane id 并集；GPU retain 必须用它，不能只看当前窗口。
pub fn all_pane_ids() -> Vec<u64> {
    let mut out = Vec::new();
    with_all_windows(|_id, st| out.extend(st.panes.keys().copied()));
    out
}

/// 所有窗口的 session 快照（按 WindowId 排序）。
///
/// 只收集纯数据；不写盘、不校验。调用方必须确保没有窗口正处于 `&mut
/// AppState` 借用中（timer / 事件循环回调之外），否则该窗口会被 `try_borrow`
/// 跳过。窗口关闭/新建后的保存都通过 `invoke_from_event_loop` 延迟到借用之外。
pub fn capture_snapshot_v2() -> SnapshotV2 {
    let mut windows = Vec::new();
    let Some(registry) = registry() else {
        return SnapshotV2::default();
    };
    for runtime in registry.borrow().runtimes() {
        let state = runtime.state.clone();
        match state.try_borrow() {
            Ok(st) => windows.push(runtime.snapshot(&st)),
            Err(_) => eprintln!(
                "[session] capture 跳过 {}（AppState 正被借用）",
                runtime.id
            ),
        };
    }
    SnapshotV2::new(windows)
}

/// 可恢复的快照：只有带 Tab 的窗口，active_tab 已 clamp。
pub fn capture_restorable_snapshot_v2() -> SnapshotV2 {
    restorable_snapshot(capture_snapshot_v2())
}

/// 在某个窗口的 `with_app` 借用内捕获全量快照。
///
/// 当前窗口的 `AppState` 已经被可变借用，不能再 `try_borrow`；调用方把 `st`
/// 传进来，其余窗口正常采集。Restart Session 必须用这个版本，否则会漏掉
/// 正在触发菜单的那个 primary 窗口。
pub fn capture_restorable_snapshot_v2_with_window(
    current_id: WindowId,
    current: &AppState,
) -> SnapshotV2 {
    let mut windows = Vec::new();
    let Some(registry) = registry() else {
        return SnapshotV2::default();
    };
    for runtime in registry.borrow().runtimes() {
        if runtime.id == current_id {
            windows.push(runtime.snapshot(current));
            continue;
        }
        let state = runtime.state.clone();
        match state.try_borrow() {
            Ok(st) => windows.push(runtime.snapshot(&st)),
            Err(_) => eprintln!(
                "[session] capture 跳过 {}（AppState 正被借用）",
                runtime.id
            ),
        };
    }
    restorable_snapshot(SnapshotV2::new(windows))
}

fn restorable_snapshot(mut snap: SnapshotV2) -> SnapshotV2 {
    snap.windows.retain(WindowSnapshot::is_restorable);
    for window in &mut snap.windows {
        window.active_tab = window.clamped_active_tab();
    }
    snap
}

fn session_save_allowed(shared: &SharedServices) -> bool {
    let (writer, restarting, restoring) =
        shared.session.with(|s| (s.writer, s.restarting, s.restoring));
    writer
        && !restarting
        && !restoring
        && shared.cfg.with(|cfg| cfg.restore_session_on_startup)
}

/// 立即保存一次多窗口快照。`force=false` 时和上次成功快照相同就不写。
///
/// 只由 writer（primary 进程）调用；所有窗口共享同一份 SharedServices，所以
/// primary 关闭后剩余窗口仍能继续保存。写盘走 `session::save_v2`，保留
/// tmp+fsync+rename 与上一代 `.bak`。
pub fn save_session_snapshot(reason: &str, force: bool) -> bool {
    let Some(registry) = registry() else {
        return false;
    };
    let shared = registry.borrow().shared();
    if !session_save_allowed(&shared) {
        return false;
    }
    let snapshot = capture_restorable_snapshot_v2();
    if snapshot.windows.is_empty() {
        if reason == "exit" {
            session::clear();
            eprintln!("[session] {reason}: 没有可保存的窗口，已清除旧快照");
        }
        return false;
    }
    if !force {
        let unchanged = shared
            .session
            .with(|session| session.last_snapshot.as_ref() == Some(&snapshot));
        if unchanged {
            return true;
        }
    }
    match session::save_v2(&snapshot) {
        Ok(path) => {
            let windows = snapshot.windows.len();
            let tabs: usize = snapshot.windows.iter().map(|w| w.tabs.len()).sum();
            shared
                .session
                .with_mut(|session| session.last_snapshot = Some(snapshot));
            eprintln!(
                "[session] {reason}: 已保存 {}（{} 个窗口 / {} 个标签）",
                path.display(),
                windows,
                tabs
            );
            true
        },
        Err(error) => {
            eprintln!("[session] {reason}: 保存失败: {error}");
            false
        },
    }
}

fn autosave_session_tick() {
    let _ = save_session_snapshot("autosave", false);
}

/// 正常退出/最后一个窗口关闭时调用：强制写一次，没有窗口则清旧快照。
pub fn save_session_on_exit() {
    let _ = save_session_snapshot("exit", true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_ids_start_at_one_and_never_reuse() {
        let mut ids = WindowIdAllocator::new();
        assert_eq!(ids.take(), WindowId::FIRST);
        let second = ids.take();
        assert_eq!(second, WindowId(2));
        // observe 只能把水位抬高，不能把已分配的 id 再发一次。
        ids.observe(WindowId(10));
        assert_eq!(ids.take(), WindowId(11));
    }

    #[test]
    fn shared_handles_point_at_one_value() {
        let a = Shared::new(41u32);
        let b = a.clone_handle();
        b.with_mut(|value| *value += 1);
        assert_eq!(*a, 42);
        assert_eq!(a.identity(), b.identity());
        let replaced = a.replace(7);
        assert_eq!(replaced, 42);
        assert_eq!(*b, 7);
    }

    #[test]
    fn window_scope_nests_and_restores() {
        assert_eq!(current_window(), None);
        {
            let _outer = enter_current_window(Some(WindowId(3)));
            assert_eq!(current_window(), Some(WindowId(3)));
            {
                let _inner = enter_current_window(Some(WindowId(9)));
                assert_eq!(current_window(), Some(WindowId(9)));
            }
            assert_eq!(current_window(), Some(WindowId(3)));
        }
        assert_eq!(current_window(), None);
    }

    #[test]
    fn session_service_starts_clean() {
        let session = SessionService::new(true);
        assert!(session.writer);
        assert!(!session.restarting);
        assert!(!session.restoring);
        assert!(session.last_snapshot.is_none());
    }
}
