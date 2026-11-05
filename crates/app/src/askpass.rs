// SPDX-License-Identifier: GPL-3.0-only
//! SSH askpass 辅助（MVP）。
//!
//! 系统 `ssh` 在 PTY 里跑；密码认证时给它设：
//! - `SSH_ASKPASS` → 临时脚本（0700，随机目录，脚本本身不含密码）
//! - `SSH_ASKPASS_REQUIRE=force` → 即使 stdin 是 tty 也走 askpass
//! - `TERVO_ASKPASS_SECRET` → 密码，只存在于 **ssh 子进程**的环境里
//!
//! 临时脚本**只对密码提示**输出 `TERVO_ASKPASS_SECRET`；主机密钥确认等其它
//! prompt 一律回答 `no`。这是为了避免 OpenSSH 在 `SSH_ASKPASS_REQUIRE=force`
//! 下把密码当成主机密钥确认答案，从而无限重试（实测会刷出数千次 askpass 调用）。
//! 目录随 `Askpass` guard 一起删除（Pane 关闭/ssh 退出后）。
//!
//! # 已知安全限制（后续必须改）
//!
//! 密码现在经**子进程环境变量**传递。同一用户（或 root）可以通过
//! `/proc/<ssh-pid>/environ` 看到它，也可能从进程内存/环境 dump 里拿到。
//! 这比写明文文件好（不落盘），但**不是最终方案**：后续应改为
//! 继承 fd/pipe（或由 Tervo 主进程用 Unix socket 按次下发），
//! 并让 askpass 从 fd 读取，环境里只留 fd 号。见 `docs/VAULT.md`。
//!
//! 另一个 MVP 限制：`auth=password` 但未勾选「保存到 Vault」时不用 askpass，
//! 由 ssh 在终端里交互式询问密码（不经过 Vault、不落盘）。
//!
//! 首次连接（主机密钥不在 known_hosts）也不会启用 askpass：交给 ssh 在终端里
//! 正常显示 host key 指纹并让用户确认；用户手输一次密码后，密钥进入 known_hosts，
//! 后续连接才走 Vault + askpass。可用性检测见 [`host_key_known`]。

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::profile::Profile;

/// 传给 ssh 子进程的密码环境变量名（敏感；不要打印）。
pub const SECRET_ENV: &str = "TERVO_ASKPASS_SECRET";

/// 一个活着的 askpass 配置。`Drop` 时删除临时目录（脚本）。
pub struct Askpass {
    dir: PathBuf,
    envs: Vec<(String, String)>,
}

impl Askpass {
    /// 写入临时脚本并准备 ssh 环境变量。密码只进内存/环境，不写日志。
    pub fn new(password: &str) -> std::io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tervo-askpass-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir)?;
        let script = dir.join("askpass.sh");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o700);
        }
        let mut file = opts.open(&script)?;
        // 脚本不含秘密；只对密码 prompt 输出密码，其它确认（host key 等）回答 no，
        // 避免 ssh 在 force 模式下把密码当 yes/no 反复重试。
        file.write_all(
            b"#!/bin/sh\ncase \"$1\" in\n  *[Pp]assword*|*\xe5\xaf\x86\xe7\xa0\x81*) printf '%s\\n' \"$TERVO_ASKPASS_SECRET\" ;;\n  *) printf 'no\\n' ;;\nesac\n",
        )?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
        }
        let envs = vec![
            ("SSH_ASKPASS".to_string(), script.to_string_lossy().into_owned()),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            (SECRET_ENV.to_string(), password.to_string()),
        ];
        Ok(Self { dir, envs })
    }

    /// 把 env 移交给待启动的 ssh 子进程：同名键以 askpass 为准（防止用户配置覆盖）。
    /// 移交后 guard 不再持有密码，只保留临时目录用于清理。
    pub fn apply(&mut self, env: &mut Vec<(String, String)>) {
        for (key, value) in self.envs.drain(..) {
            env.retain(|(k, _)| k.as_str() != key.as_str());
            env.push((key, value));
        }
    }

    #[cfg(test)]
    fn script_path(&self) -> PathBuf {
        self.dir.join("askpass.sh")
    }
}

