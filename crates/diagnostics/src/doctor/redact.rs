// SPDX-License-Identifier: GPL-3.0-only
//! Deterministic secret redaction for diagnostic evidence.
//!
//! The doctor never uploads anything; redaction is still mandatory because
//! evidence bundles and issue drafts are meant to be shared. The rules are
//! intentionally conservative:
//!
//! - sensitive key/value pairs (`token=...`, `"password": "..."`, headers);
//! - bearer/basic credentials and well-known token prefixes;
//! - URL userinfo (`scheme://user:pass@host`) and credential query parameters;
//! - PEM/PGP private-key blocks;
//! - arbitrary JWTs.
//!
//! Redaction is textual and best-effort. It is a safety net, not a license to
//! collect secrets: evidence collection itself stays bounded and opt-in.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-rule redaction counts. Only rule names and counts are retained; the
/// matched values never enter this report.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionReport {
    pub matches: u64,
    pub rules: BTreeMap<String, u64>,
}

impl RedactionReport {
    pub fn record(&mut self, rule: &str) {
        self.matches = self.matches.saturating_add(1);
        let count = self.rules.entry(rule.to_string()).or_default();
        *count = count.saturating_add(1);
    }

    pub fn merge(&mut self, other: &Self) {
        self.matches = self.matches.saturating_add(other.matches);
        for (rule, count) in &other.rules {
            let entry = self.rules.entry(rule.clone()).or_default();
            *entry = entry.saturating_add(*count);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.matches == 0
    }
}

const REDACTED: &str = "[REDACTED]";

/// Redact `input` and return the sanitized text plus rule counts.
pub fn redact_text(input: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let (without_keys, key_blocks) = redact_private_key_blocks(input);
    if key_blocks > 0 {
        for _ in 0..key_blocks {
            report.record("private_key_block");
        }
    }

    let mut output = String::with_capacity(without_keys.len());
    for (index, line) in without_keys.split('\n').enumerate() {
        if index > 0 {
            output.push('\n');
        }
        let (line, line_report) = redact_line(line);
        report.merge(&line_report);
        output.push_str(&line);
    }
    (output, report)
}

/// Redact every string value in a JSON value. Object keys that look sensitive
/// have their whole value replaced, which is safer than trying to redact a
/// nested structure field-by-field. JSON types and shape are preserved.
pub fn redact_json(value: &mut serde_json::Value) -> RedactionReport {
    let mut report = RedactionReport::default();
    redact_json_inner(value, &mut report);
    report
}

fn redact_json_inner(value: &mut serde_json::Value, report: &mut RedactionReport) {
    match value {
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let Some(child) = map.get_mut(&key) else {
                    continue;
                };
                if is_sensitive_key(&key) {
                    *child = serde_json::Value::String(REDACTED.to_string());
                    report.record("sensitive_key");
                } else {
                    redact_json_inner(child, report);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_inner(item, report);
            }
        }
        serde_json::Value::String(text) => {
            let (redacted, nested) = redact_text(text);
            *text = redacted;
            report.merge(&nested);
        }
        _ => {}
    }
}

fn redact_line(line: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let (line, url_report) = redact_urls(line);
    report.merge(&url_report);
    let (line, kv_report) = redact_key_values(&line);
    report.merge(&kv_report);
    let (line, token_report) = redact_tokens(&line);
    report.merge(&token_report);
    (line, report)
}

/// Normalize a key for sensitive-name detection: lowercase ASCII alphanumerics
/// only. `API-Key`, `api_key` and `apiKey` all become `apikey`.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

/// Whether an object/config key conventionally carries a credential.
///
/// We deliberately do not treat a bare `key` as sensitive (it is often a
/// config-map key), but `api_key`, `access_token`, `db_password`, etc. are.
pub fn is_sensitive_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    if normalized.is_empty() {
        return false;
    }
    const EXACT: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "pwd",
        "authorization",
        "proxyauthorization",
        "cookie",
        "setcookie",
        "apikey",
        "privatekey",
        "accesskey",
        "secretkey",
        "clientsecret",
        "credential",
        "credentials",
    ];
    if EXACT.contains(&normalized.as_str()) {
        return true;
    }
    const SUFFIXES: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "apikey",
        "privatekey",
        "accesskey",
        "secretkey",
        "clientsecret",
        "credentials",
    ];
    SUFFIXES
        .iter()
        .any(|suffix| normalized.len() > suffix.len() && normalized.ends_with(suffix))
}

