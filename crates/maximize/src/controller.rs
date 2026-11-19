// SPDX-License-Identifier: GPL-3.0-only
//! 纯最大化状态机。
//!
//! 这个文件刻意不依赖 X11/Slint：所有事件都是显式方法调用，`click_at` /
//! `check_timeout` 接受注入的时间，单元测试可以精确构造“通知先到/后到/丢失”
//! 的顺序。传输层只负责把真实的 X11 事件翻译成这些方法调用。

use std::time::{Duration, Instant};

/// 请求等待 PropertyNotify 确认的上限。
///
/// 这不是“校准窗口”，也不会触发任何状态改写：超时只记录 `last_error`，
/// `desired`/`pending` 保持不变（见 `check_timeout`）。取 2s 是为了覆盖
/// 慢 WM/远程 X11 的正常 round-trip，同时让故障在日志/UI 里可见。
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(2000);

/// 一次 PropertyNotify 对状态机的影响，主要用于日志/测试断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// 通知与 pending 一致：pending 清除，desired 以 observed 为准。
    Confirmed,
    /// 没有 pending，外部 WM 改变了状态：desired 跟随 observed。
    ExternalSync,
    /// 有 pending 但观察值不一致：可能是 WM 应用的中间态/旧通知，保持 pending。
    StaleWhilePending,
    /// pending 已超时，迟到的真实观察结束了过期请求：desired 跟随 observed。
    TimeoutRecovery,
    /// 观察值与状态机当前值相同，没有产生状态变化。
    Unchanged,
}

/// 状态机快照（UI 只读这个结构，不直接拿 X11/几何猜状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaximizeSnapshot {
    pub desired: bool,
    pub observed: bool,
    pub pending: Option<bool>,
    pub last_error: Option<String>,
}

impl MaximizeSnapshot {
    /// UI 图标应显示的状态：永远是 desired，不是 observed/几何。
    pub fn target(&self) -> bool {
        self.desired
    }
}

/// 工业级最大化控制器：单一状态源。
///
/// 不变量：
///
/// 1. `click` 永远翻转 `desired`，不受 `observed`/几何/timer 影响；
/// 2. 请求的 target 永远是 `desired`；
/// 3. 只有 PropertyNotify 能清除 `pending`（发送失败是“请求未离开进程”的
///    例外，记录错误并清除 pending，但绝不改 `desired`）；
/// 4. 外部 WM 改变只通过 PropertyNotify 同步，不轮询/不猜；
/// 5. 没有 800ms/180ms 校准、workarea 或标题查找。
#[derive(Debug)]
pub struct MaximizeController {
    /// 用户最后一次意图；唯一 target。
    desired: bool,
    /// 最近一次 `_NET_WM_STATE` 的真实观察（PropertyNotify 驱动）。
    observed: bool,
    /// 已发出、尚未被 PropertyNotify 确认的请求。
    pending: Option<bool>,
    /// 最近一次发送失败/超时/传输错误；成功确认后清除。
    last_error: Option<String>,
    /// pending 的发出时间；仅用于超时“报告错误”，不参与 target 决策。
    pending_since: Option<Instant>,
    /// 请求确认超时；只影响错误上报。
    timeout: Duration,
}

impl MaximizeController {
    /// 以启动时观察到的 `_NET_WM_STATE` 初始化：desired = observed。
    pub fn new(observed: bool) -> Self {
        Self::with_timeout(observed, DEFAULT_REQUEST_TIMEOUT)
    }

    /// 测试/特殊环境可注入超时；超时不影响 desired。
    pub fn with_timeout(observed: bool, timeout: Duration) -> Self {
        Self {
            desired: observed,
            observed,
            pending: None,
            last_error: None,
            pending_since: None,
            // 0 会让每次 tick 都立刻报错，没有实际意义；至少 1ms。
            timeout: timeout.max(Duration::from_millis(1)),
        }
    }

    pub fn desired(&self) -> bool {
        self.desired
    }

    pub fn observed(&self) -> bool {
        self.observed
    }

