// SPDX-License-Identifier: GPL-3.0-only
//! 通用连接状态模型（SESS-007）。
//!
//! 以前“连接中/已连接/断开/失败”只存在于 SSH 退出提示里：本地 shell 没有状态，
//! 自动重连写死 2/5/10 秒且只认 `kind == "ssh"`。这里把状态提升为与传输类型
//! 无关的模型：本地 shell、插件 transport、未来的 SSH/Serial/Telnet 都写同一份
//! [`ConnectionStatus`]，Tab 标记与底部状态 pill 只读聚合结果。
//!
//! 模块刻意不依赖 Slint，纯函数可在 `cargo test -p xtervo-app --bin xtervo` 里跑。

use std::time::Duration;

/// 连接生命周期状态。
///
/// 派生 `Ord` 的顺序就是多 pane 聚合优先级：`None < Connected < Connecting <
/// Disconnected < Failed`，一个 Tab 里有多个 pane 时取“最需要被看见”的状态
/// （失败 > 断开 > 连接中 > 已连接 > 无）。改动枚举顺序前先看
/// [`aggregate`] 的单测。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConnectionState {
    /// 还没有连接信息（例如刚恢复、尚未启动的 pane）。
    #[default]
    None,
    /// 会话在运行；本地 shell 启动成功即进入此态。
    Connected,
    /// 正在建立连接（SSH/插件 transport 从启动到收到首段输出）。
    Connecting,
    /// 连接已断开，但没有被判定为失败（退出码 0 / 用户主动结束）。
    Disconnected,
    /// 启动失败、异常退出，或自动重连次数耗尽。
    Failed,
}

impl ConnectionState {
    /// 稳定的整数编码，Slint 属性用（0=None 1=Connected 2=Connecting 3=Disconnected 4=Failed）。
    pub fn code(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Connected => 1,
            Self::Connecting => 2,
            Self::Disconnected => 3,
            Self::Failed => 4,
        }
    }

    /// Tab 上的非纯颜色字形：可连接态不占位，异常态用不同字符区分。
    pub fn glyph(self) -> &'static str {
        match self {
            Self::None | Self::Connected => "",
            Self::Connecting => "…",
            Self::Disconnected => "○",
            Self::Failed => "✕",
        }
    }

    pub fn label(self, zh: bool) -> &'static str {
        match (self, zh) {
            (Self::None, _) => "",
            (Self::Connected, true) => "已连接",
            (Self::Connected, false) => "Connected",
            (Self::Connecting, true) => "连接中",
            (Self::Connecting, false) => "Connecting",
            (Self::Disconnected, true) => "已断开",
            (Self::Disconnected, false) => "Disconnected",
            (Self::Failed, true) => "连接失败",
            (Self::Failed, false) => "Failed",
        }
    }
}

/// 单个 pane 的连接状态。重试计数在“断开等待重连”期间递增，耗尽后转
/// [`ConnectionState::Failed`]。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionStatus {
    pub state: ConnectionState,
    /// 已经安排/执行过的自动重连次数（0 = 没重试过）。
    pub retries: u32,
    /// 当前策略允许的最大自动重连次数；0 = 未启用自动重连。
    pub max_retries: u32,
    /// 给状态 pill/tooltip 的简短原因（“exit 0”、“retry 1/3 in 2s”）。
    pub detail: String,
}

impl ConnectionStatus {
    pub fn connecting() -> Self {
        Self { state: ConnectionState::Connecting, ..Self::default() }
    }

    pub fn connected() -> Self {
        Self { state: ConnectionState::Connected, ..Self::default() }
    }

    pub fn disconnected(detail: impl Into<String>) -> Self {
        Self {
            state: ConnectionState::Disconnected,
            detail: detail.into(),
            ..Self::default()
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            state: ConnectionState::Failed,
            detail: detail.into(),
            ..Self::default()
        }
    }

