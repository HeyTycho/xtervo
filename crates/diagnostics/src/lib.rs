// SPDX-License-Identifier: GPL-3.0-only
//! Backend-neutral diagnostics and observability primitives for Tervo.
//!
//! This crate deliberately has no dependency on wgpu, Skia, Slint, the terminal
//! core or the app. It provides bounded, low-overhead building blocks that the
//! renderer/presenter can feed later:
//!
//! - [`MetricsRegistry`]: lock-free atomic counters and a fixed frame-time
//!   histogram. Recording a frame performs a handful of relaxed atomic
//!   operations and never allocates or takes a lock.
//! - [`TraceBuffer`]: a bounded ring of [`TraceEvent`]s carrying a
//!   [`TraceContext`]. The bound is enforced at construction and on every push;
//!   no queue can grow without limit.
//! - [`ResourceTracker`]: the interface for image resources, placements, glyph
//!   atlas and GPU texture counts/bytes. [`AtomicResourceTracker`] is the
//!   default implementation a CPU/GPU backend can feed now.
//! - [`scene_diff`]: a debug formatter that compares two lightweight
//!   [`SceneSummary`] values without touching wgpu/Slint.
//!
//! Diagnostic snapshots are serde_json values whose fields are additive to the
//! `tervo.diag.v1` schema. There is intentionally no per-character or per-frame
//! logging here; callers must only log bounded summaries/errors.

#![forbid(unsafe_code)]

pub mod metrics;
pub mod resources;
pub mod scene_diff;
pub mod trace;

pub use metrics::{FrameSample, MetricsRegistry, FRAME_TIME_BUCKETS_US};
pub use resources::{
    rgba_bytes, AtomicResourceTracker, ResourceCount, ResourceSnapshot, ResourceTracker,
};
pub use scene_diff::{DamageRect, DamageSummary, ImageSummary, SceneDiff, SceneSummary};
pub use trace::{
    TraceBuffer, TraceContext, TraceEvent, TraceKind, TraceSnapshot, DEFAULT_TRACE_CAPACITY,
    MAX_TRACE_CAPACITY,
};