    pub fn pending(&self) -> Option<bool> {
        self.pending
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 当前请求 target：永远等于 desired。
    pub fn target(&self) -> bool {
        self.desired
    }

    pub fn snapshot(&self) -> MaximizeSnapshot {
        MaximizeSnapshot {
            desired: self.desired,
            observed: self.observed,
            pending: self.pending,
            last_error: self.last_error.clone(),
        }
    }

    /// 用户点击：只翻转 `desired`，设置 pending，返回需要发送的 target。
    ///
    /// 不读取 observed、不查询几何、不因为上一个请求还在 pending 而特殊处理：
    /// 连续点击必须严格按点击次数翻转。
    pub fn click(&mut self) -> bool {
        self.click_at(Instant::now())
    }

    /// 请求无法送达 worker 时回滚 UI：不要再显示一个从未发出的 desired。
    pub fn revert_to_observed(&mut self) {
        self.desired = self.observed;
        self.pending = None;
        self.pending_since = None;
    }

    fn click_at(&mut self, now: Instant) -> bool {
        self.desired = !self.desired;
        self.pending = Some(self.desired);
        self.pending_since = Some(now);
        // 新请求开始，旧错误不再代表当前尝试；错误日志仍保留在传输层。
        self.last_error = None;
        self.desired
    }

    /// `_NET_WM_STATE` 的 PropertyNotify 到达（传输层已读取最新属性）。
    ///
    /// * pending 与观察值一致：确认，清除 pending，desired 跟随 observed；
    /// * 没有 pending：外部 WM 改变，desired 同步到 observed；
    /// * pending 与观察值不一致：可能是 WM 的中间态或点击前排队的旧通知，
    ///   保持 pending/desired，等真正的确认或超时。
    pub fn on_property_notify(&mut self, observed: bool) -> NotifyOutcome {
        self.observed = observed;

        if self.pending == Some(observed) {
            self.pending = None;
            self.pending_since = None;
            self.desired = observed;
            self.last_error = None;
            return NotifyOutcome::Confirmed;
        }

        if self.pending.is_none() {
            let changed = self.desired != observed;
            self.desired = observed;
            return if changed {
                NotifyOutcome::ExternalSync
            } else {
                NotifyOutcome::Unchanged
            };
        }

        // pending 已超时（pending_since 被 check_timeout 清空，但按不变量
        // 保留 pending 等待确认）。此后到达的真实观察不能再被永久丢弃：
        // 用 PropertyNotify 结束过期请求，按外部实际状态同步 desired。
        if self.pending_since.is_none() {
            self.pending = None;
            self.desired = observed;
            self.last_error = None;
            return NotifyOutcome::TimeoutRecovery;
        }

        NotifyOutcome::StaleWhilePending
    }

    /// 请求没能离开进程（无 WM、EWMH 不支持、连接失败）。
    ///
    /// 错误必须可见，但 target 不能因失败回退：用户点的是“最大化”，UI 图标
    /// 和下一次 click 的翻转基准仍以 desired 为准。pending 清除是因为这个
    /// 请求从未发出，不存在等待确认的对象。
    pub fn on_request_failed(&mut self, error: impl Into<String>) {
        self.pending = None;
        self.pending_since = None;
        self.last_error = Some(error.into());
    }

    /// 传输层只读查询失败等非状态错误：只记录，不改 desired/pending。
    pub fn note_transport_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    /// 检查 pending 是否超时。
    ///
    /// 超时**只记录错误**：`desired` 不变，`pending` 也不清除（不变量 3），
    /// 这样迟到的 PropertyNotify 仍能完成确认。返回错误只上报一次；下一次
    /// click 会重置计时。
    pub fn check_timeout(&mut self, now: Instant) -> Option<String> {
        let pending = self.pending?;
        let since = self.pending_since?;
        let elapsed = now.saturating_duration_since(since);
        if elapsed < self.timeout {
            return None;
        }

        // 只报一次；pending 保留给迟到的 PropertyNotify 确认。
        self.pending_since = None;
        let message = format!(
            "PropertyNotify timeout after {}ms (target={pending}, observed={})",
            elapsed.as_millis(),
            self.observed
        );
        self.last_error = Some(message.clone());
        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(observed: bool) -> MaximizeController {
        MaximizeController::with_timeout(observed, Duration::from_millis(500))
    }

    #[test]
    fn click_flips_only_desired_and_sets_pending() {
        let mut c = controller(false);
        assert_eq!(c.click(), true);
        assert!(c.desired());
        assert!(!c.observed(), "click 不得伪造 observed");
        assert_eq!(c.pending(), Some(true));
        assert_eq!(c.target(), true);
        assert_eq!(c.snapshot().last_error, None);

        assert_eq!(c.click(), false);
        assert!(!c.desired());
        assert_eq!(c.pending(), Some(false));
        assert_eq!(c.target(), false);
    }

    #[test]
    fn pending_is_confirmed_in_order() {
        let mut c = controller(false);
        c.click();
        assert_eq!(c.pending(), Some(true));

        // WM 先上报一个旧值：pending 不能被清掉。
        assert_eq!(
            c.on_property_notify(false),
            NotifyOutcome::StaleWhilePending
        );
        assert_eq!(c.pending(), Some(true));
        assert!(c.desired());

        // 真正的新值确认请求。
        assert_eq!(c.on_property_notify(true), NotifyOutcome::Confirmed);
        assert_eq!(c.pending(), None);
        assert!(c.observed());
        assert!(c.desired());
        assert_eq!(c.target(), true);
    }

    #[test]
    fn stale_notification_after_click_does_not_clear_pending() {
        let mut c = controller(true);
        c.click(); // desired=false, pending=false
                   // 点击前排队的“已最大化”通知在 restore 请求后到达。
        assert_eq!(c.on_property_notify(true), NotifyOutcome::StaleWhilePending);
        assert_eq!(c.pending(), Some(false));
        assert!(!c.desired());

        assert_eq!(c.on_property_notify(false), NotifyOutcome::Confirmed);
        assert_eq!(c.pending(), None);
        assert!(!c.desired());
    }

    #[test]
    fn external_change_without_pending_syncs_desired() {
        let mut c = controller(false);
        assert_eq!(c.on_property_notify(true), NotifyOutcome::ExternalSync);
        assert!(c.desired());
        assert!(c.observed());
        assert_eq!(c.pending(), None);

        // 下一次 click 在外部状态基础上翻转：true -> false。
        assert_eq!(c.click(), false);
        assert_eq!(c.target(), false);
    }

    #[test]
    fn external_change_while_pending_does_not_steal_target() {
        let mut c = controller(false);
        c.click(); // pending=true, desired=true
                   // 某个外部 WM 通知说恢复了；这不是我们 pending 请求的确认。
        assert_eq!(
            c.on_property_notify(false),
            NotifyOutcome::StaleWhilePending
        );
        assert_eq!(c.pending(), Some(true));
        assert!(c.desired(), "pending 期间外部报告不得改写用户 target");
    }

    #[test]
    fn missed_notification_timeout_only_reports_error() {
        let mut c = controller(false);
        let t0 = Instant::now();
        c.click_at(t0);
        assert_eq!(c.check_timeout(t0 + Duration::from_millis(499)), None);
        let err = c
            .check_timeout(t0 + Duration::from_millis(500))
            .expect("timeout 必须报错");
        assert!(err.contains("timeout"), "{err}");
        assert!(c.desired(), "超时不得改写 target");
        assert_eq!(c.target(), true);
        assert_eq!(c.observed(), false, "超时不得伪造 observed");
        assert_eq!(
            c.pending(),
            Some(true),
            "只有 PropertyNotify 能清除 pending"
        );
        assert!(c.last_error().is_some());

        // 同一请求只报一次，避免 20ms tick 刷屏。
        assert_eq!(c.check_timeout(t0 + Duration::from_secs(5)), None);
    }

    #[test]
    fn late_notification_after_timeout_still_confirms() {
        let mut c = controller(false);
        let t0 = Instant::now();
        c.click_at(t0);
        assert!(c.check_timeout(t0 + Duration::from_secs(1)).is_some());
        assert_eq!(c.pending(), Some(true));

        assert_eq!(c.on_property_notify(true), NotifyOutcome::Confirmed);
        assert_eq!(c.pending(), None);
        assert!(c.desired());
        assert_eq!(c.last_error(), None, "确认成功后清除错误");
    }

    #[test]
    fn conflicting_notification_after_timeout_recovers_without_deadlock() {
        let mut c = controller(false);
        let t0 = Instant::now();
        c.click_at(t0); // pending=true
        assert!(c.check_timeout(t0 + Duration::from_secs(1)).is_some());
        assert_eq!(c.pending(), Some(true), "超时本身不清 pending");

        // WM 最终没有最大化（或被外部恢复）：迟到的 false 是唯一权威观察。
        assert_eq!(c.on_property_notify(false), NotifyOutcome::TimeoutRecovery);
        assert_eq!(c.pending(), None);
        assert!(!c.observed());
        assert!(!c.desired());
        assert_eq!(c.target(), false);
        assert_eq!(c.last_error(), None);
    }

    #[test]
    fn send_failure_keeps_target_and_allows_next_click() {
        let mut c = controller(false);
        assert_eq!(c.click(), true);
        c.on_request_failed("WM 未声明 _NET_SUPPORTED");
        assert!(c.desired(), "发送失败不得回退 target");
        assert_eq!(c.target(), true);
        assert_eq!(c.pending(), None, "请求未发出，没有待确认对象");
        assert!(c.last_error().unwrap().contains("WM"));

        // 再次点击仍以 desired 为基准翻转，而不是 observed。
        assert_eq!(c.click(), false);
        assert!(!c.desired());
        assert_eq!(c.target(), false);
    }

    #[test]
    fn rapid_clicks_keep_latest_target() {
        let mut c = controller(false);
        let t0 = Instant::now();
        for i in 0..6 {
            let target = c.click_at(t0 + Duration::from_millis(i * 20));
            assert_eq!(target, i % 2 == 0, "第 {} 次点击必须翻转 desired", i + 1);
        }
        assert!(!c.desired());
        assert_eq!(c.pending(), Some(false));
        assert_eq!(c.target(), false);

        // 中间态的迟到通知（maximized）不能覆盖最后一次 restore。
        assert_eq!(c.on_property_notify(true), NotifyOutcome::StaleWhilePending);
        assert!(!c.desired());
        assert_eq!(c.pending(), Some(false));

        assert_eq!(c.on_property_notify(false), NotifyOutcome::Confirmed);
        assert!(!c.desired());
        assert_eq!(c.pending(), None);
    }

    #[test]
    fn timeout_restarts_on_next_click() {
        let mut c = controller(false);
        let t0 = Instant::now();
        c.click_at(t0);
        assert!(c.check_timeout(t0 + Duration::from_secs(1)).is_some());
        // pending 未清：第二次 click 直接替换为相反 target。
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(c.click_at(t1), false);
        assert_eq!(c.pending(), Some(false));
        assert_eq!(c.last_error(), None, "新请求清除旧错误");
        assert!(c.check_timeout(t1 + Duration::from_millis(500)).is_some());
    }

    #[test]
    fn initially_maximized_click_restores() {
        let mut c = controller(true);
        assert_eq!(c.click(), false);
        assert_eq!(c.pending(), Some(false));
        assert_eq!(c.on_property_notify(false), NotifyOutcome::Confirmed);
        assert!(!c.desired());
        assert!(!c.observed());
    }

    #[test]
    fn duplicate_notification_is_harmless() {
        let mut c = controller(true);
        assert_eq!(c.on_property_notify(true), NotifyOutcome::Unchanged);
        assert!(c.desired());

        c.click();
        assert_eq!(c.on_property_notify(false), NotifyOutcome::Confirmed);
        // WM 重复发送同一个 NewValue（或读取到同值）不能再次翻转。
        assert_eq!(c.on_property_notify(false), NotifyOutcome::Unchanged);
        assert!(!c.desired());
        assert_eq!(c.pending(), None);
    }

    #[test]
    fn transport_error_does_not_touch_state() {
        let mut c = controller(false);
        c.click();
        c.note_transport_error("PropertyNotify 属性读取失败");
        assert!(c.desired());
        assert_eq!(c.pending(), Some(true));
        assert!(c.last_error().unwrap().contains("读取失败"));
    }

    #[test]
    fn snapshot_target_is_always_desired() {
        let mut c = controller(false);
        c.click();
        c.on_property_notify(false); // stale intermediate
        let snap = c.snapshot();
        assert_eq!(snap.target(), true);
        assert_eq!(snap.desired, true);
        assert_eq!(snap.observed, false);
        assert_eq!(snap.pending, Some(true));
    }
}

#[cfg(test)]
mod revert_tests {
    use super::MaximizeController;

    #[test]
    fn revert_to_observed_clears_pending_click() {
        let mut c = MaximizeController::new(false);
        assert!(c.click());
        assert!(c.pending().is_some());
        c.revert_to_observed();
        assert!(!c.desired());
        assert!(!c.observed());
        assert!(c.pending().is_none());
    }
}
