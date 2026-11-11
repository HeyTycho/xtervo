// SPDX-License-Identifier: GPL-3.0-only
//! Bounded, redacted, hashable evidence bundles.
//!
//! Every item is redacted before hashing, capped per item and in total, and
//! carries the SHA-256 of exactly the bytes that are stored. The bundle hash
//! covers the ordered manifest, so adding/removing/reordering items or editing
//! content is detectable. No item ever contains scrollback: callers pass
//! bounded viewport/log excerpts; this module only enforces the cap.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::hash::sha256_hex;
use super::redact::{redact_json, redact_text, RedactionReport};

/// Versioned evidence-bundle schema.
pub const EVIDENCE_BUNDLE_SCHEMA: &str = "tervo.evidence_bundle.v1";
/// Default per-item cap (16 KiB).
pub const DEFAULT_MAX_ITEM_BYTES: usize = 16 * 1024;
/// Default total cap (64 KiB).
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 64 * 1024;
/// Default item-count cap.
pub const DEFAULT_MAX_ITEMS: usize = 64;
const MIN_ITEM_BYTES: usize = 64;
const TRUNCATION_MARKER: &str = "\n… [truncated]";
const MIN_USEFUL_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Environment,
    Config,
    Paths,
    Frame,
    Damage,
    Metrics,
    RenderProfile,
    Logs,
    Attachment,
    Other,
}

impl EvidenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Config => "config",
            Self::Paths => "paths",
            Self::Frame => "frame",
            Self::Damage => "damage",
            Self::Metrics => "metrics",
            Self::RenderProfile => "render_profile",
            Self::Logs => "logs",
            Self::Attachment => "attachment",
            Self::Other => "other",
        }
    }
}

/// One bounded, redacted evidence item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub id: String,
    pub kind: EvidenceKind,
    pub label: String,
    pub media_type: String,
    pub content: String,
    /// Input size before redaction/truncation.
    pub original_bytes: usize,
    /// Size of `content` after redaction/truncation.
    pub stored_bytes: usize,
    pub truncated: bool,
    /// SHA-256 of `content` (the exact stored bytes).
    pub sha256: String,
    pub redaction: RedactionReport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmissionReason {
    CountLimit,
    TotalLimit,
    Empty,
}

