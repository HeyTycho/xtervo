// SPDX-License-Identifier: GPL-3.0-only
//! Atomic metrics registry.
//!
//! The recording path is allocation-free and lock-free: all counters are
//! `AtomicU64` with `Relaxed` ordering. A snapshot is expected to be rare
//! (CLI/MCP/diagnostics), so it may allocate JSON freely.
//!
//! Frame time is kept as a fixed histogram instead of an exact percentile.
//! `p95_us` is the upper bound of the first bucket whose cumulative count
//! reaches `ceil(0.95 * samples)`, so it is an upper-bound estimate. Values
//! beyond the last bucket are counted in `overflow_samples` and reported with
//! `p95_overflow=true`.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

/// Fixed upper bounds for the CPU frame-time histogram, in microseconds.
///
/// Twelve buckets cover a common 60 Hz frame budget (16.6 ms) and slow frames
/// up to one second without allocating.
pub const FRAME_TIME_BUCKETS_US: [u64; 12] = [
    500, 1_000, 2_000, 4_000, 8_000, 16_000, 33_000, 50_000, 100_000, 250_000, 500_000, 1_000_000,
];

const PPM: u64 = 1_000_000;

/// One frame's worth of cheap-to-record values.
///
/// The caller computes `damage_area`/`viewport_area` from whatever units it
/// already has (cells or pixels); the registry only stores the ratio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameSample {
    /// CPU time spent producing the frame, in microseconds.
    pub cpu_time_us: u64,
    /// Number of damage regions in this frame.
    pub damage_regions: u64,
    /// Area covered by damage in this frame (caller units, normally cells).
    pub damage_area: u64,
    /// Total damageable area (caller units); 0 means "unknown".
    pub viewport_area: u64,
    /// Monotonic scene revision presented by this frame.
    pub scene_revision: u64,
}

