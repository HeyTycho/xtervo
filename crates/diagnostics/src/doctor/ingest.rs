// SPDX-License-Identifier: GPL-3.0-only
//! Adapters from existing `tervo.diag.v1` JSON and process env.
//!
//! These adapters are intentionally strict about types and deliberately do not
//! fill missing fields. A missing metric is `None`; a CLI placeholder metrics
//! document remains marked `initialized=false`/`source=cli` and is never
//! interpreted as a healthy zero. Frame text is capped to a viewport-sized
//! excerpt; scrollback is never read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{Environment, GpuInfo, RendererInfo};
use super::redact::redact_text;
use crate::scene_diff::{DamageRect, DamageSummary};

const MAX_FRAME_ROWS: usize = 40;
const MAX_FRAME_COLS: usize = 200;
const MAX_FRAME_DAMAGE_RECTS: usize = 64;
const MAX_FRAME_IMAGES: usize = 32;
const MAX_RENDER_PROFILE_BYTES: usize = 32 * 1024;
const MAX_RENDER_PROFILE_STAGES: usize = 32;
const MAX_ENV_VALUE_CHARS: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestError {
    NotObject(&'static str),
    UnsupportedSchema(String),
    Missing(&'static str),
    WrongType(&'static str),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotObject(context) => write!(f, "{context} 不是 JSON object"),
            Self::UnsupportedSchema(schema) => write!(f, "不支持的 schema: {schema}"),
            Self::Missing(field) => write!(f, "缺少字段: {field}"),
            Self::WrongType(field) => write!(f, "字段类型错误: {field}"),
        }
    }
}

impl std::error::Error for IngestError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorSummary {
    pub row: Option<u64>,
    pub col: Option<u64>,
    pub visible: Option<bool>,
    pub shape: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamageInfo {
    /// `None` when the input omitted `damage.full`; never defaulted to false.
    pub full: Option<bool>,
    pub reported_regions: Option<u64>,
    pub rects: Vec<DamageRect>,
    pub rects_truncated: bool,
    /// Only computed when `full` was present. Partial when rects were capped.
    pub area_cells: Option<u64>,
    pub coverage_ppm: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRect {
    pub id: Option<u64>,
    pub row: Option<i64>,
    pub col: Option<u64>,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

/// Bounded viewport summary parsed from a `tervo diag snapshot`/`trace` frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSummary {
    pub schema: Option<String>,
    pub kind: Option<String>,
    pub cols: usize,
    pub rows: usize,
    pub generation: Option<u64>,
    pub cursor: Option<CursorSummary>,
    pub mode: Option<u32>,
    pub display_offset: Option<u64>,
    pub skipped: Option<bool>,
    pub damage: Option<DamageInfo>,
    pub images: Vec<ImageRect>,
    pub image_count: usize,
    pub text: Vec<String>,
    pub text_rows_total: usize,
    pub text_truncated: bool,
}

impl FrameSummary {
    pub fn from_diag_json(value: &Value) -> Result<Self, IngestError> {
        Self::from_diag_json_with_limits(value, MAX_FRAME_ROWS, MAX_FRAME_COLS)
    }

    pub fn from_diag_json_with_limits(
        value: &Value,
        max_rows: usize,
        max_cols: usize,
    ) -> Result<Self, IngestError> {
        require_diag_object(value, "frame")?;
        let cols = required_u64(value, "cols")? as usize;
        let rows = required_u64(value, "rows")? as usize;
        let viewport = (cols as u64).saturating_mul(rows as u64);
        let text = value
            .get("text")
            .ok_or(IngestError::Missing("text"))?
            .as_array()
            .ok_or(IngestError::WrongType("text"))?;
        let text_rows_total = text.len();
        let max_rows = max_rows.max(1);
        let max_cols = max_cols.max(1);
        let mut text_truncated = text_rows_total > max_rows;
        let mut lines = Vec::with_capacity(text_rows_total.min(max_rows));
        for row in text.iter().take(max_rows) {
            let line = row.as_str().ok_or(IngestError::WrongType("text[]"))?;
            let (line, redacted) = redact_text(line);
            let _ = redacted;
            if line.chars().count() > max_cols {
                text_truncated = true;
                lines.push(line.chars().take(max_cols).collect());
            } else {
                lines.push(line);
            }
        }

        let cursor = value
            .get("cursor")
            .and_then(Value::as_object)
            .map(|cursor| CursorSummary {
                row: cursor.get("row").and_then(Value::as_u64),
                col: cursor.get("col").and_then(Value::as_u64),
                visible: cursor.get("visible").and_then(Value::as_bool),
                shape: cursor.get("shape").and_then(Value::as_str).map(clean),
            });
        let damage = value
            .get("damage")
            .map(|damage| parse_damage(damage, viewport))
            .transpose()?;
        let images_array = value.get("images").and_then(Value::as_array);
        let image_count = images_array.map(Vec::len).unwrap_or(0);
        let mut images = Vec::new();
        if let Some(array) = images_array {
            for image in array.iter().take(MAX_FRAME_IMAGES) {
                let Some(object) = image.as_object() else {
                    continue;
                };
                images.push(ImageRect {
                    id: object.get("id").and_then(Value::as_u64),
                    row: object.get("row").and_then(Value::as_i64),
                    col: object.get("col").and_then(Value::as_u64),
                    width: object.get("width").and_then(Value::as_u64),
                    height: object.get("height").and_then(Value::as_u64),
                });
            }
        }

        Ok(Self {
            schema: value.get("schema").and_then(Value::as_str).map(clean),
            kind: value.get("kind").and_then(Value::as_str).map(clean),
            cols,
            rows,
            generation: value.get("generation").and_then(Value::as_u64),
            cursor,
            mode: value
                .get("mode")
                .and_then(Value::as_u64)
                .map(|mode| mode.min(u32::MAX as u64) as u32),
            display_offset: value.get("display_offset").and_then(Value::as_u64),
            skipped: value.get("skipped").and_then(Value::as_bool),
            damage,
            images,
            image_count,
            text: lines,
            text_rows_total,
            text_truncated,
        })
    }
}

fn parse_damage(value: &Value, viewport: u64) -> Result<DamageInfo, IngestError> {
    let object = value.as_object().ok_or(IngestError::WrongType("damage"))?;
    let full = object.get("full").and_then(Value::as_bool);
    let reported_regions = object.get("region_count").and_then(Value::as_u64);
    let mut rects = Vec::new();
    let mut rects_truncated = false;
    if let Some(array) = object.get("rects").and_then(Value::as_array) {
        rects_truncated = array.len() > MAX_FRAME_DAMAGE_RECTS;
        for rect in array.iter().take(MAX_FRAME_DAMAGE_RECTS) {
            let x = rect
                .get("x")
                .and_then(Value::as_u64)
                .ok_or(IngestError::WrongType("damage.rects[].x"))?;
            let y = rect
                .get("y")
                .and_then(Value::as_u64)
                .ok_or(IngestError::WrongType("damage.rects[].y"))?;
            let w = rect
                .get("w")
                .and_then(Value::as_u64)
                .ok_or(IngestError::WrongType("damage.rects[].w"))?;
            let h = rect
                .get("h")
                .and_then(Value::as_u64)
                .ok_or(IngestError::WrongType("damage.rects[].h"))?;
            rects.push(DamageRect::new(
                x.min(u32::MAX as u64) as u32,
                y.min(u32::MAX as u64) as u32,
                w.min(u32::MAX as u64) as u32,
                h.min(u32::MAX as u64) as u32,
            ));
        }
    }
    let (area_cells, coverage_ppm) = match full {
        Some(full) => {
            let summary = DamageSummary {
                full,
                rects: rects.clone(),
            };
            (
                Some(summary.area(viewport)),
                Some(summary.coverage_ppm(viewport)),
            )
        }
        None => (None, None),
    };
    Ok(DamageInfo {
        full,
        reported_regions,
        rects,
        rects_truncated,
        area_cells,
        coverage_ppm,
    })
}

/// Bounded metrics summary. Zeros are only meaningful when `initialized=true`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub initialized: Option<bool>,
    pub source: Option<String>,
    pub frame_count: Option<u64>,
    pub p95_us: Option<u64>,
    pub p95_overflow: Option<bool>,
    pub sample_count: Option<u64>,
    pub damage_ratio_last_ppm: Option<u64>,
    pub damage_ratio_max_ppm: Option<u64>,
    pub stale_frames: Option<u64>,
    pub texture_bytes_live: Option<u64>,
    pub image_placements: Option<u64>,
}

impl MetricsSummary {
    pub fn from_diag_json(value: &Value) -> Result<Self, IngestError> {
        require_diag_object(value, "metrics")?;
        let metrics = value.get("metrics").unwrap_or(value);
        let frame_time = metrics.get("frame_time");
        let damage = metrics.get("damage");
        let textures = metrics.get("textures");
        Ok(Self {
            initialized: value
                .get("initialized")
                .or_else(|| metrics.get("initialized"))
                .and_then(Value::as_bool),
            source: value
                .get("source")
                .or_else(|| metrics.get("source"))
                .and_then(Value::as_str)
                .map(clean),
            frame_count: metrics.get("frame_count").and_then(Value::as_u64),
            p95_us: frame_time
                .and_then(|frame_time| frame_time.get("p95_us"))
                .and_then(Value::as_u64),
            p95_overflow: frame_time
                .and_then(|frame_time| frame_time.get("p95_overflow"))
                .and_then(Value::as_bool),
            sample_count: frame_time
                .and_then(|frame_time| frame_time.get("samples"))
                .and_then(Value::as_u64),
            damage_ratio_last_ppm: damage
                .and_then(|damage| damage.get("area_ratio_last_ppm"))
                .and_then(Value::as_u64),
            damage_ratio_max_ppm: damage
                .and_then(|damage| damage.get("area_ratio_max_ppm"))
                .and_then(Value::as_u64),
            stale_frames: metrics.get("stale_frames").and_then(Value::as_u64),
            texture_bytes_live: textures
                .and_then(|textures| textures.get("live_bytes_estimate"))
                .and_then(Value::as_u64),
            image_placements: metrics
                .get("images")
                .and_then(|images| images.get("placements"))
                .and_then(Value::as_u64),
        })
    }

    /// `tervo diag metrics` is a schema probe with an empty registry, not live
    /// app telemetry. Never judge health from it.
    pub fn is_placeholder(&self) -> bool {
        self.initialized == Some(false) || self.source.as_deref() == Some("cli")
    }

    pub fn is_live(&self) -> bool {
        self.initialized == Some(true) && !self.is_placeholder()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageStats {
    pub samples: u64,
    pub min: f64,
    pub max: f64,
    pub avg: f64,
    pub unit: String,
}

/// Bounded render-profile excerpt plus numeric `key=value(unit)` aggregation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderProfileSummary {
    pub source: String,
    pub enabled: Option<bool>,
    pub line_count: usize,
    pub lines_truncated: bool,
    pub stages: BTreeMap<String, StageStats>,
    pub stages_truncated: bool,
    pub excerpt: String,
}

impl RenderProfileSummary {
    pub fn from_text(
        source: &str,
        text: &str,
        enabled: Option<bool>,
        max_bytes: Option<usize>,
    ) -> Self {
        let max_bytes = max_bytes.unwrap_or(MAX_RENDER_PROFILE_BYTES).max(64);
        let lines_truncated = text.len() > max_bytes;
        let bounded = take_bytes(text, max_bytes);
        let line_count = bounded.lines().count();
        let mut accum: BTreeMap<String, (u64, f64, f64, f64, String)> = BTreeMap::new();
        for token in bounded.split_whitespace() {
            let token = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''
                )
            });
            let Some((key, raw_value)) = token.split_once('=') else {
                continue;
            };
            if key.is_empty()
                || key.len() > 64
                || !key
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
            {
                continue;
            }
            if !key_looks_like_timing(key) {
                continue;
            }
            let Some((value, unit)) = parse_number_with_unit(raw_value) else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }
            if accum.len() >= MAX_RENDER_PROFILE_STAGES && !accum.contains_key(key) {
                continue;
            }
            let entry =
                accum
                    .entry(key.to_string())
                    .or_insert((0, value, value, 0.0, unit.to_string()));
            if entry.4 != unit {
                // Do not silently mix units under one key.
                continue;
            }
            entry.0 = entry.0.saturating_add(1);
            entry.1 = entry.1.min(value);
            entry.2 = entry.2.max(value);
            entry.3 += value;
        }
        let stages_truncated = accum.len() >= MAX_RENDER_PROFILE_STAGES;
        let stages = accum
            .into_iter()
            .map(|(key, (samples, min, max, sum, unit))| {
                let avg = if samples == 0 {
                    0.0
                } else {
                    sum / samples as f64
                };
                (
                    key,
                    StageStats {
                        samples,
                        min,
                        max,
                        avg,
                        unit,
                    },
                )
            })
            .collect();
        let excerpt = bounded.lines().take(32).collect::<Vec<_>>().join("\n");
        let (source, _) = redact_text(source);
        Self {
            source: truncate_chars(&source, MAX_ENV_VALUE_CHARS),
            enabled,
            line_count,
            lines_truncated,
            stages,
            stages_truncated,
            excerpt: redact_text(&excerpt).0,
        }
    }

    pub fn stage(&self, name: &str) -> Option<&StageStats> {
        self.stages.get(name)
    }

    pub fn total_samples(&self) -> u64 {
        self.stages.values().map(|stage| stage.samples).sum()
    }

    /// Largest `max` converted to milliseconds, excluding non-timing units.
    pub fn slowest_ms(&self) -> Option<(&str, f64)> {
        self.stages
            .iter()
            .filter_map(|(name, stage)| {
                let factor = match stage.unit.as_str() {
                    "ns" => 1e-6,
                    "us" | "µs" => 1e-3,
                    "ms" => 1.0,
                    "s" => 1e3,
                    "" => 1.0,
                    _ => return None,
                };
                Some((name.as_str(), stage.max * factor))
            })
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }
}

