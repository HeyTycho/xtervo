// SPDX-License-Identifier: GPL-3.0-only
//! SFTP 命令层：调用系统 `sftp`（batch 模式）完成列目录 / 上传 / 下载。
//!
//! 不做交互式 sftp：每次操作拼一段 batch 脚本，走 `sftp -q -b -`，
//! 靠退出码判断成败、靠 stderr 取错误信息。
//!
//! 传输（上传/下载）走后台子进程 + 事件通道：UI 线程只收事件，不阻塞。
//! 若 sftp 输出了 `path 42% 1.2MB 3.4MB/s` 形式的进度行，会解析成字节级进度；
//! 但 OpenSSH 的 `progress` 表在 stdout 不是 tty 时（本项目的管道场景）**不会输出**，
//! 此时 UI 退化为「当前文件 + 不确定进度」，不伪造百分比。

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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

pub fn mkdir(t: &SftpTarget, remote: &str) -> Result<(), String> {
    t.run(&format!("mkdir {remote}\n")).map(|_| ())
}

pub fn rename(t: &SftpTarget, from: &str, to: &str) -> Result<(), String> {
    t.run(&format!("rename {from} {to}\n")).map(|_| ())
}

/// 删除文件；如果是目录，`rm` 失败后回退 `rmdir`（非递归，避免误删整棵树）。
pub fn remove(t: &SftpTarget, remote: &str) -> Result<(), String> {
    match t.run(&format!("rm {remote}\n")) {
        Ok(_) => Ok(()),
        Err(first) => t.run(&format!("rmdir {remote}\n")).map(|_| ()).map_err(|_| first),
    }
}

// ------------------------------------------------------------ 传输进度解析

/// 一行 sftp 进度（OpenSSH 在 tty 下的真实格式，例：
/// `big.bin 42% 1.2MB 3.4MB/s 00:05 ETA`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressLine {
    pub path: String,
    pub percent: u8,
    pub bytes: u64,
    pub rate: String,
    pub eta: Option<String>,
}

/// 解析单行进度；不是进度行（提示符/错误/普通输出）返回 `None`。
///
/// 文件名可能包含空格，所以从右往左找 `NN%` 标记，标记左侧整体作为路径；
/// 百分比必须 0..=100，且其后必须能解析出已传字节数。
pub fn parse_progress_line(line: &str) -> Option<ProgressLine> {
    let line = line.trim();
    if line.is_empty() || line.starts_with("sftp>") || line.starts_with("Connected to") {
        return None;
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let index = tokens.iter().rposition(|token| {
        token.strip_suffix('%').and_then(|v| v.parse::<u8>().ok()).is_some_and(|v| v <= 100)
    })?;
    if index == 0 {
        return None;
    }
    let percent = tokens[index].strip_suffix('%')?.parse::<u8>().ok()?;
    let bytes = parse_size(tokens.get(index + 1)?)?;
    let rate = tokens.get(index + 2).copied().unwrap_or_default().to_string();
    let eta = {
        let rest = &tokens[index + 3..];
        if rest.is_empty() {
            None
        } else {
            Some(rest.join(" "))
        }
    };
    let path = tokens[..index].join(" ");
    if path.is_empty() {
        return None;
    }
    Some(ProgressLine { path, percent, bytes, rate, eta })
}

/// 解析一段可能含 `\r` / `\n` 的进度输出（多文件场景返回多条）。
pub fn parse_progress_output(output: &str) -> Vec<ProgressLine> {
    output.split(|c| c == '\r' || c == '\n').filter_map(parse_progress_line).collect()
}

/// `1024KB` / `1.2MB` / `0` → 字节数；未知单位返回 `None`。
fn parse_size(token: &str) -> Option<u64> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let split = token.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(token.len());
    let (number, unit) = token.split_at(split);
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let scale = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * scale).round() as u64)
}

