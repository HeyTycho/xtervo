// SPDX-License-Identifier: GPL-3.0-only
//! Bounded trace ring buffer.
//!
//! Trace events are low-frequency diagnostics (damage, image placement,
//! texture create/destroy, stale frames, errors), never per-cell or per-frame
//! info logs. The ring has a hard capacity fixed at construction; pushing into
//! a full ring drops the oldest event and increments `dropped`. Memory beyond
//! the fixed event slots only grows when a caller explicitly attaches a
//! message, and even that is bounded by the ring capacity.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default ring capacity. Small enough for a debug dump, large enough to see
/// a handful of recent transitions.
pub const DEFAULT_TRACE_CAPACITY: usize = 256;

/// Hard upper bound accepted by [`TraceBuffer::new`]. Requested capacities are
/// clamped into `1..=MAX_TRACE_CAPACITY`.
pub const MAX_TRACE_CAPACITY: usize = 4_096;

/// Maximum chars retained for one optional trace message. Longer messages are
/// truncated with an ellipsis so a single event cannot defeat the ring bound.
pub const MAX_TRACE_MESSAGE_CHARS: usize = 512;

/// Correlation ids for one observable update.
///
/// `placement_id` is the numeric `PlacementId` from `tervo-graphics`; the
/// diagnostics crate keeps it as `u64` so it stays dependency-free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub session_id: u64,
    pub update_id: u64,
    pub scene_revision: u64,
    pub frame_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    Frame,
    Damage,
    ImagePlace,
    ImageRemove,
    TextureCreate,
    TextureDestroy,
    PresenterUpdate,
    StaleFrame,
    Error,
    Custom,
}

impl TraceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Frame => "frame",
            Self::Damage => "damage",
            Self::ImagePlace => "image_place",
            Self::ImageRemove => "image_remove",
            Self::TextureCreate => "texture_create",
            Self::TextureDestroy => "texture_destroy",
            Self::PresenterUpdate => "presenter_update",
            Self::StaleFrame => "stale_frame",
            Self::Error => "error",
            Self::Custom => "custom",
        }
    }
}

/// One bounded trace event.
///
/// `value` is the numeric payload whose meaning depends on `kind` (e.g. damage
/// regions, byte count, frame id). `code` is a small caller-defined status
/// (0 = none). `message` should only be used for bounded error/custom text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub seq: u64,
    pub ts_us: u64,
    pub kind: TraceKind,
    pub context: TraceContext,
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub value: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Copy of the ring contents at one point in time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceSnapshot {
    pub capacity: usize,
    /// Slots actually allocated by the deque. Never exceeds `capacity`.
    pub allocated_slots: usize,
    pub len: usize,
    pub dropped: u64,
    pub next_seq: u64,
    pub events: Vec<TraceEvent>,
}

#[derive(Debug)]
struct RingInner {
    events: VecDeque<TraceEvent>,
    dropped: u64,
    next_seq: u64,
}

/// Bounded, thread-safe trace ring.
#[derive(Debug)]
pub struct TraceBuffer {
    capacity: usize,
    started: Instant,
    inner: Mutex<RingInner>,
}

impl Default for TraceBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_TRACE_CAPACITY)
    }
}

impl TraceBuffer {
    /// Create a ring with `capacity` slots (clamped to `1..=MAX_TRACE_CAPACITY`).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.clamp(1, MAX_TRACE_CAPACITY);
        Self {
            capacity,
            started: Instant::now(),
            inner: Mutex::new(RingInner {
                events: VecDeque::with_capacity(capacity),
                dropped: 0,
                next_seq: 1,
            }),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.lock().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().events.is_empty()
    }

    /// Number of events evicted because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// Record an event and return its 1-based sequence number.
    pub fn record(&self, kind: TraceKind, context: TraceContext, value: u64) -> u64 {
        self.push(kind, context, 0, value, None)
    }

    /// Record an event with a numeric status/code and no message.
    pub fn record_code(
        &self,
        kind: TraceKind,
        context: TraceContext,
        code: i64,
        value: u64,
    ) -> u64 {
        self.push(kind, context, code, value, None)
    }

    /// Record a bounded error/custom message. Allocations happen only here,
    /// and the number of retained messages is still capped by the ring.
    pub fn record_message(
        &self,
        kind: TraceKind,
        context: TraceContext,
        code: i64,
        value: u64,
        message: impl Into<String>,
    ) -> u64 {
        self.push(kind, context, code, value, Some(bounded_message(message.into())))
    }

