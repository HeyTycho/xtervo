// SPDX-License-Identifier: GPL-3.0-only
//! Machine-readable JSON Schema for `xtervo.diagnostic_case.v1`.
//!
//! The Rust structs are the source of truth; this document gives front-ends,
//! issue tooling and future language bindings a stable versioned contract. It
//! is additive: object schemas keep `additionalProperties: true`, so future
//! fields do not invalidate old consumers.

use serde_json::{json, Value};

use super::model::DIAGNOSTIC_CASE_SCHEMA;

/// Return the versioned JSON Schema document.
pub fn diagnostic_case_json_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://xtervo.dev/schemas/xtervo.diagnostic_case.v1.json",
        "title": "xtervo DiagnosticCase v1",
        "description": "Deterministic, offline diagnostic case produced by xtervo-doctor. Missing data is null, never a fabricated default.",
        "type": "object",
        "additionalProperties": true,
        "required": [
            "schema",
            "case_id",
            "symptom",
            "environment",
            "evidence",
            "probes",
            "findings",
            "recommendations",
            "issue_draft",
            "regression_test"
        ],
        "properties": {
            "schema": {
                "const": DIAGNOSTIC_CASE_SCHEMA,
                "description": "Versioned document id."
            },
            "case_id": {
                "type": "string",
                "pattern": "^case-[0-9a-f]{16}$",
                "description": "Deterministic hash-based id; no timestamp/random component."
            },
            "symptom": { "$ref": "#/$defs/symptom" },
            "environment": { "$ref": "#/$defs/environment" },
            "evidence": { "$ref": "#/$defs/evidence_bundle" },
            "probes": {
                "type": "array",
                "minItems": 1,
                "items": { "$ref": "#/$defs/probe" }
            },
            "findings": {
                "type": "array",
                "minItems": 1,
                "items": { "$ref": "#/$defs/finding" }
            },
            "recommendations": {
                "type": "array",
                "items": { "$ref": "#/$defs/recommendation" }
            },
            "issue_draft": { "$ref": "#/$defs/issue_draft" },
            "regression_test": { "$ref": "#/$defs/regression_test" }
        },
        "$defs": {
            "symptom": {
                "type": "object",
                "additionalProperties": true,
                "required": ["reported", "summary"],
                "properties": {
                    "reported": {"type": "boolean"},
                    "summary": {"type": "string", "minLength": 1},
                    "details": {"type": ["string", "null"]},
                    "repro_steps": {"type": "array", "items": {"type": "string"}},
                    "expected": {"type": ["string", "null"]},
                    "actual": {"type": ["string", "null"]}
                }
            },
            "environment": {
                "type": "object",
                "additionalProperties": true,
                "description": "Every field is optional. Null means not collected.",
                "properties": {
                    "source": {"type": ["string", "null"]},
                    "version": {"type": ["string", "null"]},
                    "commit": {"type": ["string", "null"]},
                    "dirty": {"type": ["boolean", "null"]},
                    "build_time": {"type": ["string", "null"]},
                    "target": {"type": ["string", "null"]},
                    "os": {"type": ["string", "null"]},
                    "display": {"type": ["string", "null"]},
                    "wayland_display": {"type": ["string", "null"]},
                    "session_type": {"type": ["string", "null"]},
                    "desktop": {"type": ["string", "null"]},
                    "renderer": {
                        "type": ["object", "null"],
                        "additionalProperties": true,
                        "properties": {
                            "requested": {"type": ["string", "null"]},
                            "active": {"type": ["string", "null"]},
                            "fallback": {"type": ["boolean", "null"]},
                            "fallback_reason": {"type": ["string", "null"]},
                            "gpu": {
                                "type": ["object", "null"],
                                "additionalProperties": true,
                                "properties": {
                                    "adapter": {"type": ["string", "null"]},
                                    "backend": {"type": ["string", "null"]},
                                    "device_type": {"type": ["string", "null"]},
                                    "driver": {"type": ["string", "null"]}
                                }
                            }
                        }
                    },
                    "config_path": {"type": ["string", "null"]},
                    "profiles_path": {"type": ["string", "null"]},
                    "log_path": {"type": ["string", "null"]},
                    "log_dir": {"type": ["string", "null"]},
                    "dri_present": {"type": ["boolean", "null"]}
                }
            },
            "redaction_report": {
                "type": "object",
                "additionalProperties": false,
                "required": ["matches", "rules"],
                "properties": {
                    "matches": {"type": "integer", "minimum": 0},
                    "rules": {"type": "object", "additionalProperties": {"type": "integer", "minimum": 0}}
                }
            },
            "evidence_item": {
                "type": "object",
                "additionalProperties": true,
                "required": [
                    "id", "kind", "label", "media_type", "content",
                    "original_bytes", "stored_bytes", "truncated", "sha256", "redaction"
                ],
                "properties": {
                    "id": {"type": "string", "minLength": 1},
                    "kind": {
                        "enum": ["environment", "config", "paths", "frame", "damage", "metrics", "render_profile", "logs", "attachment", "other"]
                    },
                    "label": {"type": "string"},
                    "media_type": {"type": "string"},
                    "content": {"type": "string"},
                    "original_bytes": {"type": "integer", "minimum": 0},
                    "stored_bytes": {"type": "integer", "minimum": 0},
                    "truncated": {"type": "boolean"},
                    "sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                    "redaction": { "$ref": "#/$defs/redaction_report" }
                }
            },
            "evidence_bundle": {
                "type": "object",
                "additionalProperties": true,
                "required": ["schema", "limits", "items", "total_bytes", "sha256", "redaction", "omissions"],
                "properties": {
                    "schema": {"const": "xtervo.evidence_bundle.v1"},
                    "limits": {
                        "type": "object",
                        "required": ["max_item_bytes", "max_total_bytes", "max_items"],
                        "properties": {
                            "max_item_bytes": {"type": "integer", "minimum": 1},
                            "max_total_bytes": {"type": "integer", "minimum": 1},
                            "max_items": {"type": "integer", "minimum": 1}
                        }
                    },
                    "items": {"type": "array", "items": {"$ref": "#/$defs/evidence_item"}},
                    "total_bytes": {"type": "integer", "minimum": 0},
                    "sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                    "redaction": {"$ref": "#/$defs/redaction_report"},
                    "omissions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["label", "kind", "reason"],
                            "properties": {
                                "label": {"type": "string"},
                                "kind": {"type": "string"},
                                "reason": {"enum": ["count_limit", "total_limit", "empty"]}
                            }
                        }
                    }
                }
            },
            "probe": {
                "type": "object",
                "additionalProperties": true,
                "required": ["id", "layer", "status", "summary", "evidence"],
                "properties": {
                    "id": {"type": "string", "minLength": 1},
                    "layer": {
                        "enum": ["bytes", "filter", "term", "frame", "damage", "framebuffer", "presenter", "renderer", "gpu", "profile", "config", "files", "build", "environment", "evidence"]
                    },
                    "status": {"enum": ["pass", "warn", "fail", "skipped", "unknown"]},
                    "summary": {"type": "string", "minLength": 1},
                    "evidence": {
                        "type": "array",
                        "minItems": 1,
                        "items": {"type": "string"}
                    },
                    "command": {"type": ["string", "null"]}
                }
            },
            "finding": {
                "type": "object",
                "additionalProperties": true,
                "required": ["id", "probe", "severity", "title", "detail", "evidence", "confidence"],
                "properties": {
                    "id": {"type": "string", "minLength": 1},
                    "probe": {"type": "string", "minLength": 1},
                    "severity": {"enum": ["info", "warning", "error"]},
                    "title": {"type": "string", "minLength": 1},
                    "detail": {"type": "string", "minLength": 1},
                    "evidence": {
                        "type": "array",
                        "minItems": 1,
                        "items": {"type": "string"}
                    },
                    "confidence": {"enum": ["observed", "high", "medium", "low", "hypothesis"]},
                    "falsify": {"type": ["string", "null"]},
                    "next_probe": {"type": ["string", "null"]}
                }
            },
            "recommendation": {
                "type": "object",
                "additionalProperties": true,
                "required": ["id", "title", "action", "kind", "evidence"],
                "properties": {
                    "id": {"type": "string", "minLength": 1},
                    "title": {"type": "string", "minLength": 1},
                    "action": {"type": "string", "minLength": 1},
                    "kind": {"enum": ["collect_evidence", "config", "probe", "code_fix", "regression_test"]},
                    "evidence": {
                        "type": "array",
                        "minItems": 1,
                        "items": {"type": "string"}
                    },
                    "command": {"type": ["string", "null"]},
                    "related_findings": {"type": "array", "items": {"type": "string"}}
                }
            },
            "issue_draft": {
                "type": "object",
                "required": ["title", "markdown"],
                "properties": {
                    "title": {"type": "string", "minLength": 1},
                    "markdown": {"type": "string", "minLength": 1},
                    "labels": {"type": "array", "items": {"type": "string"}}
                }
            },
            "regression_test": {
                "type": "object",
                "additionalProperties": true,
                "required": ["status", "note"],
                "properties": {
                    "status": {"enum": ["not_generated", "proposed", "not_applicable"]},
                    "name": {"type": ["string", "null"]},
                    "command": {"type": ["string", "null"]},
                    "expected": {"type": ["string", "null"]},
                    "evidence": {"type": "array", "items": {"type": "string"}},
                    "note": {"type": "string", "minLength": 1}
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_versioned_top_level_contract() {
        let schema = diagnostic_case_json_schema();
        assert_eq!(
            schema["properties"]["schema"]["const"],
            DIAGNOSTIC_CASE_SCHEMA
        );
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            required,
            vec![
                "schema",
                "case_id",
                "symptom",
                "environment",
                "evidence",
                "probes",
                "findings",
                "recommendations",
                "issue_draft",
                "regression_test"
            ]
        );
        assert_eq!(
            schema["$id"],
            "https://xtervo.dev/schemas/xtervo.diagnostic_case.v1.json"
        );
    }

    #[test]
    fn schema_is_valid_json_and_evidence_requires_hash() {
        let schema = diagnostic_case_json_schema();
        let text = serde_json::to_string_pretty(&schema).unwrap();
        let reparsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed, schema);
        let item_required: Vec<&str> = schema["$defs"]["evidence_item"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert!(item_required.contains(&"sha256"));
        assert_eq!(
            schema["$defs"]["evidence_bundle"]["properties"]["items"]["items"]["$ref"],
            "#/$defs/evidence_item"
        );
    }
}
