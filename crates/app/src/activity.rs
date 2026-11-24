// SPDX-License-Identifier: GPL-3.0-only
//! Tab 活动状态聚合与通知文本清洗。
//!
//! 刻意不依赖 Slint：这些是纯函数，可在 `cargo test -p xtervo-app --bin xtervo` 里单测。

use xtervo_core::{Notification, Progress, ProgressState};

/// 桌面通知标题上限（字符数，不是字节）。
pub const MAX_NOTIFY_TITLE: usize = 120;
/// 桌面通知正文上限。
pub const MAX_NOTIFY_BODY: usize = 400;

/// 单个 Pane 的活动状态，由 `poll_events` 维护。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaneActivity {
    pub progress: Progress,
    /// 有未读通知（Tab 非活动时置位，切到该 Tab 时清除）。
    pub attention: bool,
    /// 进度刚刚清除（用于 Tab 上的完成标记），切 Tab 时清除。
    pub completed: bool,
    /// 后台活动（Tab 非活动时收到标题/进度等语义事件），切 Tab 时清除。
    /// 非活动 Tab 里仍有进行中的进度也会由 `rebuild_tabs` 算作后台活动。
    pub background: bool,
    /// 会话失败（非零退出码 / 被信号终止 / OSC 进度错误），切 Tab 时清除。
    pub failed: bool,
}

/// 一个 Tab 聚合后的活动状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TabActivity {
    pub progress: Progress,
    pub attention: bool,
    pub completed: bool,
    pub background: bool,
    pub failed: bool,
}

/// 多 Pane 时选“最需要被看见”的进度：错误 > 进行中 > 暂停 > 不确定。
fn progress_priority(state: ProgressState) -> u8 {
    match state {
        ProgressState::Clear => 0,
        ProgressState::Indeterminate => 1,
        ProgressState::Paused => 2,
        ProgressState::Normal => 3,
        ProgressState::Error => 4,
    }
}

/// 聚合一个 Tab 内所有 Pane 的状态。空集合返回全默认。
pub fn aggregate<'a>(panes: impl IntoIterator<Item = &'a PaneActivity>) -> TabActivity {
    let mut out = TabActivity::default();
    let mut chosen: Option<(u8, Progress)> = None;
    for pane in panes {
        out.attention |= pane.attention;
        out.completed |= pane.completed;
        out.background |= pane.background;
        out.failed |= pane.failed;
        if pane.progress.state != ProgressState::Clear {
            let priority = progress_priority(pane.progress.state);
            if chosen.is_none_or(|(best, _)| priority > best) {
                chosen = Some((priority, pane.progress));
            }
        }
    }
    if let Some((_, progress)) = chosen {
        out.progress = progress;
    }
    out
}

/// 进度条长度比例（0.0..=1.0）；不确定/缺百分比时给一段固定长度。
pub fn progress_ratio(progress: Progress) -> f32 {
    if progress.state == ProgressState::Clear {
        return 0.0;
    }
    match progress.percent {
        Some(percent) => f32::from(percent.min(100)) / 100.0,
        None => 0.3,
    }
}

/// Tab 上显示的百分比文本；没有合法百分比时为空。
pub fn progress_label(progress: Progress) -> String {
    if progress.state == ProgressState::Clear {
        return String::new();
    }
    progress
        .percent
        .map(|percent| format!("{percent}%"))
        .unwrap_or_default()
}

/// 组装桌面通知的 (title, body)。
///
/// OSC 9 没有标题，回退到 Tab 标题；两者都为空时用 "xtervo"。
pub fn notification_content(tab_title: &str, notification: &Notification) -> (String, String) {
    let title = if notification.title.trim().is_empty() {
        tab_title
    } else {
        notification.title.as_str()
    };
    let title = if title.trim().is_empty() {
        "xtervo"
    } else {
        title
    };
    (
        sanitize_notification_text(title, MAX_NOTIFY_TITLE),
        sanitize_notification_text(&notification.body, MAX_NOTIFY_BODY),
    )
}

/// BEL 的统一通知内容。
pub fn bell_notification() -> Notification {
    Notification {
        title: String::new(),
        body: "Bell".to_string(),
    }
}

