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

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::profile::Profile;
use xtervo_core::WakeSignal;

/// 单次 SFTP 元数据命令（list/mkdir/rename/rm）的硬超时。
///
/// 10s 覆盖正常连上后的目录列举/建目录/改名/删除；连接卡死或远端不响应时
/// 到点 kill 整个子进程组并返回可解释错误。UI 侧不再同步调用，见 [`SftpOpJob`]。
pub const SFTP_TIMEOUT: Duration = Duration::from_secs(10);
/// 递归删除的总预算。
///
/// OpenSSH sftp 的 `rm`/`rmdir` 都不支持递归（见 `man sftp`），安全做法是
/// 「列目录 → 删文件/子目录 → rmdir」逐层执行。每层至少一次 sftp 连接，
/// 因此总预算比普通元数据操作大；仍可随时 Cancel（每次调用都会 kill）。
pub const SFTP_RECURSIVE_TIMEOUT: Duration = Duration::from_secs(120);
/// 递归删除最大目录深度：防御异常列表/符号链接环造成无限递归。
const RECURSIVE_DELETE_MAX_DEPTH: usize = 64;
/// 超时 kill 后等待 reader 线程收尾的宽限；主流程最多再多等这么久。
const KILL_GRACE: Duration = Duration::from_millis(200);
/// 轮询子进程退出的间隔（try_wait，不做阻塞 wait）。
const WAIT_POLL: Duration = Duration::from_millis(10);

/// 连接目标（从 SSH Profile 提取；SFTP 复用同一套认证参数）。
#[derive(Clone, Debug, PartialEq)]
pub struct SftpTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: String,
    /// ProxyJump（跳板机）：SFTP 与 SSH Profile 共用同一条跳板配置。
    pub proxy_jump: String,
    pub extra_args: Vec<String>,
    pub ciphers: String,
    pub macs: String,
    pub kex_algorithms: String,
    pub proxy_command: String,
}

impl SftpTarget {
    pub fn from_profile(p: &Profile) -> Result<Self, String> {
        if p.kind != "ssh" {
            return Err(format!(
                "只有 SSH Profile 支持 SFTP（当前 kind={}）",
                p.kind
            ));
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
            proxy_jump: p.proxy_jump.trim().to_string(),
            extra_args: p.extra_args.clone(),
            ciphers: p.ciphers.trim().to_string(),
            macs: p.macs.trim().to_string(),
            kex_algorithms: p.kex_algorithms.trim().to_string(),
            proxy_command: p.proxy_command.trim().to_string(),
        })
    }

    /// sftp 参数：`[-i key] -P port <extra> <网络超时/keepalive> user@host`。
    ///
    /// 超时策略（10s 级别，所有网络操作有界/可取消）：
    /// - `ConnectTimeout=10`：TCP/SSH 握手最多 10s，连接卡死不会无限等；
    /// - `ServerAliveInterval=5` + `ServerAliveCountMax=2`：链路断掉后约 10s
    ///   发现并让 sftp 退出（传输中也能由 UI Cancel 立即 kill）；
    /// - 用户 extra_args 在前，OpenSSH「先出现者生效」，因此用户仍可覆盖默认值。
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.key_path.is_empty() {
            args.push("-i".to_string());
            args.push(self.key_path.clone());
        }
        args.push("-P".to_string());
        args.push(self.port.to_string());
        if !self.proxy_jump.is_empty() {
            args.push("-J".to_string());
            args.push(self.proxy_jump.clone());
        }
        for (name, value) in [
            ("Ciphers", &self.ciphers),
            ("MACs", &self.macs),
            ("KexAlgorithms", &self.kex_algorithms),
            ("ProxyCommand", &self.proxy_command),
        ] {
            if !value.is_empty() {
                args.push("-o".to_string());
                args.push(format!("{name}={value}"));
            }
        }
        args.extend(
            self.extra_args
                .iter()
                .filter(|a| !a.trim().is_empty())
                .cloned(),
        );
        for option in [
            "ConnectTimeout=10",
            "ServerAliveInterval=5",
            "ServerAliveCountMax=2",
        ] {
            args.push("-o".to_string());
            args.push(option.to_string());
        }
        args.push(format!("{}@{}", self.user, self.host));
        args
    }

    /// 执行一段 batch 脚本，返回 stdout；退出码非 0 时返回 stderr 内容。
    ///
    /// 硬超时 [`SFTP_TIMEOUT`]，到点 kill 子进程组；不再无限 `wait_with_output`。
    /// UI 不应直接调用它（会阻塞调用线程最多 10s），请用 [`SftpOpJob`]。
    pub fn run(&self, batch: &str) -> Result<String, String> {
        self.run_with_timeout(batch, SFTP_TIMEOUT)
    }

    /// 带显式超时的 batch 执行；测试也用它验证假 sftp 卡死。
    pub fn run_with_timeout(&self, batch: &str, timeout: Duration) -> Result<String, String> {
        run_command_with_timeout(self.command(), batch, timeout)
    }

    fn command(&self) -> Command {
        // 选项必须在 destination 之前，否则 sftp 会把它们当成多余参数
        let mut command = Command::new("sftp");
        command.args(["-q", "-b", "-"]).args(self.args());
        command
    }
}

/// 执行任意子进程并喂一段 batch 脚本，带硬超时 / 进程组 kill。
///
/// 抽成公开函数是为了让测试用假脚本（`sleep` 永不退出）验证超时路径；
/// 生产调用由 [`SftpTarget::run_with_timeout`] 或 [`SftpOpJob`] 发起。
pub fn run_command_with_timeout(
    command: Command,
    batch: &str,
    timeout: Duration,
) -> Result<String, String> {
    run_command_with_timeout_cancel(command, batch, timeout, None)
}

/// [`run_command_with_timeout`] + 外部取消标志：UI 点 Cancel 时 kill 子进程组，
/// 返回「已取消」。网络操作既受 10s 硬超时约束，也能被用户随时中断。
pub fn run_command_with_timeout_cancel(
    mut command: Command,
    batch: &str,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<String, String> {
    // 独立进程组：sftp 可能再起 ssh 子进程；超时要连它们一起 kill，
    // 否则孙进程仍持有 stdout/stderr 管道，reader 线程永远等不到 EOF。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 临时脚本刚写完时可能 ETXTBSY（测试/某些文件系统）；重试几次。
    let mut child = None;
    for attempt in 0..5 {
        match command.spawn() {
            Ok(spawned) => {
                child = Some(spawned);
                break;
            }
            Err(e) if e.raw_os_error() == Some(26) && attempt < 4 => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("启动 sftp 失败: {e}")),
        }
    }
    let mut child = child.ok_or_else(|| "启动 sftp 失败: 重试后仍失败".to_string())?;

    let write_error = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(batch.as_bytes())
            .err()
            .map(|e| format!("写入 batch 失败: {e}")),
        None => Some("无法接管 sftp stdin".to_string()),
    };

    // reader 线程 + 通道：kill 后即使有进程逃出进程组，也不会把主流程 join 死。
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.take(64 * 1024 * 1024).read_to_end(&mut buf);
            let _ = out_tx.send(buf);
        }));
    } else {
        let _ = out_tx.send(Vec::new());
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.take(64 * 1024 * 1024).read_to_end(&mut buf);
            let _ = err_tx.send(buf);
        }));
    } else {
        let _ = err_tx.send(Vec::new());
    }

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            cancelled = true;
            kill_child_tree(&mut child);
            break match reap_after_kill(&mut child) {
                Some(status) => Ok(status),
                None => Err(std::io::Error::other("取消 kill 后未在宽限内回收")),
            };
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            kill_child_tree(&mut child);
            // SIGKILL 后通常立即 reap；仍加 1s 上限，避免 D-state 把后台线程挂死
            // （即使挂死也不能再拖住 UI，因为 UI 只在 poll 里 try_recv）。
            break match reap_after_kill(&mut child) {
                Some(status) => Ok(status),
                None => Err(std::io::Error::other("超时 kill 后未在宽限内回收")),
            };
        }
        thread::sleep(remaining.min(WAIT_POLL));
    };

    // kill 后给 reader 一个有限宽限；正常退出则允许 2s 把剩余 stdout 读完。
    let grace = if timed_out || cancelled {
        KILL_GRACE
    } else {
        Duration::from_secs(2)
    };
    let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
    for reader in readers {
        if reader.is_finished() {
            let _ = reader.join();
        }
    }

    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).to_string();
    if cancelled {
        return Err("sftp 操作已取消".into());
    }
    if timed_out {
        return Err(format!(
            "sftp 超时（{}），已终止子进程",
            format_timeout(timeout)
        ));
    }
    match status {
        Ok(status) if status.success() => Ok(stdout),
        Ok(status) => {
            let msg = if stderr.trim().is_empty() {
                stdout.trim().to_string()
            } else {
                stderr.trim().to_string()
            };
            // 连接/认证失败时 sftp 可能很快退出并留下 stderr；优先展示真实错误。
            if msg.is_empty() {
                if let Some(error) = write_error {
                    return Err(error);
                }
            }
            Err(format!("sftp 失败（{status}）: {msg}"))
        }
        Err(e) => Err(format!("等待 sftp 失败: {e}")),
    }
}

/// 超时文案：≥1s 用秒（10s），测试里的短超时用毫秒。
///
/// 秒数向上取整：List 会把 10s 总预算减去已耗时后传给单次调用（剩余
/// 9.99s），floor 会把“10s 硬超时”写成“9s”，与 UI 显示的 timeout=10s
/// 自相矛盾。向上取整保证剩余预算仍归入它所属的整秒预算。
fn format_timeout(timeout: Duration) -> String {
    if timeout.as_secs() >= 1 {
        let secs = timeout.as_secs() + u64::from(timeout.subsec_nanos() > 0);
        format!("{secs}s")
    } else {
        format!("{}ms", timeout.as_millis())
    }
}

/// 超时 kill：Unix 杀整个进程组（含 sftp 拉起的 ssh），其它平台杀直接子进程。
fn kill_child_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // SAFETY: kill(2) 对负 pid 表示进程组；子进程由本函数以 process_group(0)
        // 启动，pgid == pid。即使 kill 返回 ESRCH 也是合法失败，忽略返回值。
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// kill 后最多等 1s 回收子进程；不无限 `wait()`（D-state 也不能拖死后台）。
fn reap_after_kill(child: &mut Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// 远端路径的父目录；根目录/空路径保持 "/"。
pub fn parent_remote(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(index) => trimmed[..index].to_string(),
    }
}

/// sftp batch 参数转义：双引号包裹，反斜杠/双引号转义；空路径用 "."。
/// 远端文件名可能包含空格/引号/`;`，绝不能直接拼进 batch。
pub fn quote_batch_path(path: &str) -> String {
    if path.is_empty() {
        return "\".\"".into();
    }
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 过滤 sftp 的提示行（`sftp> ...`）、连接横幅与空行，留下 `ls -1` 文件名。
pub fn parse_list(out: &str) -> Vec<String> {
    out.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("sftp>") && !l.starts_with("Connected to"))
        .map(|l| l.to_string())
        .collect()
}