fn is_sensitive_header(key: &str) -> bool {
    matches!(
        normalize_key(key).as_str(),
        "authorization" | "proxyauthorization" | "cookie" | "setcookie"
    )
}

/// Replace PEM/PGP private-key blocks. Returns `(text, block_count)`.
fn redact_private_key_blocks(input: &str) -> (String, u64) {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    let mut count = 0u64;
    while let Some(begin) = rest.find("-----BEGIN ") {
        let after_begin = &rest[begin..];
        let header_end = after_begin.find('\n').unwrap_or(after_begin.len());
        let header = &after_begin[..header_end];
        if !header.contains("PRIVATE KEY") {
            output.push_str(&rest[..begin + 11]);
            rest = &rest[begin + 11..];
            continue;
        }
        output.push_str(&rest[..begin]);
        let end_marker = after_begin
            .find("-----END ")
            .and_then(|end_start| {
                after_begin[end_start..]
                    .find('\n')
                    .map(|n| end_start + n + 1)
            })
            .unwrap_or(after_begin.len());
        output.push_str(REDACTED);
        rest = &after_begin[end_marker..];
        count += 1;
    }
    output.push_str(rest);
    (output, count)
}

/// Redact URL userinfo and credential-looking query parameters per token.
fn redact_urls(line: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let mut output = String::with_capacity(line.len());
    let mut start = 0usize;
    for (index, ch) in line.char_indices() {
        if !ch.is_whitespace() {
            continue;
        }
        let (token, token_report) = redact_url_token(&line[start..index]);
        report.merge(&token_report);
        output.push_str(&token);
        output.push(ch);
        start = index + ch.len_utf8();
    }
    if start < line.len() {
        let (token, token_report) = redact_url_token(&line[start..]);
        report.merge(&token_report);
        output.push_str(&token);
    }
    (output, report)
}

fn redact_url_token(token: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let Some(scheme_end) = token.find("://") else {
        return (token.to_string(), report);
    };
    if scheme_end == 0 {
        return (token.to_string(), report);
    }

    let authority_start = scheme_end + 3;
    let mut out = token.to_string();
    let authority_end = out[authority_start..]
        .find(['/', '?', '#'])
        .map(|offset| authority_start + offset)
        .unwrap_or(out.len());

    if let Some(at_offset) = out[authority_start..authority_end].rfind('@') {
        let at = authority_start + at_offset;
        out.replace_range(authority_start..at, REDACTED);
        report.record("url_userinfo");
    }

    if let Some(query_start) = out.find('?') {
        let (head, query_and_fragment) = out.split_at(query_start + 1);
        let mut query = String::with_capacity(query_and_fragment.len());
        let (params, fragment) = match query_and_fragment.find('#') {
            Some(index) => (&query_and_fragment[..index], &query_and_fragment[index..]),
            None => (query_and_fragment, ""),
        };
        for (index, param) in params.split('&').enumerate() {
            if index > 0 {
                query.push('&');
            }
            let Some((key, _value)) = param.split_once('=') else {
                query.push_str(param);
                continue;
            };
            if is_sensitive_key(key) {
                query.push_str(key);
                query.push('=');
                query.push_str(REDACTED);
                report.record("url_query_credential");
            } else {
                query.push_str(param);
            }
        }
        query.push_str(fragment);
        out = format!("{head}{query}");
    }

    (out, report)
}

