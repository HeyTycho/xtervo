// SPDX-License-Identifier: GPL-3.0-only
//! known_hosts 主机密钥预检：首次连接/密钥变更时给出明确提示，绝不静默信任。
//!
//! 连接 SSH Profile 之前，在**后台线程**里做两件本地/网络操作（最多
//! [`HOST_KEY_TIMEOUT`]，可取消）：
//! 1. 用 `ssh -G` 取本次连接真正生效的 `StrictHostKeyChecking` /
//!    `HostName` / `Port` / `UserKnownHostsFile` / `HostKeyAlias` / `ProxyJump`，
//!    再用 `ssh-keygen -F` 在 known_hosts 里查已有条目；
//! 2. 没有配置 ProxyJump 时用 `ssh-keyscan -T 5` 取服务器当前公钥，和已有条目
//!    按 key 类型逐项比较，得到 [`Verdict`]。
//!
//! 安全边界：
//! - `StrictHostKeyChecking` 显式设为 `no`/`off`/`accept-new` 时由用户配置接管，
//!   预检直接返回 [`Verdict::Known`]（不改默认行为，但也不额外弹出确认）。
//! - `ssh-keyscan` 失败/配置了跳板机时返回 [`Verdict::Unverifiable`]，调用方
//!   应回退到“让 ssh 在终端里显示指纹确认”，而不是直接信任。
//! - 只有用户显式点「信任/更新」才写 known_hosts；`approve`/`trust` 是仅有的
//!   写入口，且写入前会先删掉该主机的旧条目（变更场景）。
//! - 本模块不接触密码/私钥，日志只打印主机名、key 类型与指纹。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tervo_core::WakeSignal;

use crate::profile::Profile;

/// 单次主机密钥预检的硬超时（网络操作）。与 SFTP 元数据命令一致。
pub const HOST_KEY_TIMEOUT: Duration = Duration::from_secs(10);
/// `ssh-keyscan` 自己的连接超时；比外层硬超时短，失败信息更可解释。
const KEYSCAN_CONNECT_TIMEOUT: &str = "5";
const KILL_GRACE: Duration = Duration::from_millis(200);
const WAIT_POLL: Duration = Duration::from_millis(10);

/// 服务器/known_hosts 里的一条公钥。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownKey {
    /// 如 `ssh-ed25519` / `ecdsa-sha2-nistp256`。
    pub key_type: String,
    /// base64 公钥；比较是否变更时用它（不依赖注释/主机名）。
    pub key: String,
    /// 展示用指纹（`SHA256:...`）；ssh-keygen 不可用时退化为 key 前缀。
    pub fingerprint: String,
}

impl KnownKey {
    /// 指纹不可用时的可读兜底；绝不把完整公钥塞进日志/UI。
    fn fallback_fingerprint(key_type: &str, key: &str) -> String {
        let short: String = key.chars().take(12).collect();
        format!("{key_type}:{short}…")
    }
}

/// 预检结论。`Known` 之外的每种情况都需要用户明确选择。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 已有匹配主机密钥（或用户配置里显式关闭了确认）。
    Known,
    /// known_hosts 里没有该主机：首次连接。
    Unknown { current: Vec<KnownKey> },
    /// known_hosts 里该主机的某个 key 类型和服务器当前公钥不一致。
    Changed { old: Vec<KnownKey>, new: Vec<KnownKey> },
    /// 无法判断（keyscan 失败/跳板机/超时）。`local` 表示本地是否已有条目。
    Unverifiable { reason: String, local: bool },
}

impl Verdict {
    /// 是否需要弹出 UI 确认。
    pub fn needs_prompt(&self) -> bool {
        !matches!(self, Verdict::Known)
    }

    /// 一行摘要（不含秘密）。
    pub fn summary(&self) -> String {
        match self {
            Verdict::Known => "known_hosts 已确认".into(),
            Verdict::Unknown { current } => {
                format!("首次连接，服务器公钥 {} 条", current.len())
            },
            Verdict::Changed { old, new } => {
                format!("主机密钥已变更（旧 {} 条，新 {} 条）", old.len(), new.len())
            },
            Verdict::Unverifiable { reason, .. } => format!("无法预检主机密钥: {reason}"),
        }
    }