/// 远端目录条目（`ls -l` 解析结果）。
///
/// `mode/size/mtime` 来自 OpenSSH sftp 的 `ls -l` 长格式；服务器不支持/输出
/// 为空时回退 `ls -1`，此时只有 `name`，`is_dir = None`（类型未知，删除走
/// 非递归的 rm/rmdir，绝不会因为“猜是目录”而展开整棵树）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    /// 文件名本身（符号链接已去掉 ` -> target`）。
    pub name: String,
    /// 权限/类型串，例如 `drwxr-xr-x` / `-rw-r--r--`。
    pub mode: String,
    pub size: Option<u64>,
    /// 服务端格式化的时间，例如 `Nov 20 10:00` / `Nov 20  2023`。
    pub mtime: String,
    /// `None` = 来自 `ls -1`，类型未知。
    pub is_dir: Option<bool>,
    pub link_target: Option<String>,
}

impl RemoteEntry {
    /// `ls -1` 回退条目：只有文件名，未知类型/大小/时间。
    pub fn from_name(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            mode: String::new(),
            size: None,
            mtime: String::new(),
            is_dir: None,
            link_target: None,
        }
    }

    /// 已确认是目录（`ls -l` 的 d 位）；符号链接不算，删除链接本身更安全。
    pub fn is_known_dir(&self) -> bool {
        self.is_dir == Some(true)
    }

    /// UI 行：`mode size mtime name`；`ls -1` 回退时只显示文件名。
    ///
    /// 不改变 `name` 字段：选中后的路径拼接始终使用真实文件名，
    /// 不会把展示用的 mode/size 拼进远端路径。
    pub fn display(&self) -> String {
        if self.mode.is_empty() {
            return self.name.clone();
        }
        let size = self
            .size
            .map(format_bytes)
            .unwrap_or_else(|| "-".to_string());
        let mtime = if self.mtime.is_empty() {
            "-"
        } else {
            self.mtime.as_str()
        };
        let mut text = format!("{} {:>8} {} {}", self.mode, size, mtime, self.name);
        if let Some(target) = &self.link_target {
            text.push_str(" -> ");
            text.push_str(target);
        }
        text
    }
}

/// 按空白切分并保留每个 field 在原始行里的字节起点（用于保留文件名内部空格）。
fn split_fields(line: &str) -> Vec<(usize, &str)> {
    let mut fields = Vec::new();
    let mut start = None;
    for (index, ch) in line.char_indices() {
        if ch.is_whitespace() {
            if let Some(begin) = start.take() {
                fields.push((begin, &line[begin..index]));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        fields.push((begin, &line[begin..]));
    }
    fields
}

/// OpenSSH `ls -l` 的第一个字段：文件类型 + 9 个权限位（可能多一个 ACL 标记）。
fn looks_like_mode(mode: &str) -> bool {
    let bytes = mode.as_bytes();
    bytes.len() >= 10
        && matches!(
            bytes[0],
            b'-' | b'd' | b'l' | b'b' | b'c' | b'p' | b's' | b'?'
        )
}

/// 是否是长格式行（即使因 `.`/`..` 被过滤，也不应当作纯文件名）。
fn looks_like_long_line(line: &str) -> bool {
    let fields = split_fields(line);
    fields.len() >= 9 && looks_like_mode(fields[0].1)
}

/// 解析单行 `ls -l`。不是长格式（提示符/普通文件名）返回 `None`。
fn parse_long_entry(line: &str) -> Option<RemoteEntry> {
    let fields = split_fields(line);
    // mode nlink user group size mon day time name
    if fields.len() < 9 || !looks_like_mode(fields[0].1) {
        return None;
    }
    let mode = fields[0].1;
    let size_token = fields[4].1.replace(',', "");
    let size = parse_size(&size_token);
    let mtime = format!("{} {} {}", fields[5].1, fields[6].1, fields[7].1);
    // 从原始字节位置取 name，保留文件名内部空格；只去掉行尾 `\r`。
    let name_raw = line[fields[8].0..].trim_end_matches('\r');
    if name_raw.trim().is_empty() {
        return None;
    }
    let mode_kind = mode.as_bytes()[0];
    let mut name = name_raw.to_string();
    let mut link_target = None;
    if mode_kind == b'l' {
        if let Some((link, target)) = name_raw.split_once(" -> ") {
            name = link.to_string();
            link_target = Some(target.to_string());
        }
    }
    // 远端列表里的 . / .. 没有操作价值，而且容易让 Up/递归路径重复。
    if name == "." || name == ".." {
        return None;
    }
    Some(RemoteEntry {
        name,
        mode: mode.to_string(),
        size,
        mtime,
        is_dir: Some(mode_kind == b'd'),
        link_target,
    })
}

/// 解析 `ls -l` 输出；无法识别的非提示行按纯文件名保留，避免静默丢条目。
pub fn parse_list_long(out: &str) -> Vec<RemoteEntry> {
    let mut entries = Vec::new();
    for raw in out.lines() {
        let line = raw.trim_start();
        if line.trim().is_empty() || line.starts_with("sftp>") || line.starts_with("Connected to") {
            continue;
        }
        if let Some(entry) = parse_long_entry(line) {
            if entry.name != "." && entry.name != ".." {
                entries.push(entry);
            }
        } else if !looks_like_long_line(line) {
            // 不是长格式：某些服务器/假脚本会直接返回纯文件名，保留它们。
            let name = line.trim_end();
            if !name.is_empty() && name != "." && name != ".." {
                entries.push(RemoteEntry::from_name(name));
            }
        }
    }
    entries
}

/// 解析 `ls -1` 输出为无名单元数据条目。
fn parse_short_entries(out: &str) -> Vec<RemoteEntry> {
    parse_list(out)
        .into_iter()
        .filter(|name| name != "." && name != "..")
        .map(RemoteEntry::from_name)
        .collect()
}

fn ls_long_batch(remote: &str) -> String {
    format!("ls -la {}\n", quote_batch_path(remote))
}

fn ls_short_batch(remote: &str) -> String {
    format!("ls -1a {}\n", quote_batch_path(remote))
}

/// `ls -l` 优先、失败/空输出回退 `ls -1`；`run` 由调用方注入超时/取消。
fn list_entries_with<F>(remote: &str, mut run: F) -> Result<Vec<RemoteEntry>, String>
where
    F: FnMut(&str) -> Result<String, String>,
{
    match run(&ls_long_batch(remote)) {
        Ok(out) => {
            let entries = parse_list_long(&out);
            if !entries.is_empty() {
                return Ok(entries);
            }
            // 空目录也会走到这里；多一次 `ls -1` 是可接受的，换取对旧服务器/
            // 假 sftp 脚本的兼容（它们可能不认识 -l，但会返回纯文件名）。
            run(&ls_short_batch(remote)).map(|out| parse_short_entries(&out))
        }
        Err(first) => {
            // 超时/取消已经 kill 子进程，再回退只会重复同一错误并浪费时间。
            if is_terminal_sftp_error(&first) {
                return Err(first);
            }
            match run(&ls_short_batch(remote)) {
                Ok(out) => Ok(parse_short_entries(&out)),
                Err(second) => Err(second),
            }
        }
    }
}

/// 超时/取消是终止性错误：list 不再回退，递归删除也不继续下一层。
fn is_terminal_sftp_error(message: &str) -> bool {
    message.contains("超时") || message.contains("已取消")
}

/// 规范化远端目录：去掉尾部 `/`；根目录固定为 `/`。
pub fn normalize_remote(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    }
}

/// 覆盖检查所需的信息：已知存在/已知不存在/列表未覆盖该目录。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverwriteRisk {
    None,
    Existing,
    Unknown,
}

impl OverwriteRisk {
    pub fn needs_confirmation(self) -> bool {
        self != Self::None
    }
}

/// 本地下载目标是否已存在（文件/目录/符号链接都算，取消时不得改动）。
pub fn local_target_exists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// 用当前远端列表判断上传目标是否已存在。
///
/// 只有列表目录与目标父目录一致时才能下“不存在”的结论；目录已切换/未加载
/// 一律返回 `Unknown`，由 UI 先刷新列表或弹“可能覆盖”确认。
pub fn overwrite_risk(
    listed_remote: Option<&str>,
    remote_dir: &str,
    entries: &[RemoteEntry],
    name: &str,
) -> OverwriteRisk {
    let Some(listed) = listed_remote else {
        return OverwriteRisk::Unknown;
    };
    if normalize_remote(listed) != normalize_remote(remote_dir) {
        return OverwriteRisk::Unknown;
    }
    if entries.iter().any(|entry| entry.name == name) {
        OverwriteRisk::Existing
    } else {
        OverwriteRisk::None
    }
}

/// 列表/上传/下载/改名/删除的 batch 入口（`ls -l` 优先，失败回退 `ls -1`）。
pub fn list(t: &SftpTarget, remote: &str) -> Result<Vec<RemoteEntry>, String> {
    list_entries_with(remote, |batch| t.run(batch))
}

pub fn mkdir(t: &SftpTarget, remote: &str) -> Result<(), String> {
    t.run(&format!("mkdir {}\n", quote_batch_path(remote)))
        .map(|_| ())
}

pub fn rename(t: &SftpTarget, from: &str, to: &str) -> Result<(), String> {
    t.run(&format!(
        "rename {} {}\n",
        quote_batch_path(from),
        quote_batch_path(to)
    ))
    .map(|_| ())
}

/// 删除文件；如果是目录，`rm` 失败后回退 `rmdir`（非递归，避免误删整棵树）。
pub fn remove(t: &SftpTarget, remote: &str) -> Result<(), String> {
    let quoted = quote_batch_path(remote);
    match t.run(&format!("rm {quoted}\n")) {
        Ok(_) => Ok(()),
        Err(first) => t
            .run(&format!("rmdir {quoted}\n"))
            .map(|_| ())
            .map_err(|_| first),
    }
}

// ------------------------------------------------------------ 元数据后台任务

/// 元数据命令（打开 SFTP 面板后的 list/mkdir/rename/delete）。
///
/// 这些操作以前在 Slint 回调里同步 `wait_with_output()`，远端卡住会把整个 UI
/// 冻死；现在统一进 [`SftpOpJob`] 后台线程 + 硬超时。
#[derive(Clone, Debug, PartialEq)]
pub enum SftpCommand {
    List {
        target: SftpTarget,
        remote: String,
    },
    Mkdir {
        target: SftpTarget,
        remote: String,
    },
    Rename {
        target: SftpTarget,
        from: String,
        to: String,
    },
    Remove {
        target: SftpTarget,
        remote: String,
    },
    /// 递归删除目录：逐层 list/rm/rmdir，总预算 [`SFTP_RECURSIVE_TIMEOUT`]。
    RemoveRecursive {
        target: SftpTarget,
        remote: String,
    },
    /// `chmod <mode> <remote>`；mode 已由 UI 解析成八进制权限位。
    Chmod {
        target: SftpTarget,
        remote: String,
        mode: u32,
    },
}