    /// 已安排第 `retries` 次重连：保持 Disconnected，但带上计数与等待信息。
    pub fn retrying(
        mut self,
        retries: u32,
        max_retries: u32,
        detail: impl Into<String>,
    ) -> Self {
        self.state = ConnectionState::Disconnected;
        self.retries = retries;
        self.max_retries = max_retries;
        self.detail = detail.into();
        self
    }

    /// 重连次数耗尽：转 Failed，保留最终计数。
    pub fn gave_up(mut self, retries: u32, max_retries: u32) -> Self {
        self.state = ConnectionState::Failed;
        self.retries = retries;
        self.max_retries = max_retries;
        self.detail = if max_retries > 0 {
            format!("gave up after {retries}/{max_retries} retries")
        } else {
            "auto-reconnect disabled".to_string()
        };
        self
    }

    pub fn note_connected(&mut self) {
        self.state = ConnectionState::Connected;
        self.detail.clear();
    }

    /// 状态 pill 文本：连接中/已连接不显示重试次数，断开/失败显示进度。
    pub fn label(&self, zh: bool) -> String {
        let base = self.state.label(zh);
        if base.is_empty() {
            return String::new();
        }
        if matches!(self.state, ConnectionState::Disconnected | ConnectionState::Failed)
            && self.max_retries > 0
        {
            if self.retries > 0 {
                return if zh {
                    format!("{base} {}/{}", self.retries, self.max_retries)
                } else {
                    format!("{base} {}/{}", self.retries, self.max_retries)
                };
            }
        }
        base.to_string()
    }

    /// Tab tooltip / 状态 pill 的详细说明；没内容时回退到 label。
    pub fn tooltip(&self, zh: bool) -> String {
        let label = self.label(zh);
        if self.detail.is_empty() {
            label
        } else if label.is_empty() {
            self.detail.clone()
        } else {
            format!("{label} · {}", self.detail)
        }
    }
}

/// 多 pane 聚合：取优先级最高的状态；所有 pane 都没有状态时返回 `None`。
pub fn aggregate<'a>(panes: impl IntoIterator<Item = &'a ConnectionStatus>) -> ConnectionState {
    panes
        .into_iter()
        .map(|status| status.state)
        .max()
        .unwrap_or(ConnectionState::None)
}

/// 可配置的自动重连策略（全局 config；Profile 的 `auto_reconnect` 仍是开关）。
///
/// 默认值与历史行为一致：2s / 5s / 10s，第 4 次放弃。`max_attempts = 0`
/// 表示关闭自动重连；退避列表短于次数时重复最后一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub backoff_ms: Vec<u64>,
}

pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
pub const DEFAULT_BACKOFF_MS: [u64; 3] = [2_000, 5_000, 10_000];
/// 上限：避免手写 config 写出 100 次重连/一小时退避把 UI 卡住。
pub const MAX_ATTEMPTS_CAP: u32 = 20;
pub const MIN_BACKOFF_MS: u64 = 100;
pub const MAX_BACKOFF_MS: u64 = 600_000;

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff_ms: DEFAULT_BACKOFF_MS.to_vec(),
        }
    }
}

impl ReconnectPolicy {
    /// 从 config 构造并夹取：次数 ≤ 20，每次退避 100ms–10min；空列表回退默认。
    pub fn from_config(max_attempts: u32, backoff_ms: &[u64]) -> Self {
        let max_attempts = max_attempts.min(MAX_ATTEMPTS_CAP);
        let backoff_ms = if backoff_ms.is_empty() {
            DEFAULT_BACKOFF_MS.to_vec()
        } else {
            backoff_ms
                .iter()
                .map(|ms| (*ms).clamp(MIN_BACKOFF_MS, MAX_BACKOFF_MS))
                .collect()
        };
        Self { max_attempts, backoff_ms }
    }