/// Redact values for sensitive keys. Handles `key=value`, `"key": "value"`,
/// `key: value`, and `--key value`.
fn redact_key_values(line: &str) -> (String, RedactionReport) {
    let bytes = line.as_bytes();
    let mut ranges: Vec<(usize, usize, String)> = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if !is_word_byte(bytes[index]) {
            index += 1;
            continue;
        }
        let key_start = index;
        while index < bytes.len() && is_word_byte(bytes[index]) {
            index += 1;
        }
        let key_end = index;
        let key = &line[key_start..key_end];
        if !is_sensitive_key(key) {
            continue;
        }

        // Skip optional spaces and one `=`/`:` separator. A quoted key is not
        // included in the word scan, so this stays on the value side.
        let mut cursor = key_end;
        // A quoted object key has its closing quote between the key and `:`.
        if cursor < bytes.len() && matches!(bytes[cursor], b'"' | b'\'') {
            cursor += 1;
        }
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }
        let value_start = if cursor < bytes.len() && matches!(bytes[cursor], b'=' | b':') {
            cursor += 1;
            while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
                cursor += 1;
            }
            cursor
        } else {
            // Support CLI-style `--token value` only; do not swallow prose.
            let prefix_is_flag =
                key.starts_with('-') || line[..key_start].trim_end().ends_with('-');
            if !prefix_is_flag || cursor == key_end {
                continue;
            }
            cursor
        };

        if value_start >= bytes.len() {
            continue;
        }
        let end = if is_sensitive_header(key) {
            line.len()
        } else if matches!(bytes[value_start], b'"' | b'\'') {
            let quote = bytes[value_start];
            let mut end = value_start + 1;
            let mut escaped = false;
            while end < bytes.len() {
                let byte = bytes[end];
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == quote {
                    end += 1;
                    break;
                }
                end += 1;
            }
            end
        } else {
            let mut end = value_start;
            while end < bytes.len()
                && !matches!(bytes[end], b' ' | b'\t' | b',' | b';' | b'&' | b'#' | b'\r')
            {
                end += 1;
            }
            end
        };
        if end <= value_start {
            continue;
        }

        let quoted = matches!(bytes[value_start], b'"' | b'\'');
        let replacement = if quoted {
            format!(
                "{}{}{}",
                bytes[value_start] as char, REDACTED, bytes[value_start] as char
            )
        } else {
            REDACTED.to_string()
        };
        ranges.push((value_start, end, replacement));
        index = end;
    }

    if ranges.is_empty() {
        return (line.to_string(), RedactionReport::default());
    }
    let mut report = RedactionReport::default();
    let mut output = String::with_capacity(line.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in ranges {
        if start < cursor {
            continue;
        }
        output.push_str(&line[cursor..start]);
        output.push_str(&replacement);
        cursor = end;
        report.record("key_value");
    }
    output.push_str(&line[cursor..]);
    (output, report)
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

/// Redact bearer/basic credentials, well-known token prefixes and JWTs.
fn redact_tokens(line: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let mut output = String::with_capacity(line.len());
    let mut cursor = 0usize;
    let mut redact_next = false;
    for (index, ch) in line.char_indices() {
        if !ch.is_whitespace() {
            continue;
        }
        let (token, token_report) = redact_one_token(&line[cursor..index], redact_next);
        report.merge(&token_report);
        let lower = token.to_ascii_lowercase();
        redact_next = matches!(lower.trim_matches(is_token_punctuation), "bearer" | "basic");
        output.push_str(&token);
        output.push(ch);
        cursor = index + ch.len_utf8();
    }
    if cursor < line.len() {
        let (token, token_report) = redact_one_token(&line[cursor..], redact_next);
        report.merge(&token_report);
        output.push_str(&token);
    }
    (output, report)
}

fn redact_one_token(token: &str, force: bool) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let trimmed = token.trim_matches(is_token_punctuation);
    if trimmed.is_empty() {
        return (token.to_string(), report);
    }
    let should_redact = force || looks_like_secret(trimmed);
    if !should_redact {
        return (token.to_string(), report);
    }
    let start = token.find(trimmed).unwrap_or(0);
    let end = start + trimmed.len();
    let rule = if force {
        "auth_scheme_value"
    } else if is_jwt(trimmed) {
        "jwt"
    } else {
        "known_token_prefix"
    };
    report.record(rule);
    (
        format!("{}{}{}", &token[..start], REDACTED, &token[end..]),
        report,
    )
}

fn is_token_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';' | ':' | '.'
    )
}

fn looks_like_secret(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxa-",
        "xoxb-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "aiza",
        "ya29.",
        "glpat-",
        "npm_",
        "pypi-",
    ];
    if PREFIXES.iter().any(|prefix| lower.starts_with(prefix)) {
        return true;
    }
    if (token.starts_with("AKIA") || token.starts_with("ASIA"))
        && token.len() == 20
        && token[4..].bytes().all(|byte| byte.is_ascii_uppercase())
    {
        return true;
    }
    is_jwt(token)
}

