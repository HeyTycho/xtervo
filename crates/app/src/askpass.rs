// SPDX-License-Identifier: GPL-3.0-only
//! SSH askpass 辅助（MVP）。
//!
//! 系统 `ssh` 在 PTY 里跑；密码认证时给它设：
//! - `SSH_ASKPASS` → 临时脚本（0700，随机目录，脚本本身不含密码）
//! - `SSH_ASKPASS_REQUIRE=force` → 即使 stdin 是 tty 也走 askpass
//! - `TERVO_ASKPASS_SECRET` → 密码，只存在于 **ssh 子进程**的环境里
//! - `TERVO_ASKPASS_DEBUG`（可选，Tervo 进程的环境变量）→ 脚本每被调用一次就
//!   追加一行 prompt/匹配结果到该文件；**不写密码**，仅用于排查 askpass 是否
//!   被调用、prompt 长什么样。文件由用户显式指定，默认不写。
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

/// 可选的 askpass 调用诊断文件（用户从 Tervo 进程环境显式指定才生效）。
/// 脚本只写 prompt 与匹配结果，绝不写密码。
pub const DEBUG_ENV: &str = "TERVO_ASKPASS_DEBUG";

/// askpass 诊断是否启用（供启动日志提示；不打印路径以外的东西）。
pub fn debug_enabled() -> bool {
    std::env::var_os(DEBUG_ENV).map(|v| !v.is_empty()).unwrap_or(false)
}

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
        // TERVO_ASKPASS_DEBUG 是可选诊断：只记 prompt/匹配结果，绝不写密码。
        let body = r#"#!/bin/sh
prompt="$1"
case "$prompt" in
  *[Pp]assword*|*密码*)
    if [ -n "$TERVO_ASKPASS_DEBUG" ]; then
      printf '%s\n' "match=password prompt=$prompt" >>"$TERVO_ASKPASS_DEBUG" 2>/dev/null
    fi
    printf '%s\n' "$TERVO_ASKPASS_SECRET"
    ;;
  *)
    if [ -n "$TERVO_ASKPASS_DEBUG" ]; then
      printf '%s\n' "match=no prompt=$prompt" >>"$TERVO_ASKPASS_DEBUG" 2>/dev/null
    fi
    printf 'no\n'
    ;;