/// 主机密钥是否已经可以让 ssh 不再提示确认。
///
/// 用 `ssh -G` 取实际生效的 `stricthostkeychecking` / known_hosts 路径，再用
/// `ssh-keygen -F` 查主机条目。无法判断时返回 `false`（保守：先让用户在终端确认）。
///
/// 已知边界：`KnownHostsCommand` / `Match exec` 等动态配置、或 host 已存在但
/// 服务器换了密钥类型（known_hosts 有 host 条目、没有对应 key 类型）时，可能误判
/// “已知”而仍然出现确认 prompt；此时 askpass 脚本会对非密码 prompt 回答 `no`，
/// ssh 快速失败而不是死循环。用户需在终端手动连接一次。
pub fn host_key_known(profile: &Profile) -> bool {
    let host = profile.host.trim();
    if host.is_empty() {
        return false;
    }
    let (prog, args) = profile.ssh_command();
    let mut gargs = vec!["-G".to_string()];
    gargs.extend(args);
    let mut strict = "ask".to_string();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut match_host = host.to_string();
    if let Ok(out) = Command::new(&prog).args(&gargs).output() {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(v) = line.strip_prefix("stricthostkeychecking ") {
                    strict = v.trim().to_ascii_lowercase();
                } else if let Some(v) = line.strip_prefix("userknownhostsfile ") {
                    files.extend(split_ssh_paths(v));
                } else if let Some(v) = line.strip_prefix("globalknownhostsfile ") {
                    files.extend(split_ssh_paths(v));
                } else if let Some(v) = line.strip_prefix("hostkeyalias ") {
                    let alias = v.trim();
                    if !alias.is_empty() {
                        match_host = alias.to_string();
                    }
                }
            }
        }
    }
    // no / off / accept-new：不会弹 host key 确认（accept-new 自动信任新 key）。
    if strict != "ask" && strict != "yes" {
        return true;
    }
    if files.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            let ssh = PathBuf::from(home).join(".ssh");
            files.push(ssh.join("known_hosts"));
            files.push(ssh.join("known_hosts2"));
        }
    }
    let spec = host_spec(&match_host, profile.port);
    files.iter().any(|file| ssh_keygen_finds(file, &spec))
}

