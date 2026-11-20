// SPDX-License-Identifier: GPL-3.0-only
//! 轻量 OpenSSH client config 解析（native SSH 用，替代 `ssh -G`）。
//!
//! 只解析 native 后端真正需要/必须拒绝的选项：HostName/Port/User、
//! ProxyJump/ProxyCommand、ForwardAgent、端口转发、known_hosts 文件、
//! StrictHostKeyChecking、HostKeyAlias。其它未知选项保留默认行为；遇到
//! `Match`/`Include` 无法完整支持时标记 unsupported，让调用方回退系统 ssh。

use std::path::{Path, PathBuf};

use crate::profile::Profile;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSsh {
    pub hostname: String,
    pub port: u16,
    pub user: String,
    pub proxy_jump: String,
    pub proxy_command: String,
    pub strict: String,
    pub user_known_hosts: Vec<PathBuf>,
    pub global_known_hosts: Vec<PathBuf>,
    pub host_key_alias: Option<String>,
    pub forward_agent: bool,
    pub identity_files: Vec<String>,
    pub keepalive_interval: Option<std::time::Duration>,
    pub keepalive_max: usize,
    /// 非空时 native 无法保持 OpenSSH 语义，调用方应回退系统 ssh。
    pub unsupported: Option<String>,
}

impl ResolvedSsh {
    pub fn host_key_spec(&self) -> String {
        match &self.host_key_alias {
            Some(alias) if !alias.is_empty() => alias.clone(),
            _ => crate::known_hosts::host_spec(&self.hostname, self.port),
        }
    }
}

#[derive(Default)]
struct Raw {
    hostname: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    proxy_jump: Option<String>,
    proxy_command: Option<String>,
    strict: Option<String>,
    user_known_hosts: Option<Vec<PathBuf>>,
    global_known_hosts: Option<Vec<PathBuf>>,
    host_key_alias: Option<String>,
    forward_agent: Option<bool>,
    identity_files: Vec<String>,
    keepalive_interval: Option<u64>,
    keepalive_max: Option<usize>,
    unsupported: Option<String>,
}

/// 生产路径：`TERVO_SSH_CONFIG`（测试/覆盖）或 ~/.ssh/config + /etc/ssh/ssh_config。
pub fn resolve(profile: &Profile) -> ResolvedSsh {
    let paths = match std::env::var_os("TERVO_SSH_CONFIG") {
        Some(path) if !path.is_empty() => vec![PathBuf::from(path)],
        _ => default_config_paths(),
    };
    resolve_with_paths(profile, &paths)
}

pub fn resolve_with_paths(profile: &Profile, paths: &[PathBuf]) -> ResolvedSsh {
    let mut raw = Raw::default();
    let mut active = true;
    for path in paths {
        let root = config_root(path);
        parse_file(path, profile.host.trim(), &mut raw, &mut active, &root, 0);
    }
    finish(profile, raw, true)
}

/// ProxyJump 主机：未显式给端口时允许 ssh_config 的 Port。
pub fn resolve_jump(profile: &Profile) -> ResolvedSsh {
    let paths = match std::env::var_os("TERVO_SSH_CONFIG") {
        Some(path) if !path.is_empty() => vec![PathBuf::from(path)],
        _ => default_config_paths(),
    };
    resolve_jump_with_paths(profile, &paths)
}

fn resolve_jump_with_paths(profile: &Profile, paths: &[PathBuf]) -> ResolvedSsh {
    let mut raw = Raw::default();
    let mut active = true;
    for path in paths {
        let root = config_root(path);
        parse_file(path, profile.host.trim(), &mut raw, &mut active, &root, 0);
    }
    finish(profile, raw, false)
}

/// 测试用：直接喂文本，路径只用于 Include 相对路径。
#[cfg(test)]
fn resolve_with_text(profile: &Profile, texts: &[&str]) -> ResolvedSsh {
    let mut raw = Raw::default();
    let mut active = true;
    let root = Path::new(".");
    for text in texts {
        parse_text(text, profile.host.trim(), &mut raw, &mut active, root, 0);
    }
    finish(profile, raw, true)
}

