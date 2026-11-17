// SPDX-License-Identifier: GPL-3.0-only
//! Tervo 终端核心（Phase 0 可行性验证）。
//!
//! 架构要点（已冻结）：
//! - 每个 pane 一个 [`alacritty_terminal::term::Term`]，跑在专属 actor 线程；
//! - 终端核心只通过**有界**通道把事件推给 UI；
//! - UI 侧主动**拉取**网格快照（脏行优先），不由核心推送整帧。

pub mod bench_support;
pub mod event;
pub mod frame;
pub mod iip;
pub mod keyboard;
pub mod kitty_graphics;
pub mod link;
pub mod mouse;
pub mod osc;
pub mod palette;
pub mod pane;
pub mod sixel;
pub mod transport;

pub use event::{
    ClipboardRequest, ClipboardSelection, CoreEvent, EventSink, Notification, PaneStats, Progress,
    ProgressState, ShellMark, WakeSignal,
};
pub use frame::{
    flags, CellData, CursorShape, CursorState, Frame, ImageCrop, ImagePlacementMeta, InlineImage, LineDamage, Span,
};
pub use keyboard::{KeyboardFlags, MODIFY_OTHER_KEYS_MAX_LEVEL};
pub use link::{is_allowed_uri, Link, LinkKind};
pub use mouse::{Modifiers, MouseAction, MouseButton, MouseEvent};
pub use bench_support::TermSim;
pub use palette::{Palette, PaletteSpec, Theme, UiColors};
pub use pane::fill_frame;
pub use pane::{
    spawn, spawn_with_transport, Backend, GridSize, OutputFilter, OutputFilterHandle, PaneConfig,
    PaneHandle, ScrollPosition, SearchHit, SearchOptions, MAX_OUTPUT_FILTER_BYTES,
};
pub use sixel::{SixelEvent, SixelFilter, SixelImage};
pub use iip::{IipEvent, IipFilter, IipImage};