    /// UI 展示用的多行说明；先旧后新，带 key 类型与指纹。
    pub fn detail(&self) -> String {
        match self {
            Verdict::Known => String::new(),
            Verdict::Unknown { current } => {
                let mut out = String::from("首次连接，known_hosts 中没有该主机。服务器公钥：\n");
                for key in current {
                    out.push_str(&format!("  {} {}\n", key.key_type, key.fingerprint));
                }
                out
            },
            Verdict::Changed { old, new } => {
                let mut out = String::from("警告：服务器出示的主机密钥与 known_hosts 记录不一致！\n");
                out.push_str("旧：\n");
                for key in old {
                    out.push_str(&format!("  {} {}\n", key.key_type, key.fingerprint));
                }
                out.push_str("新：\n");
                for key in new {
                    out.push_str(&format!("  {} {}\n", key.key_type, key.fingerprint));
                }
                out
            },
            Verdict::Unverifiable { reason, local } => format!(
                "{reason}\n{}",
                if *local {
                    "known_hosts 里已有条目，但本次无法比对；继续连接后由 ssh 在终端里显示指纹，请人工确认。"
                } else {
                    "继续连接后由 ssh 在终端里显示主机指纹，请人工确认。"
                }
            ),
        }
    }
}

/// `ssh -G` 解析出的本次连接生效配置（只含非秘密字段）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveSsh {
    /// `ssh -G` 的 `stricthostkeychecking`（小写）。
    pub strict: String,
    /// 参与查找的 known_hosts 文件，按 ssh 的查找顺序。
    pub files: Vec<PathBuf>,
    /// 用户级 known_hosts（`UserKnownHostsFile` 的第一个）；信任写入用它，
    /// 绝不往 `/etc/ssh/ssh_known_hosts` 这类系统文件写。
    pub write_file: Option<PathBuf>,
    /// 查 known_hosts 用的键：`host`、`[host]:port` 或 `HostKeyAlias` 原文。
    pub spec: String,
    /// `ssh -G` 生效的 HostName（可能来自 ssh_config 的 Host 别名）。
    pub hostname: String,
    /// `ssh -G` 生效的 Port。
    pub port: u16,
    /// `ssh -G` 生效的 User。
    pub user: String,
    /// `ProxyJump`（空 = 直连）；有值时无法用 keyscan 预检。
    pub proxy_jump: String,
    /// `ProxyCommand`（空 = 无）；native 不支持。
    pub proxy_command: String,
}

/// 用户显式配置了 no/off/accept-new：不会弹主机密钥确认，预检直接放行。
fn strict_skips_prompt(strict: &str) -> bool {
    !matches!(strict, "ask" | "yes" | "true")
}

/// 解析 `ssh -G` 的输出。抽成纯函数便于用固定文本做单测。
pub fn parse_ssh_g(output: &str, profile: &Profile) -> EffectiveSsh {
    let mut strict = "ask".to_string();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut user_files: Vec<PathBuf> = Vec::new();
    let mut hostname = profile.host.trim().to_string();
    let mut port = profile.port;
    let mut host_key_alias: Option<String> = None;
    let mut proxy_jump = String::new();
    let mut proxy_command = String::new();
    let mut user = profile.user.trim().to_string();
    for line in output.lines() {
        if let Some(v) = line.strip_prefix("stricthostkeychecking ") {
            strict = v.trim().to_ascii_lowercase();
        } else if let Some(v) = line.strip_prefix("userknownhostsfile ") {
            let paths = split_ssh_paths(v);
            files.extend(paths.iter().cloned());
            user_files.extend(paths);
        } else if let Some(v) = line.strip_prefix("globalknownhostsfile ") {
            files.extend(split_ssh_paths(v));
        } else if let Some(v) = line.strip_prefix("hostname ") {
            let effective = v.trim();
            if !effective.is_empty() {
                hostname = effective.to_string();
            }
        } else if let Some(v) = line.strip_prefix("port ") {
            if let Ok(p) = v.trim().parse::<u16>() {
                port = p;
            }
        } else if let Some(v) = line.strip_prefix("hostkeyalias ") {
            let alias = v.trim();
            if !alias.is_empty() {
                host_key_alias = Some(alias.to_string());
            }
        } else if let Some(v) = line.strip_prefix("proxyjump ") {
            let jump = v.trim();
            if !jump.is_empty() && jump != "none" {
                proxy_jump = jump.to_string();
            }
        } else if let Some(v) = line.strip_prefix("proxycommand ") {
            let command = v.trim();
            if !command.is_empty() && command != "none" {
                proxy_command = command.to_string();
            }
        } else if let Some(v) = line.strip_prefix("user ") {
            let value = v.trim();
            if !value.is_empty() {
                user = value.to_string();
            }
        }
    }
    if files.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            let ssh = PathBuf::from(home).join(".ssh");
            files.push(ssh.join("known_hosts"));
            files.push(ssh.join("known_hosts2"));
            user_files.push(ssh.join("known_hosts"));
        }
    }
    let spec = match host_key_alias {
        Some(alias) => alias,
        None => host_spec(&hostname, port),
    };
    EffectiveSsh {
        strict,
        files,
        write_file: user_files.into_iter().next(),
        spec,
        hostname,
        port,
        user,
        proxy_jump,
        proxy_command,
    }
}