fn finish(profile: &Profile, raw: Raw, prefer_profile_port: bool) -> ResolvedSsh {
    let hostname = raw
        .hostname
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| profile.host.trim().to_string());
    // 系统 ssh 路径始终传 `-p profile.port`；native 保持一致，不能让
    // ssh_config 的 Port 悄悄把目标改成另一个服务。
    let port = if prefer_profile_port {
        profile.port.max(1)
    } else {
        raw.port.unwrap_or_else(|| if profile.port == 0 { 22 } else { profile.port })
    };
    let user = profile
        .user
        .trim()
        .to_string()
        .or_nonempty(raw.user)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".into());
    let strict = raw.strict.unwrap_or_else(|| "ask".into()).to_ascii_lowercase();
    let user_known_hosts = raw.user_known_hosts.unwrap_or_else(default_user_known_hosts);
    let global_known_hosts =
        raw.global_known_hosts.unwrap_or_else(default_global_known_hosts);
    let keepalive_interval = raw
        .keepalive_interval
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs);
    let keepalive_max = raw.keepalive_max.unwrap_or(3);
    let unsupported = raw
        .unsupported
        .or_else(|| {
            if raw.forward_agent == Some(true) {
                Some("ForwardAgent".into())
            } else {
                None
            }
        })
        ;
    let proxy_jump = if profile.proxy_jump.trim().is_empty() {
        raw.proxy_jump.unwrap_or_default()
    } else {
        profile.proxy_jump.trim().to_string()
    };
    ResolvedSsh {
        hostname,
        port,
        user,
        proxy_jump,
        proxy_command: raw.proxy_command.unwrap_or_default(),
        keepalive_interval,
        keepalive_max,
        strict,
        user_known_hosts,
        global_known_hosts,
        host_key_alias: raw.host_key_alias,
        forward_agent: raw.forward_agent.unwrap_or(false),
        identity_files: raw.identity_files,
        unsupported,
    }
}

trait OrNonEmpty {
    fn or_nonempty(self, fallback: Option<String>) -> Option<String>;
}
impl OrNonEmpty for String {
    fn or_nonempty(self, fallback: Option<String>) -> Option<String> {
        if self.is_empty() {
            fallback
        } else {
            Some(self)
        }
    }
}

fn default_config_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(home).join(".ssh/config"));
    }
    out.push(PathBuf::from("/etc/ssh/ssh_config"));
    out
}

fn default_user_known_hosts() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let ssh = PathBuf::from(home).join(".ssh");
    vec![ssh.join("known_hosts"), ssh.join("known_hosts2")]
}

fn default_global_known_hosts() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/etc/ssh/ssh_known_hosts"),
        PathBuf::from("/etc/ssh/ssh_known_hosts2"),
    ]
}

fn config_root(path: &Path) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let user = PathBuf::from(home).join(".ssh");
        if path.starts_with(&user) {
            return user;
        }
    }
    if path.starts_with("/etc/ssh") {
        return PathBuf::from("/etc/ssh");
    }
    path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

fn parse_file(
    path: &Path,
    alias: &str,
    raw: &mut Raw,
    active: &mut bool,
    root: &Path,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    parse_text_inner(&text, alias, raw, active, depth, Some(path), root);
}

fn parse_text(
    text: &str,
    alias: &str,
    raw: &mut Raw,
    active: &mut bool,
    root: &Path,
    depth: usize,
) {
    parse_text_inner(text, alias, raw, active, depth, None, root);
}

fn parse_text_inner(
    text: &str,
    alias: &str,
    raw: &mut Raw,
    active: &mut bool,
    depth: usize,
    source: Option<&Path>,
    root: &Path,
) {
    for line in text.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = split_directive(line) else { continue };
        let key = key.to_ascii_lowercase();
        match key.as_str() {
            "host" => {
                let patterns = split_words(value);
                *active = host_matches(alias, &patterns);
            },
            "match" => {
                if *active {
                    raw.unsupported.get_or_insert_with(|| "Match".into());
                }
                *active = false;
            },
            "include" => {
                if !*active {
                    continue;
                }
                for include in expand_includes(value, source, root) {
                    parse_file(&include, alias, raw, active, root, depth + 1);
                }
            },
            _ if *active => apply_option(&key, value, raw),
            _ => {},
        }
    }
}