/// Lock-free, bounded metrics registry.
#[derive(Debug)]
pub struct MetricsRegistry {
    frame_count: AtomicU64,
    frame_time_last_us: AtomicU64,
    frame_time_sum_us: AtomicU64,
    frame_time_samples: AtomicU64,
    frame_time_buckets: [AtomicU64; FRAME_TIME_BUCKETS_US.len()],
    frame_time_overflow: AtomicU64,
    damage_regions_last: AtomicU64,
    damage_regions_total: AtomicU64,
    damage_area_last: AtomicU64,
    damage_area_total: AtomicU64,
    damage_viewport_area_last: AtomicU64,
    damage_ratio_ppm_last: AtomicU64,
    damage_ratio_ppm_max: AtomicU64,
    scene_revision: AtomicU64,
    image_instance_count: AtomicU64,
    image_placement_count: AtomicU64,
    textures_created: AtomicU64,
    textures_destroyed: AtomicU64,
    texture_bytes_live: AtomicU64,
    texture_bytes_peak: AtomicU64,
    presenter_revision: AtomicU64,
    stale_frames: AtomicU64,
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            frame_count: AtomicU64::new(0),
            frame_time_last_us: AtomicU64::new(0),
            frame_time_sum_us: AtomicU64::new(0),
            frame_time_samples: AtomicU64::new(0),
            frame_time_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            frame_time_overflow: AtomicU64::new(0),
            damage_regions_last: AtomicU64::new(0),
            damage_regions_total: AtomicU64::new(0),
            damage_area_last: AtomicU64::new(0),
            damage_area_total: AtomicU64::new(0),
            damage_viewport_area_last: AtomicU64::new(0),
            damage_ratio_ppm_last: AtomicU64::new(0),
            damage_ratio_ppm_max: AtomicU64::new(0),
            scene_revision: AtomicU64::new(0),
            image_instance_count: AtomicU64::new(0),
            image_placement_count: AtomicU64::new(0),
            textures_created: AtomicU64::new(0),
            textures_destroyed: AtomicU64::new(0),
            texture_bytes_live: AtomicU64::new(0),
            texture_bytes_peak: AtomicU64::new(0),
            presenter_revision: AtomicU64::new(0),
            stale_frames: AtomicU64::new(0),
        }
    }

    /// Record one rendered/presented frame.
    ///
    /// This is the hot path: no allocation, no lock, only relaxed atomics.
    pub fn record_frame(&self, sample: FrameSample) {
        self.frame_count.fetch_add(1, Ordering::Relaxed);
        self.frame_time_samples.fetch_add(1, Ordering::Relaxed);
        self.frame_time_last_us
            .store(sample.cpu_time_us, Ordering::Relaxed);
        self.frame_time_sum_us
            .fetch_add(sample.cpu_time_us, Ordering::Relaxed);
        match frame_time_bucket(sample.cpu_time_us) {
            Some(index) => {
                self.frame_time_buckets[index].fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.frame_time_overflow.fetch_add(1, Ordering::Relaxed);
            }
        }

        self.damage_regions_last
            .store(sample.damage_regions, Ordering::Relaxed);
        self.damage_regions_total
            .fetch_add(sample.damage_regions, Ordering::Relaxed);
        self.damage_area_last
            .store(sample.damage_area, Ordering::Relaxed);
        self.damage_area_total
            .fetch_add(sample.damage_area, Ordering::Relaxed);
        self.damage_viewport_area_last
            .store(sample.viewport_area, Ordering::Relaxed);
        let ratio_ppm = damage_ratio_ppm(sample.damage_area, sample.viewport_area);
        self.damage_ratio_ppm_last
            .store(ratio_ppm, Ordering::Relaxed);
        self.damage_ratio_ppm_max
            .fetch_max(ratio_ppm, Ordering::Relaxed);
        self.scene_revision
            .fetch_max(sample.scene_revision, Ordering::Relaxed);
    }

    /// A pull that returned a skipped/stale frame. These are not frame_count
    /// entries because no frame was presented.
    pub fn record_stale_frame(&self) {
        self.stale_frames.fetch_add(1, Ordering::Relaxed);
    }

    /// Latest scene revision. Monotonic; stale updates never move it backwards.
    pub fn set_scene_revision(&self, revision: u64) {
        self.scene_revision.fetch_max(revision, Ordering::Relaxed);
    }

    /// Current image instance count in the scene and placement count in the
    /// graphics manager. Gauges, not totals.
    pub fn set_image_counts(&self, instances: u64, placements: u64) {
        self.image_instance_count
            .store(instances, Ordering::Relaxed);
        self.image_placement_count
            .store(placements, Ordering::Relaxed);
    }

    /// Latest revision handed to the presenter. Monotonic.
    pub fn set_presenter_revision(&self, revision: u64) {
        self.presenter_revision
            .fetch_max(revision, Ordering::Relaxed);
    }

    /// Estimate: one texture was created/uploaded.
    pub fn record_texture_created(&self, bytes: u64) {
        self.textures_created.fetch_add(1, Ordering::Relaxed);
        let live = self
            .texture_bytes_live
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        self.texture_bytes_peak.fetch_max(live, Ordering::Relaxed);
    }

    /// Estimate: one texture was destroyed. Saturates at zero so a double-free
    /// or a tracker bug cannot wrap the live-bytes gauge.
    pub fn record_texture_destroyed(&self, bytes: u64) {
        self.textures_destroyed.fetch_add(1, Ordering::Relaxed);
        let _ =
            self.texture_bytes_live
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_sub(bytes))
                });
    }

    /// Additive `tervo.diag.v1` metrics object. Callers wrap it with
    /// `schema`/`kind`; the fields here are stable but additive.
    pub fn snapshot(&self) -> Value {
        let samples = self.frame_time_samples.load(Ordering::Relaxed);
        let sum_us = self.frame_time_sum_us.load(Ordering::Relaxed);
        let avg_us = if samples == 0 { 0 } else { sum_us / samples };
        let overflow = self.frame_time_overflow.load(Ordering::Relaxed);

        let mut histogram = Vec::with_capacity(FRAME_TIME_BUCKETS_US.len());
        let mut cumulative = 0u64;
        let mut p95 = Value::Null;
        let mut p95_bucket: Option<usize> = None;
        let target = if samples == 0 {
            0
        } else {
            samples.saturating_mul(95).div_ceil(100).max(1)
        };
        for (index, upper_us) in FRAME_TIME_BUCKETS_US.iter().copied().enumerate() {
            let count = self.frame_time_buckets[index].load(Ordering::Relaxed);
            cumulative = cumulative.saturating_add(count);
            if samples > 0 && p95_bucket.is_none() && cumulative >= target {
                p95_bucket = Some(index);
                p95 = json!(upper_us);
            }
            histogram.push(json!({
                "upper_us": upper_us,
                "count": count,
            }));
        }
        // Values beyond the last bound are approximate too, but cannot be
        // represented by a finite bucket. Report the lower bound and flag it.
        let p95_overflow = samples > 0 && p95_bucket.is_none();
        if p95_overflow {
            p95 = json!(FRAME_TIME_BUCKETS_US[FRAME_TIME_BUCKETS_US.len() - 1]);
        }
        let ratio_ppm_last = self.damage_ratio_ppm_last.load(Ordering::Relaxed);
        let ratio_ppm_max = self.damage_ratio_ppm_max.load(Ordering::Relaxed);
        let created = self.textures_created.load(Ordering::Relaxed);
        let destroyed = self.textures_destroyed.load(Ordering::Relaxed);

        json!({
            "frame_count": self.frame_count.load(Ordering::Relaxed),
            "frame_time": {
                "last_us": self.frame_time_last_us.load(Ordering::Relaxed),
                "avg_us": avg_us,
                // Bucket upper bound, `null` until the first sample.
                "p95_us": p95,
                "p95_overflow": p95_overflow,
                "samples": samples,
                "overflow_samples": overflow,
                "histogram": histogram,
            },
            "damage": {
                "regions_last": self.damage_regions_last.load(Ordering::Relaxed),
                "regions_total": self.damage_regions_total.load(Ordering::Relaxed),
                "area_last": self.damage_area_last.load(Ordering::Relaxed),
                "area_total": self.damage_area_total.load(Ordering::Relaxed),
                "viewport_area_last": self.damage_viewport_area_last.load(Ordering::Relaxed),
                "area_ratio_last_ppm": ratio_ppm_last,
                "area_ratio_last": ratio_ppm_last as f64 / PPM as f64,
                "area_ratio_max_ppm": ratio_ppm_max,
                "area_ratio_max": ratio_ppm_max as f64 / PPM as f64,
            },
            "scene_revision": self.scene_revision.load(Ordering::Relaxed),
            "images": {
                "instances": self.image_instance_count.load(Ordering::Relaxed),
                "placements": self.image_placement_count.load(Ordering::Relaxed),
            },
            "textures": {
                "created": created,
                "destroyed": destroyed,
                "live_count_estimate": created.saturating_sub(destroyed),
                "live_bytes_estimate": self.texture_bytes_live.load(Ordering::Relaxed),
                "peak_bytes_estimate": self.texture_bytes_peak.load(Ordering::Relaxed),
            },
            "presenter_revision": self.presenter_revision.load(Ordering::Relaxed),
            "stale_frames": self.stale_frames.load(Ordering::Relaxed),
        })
    }
}