/// 调 `ssh -G` 取生效配置；失败时按 Profile 字段保守回退（strict=ask）。
pub fn effective_ssh(profile: &Profile) -> EffectiveSsh {
    let (prog, args) = profile.ssh_command();
    let mut gargs = vec!["-G".to_string()];
    gargs.extend(args);
    if let Ok(out) = Command::new(&prog).args(&gargs).output() {
        if out.status.success() {
            return parse_ssh_g(&String::from_utf8_lossy(&out.stdout), profile);
        }
    }
    parse_ssh_g("", profile)
}

/// known_hosts 查找键：默认 22 端口不带后缀，其它端口用 `[host]:port`。
pub fn host_spec(host: &str, port: u16) -> String {
    if port == 22 || port == 0 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
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

/// 本地是否已有该主机条目（不联网，供 askpass/启动路径快速判断）。
///
/// `StrictHostKeyChecking=no/off/accept-new` 时 ssh 自己不会弹主机密钥确认，
/// 因此也返回 true（与旧启动路径行为一致；用户显式配置优先）。
pub fn has_known_entry(profile: &Profile) -> bool {
    let effective = effective_ssh(profile);
    if strict_skips_prompt(&effective.strict) {
        return true;
    }
    if local_known_entries(profile).map(|keys| !keys.is_empty()).unwrap_or(false) {
        return true;
    }
    eprintln!(
        "[known_hosts] 未命中：lookup={} strict={} files={:?}",
        effective.spec, effective.strict, effective.files
    );
    false
}

/// 用 `ssh-keygen -F` 读 known_hosts 里该主机的条目，并补指纹。
///
/// 只读本地文件，不联网；文件不存在/无法解析时返回空列表（保守）。
pub fn local_known_entries(profile: &Profile) -> Result<Vec<KnownKey>, String> {
    let effective = effective_ssh(profile);
    let mut entries = Vec::new();
    for file in &effective.files {
        if !file.is_file() {
            continue;
        }
        let out = Command::new("ssh-keygen")
            .arg("-F")
            .arg(&effective.spec)
            .arg("-f")
            .arg(file)
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        entries.extend(parse_known_hosts_lines(&String::from_utf8_lossy(&out.stdout)));
    }
    entries.sort_by(|a, b| (&a.key_type, &a.key).cmp(&(&b.key_type, &b.key)));
    entries.dedup_by(|a, b| a.key_type == b.key_type && a.key == b.key);
    attach_fingerprints(&mut entries);
    Ok(entries)
}

/// 解析 known_hosts 行；跳过注释、空行与 `@cert-authority`/`@revoked` 标记行。
pub fn parse_known_hosts_lines(text: &str) -> Vec<KnownKey> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
                return None;
            }
            let mut parts = line.split_whitespace();
            let _host = parts.next()?;
            let key_type = parts.next()?;
            let key = parts.next()?;
            if key_type.is_empty() || key.is_empty() {
                return None;
            }
            Some(KnownKey {
                key_type: key_type.to_string(),
                key: key.to_string(),
                fingerprint: KnownKey::fallback_fingerprint(key_type, key),
            })
        })
        .collect()
}

/// 读取 known_hosts 里的 `@revoked` 公钥（忽略 host 模式，保守地全局撤销）。
///
/// OpenSSH 的 revocation 是“某 host 的某 key 被撤销”；native SSH 不做完整
/// host-pattern 匹配，因此只要 key 出现在 @revoked 行就拒绝，宁可回退系统 ssh。
pub fn revoked_keys(profile: &Profile) -> Vec<KnownKey> {
    let effective = effective_ssh(profile);
    let mut out = Vec::new();
    for file in &effective.files {
        let Ok(text) = std::fs::read_to_string(file) else { continue };
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("@revoked ") else { continue };
            let mut parts = rest.split_whitespace();
            let _host = parts.next();
            let (Some(key_type), Some(key)) = (parts.next(), parts.next()) else { continue };
            out.push(KnownKey {
                key_type: key_type.to_string(),
                key: key.to_string(),
                fingerprint: KnownKey::fallback_fingerprint(key_type, key),
            });
        }
    }
    out
}