fn key_looks_like_timing(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "frame", "draw", "render", "raster", "upload", "present", "slint", "paint", "flush",
        "encode", "decode", "ms", "us", "latency", "time",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn parse_number_with_unit(raw: &str) -> Option<(f64, &'static str)> {
    let raw = raw.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''
        )
    });
    for (suffix, unit) in [
        ("ms", "ms"),
        ("us", "us"),
        ("µs", "µs"),
        ("ns", "ns"),
        ("s", "s"),
    ] {
        if let Some(number) = raw.strip_suffix(suffix) {
            return number.parse::<f64>().ok().map(|value| (value, unit));
        }
    }
    raw.parse::<f64>().ok().map(|value| (value, ""))
}

impl Environment {
    /// Collect the current process environment without reading arbitrary env
    /// values into evidence. Only specific, documented variables are read.
    pub fn from_process() -> Self {
        let requested = env_string("SLINT_BACKEND").or_else(|| env_string("TERVO_RENDERER"));
        let active = env_string("TERVO_ACTIVE_RENDERER");
        let fallback = env_string("TERVO_RENDERER_FALLBACK").and_then(|value| parse_bool(&value));
        let reason = env_string("TERVO_FALLBACK_REASON");
        let gpu = {
            let adapter =
                env_string("TERVO_GPU_ADAPTER").or_else(|| env_string("WGPU_ADAPTER_NAME"));
            let backend = env_string("WGPU_BACKEND");
            let driver = env_string("TERVO_GPU_DRIVER");
            let device_type = env_string("TERVO_GPU_DEVICE_TYPE");
            (adapter.is_some() || backend.is_some() || driver.is_some() || device_type.is_some())
                .then_some(GpuInfo {
                    adapter,
                    backend,
                    device_type,
                    driver,
                })
        };
        let renderer = (requested.is_some()
            || active.is_some()
            || fallback.is_some()
            || reason.is_some()
            || gpu.is_some())
        .then_some(RendererInfo {
            requested,
            active,
            fallback,
            fallback_reason: reason,
            gpu,
        });
        let log_dir = Some(clean(&log_dir().display().to_string()));
        Self {
            source: Some("process".to_string()),
            version: Some(clean(env!("CARGO_PKG_VERSION"))),
            commit: option_env!("TERVO_GIT_COMMIT").map(clean),
            dirty: option_env!("TERVO_GIT_DIRTY").and_then(parse_bool),
            build_time: option_env!("TERVO_BUILD_TIME").map(clean),
            target: option_env!("TERVO_BUILD_TARGET").map(clean),
            os: Some(format!(
                "{} ({})",
                std::env::consts::OS,
                std::env::consts::ARCH
            )),
            display: env_string("DISPLAY"),
            wayland_display: env_string("WAYLAND_DISPLAY"),
            session_type: env_string("XDG_SESSION_TYPE"),
            desktop: env_string("XDG_CURRENT_DESKTOP"),
            renderer,
            config_path: Some(clean(&config_path().display().to_string())),
            profiles_path: Some(clean(&profiles_path().display().to_string())),
            log_path: None,
            log_dir,
            dri_present: Some(Path::new("/dev/dri").exists()),
        }
    }