fn frame_time_bucket(micros: u64) -> Option<usize> {
    let index = FRAME_TIME_BUCKETS_US.partition_point(|upper| *upper < micros);
    (index < FRAME_TIME_BUCKETS_US.len()).then_some(index)
}

fn damage_ratio_ppm(area: u64, viewport: u64) -> u64 {
    if viewport == 0 {
        return 0;
    }
    area.min(viewport).saturating_mul(PPM) / viewport
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        cpu_time_us: u64,
        damage_regions: u64,
        damage_area: u64,
        viewport: u64,
        rev: u64,
    ) -> FrameSample {
        FrameSample {
            cpu_time_us,
            damage_regions,
            damage_area,
            viewport_area: viewport,
            scene_revision: rev,
        }
    }

    #[test]
    fn snapshot_has_expected_fields_and_defaults() {
        let metrics = MetricsRegistry::new();
        let value = metrics.snapshot();
        assert_eq!(value["frame_count"], 0);
        assert_eq!(value["frame_time"]["samples"], 0);
        assert_eq!(value["frame_time"]["p95_us"], Value::Null);
        assert_eq!(value["damage"]["regions_last"], 0);
        assert_eq!(value["images"]["placements"], 0);
        assert!(value["textures"]["live_bytes_estimate"].is_number());
        assert_eq!(value["presenter_revision"], 0);
    }

    #[test]
    fn recorded_frame_updates_counters_and_percentile_bucket() {
        let metrics = MetricsRegistry::new();
        for _ in 0..100 {
            metrics.record_frame(sample(1_000, 3, 50, 4_000, 7));
        }
        metrics.record_frame(sample(20_000, 1, 4_000, 4_000, 9));
        let value = metrics.snapshot();
        assert_eq!(value["frame_count"], 101);
        assert_eq!(value["frame_time"]["samples"], 101);
        assert_eq!(value["frame_time"]["last_us"], 20_000);
        assert_eq!(value["frame_time"]["p95_us"], 1_000);
        assert_eq!(value["frame_time"]["p95_overflow"], false);
        assert_eq!(value["damage"]["regions_last"], 1);
        assert_eq!(value["damage"]["area_ratio_last_ppm"], 1_000_000);
        assert_eq!(value["scene_revision"], 9);
    }

    #[test]
    fn texture_bytes_are_saturating_and_peak_is_tracked() {
        let metrics = MetricsRegistry::new();
        metrics.record_texture_created(1_000);
        metrics.record_texture_created(500);
        metrics.record_texture_destroyed(600);
        metrics.record_texture_destroyed(9_999);
        let value = metrics.snapshot();
        assert_eq!(value["textures"]["created"], 2);
        assert_eq!(value["textures"]["destroyed"], 2);
        assert_eq!(value["textures"]["live_bytes_estimate"], 0);
        assert_eq!(value["textures"]["peak_bytes_estimate"], 1_500);
        assert_eq!(value["textures"]["live_count_estimate"], 0);
    }

    #[test]
    fn concurrent_recording_loses_no_frames() {
        let metrics = std::sync::Arc::new(MetricsRegistry::new());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let metrics = metrics.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..1_000 {
                    metrics.record_frame(sample(500, 1, 1, 2, 1));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(metrics.snapshot()["frame_count"], 4_000);
        assert_eq!(metrics.snapshot()["frame_time"]["samples"], 4_000);
    }

    #[test]
    fn stale_frames_do_not_increment_frame_count() {
        let metrics = MetricsRegistry::new();
        metrics.record_stale_frame();
        metrics.record_stale_frame();
        let value = metrics.snapshot();
        assert_eq!(value["stale_frames"], 2);
        assert_eq!(value["frame_count"], 0);
    }

    #[test]
    fn p95_overflow_is_flagged() {
        let metrics = MetricsRegistry::new();
        metrics.record_frame(sample(2_000_000, 0, 0, 0, 0));
        let value = metrics.snapshot();
        assert_eq!(value["frame_time"]["p95_overflow"], true);
        assert_eq!(value["frame_time"]["overflow_samples"], 1);
    }
}