/// 解析 `ssh-keyscan` 输出（主机名同样在首列，注释可选）。
pub fn parse_keyscan_lines(text: &str) -> Vec<KnownKey> {
    let mut keys = parse_known_hosts_lines(text);
    keys.sort_by(|a, b| (&a.key_type, &a.key).cmp(&(&b.key_type, &b.key)));
    keys.dedup_by(|a, b| a.key_type == b.key_type && a.key == b.key);
    keys
}

/// 按 key 类型比较：某个类型旧/新都存在且不同 → Changed。
///
/// 只比较双方都有的类型：服务器不再提供旧类型不算变更；新增类型也不算。
pub fn classify(known: &[KnownKey], current: &[KnownKey]) -> Verdict {
    if known.is_empty() {
        if current.is_empty() {
            return Verdict::Unverifiable {
                reason: "没有读取到服务器公钥".into(),
                local: false,
            };
        }
        return Verdict::Unknown { current: current.to_vec() };
    }
    if current.is_empty() {
        return Verdict::Unverifiable { reason: "没有读取到服务器公钥".into(), local: true };
    }
    let mut changed = false;
    let mut common = 0usize;
    for now in current {
        if let Some(before) = known.iter().find(|old| old.key_type == now.key_type) {
            common += 1;
            if before.key != now.key {
                changed = true;
                break;
            }
        }
    }
    if changed {
        Verdict::Changed { old: known.to_vec(), new: current.to_vec() }
    } else if common == 0 {
        // 本地只有 RSA、服务器只剩 ed25519 之类：不能拿旧条目“确认”新 key，
        // 交给用户/终端决定，避免静默接受一个从未记录过的 key 类型。
        Verdict::Unverifiable {
            reason: "known_hosts 与服务器没有共同的公钥类型，无法比对".into(),
            local: true,
        }
    } else {
        Verdict::Known
    }
}

/// `ssh-keyscan` 取服务器当前公钥；失败返回可读原因（不含秘密）。
pub fn fetch_current_keys(profile: &Profile) -> Result<Vec<KnownKey>, String> {
    fetch_current_keys_with(profile, Path::new("ssh-keyscan"), HOST_KEY_TIMEOUT, None)
}