    /// Parse `tervo diag env --json`. Additive fields for renderer/GPU are
    /// accepted when present; absent fields stay absent.
    pub fn from_diag_env_json(value: &Value) -> Result<Self, IngestError> {
        require_diag_object(value, "env")?;
        let renderer_value = value.get("renderer");
        let renderer_object = renderer_value.and_then(Value::as_object);
        let requested = value
            .get("backend")
            .and_then(Value::as_str)
            .or_else(|| {
                renderer_object
                    .and_then(|object| object.get("requested"))
                    .and_then(Value::as_str)
            })
            .map(clean);
        let active = value
            .get("renderer_active")
            .and_then(Value::as_str)
            .or_else(|| value.get("renderer").and_then(Value::as_str))
            .or_else(|| {
                renderer_object
                    .and_then(|object| object.get("active"))
                    .and_then(Value::as_str)
            })
            .map(clean);
        let fallback = value
            .get("renderer_fallback")
            .and_then(Value::as_bool)
            .or_else(|| {
                renderer_object
                    .and_then(|object| object.get("fallback"))
                    .and_then(Value::as_bool)
            });
        let fallback_reason = value
            .get("renderer_fallback_reason")
            .and_then(Value::as_str)
            .or_else(|| {
                renderer_object
                    .and_then(|object| object.get("fallback_reason"))
                    .and_then(Value::as_str)
            })
            .map(clean);
        let gpu = {
            let gpu_object = renderer_object
                .and_then(|object| object.get("gpu"))
                .and_then(Value::as_object);
            let adapter = value
                .get("gpu_adapter")
                .and_then(Value::as_str)
                .or_else(|| {
                    gpu_object
                        .and_then(|object| object.get("adapter"))
                        .and_then(Value::as_str)
                });
            let backend = value
                .get("gpu_backend")
                .and_then(Value::as_str)
                .or_else(|| {
                    gpu_object
                        .and_then(|object| object.get("backend"))
                        .and_then(Value::as_str)
                });
            let device_type = value
                .get("gpu_device_type")
                .and_then(Value::as_str)
                .or_else(|| {
                    gpu_object
                        .and_then(|object| object.get("device_type"))
                        .and_then(Value::as_str)
                });
            let driver = value.get("gpu_driver").and_then(Value::as_str).or_else(|| {
                gpu_object
                    .and_then(|object| object.get("driver"))
                    .and_then(Value::as_str)
            });
            (adapter.is_some() || backend.is_some() || device_type.is_some() || driver.is_some())
                .then_some(GpuInfo {
                    adapter: adapter.map(clean),
                    backend: backend.map(clean),
                    device_type: device_type.map(clean),
                    driver: driver.map(clean),
                })
        };
        let renderer = (requested.is_some()
            || active.is_some()
            || fallback.is_some()
            || fallback_reason.is_some()
            || gpu.is_some())
        .then_some(RendererInfo {
            requested,
            active,
            fallback,
            fallback_reason,
            gpu,
        });
        Ok(Self {
            source: Some(
                value
                    .get("source")
                    .and_then(Value::as_str)
                    .map(clean)
                    .unwrap_or_else(|| "tervo.diag.v1 env".to_string()),
            ),
            version: value.get("version").and_then(Value::as_str).map(clean),
            commit: value.get("commit").and_then(Value::as_str).map(clean),
            dirty: value.get("dirty").and_then(Value::as_bool),
            build_time: value
                .get("built")
                .or_else(|| value.get("build_time"))
                .and_then(Value::as_str)
                .map(clean),
            target: value.get("target").and_then(Value::as_str).map(clean),
            os: value.get("os").and_then(Value::as_str).map(clean),
            display: value.get("display").and_then(Value::as_str).map(clean),
            wayland_display: value
                .get("wayland_display")
                .and_then(Value::as_str)
                .map(clean),
            session_type: value.get("session_type").and_then(Value::as_str).map(clean),
            desktop: value.get("desktop").and_then(Value::as_str).map(clean),
            renderer,
            config_path: value.get("config").and_then(Value::as_str).map(clean),
            profiles_path: value.get("profiles").and_then(Value::as_str).map(clean),
            log_path: value.get("log").and_then(Value::as_str).map(clean),
            log_dir: value.get("log_dir").and_then(Value::as_str).map(clean),
            dri_present: value.get("dri").and_then(Value::as_bool),
        })
    }
}