/// 从字节缓冲中取出所有完整行（`\r` 或 `\n` 分隔），不完整的尾巴留在缓冲里。
fn take_complete_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        let Some(position) = buffer.iter().position(|byte| *byte == b'\r' || *byte == b'\n') else {
            break;
        };
        let mut line: Vec<u8> = buffer.drain(..=position).collect();
        line.pop(); // 去掉分隔符
        let text = String::from_utf8_lossy(&line).trim().to_string();
        if !text.is_empty() {
            lines.push(text);
        }
    }
    lines
}

// ------------------------------------------------------------ 后台传输任务

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

impl TransferDirection {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Upload => "上传",
            Self::Download => "下载",
        }
    }
}

/// 后台任务发往 UI 的事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferEvent {
    /// sftp 输出了可解析的进度行。
    Progress(ProgressLine),
    /// 任务结束；`cancelled` 优先于 `error`。
    Finished { cancelled: bool, error: Option<String> },
}

/// 一次传输请求。下载始终用 `get -R`（对普通文件无副作用，对目录才能递归）；
/// 上传只有本地是目录时才加 `put -R`。
#[derive(Clone, Debug)]
pub struct TransferRequest {
    pub direction: TransferDirection,
    pub target: SftpTarget,
    pub local: String,
    pub remote: String,
    pub recursive: bool,
}

impl TransferRequest {
    pub fn batch_script(&self) -> String {
        let recursive = self.direction == TransferDirection::Download || self.recursive;
        let flag = if recursive { "-R " } else { "" };
        match self.direction {
            // get <remote> <local> / put <local> <remote>
            TransferDirection::Upload => {
                format!("progress\nput {flag}{} {}\n", self.local, self.remote)
            },
            TransferDirection::Download => {
                format!("progress\nget {flag}{} {}\n", self.remote, self.local)
            },
        }
    }
}

/// 正在运行的传输：子进程 + 进度事件接收端 + 取消标志。
///
/// `events` 由 UI 线程非阻塞 `try_recv`；`cancel()` 只发信号/kill，不等待。
/// 线程在最终事件发出后退出，UI 侧用 [`TransferJob::join`] 回收。
pub struct TransferJob {
    pub events: Receiver<TransferEvent>,
    cancel: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
    join: Option<JoinHandle<()>>,
}

impl TransferJob {
    /// 启动真实的 sftp 传输。
    pub fn start(request: &TransferRequest) -> Result<Self, String> {
        let mut command = Command::new("sftp");
        command.args(["-q", "-b", "-"]).args(request.target.args());
        Self::spawn_with_script(command, request.batch_script())
    }

