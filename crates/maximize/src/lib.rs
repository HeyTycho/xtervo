// SPDX-License-Identifier: GPL-3.0-only
//! xtervo 最大化状态机。
//!
//! 设计契约见 `docs/MAXIMIZE_ENGINEERING.md`：
//!
//! * `desired` 是用户最后一次意图，也是唯一的 target；
//! * `observed` 只由 `_NET_WM_STATE` 的 PropertyNotify 更新；
//! * `pending` 表示“已发出、尚未被 PropertyNotify 确认”的请求；
//! * 超时只记录 `last_error`，绝不根据猜测改写 `desired`/`pending`；
//! * 不使用几何、`_NET_WORKAREA`、标题查找或定时校准。
//!
//! 纯状态机在 [`controller`]，X11 传输层在 [`x11`]。状态机不依赖 X11，
//! 单元测试可以用注入的 `Instant` 精确验证事件顺序。

mod controller;
pub mod x11;

pub use controller::{
    MaximizeController, MaximizeSnapshot, NotifyOutcome, DEFAULT_REQUEST_TIMEOUT,
};
pub use x11::{MaximizeRuntime, RuntimeOptions};