/// 后台元数据任务的结果：带原始路径，便于 UI 写回状态文案/刷新列表。
#[derive(Clone, Debug, PartialEq)]
pub enum SftpOutcome {
    List {
        remote: String,
        items: Result<Vec<RemoteEntry>, String>,
    },
    Mkdir {
        remote: String,
        result: Result<(), String>,
    },
    Rename {
        from: String,
        to: String,
        result: Result<(), String>,
    },
    Remove {
        path: String,
        result: Result<(), String>,
    },
    Chmod {
        path: String,
        mode: u32,
        result: Result<(), String>,
    },
}

impl SftpOutcome {
    /// 单行可解释摘要（日志/调试用）。
    pub fn describe(&self) -> String {
        match self {
            Self::List { remote, items } => match items {
                Ok(items) => format!("list {remote}: {} 条", items.len()),
                Err(e) => format!("list {remote} 失败: {e}"),
            },
            Self::Mkdir { remote, result } => format_result("mkdir", remote, result),
            Self::Rename { from, to, result } => {
                format!("rename {from} → {to}: {}", ok_or_err(result))
            }
            Self::Remove { path, result } => format_result("remove", path, result),
            Self::Chmod { path, mode, result } => {
                format!("chmod {mode:04o} {path}: {}", ok_or_err(result))
            }
        }
    }
}

fn ok_or_err<T>(result: &Result<T, String>) -> &str {
    match result {
        Ok(_) => "ok",
        Err(e) => e,
    }
}

fn format_result(verb: &str, path: &str, result: &Result<(), String>) -> String {
    format!("{verb} {path}: {}", ok_or_err(result))
}

impl SftpCommand {
    /// 该命令的硬超时。递归删除需要多次 sftp 调用，使用更大的总预算。
    pub fn timeout(&self) -> Duration {
        match self {
            Self::RemoveRecursive { .. } => SFTP_RECURSIVE_TIMEOUT,
            _ => SFTP_TIMEOUT,
        }
    }

    /// 是否递归删除（UI 进度区显示不同的超时文案）。
    pub fn is_recursive_delete(&self) -> bool {
        matches!(self, Self::RemoveRecursive { .. })
    }

    /// 可注入程序/超时的执行入口（测试用假 sftp 脚本，避免真连网）。
    pub fn execute_with(self, program: &Path, timeout: Duration) -> SftpOutcome {
        self.execute_with_cancel(program, timeout, None)
    }

    /// 带取消标志的执行入口：UI Cancel 时 kill 子进程组并返回「已取消」。
    pub fn execute_with_cancel(
        self,
        program: &Path,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> SftpOutcome {
        match self {
            Self::List { target, remote } => {
                let started = Instant::now();
                let mut run = |batch: &str| -> Result<String, String> {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(format!(
                            "sftp 超时（{}），已终止子进程",
                            format_timeout(timeout)
                        ));
                    }
                    run_program_cancel(program, &target, batch, remaining.min(SFTP_TIMEOUT), cancel)
                };
                let items = list_entries_with(&remote, &mut run);
                SftpOutcome::List { remote, items }
            }
            Self::Mkdir { target, remote } => {
                let result = run_program_cancel(
                    program,
                    &target,
                    &format!("mkdir {}\n", quote_batch_path(remote.as_str())),
                    timeout,
                    cancel,
                )
                .map(|_| ());
                SftpOutcome::Mkdir { remote, result }
            }
            Self::Rename { target, from, to } => {
                let result = run_program_cancel(
                    program,
                    &target,
                    &format!(
                        "rename {} {}\n",
                        quote_batch_path(from.as_str()),
                        quote_batch_path(to.as_str())
                    ),
                    timeout,
                    cancel,
                )
                .map(|_| ());
                SftpOutcome::Rename { from, to, result }
            }
            Self::Remove { target, remote } => {
                let started = Instant::now();
                let result = match run_program_cancel(
                    program,
                    &target,
                    &format!("rm {}\n", quote_batch_path(remote.as_str())),
                    timeout,
                    cancel,
                ) {
                    Ok(_) => Ok(()),
                    Err(first) => {
                        // rmdir 回退也受同一次操作的总超时约束：第一次已超时就直接
                        // 返回，避免 10s + 10s 双倍卡后台。
                        let remaining = timeout.saturating_sub(started.elapsed());
                        if remaining.is_zero() {
                            Err(first)
                        } else {
                            run_program_cancel(
                                program,
                                &target,
                                &format!("rmdir {}\n", quote_batch_path(remote.as_str())),
                                remaining.min(SFTP_TIMEOUT),
                                cancel,
                            )
                            .map(|_| ())
                            .map_err(|_| first)
                        }
                    }
                };
                SftpOutcome::Remove {
                    path: remote,
                    result,
                }
            }
            Self::RemoveRecursive { target, remote } => {
                let result = remove_tree(program, &target, &remote, timeout, cancel);
                SftpOutcome::Remove {
                    path: remote,
                    result,
                }
            }
            Self::Chmod {
                target,
                remote,
                mode,
            } => {
                let result = run_program_cancel(
                    program,
                    &target,
                    &format!("chmod {mode:04o} {}\n", quote_batch_path(remote.as_str())),
                    timeout,
                    cancel,
                )
                .map(|_| ());
                SftpOutcome::Chmod {
                    path: remote,
                    mode,
                    result,
                }
            }
        }
    }
}

fn run_program_cancel(
    program: &Path,
    target: &SftpTarget,
    batch: &str,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(["-q", "-b", "-"]).args(target.args());
    run_command_with_timeout_cancel(command, batch, timeout, cancel)
}

// ------------------------------------------------------------ 递归删除

/// 远端路径拼接（`name` 必须是单个目录项，不能含 `/`）。
fn join_remote_path(dir: &str, name: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        format!("/{name}")
    } else {
        format!("{trimmed}/{name}")
    }
}

/// 递归删除只接受单个目录项名：拒绝空、`.`/`..`、含 `/` 的恶意/异常列表，
/// 防止服务端返回 `/etc` 或 `../../x` 让删除逃逸用户选中的目录。
fn child_remote_path(dir: &str, name: &str) -> Result<String, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(format!(
            "拒绝可疑远端条目名 {name:?}（目录 {dir}），已中止删除"
        ));
    }
    Ok(join_remote_path(dir, name))
}

/// 递归删除的总预算/取消检查；每次子进程调用都拿剩余预算并为单次调用封顶。
struct DeleteBudget<'a> {
    deadline: Instant,
    total: Duration,
    cancel: Option<&'a AtomicBool>,
}

impl<'a> DeleteBudget<'a> {
    fn new(total: Duration, cancel: Option<&'a AtomicBool>) -> Self {
        Self {
            deadline: Instant::now() + total,
            total,
            cancel,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    fn remaining(&self) -> Result<Duration, String> {
        if self.cancelled() {
            return Err("sftp 操作已取消".into());
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "递归删除超时（{}），操作已中止（可能已部分删除）",
                format_timeout(self.total)
            ));
        }
        Ok(remaining)
    }

    fn run<F>(&self, run: &mut F, batch: &str) -> Result<String, String>
    where
        F: FnMut(&str, Duration) -> Result<String, String>,
    {
        let remaining = self.remaining()?;
        let per_call = remaining.min(SFTP_TIMEOUT);
        run(batch, per_call).map_err(|error| {
            if self.cancelled() {
                "sftp 操作已取消".into()
            } else {
                error
            }
        })
    }
}

/// 生产入口：注入真实 sftp 程序；测试用 [`remove_tree_with`] 注入内存 runner。
fn remove_tree(
    program: &Path,
    target: &SftpTarget,
    root: &str,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    remove_tree_with(root, timeout, cancel, |batch, call_timeout| {
        run_program_cancel(program, target, batch, call_timeout, cancel)
    })
}

/// 逐层删除：先 rmdir（空目录一次完成），失败后 list，删文件/子目录，再 rmdir。
///
/// 不用 `rm -r`：本机 OpenSSH sftp 的 `rm` 只接受单个文件（`man sftp`），
/// `rmdir` 也只删空目录。这里只调用这两个明确支持的命令，且每一步都经过
/// [`quote_batch_path`]；任何一步失败都中止并返回错误，不会把失败当成功。
fn remove_tree_with<F>(
    root: &str,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    mut run: F,
) -> Result<(), String>
where
    F: FnMut(&str, Duration) -> Result<String, String>,
{
    let budget = DeleteBudget::new(timeout, cancel);
    remove_tree_inner(&budget, &mut run, root, 0)
}

fn remove_tree_inner<F>(
    budget: &DeleteBudget<'_>,
    run: &mut F,
    path: &str,
    depth: usize,
) -> Result<(), String>
where
    F: FnMut(&str, Duration) -> Result<String, String>,
{
    if depth > RECURSIVE_DELETE_MAX_DEPTH {
        return Err(format!(
            "递归删除超过最大深度 {RECURSIVE_DELETE_MAX_DEPTH}: {path}"
        ));
    }
    // 空目录直接成功；非空目录会失败，继续列举。
    match budget.run(run, &format!("rmdir {}\n", quote_batch_path(path))) {
        Ok(_) => return Ok(()),
        Err(error) if is_terminal_sftp_error(&error) => return Err(error),
        Err(_) => {}
    }
    let entries = list_entries_with(path, |batch| budget.run(run, batch))?;
    remove_children(budget, run, path, entries, depth)?;
    budget
        .run(run, &format!("rmdir {}\n", quote_batch_path(path)))
        .map(|_| ())
}

fn remove_children<F>(
    budget: &DeleteBudget<'_>,
    run: &mut F,
    dir: &str,
    entries: Vec<RemoteEntry>,
    depth: usize,
) -> Result<(), String>
where
    F: FnMut(&str, Duration) -> Result<String, String>,
{
    // 先校验整层条目类型，全部可判定后才开始删除；不能在删掉前面的子树后
    // 才发现后面的条目类型未知。
    let mut checked = Vec::with_capacity(entries.len());
    for entry in entries {
        let child = child_remote_path(dir, &entry.name)?;
        if entry.is_dir.is_none() {
            return Err(format!(
                "无法确认 {} 是目录（仅名称列表），拒绝递归删除；请先获取 ls -l 元数据",
                child
            ));
        }
        checked.push((child, entry.is_dir == Some(true)));
    }
    let mut files = Vec::new();
    for (child, is_dir) in checked {
        if is_dir {
            remove_tree_inner(budget, run, &child, depth + 1)?;
        } else {
            files.push(child);
        }
    }
    if files.is_empty() {
        return Ok(());
    }
    // 同一目录的普通文件合成一个 batch（sftp 会顺序执行 rm）；失败即中止，
    // 下一次重试会重新列目录，不会把未删的当成已删。
    let mut batch = String::new();
    for file in &files {
        batch.push_str(&format!("rm {}\n", quote_batch_path(file)));
    }
    budget.run(run, &batch).map(|_| ())
}