    /// 启动任意子进程并喂一段脚本；抽出来便于用临时 shell 脚本测取消/状态机。
    pub fn spawn_with_script(mut command: Command, script: String) -> Result<Self, String> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // 临时脚本刚写完时可能 ETXTBSY（测试/某些文件系统）；重试几次。
        let mut child = None;
        for attempt in 0..5 {
            match command.spawn() {
                Ok(spawned) => {
                    child = Some(spawned);
                    break;
                },
                Err(e) if e.raw_os_error() == Some(26) && attempt < 4 => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                },
                Err(e) => return Err(format!("启动子进程失败: {e}")),
            }
        }
        let mut child = child.ok_or_else(|| "启动子进程失败: 重试后仍失败".to_string())?;
        let stdin = child.stdin.take().ok_or_else(|| "无法接管子进程 stdin".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "无法读取子进程 stdout".to_string())?;
        let stderr = child.stderr.take().ok_or_else(|| "无法读取子进程 stderr".to_string())?;

        let child = Arc::new(Mutex::new(child));
        let cancel = Arc::new(AtomicBool::new(false));
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let (sender, events) = mpsc::channel();

        let stdout_sender = sender.clone();
        let stdout_reader = thread::spawn(move || read_stream(stdout, stdout_sender, None));
        let stderr_sender = sender.clone();
        let stderr_sink = stderr_text.clone();
        let stderr_reader =
            thread::spawn(move || read_stream(stderr, stderr_sender, Some(stderr_sink)));

        let worker_child = child.clone();
        let worker_cancel = cancel.clone();
        let worker_stderr = stderr_text;
        let join = thread::spawn(move || {
            let mut stdin = stdin;
            let write_error = stdin.write_all(script.as_bytes()).err().map(|e| format!("写入 batch 失败: {e}"));
            drop(stdin);
            if write_error.is_some() {
                terminate(&worker_child);
            }
            let status = wait_child(&worker_child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            let cancelled = worker_cancel.load(Ordering::SeqCst);
            let stderr = worker_stderr.lock().map(|text| text.trim().to_string()).unwrap_or_default();
            let event = if cancelled {
                TransferEvent::Finished { cancelled: true, error: None }
            } else {
                match status {
                    Ok(status) if status.success() => {
                        // 子进程已成功：即使写 stdin 时报了 EPIPE（sftp 提前退出），
                        // 也以实际退出码为准，避免把成功报成失败。
                        TransferEvent::Finished { cancelled: false, error: None }
                    },
                    Ok(status) => {
                        // 连接/认证失败时 sftp 可能很快退出并留下 stderr；
                        // 优先展示真实的 sftp 错误，其次才是写 stdin 失败。
                        let message = if !stderr.is_empty() {
                            format!("sftp 失败（{status}）: {stderr}")
                        } else if let Some(error) = write_error {
                            error
                        } else {
                            format!("sftp 失败（{status}）")
                        };
                        TransferEvent::Finished { cancelled: false, error: Some(message) }
                    },
                    Err(e) => TransferEvent::Finished {
                        cancelled: false,
                        error: Some(format!("等待 sftp 失败: {e}")),
                    },
                }
            };
            let _ = sender.send(event);
        });

        Ok(Self { events, cancel, child, join: Some(join) })
    }

    /// UI 线程调用：记录取消并（Unix）kill 子进程；不等待、不阻塞。
    pub fn cancel(&self) {
        if self.cancel.swap(true, Ordering::SeqCst) {
            return;
        }
        #[cfg(unix)]
        {
            if terminate(&self.child) {
                eprintln!("[sftp] 已 kill 传输子进程");
            } else {
                eprintln!("[sftp] 取消已记录（子进程可能已退出）");
            }
        }
        #[cfg(not(unix))]
        eprintln!("[sftp] 取消已记录（当前平台不 kill 子进程）");
    }

    /// 回收任务线程；最终事件发出后应很快返回。
    pub fn join(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// 同步等待任务结束；只给 `--sftp-probe` 这类无 UI 的内部探针用。
    pub fn wait(self) -> Result<(), String> {
        let mut outcome = None;
        loop {
            match self.events.recv() {
                Ok(TransferEvent::Progress(progress)) => {
                    eprintln!(
                        "[sftp] 进度 {}% {} {}",
                        progress.percent, progress.rate, progress.path
                    );
                },
                Ok(TransferEvent::Finished { cancelled, error }) => {
                    outcome = Some((cancelled, error));
                    break;
                },
                Err(_) => break,
            }
        }
        self.join();
        match outcome {
            Some((true, _)) => Err("传输已取消".into()),
            Some((false, Some(error))) => Err(error),
            Some((false, None)) => Ok(()),
            None => Err("传输进程意外结束".into()),
        }
    }
}