/// `$TERVO_CONFIG` or `$XDG_CONFIG_HOME/tervo/config.toml`.
pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TERVO_CONFIG") {
        return PathBuf::from(path);
    }
    config_home().join("tervo").join("config.toml")
}

pub fn profiles_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TERVO_PROFILES") {
        return PathBuf::from(path);
    }
    config_home().join("tervo").join("profiles.toml")
}

/// `$TERVO_LOG_DIR` or `$XDG_STATE_HOME/tervo/logs`.
pub fn log_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("TERVO_LOG_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("tervo").join("logs");
    }
    home()
        .join(".local")
        .join("state")
        .join("tervo")
        .join("logs")
}

fn config_home() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path);
    }
    home().join(".config")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn require_diag_object(value: &Value, context: &'static str) -> Result<(), IngestError> {
    if !value.is_object() {
        return Err(IngestError::NotObject(context));
    }
    if let Some(schema) = value.get("schema").and_then(Value::as_str) {
        if !(schema == "tervo.diag.v1" || schema.starts_with("tervo.diag.v1.")) {
            return Err(IngestError::UnsupportedSchema(schema.to_string()));
        }
    }
    Ok(())
}

fn required_u64(value: &Value, field: &'static str) -> Result<u64, IngestError> {
    value
        .get(field)
        .ok_or(IngestError::Missing(field))?
        .as_u64()
        .ok_or(IngestError::WrongType(field))
}