    fn push(
        &self,
        kind: TraceKind,
        context: TraceContext,
        code: i64,
        value: u64,
        message: Option<String>,
    ) -> u64 {
        let mut inner = self.lock();
        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.wrapping_add(1);
        let ts_us = self.started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        if inner.events.len() == self.capacity {
            inner.events.pop_front();
            inner.dropped = inner.dropped.saturating_add(1);
        }
        inner.events.push_back(TraceEvent {
            seq,
            ts_us,
            kind,
            context,
            code,
            value,
            message,
        });
        seq
    }

    /// Snapshot events oldest -> newest. Allocation is proportional to the
    /// fixed capacity, never to total recorded events.
    pub fn snapshot(&self) -> TraceSnapshot {
        let inner = self.lock();
        TraceSnapshot {
            capacity: self.capacity,
            allocated_slots: inner.events.capacity(),
            len: inner.events.len(),
            dropped: inner.dropped,
            next_seq: inner.next_seq,
            events: inner.events.iter().cloned().collect(),
        }
    }

    /// Additive `tervo.diag.v1` trace object.
    pub fn snapshot_json(&self) -> Value {
        let snapshot = self.snapshot();
        serde_json::to_value(snapshot).unwrap_or(Value::Null)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RingInner> {
        // Diagnostics must not panic on a poisoned ring; the data is only
        // debug state, so recovering the inner value is the safer behavior.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn bounded_message(message: String) -> String {
    if message.chars().count() <= MAX_TRACE_MESSAGE_CHARS {
        return message;
    }
    let mut truncated: String = message
        .chars()
        .take(MAX_TRACE_MESSAGE_CHARS.saturating_sub(1))
        .collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(frame_id: u64) -> TraceContext {
        TraceContext {
            session_id: 1,
            update_id: frame_id,
            scene_revision: frame_id,
            frame_id,
            placement_id: None,
        }
    }

    #[test]
    fn ring_buffer_is_bounded_and_keeps_newest() {
        let trace = TraceBuffer::new(4);
        for i in 1..=10 {
            trace.record(TraceKind::Frame, ctx(i), i);
        }
        let snapshot = trace.snapshot();
        assert_eq!(snapshot.capacity, 4);
        assert_eq!(snapshot.len, 4);
        assert_eq!(snapshot.dropped, 6);
        assert_eq!(snapshot.allocated_slots, 4);
        let seqs: Vec<u64> = snapshot.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![7, 8, 9, 10]);
        assert_eq!(snapshot.next_seq, 11);
    }

    #[test]
    fn ring_buffer_never_grows_past_capacity() {
        let trace = TraceBuffer::new(8);
        for i in 0..50_000 {
            trace.record(TraceKind::Damage, ctx(i), i % 3);
        }
        let snapshot = trace.snapshot();
        assert_eq!(snapshot.len, 8);
        assert_eq!(snapshot.dropped, 49_992);
        assert!(
            snapshot.allocated_slots <= 8,
            "allocated {} slots",
            snapshot.allocated_slots
        );
    }

    #[test]
    fn concurrent_writers_stay_within_bound() {
        let trace = std::sync::Arc::new(TraceBuffer::new(64));
        let mut handles = Vec::new();
        for thread in 0..8u64 {
            let trace = trace.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..1_000u64 {
                    trace.record(TraceKind::Custom, ctx(thread * 1_000 + i), i);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let snapshot = trace.snapshot();
        assert_eq!(snapshot.len, 64);
        assert_eq!(snapshot.dropped + snapshot.len as u64, 8_000);
        assert_eq!(snapshot.allocated_slots, 64);
        let seqs: Vec<u64> = snapshot.events.iter().map(|e| e.seq).collect();
        assert!(
            seqs.windows(2).all(|pair| pair[0] < pair[1]),
            "seqs: {seqs:?}"
        );
    }

    #[test]
    fn long_messages_are_truncated() {
        let trace = TraceBuffer::new(1);
        trace.record_message(TraceKind::Error, ctx(1), 0, 0, "x".repeat(2_000));
        let message = trace.snapshot().events[0].message.clone().unwrap();
        assert_eq!(message.chars().count(), MAX_TRACE_MESSAGE_CHARS);
        assert!(message.ends_with('…'));
    }

    #[test]
    fn snapshot_json_carries_context_and_is_additive() {
        let trace = TraceBuffer::new(2);
        trace.record_message(
            TraceKind::Error,
            TraceContext {
                session_id: 9,
                update_id: 10,
                scene_revision: 11,
                frame_id: 12,
                placement_id: Some(13),
            },
            -1,
            0,
            "bounded error",
        );
        let value = trace.snapshot_json();
        assert_eq!(value["capacity"], 2);
        assert_eq!(value["len"], 1);
        assert_eq!(value["events"][0]["kind"], "error");
        assert_eq!(value["events"][0]["context"]["placement_id"], 13);
        assert_eq!(value["events"][0]["message"], "bounded error");
    }
}