impl Drop for TransferJob {
    fn drop(&mut self) {
        // 防止 UI 忘记回收时留下孤儿 sftp：先终止，再 join 线程。
        self.cancel.store(true, Ordering::SeqCst);
        terminate(&self.child);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn terminate(child: &Arc<Mutex<Child>>) -> bool {
    match child.lock() {
        Ok(mut child) => child.kill().is_ok(),
        Err(_) => false,
    }
}

fn wait_child(child: &Arc<Mutex<Child>>) -> std::io::Result<std::process::ExitStatus> {
    // 不在锁内 wait：cancel() 需要能随时拿到子进程句柄发 kill。
    loop {
        let status = {
            let mut child = match child.lock() {
                Ok(child) => child,
                Err(poisoned) => poisoned.into_inner(),
            };
            child.try_wait()
        };
        match status {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {},
            Err(e) => return Err(e),
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn read_stream<R: Read>(
    mut reader: R,
    sender: Sender<TransferEvent>,
    stderr_sink: Option<Arc<Mutex<String>>>,
) {
    let mut pending = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                for line in take_complete_lines(&mut pending) {
                    dispatch_line(&line, &sender, stderr_sink.as_ref());
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        let line = String::from_utf8_lossy(&pending).trim().to_string();
        if !line.is_empty() {
            dispatch_line(&line, &sender, stderr_sink.as_ref());
        }
    }
}

fn dispatch_line(line: &str, sender: &Sender<TransferEvent>, stderr_sink: Option<&Arc<Mutex<String>>>) {
    if let Some(progress) = parse_progress_line(line) {
        let _ = sender.send(TransferEvent::Progress(progress));
        return;
    }
    if let Some(sink) = stderr_sink {
        if let Ok(mut text) = sink.lock() {
            // 只保留错误文本的开头，避免异常输出撑爆内存。
            if text.len() < 8192 {
                text.push_str(line.trim());
                text.push('\n');
            }
        }
    }
}

// ------------------------------------------------------------ 传输状态机

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferPhase {
    Idle,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// 传给 UI 的传输状态：纯数据 + 纯函数，便于单测（不启动 GUI）。
#[derive(Clone, Debug)]
pub struct TransferState {
    pub direction: TransferDirection,
    pub phase: TransferPhase,
    pub local: String,
    pub remote: String,
    pub current: String,
    pub percent: Option<u8>,
    pub bytes: u64,
    pub rate: String,
    pub message: String,
}

impl Default for TransferState {
    fn default() -> Self {
        Self::idle()
    }
}

impl TransferState {
    pub fn idle() -> Self {
        Self {
            direction: TransferDirection::Download,
            phase: TransferPhase::Idle,
            local: String::new(),
            remote: String::new(),
            current: String::new(),
            percent: None,
            bytes: 0,
            rate: String::new(),
            message: String::new(),
        }
    }

    pub fn running(
        direction: TransferDirection,
        local: impl Into<String>,
        remote: impl Into<String>,
    ) -> Self {
        let local = local.into();
        let remote = remote.into();
        let source = match direction {
            TransferDirection::Upload => &local,
            TransferDirection::Download => &remote,
        };
        Self {
            direction,
            phase: TransferPhase::Running,
            current: display_name(source),
            local,
            remote,
            percent: None,
            bytes: 0,
            rate: String::new(),
            message: String::new(),
        }
    }

    /// 应用事件；进入终态后的迟到事件会被忽略。
    pub fn apply(&mut self, event: TransferEvent) {
        if !self.is_active() {
            return;
        }
        match event {
            TransferEvent::Progress(progress) => {
                let name = display_name(&progress.path);
                if !name.is_empty() {
                    self.current = name;
                }
                self.percent = Some(progress.percent);
                self.bytes = progress.bytes;
                self.rate = progress.rate;
            },
            TransferEvent::Finished { cancelled, error } => {
                if cancelled {
                    self.phase = TransferPhase::Cancelled;
                } else if let Some(error) = error {
                    self.phase = TransferPhase::Failed;
                    self.message = error;
                } else {
                    self.phase = TransferPhase::Done;
                }
            },
        }
    }

    pub fn is_active(&self) -> bool {
        self.phase == TransferPhase::Running
    }

    pub fn is_done_upload(&self) -> bool {
        self.phase == TransferPhase::Done && self.direction == TransferDirection::Upload
    }

    /// Slint 进度条用：`-1` = 不确定进度。
    pub fn percent_i32(&self) -> i32 {
        self.percent.map(i32::from).unwrap_or(-1)
    }

    pub fn status_text(&self) -> String {
        match self.phase {
            TransferPhase::Idle => String::new(),
            TransferPhase::Running => format!("{}中: {}", self.direction.verb(), self.current),
            TransferPhase::Done => match self.direction {
                TransferDirection::Upload => format!("已上传 {} → {}", self.local, self.remote),
                TransferDirection::Download => format!("已下载 {} → {}", self.remote, self.local),
            },
            TransferPhase::Cancelled => {
                format!("已取消{}: {}", self.direction.verb(), self.current)
            },
            TransferPhase::Failed => {
                format!("{}失败: {}", self.direction.verb(), self.message)
            },
        }
    }

    pub fn detail_text(&self) -> String {
        if self.phase != TransferPhase::Running {
            return String::new();
        }
        let Some(percent) = self.percent else {
            return "不确定进度".to_string();
        };
        let mut parts = vec![format!("{percent}%"), format_bytes(self.bytes)];
        if !self.rate.is_empty() {
            parts.push(self.rate.clone());
        }
        parts.join(" · ")
    }
}

fn display_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.is_empty() {
        trimmed.to_string()
    } else {
        name.to_string()
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    fn target() -> SftpTarget {
        SftpTarget {
            host: "example.com".into(),
            port: 2222,
            user: "yt".into(),
            key_path: "/home/user/.ssh/id_ed25519".into(),
            extra_args: vec!["-o".into(), "StrictHostKeyChecking=yes".into()],
        }
    }

    #[test]
    fn args_follow_sftp_convention() {
        assert_eq!(
            target().args(),
            vec![
                "-i",
                "/home/user/.ssh/id_ed25519",
                "-P",
                "2222",
                "-o",
                "StrictHostKeyChecking=yes",
                "user@example.com"
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

    // ------------------------------------------------------------ 进度解析

    #[test]
    fn progress_parser_handles_openssh_format() {
        let progress = parse_progress_line("big.bin 42% 1024KB 7.5MB/s 00:05 ETA").unwrap();
        assert_eq!(progress.path, "big.bin");
        assert_eq!(progress.percent, 42);
        assert_eq!(progress.bytes, 1024 * 1024);
        assert_eq!(progress.rate, "7.5MB/s");
        assert_eq!(progress.eta.as_deref(), Some("00:05 ETA"));
    }

    #[test]
    fn progress_parser_handles_zero_and_completion() {
        let zero = parse_progress_line("big.bin 0% 0 0.0KB/s --:-- ETA").unwrap();
        assert_eq!(zero.percent, 0);
        assert_eq!(zero.bytes, 0);
        let done = parse_progress_line("big.bin 100% 1024KB 7.5MB/s 00:00").unwrap();
        assert_eq!(done.percent, 100);
        assert_eq!(done.eta.as_deref(), Some("00:00"));
    }

    #[test]
    fn progress_parser_keeps_spaces_in_path() {
        let progress = parse_progress_line("dir/my file.bin 42% 1.2MB 3.4MB/s").unwrap();
        assert_eq!(progress.path, "dir/my file.bin");
        assert_eq!(progress.percent, 42);
        assert_eq!(progress.bytes, 1_258_291); // 1.2 MiB 四舍五入
    }

    #[test]
    fn progress_parser_rejects_non_progress_lines() {
        for line in [
            "",
            "sftp> get file1 file2",
            "Connected to host.",
            "big.bin 101% 1MB 1MB/s",
            "big.bin 42%",
            "big.bin 42% nope 1MB/s",
        ] {
            assert!(parse_progress_line(line).is_none(), "should reject: {line:?}");
        }
    }

    #[test]
    fn progress_parser_handles_multiple_files() {
        let output = "a.bin 0% 0 0.0KB/s --:-- ETA\rb.bin 100% 1024KB 7.5MB/s 00:00\r\n";
        let lines = parse_progress_output(output);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].path, "a.bin");
        assert_eq!(lines[1].path, "b.bin");
        assert_eq!(lines[1].percent, 100);
    }

    #[test]
    fn stream_lines_split_on_cr_or_lf() {
        let mut buffer = b"one\rtwo\r\nthree\npart".to_vec();
        assert_eq!(take_complete_lines(&mut buffer), vec!["one", "two", "three"]);
        assert_eq!(buffer, b"part");
    }

    // ------------------------------------------------------------ 状态机

    #[test]
    fn state_machine_tracks_progress_and_completion() {
        let mut state =
            TransferState::running(TransferDirection::Download, "/tmp/out", "/remote/out");
        assert!(state.is_active());
        assert_eq!(state.detail_text(), "不确定进度");

        state.apply(TransferEvent::Progress(ProgressLine {
            path: "/remote/out".into(),
            percent: 42,
            bytes: 1024 * 1024,
            rate: "3.4MB/s".into(),
            eta: None,
        }));
        assert_eq!(state.current, "out");
        assert_eq!(state.percent, Some(42));
        assert!(state.detail_text().contains("42%"));
        assert!(state.detail_text().contains("1.0 MB"));

        state.apply(TransferEvent::Finished { cancelled: false, error: None });
        assert!(!state.is_active());
        assert_eq!(state.phase, TransferPhase::Done);
        assert!(state.status_text().contains("已下载"));

        // 终态后的迟到事件不改变状态。
        state.apply(TransferEvent::Progress(ProgressLine {
            path: "late.bin".into(),
            percent: 99,
            bytes: 1,
            rate: String::new(),
            eta: None,
        }));
        assert_eq!(state.percent, Some(42));
    }

    #[test]
    fn state_machine_handles_cancel_and_failure() {
        let mut cancelled =
            TransferState::running(TransferDirection::Upload, "/tmp/a", "/remote/a");
        cancelled.apply(TransferEvent::Finished { cancelled: true, error: None });
        assert_eq!(cancelled.phase, TransferPhase::Cancelled);
        assert!(cancelled.status_text().contains("已取消"));

        let mut failed = TransferState::running(TransferDirection::Download, "/tmp/b", "/remote/b");
        failed.apply(TransferEvent::Finished {
            cancelled: false,
            error: Some("permission denied".into()),
        });
        assert_eq!(failed.phase, TransferPhase::Failed);
        assert!(failed.status_text().contains("permission denied"));
        assert_eq!(failed.detail_text(), "");
    }

    #[test]
    fn upload_done_is_detected_for_list_refresh() {
        let mut state = TransferState::running(TransferDirection::Upload, "/tmp/a", "/remote/a");
        state.apply(TransferEvent::Finished { cancelled: false, error: None });
        assert!(state.is_done_upload());

        let mut download =
            TransferState::running(TransferDirection::Download, "/tmp/b", "/remote/b");
        download.apply(TransferEvent::Finished { cancelled: false, error: None });
        assert!(!download.is_done_upload());
    }

    // ------------------------------------------------------------ 任务/取消

    #[test]
    fn batch_script_uses_progress_and_recursion() {
        let download = TransferRequest {
            direction: TransferDirection::Download,
            target: target(),
            local: "/tmp/local.bin".into(),
            remote: "/remote/local.bin".into(),
            recursive: false,
        };
        assert_eq!(download.batch_script(), "progress\nget -R /remote/local.bin /tmp/local.bin\n");

        let upload_file = TransferRequest {
            direction: TransferDirection::Upload,
            target: target(),
            local: "/tmp/local.bin".into(),
            remote: "/remote/local.bin".into(),
            recursive: false,
        };
        assert_eq!(upload_file.batch_script(), "progress\nput /tmp/local.bin /remote/local.bin\n");

        let upload_dir = TransferRequest { recursive: true, ..upload_file };
        assert_eq!(
            upload_dir.batch_script(),
            "progress\nput -R /tmp/local.bin /remote/local.bin\n"
        );
    }

    #[cfg(unix)]
    fn temp_dir(tag: &str) -> PathBuf {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("tervo-sftp-{}-{tag}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-sftp.sh");
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn transfer_job_cancel_kills_slow_child() {
        let dir = temp_dir("cancel");
        // 用临时 shell 脚本模拟一个长时间传输；输出一行进度后 exec sleep，确保 kill 命中进程本身。
        let script = write_script(
            &dir,
            "#!/bin/sh\nprintf 'slow.bin 10%% 128KB 1.0MB/s --:-- ETA\\r'\nexec sleep 30\n",
        );
        let job = TransferJob::spawn_with_script(Command::new(&script), String::new()).unwrap();
        let first = job.events.recv_timeout(Duration::from_secs(5)).expect("应收到进度事件");
        match first {
            TransferEvent::Progress(progress) => assert_eq!(progress.percent, 10),
            other => panic!("期望进度事件，得到 {other:?}"),
        }

        let started = Instant::now();
        job.cancel();
        let mut finished = None;
        while started.elapsed() < Duration::from_secs(5) {
            match job.events.recv_timeout(Duration::from_millis(200)) {
                Ok(TransferEvent::Finished { cancelled, error }) => {
                    finished = Some((cancelled, error));
                    break;
                },
                Ok(TransferEvent::Progress(_)) => {},
                Err(_) => break,
            }
        }
        let (cancelled, error) = finished.expect("取消后应收到 Finished 事件");
        assert!(cancelled);
        assert!(error.is_none());
        assert!(started.elapsed() < Duration::from_secs(5), "取消后应及时回收");
        job.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn transfer_job_completes_and_reports_success() {
        let dir = temp_dir("done");
        let script = write_script(
            &dir,
            "#!/bin/sh\nprintf 'done.bin 50%% 512KB 2.0MB/s 00:01 ETA\\rdone.bin 100%% 1024KB 2.0MB/s 00:00\\r'\n",
        );
        let job = TransferJob::spawn_with_script(Command::new(&script), String::new()).unwrap();
        let mut state =
            TransferState::running(TransferDirection::Upload, "/tmp/done.bin", "/remote/done.bin");
        let mut done = false;
        while !done {
            match job.events.recv_timeout(Duration::from_secs(5)).expect("任务应结束") {
                TransferEvent::Progress(progress) => state.apply(TransferEvent::Progress(progress)),
                TransferEvent::Finished { cancelled, error } => {
                    state.apply(TransferEvent::Finished { cancelled, error });
                    done = true;
                },
            }
        }
        assert_eq!(state.phase, TransferPhase::Done);
        assert_eq!(state.percent, Some(100));
        assert!(state.status_text().contains("已上传"));
        job.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn transfer_job_captures_stderr_failure() {
        let dir = temp_dir("fail");
        let script =
            write_script(&dir, "#!/bin/sh\nprintf 'permission denied\\n' >&2\nexit 3\n");
        let job = TransferJob::spawn_with_script(Command::new(&script), String::new()).unwrap();
        let mut state =
            TransferState::running(TransferDirection::Download, "/tmp/x", "/remote/x");
        let event = job.events.recv_timeout(Duration::from_secs(5)).expect("任务应结束");
        match event {
            TransferEvent::Finished { cancelled, error } => {
                assert!(!cancelled);
                let error = error.expect("非零退出码应带错误信息");
                assert!(error.contains("permission denied"), "error={error}");
                state.apply(TransferEvent::Finished { cancelled, error: Some(error) });
            },
            other => panic!("期望 Finished 事件，得到 {other:?}"),
        }
        assert_eq!(state.phase, TransferPhase::Failed);
        job.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