fn split_ssh_paths(value: &str) -> Vec<PathBuf> {
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

fn host_spec(host: &str, port: u16) -> String {
    if port == 22 || port == 0 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

fn ssh_keygen_finds(file: &PathBuf, host_spec: &str) -> bool {
    Command::new("ssh-keygen")
        .arg("-F")
        .arg(host_spec)
        .arg("-f")
        .arg(file)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

impl Drop for Askpass {
    fn drop(&mut self) {
        // 先覆盖环境里的密码引用（内存清零尽力而为），再删临时目录。
        for (_, value) in &mut self.envs {
            if !value.is_empty() {
                unsafe {
                    value.as_bytes_mut().fill(0);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl std::fmt::Debug for Askpass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 永不 Debug 出密码。
        f.debug_struct("Askpass").field("dir", &self.dir).field("envs", &"[redacted]").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_script(ask: &Askpass, prompt: &str, secret: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg(ask.script_path())
            .arg(prompt)
            .env(SECRET_ENV, secret)
            .output()
            .unwrap();
        assert!(out.status.success(), "stderr={:?}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn script_returns_secret_only_for_password_prompts() {
        let password = "p@ss w0rd \"$x\" \\n ; rm -rf /";
        let ask = Askpass::new(password).unwrap();
        let script = ask.script_path();
        let body = std::fs::read_to_string(&script).unwrap();
        assert!(!body.contains(password), "脚本本身不能含密码");
        assert!(body.contains("TERVO_ASKPASS_SECRET"));
        assert_eq!(
            run_script(&ask, "user@example.com's password: ", password),
            format!("{password}\n"),
            "密码 prompt 必须原样输出密码 + 换行"
        );
        assert_eq!(
            run_script(&ask, "请输入密码：", password),
            format!("{password}\n"),
            "中文密码 prompt 也要识别"
        );
        // 主机密钥确认等 prompt 绝不能拿到密码，也不能进入 yes/no 重试循环。
        for prompt in [
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "Please type 'yes', 'no' or the fingerprint: ",
            "Permission denied, please try again.",
        ] {
            let got = run_script(&ask, prompt, password);
            assert_eq!(got, "no\n", "prompt={prompt:?} 应回答 no");
            assert!(!got.contains(password));
        }
    }

    #[test]
    fn envs_set_force_and_are_redacted_from_debug() {
        let mut ask = Askpass::new("s3cret").unwrap();
        let mut env = vec![
            ("SSH_ASKPASS".to_string(), "/malicious".to_string()),
            ("TERM".to_string(), "xterm".to_string()),
        ];
        ask.apply(&mut env);
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert!(get("SSH_ASKPASS").unwrap().ends_with("askpass.sh"));
        assert_eq!(get("SSH_ASKPASS_REQUIRE").as_deref(), Some("force"));
        assert_eq!(get(SECRET_ENV).as_deref(), Some("s3cret"));
        assert_eq!(get("TERM").as_deref(), Some("xterm"));
        let debug = format!("{ask:?}");
        assert!(!debug.contains("s3cret"), "Debug 输出不能含密码: {debug}");
        // 移交后 guard 不再持有密码。
        assert!(ask.envs.is_empty());
        let dir = ask.dir.clone();
        drop(ask);
        assert!(!dir.exists(), "guard 释放后临时目录应删除");
    }

    #[test]
    fn host_key_known_uses_effective_known_hosts() {
        // 依赖系统 ssh/ssh-keygen；缺失时跳过（不把环境依赖变成硬失败）。
        if Command::new("ssh").arg("-V").output().is_err()
            || Command::new("ssh-keygen").arg("-V").output().is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tervo-askpass-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let kh = dir.join("known_hosts");
        std::fs::write(
            &kh,
            "example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKNQBVooZwwKkPSd6IMylF98NDRrI/y3pjbO8e0z53Vy\n",
        )
        .unwrap();
        let base = Profile {
            kind: "ssh".into(),
            host: "example.com".into(),
            port: 22,
            user: "u".into(),
            auth: "password".into(),
            extra_args: vec![
                "-o".into(),
                format!("UserKnownHostsFile={}", kh.display()),
                "-o".into(),
                "StrictHostKeyChecking=ask".into(),
            ],
            ..Profile::default()
        };
        assert!(host_key_known(&base), "known_hosts 命中时应允许 askpass");
        let unknown = Profile { host: "unknown.invalid".into(), ..base.clone() };
        assert!(!host_key_known(&unknown), "未确认的主机应回退到终端交互");
        let auto = Profile {
            host: "unknown.invalid".into(),
            extra_args: vec![
                "-o".into(),
                format!("UserKnownHostsFile={}", kh.display()),
                "-o".into(),
                "StrictHostKeyChecking=accept-new".into(),
            ],
            ..base
        };
        assert!(host_key_known(&auto), "accept-new 不会弹确认，可直接 askpass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_spec_uses_bracketed_port_except_22() {
        assert_eq!(host_spec("example.com", 22), "example.com");
        assert_eq!(host_spec("example.com", 0), "example.com");
        assert_eq!(host_spec("example.com", 2222), "[example.com]:2222");
        assert_eq!(host_spec("2001:db8::1", 2222), "[2001:db8::1]:2222");
        let paths = split_ssh_paths("none /tmp/kh2   ~/.ssh/known_hosts");
        assert_eq!(paths, vec![PathBuf::from("/tmp/kh2"), expand_home("~/.ssh/known_hosts")]);
    }

    #[cfg(unix)]
    #[test]
    fn script_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let ask = Askpass::new("pw").unwrap();
        let mode = std::fs::metadata(ask.script_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let dmode = std::fs::metadata(&ask.dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700);
    }
}
