// SPDX-License-Identifier: GPL-3.0-only
//! 纯 Rust known_hosts 读取/匹配（native SSH 用，替代 ssh-keygen -F）。
//!
//! 支持 plain/hashed (`|1|salt|hash`) host、逗号列表、`*`/`?`、`!` 否定、
//! `@revoked` 与 `@cert-authority`。`@cert-authority` 不作为普通主机密钥；
//! 遇到未知 marker 保守跳过。

use std::path::Path;

use base64::Engine;
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::known_hosts::{host_spec, KnownKey};
use crate::ssh_config::ResolvedSsh;

type HmacSha1 = Hmac<Sha1>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lookup {
    pub known: Vec<KnownKey>,
    pub revoked: Vec<KnownKey>,
}

/// 读取 resolved.user_known_hosts + global_known_hosts，返回匹配该主机的 key。
pub fn lookup(resolved: &ResolvedSsh) -> Lookup {
    let mut known = Vec::new();
    let mut revoked = Vec::new();
    let candidates = candidates(resolved);
    let mut files: Vec<&Path> = resolved
        .user_known_hosts
        .iter()
        .chain(resolved.global_known_hosts.iter())
        .map(|p| p.as_path())
        .collect();
    files.dedup();
    for file in files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines() {
            let Some(entry) = parse_line(line) else {
                continue;
            };
            if !hosts_match(&entry.patterns, &candidates) {
                continue;
            }
            let key = KnownKey {
                key_type: entry.key_type,
                key: entry.key,
                fingerprint: String::new(),
            };
            if entry.revoked {
                revoked.push(key);
            } else if !entry.cert_authority {
                known.push(key);
            }
        }
    }
    sort_dedup(&mut known);
    sort_dedup(&mut revoked);
    Lookup { known, revoked }
}

fn sort_dedup(keys: &mut Vec<KnownKey>) {
    keys.sort_by(|a, b| (&a.key_type, &a.key).cmp(&(&b.key_type, &b.key)));
    keys.dedup_by(|a, b| a.key_type == b.key_type && a.key == b.key);
}

#[derive(Debug)]
struct Entry {
    patterns: Vec<String>,
    key_type: String,
    key: String,
    revoked: bool,
    cert_authority: bool,
}

fn parse_line(line: &str) -> Option<Entry> {
    let mut line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut revoked = false;
    let mut cert_authority = false;
    if let Some(rest) = line.strip_prefix('@') {
        let marker_end = rest.find(char::is_whitespace)?;
        let marker = &rest[..marker_end];
        line = rest[marker_end..].trim_start();
        match marker {
            "revoked" => revoked = true,
            "cert-authority" => cert_authority = true,
            _ => return None,
        }
    }
    let mut parts = line.split_whitespace();
    let patterns = parts.next()?.to_string();
    let key_type = parts.next()?.to_string();
    let key = parts.next()?.to_string();
    if key_type.is_empty() || key.is_empty() {
        return None;
    }
    Some(Entry {
        patterns: patterns.split(',').map(|s| s.to_string()).collect(),
        key_type,
        key,
        revoked,
        cert_authority,
    })
}

fn candidates(resolved: &ResolvedSsh) -> Vec<String> {
    // OpenSSH 只使用一个 host-key identity：HostKeyAlias 优先，否则
    // host（22）或 [host]:port（非 22）。不能把裸 host 和带端口条目混用。
    let host = resolved.hostname.to_ascii_lowercase();
    let spec = match &resolved.host_key_alias {
        Some(alias) if !alias.is_empty() => alias.to_ascii_lowercase(),
        _ => host_spec(&host, resolved.port).to_ascii_lowercase(),
    };
    vec![spec]
}

fn hosts_match(patterns: &[String], candidates: &[String]) -> bool {
    // 与 OpenSSH 一样：对每个候选 host 计算；同一行内 pattern 依次匹配。
    for candidate in candidates {
        let mut positive = false;
        let mut negated_hit = false;
        for pattern in patterns {
            let (negated, pattern) = match pattern.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, pattern.as_str()),
            };
            if pattern.is_empty() {
                continue;
            }
            let matched = if pattern.starts_with("|1|") {
                hashed_matches(pattern, candidate)
            } else {
                crate::ssh_config::glob_match(&pattern.to_ascii_lowercase(), candidate)
            };
            if matched {
                if negated {
                    negated_hit = true;
                } else {
                    positive = true;
                }
            }
        }
        if positive && !negated_hit {
            return true;
        }
    }
    false
}

fn hashed_matches(pattern: &str, candidate: &str) -> bool {
    let mut parts = pattern.split('|');
    // "", "1", salt, hash
    let _ = parts.next();
    let Some(version) = parts.next() else {
        return false;
    };
    let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else {
        return false;
    };
    if version != "1" {
        return false;
    }
    let (Ok(salt), Ok(hash)) = (
        base64::engine::general_purpose::STANDARD.decode(salt),
        base64::engine::general_purpose::STANDARD.decode(hash),
    ) else {
        return false;
    };
    let Ok(mut mac) = HmacSha1::new_from_slice(&salt) else {
        return false;
    };
    mac.update(candidate.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_config::ResolvedSsh;

    fn resolved(host: &str, port: u16) -> ResolvedSsh {
        ResolvedSsh {
            hostname: host.into(),
            port,
            user: "u".into(),
            proxy_jump: String::new(),
            proxy_command: String::new(),
            keepalive_interval: None,
            keepalive_max: 3,
            strict: "ask".into(),
            user_known_hosts: Vec::new(),
            global_known_hosts: Vec::new(),
            host_key_alias: None,
            forward_agent: false,
            identity_files: Vec::new(),
            unsupported: None,
        }
    }

    #[test]
    fn plain_bracket_and_port_match() {
        let r = resolved("example.com", 22);
        let names = candidates(&r);
        assert!(hosts_match(&["example.com".into()], &names));
        assert!(!hosts_match(&["other.com".into()], &names));
        let r = resolved("example.com", 2222);
        assert!(hosts_match(&["[example.com]:2222".into()], &candidates(&r)));
        assert!(!hosts_match(&["example.com".into()], &candidates(&r)));
        let mut alias = resolved("example.com", 22);
        alias.host_key_alias = Some("pinned-alias".into());
        assert_eq!(candidates(&alias), vec!["pinned-alias"]);
        assert!(!hosts_match(&["example.com".into()], &candidates(&alias)));
    }

    #[test]
    fn wildcard_negation_and_hashed() {
        let r = resolved("example.com", 22);
        let c = candidates(&r);
        assert!(hosts_match(&["*.com".into()], &c));
        assert!(!hosts_match(&["*.com,!example.com".into()], &c));
        // salt=0123456789abcdef, HMAC-SHA1("example.com")
        assert!(hashed_matches(
            "|1|MDEyMzQ1Njc4OWFiY2RlZg==|SRLY8gP13Q2WpJBR1mnYWd8QETQ=",
            "example.com"
        ));
        assert!(!hashed_matches(
            "|1|MDEyMzQ1Njc4OWFiY2RlZg==|SRLY8gP13Q2WpJBR1mnYWd8QETQ=",
            "other.com"
        ));
    }

    #[test]
    fn revocations_and_cert_authority_parse() {
        let revoked = parse_line("@revoked\texample.com\tssh-ed25519\tAAAA").unwrap();
        assert!(revoked.revoked);
        let ca = parse_line("@cert-authority example.com ssh-ed25519 AAAA").unwrap();
        assert!(ca.cert_authority);
        assert!(parse_line("# comment").is_none());
    }
}