    /// 第 `attempt` 次（1-based）重连的等待时间；超过次数/关闭时返回 None。
    pub fn delay(&self, attempt: u32) -> Option<Duration> {
        if attempt == 0 || self.max_attempts == 0 || attempt > self.max_attempts {
            return None;
        }
        let idx = (attempt as usize - 1).min(self.backoff_ms.len().saturating_sub(1));
        self.backoff_ms.get(idx).map(|ms| Duration::from_millis(*ms))
    }

    /// 策略下是否还有下一次重连（用于文案“第 4 次放弃”）。
    pub fn has_next(&self, attempt: u32) -> bool {
        self.delay(attempt.saturating_add(1)).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_priority_is_failure_first() {
        let states = [
            ConnectionStatus::connected(),
            ConnectionStatus::connecting(),
            ConnectionStatus::disconnected("exit 0"),
        ];
        assert_eq!(aggregate(&states), ConnectionState::Disconnected);
        let with_failed = [ConnectionStatus::disconnected("x"), ConnectionStatus::failed("boom")];
        assert_eq!(aggregate(&with_failed), ConnectionState::Failed);
        let none: [ConnectionStatus; 0] = [];
        assert_eq!(aggregate(&none), ConnectionState::None);
    }

    #[test]
    fn status_labels_and_retry_progress() {
        let mut s = ConnectionStatus::disconnected("exit 0");
        assert_eq!(s.label(true), "已断开");
        assert_eq!(s.label(false), "Disconnected");
        s = s.retrying(2, 3, "retry 2 in 5s");
        assert_eq!(s.label(true), "已断开 2/3");
        assert_eq!(s.tooltip(true), "已断开 2/3 · retry 2 in 5s");
        s = s.gave_up(3, 3);
        assert_eq!(s.state, ConnectionState::Failed);
        assert_eq!(s.label(false), "Failed 3/3");
        assert!(s.detail.contains("3/3"));

        let c = ConnectionStatus::connected();
        assert_eq!(c.state.code(), 1);
        assert_eq!(c.state.glyph(), "");
        assert_eq!(ConnectionState::Connecting.glyph(), "…");
        assert_eq!(ConnectionState::Disconnected.glyph(), "○");
        assert_eq!(ConnectionState::Failed.glyph(), "✕");
        assert_eq!(ConnectionState::None.code(), 0);
    }

    #[test]
    fn default_policy_matches_legacy_2_5_10_backoff() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.delay(1), Some(Duration::from_secs(2)));
        assert_eq!(p.delay(2), Some(Duration::from_secs(5)));
        assert_eq!(p.delay(3), Some(Duration::from_secs(10)));
        assert_eq!(p.delay(4), None, "第 4 次应停止重连");
        assert!(!p.has_next(3));
        assert!(p.has_next(2));
    }

    #[test]
    fn policy_is_configurable_and_clamped() {
        let p = ReconnectPolicy::from_config(1, &[500]);
        assert_eq!(p.delay(1), Some(Duration::from_millis(500)));
        assert_eq!(p.delay(2), None);

        // 退避列表短于次数：重复最后一项，而不是直接放弃。
        let p = ReconnectPolicy::from_config(4, &[1000, 3000]);
        assert_eq!(p.delay(3), Some(Duration::from_millis(3000)));
        assert_eq!(p.delay(4), Some(Duration::from_millis(3000)));
        assert_eq!(p.delay(5), None);

        // 0 = 关闭自动重连；空退避回退默认；越界夹取。
        assert_eq!(ReconnectPolicy::from_config(0, &[1000]).delay(1), None);
        let p = ReconnectPolicy::from_config(2, &[]);
        assert_eq!(p.delay(1), Some(Duration::from_secs(2)));
        let p = ReconnectPolicy::from_config(999, &[1, 99_999_999]);
        assert_eq!(p.max_attempts, MAX_ATTEMPTS_CAP);
        assert_eq!(p.delay(1), Some(Duration::from_millis(MIN_BACKOFF_MS)));
        assert_eq!(p.delay(2), Some(Duration::from_millis(MAX_BACKOFF_MS)));
    }
}