/// 清洗通知文本：控制字符（含 NUL/ESC）丢弃，换行/制表符视作空格，
/// 连续空格合并，首尾去空格；超过 `max_chars` 截断并加省略号。
pub fn sanitize_notification_text(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(input.len().min(max_chars * 4));
    let mut chars = 0usize;
    let mut last_space = false;
    let mut truncated = false;
    for ch in input.chars() {
        let ch = match ch {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        if ch == ' ' {
            if out.is_empty() || last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        if chars >= max_chars {
            truncated = true;
            break;
        }
        out.push(ch);
        chars += 1;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    if truncated {
        // 预留一个字符给省略号；max_chars == 1 时只留省略号。
        if max_chars <= 1 {
            out.clear();
        } else {
            out.pop();
            while out.ends_with(' ') {
                out.pop();
            }
        }
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(state: ProgressState, percent: Option<u8>) -> PaneActivity {
        PaneActivity {
            progress: Progress { state, percent },
            ..PaneActivity::default()
        }
    }

    #[test]
    fn aggregate_empty_is_clear() {
        let none: [PaneActivity; 0] = [];
        assert_eq!(aggregate(&none), TabActivity::default());
    }

    #[test]
    fn aggregate_prefers_error_then_progress() {
        let panes = [
            pane(ProgressState::Normal, Some(10)),
            pane(ProgressState::Error, Some(50)),
            pane(ProgressState::Indeterminate, None),
        ];
        let agg = aggregate(&panes);
        assert_eq!(agg.progress.state, ProgressState::Error);
        assert_eq!(agg.progress.percent, Some(50));

        let panes = [
            pane(ProgressState::Clear, None),
            pane(ProgressState::Paused, Some(5)),
            pane(ProgressState::Normal, Some(80)),
        ];
        let agg = aggregate(&panes);
        assert_eq!(agg.progress.state, ProgressState::Normal);
        assert_eq!(agg.progress.percent, Some(80));
    }

    #[test]
    fn aggregate_ors_attention_and_completion() {
        let mut a = pane(ProgressState::Clear, None);
        a.attention = true;
        let mut b = pane(ProgressState::Clear, None);
        b.completed = true;
        let mut c = pane(ProgressState::Clear, None);
        c.background = true;
        let mut d = pane(ProgressState::Clear, None);
        d.failed = true;
        let agg = aggregate(&[a, b, c, d]);
        assert!(agg.attention);
        assert!(agg.completed);
        assert!(agg.background);
        assert!(agg.failed);
        assert_eq!(agg.progress.state, ProgressState::Clear);
    }

    #[test]
    fn progress_ratio_and_label() {
        assert_eq!(progress_ratio(Progress::default()), 0.0);
        assert_eq!(
            progress_ratio(Progress {
                state: ProgressState::Indeterminate,
                percent: None
            }),
            0.3
        );
        assert!(
            (progress_ratio(Progress {
                state: ProgressState::Normal,
                percent: Some(42)
            }) - 0.42)
                .abs()
                < 0.001
        );
        assert_eq!(
            progress_label(Progress {
                state: ProgressState::Normal,
                percent: Some(100)
            }),
            "100%"
        );
        assert_eq!(
            progress_label(Progress {
                state: ProgressState::Normal,
                percent: None
            }),
            ""
        );
        assert_eq!(progress_label(Progress::default()), "");
    }

    #[test]
    fn sanitize_strips_controls_and_normalizes_space() {
        assert_eq!(
            sanitize_notification_text("  a\n\tb\x00c\x1b  d  ", 100),
            "a bc d"
        );
        assert_eq!(sanitize_notification_text("\u{7f}ok", 100), "ok");
        assert_eq!(sanitize_notification_text("   ", 100), "");
    }

    #[test]
    fn sanitize_truncates_with_ellipsis() {
        assert_eq!(sanitize_notification_text("abcdef", 5), "abcd…");
        assert_eq!(sanitize_notification_text("abcdef", 1), "…");
        assert_eq!(sanitize_notification_text("abc", 3), "abc");
        // 中文按字符计数，不按字节
        let long = "中".repeat(10);
        let cut = sanitize_notification_text(&long, 4);
        assert_eq!(cut.chars().count(), 4);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn notification_content_falls_back_to_tab_title() {
        let note = Notification {
            title: String::new(),
            body: "done".into(),
        };
        assert_eq!(
            notification_content("Terminal 2", &note),
            ("Terminal 2".to_string(), "done".to_string())
        );
        let note = Notification {
            title: "Build".into(),
            body: String::new(),
        };
        assert_eq!(notification_content("Terminal 2", &note).0, "Build");
        assert_eq!(
            notification_content("", &Notification::default()).0,
            "xtervo"
        );
        assert_eq!(bell_notification().body, "Bell");
    }
}