fn apply_option(key: &str, value: &str, raw: &mut Raw) {
    let value = unquote(value.trim());
    match key {
        "hostname" => {
            if value.contains('%') {
                raw.unsupported.get_or_insert_with(|| "HostName token".into());
            } else if raw.hostname.is_none() {
                raw.hostname = Some(value.to_string());
            }
        },
        "port" => {
            if raw.port.is_none() {
                if let Ok(port) = value.parse::<u16>() {
                    raw.port = Some(port);
                }
            }
        },
        "user" => {
            if raw.user.is_none() {
                raw.user = Some(value.to_string());
            }
        },
        "proxyjump" => {
            if raw.proxy_jump.is_none() {
                raw.proxy_jump = Some(if value.eq_ignore_ascii_case("none") {
                    String::new()
                } else {
                    value.to_string()
                });
            }
        },
        "proxycommand" => {
            if raw.proxy_command.is_none() {
                raw.proxy_command = Some(if value.eq_ignore_ascii_case("none") {
                    String::new()
                } else {
                    value.to_string()
                });
            }
        },
        "stricthostkeychecking" => {
            if raw.strict.is_none() {
                raw.strict = Some(value.to_ascii_lowercase());
            }
        },
        "userknownhostsfile" => {
            if raw.user_known_hosts.is_none() {
                raw.user_known_hosts = Some(parse_paths(&value));
            }
        },
        "globalknownhostsfile" => {
            if raw.global_known_hosts.is_none() {
                raw.global_known_hosts = Some(parse_paths(&value));
            }
        },
        "hostkeyalias" => {
            if raw.host_key_alias.is_none() {
                raw.host_key_alias = Some(value.to_string());
            }
        },
        "forwardagent" => {
            if raw.forward_agent.is_none() {
                raw.forward_agent = Some(matches!(value.to_ascii_lowercase().as_str(), "yes" | "true"));
            }
        },
        "serveraliveinterval" => {
            if raw.keepalive_interval.is_none() {
                if let Ok(seconds) = value.parse::<u64>() {
                    raw.keepalive_interval = Some(seconds);
                }
            }
        },
        "serveralivecountmax" => {
            if raw.keepalive_max.is_none() {
                if let Ok(max) = value.parse::<usize>() {
                    raw.keepalive_max = Some(max);
                }
            }
        },
        "identityfile" => {
            if !value.is_empty() {
                raw.identity_files.push(value.to_string());
            }
        },
        "localforward" | "remoteforward" | "dynamicforward" => {
            raw.unsupported.get_or_insert_with(|| key.to_string());
        },
        _ => {},
    }
}

fn parse_paths(value: &str) -> Vec<PathBuf> {
    value
        .split_whitespace()
        .filter(|p| !p.is_empty() && *p != "none")
        .map(expand_home)
        .collect()
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn expand_includes(value: &str, source: Option<&Path>, root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for token in split_words(value) {
        let mut path = expand_home(&token);
        if path.is_relative() {
            path = root.join(path);
        }
        if token.contains('*') || token.contains('?') {
            let (dir, pattern) = match path.parent() {
                Some(dir) => (dir.to_path_buf(), path.file_name().unwrap_or_default().to_string_lossy().to_string()),
                None => (PathBuf::new(), token.clone()),
            };
            let dir = if dir.as_os_str().is_empty() {
                source.and_then(|s| s.parent()).unwrap_or_else(|| Path::new(".")).to_path_buf()
            } else {
                dir
            };
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut matches: Vec<PathBuf> = entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|file| {
                        file.file_name()
                            .map(|name| glob_match(&pattern, &name.to_string_lossy()))
                            .unwrap_or(false)
                    })
                    .collect();
                matches.sort();
                out.extend(matches);
            }
        } else {
            out.push(path);
        }
    }
    out
}

fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            c if Some(c) == quote => quote = None,
            '#' if quote.is_none() => return &line[..i],
            _ => {},
        }
    }
    line
}