impl OmissionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CountLimit => "count_limit",
            Self::TotalLimit => "total_limit",
            Self::Empty => "empty",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceOmission {
    pub label: String,
    pub kind: EvidenceKind,
    pub reason: OmissionReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLimits {
    pub max_item_bytes: usize,
    pub max_total_bytes: usize,
    pub max_items: usize,
}

impl Default for EvidenceLimits {
    fn default() -> Self {
        Self {
            max_item_bytes: DEFAULT_MAX_ITEM_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_items: DEFAULT_MAX_ITEMS,
        }
    }
}

impl EvidenceLimits {
    /// Clamp nonsensical zero caps to a still-useful minimum. The caps are a
    /// safety boundary, not a way to collect nothing.
    pub fn normalized(self) -> Self {
        let max_item_bytes = self.max_item_bytes.max(MIN_ITEM_BYTES);
        Self {
            max_item_bytes,
            max_total_bytes: self.max_total_bytes.max(max_item_bytes),
            max_items: self.max_items.max(1),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundle {
    pub schema: String,
    pub limits: EvidenceLimits,
    pub items: Vec<Evidence>,
    pub total_bytes: usize,
    pub sha256: String,
    pub redaction: RedactionReport,
    pub omissions: Vec<EvidenceOmission>,
}

impl EvidenceBundle {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Recompute item and bundle hashes. Catches edited content, dropped items
    /// and cap violations in an imported case.
    pub fn verify(&self) -> Result<(), EvidenceVerifyError> {
        if self.schema != EVIDENCE_BUNDLE_SCHEMA {
            return Err(EvidenceVerifyError::WrongSchema(self.schema.clone()));
        }
        if self.items.len() > self.limits.max_items {
            return Err(EvidenceVerifyError::ItemCount {
                actual: self.items.len(),
                max: self.limits.max_items,
            });
        }
        let mut total = 0usize;
        for item in &self.items {
            if item.stored_bytes != item.content.len() {
                return Err(EvidenceVerifyError::StoredBytes {
                    id: item.id.clone(),
                    recorded: item.stored_bytes,
                    actual: item.content.len(),
                });
            }
            if item.content.len() > self.limits.max_item_bytes {
                return Err(EvidenceVerifyError::ItemTooLarge {
                    id: item.id.clone(),
                    actual: item.content.len(),
                    max: self.limits.max_item_bytes,
                });
            }
            let expected = sha256_hex(item.content.as_bytes());
            if expected != item.sha256 {
                return Err(EvidenceVerifyError::ItemHash(item.id.clone()));
            }
            total = total.saturating_add(item.stored_bytes);
        }
        if total > self.limits.max_total_bytes {
            return Err(EvidenceVerifyError::TotalTooLarge {
                actual: total,
                max: self.limits.max_total_bytes,
            });
        }
        if total != self.total_bytes {
            return Err(EvidenceVerifyError::TotalMismatch {
                recorded: self.total_bytes,
                actual: total,
            });
        }
        let expected = bundle_hash(&self.items, &self.omissions, total);
        if expected != self.sha256 {
            return Err(EvidenceVerifyError::BundleHash);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceVerifyError {
    WrongSchema(String),
    ItemCount {
        actual: usize,
        max: usize,
    },
    ItemTooLarge {
        id: String,
        actual: usize,
        max: usize,
    },
    TotalTooLarge {
        actual: usize,
        max: usize,
    },
    StoredBytes {
        id: String,
        recorded: usize,
        actual: usize,
    },
    ItemHash(String),
    TotalMismatch {
        recorded: usize,
        actual: usize,
    },
    BundleHash,
}

impl std::fmt::Display for EvidenceVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongSchema(schema) => write!(f, "unknown evidence schema: {schema}"),
            Self::ItemCount { actual, max } => {
                write!(f, "evidence has {actual} items, limit is {max}")
            }
            Self::ItemTooLarge { id, actual, max } => {
                write!(f, "evidence item {id} is {actual} bytes, limit is {max}")
            }
            Self::TotalTooLarge { actual, max } => {
                write!(f, "evidence total is {actual} bytes, limit is {max}")
            }
            Self::StoredBytes {
                id,
                recorded,
                actual,
            } => write!(
                f,
                "evidence item {id} recorded {recorded} bytes but stores {actual}"
            ),
            Self::ItemHash(id) => write!(f, "evidence item {id} content hash mismatch"),
            Self::TotalMismatch { recorded, actual } => {
                write!(f, "evidence total recorded {recorded}, actual {actual}")
            }
            Self::BundleHash => write!(f, "evidence bundle hash mismatch"),
        }
    }
}

impl std::error::Error for EvidenceVerifyError {}

/// Builder stats, useful for the deterministic "evidence health" probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EvidenceStats {
    pub item_count: usize,
    pub total_bytes: usize,
    pub truncated_items: usize,
    pub omissions: usize,
}

/// Mutable collector. Not serialized; call [`Self::finish`] for the immutable
/// bundle.
#[derive(Debug)]
pub struct EvidenceBundleBuilder {
    limits: EvidenceLimits,
    items: Vec<Evidence>,
    total_bytes: usize,
    omissions: Vec<EvidenceOmission>,
    used_ids: std::collections::BTreeSet<String>,
}

impl EvidenceBundleBuilder {
    pub fn new(limits: EvidenceLimits) -> Self {
        Self {
            limits: limits.normalized(),
            items: Vec::new(),
            total_bytes: 0,
            omissions: Vec::new(),
            used_ids: std::collections::BTreeSet::new(),
        }
    }

    pub fn limits(&self) -> EvidenceLimits {
        self.limits
    }

    /// Add text evidence. Returns the item id, or `None` if the bundle limit
    /// prevented storage (the omission is recorded).
    pub fn add_text(&mut self, kind: EvidenceKind, label: &str, text: &str) -> Option<String> {
        let (redacted, report) = redact_text(text);
        self.add_prepared(
            kind,
            label,
            &redacted,
            text.len(),
            "text/plain; charset=utf-8",
            report,
        )
    }

    /// Add JSON evidence with structured redaction. The stored content is
    /// pretty-printed JSON, so it remains inspectable after redaction.
    pub fn add_json(&mut self, kind: EvidenceKind, label: &str, value: &Value) -> Option<String> {
        let original_bytes = serde_json::to_string_pretty(value)
            .map(|raw| raw.len())
            .unwrap_or(0);
        let mut value = value.clone();
        let report = redact_json(&mut value);
        let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());
        self.add_prepared(
            kind,
            label,
            &pretty,
            original_bytes,
            "application/json",
            report,
        )
    }

    fn add_prepared(
        &mut self,
        kind: EvidenceKind,
        label: &str,
        content: &str,
        original_bytes: usize,
        media_type: &str,
        report: RedactionReport,
    ) -> Option<String> {
        let (label, _) = redact_text(label);
        let label = label.trim();
        let label = if label.is_empty() { "evidence" } else { label };
        if content.is_empty() {
            self.omissions.push(EvidenceOmission {
                label: truncate_chars(label, 80),
                kind,
                reason: OmissionReason::Empty,
            });
            return None;
        }
        if self.items.len() >= self.limits.max_items {
            self.omissions.push(EvidenceOmission {
                label: truncate_chars(label, 80),
                kind,
                reason: OmissionReason::CountLimit,
            });
            return None;
        }

        let remaining = self.limits.max_total_bytes.saturating_sub(self.total_bytes);
        if remaining < MIN_USEFUL_BYTES {
            self.omissions.push(EvidenceOmission {
                label: truncate_chars(label, 80),
                kind,
                reason: OmissionReason::TotalLimit,
            });
            return None;
        }
        let per_item_cap = self.limits.max_item_bytes.min(remaining);
        let (mut content, mut truncated) = cap_text(content, per_item_cap);
        if content.len() > remaining {
            let (shrunk, _) = cap_text(&content, remaining);
            content = shrunk;
            truncated = true;
        }
        if content.is_empty() {
            self.omissions.push(EvidenceOmission {
                label: truncate_chars(label, 80),
                kind,
                reason: OmissionReason::TotalLimit,
            });
            return None;
        }

        let id = self.unique_id(label);
        let stored_bytes = content.len();
        let sha256 = sha256_hex(content.as_bytes());
        self.total_bytes = self.total_bytes.saturating_add(stored_bytes);
        self.items.push(Evidence {
            id: id.clone(),
            kind,
            label: label.to_string(),
            media_type: media_type.to_string(),
            content,
            original_bytes,
            stored_bytes,
            truncated,
            sha256,
            redaction: report,
        });
        Some(id)
    }

    pub fn stats(&self) -> EvidenceStats {
        EvidenceStats {
            item_count: self.items.len(),
            total_bytes: self.total_bytes,
            truncated_items: self.items.iter().filter(|item| item.truncated).count(),
            omissions: self.omissions.len(),
        }
    }

    pub fn finish(mut self) -> EvidenceBundle {
        let mut redaction = RedactionReport::default();
        for item in &self.items {
            redaction.merge(&item.redaction);
        }
        let total_bytes = self.total_bytes;
        let sha256 = bundle_hash(&self.items, &self.omissions, total_bytes);
        self.omissions.shrink_to_fit();
        EvidenceBundle {
            schema: EVIDENCE_BUNDLE_SCHEMA.to_string(),
            limits: self.limits,
            items: self.items,
            total_bytes,
            sha256,
            redaction,
            omissions: self.omissions,
        }
    }

    fn unique_id(&mut self, label: &str) -> String {
        let base = slug(label);
        let mut candidate = base.clone();
        let mut suffix = 2u32;
        while self.used_ids.contains(&candidate) {
            candidate = format!("{base}-{suffix}");
            suffix = suffix.saturating_add(1);
        }
        self.used_ids.insert(candidate.clone());
        candidate
    }
}

fn bundle_hash(items: &[Evidence], omissions: &[EvidenceOmission], total_bytes: usize) -> String {
    let mut canonical = String::with_capacity(128 + items.len() * 160);
    canonical.push_str(EVIDENCE_BUNDLE_SCHEMA);
    canonical.push('\n');
    for item in items {
        canonical.push_str(&item.id);
        canonical.push('\0');
        canonical.push_str(item.kind.as_str());
        canonical.push('\0');
        canonical.push_str(&item.sha256);
        canonical.push('\0');
        canonical.push_str(&item.stored_bytes.to_string());
        canonical.push('\n');
    }
    for omission in omissions {
        canonical.push_str("omitted\0");
        canonical.push_str(&omission.label);
        canonical.push('\0');
        canonical.push_str(omission.kind.as_str());
        canonical.push('\0');
        canonical.push_str(omission.reason.as_str());
        canonical.push('\n');
    }
    canonical.push_str("total\0");
    canonical.push_str(&total_bytes.to_string());
    sha256_hex(canonical.as_bytes())
}

fn cap_text(text: &str, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text.to_string(), false);
    }
    if cap <= TRUNCATION_MARKER.len() + 8 {
        return (truncate_utf8(text, cap), true);
    }
    let budget = cap - TRUNCATION_MARKER.len();
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + TRUNCATION_MARKER.len());
    out.push_str(&text[..end]);
    out.push_str(TRUNCATION_MARKER);
    (out, true)
}

