// SPDX-License-Identifier: GPL-3.0-only
//! Tabby `config.yaml` 的尽力导入/导出。
//!
//! 官方 Tabby 的配置结构随版本变化（网络不可用，无法逐版核对），这里只映射
//! **Profile 的稳定子集**：local / ssh / serial / telnet，以及 `terminal.fontSize`。
//! 密码等秘密字段不会导入（没有 Vault），未知类型/字段只计入报告，不静默丢弃。

use crate::profile::{Profile, ProfileStore};
use serde_yaml::{Mapping, Value};

/// 导入结果（用于 CLI 输出与 UI 状态行）。
#[derive(Debug, Default)]
pub struct ImportReport {
    pub added: usize,
    /// 重名后自动改名（`name (tabby 2)`）的数量。
    pub renamed: usize,
    /// (Profile 名, 跳过原因)
    pub skipped: Vec<(String, String)>,
    /// 带过来的字号（仅当 Tabby 配置里有 `terminal.fontSize`）。
    pub font_size: Option<f32>,
}

fn as_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn as_u16(v: Option<&Value>) -> Option<u16> {
    match v {
        Some(Value::Number(n)) => n.as_u64().and_then(|x| u16::try_from(x).ok()),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_string_seq(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_sequence)
        .map(|seq| seq.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn as_env(v: Option<&Value>) -> Vec<(String, String)> {
    let Some(map) = v.and_then(Value::as_mapping) else { return Vec::new() };
    map.iter()
        .filter_map(|(k, val)| {
            let key = k.as_str()?;
            let value = val.as_str().or_else(|| val.as_u64().map(|_| ""))?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// 解析 Tabby YAML 并合并进 `store`（调用方负责 `store.save()`）。
pub fn import_yaml(text: &str, store: &mut ProfileStore) -> Result<ImportReport, String> {
    let root: Value = serde_yaml::from_str(text).map_err(|e| format!("YAML 解析失败: {e}"))?;
    let profiles = root
        .get("profiles")
        .and_then(Value::as_sequence)
        .ok_or_else(|| "配置里没有 profiles 列表".to_string())?;

    let mut report = ImportReport::default();
    for (idx, item) in profiles.iter().enumerate() {
        let raw_name = as_str(item.get("name")).unwrap_or("").to_string();
        let name = if raw_name.is_empty() { format!("tabby-{idx}") } else { raw_name };
        let ty = as_str(item.get("type")).unwrap_or("");
        let opts = item.get("options");

        let mut p = Profile { name: name.clone(), ..Profile::default() };
        match ty {
            "local" => {
                p.kind = "local".into();
                p.shell = as_str(opts.and_then(|o| o.get("shell"))).unwrap_or("").to_string();
                p.args = as_string_seq(opts.and_then(|o| o.get("args")));
                if let Some(cwd) = as_str(opts.and_then(|o| o.get("cwd"))) {
                    if cwd != "~" {
                        p.cwd = Some(cwd.to_string());
                    }
                }
                p.env = as_env(opts.and_then(|o| o.get("env")));
            },
            "ssh" => {
                p.kind = "ssh".into();
                p.host = as_str(opts.and_then(|o| o.get("host"))).unwrap_or("").to_string();
                p.port = as_u16(opts.and_then(|o| o.get("port"))).unwrap_or(22);
                p.user = as_str(opts.and_then(|o| o.get("user"))).unwrap_or("").to_string();
                p.auth = match as_str(opts.and_then(|o| o.get("auth"))) {
                    Some("key") => "key".into(),
                    Some("password") => "password".into(),
                    _ => "agent".into(),
                };
                p.key_path = as_str(opts.and_then(|o| o.get("privateKey")))
                    .or_else(|| as_str(opts.and_then(|o| o.get("privateKeyPath"))))
                    .unwrap_or("")
                    .to_string();
            },
            "serial" => {
                p.kind = "serial".into();
                p.serial_port = as_str(opts.and_then(|o| o.get("port")))
                    .or_else(|| as_str(opts.and_then(|o| o.get("path"))))
                    .unwrap_or("")
                    .to_string();
                p.baud = as_u16(opts.and_then(|o| o.get("baudRate")))
                    .map(u32::from)
                    .unwrap_or(115200);
            },
            "telnet" => {
                p.kind = "telnet".into();
                p.host = as_str(opts.and_then(|o| o.get("host"))).unwrap_or("").to_string();
                p.port = as_u16(opts.and_then(|o| o.get("port"))).unwrap_or(23);
            },
            other => {
                report.skipped.push((
                    name,
                    format!("不支持的 Profile 类型: {other}（只导入 local/ssh/serial/telnet）"),
                ));
                continue;
            },
        }

        // 重名不覆盖已有 Profile：加后缀，报告里计数。
        let base = p.name.clone();
        let mut candidate = base.clone();
        let mut n = 2;
        while store.profiles.iter().any(|x| x.name == candidate) {
            candidate = format!("{base} (tabby {n})");
            n += 1;
        }
        p.name = candidate.clone();
        match store.add(p) {
            Ok(_) => {
                report.added += 1;
                if candidate != base {
                    report.renamed += 1;
                }
            },
            Err(e) => report.skipped.push((base, e)),
        }
    }

    report.font_size = root
        .get("terminal")
        .and_then(|t| t.get("fontSize"))
        .and_then(Value::as_f64)
        .map(|v| v as f32);
    Ok(report)
}

/// 导出成 Tabby 可读的 YAML（只含我们支持的 Profile 字段）。
pub fn export_yaml(store: &ProfileStore) -> String {
    let mut profiles = Vec::new();
    for p in &store.profiles {
        let mut m = Mapping::new();
        m.insert("name".into(), p.name.clone().into());
        m.insert("type".into(), p.kind.clone().into());
        let mut opts = Mapping::new();
        match p.kind.as_str() {
            "ssh" => {
                opts.insert("host".into(), p.host.clone().into());
                opts.insert("port".into(), (p.port as u64).into());
                opts.insert("user".into(), p.user.clone().into());
                opts.insert("auth".into(), p.auth.clone().into());
                if !p.key_path.is_empty() {
                    opts.insert("privateKey".into(), p.key_path.clone().into());
                }
            },
            "serial" => {
                opts.insert("port".into(), p.serial_port.clone().into());
                opts.insert("baudRate".into(), (p.baud as u64).into());
            },
            "telnet" => {
                opts.insert("host".into(), p.host.clone().into());
                opts.insert("port".into(), (p.port as u64).into());
            },
            _ => {
                opts.insert("shell".into(), p.shell.clone().into());
                opts.insert("args".into(), Value::Sequence(p.args.iter().map(|a| a.clone().into()).collect()));
                if let Some(cwd) = &p.cwd {
                    opts.insert("cwd".into(), cwd.clone().into());
                }
                if !p.env.is_empty() {
                    let mut env = Mapping::new();
                    for (k, v) in &p.env {
                        env.insert(k.clone().into(), v.clone().into());
                    }
                    opts.insert("env".into(), Value::Mapping(env));
                }
            },
        }
        m.insert("options".into(), Value::Mapping(opts));
        profiles.push(Value::Mapping(m));
    }
    let mut root = Mapping::new();
    root.insert("version".into(), 1u64.into());
    root.insert("profiles".into(), Value::Sequence(profiles));
    serde_yaml::to_string(&Value::Mapping(root)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        r#"
version: 1
profiles:
  - name: Local
    type: local
    options:
      shell: /bin/zsh
      args: ["-l"]
      cwd: /tmp
      env:
        FOO: bar
  - name: Server
    type: ssh
    options:
      host: example.com
      port: 2222
      user: yt
      auth: key
      privateKey: /home/user/.ssh/id_ed25519
  - name: Board
    type: serial
    options:
      port: /dev/ttyUSB0
      baudRate: 115200
  - name: BBS
    type: telnet
    options:
      host: 10.0.0.1
      port: 23
  - name: Weird
    type: kubernetes
    options: {}
terminal:
  fontSize: 16
"#
    }

    #[test]
    fn imports_all_supported_kinds_and_reports_unknown() {
        let mut store = ProfileStore::default();
        let r = import_yaml(sample(), &mut store).unwrap();
        assert_eq!(r.added, 4, "四个受支持类型都应导入: {r:?}");
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].1.contains("kubernetes"), "{:?}", r.skipped);
        assert_eq!(r.font_size, Some(16.0));

        let ssh = store.profiles.iter().find(|p| p.name == "Server").unwrap();
        assert_eq!(ssh.kind, "ssh");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.auth, "key");
        assert_eq!(ssh.key_path, "/home/user/.ssh/id_ed25519");

        let local = store.profiles.iter().find(|p| p.name == "Local").unwrap();
        assert_eq!(local.shell, "/bin/zsh");
        assert_eq!(local.args, vec!["-l"]);
        assert_eq!(local.cwd.as_deref(), Some("/tmp"));
        assert_eq!(local.env, vec![("FOO".to_string(), "bar".to_string())]);

        let serial = store.profiles.iter().find(|p| p.name == "Board").unwrap();
        assert_eq!(serial.serial_port, "/dev/ttyUSB0");
        assert_eq!(serial.baud, 115200);
    }

    #[test]
    fn duplicate_names_are_suffixed() {
        let mut store = ProfileStore::default();
        store.add(Profile { name: "Local".into(), ..Profile::default() }).unwrap();
        let r = import_yaml(sample(), &mut store).unwrap();
        assert_eq!(r.added, 4);
        assert_eq!(r.renamed, 1);
        assert!(store.profiles.iter().any(|p| p.name == "Local (tabby 2)"));
    }

    #[test]
    fn export_import_roundtrip() {
        let mut store = ProfileStore::default();
        store
            .add(Profile {
                name: "S".into(),
                kind: "ssh".into(),
                host: "h".into(),
                port: 2022,
                user: "u".into(),
                auth: "key".into(),
                key_path: "/k".into(),
                ..Profile::default()
            })
            .unwrap();
        store
            .add(Profile {
                name: "L".into(),
                kind: "local".into(),
                shell: "/bin/sh".into(),
                args: vec!["-c".into(), "true".into()],
                cwd: Some("/".into()),
                ..Profile::default()
            })
            .unwrap();
        let yaml = export_yaml(&store);
        let mut back = ProfileStore::default();
        let r = import_yaml(&yaml, &mut back).unwrap();
        assert_eq!(r.added, 2);
        assert_eq!(back.profiles.len(), 2);
        let ssh = back.profiles.iter().find(|p| p.name == "S").unwrap();
        assert_eq!((ssh.kind.as_str(), ssh.port, ssh.user.as_str()), ("ssh", 2022, "u"));
        let local = back.profiles.iter().find(|p| p.name == "L").unwrap();
        assert_eq!(local.args, vec!["-c".to_string(), "true".to_string()]);
        assert_eq!(local.cwd.as_deref(), Some("/"));
    }

    #[test]
    fn broken_or_missing_profiles_is_error() {
        let mut store = ProfileStore::default();
        assert!(import_yaml("profiles: [", &mut store).is_err());
        assert!(import_yaml("version: 1", &mut store).is_err());
        assert!(store.profiles.is_empty());
    }
}
