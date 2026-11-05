//! SFTP 命令层：调用系统 `sftp`（batch 模式）完成列目录 / 上传 / 下载。
//!
//! 不做交互式 sftp：每次操作拼一段 batch 脚本，走 `sftp -q -b -`，
//! 靠退出码判断成败、靠 stderr 取错误信息。面板 UI 与传输进度在此基础上继续做。

use std::io::Write;
use std::process::{Command, Stdio};

use crate::profile::Profile;

/// 连接目标（从 SSH Profile 提取；SFTP 复用同一套认证参数）。
#[derive(Clone, Debug, PartialEq)]
pub struct SftpTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: String,
    pub extra_args: Vec<String>,
}

impl SftpTarget {
    pub fn from_profile(p: &Profile) -> Result<Self, String> {
        if p.kind != "ssh" {
            return Err(format!("只有 SSH Profile 支持 SFTP（当前 kind={}）", p.kind));
        }
        if p.host.trim().is_empty() {
            return Err("SFTP 需要 host".into());
        }
        if p.user.trim().is_empty() {
            return Err("SFTP 需要 user".into());
        }
        Ok(Self {
            host: p.host.trim().to_string(),
            port: p.port.max(1),
            user: p.user.trim().to_string(),
            key_path: p.key_path.trim().to_string(),
            extra_args: p.extra_args.clone(),
        })
    }

    /// sftp 参数：`[-i key] -P port <extra> user@host`。
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.key_path.is_empty() {
            args.push("-i".to_string());
            args.push(self.key_path.clone());
        }
        args.push("-P".to_string());
        args.push(self.port.to_string());
        args.extend(self.extra_args.iter().filter(|a| !a.trim().is_empty()).cloned());
        args.push(format!("{}@{}", self.user, self.host));
        args
    }

    /// 执行一段 batch 脚本，返回 stdout；退出码非 0 时返回 stderr 内容。
    pub fn run(&self, batch: &str) -> Result<String, String> {
        // 选项必须在 destination 之前，否则 sftp 会把它们当成多余参数
        let mut child = Command::new("sftp")
            .args(["-q", "-b", "-"])
            .args(self.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("启动 sftp 失败: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(batch.as_bytes()).map_err(|e| format!("写入 batch 失败: {e}"))?;
        }
        let out = child.wait_with_output().map_err(|e| format!("等待 sftp 失败: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success() {
            let msg = if stderr.trim().is_empty() { stdout.trim().to_string() } else { stderr.trim().to_string() };
            return Err(format!("sftp 失败（{}）: {msg}", out.status));
        }
        Ok(stdout)
    }
}

/// 过滤 sftp 的提示行（`sftp> ...`）与空行，留下远端目录条目。
pub fn parse_list(out: &str) -> Vec<String> {
    out.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("sftp>") && !l.starts_with("Connected to"))
        .map(|l| l.to_string())
        .collect()
}

pub fn list(t: &SftpTarget, remote: &str) -> Result<Vec<String>, String> {
    let out = t.run(&format!("ls -1 {remote}\n"))?;
    Ok(parse_list(&out))
}

pub fn download(t: &SftpTarget, remote: &str, local: &str) -> Result<(), String> {
    t.run(&format!("get {remote} {local}\n")).map(|_| ())
}

pub fn upload(t: &SftpTarget, local: &str, remote: &str) -> Result<(), String> {
    t.run(&format!("put {local} {remote}\n")).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SftpTarget {
        SftpTarget {
            host: "example.com".into(),
            port: 2222,
            user: "yt".into(),
            key_path: "/home/yt/.ssh/id_ed25519".into(),
            extra_args: vec!["-o".into(), "StrictHostKeyChecking=yes".into()],
        }
    }

    #[test]
    fn args_follow_sftp_convention() {
        assert_eq!(
            target().args(),
            vec![
                "-i",
                "/home/yt/.ssh/id_ed25519",
                "-P",
                "2222",
                "-o",
                "StrictHostKeyChecking=yes",
                "yt@example.com"
            ]
        );
    }

    #[test]
    fn target_requires_ssh_profile_fields() {
        let mut p = Profile { kind: "local".into(), ..Profile::default() };
        assert!(SftpTarget::from_profile(&p).unwrap_err().contains("SSH"));
        p.kind = "ssh".into();
        p.host = "h".into();
        p.user = String::new();
        assert!(SftpTarget::from_profile(&p).unwrap_err().contains("user"));
        p.user = "u".into();
        let t = SftpTarget::from_profile(&p).unwrap();
        assert_eq!(t.port, 22);
        assert!(t.key_path.is_empty());
        assert_eq!(t.args(), vec!["-P", "22", "u@h"]);
    }

    #[test]
    fn list_parsing_drops_prompts() {
        let out = "sftp> ls -1 /tmp\nfile1\nfile2\n\nConnected to host.\n";
        assert_eq!(parse_list(out), vec!["file1", "file2"]);
    }
}