fn fetch_current_keys_with(
    profile: &Profile,
    program: &Path,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<KnownKey>, String> {
    let host = profile.host.trim();
    if host.is_empty() {
        return Err("主机为空".into());
    }
    let mut command = Command::new(program);
    command
        .arg("-T")
        .arg(KEYSCAN_CONNECT_TIMEOUT)
        .arg("-p")
        .arg(profile.port.max(1).to_string())
        .arg(host);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = run_capture(command, timeout, cancel)?;
    let mut keys = parse_keyscan_lines(&out);
    attach_fingerprints(&mut keys);
    if keys.is_empty() {
        return Err(format!("ssh-keyscan 没有返回 {host} 的公钥"));
    }
    Ok(keys)
}

/// 完整预检：本地条目 + 网络公钥 → 结论。网络失败/跳板机 → Unverifiable。
pub fn assess(profile: &Profile) -> Verdict {
    assess_with(profile, Path::new("ssh-keyscan"), HOST_KEY_TIMEOUT, None)
}

fn assess_with(
    profile: &Profile,
    keyscan_program: &Path,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Verdict {
    let effective = effective_ssh(profile);
    if strict_skips_prompt(&effective.strict) {
        return Verdict::Known;
    }
    let known = match local_known_entries(profile) {
        Ok(keys) => keys,
        Err(e) => {
            return Verdict::Unverifiable { reason: format!("读取 known_hosts 失败: {e}"), local: true };
        },
    };
    if !effective.proxy_jump.is_empty() {
        return Verdict::Unverifiable {
            reason: format!("配置了 ProxyJump（{}），ssh-keyscan 无法穿透跳板机", effective.proxy_jump),
            local: !known.is_empty(),
        };
    }
    let current = match fetch_current_keys_with(profile, keyscan_program, timeout, cancel) {
        Ok(keys) => keys,
        Err(e) => return Verdict::Unverifiable { reason: e, local: !known.is_empty() },
    };
    classify(&known, &current)
}

/// 把服务器公钥写入 known_hosts（变更场景会先删除该主机旧条目）。
///
/// 只应在用户明确点「信任/更新」后调用；写入文件权限 0600、目录 0700，原子追加。
pub fn trust_keys(profile: &Profile, keys: &[KnownKey]) -> Result<(), String> {
    if keys.is_empty() {
        return Err("没有可写入的主机公钥".into());
    }
    let effective = effective_ssh(profile);
    let file = effective
        .write_file
        .clone()
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ssh/known_hosts")))
        .ok_or_else(|| "无法确定 known_hosts 路径".to_string())?;
    let parent = file.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("创建 {} 失败: {e}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    // 先删除旧条目（ssh-keygen -R 处理明文/哈希条目）；失败不致命（文件可能不存在）。
    let _ = Command::new("ssh-keygen")
        .arg("-R")
        .arg(&effective.spec)
        .arg("-f")
        .arg(&file)
        .output();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut out =
        opts.open(&file).map_err(|e| format!("打开 {} 失败: {e}", file.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置 {} 权限失败: {e}", file.display()))?;
    }
    let mut body = String::new();
    for key in keys {
        body.push_str(&format!("{} {} {}\n", effective.spec, key.key_type, key.key));
    }
    out.write_all(body.as_bytes())
        .map_err(|e| format!("写入 {} 失败: {e}", file.display()))?;
    out.sync_all().map_err(|e| format!("同步 {} 失败: {e}", file.display()))?;
    eprintln!(
        "[known_hosts] 已写入 {} 条主机公钥 host={} file={}",
        keys.len(),
        effective.spec,
        file.display()
    );
    Ok(())
}

/// 后台预检任务：启动后立即返回；结果通过非阻塞 `try_recv` 交给 UI。
pub struct HostKeyJob {
    events: Receiver<Verdict>,
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl HostKeyJob {
    /// 生产入口：结果写通道后唤醒 UI。
    pub fn start(profile: Profile, wake: WakeSignal) -> Self {
        Self::start_inner(profile, Path::new("ssh-keyscan").to_path_buf(), HOST_KEY_TIMEOUT, Some(wake))
    }

    /// 测试入口：注入假 keyscan / 短超时。
    #[cfg(test)]
    pub fn start_with(profile: Profile, program: PathBuf, timeout: Duration) -> Self {
        Self::start_inner(profile, program, timeout, None)
    }

    fn start_inner(
        profile: Profile,
        program: PathBuf,
        timeout: Duration,
        wake: Option<WakeSignal>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, events) = mpsc::channel();
        let worker_cancel = cancel.clone();
        let join = thread::spawn(move || {
            let verdict = assess_with(&profile, &program, timeout, Some(&worker_cancel));
            if !worker_cancel.load(Ordering::SeqCst) {
                if sender.send(verdict).is_ok() {
                    if let Some(wake) = &wake {
                        wake.notify();
                    }
                }
            }
        });
        Self { events, cancel, join: Some(join) }
    }

    pub fn try_recv(&self) -> Result<Verdict, mpsc::TryRecvError> {
        self.events.try_recv()
    }

    /// 取消预检：标记取消并 kill 子进程，最多等 `KILL_GRACE×2`；不阻塞 UI。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub fn wait(mut self) -> Option<Verdict> {
        let result = self.events.recv_timeout(HOST_KEY_TIMEOUT + KILL_GRACE).ok();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        result
    }
}

impl Drop for HostKeyJob {
    fn drop(&mut self) {
        // 不 join：UI 丢弃时后台最多还有一个 10s 超时；线程自行结束。
        self.cancel.store(true, Ordering::SeqCst);
        self.join.take();
    }
}

// ------------------------------------------------------------ 子进程执行

/// 执行一个短命令，带硬超时 / 取消 / 进程组 kill；只返回 stdout 文本。
///
/// 与 SFTP 的 batch 执行器独立（那里要喂 stdin、保留退出码语义），这里只服务
/// 于 `ssh -G`/`ssh-keygen` 已同步完成、真正需要超时的 `ssh-keyscan`。
fn run_capture(
    mut command: Command,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("启动 ssh-keyscan 失败: {e}"))?;

    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        readers.push(thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.take(1 << 20).read_to_end(&mut buf);
            let _ = out_tx.send(buf);
        }));
    }
    if let Some(mut stderr) = child.stderr.take() {
        readers.push(thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.take(1 << 20).read_to_end(&mut buf);
            let _ = err_tx.send(buf);
        }));
    }

    let deadline = Instant::now() + timeout;
    let mut cancelled = false;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {},
            Err(_) => break None,
        }
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            cancelled = true;
            kill_and_reap(&mut child);
            break None;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_and_reap(&mut child);
            break None;
        }
        thread::sleep(WAIT_POLL);
    };
    let grace = KILL_GRACE * 4;
    let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
    for reader in readers {
        if reader.is_finished() {
            let _ = reader.join();
        }
    }
    if cancelled {
        return Err("主机密钥预检已取消".into());
    }
    if timed_out {
        return Err(format!("ssh-keyscan 超时（{}s）", timeout.as_secs_f32()));
    }
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
    match status {
        Some(status) if status.success() => Ok(stdout),
        _ => Err(if stderr.is_empty() {
            "ssh-keyscan 未返回公钥（主机不可达或端口错误）".into()
        } else {
            format!("ssh-keyscan 失败: {stderr}")
        }),
    }
}