esac
"#;
        file.write_all(body.as_bytes())?;
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
/// 用 `ssh -G` 取实际生效的 `stricthostkeychecking` / `hostname` / `port` /
/// `hostkeyalias` / known_hosts 路径，再用 `ssh-keygen -F` 查主机条目。
/// 无法判断时返回 `false`（保守：先让用户在终端确认）。
///
/// 键的选择必须和 OpenSSH 自己的 `known_hosts` 查找一致，否则会出现
/// “known_hosts 明明有条目、Tervo 仍判定未确认”：
/// - 默认用 `ssh -G` 的 **`hostname`**（`Host` 别名经 `HostName` 解析后的真实主机），
///   而不是 Profile 里填的别名；非 22 端口加 `[host]:port` 后缀。
/// - 配了 `HostKeyAlias` 时，OpenSSH 直接拿 **别名原文** 查表（不附加 `[alias]:port`），
///   已用真实 `ssh` 验证。
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
    // ssh -G 的输出是“实际生效的值”，默认值先按 Profile 填。
    let mut match_host = host.to_string();
    let mut effective_port = profile.port;
    let mut host_key_alias: Option<String> = None;
    if let Ok(out) = Command::new(&prog).args(&gargs).output() {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(v) = line.strip_prefix("stricthostkeychecking ") {
                    strict = v.trim().to_ascii_lowercase();
                } else if let Some(v) = line.strip_prefix("userknownhostsfile ") {
                    files.extend(split_ssh_paths(v));
                } else if let Some(v) = line.strip_prefix("globalknownhostsfile ") {
                    files.extend(split_ssh_paths(v));
                } else if let Some(v) = line.strip_prefix("hostname ") {
                    let effective = v.trim();
                    if !effective.is_empty() {
                        match_host = effective.to_string();
                    }
                } else if let Some(v) = line.strip_prefix("port ") {
                    if let Ok(p) = v.trim().parse::<u16>() {
                        effective_port = p;
                    }
                } else if let Some(v) = line.strip_prefix("hostkeyalias ") {
                    let alias = v.trim();
                    if !alias.is_empty() {
                        host_key_alias = Some(alias.to_string());
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
    // HostKeyAlias 是 known_hosts 里原样的键（不套端口）；否则用解析后的 hostname。
    let spec = match host_key_alias {
        Some(alias) => alias,
        None => host_spec(&match_host, effective_port),
    };
    let found = files.iter().any(|file| ssh_keygen_finds(file, &spec));
    if !found {
        // 只打印查找键与候选文件（不含密码），便于排查 known_hosts 写到了别处。
        eprintln!(
            "[askpass] known_hosts 未命中：lookup={spec} strict={strict} files={files:?}"
        );
    }
    found
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
            .env_remove(DEBUG_ENV)
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
            run_script(&ask, "yt@example.com's password: ", password),
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

    /// 可选诊断文件：记录脚本是否被调用、prompt 与匹配分支，但绝不记录密码。
    #[test]
    fn debug_file_records_prompt_without_secret() {
        let dir = test_dir("debug-file");
        let debug = dir.join("askpass-debug.log");
        let password = "s3cret-debug";
        let ask = Askpass::new(password).unwrap();
        let run = |prompt: &str| {
            let out = std::process::Command::new("sh")
                .arg(ask.script_path())
                .arg(prompt)
                .env(SECRET_ENV, password)
                .env(DEBUG_ENV, &debug)
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(run("test@127.0.0.1's password: "), format!("{password}\n"));
        assert_eq!(
            run("Are you sure you want to continue connecting (yes/no/[fingerprint])? "),
            "no\n"
        );
        let log = std::fs::read_to_string(&debug).unwrap();
        assert!(log.contains("match=password prompt=test@127.0.0.1's password:"), "log={log:?}");
        assert!(log.contains("match=no prompt=Are you sure"), "log={log:?}");
        assert!(!log.contains(password), "诊断文件不能出现密码: {log:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 环境变量必须真正到达子进程（而不仅是 apply 到 Vec）。
    #[test]
    fn applied_envs_reach_real_child_process() {
        let mut ask = Askpass::new("child-secret").unwrap();
        let script = ask.script_path();
        let mut env = Vec::new();
        ask.apply(&mut env);
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(
            "printf '%s\\n%s\\n%s\\n' \"$SSH_ASKPASS\" \"$SSH_ASKPASS_REQUIRE\" \"$TERVO_ASKPASS_SECRET\"",
        );
        // 清掉父进程可能已有的同名变量，只保留 apply 注入的值。
        for key in ["SSH_ASKPASS", "SSH_ASKPASS_REQUIRE", SECRET_ENV] {
            cmd.env_remove(key);
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "stderr={:?}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        let script_path = script.to_string_lossy().into_owned();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec![script_path.as_str(), "force", "child-secret"]);
        assert!(script.is_file(), "SSH_ASKPASS 指向的脚本必须存在: {}", script.display());
        let _ = std::fs::remove_dir_all(ask.dir.clone());
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

    /// 测试用公钥；known_hosts 只按主机名匹配，不校验 key 内容。
    const TEST_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKNQBVooZwwKkPSd6IMylF98NDRrI/y3pjbO8e0z53Vy";

    /// 依赖系统 ssh/ssh-keygen；缺失时跳过（不把环境依赖变成硬失败）。
    fn have_ssh_tools() -> bool {
        Command::new("ssh").arg("-V").output().is_ok()
            && Command::new("ssh-keygen").arg("-V").output().is_ok()
    }

    fn test_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tervo-askpass-test-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_known_hosts(path: &std::path::Path, entries: &[&str]) {
        let body: String = entries.iter().map(|e| format!("{e}\n")).collect();
        std::fs::write(path, body).unwrap();
    }

    /// 隔离用户真实 `~/.ssh/config` 的空配置。
    fn empty_ssh_config(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("ssh_config");
        std::fs::write(&path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    #[test]
    fn host_key_known_uses_effective_known_hosts() {
        if !have_ssh_tools() {
            return;
        }
        let dir = test_dir("effective");
        let kh = dir.join("known_hosts");
        write_known_hosts(&kh, &[&format!("example.com {TEST_KEY}")]);
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

    /// 非 22 端口：known_hosts 键是 `[host]:port`，与 ssh 实际查找一致。
    #[test]
    fn host_key_known_matches_nonstandard_port_entry() {
        if !have_ssh_tools() {
            return;
        }
        let dir = test_dir("port-2222");
        let kh = dir.join("known_hosts");
        write_known_hosts(&kh, &[&format!("[127.0.0.1]:2222 {TEST_KEY}")]);
        let matches = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "test".into(),
            auth: "password".into(),
            extra_args: vec![
                "-o".into(),
                format!("UserKnownHostsFile={}", kh.display()),
                "-o".into(),
                "StrictHostKeyChecking=ask".into(),
            ],
            ..Profile::default()
        };
        assert!(host_key_known(&matches), "[host]:2222 条目应命中");
        let wrong_port = Profile { port: 2223, ..matches.clone() };
        assert!(!host_key_known(&wrong_port), "端口不同不能误判为已知");
        let wrong_host = Profile { host: "127.0.0.2".into(), ..matches };
        assert!(!host_key_known(&wrong_host), "主机不同不能误判为已知");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_keygen_matches_bracketed_port_spec() {
        if !have_ssh_tools() {
            return;
        }
        let dir = test_dir("keygen-port");
        let kh = dir.join("known_hosts");
        write_known_hosts(&kh, &[&format!("[127.0.0.1]:2222 {TEST_KEY}")]);
        assert!(
            ssh_keygen_finds(&kh, &host_spec("127.0.0.1", 2222)),
            "ssh-keygen -F 应命中 [127.0.0.1]:2222"
        );
        assert!(!ssh_keygen_finds(&kh, &host_spec("127.0.0.1", 22)), "端口必须参与匹配");
        assert!(!ssh_keygen_finds(&kh, &host_spec("127.0.0.2", 2222)), "主机必须参与匹配");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Host alias` + `HostName 127.0.0.1`：ssh 用解析后的主机名查 known_hosts，
    /// 不能用 Profile 里的别名（旧实现会误判“未确认”）。
    #[test]
    fn host_key_known_uses_resolved_hostname_from_ssh_config() {
        if !have_ssh_tools() {
            return;
        }
        let dir = test_dir("hostname-alias");
        let kh = dir.join("known_hosts");
        write_known_hosts(&kh, &[&format!("[127.0.0.1]:2222 {TEST_KEY}")]);
        let config = dir.join("ssh_config");
        std::fs::write(
            &config,
            format!(
                "Host vaultssh-alias\n  HostName 127.0.0.1\n  Port 2222\n  User test\n  \
                 UserKnownHostsFile {}\n  StrictHostKeyChecking ask\n",
                kh.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let profile = Profile {
            kind: "ssh".into(),
            host: "vaultssh-alias".into(),
            port: 2222,
            user: "test".into(),
            auth: "password".into(),
            extra_args: vec!["-F".into(), config.display().to_string()],
            ..Profile::default()
        };
        // 先确认 ssh 自己确实把 HostName 解析成了 127.0.0.1（测试前提）。
        let out = Command::new("ssh")
            .args(["-G", "-F"])
            .arg(&config)
            .arg("vaultssh-alias")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("hostname 127.0.0.1\n"),
            "ssh -G 未解析 HostName：{}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            host_key_known(&profile),
            "known_hosts 键是解析后的 127.0.0.1，不应拿别名查表"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `HostKeyAlias=myalias`：known_hosts 键是别名原文，不附加 `[alias]:port`。
    #[test]
    fn host_key_known_uses_host_key_alias_verbatim() {
        if !have_ssh_tools() {
            return;
        }
        let dir = test_dir("host-key-alias");
        let kh = dir.join("known_hosts");
        write_known_hosts(&kh, &[&format!("myalias {TEST_KEY}")]);
        let empty = empty_ssh_config(&dir);
        let profile = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "test".into(),
            auth: "password".into(),
            extra_args: vec![
                "-F".into(),
                empty.display().to_string(),
                "-o".into(),
                "HostKeyAlias=myalias".into(),
                "-o".into(),
                format!("UserKnownHostsFile={}", kh.display()),
                "-o".into(),
                "StrictHostKeyChecking=ask".into(),
            ],
            ..Profile::default()
        };
        assert!(
            host_key_known(&profile),
            "HostKeyAlias 应使用别名原文查 known_hosts（不含端口后缀）"
        );
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