fn truncate_utf8(text: &str, cap: usize) -> String {
    let mut end = cap.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len().min(64));
    for ch in label.chars() {
        if out.len() >= 64 {
            break;
        }
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            out.push(ch);
        } else if (ch.is_whitespace() || matches!(ch, '/' | ':' | '#' | '@')) && !out.ends_with('-')
        {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "evidence".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bundle_redacts_hashes_and_verifies() {
        let mut builder = EvidenceBundleBuilder::new(EvidenceLimits::default());
        let id = builder
            .add_text(
                EvidenceKind::Logs,
                "log tail",
                "starting\napi_key=sk-live-1234567890\nready",
            )
            .expect("evidence should fit");
        assert_eq!(id, "log-tail");
        let bundle = builder.finish();
        assert_eq!(bundle.schema, EVIDENCE_BUNDLE_SCHEMA);
        assert_eq!(bundle.sha256.len(), 64);
        assert_eq!(bundle.items.len(), 1);
        assert!(!bundle.items[0].content.contains("sk-live"));
        assert!(bundle.items[0].redaction.matches >= 1);
        assert_eq!(bundle.items[0].sha256.len(), 64);
        bundle.verify().expect("bundle should verify");
    }

    #[test]
    fn tampering_is_detected() {
        let mut builder = EvidenceBundleBuilder::new(EvidenceLimits::default());
        builder
            .add_text(EvidenceKind::Frame, "frame", "hello")
            .unwrap();
        let mut bundle = builder.finish();
        bundle.verify().unwrap();
        bundle.items[0].content.push('!');
        bundle.items[0].stored_bytes += 1;
        bundle.total_bytes += 1;
        assert!(matches!(
            bundle.verify(),
            Err(EvidenceVerifyError::ItemHash(_))
        ));

        let mut builder = EvidenceBundleBuilder::new(EvidenceLimits::default());
        builder
            .add_text(EvidenceKind::Frame, "frame", "hello")
            .unwrap();
        let mut bundle = builder.finish();
        bundle.items.clear();
        bundle.total_bytes = 0;
        assert!(matches!(
            bundle.verify(),
            Err(EvidenceVerifyError::BundleHash)
        ));
    }

    #[test]
    fn caps_truncate_and_omit_without_exceeding_limits() {
        let limits = EvidenceLimits {
            max_item_bytes: 128,
            max_total_bytes: 220,
            max_items: 4,
        };
        let mut builder = EvidenceBundleBuilder::new(limits);
        builder.add_text(EvidenceKind::Logs, "a", &"x".repeat(500));
        builder.add_text(EvidenceKind::Logs, "b", &"y".repeat(500));
        builder.add_text(EvidenceKind::Logs, "c", &"z".repeat(500));
        let stats = builder.stats();
        assert!(stats.truncated_items >= 1);
        let bundle = builder.finish();
        assert!(bundle.total_bytes <= limits.max_total_bytes);
        for item in &bundle.items {
            assert!(item.content.len() <= limits.max_item_bytes);
            assert!(item.truncated);
        }
        assert!(!bundle.omissions.is_empty());
        bundle.verify().expect("capped bundle must still verify");
    }

    #[test]
    fn json_evidence_is_redacted_and_still_json() {
        let mut builder = EvidenceBundleBuilder::new(EvidenceLimits::default());
        builder
            .add_json(
                EvidenceKind::Environment,
                "environment",
                &json!({
                    "version": "0.3.1",
                    "token": "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                    "display": "https://alice:pass@host",
                }),
            )
            .unwrap();
        let bundle = builder.finish();
        let text = &bundle.items[0].content;
        assert!(!text.contains("ghp_"));
        assert!(!text.contains("alice"));
        assert!(serde_json::from_str::<Value>(text).is_ok());
        bundle.verify().unwrap();
    }

    #[test]
    fn build_is_deterministic() {
        let build = || {
            let mut builder = EvidenceBundleBuilder::new(EvidenceLimits::default());
            builder.add_text(EvidenceKind::Other, "one", "hello");
            builder.add_json(EvidenceKind::Other, "two", &json!({"b": 2, "a": 1}));
            builder.finish()
        };
        let left = build();
        let right = build();
        assert_eq!(left.sha256, right.sha256);
        assert_eq!(left.items, right.items);
    }

    #[test]
    fn labels_are_redacted_and_ids_unique() {
        let mut builder = EvidenceBundleBuilder::new(EvidenceLimits::default());
        let one = builder
            .add_text(EvidenceKind::Other, "token=secret label", "one")
            .unwrap();
        let two = builder
            .add_text(EvidenceKind::Other, "token=secret label", "two")
            .unwrap();
        assert_ne!(one, two);
        let bundle = builder.finish();
        for item in &bundle.items {
            assert!(!item.label.contains("secret"));
        }
    }

    #[test]
    fn empty_evidence_is_recorded_as_omitted() {
        let mut builder = EvidenceBundleBuilder::new(EvidenceLimits::default());
        assert!(builder.add_text(EvidenceKind::Other, "empty", "").is_none());
        let bundle = builder.finish();
        assert!(bundle.is_empty());
        assert_eq!(bundle.omissions[0].reason, OmissionReason::Empty);
        bundle.verify().unwrap();
    }
}