fn kill_child_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // SAFETY: process_group(0) 使 pgid == pid；负 pid 表示整组。ESRCH 忽略。
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// kill 后最多等 1s 回收，避免 D-state 把后台线程永远挂住。
fn kill_and_reap(child: &mut Child) {
    kill_child_tree(child);
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// 给一组公钥补 `SHA256:` 指纹（`ssh-keygen -lf`）；失败保留 fallback。
fn attach_fingerprints(keys: &mut [KnownKey]) {
    if keys.is_empty() {
        return;
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut body = String::new();
    for key in keys.iter() {
        body.push_str(&format!("host {} {}\n", key.key_type, key.key));
    }
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "tervo-kh-{}-{unique}.txt",
        std::process::id()
    ));
    if std::fs::write(&path, body.as_bytes()).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    if let Ok(out) = Command::new("ssh-keygen").arg("-lf").arg(&path).output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for (key, line) in keys.iter_mut().zip(text.lines()) {
                if let Some(fp) = line.split_whitespace().nth(1) {
                    if fp.starts_with("SHA256:") || fp.starts_with("MD5:") {
                        key.fingerprint = fp.to_string();
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用公钥（不是真实主机的密钥）。
    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKNQBVooZwwKkPSd6IMylF98NDRrI/y3pjbO8e0z53Vy";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const KEY_RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDexampleexampleexample";

    fn key(entry: &str) -> KnownKey {
        parse_known_hosts_lines(&format!("host {entry}"))
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn classify_first_connect_and_unchanged() {
        let current = vec![key(KEY_A)];
        assert_eq!(
            classify(&[], &current),
            Verdict::Unknown { current: current.clone() }
        );
        let known = vec![key(KEY_A)];
        assert_eq!(classify(&known, &current), Verdict::Known);
        assert!(!Verdict::Known.needs_prompt());
        assert!(Verdict::Unknown { current }.needs_prompt());
    }

    #[test]
    fn classify_detects_changed_key_by_type() {
        let known = vec![key(KEY_A)];
        let current = vec![key(KEY_B)];
        let verdict = classify(&known, &current);
        match &verdict {
            Verdict::Changed { old, new } => {
                assert_eq!(old[0].key, key(KEY_A).key);
                assert_eq!(new[0].key, key(KEY_B).key);
            },
            other => panic!("expected Changed, got {other:?}"),
        }
        assert!(verdict.detail().contains("不一致"));
        assert!(verdict.detail().contains("旧"));
        assert!(verdict.detail().contains("新"));
    }

    #[test]
    fn classify_ignores_new_or_missing_key_types() {
        // 服务器换了算法/新增类型，但共有的 ed25519 没变：不算 changed。
        let known = vec![key(KEY_A), key(KEY_RSA)];
        let current = vec![key(KEY_A)];
        assert_eq!(classify(&known, &current), Verdict::Known);
        // 完全没有共同 key 类型时无法确认，必须交给用户/终端，不能静默放行。
        let known = vec![key(KEY_RSA)];
        let current = vec![key(KEY_A)];
        assert!(matches!(
            classify(&known, &current),
            Verdict::Unverifiable { local: true, .. }
        ));
    }

    #[test]
    fn classify_empty_current_is_unverifiable() {
        let known = vec![key(KEY_A)];
        assert!(matches!(
            classify(&known, &[]),
            Verdict::Unverifiable { local: true, .. }
        ));
        assert!(matches!(
            classify(&[], &[]),
            Verdict::Unverifiable { local: false, .. }
        ));
    }

    #[test]
    fn parser_skips_markers_comments_and_hashed_lines() {
        let text = "\
# comment
@cert-authority *.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIcert
|1|abc=|def= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIhashed
example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIok
";
        let keys = parse_known_hosts_lines(text);
        // 哈希行虽然首个字段不是主机名，但结构仍是 host type key，保留给比较用。
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key_type, "ssh-ed25519");
        assert_eq!(keys[1].key, "AAAAC3NzaC1lZDI1NTE5AAAAIok");
    }

    #[test]
    fn keyscan_parser_dedups_repeated_hosts() {
        let text = format!(
            "# {}\n[example.com]:2222 {}\n[example.com]:2222 {}\n",
            "example.com:22 SSH-2.0-OpenSSH_9.6",
            KEY_A,
            KEY_A
        );
        let keys = parse_keyscan_lines(&text);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key_type, "ssh-ed25519");
    }

    #[test]
    fn ssh_g_parser_uses_effective_hostname_port_and_files() {
        let profile = Profile {
            kind: "ssh".into(),
            host: "alias".into(),
            port: 22,
            user: "u".into(),
            ..Profile::default()
        };
        let output = "\
hostname 127.0.0.1
port 2222
userknownhostsfile /tmp/kh1 /tmp/kh2
globalknownhostsfile none
stricthostkeychecking ask
hostkeyalias myalias
proxyjump jump@bastion:2200
";
        let effective = parse_ssh_g(output, &profile);
        assert_eq!(effective.strict, "ask");
        assert_eq!(effective.spec, "myalias", "HostKeyAlias 原文作为键，不套端口");
        assert_eq!(effective.files, vec![PathBuf::from("/tmp/kh1"), PathBuf::from("/tmp/kh2")]);
        assert_eq!(effective.write_file.as_deref(), Some(Path::new("/tmp/kh1")));
        assert_eq!(effective.proxy_jump, "jump@bastion:2200");
        assert!(!strict_skips_prompt(&effective.strict));

        let no_alias = parse_ssh_g(
            "hostname 127.0.0.1\nport 2222\nuserknownhostsfile /tmp/kh\n",
            &profile,
        );
        assert_eq!(no_alias.spec, "[127.0.0.1]:2222");
        assert!(no_alias.proxy_jump.is_empty());
    }

    #[test]
    fn explicit_strict_no_skips_prompt() {
        for strict in ["no", "off", "accept-new"] {
            let effective = EffectiveSsh {
                strict: strict.into(),
                files: vec![PathBuf::from("/tmp/kh")],
                write_file: Some(PathBuf::from("/tmp/kh")),
                spec: "h".into(),
                hostname: "h".into(),
                port: 22,
                user: "u".into(),
                proxy_jump: String::new(),
                proxy_command: String::new(),
            };
            assert!(strict_skips_prompt(&effective.strict), "{strict}");
        }
        for strict in ["ask", "yes", "true"] {
            assert!(!strict_skips_prompt(strict), "{strict}");
        }
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

    // ---- 依赖系统 ssh/ssh-keygen 的集成测试（缺失时跳过） ----

    fn have_ssh_tools() -> bool {
        Command::new("ssh").arg("-V").output().is_ok()
            && Command::new("ssh-keygen").arg("-V").output().is_ok()
    }

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tervo-known-hosts-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn profile_with_known_hosts(host: &str, port: u16, kh: &Path) -> Profile {
        Profile {
            kind: "ssh".into(),
            host: host.into(),
            port,
            user: "u".into(),
            auth: "key".into(),
            key_path: "/dev/null".into(),
            // 隔离用户真实 ~/.ssh/config，避免 Host 匹配/ProxyJump 干扰。
            extra_args: vec![
                "-F".into(),
                "/dev/null".into(),
                "-o".into(),
                format!("UserKnownHostsFile={}", kh.display()),
                "-o".into(),
                "StrictHostKeyChecking=ask".into(),
            ],
            ..Profile::default()
        }
    }

    #[test]
    fn local_entries_find_plain_and_bracketed_port_keys() {
        if !have_ssh_tools() {
            return;
        }
        let dir = test_dir("entries");
        let kh = dir.join("known_hosts");
        std::fs::write(
            &kh,
            format!("example.com {KEY_A}\n[127.0.0.1]:2222 {KEY_B}\n"),
        )
        .unwrap();
        let profile = profile_with_known_hosts("example.com", 22, &kh);
        let entries = local_known_entries(&profile).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, key(KEY_A).key);
        assert!(has_known_entry(&profile));
        assert!(
            entries[0].fingerprint.starts_with("SHA256:") || entries[0].fingerprint.contains('…'),
            "fingerprint={:?}",
            entries[0].fingerprint
        );

        let p = profile_with_known_hosts("127.0.0.1", 2222, &kh);
        let entries = local_known_entries(&p).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, key(KEY_B).key);
        let wrong_port = profile_with_known_hosts("127.0.0.1", 2223, &kh);
        assert!(local_known_entries(&wrong_port).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trust_keys_writes_private_known_hosts_and_replaces_old() {
        if !have_ssh_tools() {
            return;
        }
        let dir = test_dir("trust");
        let kh = dir.join("known_hosts");
        std::fs::write(&kh, format!("example.com {KEY_A}\n")).unwrap();
        let profile = profile_with_known_hosts("example.com", 22, &kh);
        trust_keys(&profile, &[key(KEY_B)]).unwrap();
        let text = std::fs::read_to_string(&kh).unwrap();
        assert!(!text.contains(&key(KEY_A).key), "旧 key 必须被删除: {text:?}");
        assert!(text.contains(&key(KEY_B).key));
        assert!(text.lines().any(|l| l.starts_with("example.com ssh-ed25519 ")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&kh).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "known_hosts 权限必须 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn keyscan_timeout_and_cancel_do_not_block() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_dir("timeout");
        let script = dir.join("slow-keyscan.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profile = profile_with_known_hosts("example.com", 22, &dir.join("known_hosts"));
        let started = Instant::now();
        let err = fetch_current_keys_with(
            &profile,
            &script,
            Duration::from_millis(150),
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("超时") || err.contains("失败") || err.contains("没有返回"),
            "{err}"
        );
        assert!(started.elapsed() < Duration::from_secs(5), "超时必须及时返回");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn fake_keyscan_returns_unknown_and_changed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_dir("fake-scan");
        let kh = dir.join("known_hosts");
        let script = dir.join("scan.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' 'example.com {KEY_B}'\nsleep 0.01\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&kh, format!("example.com {KEY_A}\n")).unwrap();
        let profile = profile_with_known_hosts("example.com", 22, &kh);
        let verdict = assess_with(&profile, &script, Duration::from_secs(2), None);
        assert!(matches!(verdict, Verdict::Changed { .. }), "{verdict:?}");

        std::fs::write(&kh, "").unwrap();
        let verdict = assess_with(&profile, &script, Duration::from_secs(2), None);
        assert!(matches!(verdict, Verdict::Unknown { .. }), "{verdict:?}");

        // 一致时 Known。
        std::fs::write(&kh, format!("example.com {KEY_B}\n")).unwrap();
        let verdict = assess_with(&profile, &script, Duration::from_secs(2), None);
        assert_eq!(verdict, Verdict::Known);

        // 后台 job 能收到结果。
        let job = HostKeyJob::start_with(profile, script, Duration::from_secs(2));
        let verdict = job.wait().expect("job 应返回结论");
        assert_eq!(verdict, Verdict::Known);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assess_skips_network_when_strict_is_disabled() {
        // StrictHostKeyChecking=no：用户显式接管，不跑 keyscan（即使 keyscan 不存在）。
        let profile = Profile {
            kind: "ssh".into(),
            host: "example.invalid".into(),
            port: 22,
            user: "u".into(),
            auth: "key".into(),
            key_path: "/dev/null".into(),
            extra_args: vec![
                "-o".into(),
                "UserKnownHostsFile=/tmp/nonexistent-tervo-kh".into(),
                "-o".into(),
                "StrictHostKeyChecking=no".into(),
            ],
            ..Profile::default()
        };
        let started = Instant::now();
        assert_eq!(
            assess_with(&profile, Path::new("/nonexistent/keyscan"), Duration::from_secs(1), None),
            Verdict::Known
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