/// 把 UI 里的权限输入解析成八进制权限位。
///
/// 接受 `644` / `0644` / `0o644` / `u=rw,go=r` 之外的纯八进制形式；
/// 范围 0..=0o7777（含 setuid/setgid/sticky）。空/含非法字符/超过 4 位拒绝。
pub fn parse_mode(input: &str) -> Result<u32, String> {
    let trimmed = input.trim();
    let digits = trimmed.strip_prefix("0o").unwrap_or(trimmed);
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| (b'0'..=b'7').contains(&b))
    {
        return Err(format!(
            "权限必须是 0–4 位八进制（如 644），实际: {input:?}"
        ));
    }
    let mode = u32::from_str_radix(digits, 8).map_err(|_| format!("权限无效: {input:?}"))?;
    if mode > 0o7777 {
        return Err(format!("权限超出 0–7777: {input:?}"));
    }
    Ok(mode)
}

/// 后台元数据任务：启动后立即返回，结果通过非阻塞 `events.try_recv()` 交给 UI。
///
/// 与 [`TransferJob`] 不同，它没有进度事件，也不要求 UI join：worker 发完唯一
/// 的结果就退出；UI 在 poll 里收到结果后丢弃句柄即可（不会阻塞）。
pub struct SftpOpJob {
    pub events: Receiver<SftpOutcome>,
    cancel: Arc<AtomicBool>,
    timeout: Duration,
    join: Option<JoinHandle<()>>,
}

impl SftpOpJob {
    /// 启动生产任务（无 UI 唤醒；测试/探针用；UI 用 [`Self::start_woken`]）。
    #[allow(dead_code)]
    pub fn start(command: SftpCommand) -> Self {
        let timeout = command.timeout();
        Self::start_inner(command, Path::new("sftp").to_path_buf(), timeout, None)
    }

    /// 生产入口：结果（含超时）写通道后额外唤醒 UI，把完成延迟从 400ms 降到事件级。
    pub fn start_woken(command: SftpCommand, wake: WakeSignal) -> Self {
        let timeout = command.timeout();
        Self::start_inner(
            command,
            Path::new("sftp").to_path_buf(),
            timeout,
            Some(wake),
        )
    }

    /// 注入程序/超时的启动入口（测试/探针用；生产用 [`Self::start_woken`]）。
    #[allow(dead_code)]
    pub fn start_with(
        command: SftpCommand,
        program: std::path::PathBuf,
        timeout: Duration,
    ) -> Self {
        Self::start_inner(command, program, timeout, None)
    }

    /// 内部统一入口；`wake` 为 `Some` 时结果到达后立即通知 UI。
    fn start_inner(
        command: SftpCommand,
        program: std::path::PathBuf,
        timeout: Duration,
        wake: Option<WakeSignal>,
    ) -> Self {
        let (sender, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let join = thread::spawn(move || {
            let outcome = command.execute_with_cancel(&program, timeout, Some(&worker_cancel));
            // 先写通道再唤醒：UI 被唤醒后 drain 一定能看到结果，不会空转。
            if sender.send(outcome).is_ok() {
                if let Some(wake) = &wake {
                    wake.notify();
                }
            }
        });
        Self {
            events,
            cancel,
            timeout,
            join: Some(join),
        }
    }

    /// UI 线程调用：标记取消并唤醒 worker 的轮询（worker 负责 kill 子进程组）。
    /// 不等待、不阻塞；结果仍会以 `Err("sftp 操作已取消")` 形式回到通道。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        eprintln!("[sftp] 已请求取消远端操作");
    }

    /// UI 线程非阻塞收结果。
    pub fn try_recv(&self) -> Result<SftpOutcome, mpsc::TryRecvError> {
        self.events.try_recv()
    }

    /// 测试/探针：等结果并回收线程；只应在确认 worker 已结束（或可接受等待）时用。
    #[allow(dead_code)]
    pub fn wait(mut self) -> Option<SftpOutcome> {
        let outcome = self.events.recv_timeout(self.timeout + KILL_GRACE * 4).ok();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        outcome
    }
}

impl Drop for SftpOpJob {
    fn drop(&mut self) {
        // 绝不 join：UI 侧丢弃时 worker 可能还在等 10s 超时；让它自行结束。
        // 进程退出时 OS 会回收；正常收到结果后 join 也已在 wait 里做过。
        self.join.take();
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
        token
            .strip_suffix('%')
            .and_then(|v| v.parse::<u8>().ok())
            .is_some_and(|v| v <= 100)
    })?;
    if index == 0 {
        return None;
    }
    let percent = tokens[index].strip_suffix('%')?.parse::<u8>().ok()?;
    let bytes = parse_size(tokens.get(index + 1)?)?;
    let rate = tokens
        .get(index + 2)
        .copied()
        .unwrap_or_default()
        .to_string();
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
    Some(ProgressLine {
        path,
        percent,
        bytes,
        rate,
        eta,
    })
}

/// 解析一段可能含 `\r` / `\n` 的进度输出（多文件场景返回多条）。
pub fn parse_progress_output(output: &str) -> Vec<ProgressLine> {
    output
        .split(|c| c == '\r' || c == '\n')
        .filter_map(parse_progress_line)
        .collect()
}

/// `1024KB` / `1.2MB` / `0` → 字节数；未知单位返回 `None`。
fn parse_size(token: &str) -> Option<u64> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let split = token
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(token.len());
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
        let Some(position) = buffer
            .iter()
            .position(|byte| *byte == b'\r' || *byte == b'\n')
        else {
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
    Finished {
        cancelled: bool,
        error: Option<String>,
    },
}

/// 一次传输请求。下载始终用 `get -R`（对普通文件无副作用，对目录才能递归）；
/// 上传只有本地是目录时才加 `put -R`。
#[derive(Clone, Debug, PartialEq)]
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
                format!(
                    "progress\nput {flag}{} {}\n",
                    quote_batch_path(&self.local),
                    quote_batch_path(&self.remote)
                )
            }
            TransferDirection::Download => {
                format!(
                    "progress\nget -R {} {}\n",
                    quote_batch_path(&self.remote),
                    quote_batch_path(&self.local)
                )
            }
        }
    }

    /// UI 队列里的一行（不含密码/主机凭据）。
    pub fn queue_label(&self) -> String {
        let (src, dst) = match self.direction {
            TransferDirection::Upload => (&self.local, &self.remote),
            TransferDirection::Download => (&self.remote, &self.local),
        };
        format!(
            "{} {} → {}",
            self.direction.verb(),
            display_name(src),
            display_name(dst)
        )
    }
}

/// `request_transfer` 的结果：覆盖风险明确时直接返回请求，否则先等 UI 确认。
#[derive(Clone, Debug, PartialEq)]
pub enum SftpSubmit {
    Ready(TransferRequest),
    ConfirmationRequired,
}

/// 等待用户显式确认的操作。确认前不启动传输/删除；取消只丢弃该状态。
#[derive(Clone, Debug, PartialEq)]
pub enum SftpConfirmation {
    Overwrite {
        request: TransferRequest,
        existing: bool,
    },
    RecursiveDelete {
        target: SftpTarget,
        remote: String,
    },
}

impl SftpConfirmation {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Overwrite { existing: true, .. } => "目标已存在，继续覆盖？",
            Self::Overwrite {
                existing: false, ..
            } => "无法确认目标，继续上传？",
            Self::RecursiveDelete { .. } => "递归删除目录？",
        }
    }

    pub fn text(&self) -> String {
        match self {
            Self::Overwrite { request, existing } => {
                let target = match request.direction {
                    TransferDirection::Upload => &request.remote,
                    TransferDirection::Download => &request.local,
                };
                if *existing {
                    format!(
                        "{} 目标已存在：{}\n继续{}会覆盖现有内容；取消不会改动任何文件。",
                        request.direction.verb(),
                        target,
                        request.direction.verb()
                    )
                } else {
                    format!(
                        "无法确认上传目标是否存在：{}\n继续上传可能覆盖现有文件；取消不会改动任何文件。",
                        request.remote
                    )
                }
            }
            Self::RecursiveDelete { remote, .. } => format!(
                "将逐层删除远端目录及其全部内容：\n{remote}\n删除不可恢复；取消不会改动任何文件。"
            ),
        }
    }

    pub fn ok_label(&self) -> &'static str {
        match self {
            Self::Overwrite { .. } => "覆盖",
            Self::RecursiveDelete { .. } => "递归删除",
        }
    }
}

/// 覆盖/递归删除确认状态机（纯数据，便于单测）。
///
/// 关键保证：`request_transfer` 遇到已存在/未知目标时**不返回可执行的请求**，
/// 只有 UI 调 `take()` 明确确认后才把请求交给传输队列；`cancel()` 只丢弃状态，
/// 不会触发任何文件改动。
#[derive(Debug, Default)]
pub struct SftpGate {
    pending: Option<SftpConfirmation>,
}

impl SftpGate {
    pub fn request_transfer(
        &mut self,
        request: TransferRequest,
        risk: OverwriteRisk,
    ) -> SftpSubmit {
        if risk.needs_confirmation() {
            self.pending = Some(SftpConfirmation::Overwrite {
                request,
                existing: risk == OverwriteRisk::Existing,
            });
            SftpSubmit::ConfirmationRequired
        } else {
            SftpSubmit::Ready(request)
        }
    }

    pub fn request_recursive_delete(&mut self, target: SftpTarget, remote: String) {
        self.pending = Some(SftpConfirmation::RecursiveDelete { target, remote });
    }

    pub fn pending(&self) -> Option<&SftpConfirmation> {
        self.pending.as_ref()
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// 用户确认：取出待执行动作；第二次调用返回 `None`。
    pub fn take(&mut self) -> Option<SftpConfirmation> {
        self.pending.take()
    }

    /// 用户取消：丢弃待执行动作（调用方不得再执行它）。
    pub fn cancel(&mut self) -> Option<SftpConfirmation> {
        self.pending.take()
    }
}

/// 上传/下载队列：纯数据结构，便于单测顺序/重试/清空。
///
/// UI 线程只做 push/pop，不持有子进程；同一时间仍只有一个 `TransferJob` 在跑，
/// 完成后由 `poll_sftp_transfer` 自动取下一个。
#[derive(Debug, Default)]
pub struct TransferQueue {
    pending: VecDeque<TransferRequest>,
}

impl TransferQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn push(&mut self, request: TransferRequest) -> usize {
        self.pending.push_back(request);
        self.pending.len()
    }

    /// 重试时插到队首（优先于用户后来排队的项）。
    pub fn push_front(&mut self, request: TransferRequest) -> usize {
        self.pending.push_front(request);
        self.pending.len()
    }

    pub fn pop(&mut self) -> Option<TransferRequest> {
        self.pending.pop_front()
    }

    pub fn clear(&mut self) -> usize {
        let n = self.pending.len();
        self.pending.clear();
        n
    }