fn is_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3
        && parts[0].starts_with("eyJ")
        && parts[0].len() >= 8
        && parts[1].len() >= 8
        && parts[2].len() >= 8
        && parts.iter().all(|part| part.bytes().all(is_base64url_byte))
}

fn is_base64url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'=')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_clean(output: &str, secrets: &[&str]) {
        for secret in secrets {
            assert!(
                !output.contains(secret),
                "secret leaked: {secret:?} in {output:?}"
            );
        }
    }

    #[test]
    fn redacts_key_value_pairs_and_preserves_format() {
        let input = r#"api_key = "sk-live-1234567890"
password: hunter2
TOKEN=abc.def.ghi
fine = 42
"#;
        let (output, report) = redact_text(input);
        assert_clean(&output, &["sk-live", "hunter2", "abc.def.ghi"]);
        assert!(report.matches >= 3);
        assert!(output.contains("fine = 42"));
        assert!(output.contains(REDACTED));
    }

    #[test]
    fn redacts_url_userinfo_and_query_credentials() {
        let input = "fetch https://alice:s3cr3t@example.com/x?token=tok123&ok=1&api_key=k9#frag";
        let (output, report) = redact_text(input);
        assert_clean(&output, &["alice", "s3cr3t", "tok123", "k9"]);
        assert!(
            output.contains("example.com/x?token=[REDACTED]&ok=1&api_key=[REDACTED]#frag"),
            "unexpected url redaction: {output:?}"
        );
        assert!(report.rules.contains_key("url_userinfo"));
    }

    #[test]
    fn redacts_bearer_and_known_prefixes() {
        let input =
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature123\n\
                     curl -H 'X: ghp_abcdefghijklmnopqrstuvwxyz0123456789' https://x";
        let (output, report) = redact_text(input);
        assert_clean(
            &output,
            &[
                "eyJhbGciOiJIUzI1NiJ9",
                "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            ],
        );
        assert!(report.matches >= 2);
    }

    #[test]
    fn redacts_private_key_blocks() {
        let input = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let (output, report) = redact_text(input);
        assert_clean(&output, &["AAAA", "BBBB"]);
        assert!(output.contains("before"));
        assert!(output.contains("after"));
        assert_eq!(report.rules.get("private_key_block"), Some(&1));
    }

    #[test]
    fn redacts_nested_json_values() {
        let mut value = serde_json::json!({
            "ok": 1,
            "headers": {"Authorization": "Bearer abcdefghijklmnop", "X-Trace": "keep"},
            "url": "ssh://user:pass@host/repo",
            "nested": [{"password": "pw"}, {"note": "token=topsecret"}],
        });
        let report = redact_json(&mut value);
        let text = serde_json::to_string(&value).unwrap();
        assert_clean(&text, &["abcdefghijklmnop", "user:pass", "pw", "topsecret"]);
        assert!(report.matches >= 3);
        assert_eq!(value["ok"], 1);
        assert_eq!(value["headers"]["X-Trace"], "keep");
    }

    #[test]
    fn redaction_is_idempotent_and_does_not_touch_plain_text() {
        let input = "nothing secret here, just a key and a tokenizer.";
        let (output, report) = redact_text(input);
        assert_eq!(output, input);
        assert!(report.is_empty());

        let input = "api_key=abc123 url=https://u:p@h/x";
        let (once, _) = redact_text(input);
        let (twice, _) = redact_text(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn sensitive_keys_are_narrow_but_cover_conventions() {
        for key in [
            "api_key",
            "apiKey",
            "ACCESS_TOKEN",
            "refresh-token",
            "db_password",
            "AWS_SECRET_ACCESS_KEY",
            "client_secret",
            "private_key",
            "Authorization",
            "cookie",
        ] {
            assert!(is_sensitive_key(key), "{key} should be sensitive");
        }
        for key in ["key", "monkey", "tokenizer", "public_key", "max_tokens"] {
            assert!(!is_sensitive_key(key), "{key} should not be sensitive");
        }
    }

    #[test]
    fn redacts_cli_style_flags() {
        let (output, report) = redact_text("tervo --token abcdef --verbose");
        assert_clean(&output, &["abcdef"]);
        assert!(output.contains("--verbose"));
        assert!(report.matches >= 1);
    }
}