fn env_string(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| {
        let text = value.to_string_lossy();
        clean(&truncate_chars(&text, MAX_ENV_VALUE_CHARS))
    })
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn clean(value: &str) -> String {
    truncate_chars(&redact_text(value).0, MAX_ENV_VALUE_CHARS)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn take_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_frame() -> Value {
        json!({
            "schema": "tervo.diag.v1",
            "kind": "snapshot",
            "cols": 10,
            "rows": 3,
            "generation": 7,
            "cursor": {"row": 1, "col": 2, "visible": true, "shape": "block"},
            "mode": 1,
            "damage": {
                "full": false,
                "region_count": 2,
                "rects": [
                    {"x": 0, "y": 0, "w": 5, "h": 1},
                    {"x": 3, "y": 0, "w": 5, "h": 1}
                ]
            },
            "images": [{"id": 1, "row": 1, "col": 2, "width": 3, "height": 1}],
            "text": ["hello", "world", "tail"]
        })
    }

    #[test]
    fn parses_frame_and_computes_union_damage() {
        let frame = FrameSummary::from_diag_json(&sample_frame()).unwrap();
        assert_eq!(frame.cols, 10);
        assert_eq!(frame.rows, 3);
        assert_eq!(frame.generation, Some(7));
        assert_eq!(frame.text.len(), 3);
        let damage = frame.damage.as_ref().unwrap();
        assert_eq!(damage.full, Some(false));
        assert_eq!(damage.area_cells, Some(8)); // union of [0,5) and [3,8)
        assert_eq!(damage.coverage_ppm, Some(266_666));
    }

    #[test]
    fn frame_text_is_capped_to_a_viewport_excerpt() {
        let mut value = sample_frame();
        value["text"] = json!((0..10_000)
            .map(|index| format!("row {index}"))
            .collect::<Vec<_>>());
        let frame = FrameSummary::from_diag_json_with_limits(&value, 5, 8).unwrap();
        assert_eq!(frame.text.len(), 5);
        assert_eq!(frame.text_rows_total, 10_000);
        assert!(frame.text_truncated);
        assert!(frame.text.iter().all(|line| line.chars().count() <= 8));
    }

    #[test]
    fn frame_does_not_default_missing_damage_full() {
        let mut value = sample_frame();
        value["damage"] = json!({"region_count": 1, "rects": []});
        let frame = FrameSummary::from_diag_json(&value).unwrap();
        let damage = frame.damage.unwrap();
        assert_eq!(damage.full, None);
        assert_eq!(damage.area_cells, None);
        assert_eq!(damage.coverage_ppm, None);
    }

    #[test]
    fn rejects_wrong_schema_and_missing_fields() {
        let mut value = sample_frame();
        value["schema"] = json!("tervo.diag.v2");
        assert!(matches!(
            FrameSummary::from_diag_json(&value),
            Err(IngestError::UnsupportedSchema(_))
        ));
        let mut value = sample_frame();
        value.as_object_mut().unwrap().remove("text");
        assert!(matches!(
            FrameSummary::from_diag_json(&value),
            Err(IngestError::Missing("text"))
        ));
    }

    #[test]
    fn parses_placeholder_metrics_without_promoting_them() {
        let value = json!({
            "schema": "tervo.diag.v1",
            "kind": "metrics",
            "initialized": false,
            "source": "cli",
            "metrics": {
                "frame_count": 0,
                "frame_time": {"p95_us": null, "samples": 0, "p95_overflow": false},
                "damage": {"area_ratio_last_ppm": 0, "area_ratio_max_ppm": 0},
                "stale_frames": 0,
                "textures": {"live_bytes_estimate": 0},
                "images": {"placements": 0}
            }
        });
        let metrics = MetricsSummary::from_diag_json(&value).unwrap();
        assert!(metrics.initialized == Some(false));
        assert_eq!(metrics.p95_us, None);
        assert_eq!(metrics.sample_count, Some(0));
        assert!(metrics.is_placeholder());
        assert!(!metrics.is_live());
    }

    #[test]
    fn parses_render_profile_stage_stats() {
        let text = "frame_ms=12.5 draw_ms=3.0 upload_ms=1.25 frame_ms=33.0\n\
                    noise=42 other frame=9ms rust=1.2.3";
        let summary = RenderProfileSummary::from_text("/tmp/profile.log", text, Some(true), None);
        assert_eq!(summary.stage("frame_ms").unwrap().samples, 2);
        assert_eq!(summary.stage("frame_ms").unwrap().max, 33.0);
        assert_eq!(summary.slowest_ms().unwrap().0, "frame_ms");
        assert!(summary.total_samples() >= 3);
        assert_eq!(summary.enabled, Some(true));
    }

    #[test]
    fn render_profile_without_numbers_does_not_invent_samples() {
        let summary = RenderProfileSummary::from_text("src", "no timing here", None, None);
        assert!(summary.stages.is_empty());
        assert_eq!(summary.total_samples(), 0);
        assert_eq!(summary.slowest_ms(), None);
    }

    #[test]
    fn parses_env_json_and_keeps_missing_fields_missing() {
        let value = json!({
            "schema": "tervo.diag.v1",
            "kind": "env",
            "version": "0.3.1",
            "commit": "a6afbd1",
            "dirty": false,
            "backend": "skia",
            "config": "/home/user/.config/tervo/config.toml",
            "log": "/home/user/.state/tervo/logs/tervo-1.log",
            "gpu_adapter": "llvmpipe",
            "renderer_fallback": true,
            "renderer_fallback_reason": "wgpu init failed"
        });
        let env = Environment::from_diag_env_json(&value).unwrap();
        assert_eq!(env.version.as_deref(), Some("0.3.1"));
        assert_eq!(env.commit.as_deref(), Some("a6afbd1"));
        assert_eq!(env.dirty, Some(false));
        assert_eq!(
            env.renderer.as_ref().unwrap().requested.as_deref(),
            Some("skia")
        );
        assert_eq!(env.renderer.as_ref().unwrap().fallback, Some(true));
        assert_eq!(
            env.renderer
                .as_ref()
                .unwrap()
                .gpu
                .as_ref()
                .unwrap()
                .adapter
                .as_deref(),
            Some("llvmpipe")
        );
        assert_eq!(env.display, None);
        assert_eq!(env.os, None);
    }

    #[test]
    fn env_json_strings_are_redacted() {
        let value = json!({
            "schema": "tervo.diag.v1",
            "kind": "env",
            "backend": "https://user:pass@example.com",
            "display": "token=ghp_abcdefghijklmnop"
        });
        let env = Environment::from_diag_env_json(&value).unwrap();
        let serialized = serde_json::to_string(&env).unwrap();
        assert!(!serialized.contains("user:pass"));
        assert!(!serialized.contains("ghp_"));
    }
}
