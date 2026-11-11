// SPDX-License-Identifier: GPL-3.0-only
//! Deterministic, offline doctor primitives.
//!
//! This module turns existing `tervo.diag.v1` data and explicit user input into
//! a versioned [`DiagnosticCase`]. It never contacts the network, never calls a
//! model, never reads scrollback and never invents measurements: absent data is
//! represented as absent (or `Skipped`/`Unknown`), not as a healthy zero.
//!
//! The public surface is intentionally small so `tervo diag` (or another
//! front-end) can call it as a library; `src/bin/tervo-doctor.rs` is a thin CLI
//! over the same API.

pub mod evidence;
pub mod hash;
pub mod health;
pub mod ingest;
pub mod issue;
pub mod model;
pub mod redact;
pub mod schema;

pub use evidence::{
    Evidence, EvidenceBundle, EvidenceKind, EvidenceLimits, EvidenceOmission, EvidenceStats,
};
pub use health::{Attachment, Doctor, DoctorConfig, DoctorError, DoctorInput};
pub use ingest::{
    CursorSummary, DamageInfo, FrameSummary, ImageRect, IngestError, MetricsSummary,
    RenderProfileSummary, StageStats,
};
pub use model::{
    Confidence, DiagnosticCase, Environment, Finding, GpuInfo, IssueDraft, Layer, Probe,
    ProbeStatus, Recommendation, RecommendationKind, RegressionStatus, RegressionTest,
    RendererInfo, Severity, Symptom, DIAGNOSTIC_CASE_SCHEMA,
};
pub use redact::{redact_json, redact_text, RedactionReport};
pub use schema::diagnostic_case_json_schema;