fn split_directive(line: &str) -> Option<(&str, &str)> {
    let mut split = line.splitn(2, |c: char| c.is_whitespace() || c == '=');
    let key = split.next()?.trim();
    let value = split.next().unwrap_or("").trim().trim_start_matches('=').trim();
    if key.is_empty() {
        None
    } else {
        Some((key, value))
    }
}

fn split_words(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escape = false;
    for ch in value.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }
        match ch {
            '\\' if quote != Some('\'') => escape = true,
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            c if Some(c) == quote => quote = None,
            c if c.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            },
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// OpenSSH host pattern matching: `*`/`?`, comma lists, `!` negation.
pub fn host_matches(host: &str, patterns: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    let mut positive = false;
    for pattern in patterns {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern.as_str()),
        };
        if pattern.is_empty() {
            continue;
        }
        if glob_match(&pattern.to_ascii_lowercase(), &host) {
            if negated {
                return false;
            }
            positive = true;
        }
    }
    positive
}

pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            b'?' => !t.is_empty() && go(&p[1..], &t[1..]),
            c => !t.is_empty() && c == t[0] && go(&p[1..], &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(host: &str) -> Profile {
        Profile { kind: "ssh".into(), host: host.into(), port: 22, ..Profile::default() }
    }

    #[test]
    fn host_alias_resolves_hostname_port_user() {
        let p = profile("alias");
        let r = resolve_with_text(
            &p,
            &["Host alias\n HostName real.example\n Port 2200\n User alice\n"],
        );
        assert_eq!(r.hostname, "real.example");
        // 系统 ssh 路径总是传 `-p profile.port`，配置文件 Port 不能改目标。
        assert_eq!(r.port, 22);
        assert_eq!(r.user, "alice");
        assert!(r.unsupported.is_none());
    }

    #[test]
    fn first_value_wins_and_negated_patterns() {
        let p = profile("prod");
        let r = resolve_with_text(
            &p,
            &["Host *\n Port 2201\nHost prod !bad\n Port 2202\n HostName h\n"],
        );
        assert_eq!(r.port, 22);
        assert_eq!(r.hostname, "h");
    }

    #[test]
    fn match_and_forwards_are_unsupported() {
        let p = profile("h");
        assert!(resolve_with_text(&p, &["Match all\n Port 22\n"]).unsupported.is_some());
        assert!(resolve_with_text(&p, &["Host *\n LocalForward 8080 localhost:80\n"]).unsupported.is_some());
        assert!(resolve_with_text(&p, &["Host *\n ProxyJump jump\n"]).proxy_jump == "jump");
    }

    #[test]
    fn proxy_none_blocks_wildcard_and_profile_jump_wins() {
        let p = profile("alias");
        let r = resolve_with_text(
            &p,
            &["Host alias\n ProxyJump none\nHost *\n ProxyJump jump.example\n"],
        );
        assert!(r.proxy_jump.is_empty(), "explicit none must not be overridden");
        let mut p = profile("alias");
        p.proxy_jump = "profile.jump".into();
        let r = resolve_with_text(&p, &["Host *\n ProxyJump config.jump\n"]);
        assert_eq!(r.proxy_jump, "profile.jump");
    }

    #[test]
    fn jump_port_defaults_to_22_without_config() {
        let mut p = profile("jump");
        p.port = 0;
        let r = resolve_jump_with_paths(&p, &[]);
        assert_eq!(r.port, 22);
    }

    #[test]
    fn server_alive_options_parse() {
        let p = profile("h");
        let r = resolve_with_text(
            &p,
            &["Host *\n ServerAliveInterval 30\n ServerAliveCountMax 5\n"],
        );
        assert_eq!(r.keepalive_interval, Some(std::time::Duration::from_secs(30)));
        assert_eq!(r.keepalive_max, 5);
        let r = resolve_with_text(&p, &["Host *\n ServerAliveInterval 0\n"]);
        assert_eq!(r.keepalive_interval, None);
    }

    #[test]
    fn quoted_and_equals_forms() {
        let p = profile("h");
        let r = resolve_with_text(&p, &["Host=h\n User = \"bob\"\nPort=2222\n"]);
        assert_eq!(r.user, "bob");
        assert_eq!(r.port, 22);
    }
}