    pub fn iter(&self) -> impl Iterator<Item = &TransferRequest> {
        self.pending.iter()
    }

    /// 队列展示行（最多 `limit` 条，超出用一行汇总）。
    pub fn labels(&self, limit: usize) -> Vec<String> {
        let mut out: Vec<String> = self
            .pending
            .iter()
            .take(limit)
            .map(|r| r.queue_label())
            .collect();
        if self.pending.len() > limit {
            out.push(format!("… 还有 {} 项", self.pending.len() - limit));
        }
        out
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
    /// 启动真实的 sftp 传输（无 UI 唤醒；测试/探针用）。
    pub fn start(request: &TransferRequest) -> Result<Self, String> {
        let mut command = Command::new("sftp");
        command.args(["-q", "-b", "-"]).args(request.target.args());
        Self::spawn_with_script(command, request.batch_script())
    }

    /// 生产入口：进度/结束事件写通道后额外唤醒 UI（容量 1，洪峰合并）。
    pub fn start_woken(request: &TransferRequest, wake: WakeSignal) -> Result<Self, String> {
        let mut command = Command::new("sftp");
        command.args(["-q", "-b", "-"]).args(request.target.args());
        Self::spawn_with_script_and_wake(command, request.batch_script(), Some(wake))
    }

    /// 启动任意子进程并喂一段脚本；抽出来便于用临时 shell 脚本测取消/状态机。
    pub fn spawn_with_script(command: Command, script: String) -> Result<Self, String> {
        Self::spawn_with_script_and_wake(command, script, None)
    }

    fn spawn_with_script_and_wake(
        mut command: Command,
        script: String,
        wake: Option<WakeSignal>,
    ) -> Result<Self, String> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 临时脚本刚写完时可能 ETXTBSY（测试/某些文件系统）；重试几次。
        let mut child = None;
        for attempt in 0..5 {
            match command.spawn() {
                Ok(spawned) => {
                    child = Some(spawned);
                    break;
                }
                Err(e) if e.raw_os_error() == Some(26) && attempt < 4 => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(format!("启动子进程失败: {e}")),
            }
        }
        let mut child = child.ok_or_else(|| "启动子进程失败: 重试后仍失败".to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "无法接管子进程 stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "无法读取子进程 stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "无法读取子进程 stderr".to_string())?;

        let child = Arc::new(Mutex::new(child));
        let cancel = Arc::new(AtomicBool::new(false));
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let (sender, events) = mpsc::channel();

        let stdout_sender = sender.clone();
        let stdout_wake = wake.clone();
        let stdout_reader =
            thread::spawn(move || read_stream(stdout, stdout_sender, None, stdout_wake));
        let stderr_sender = sender.clone();
        let stderr_sink = stderr_text.clone();
        let stderr_wake = wake.clone();
        let stderr_reader = thread::spawn(move || {
            read_stream(stderr, stderr_sender, Some(stderr_sink), stderr_wake)
        });

        let worker_child = child.clone();
        let worker_cancel = cancel.clone();
        let worker_stderr = stderr_text;
        let join = thread::spawn(move || {
            let mut stdin = stdin;
            let write_error = stdin
                .write_all(script.as_bytes())
                .err()
                .map(|e| format!("写入 batch 失败: {e}"));
            drop(stdin);
            if write_error.is_some() {
                terminate(&worker_child);
            }
            let status = wait_child(&worker_child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            let cancelled = worker_cancel.load(Ordering::SeqCst);
            let stderr = worker_stderr
                .lock()
                .map(|text| text.trim().to_string())
                .unwrap_or_default();
            let event = if cancelled {
                TransferEvent::Finished {
                    cancelled: true,
                    error: None,
                }
            } else {
                match status {
                    Ok(status) if status.success() => {
                        // 子进程已成功：即使写 stdin 时报了 EPIPE（sftp 提前退出），
                        // 也以实际退出码为准，避免把成功报成失败。
                        TransferEvent::Finished {
                            cancelled: false,
                            error: None,
                        }
                    }
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
                        TransferEvent::Finished {
                            cancelled: false,
                            error: Some(message),
                        }
                    }
                    Err(e) => TransferEvent::Finished {
                        cancelled: false,
                        error: Some(format!("等待 sftp 失败: {e}")),
                    },
                }
            };
            let _ = sender.send(event);
            if let Some(wake) = &wake {
                wake.notify();
            }
        });

        Ok(Self {
            events,
            cancel,
            child,
            join: Some(join),
        })
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
                }
                Ok(TransferEvent::Finished { cancelled, error }) => {
                    outcome = Some((cancelled, error));
                    break;
                }
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
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn read_stream<R: Read>(
    mut reader: R,
    sender: Sender<TransferEvent>,
    stderr_sink: Option<Arc<Mutex<String>>>,
    wake: Option<WakeSignal>,
) {
    let mut pending = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                for line in take_complete_lines(&mut pending) {
                    dispatch_line(&line, &sender, stderr_sink.as_ref(), wake.as_ref());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        let line = String::from_utf8_lossy(&pending).trim().to_string();
        if !line.is_empty() {
            dispatch_line(&line, &sender, stderr_sink.as_ref(), wake.as_ref());
        }
    }
}

fn dispatch_line(
    line: &str,
    sender: &Sender<TransferEvent>,
    stderr_sink: Option<&Arc<Mutex<String>>>,
    wake: Option<&WakeSignal>,
) {
    if let Some(progress) = parse_progress_line(line) {
        // 先写通道再唤醒，UI drain 时事件一定已在队列里。
        if sender.send(TransferEvent::Progress(progress)).is_ok() {
            if let Some(wake) = wake {
                wake.notify();
            }
        }
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
            }
            TransferEvent::Finished { cancelled, error } => {
                if cancelled {
                    self.phase = TransferPhase::Cancelled;
                } else if let Some(error) = error {
                    self.phase = TransferPhase::Failed;
                    self.message = error;
                } else {
                    self.phase = TransferPhase::Done;
                }
            }
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
            }
            TransferPhase::Failed => {
                format!("{}失败: {}", self.direction.verb(), self.message)
            }
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    fn target() -> SftpTarget {
        SftpTarget {
            host: "example.com".into(),
            port: 2222,
            user: "demo".into(),
            key_path: "/home/user/.ssh/id_ed25519".into(),
            proxy_jump: String::new(),
            extra_args: vec!["-o".into(), "StrictHostKeyChecking=yes".into()],
            ciphers: String::new(),
            macs: String::new(),
            kex_algorithms: String::new(),
            proxy_command: String::new(),
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
                // 用户 extra_args 在前（OpenSSH 先出现者生效），可覆盖默认超时。
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                "demo@example.com"
            ]
        );
    }

    #[test]
    fn args_bound_network_timeouts_and_allow_user_override() {
        let args = target().args();
        for option in [
            "ConnectTimeout=10",
            "ServerAliveInterval=5",
            "ServerAliveCountMax=2",
        ] {
            assert!(
                args.iter().any(|a| a == option),
                "缺少网络超时选项 {option}: {args:?}"
            );
        }
        // 用户在 extra_args 里写的 ConnectTimeout 必须排在默认值之前。
        let mut t = target();
        t.extra_args = vec!["-o".into(), "ConnectTimeout=30".into()];
        let args = t.args();
        let user = args.iter().position(|a| a == "ConnectTimeout=30").unwrap();
        let default = args.iter().position(|a| a == "ConnectTimeout=10").unwrap();
        assert!(user < default, "用户选项应先出现: {args:?}");
        assert!(args.last().is_some_and(|a| a == "demo@example.com"));

        let mut jump = target();
        jump.proxy_jump = "ops@bastion:2222".into();
        let args = jump.args();
        let j = args.iter().position(|a| a == "-J").expect("-J 缺失");
        assert_eq!(args[j + 1], "ops@bastion:2222");
        assert!(j < args.iter().position(|a| a == "demo@example.com").unwrap());
    }

    #[test]
    fn parse_mode_accepts_octal_and_rejects_junk() {
        for (input, want) in [
            ("644", 0o644),
            ("0644", 0o644),
            ("0o755", 0o755),
            (" 600 ", 0o600),
            ("7777", 0o7777),
            ("0", 0),
        ] {
            assert_eq!(parse_mode(input).unwrap(), want, "input={input:?}");
        }
        for bad in [
            "",
            "   ",
            "8",
            "9999",
            "06444",
            "-644",
            "rwxr-xr-x",
            "0x1ff",
        ] {
            assert!(parse_mode(bad).is_err(), "应拒绝 {bad:?}");
        }
    }

    #[test]
    fn target_requires_ssh_profile_fields() {
        let mut p = Profile {
            kind: "local".into(),
            ..Profile::default()
        };
        assert!(SftpTarget::from_profile(&p).unwrap_err().contains("SSH"));
        p.kind = "ssh".into();
        p.host = "h".into();
        p.user = String::new();
        assert!(SftpTarget::from_profile(&p).unwrap_err().contains("user"));
        p.user = "u".into();
        let t = SftpTarget::from_profile(&p).unwrap();
        assert_eq!(t.port, 22);
        assert!(t.key_path.is_empty());
        assert_eq!(
            t.args(),
            vec![
                "-P",
                "22",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                "u@h"
            ]
        );
    }

    #[test]
    fn list_parsing_drops_prompts() {
        let out = "sftp> ls -1 /tmp\nfile1\nfile2\n\nConnected to host.\n";
        assert_eq!(parse_list(out), vec!["file1", "file2"]);
    }

    #[test]
    fn parse_list_long_extracts_metadata_and_preserves_names() {
        let out = r#"sftp> ls -l /work
-rw-r--r--    1 yt   staff       1,234 Nov 20 10:00 report final.txt
drwxr-xr-x    2 yt   staff        4096 Nov 19  2023 old dir
lrwxrwxrwx    1 yt   staff           5 Nov 20 10:00 link name -> target file
drwxr-xr-x    2 yt   staff        4096 Nov 20 10:00 .
"#;
        let entries = parse_list_long(out);
        assert_eq!(entries.len(), 3, "entries={entries:?}");

        let file = &entries[0];
        assert_eq!(file.name, "report final.txt");
        assert_eq!(file.mode, "-rw-r--r--");
        assert_eq!(file.size, Some(1234));
        assert_eq!(file.mtime, "Nov 20 10:00");
        assert_eq!(file.is_dir, Some(false));
        let display = file.display();
        assert!(display.starts_with("-rw-r--r--"), "{display}");
        assert!(display.contains("1.2 KB"), "{display}");
        assert!(display.ends_with("report final.txt"), "{display}");

        let dir = &entries[1];
        assert_eq!(dir.name, "old dir");
        assert_eq!(dir.is_dir, Some(true));
        assert_eq!(dir.mtime, "Nov 19 2023");
        assert_eq!(dir.size, Some(4096));

        let link = &entries[2];
        assert_eq!(link.name, "link name");
        assert_eq!(link.link_target.as_deref(), Some("target file"));
        assert_eq!(link.is_dir, Some(false), "符号链接不能当作目录递归");
        assert!(link.display().ends_with("link name -> target file"));
    }

    #[test]
    fn parse_list_long_keeps_unparsed_names_without_metadata() {
        let entries = parse_list_long("alpha\n\nsftp> ls -l /tmp\nConnected to host.\nbeta\n");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(entries
            .iter()
            .all(|entry| entry.mode.is_empty() && entry.is_dir.is_none()));
    }

    #[test]
    fn overwrite_risk_uses_only_fresh_matching_list() {
        let entries = vec![
            RemoteEntry::from_name("alpha.txt"),
            RemoteEntry::from_name("dir1"),
        ];
        assert_eq!(
            overwrite_risk(Some("/work/"), "/work", &entries, "alpha.txt"),
            OverwriteRisk::Existing
        );
        assert_eq!(
            overwrite_risk(Some("/work"), "/work/", &entries, "new.txt"),
            OverwriteRisk::None
        );
        assert_eq!(
            overwrite_risk(Some("/work"), "/other", &entries, "alpha.txt"),
            OverwriteRisk::Unknown,
            "目录切换后不能拿旧列表下不存在结论"
        );
        assert_eq!(
            overwrite_risk(None, "/work", &entries, "alpha.txt"),
            OverwriteRisk::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_target_exists_counts_files_dirs_and_broken_links() {
        let dir = temp_dir("local-exists");
        let file = dir.join("target.bin");
        assert!(!local_target_exists(file.to_str().unwrap()));
        std::fs::write(&file, b"x").unwrap();
        assert!(local_target_exists(file.to_str().unwrap()));
        assert!(local_target_exists(dir.to_str().unwrap()));

        let link = dir.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(local_target_exists(link.to_str().unwrap()));
        std::fs::remove_file(&file).unwrap();
        assert!(
            local_target_exists(link.to_str().unwrap()),
            "断链也必须先确认，不能被 get 覆盖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrite_gate_requires_explicit_accept_or_cancel() {
        let request = TransferRequest {
            direction: TransferDirection::Upload,
            target: target(),
            local: "/tmp/alpha.txt".into(),
            remote: "/work/alpha.txt".into(),
            recursive: false,
        };
        let mut gate = SftpGate::default();
        assert_eq!(
            gate.request_transfer(request.clone(), OverwriteRisk::None),
            SftpSubmit::Ready(request.clone())
        );
        assert!(!gate.is_pending());

        assert_eq!(
            gate.request_transfer(request.clone(), OverwriteRisk::Existing),
            SftpSubmit::ConfirmationRequired
        );
        assert!(gate.is_pending());
        let pending = gate.pending().expect("应进入确认状态");
        assert!(pending.title().contains("覆盖"));
        assert!(pending.text().contains("/work/alpha.txt"));
        assert_eq!(pending.ok_label(), "覆盖");

        // 取消只丢弃状态，不会产出可执行请求。
        let cancelled = gate.cancel().expect("cancel 应取走待确认动作");
        assert!(matches!(
            cancelled,
            SftpConfirmation::Overwrite { existing: true, .. }
        ));
        assert!(!gate.is_pending());
        assert!(gate.take().is_none());
    }

    #[test]
    fn overwrite_gate_confirms_unknown_target_and_accepts_once() {
        let request = TransferRequest {
            direction: TransferDirection::Download,
            target: target(),
            local: "/tmp/existing.bin".into(),
            remote: "/work/existing.bin".into(),
            recursive: false,
        };
        let mut gate = SftpGate::default();
        assert_eq!(
            gate.request_transfer(request.clone(), OverwriteRisk::Unknown),
            SftpSubmit::ConfirmationRequired
        );
        assert!(gate.pending().unwrap().text().contains("无法确认"));
        assert_eq!(
            gate.take(),
            Some(SftpConfirmation::Overwrite {
                request,
                existing: false
            })
        );
        assert!(gate.take().is_none(), "确认动作只能执行一次");
    }

    #[test]
    fn recursive_delete_gate_names_path_and_cancels_cleanly() {
        let mut gate = SftpGate::default();
        gate.request_recursive_delete(target(), "/work/tree".into());
        let pending = gate.pending().expect("应进入确认状态");
        assert_eq!(pending.title(), "递归删除目录？");
        assert!(pending.text().contains("/work/tree"));
        assert_eq!(pending.ok_label(), "递归删除");
        let cancelled = gate.cancel().expect("cancel 应取走删除动作");
        assert!(
            matches!(cancelled, SftpConfirmation::RecursiveDelete { remote, .. } if remote == "/work/tree")
        );
        assert!(gate.take().is_none());
    }

    #[test]
    fn recursive_command_uses_extended_total_timeout() {
        let command = SftpCommand::RemoveRecursive {
            target: target(),
            remote: "/work/tree".into(),
        };
        assert!(command.is_recursive_delete());
        assert_eq!(command.timeout(), SFTP_RECURSIVE_TIMEOUT);
        assert_eq!(
            SftpCommand::List {
                target: target(),
                remote: "/".into()
            }
            .timeout(),
            SFTP_TIMEOUT
        );
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
            assert!(
                parse_progress_line(line).is_none(),
                "should reject: {line:?}"
            );
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
        assert_eq!(
            take_complete_lines(&mut buffer),
            vec!["one", "two", "three"]
        );
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

        state.apply(TransferEvent::Finished {
            cancelled: false,
            error: None,
        });
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
        cancelled.apply(TransferEvent::Finished {
            cancelled: true,
            error: None,
        });
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
        state.apply(TransferEvent::Finished {
            cancelled: false,
            error: None,
        });
        assert!(state.is_done_upload());

        let mut download =
            TransferState::running(TransferDirection::Download, "/tmp/b", "/remote/b");
        download.apply(TransferEvent::Finished {
            cancelled: false,
            error: None,
        });
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
        assert_eq!(
            download.batch_script(),
            "progress\nget -R \"/remote/local.bin\" \"/tmp/local.bin\"\n"
        );

        let upload_file = TransferRequest {
            direction: TransferDirection::Upload,
            target: target(),
            local: "/tmp/local.bin".into(),
            remote: "/remote/local.bin".into(),
            recursive: false,
        };
        assert_eq!(
            upload_file.batch_script(),
            "progress\nput \"/tmp/local.bin\" \"/remote/local.bin\"\n"
        );

        let upload_dir = TransferRequest {
            recursive: true,
            ..upload_file
        };
        assert_eq!(
            upload_dir.batch_script(),
            "progress\nput -R \"/tmp/local.bin\" \"/remote/local.bin\"\n"
        );
    }

    #[test]
    fn parent_remote_handles_root_and_nested_paths() {
        assert_eq!(parent_remote("/"), "/");
        assert_eq!(parent_remote(""), "/");
        assert_eq!(parent_remote("/tmp/a"), "/tmp");
        assert_eq!(parent_remote("/tmp/a/"), "/tmp");
        assert_eq!(parent_remote("/tmp"), "/");
    }

    #[test]
    fn quote_batch_path_escapes_spaces_quotes_and_backslashes() {
        assert_eq!(quote_batch_path("/tmp/a b"), "\"/tmp/a b\"");
        assert_eq!(quote_batch_path("/tmp/a\"b"), "\"/tmp/a\\\"b\"");
        assert_eq!(quote_batch_path("/tmp/a\\b"), "\"/tmp/a\\\\b\"");
        assert_eq!(quote_batch_path(""), "\".\"");
    }

    #[cfg(unix)]
    #[test]
    fn batch_commands_quote_special_paths_everywhere() {
        let dir = temp_dir("quote-cmds");
        let (script, capture) = capture_batch(&dir);
        let read = || std::fs::read_to_string(&capture).unwrap_or_default();
        let remote = "/tmp/a b\"c\\d;e";
        let remote2 = "/tmp/out 'x';y";
        let quoted = quote_batch_path(remote);
        let quoted2 = quote_batch_path(remote2);

        let list = SftpCommand::List {
            target: target(),
            remote: remote.into(),
        }
        .execute_with(&script, Duration::from_secs(2));
        assert!(
            matches!(list, SftpOutcome::List { items: Ok(_), .. }),
            "{list:?}"
        );
        assert_eq!(
            read(),
            format!("ls -la {quoted}\n"),
            "list 必须转义远端路径"
        );

        let _ = SftpCommand::Mkdir {
            target: target(),
            remote: remote.into(),
        }
        .execute_with(&script, Duration::from_secs(2));
        assert_eq!(read(), format!("mkdir {quoted}\n"));

        let _ = SftpCommand::Rename {
            target: target(),
            from: remote.into(),
            to: remote2.into(),
        }
        .execute_with(&script, Duration::from_secs(2));
        assert_eq!(read(), format!("rename {quoted} {quoted2}\n"));

        let _ = SftpCommand::Remove {
            target: target(),
            remote: remote.into(),
        }
        .execute_with(&script, Duration::from_secs(2));
        assert_eq!(read(), format!("rm {quoted}\n"));

        let _ = SftpCommand::Chmod {
            target: target(),
            remote: remote.into(),
            mode: 0o640,
        }
        .execute_with(&script, Duration::from_secs(2));
        assert_eq!(read(), format!("chmod 0640 {quoted}\n"));

        // put/get 的 batch 是纯函数，直接断言两个参数都被 quoting。
        let upload = TransferRequest {
            direction: TransferDirection::Upload,
            target: target(),
            local: remote.into(),
            remote: remote2.into(),
            recursive: false,
        };
        assert_eq!(
            upload.batch_script(),
            format!("progress\nput {quoted} {quoted2}\n")
        );
        let download = TransferRequest {
            direction: TransferDirection::Download,
            target: target(),
            local: remote2.into(),
            remote: remote.into(),
            recursive: false,
        };
        assert_eq!(
            download.batch_script(),
            format!("progress\nget -R {quoted} {quoted2}\n")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn list_fallback_to_short_format_still_quotes_path() {
        let dir = temp_dir("quote-fallback");
        let capture = dir.join("batch.txt");
        let script = write_script(
            &dir,
            &format!(
                "#!/bin/sh\nbatch=$(cat)\nprintf '%s\\n' \"$batch\" > '{}'\ncase \"$batch\" in\n  ls\\ -l*) exit 1;;\nesac\nprintf 'alpha\\nbeta\\n'\n",
                capture.display()
            ),
        );
        let remote = "/tmp/a b\"c";
        let quoted = quote_batch_path(remote);
        let outcome = SftpCommand::List {
            target: target(),
            remote: remote.into(),
        }
        .execute_with(&script, Duration::from_secs(2));
        match outcome {
            SftpOutcome::List {
                items: Ok(items), ..
            } => {
                let names: Vec<&str> = items.iter().map(|entry| entry.name.as_str()).collect();
                assert_eq!(names, vec!["alpha", "beta"]);
                assert!(items.iter().all(|entry| entry.mode.is_empty()));
            }
            other => panic!("期望回退 ls -1 成功，得到 {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&capture).unwrap(),
            format!("ls -1a {quoted}\n"),
            "回退分支也必须调用 quote_batch_path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_keeps_fifo_order_retry_front_and_clear() {
        let request = |name: &str, direction| TransferRequest {
            direction,
            target: target(),
            local: format!("/tmp/{name}"),
            remote: format!("/remote/{name}"),
            recursive: false,
        };
        let mut q = TransferQueue::new();
        assert!(q.is_empty());
        q.push(request("a", TransferDirection::Upload));
        q.push(request("b", TransferDirection::Download));
        assert_eq!(q.len(), 2);
        let labels = q.labels(2);
        assert_eq!(labels.len(), 2);
        assert!(labels[0].contains("上传") && labels[0].contains('a'));
        assert!(labels[1].contains("下载") && labels[1].contains('b'));
        assert_eq!(q.pop().unwrap().local, "/tmp/a", "FIFO");
        q.push_front(request("retry", TransferDirection::Upload));
        assert_eq!(q.pop().unwrap().local, "/tmp/retry", "重试插到队首");
        assert_eq!(q.pop().unwrap().local, "/tmp/b");
        assert!(q.pop().is_none());

        q.push(request("x", TransferDirection::Upload));
        q.push(request("y", TransferDirection::Upload));
        q.push(request("z", TransferDirection::Upload));
        let labels = q.labels(2);
        assert_eq!(labels.len(), 3);
        assert!(labels[2].contains("还有 1 项"), "{labels:?}");
        assert_eq!(q.clear(), 3);
        assert!(q.is_empty());
    }

    #[test]
    fn chmod_command_and_outcome_are_wired() {
        let command = SftpCommand::Chmod {
            target: target(),
            remote: "/remote/file".into(),
            mode: 0o640,
        };
        let SftpOutcome::Chmod { path, mode, result } =
            command.execute_with(Path::new("/bin/false"), Duration::from_millis(500))
        else {
            panic!("期望 Chmod outcome");
        };
        assert_eq!(path, "/remote/file");
        assert_eq!(mode, 0o640);
        assert!(result.is_err());
        let SftpOutcome::Chmod { result, .. } = SftpCommand::Chmod {
            target: target(),
            remote: "/remote/file".into(),
            mode: 0o644,
        }
        .execute_with(Path::new("/bin/true"), Duration::from_millis(500)) else {
            panic!("期望 Chmod outcome");
        };
        assert!(result.is_ok());
    }

    #[cfg(unix)]
    fn temp_dir(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("xtervo-sftp-{}-{tag}-{unique}", std::process::id()));
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

    /// 假 sftp：把收到的 batch 原样落盘，然后成功退出。
    #[cfg(unix)]
    fn capture_batch(dir: &Path) -> (PathBuf, PathBuf) {
        let capture = dir.join("batch.txt");
        let script = write_script(
            dir,
            &format!("#!/bin/sh\ncat > '{}'\nprintf 'ok\\n'\n", capture.display()),
        );
        (script, capture)
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
        let first = job
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("应收到进度事件");
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
                }
                Ok(TransferEvent::Progress(_)) => {}
                Err(_) => break,
            }
        }
        let (cancelled, error) = finished.expect("取消后应收到 Finished 事件");
        assert!(cancelled);
        assert!(error.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消后应及时回收"
        );
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
        let mut state = TransferState::running(
            TransferDirection::Upload,
            "/tmp/done.bin",
            "/remote/done.bin",
        );
        let mut done = false;
        while !done {
            match job
                .events
                .recv_timeout(Duration::from_secs(5))
                .expect("任务应结束")
            {
                TransferEvent::Progress(progress) => state.apply(TransferEvent::Progress(progress)),
                TransferEvent::Finished { cancelled, error } => {
                    state.apply(TransferEvent::Finished { cancelled, error });
                    done = true;
                }
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
    fn transfer_job_wake_fires_before_poll_and_after_event_is_queued() {
        let dir = temp_dir("wake");
        let script = write_script(
            &dir,
            "#!/bin/sh\nprintf 'wake.bin 10%% 1KB 1.0MB/s --:-- ETA\\r'\nsleep 0.05\nprintf 'wake.bin 100%% 2KB 1.0MB/s 00:00\\r'\nexit 0\n",
        );
        let (wake, rx) = WakeSignal::new();
        let job = TransferJob::spawn_with_script_and_wake(
            Command::new(&script),
            String::new(),
            Some(wake),
        )
        .unwrap();
        let started = Instant::now();
        rx.recv_timeout(Duration::from_secs(2))
            .expect("进度事件应唤醒 UI");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "唤醒不应等 400ms 轮询: {:?}",
            started.elapsed()
        );
        // 唤醒只是一个“去看”的信号：事件必须已经在独立通道里，不能靠唤醒携带。
        let first = job.events.try_recv().expect("唤醒后进度事件必须已在队列");
        assert!(
            matches!(first, TransferEvent::Progress(_)),
            "first={first:?}"
        );

        // 等待结束事件（期间可能有合并的进度唤醒）。
        let mut finished = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let _ = rx.recv_timeout(Duration::from_millis(200));
            while let Ok(event) = job.events.try_recv() {
                if let TransferEvent::Finished { cancelled, error } = event {
                    finished = Some((cancelled, error));
                }
            }
            if finished.is_some() {
                break;
            }
        }
        assert_eq!(finished, Some((false, None)), "应收到成功结束事件");
        job.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn sftp_op_job_wake_fires_before_400ms_poll() {
        let dir = temp_dir("op-wake");
        let script = write_script(&dir, "#!/bin/sh\ncat >/dev/null\nprintf 'one\\ntwo\\n'\n");
        let (wake, rx) = WakeSignal::new();
        let job = SftpOpJob::start_inner(
            SftpCommand::List {
                target: target(),
                remote: "/".into(),
            },
            script,
            SFTP_TIMEOUT,
            Some(wake),
        );
        let started = Instant::now();
        rx.recv_timeout(Duration::from_secs(2))
            .expect("后台结果应唤醒 UI");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "结果唤醒不应等 400ms 轮询: {:?}",
            started.elapsed()
        );
        let outcome = job.try_recv().expect("唤醒后结果必须已在队列");
        match outcome {
            SftpOutcome::List {
                items: Ok(items), ..
            } => {
                let names: Vec<&str> = items.iter().map(|entry| entry.name.as_str()).collect();
                assert_eq!(names, vec!["one", "two"]);
            }
            other => panic!("期望 List 成功，得到 {other:?}"),
        }
        let _ = job.wait(); // 已 drain；这里只回收 worker
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn transfer_job_captures_stderr_failure() {
        let dir = temp_dir("fail");
        let script = write_script(
            &dir,
            "#!/bin/sh\nprintf 'permission denied\\n' >&2\nexit 3\n",
        );
        let job = TransferJob::spawn_with_script(Command::new(&script), String::new()).unwrap();
        let mut state = TransferState::running(TransferDirection::Download, "/tmp/x", "/remote/x");
        let event = job
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("任务应结束");
        match event {
            TransferEvent::Finished { cancelled, error } => {
                assert!(!cancelled);
                let error = error.expect("非零退出码应带错误信息");
                assert!(error.contains("permission denied"), "error={error}");
                state.apply(TransferEvent::Finished {
                    cancelled,
                    error: Some(error),
                });
            }
            other => panic!("期望 Finished 事件，得到 {other:?}"),
        }
        assert_eq!(state.phase, TransferPhase::Failed);
        job.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------- 元数据超时/后台

    #[test]
    fn timeout_text_rounds_partial_seconds_up() {
        // 10s 预算先减去已耗时，再进入单次调用；剩余 9.99s 不能显示成 9s。
        assert_eq!(format_timeout(SFTP_TIMEOUT), "10s");
        assert_eq!(format_timeout(Duration::from_millis(9_999)), "10s");
        assert_eq!(format_timeout(Duration::from_millis(9_001)), "10s");
        // 真正的亚秒超时仍用毫秒，不凑成 1s。
        assert_eq!(format_timeout(Duration::from_millis(999)), "999ms");
        assert_eq!(format_timeout(Duration::from_millis(250)), "250ms");
    }

    #[cfg(unix)]
    #[test]
    fn run_command_hard_timeout_kills_child() {
        let dir = temp_dir("timeout");
        // exec 让 shell 自身变成 sleep，超时 kill 能直接命中进程组。
        let script = write_script(&dir, "#!/bin/sh\nexec sleep 30\n");
        let started = Instant::now();
        let err = run_command_with_timeout(Command::new(&script), "", Duration::from_millis(250))
            .expect_err("sleep 30 必须超时");
        let elapsed = started.elapsed();
        assert!(err.contains("超时"), "error={err}");
        assert!(err.contains("已终止"), "error 应说明已 kill: {err}");
        assert!(
            elapsed >= Duration::from_millis(200),
            "过早返回: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时回收太慢: {elapsed:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn sftp_op_job_start_is_non_blocking_and_reports_timeout() {
        let dir = temp_dir("job-timeout");
        let script = write_script(&dir, "#!/bin/sh\nexec sleep 30\n");
        let started = Instant::now();
        // 旧实现这里会同步 wait_with_output，UI 冻结 30s；新实现只 spawn 线程。
        let job = SftpOpJob::start_with(
            SftpCommand::List {
                target: target(),
                remote: "/tmp".into(),
            },
            script,
            Duration::from_millis(300),
        );
        let start_cost = started.elapsed();
        assert!(
            start_cost < Duration::from_millis(100),
            "SftpOpJob::start 阻塞了调用线程: {start_cost:?}"
        );
        let outcome = job
            .events
            .recv_timeout(Duration::from_secs(3))
            .expect("后台任务应在超时后发结果");
        match outcome {
            SftpOutcome::List { items: Err(e), .. } => {
                assert!(e.contains("超时"), "error={e}");
                assert!(e.contains("已终止"), "error 应说明已 kill: {e}");
            }
            other => panic!("期望 List 超时结果，得到 {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "后台超时没有及时返回: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn sftp_op_job_cancel_kills_child_and_reports_cancelled() {
        let dir = temp_dir("op-cancel");
        let script = write_script(&dir, "#!/bin/sh\nexec sleep 30\n");
        let job = SftpOpJob::start_with(
            SftpCommand::List {
                target: target(),
                remote: "/tmp".into(),
            },
            script,
            Duration::from_secs(10),
        );
        std::thread::sleep(Duration::from_millis(80));
        let started = Instant::now();
        job.cancel();
        let outcome = job
            .events
            .recv_timeout(Duration::from_secs(3))
            .expect("取消后应尽快收到结果");
        match outcome {
            SftpOutcome::List { items: Err(e), .. } => {
                assert!(e.contains("已取消"), "error={e}");
            }
            other => panic!("期望 List 取消结果，得到 {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(3), "取消回收太慢");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn sftp_command_list_and_mkdir_use_fake_program_successfully() {
        let dir = temp_dir("fake-ok");
        let script = write_script(
            &dir,
            "#!/bin/sh\ncat >/dev/null\nprintf 'alpha\\nbeta\\n'\n",
        );

        let list = SftpCommand::List {
            target: target(),
            remote: "/tmp".into(),
        }
        .execute_with(&script, Duration::from_secs(2));
        match list {
            SftpOutcome::List { remote, items } => {
                assert_eq!(remote, "/tmp");
                let items = items.expect("list 应成功");
                let names: Vec<&str> = items.iter().map(|entry| entry.name.as_str()).collect();
                assert_eq!(names, vec!["alpha", "beta"]);
            }
            other => panic!("期望 List 结果，得到 {other:?}"),
        }

        let mkdir = SftpCommand::Mkdir {
            target: target(),
            remote: "/tmp/new".into(),
        }
        .execute_with(&script, Duration::from_secs(2));
        match mkdir {
            SftpOutcome::Mkdir { remote, result } => {
                assert_eq!(remote, "/tmp/new");
                assert_eq!(result, Ok(()));
            }
            other => panic!("期望 Mkdir 结果，得到 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn remove_falls_back_to_rmdir_within_one_deadline() {
        let dir = temp_dir("rm-fallback");
        // 读 batch 脚本：rm 失败，rmdir 成功；总耗时仍受同一个 timeout 约束。
        let script = write_script(
            &dir,
            "#!/bin/sh\nscript=$(cat)\ncase \"$script\" in\n  rmdir*) exit 0;;\n  rm*) echo 'rm denied' >&2; exit 1;;\n  *) exit 2;;\nesac\n",
        );
        let outcome = SftpCommand::Remove {
            target: target(),
            remote: "/tmp/dir".into(),
        }
        .execute_with(&script, Duration::from_secs(2));
        match outcome {
            SftpOutcome::Remove { path, result } => {
                assert_eq!(path, "/tmp/dir");
                assert_eq!(result, Ok(()), "rm 失败后应回退 rmdir");
            }
            other => panic!("期望 Remove 结果，得到 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------ 递归删除（内存假远端）

    #[test]
    fn recursive_child_paths_reject_escape_names() {
        assert_eq!(
            child_remote_path("/root", "a b.txt").unwrap(),
            "/root/a b.txt"
        );
        assert_eq!(child_remote_path("/", "a").unwrap(), "/a");
        for bad in ["", ".", "..", "/etc/passwd", "../escape", "sub/child"] {
            assert!(
                child_remote_path("/root", bad).is_err(),
                "可疑条目名必须中止递归删除: {bad:?}"
            );
        }
    }

    fn fake_parent(path: &str) -> String {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(0) | None => "/".into(),
            Some(index) => trimmed[..index].to_string(),
        }
    }

    fn fake_base(path: &str) -> &str {
        path.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
    }

    fn unquote_batch_path(input: &str) -> String {
        let trimmed = input.trim();
        let inner = if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        };
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// 内存假远端：解释 sftp batch，模拟 `rm` 遇目录失败、`rmdir` 遇非空失败。
    #[derive(Default)]
    struct FakeRemote {
        dirs: BTreeSet<String>,
        files: BTreeSet<String>,
        links: BTreeMap<String, String>,
        log: Vec<String>,
        fail_rm: Option<String>,
        /// 模拟只支持 `ls -1` 的旧服务器：`ls -l` 永远返回空输出。
        short_only: bool,
    }

    impl FakeRemote {
        fn dir(&mut self, path: &str) {
            self.dirs.insert(path.to_string());
        }

        fn file(&mut self, path: &str) {
            self.files.insert(path.to_string());
        }

        fn link(&mut self, path: &str, target: &str) {
            self.files.insert(path.to_string());
            self.links.insert(path.to_string(), target.to_string());
        }

        fn children(&self, parent: &str) -> Vec<String> {
            let mut out: Vec<String> = self
                .dirs
                .iter()
                .chain(self.files.iter())
                .filter(|path| fake_parent(path) == parent)
                .cloned()
                .collect();
            out.sort();
            out
        }

        fn long_line(&self, path: &str) -> String {
            let name = fake_base(path);
            if self.dirs.contains(path) {
                format!("drwxr-xr-x    2 yt yt     4096 Nov 20 10:00 {name}")
            } else if let Some(target) = self.links.get(path) {
                format!("lrwxrwxrwx    1 yt yt        5 Nov 20 10:00 {name} -> {target}")
            } else {
                format!("-rw-r--r--    1 yt yt       16 Nov 20 10:00 {name}")
            }
        }

        fn apply(&mut self, batch: &str) -> Result<String, String> {
            let mut output = String::new();
            for line in batch.lines().map(str::trim).filter(|line| !line.is_empty()) {
                self.log.push(line.to_string());
                let long = line
                    .strip_prefix("ls -la ")
                    .or_else(|| line.strip_prefix("ls -l "));
                if let Some(rest) = long {
                    let path = unquote_batch_path(rest);
                    if !self.dirs.contains(&path) {
                        return Err(format!("Can't ls: {path}: No such file"));
                    }
                    if self.short_only {
                        continue;
                    }
                    for child in self.children(&path) {
                        output.push_str(&self.long_line(&child));
                        output.push('\n');
                    }
                } else if let Some(rest) = line
                    .strip_prefix("ls -1a ")
                    .or_else(|| line.strip_prefix("ls -1 "))
                {
                    let path = unquote_batch_path(rest);
                    if !self.dirs.contains(&path) {
                        return Err(format!("Can't ls: {path}: No such file"));
                    }
                    for child in self.children(&path) {
                        output.push_str(fake_base(&child));
                        output.push('\n');
                    }
                } else if let Some(rest) = line.strip_prefix("rmdir ") {
                    let path = unquote_batch_path(rest);
                    if !self.dirs.contains(&path) {
                        return Err(format!("Couldn't remove directory: {path}"));
                    }
                    if !self.children(&path).is_empty() {
                        return Err(format!("Couldn't remove directory: {path}: not empty"));
                    }
                    self.dirs.remove(&path);
                } else if let Some(rest) = line.strip_prefix("rm ") {
                    let path = unquote_batch_path(rest);
                    if self.fail_rm.as_deref() == Some(path.as_str()) {
                        return Err(format!("Couldn't delete file: {path}: denied"));
                    }
                    if !self.files.remove(&path) {
                        return Err(format!("Couldn't delete file: {path}"));
                    }
                    self.links.remove(&path);
                } else {
                    return Err(format!("unexpected batch: {line}"));
                }
            }
            Ok(output)
        }
    }

    #[cfg(unix)]
    #[test]
    fn recursive_delete_removes_tree_bottom_up_without_following_links() {
        let mut fake = FakeRemote::default();
        fake.dir("/root");
        fake.dir("/root/sub");
        fake.dir("/root/sub/deep");
        fake.dir("/root/empty dir");
        fake.dir("/outside");
        fake.file("/root/a.txt");
        fake.file("/root/a b.txt");
        fake.file("/root/sub/b.txt");
        fake.file("/root/sub/deep/c.txt");
        fake.link("/root/link", "/outside");

        remove_tree_with("/root", Duration::from_secs(5), None, |batch, timeout| {
            let _ = timeout;
            fake.apply(batch)
        })
        .expect("递归删除应成功");

        assert!(!fake.dirs.contains("/root"));
        assert!(!fake.dirs.contains("/root/sub"));
        assert!(!fake.dirs.contains("/root/sub/deep"));
        assert!(!fake.dirs.contains("/root/empty dir"));
        assert!(!fake.files.contains("/root/a.txt"));
        assert!(!fake.files.contains("/root/a b.txt"));
        assert!(!fake.files.contains("/root/sub/b.txt"));
        assert!(!fake.files.contains("/root/sub/deep/c.txt"));
        assert!(!fake.files.contains("/root/link"));
        assert!(fake.dirs.contains("/outside"), "符号链接目标不能被跟随删除");
        assert!(fake.log.iter().any(|line| line == "rm \"/root/a b.txt\""));
        assert!(fake
            .log
            .iter()
            .any(|line| line == "rmdir \"/root/empty dir\""));
        assert!(fake.log.iter().any(|line| line == "rm \"/root/link\""));
        assert!(!fake.log.iter().any(|line| line.contains("/outside")));

        let sub = fake
            .log
            .iter()
            .position(|line| line == "rmdir \"/root/sub\"")
            .expect("应先删子目录");
        let root = fake
            .log
            .iter()
            .rposition(|line| line == "rmdir \"/root\"")
            .expect("最后删根目录");
        assert!(sub < root, "必须自底向上 rmdir: {:?}", fake.log);
        assert_eq!(fake.log.last().map(String::as_str), Some("rmdir \"/root\""));
    }

    #[cfg(unix)]
    #[test]
    fn recursive_delete_refuses_name_only_fallback_lists() {
        let mut fake = FakeRemote {
            short_only: true,
            ..Default::default()
        };
        fake.dir("/root");
        fake.dir("/root/sub");
        fake.dir("/root/empty");
        fake.file("/root/a.txt");
        fake.file("/root/sub/b.txt");

        let error = remove_tree_with("/root", Duration::from_secs(5), None, |batch, _| {
            fake.apply(batch)
        })
        .expect_err("只有 ls -1 时类型未知，必须拒绝递归删除");
        assert!(error.contains("无法确认"), "{error}");
        assert!(fake.dirs.contains("/root"));
        assert!(fake.files.contains("/root/a.txt"), "拒绝后不能删文件");
    }

    #[cfg(unix)]
    #[test]
    fn recursive_delete_cancel_and_timeout_do_not_touch_files() {
        let cancel = AtomicBool::new(true);
        let mut calls = 0;
        let error = remove_tree_with("/root", Duration::from_secs(5), Some(&cancel), |_, _| {
            calls += 1;
            Ok(String::new())
        })
        .expect_err("取消后必须报错");
        assert!(error.contains("已取消"), "{error}");
        assert_eq!(calls, 0, "取消后不能启动任何 sftp 调用");

        let mut calls = 0;
        let error = remove_tree_with("/root", Duration::ZERO, None, |_, _| {
            calls += 1;
            Ok(String::new())
        })
        .expect_err("零预算必须超时");
        assert!(error.contains("超时"), "{error}");
        assert_eq!(calls, 0);
    }

    #[cfg(unix)]
    #[test]
    fn recursive_delete_reports_failure_and_keeps_unfinished_dirs() {
        let mut fake = FakeRemote::default();
        fake.dir("/root");
        fake.file("/root/a.txt");
        fake.file("/root/blocked.txt");
        fake.fail_rm = Some("/root/blocked.txt".into());

        let error = remove_tree_with("/root", Duration::from_secs(5), None, |batch, _| {
            fake.apply(batch)
        })
        .expect_err("文件删除失败时不能报成功");
        assert!(error.contains("blocked.txt"), "{error}");
        assert!(
            fake.dirs.contains("/root"),
            "失败时根目录应保留，便于重试/排查"
        );
    }
}
