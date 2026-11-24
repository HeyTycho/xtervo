// SPDX-License-Identifier: GPL-3.0-only
//! Vault：加密凭据存储（MVP）。
//!
//! 文件默认 `~/.config/xtervo/vault.enc`（`XTERVO_VAULT` 可覆盖）。格式是纯文本 +
//! base64 字段，便于排查损坏（不含明文秘密）：
//!
//! ```text
//! XTERVO-VAULT 1
//! kdf=argon2id
//! m=19456
//! t=2
//! p=1
//! salt=<base64 16B>
//! nonce=<base64 12B>
//! ciphertext=<base64 ChaCha20-Poly1305 密文+tag>
//! ```
//!
//! - key = Argon2id(主密码, salt, 32B)；
//! - 密文 = ChaCha20-Poly1305(key, nonce, JSON `{profile_id: password}`)，
//!   AAD 是 `ciphertext=` 之前的完整头部（magic/version/KDF 参数/salt/nonce 都被认证）；
//! - 主密码与明文密码**不落盘、不写日志**；文件权限 0600，唯一临时文件 +
//!   `O_EXCL|O_NOFOLLOW` + `rename` + 父目录 fsync；`<vault>.lock` 上的 `flock`
//!   串行化跨进程 read-modify-write，解锁/写入时把过宽权限收紧到 0600。
//!
//! 安全限制（务必与 `docs/VAULT.md` 保持一致）：
//! - 解锁后密码在进程内存中；本 MVP 只做 best-effort 清零（key、明文 JSON、
//!   被覆盖/删除的条目），未做 `mlock`/完整 zeroize 级别防护。
//! - 未接 Linux Secret Service/Keyring；加密文件是唯一后端。
//! - 错密码与密文损坏在 AEAD 层不可区分，统一返回「密码错误或文件损坏」。
//! - 与 Vault 配套的 SSH askpass 仍通过子进程环境变量传密码，同用户可经
//!   `/proc/<pid>/environ` 观察；后续应改为继承 fd/pipe（见 `askpass.rs`）。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use xtervo_core::WakeSignal;

/// 文件首行的 magic（与版本分两段：`XTERVO-VAULT <version>`）。
pub const MAGIC: &str = "XTERVO-VAULT";
/// 当前 vault 文件版本。
pub const VERSION: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
/// Argon2id 参数（OWASP 推荐档：19 MiB / t=2 / p=1）。
const KDF_M_COST: u32 = 19_456;
const KDF_T_COST: u32 = 2;
const KDF_P_COST: u32 = 1;
/// 头部允许的 KDF 参数硬上限：恶意文件不能要求超大内存/CPU。
/// 当前写入档位 19456/2/1 远低于上限，保留余量但不接受离谱参数。
const KDF_M_MAX: u32 = 65_536;
const KDF_T_MAX: u32 = 4;
const KDF_P_MAX: u32 = 4;
/// 防止被塞入超大文件/超大字段拖死 UI。
const MAX_FILE: u64 = 4 * 1024 * 1024;
const MAX_SECRET_LEN: usize = 4096;
/// 主密码最短长度（只约束新建/修改，解锁不限制，避免锁死旧文件）。
const MIN_MASTER_LEN: usize = 8;

/// Vault 对外状态（UI 用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultStatus {
    /// 文件不存在：还没设置主密码。
    Uninitialized,
    /// 文件存在但主密码未在本次进程内提供。
    Locked,
    /// 本次进程内已解密。
    Unlocked,
}

impl VaultStatus {
    pub fn label(self) -> &'static str {
        match self {
            VaultStatus::Uninitialized => "未初始化",
            VaultStatus::Locked => "已锁定",
            VaultStatus::Unlocked => "已解锁",
        }
    }
}

/// Vault 错误。`Display` 里绝不包含主密码/凭据内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// 未解锁（或未初始化）时尝试读写凭据。
    Locked,
    /// 文件不存在。
    NotInitialized,
    /// 文件已存在，拒绝覆盖。
    AlreadyInitialized,
    /// 主密码错误 **或** 密文被篡改/损坏（AEAD 无法区分）。
    WrongPassword,
    /// 文件结构/版本/KDF 参数不合法。
    Corrupt(String),
    /// 文件已被另一个进程改写（主密码或凭据已变化），当前内存会话过期。
    Stale,
    /// 输入不合法（空密码、超长等）。
    Invalid(String),
    /// 指定 Profile 没有保存密码。
    NotFound(u64),
    /// 文件 I/O 失败。
    Io(String),
    /// 序列化/加密等内部错误。
    Internal(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::Locked => write!(f, "Vault 已锁定，请先解锁"),
            VaultError::NotInitialized => write!(f, "Vault 尚未初始化，请先设置主密码"),
            VaultError::AlreadyInitialized => write!(f, "Vault 已存在，不能重复初始化"),
            VaultError::WrongPassword => write!(f, "主密码错误或 vault 文件已损坏"),
            VaultError::Corrupt(m) => write!(f, "vault 文件损坏: {m}"),
            VaultError::Stale => write!(f, "Vault 已被其他进程修改，请重新解锁"),
            VaultError::Invalid(m) => write!(f, "{m}"),
            VaultError::NotFound(id) => write!(f, "Vault 中没有 Profile {id} 的密码"),
            VaultError::Io(m) => write!(f, "vault 文件读写失败: {m}"),
            VaultError::Internal(m) => write!(f, "vault 内部错误: {m}"),
        }
    }
}

impl std::error::Error for VaultError {}

impl From<std::io::Error> for VaultError {
    fn from(e: std::io::Error) -> Self {
        VaultError::Io(e.to_string())
    }
}

/// 解密后的内存状态。`entries` 是 profile_id → password。
struct Unlocked {
    key: [u8; KEY_LEN],
    salt: [u8; SALT_LEN],
    entries: BTreeMap<u64, String>,
}

impl Unlocked {
    /// 尽力覆盖 key 与密码字符串（Rust 分配器可能仍留旧副本，见模块文档）。
    /// Drop 与显式清理（clear/remove/覆盖）共用同一条擦除路径。
    fn wipe(&mut self) {
        self.key.fill(0);
        wipe_entries(&mut self.entries);
    }
}

impl Drop for Unlocked {
    fn drop(&mut self) {
        self.wipe();
    }
}

enum State {
    Uninitialized,
    Locked,
    Unlocked(Unlocked),
}

/// 后台 KDF 成功后移交回 UI 的会话。只在本进程内传递，**不经过任何
/// 序列化/日志/UI 属性**；`Display/Debug` 故意不实现。
pub struct VaultSession(Unlocked);

impl VaultSession {
    /// 凭据条数（只读；给测试/日志用，不暴露密码）。
    pub fn entry_count(&self) -> usize {
        self.0.entries.len()
    }
}

/// 改主密码需要的当前会话快照。只允许在解锁状态下取；交给后台线程后
/// 旧 key/凭据副本在工作线程结束时由 `Unlocked::drop` 清零。
pub struct VaultChange(Unlocked);

/// 后台任务结果。错误里绝不包含主密码/凭据内容。
pub enum VaultTaskOutcome {
    Ok(VaultSession),
    Err(VaultError),
}

/// 一次 Vault 后台任务（解锁 / 初始化 / 改主密码）。
///
/// Argon2id 是刻意慢的 KDF：如果在 Slint 回调里同步跑，设置页会卡住不能重绘。
/// 这里把派生+解密/加密全部放到独立线程，UI 只轮询结果。
pub struct VaultTask {
    events: Receiver<VaultTaskOutcome>,
    join: Option<JoinHandle<()>>,
}

impl VaultTask {
    /// 解锁：读文件 + Argon2id + AEAD 解密。
    pub fn unlock(path: PathBuf, master: String, wake: WakeSignal) -> Self {
        Self::start(wake, move || {
            let mut master = master;
            let result = prepare_unlock(&path, &master);
            zeroize_string(&mut master);
            result
        })
    }

    /// 初始化：校验主密码 + Argon2id + 写空 vault。
    pub fn initialize(path: PathBuf, master: String, wake: WakeSignal) -> Self {
        Self::start(wake, move || {
            let mut master = master;
            let result = prepare_initialize(&path, &master);
            zeroize_string(&mut master);
            result
        })
    }

    /// 改主密码：用当前 key/entries 重新加密全部凭据。
    pub fn change_master(
        path: PathBuf,
        current: VaultChange,
        new_master: String,
        wake: WakeSignal,
    ) -> Self {
        Self::start(wake, move || {
            let mut new_master = new_master;
            let result = prepare_change(&path, current, &new_master);
            zeroize_string(&mut new_master);
            result
        })
    }

    fn start(wake: WakeSignal, work: impl FnOnce() -> Result<Unlocked, VaultError> + Send + 'static) -> Self {
        let (sender, events) = mpsc::channel();
        let join = thread::Builder::new()
            .name("vault-kdf".into())
            .spawn(move || {
                maybe_test_kdf_delay();
                let outcome = match work() {
                    Ok(unlocked) => VaultTaskOutcome::Ok(VaultSession(unlocked)),
                    Err(e) => VaultTaskOutcome::Err(e),
                };
                // 先入队再唤醒：UI 被叫醒后 try_recv 一定能拿到结果。
                if sender.send(outcome).is_ok() {
                    wake.notify();
                }
            });
        match join {
            Ok(join) => Self { events, join: Some(join) },
            Err(e) => {
                // 线程都起不来时给一个立即失败的结果，UI 状态机不会永远 busy。
                let (sender, events) = mpsc::channel();
                let _ = sender.send(VaultTaskOutcome::Err(VaultError::Internal(format!(
                    "启动 vault KDF 线程失败: {e}"
                ))));
                Self { events, join: None }
            },
        }
    }

    /// UI 线程非阻塞取结果；`Disconnected` 表示线程异常结束。
    pub fn try_recv(&self) -> Result<VaultTaskOutcome, mpsc::TryRecvError> {
        self.events.try_recv()
    }

    /// 测试/探针：阻塞等结果并回收线程；UI 不使用（会卡事件循环）。
    #[cfg(test)]
    pub fn wait(mut self) -> Option<VaultTaskOutcome> {
        let result = self.events.recv_timeout(Duration::from_secs(60)).ok();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        result
    }
}

impl Drop for VaultTask {
    fn drop(&mut self) {
        // 不 join：Argon2 不能被强杀，线程最多再跑完一次派生；结果通道断开后
        // 线程自然结束。UI 丢弃时不会阻塞。
        self.join.take();
    }
}

/// 仅用于 GUI e2e 截图：让 Argon2 派生前多停留一会儿，使「正在解锁」状态
/// 可被截到。默认 0；只从进程环境读一次，不写配置、不影响生产行为。
fn maybe_test_kdf_delay() {
    static DELAY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let ms = *DELAY.get_or_init(|| {
        std::env::var("XTERVO_VAULT_KDF_DELAY_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
            .min(10_000)
    });
    if ms > 0 {
        thread::sleep(Duration::from_millis(ms));
    }
}

/// 加密凭据容器。一个进程一个实例；`lock()` 后内存中的 key/密码一并丢弃。
pub struct Vault {
    path: PathBuf,
    state: State,
}

impl Vault {
    /// 默认路径：`$XDG_CONFIG_HOME/xtervo/vault.enc` 或 `~/.config/xtervo/vault.enc`；
    /// `XTERVO_VAULT` 优先（测试/多实例）。
    pub fn default_path() -> PathBuf {
        default_path_from(
            std::env::var("XTERVO_VAULT").ok().as_deref(),
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    pub fn open_default() -> Self {
        Self::new(Self::default_path())
    }

    /// 用指定路径打开（测试用）。只判断文件是否存在，不读取内容。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = if path.exists() { State::Locked } else { State::Uninitialized };
        Self { path, state }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn status(&self) -> VaultStatus {
        match self.state {
            State::Uninitialized => VaultStatus::Uninitialized,
            State::Locked => VaultStatus::Locked,
            State::Unlocked(_) => VaultStatus::Unlocked,
        }
    }

    pub fn is_unlocked(&self) -> bool {
        matches!(self.state, State::Unlocked(_))
    }

    /// 已解锁时的凭据条数；锁定/未初始化返回 0（不猜测、不解密）。
    pub fn entry_count(&self) -> usize {
        match &self.state {
            State::Unlocked(u) => u.entries.len(),
            _ => 0,
        }
    }

    /// 创建空 vault 并解锁。已存在文件时拒绝（绝不覆盖旧凭据）。
    ///
    /// UI 请用 [`VaultTask::initialize`]（Argon2 不阻塞事件循环）；保留同步版本
    /// 给探针/单测使用。
    pub fn initialize(&mut self, master: &str) -> Result<(), VaultError> {
        let unlocked = prepare_initialize(&self.path, master)?;
        self.state = State::Unlocked(unlocked);
        Ok(())
    }

    /// 解锁：读取文件 → 校验结构 → Argon2id 派生 → AEAD 解密。
    /// 失败时保持原状态（不会把半解密数据留在内存里）。
    ///
    /// UI 请用 [`VaultTask::unlock`]；保留同步版本给探针/单测使用。
    pub fn unlock(&mut self, master: &str) -> Result<(), VaultError> {
        match prepare_unlock(&self.path, master) {
            Ok(unlocked) => {
                self.state = State::Unlocked(unlocked);
                Ok(())
            },
            Err(VaultError::NotInitialized) => {
                self.state = State::Uninitialized;
                Err(VaultError::NotInitialized)
            },
            Err(e) => Err(e),
        }
    }

    /// 文件被外部删除/重建后同步状态（后台任务返回 NotInitialized 时用）。
    pub fn refresh_status(&mut self) {
        if matches!(self.state, State::Unlocked(_)) {
            return;
        }
        self.state = if self.path.exists() { State::Locked } else { State::Uninitialized };
    }

    /// 把后台任务成功解锁的会话装回 UI 状态（不重新派生、不落盘）。
    pub fn adopt(&mut self, session: VaultSession) {
        self.state = State::Unlocked(session.0);
    }

    /// 清空内存状态；之后 get/set/remove 都返回 `Locked`。
    /// 未初始化状态保持不变（不能因为点 Lock 就假装文件存在）。
    pub fn lock(&mut self) {
        if !matches!(self.state, State::Unlocked(_)) {
            return;
        }
        // 换出 State 后由 `Unlocked::drop` 清零 key 与密码。
        self.state = State::Locked;
    }

    /// 取一份当前会话快照供后台改密码线程使用；锁定/未初始化返回错误。
    pub fn change_snapshot(&self) -> Result<VaultChange, VaultError> {
        match &self.state {
            State::Unlocked(cur) => Ok(VaultChange(Unlocked {
                key: cur.key,
                salt: cur.salt,
                entries: cur.entries.clone(),
            })),
            State::Uninitialized => Err(VaultError::NotInitialized),
            State::Locked => Err(VaultError::Locked),
        }
    }

    /// 修改主密码：必须已解锁；用新 salt/key 重新加密全部凭据。
    ///
    /// UI 请用 [`VaultTask::change_master`]；保留同步版本给探针/单测使用。
    pub fn change_master(&mut self, new_master: &str) -> Result<(), VaultError> {
        let snapshot = self.change_snapshot()?;
        let next = prepare_change(&self.path, snapshot, new_master)?;
        self.state = State::Unlocked(next);
        Ok(())
    }

    /// 写入/覆盖一个 Profile 的密码并持久化。失败时回滚内存值。
    pub fn set(&mut self, profile_id: u64, password: &str) -> Result<(), VaultError> {
        let State::Unlocked(cur) = &mut self.state else {
            return Err(VaultError::Locked);
        };
        validate_secret(password)?;
        // 跨进程串行化：锁内重读磁盘的最新状态再合并，不能拿旧快照直接覆盖。
        let _lock = VaultLock::acquire(&self.path)?;
        let mut merged = match read_disk_entries(&self.path, &cur.key, &cur.salt)? {
            DiskState::Missing => cur.entries.clone(),
            DiskState::Stale => return Err(VaultError::Stale),
            DiskState::Current(entries) => entries,
        };
        let displaced = merged.insert(profile_id, password.to_string());
        match write_store_locked(&self.path, &cur.key, &cur.salt, &merged) {
            Ok(()) => {
                // 内存切到磁盘合并结果；被替换的旧副本全部擦除。
                let mut old_memory = std::mem::replace(&mut cur.entries, merged);
                wipe_entries(&mut old_memory);
                if let Some(mut old) = displaced {
                    wipe_string(&mut old);
                }
                Ok(())
            },
            Err(e) => {
                // 写失败：磁盘未变，内存保持原样；临时合并副本必须擦除。
                wipe_entries(&mut merged);
                if let Some(mut old) = displaced {
                    wipe_string(&mut old);
                }
                Err(e)
            },
        }
    }

    /// 加密私钥 passphrase 使用独立 slot，避免和登录密码互相覆盖。
    const KEY_PASSPHRASE_SLOT: u64 = 1 << 63;

    pub fn set_key_passphrase(&mut self, profile_id: u64, passphrase: &str) -> Result<(), VaultError> {
        self.set(profile_id ^ Self::KEY_PASSPHRASE_SLOT, passphrase)
    }

    pub fn get_key_passphrase(&self, profile_id: u64) -> Result<String, VaultError> {
        self.get(profile_id ^ Self::KEY_PASSPHRASE_SLOT)
    }

    pub fn has_key_passphrase(&self, profile_id: u64) -> Result<bool, VaultError> {
        self.contains(profile_id ^ Self::KEY_PASSPHRASE_SLOT)
    }

    pub fn remove_key_passphrase(&mut self, profile_id: u64) -> Result<bool, VaultError> {
        self.remove(profile_id ^ Self::KEY_PASSPHRASE_SLOT)
    }

    /// 读取密码；锁定/未初始化返回错误。
    pub fn get(&self, profile_id: u64) -> Result<String, VaultError> {
        match &self.state {
            State::Unlocked(u) => {
                u.entries.get(&profile_id).cloned().ok_or(VaultError::NotFound(profile_id))
            },
            State::Uninitialized => Err(VaultError::NotInitialized),
            State::Locked => Err(VaultError::Locked),
        }
    }

    /// 是否保存了该 Profile 的密码（锁定/未初始化返回错误）。
    pub fn contains(&self, profile_id: u64) -> Result<bool, VaultError> {
        match &self.state {
            State::Unlocked(u) => Ok(u.entries.contains_key(&profile_id)),
            State::Uninitialized => Err(VaultError::NotInitialized),
            State::Locked => Err(VaultError::Locked),
        }
    }

    /// 删除一个 Profile 的密码并持久化；返回是否确实删除。
    pub fn remove(&mut self, profile_id: u64) -> Result<bool, VaultError> {
        let State::Unlocked(cur) = &mut self.state else {
            return Err(VaultError::Locked);
        };
        let _lock = VaultLock::acquire(&self.path)?;
        let mut merged = match read_disk_entries(&self.path, &cur.key, &cur.salt)? {
            DiskState::Missing => cur.entries.clone(),
            DiskState::Stale => return Err(VaultError::Stale),
            DiskState::Current(entries) => entries,
        };
        let Some(mut removed) = merged.remove(&profile_id) else {
            // 磁盘上已经不存在（可能被别的进程删了）：以磁盘为准同步内存即可。
            let mut old_memory = std::mem::replace(&mut cur.entries, merged);
            wipe_entries(&mut old_memory);
            return Ok(false);
        };
        match write_store_locked(&self.path, &cur.key, &cur.salt, &merged) {
            Ok(()) => {
                let mut old_memory = std::mem::replace(&mut cur.entries, merged);
                wipe_entries(&mut old_memory);
                wipe_string(&mut removed);
                Ok(true)
            },
            Err(e) => {
                wipe_entries(&mut merged);
                wipe_string(&mut removed);
                Err(e)
            },
        }
    }

    /// 清空全部凭据（保留文件与主密码），并持久化。
    pub fn clear(&mut self) -> Result<(), VaultError> {
        let State::Unlocked(cur) = &mut self.state else {
            return Err(VaultError::Locked);
        };
        let _lock = VaultLock::acquire(&self.path)?;
        // 锁内确认没有别的进程换过主密码；解出的条目只用于校验，立即擦除。
        match read_disk_entries(&self.path, &cur.key, &cur.salt)? {
            DiskState::Missing => {},
            DiskState::Stale => return Err(VaultError::Stale),
            DiskState::Current(mut entries) => wipe_entries(&mut entries),
        }
        let mut old = std::mem::take(&mut cur.entries);
        match write_store_locked(&self.path, &cur.key, &cur.salt, &cur.entries) {
            Ok(()) => {
                wipe_entries(&mut old);
                Ok(())
            },
            Err(e) => {
                cur.entries = old;
                Err(e)
            },
        }
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        // 退出/重建 AppState 时也清掉内存里的 key/密码。
        self.lock();
    }
}

/// 纯函数版路径解析（测试不需要改进程环境变量）。
fn default_path_from(xtervo_vault: Option<&str>, xdg: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(p) = xtervo_vault.filter(|p| !p.trim().is_empty()) {
        return PathBuf::from(p);
    }
    let base = xdg
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home.unwrap_or(".")).join(".config"));
    base.join("xtervo").join("vault.enc")
}

fn validate_master(master: &str) -> Result<(), VaultError> {
    if master.chars().count() < MIN_MASTER_LEN {
        return Err(VaultError::Invalid(format!("主密码至少 {MIN_MASTER_LEN} 个字符")));
    }
    if master.chars().count() > 1024 {
        return Err(VaultError::Invalid("主密码过长".into()));
    }
    Ok(())
}

fn validate_secret(password: &str) -> Result<(), VaultError> {
    if password.is_empty() {
        return Err(VaultError::Invalid("密码不能为空".into()));
    }
    if password.chars().count() > MAX_SECRET_LEN {
        return Err(VaultError::Invalid("密码过长".into()));
    }
    // askpass 以「密码 + 换行」输出；带换行的密码无法可靠自动填充。
    if password.contains('\n') || password.contains('\r') {
        return Err(VaultError::Invalid("密码不能包含换行".into()));
    }
    Ok(())
}

fn random_array<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

/// 尽力覆盖堆上的主密码副本；Rust 分配器/寄存器仍可能留旧值（见模块安全限制）。
fn zeroize_string(value: &mut String) {
    wipe_string(value);
}

fn derive_key(master: &str, salt: &[u8]) -> Result<[u8; KEY_LEN], VaultError> {
    derive_key_with(master, salt, KDF_M_COST, KDF_T_COST, KDF_P_COST)
}

fn derive_key_with(
    master: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<[u8; KEY_LEN], VaultError> {
    let params = Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN))
        .map_err(|e| VaultError::Corrupt(format!("Argon2 参数无效: {e}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    if let Err(e) = argon2.hash_password_into(master.as_bytes(), salt, &mut key) {
        key.fill(0);
        return Err(VaultError::Internal(format!("Argon2id 派生失败: {e}")));
    }
    Ok(key)
}

/// 解锁的纯准备步骤：读文件 + 结构校验 + Argon2 + AEAD 解密。
/// 不修改 `Vault`，可由后台线程调用；返回的 `Unlocked` 含明文凭据。
fn prepare_unlock(path: &Path, master: &str) -> Result<Unlocked, VaultError> {
    let text = read_vault_text(path)?;
    let parsed = ParsedVault::parse(&text)?;
    let key = derive_key_with(master, &parsed.salt, parsed.m_cost, parsed.t_cost, parsed.p_cost)?;
    // key 先放进 Unlocked：后面解密/JSON 任一步失败，Drop 都会把它清零。
    let mut unlocked = Unlocked { key, salt: parsed.salt, entries: BTreeMap::new() };
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&unlocked.key));
    let nonce = Nonce::from_slice(&parsed.nonce);
    let plaintext = ZeroizeVec::new(
        cipher
            .decrypt(nonce, Payload { msg: &parsed.ciphertext, aad: parsed.aad })
            .map_err(|_| VaultError::WrongPassword)?,
    );
    let entries: BTreeMap<u64, String> = serde_json::from_slice(plaintext.as_slice())
        .map_err(|e| VaultError::Corrupt(format!("凭据 JSON 无效: {e}")))?;
    unlocked.entries = entries;
    Ok(unlocked)
}

/// 初始化的纯准备步骤：校验参数 + 派生 key + 写空 vault。
fn prepare_initialize(path: &Path, master: &str) -> Result<Unlocked, VaultError> {
    if path.exists() {
        return Err(VaultError::AlreadyInitialized);
    }
    validate_master(master)?;
    let salt = random_array::<SALT_LEN>();
    let key = derive_key(master, &salt)?;
    let unlocked = Unlocked { key, salt, entries: BTreeMap::new() };
    // Argon2 在锁外完成；锁内再查一次，避免两个进程同时 initialize 互相覆盖。
    let _lock = VaultLock::acquire(path)?;
    if path.exists() {
        return Err(VaultError::AlreadyInitialized);
    }
    write_store_locked(path, &unlocked.key, &unlocked.salt, &unlocked.entries)?;
    Ok(unlocked)
}

/// 改主密码的纯准备步骤：校验新密码 + 新 salt/key + 重新加密全部凭据并落盘。
fn prepare_change(
    path: &Path,
    current: VaultChange,
    new_master: &str,
) -> Result<Unlocked, VaultError> {
    validate_master(new_master)?;
    let salt = random_array::<SALT_LEN>();
    let key = derive_key(new_master, &salt)?;
    let mut next = Unlocked { key, salt, entries: BTreeMap::new() };
    // 新 key 在锁外派生；锁内重读旧文件，确认快照没有过期。
    let _lock = VaultLock::acquire(path)?;
    let disk_entries = match read_disk_entries(path, &current.0.key, &current.0.salt)? {
        DiskState::Missing => return Err(VaultError::NotInitialized),
        DiskState::Stale => return Err(VaultError::Stale),
        DiskState::Current(entries) => entries,
    };
    if disk_entries != current.0.entries {
        // 别的进程在快照之后增删了凭据：不能用旧快照覆盖新条目。
        let mut disk_entries = disk_entries;
        wipe_entries(&mut disk_entries);
        return Err(VaultError::Stale);
    }
    next.entries = disk_entries;
    write_store_locked(path, &next.key, &next.salt, &next.entries)?;
    Ok(next)
}


struct ParsedVault<'a> {
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
    ciphertext: Vec<u8>,
    aad: &'a [u8],
}

impl<'a> ParsedVault<'a> {
    fn parse(text: &'a str) -> Result<Self, VaultError> {
        if text.len() as u64 > MAX_FILE {
            return Err(VaultError::Corrupt("文件超过大小上限".into()));
        }
        let marker = "ciphertext=";
        let pos = text
            .find(marker)
            .ok_or_else(|| VaultError::Corrupt("缺少 ciphertext 字段".into()))?;
        if pos == 0 || text.as_bytes()[pos - 1] != b'\n' {
            return Err(VaultError::Corrupt("ciphertext 不在独立行".into()));
        }
        let aad = &text.as_bytes()[..pos];
        let header = &text[..pos];
        let mut lines = header.lines();
        let first = lines.next().ok_or_else(|| VaultError::Corrupt("空文件".into()))?;
        let (magic, ver) =
            first.split_once(' ').ok_or_else(|| VaultError::Corrupt("缺少 magic".into()))?;
        if magic != MAGIC {
            return Err(VaultError::Corrupt("magic 不匹配，不是 xtervo vault 文件".into()));
        }
        let version: u32 = ver
            .trim()
            .parse()
            .map_err(|_| VaultError::Corrupt(format!("版本号无效: {ver:?}")))?;
        if version != VERSION {
            return Err(VaultError::Corrupt(format!(
                "不支持的 vault 版本 {version}（本程序支持 {VERSION}）"
            )));
        }

        let mut kdf_seen = false;
        let mut m_cost = None;
        let mut t_cost = None;
        let mut p_cost = None;
        let mut salt = None;
        let mut nonce = None;
        for line in lines {
            if line.is_empty() {
                return Err(VaultError::Corrupt("头部存在空行".into()));
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| VaultError::Corrupt(format!("头部字段无效: {line:?}")))?;
            match key {
                "kdf" => {
                    if value != "argon2id" {
                        return Err(VaultError::Corrupt(format!("不支持的 KDF: {value}")));
                    }
                    kdf_seen = true;
                },
                "m" => m_cost = Some(parse_u32(value, "m")?),
                "t" => t_cost = Some(parse_u32(value, "t")?),
                "p" => p_cost = Some(parse_u32(value, "p")?),
                "salt" => salt = Some(decode_exact::<SALT_LEN>(value, "salt")?),
                "nonce" => nonce = Some(decode_exact::<NONCE_LEN>(value, "nonce")?),
                other => {
                    return Err(VaultError::Corrupt(format!("未知头部字段: {other}")));
                },
            }
        }
        if !kdf_seen {
            return Err(VaultError::Corrupt("缺少 kdf 字段".into()));
        }
        let m_cost = m_cost.ok_or_else(|| VaultError::Corrupt("缺少 m 参数".into()))?;
        let t_cost = t_cost.ok_or_else(|| VaultError::Corrupt("缺少 t 参数".into()))?;
        let p_cost = p_cost.ok_or_else(|| VaultError::Corrupt("缺少 p 参数".into()))?;
        // 参数上下限：防恶意文件让 Argon2 吃掉内存/CPU（超限在派生之前快速失败）。
        if !(8192..=KDF_M_MAX).contains(&m_cost)
            || !(1..=KDF_T_MAX).contains(&t_cost)
            || !(1..=KDF_P_MAX).contains(&p_cost)
        {
            return Err(VaultError::Corrupt(format!(
                "Argon2 参数超出允许范围: m={m_cost} t={t_cost} p={p_cost}"
            )));
        }
        let salt = salt.ok_or_else(|| VaultError::Corrupt("缺少 salt".into()))?;
        let nonce = nonce.ok_or_else(|| VaultError::Corrupt("缺少 nonce".into()))?;

        let cipher_b64 = text[pos + marker.len()..].trim_end_matches('\n');
        let ciphertext = B64
            .decode(cipher_b64)
            .map_err(|e| VaultError::Corrupt(format!("ciphertext base64 无效: {e}")))?;
        // ChaCha20-Poly1305 tag 16 字节；空 JSON 也至少有一个字符。
        if ciphertext.len() < 16 {
            return Err(VaultError::Corrupt("ciphertext 过短".into()));
        }
        Ok(Self { m_cost, t_cost, p_cost, salt, nonce, ciphertext, aad })
    }
}

fn parse_u32(value: &str, name: &str) -> Result<u32, VaultError> {
    value.trim().parse().map_err(|_| VaultError::Corrupt(format!("{name} 参数无效: {value:?}")))
}

fn decode_exact<const N: usize>(value: &str, name: &str) -> Result<[u8; N], VaultError> {
    let raw = B64
        .decode(value.trim())
        .map_err(|e| VaultError::Corrupt(format!("{name} base64 无效: {e}")))?;
    raw.as_slice()
        .try_into()
        .map_err(|_| VaultError::Corrupt(format!("{name} 长度错误（期望 {N} 字节）")))
}

/// 尽力清零的字节缓冲：明文 JSON / 解密结果短暂存在，Drop 时覆盖。
struct ZeroizeVec(Vec<u8>);

impl ZeroizeVec {
    fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for ZeroizeVec {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// 清零一个 String 的内容（保留长度，覆盖分配器里已有的明文）。
fn wipe_string(value: &mut String) {
    #[cfg(test)]
    wipe_probe::bump();
    // SAFETY: 用 0 覆盖 UTF-8 字节是合法 UTF-8（NUL 是有效字符）。
    unsafe {
        value.as_bytes_mut().fill(0);
    }
}

fn wipe_entries(entries: &mut BTreeMap<u64, String>) {
    for value in entries.values_mut() {
        wipe_string(value);
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// 跨进程写锁：`<vault>.lock` 上的独占 `flock`。
///
/// 所有「读磁盘 → 改内存副本 → 原子写」的入口都必须先拿这把锁，否则两个
/// xtervo 进程会各自基于旧快照写回，互相丢更新（第二进程 / change_master）。
struct VaultLock {
    file: fs::File,
}

impl VaultLock {
    fn acquire(path: &Path) -> Result<Self, VaultError> {
        let dir = parent_dir(path);
        fs::create_dir_all(dir)
            .map_err(|e| VaultError::Io(format!("创建目录 {} 失败: {e}", dir.display())))?;
        let lock_path = sibling_path(path, ".lock");
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
            // 锁文件不接受符号链接，避免把 flock 下到别处。
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let file = opts
            .open(&lock_path)
            .map_err(|e| VaultError::Io(format!("打开锁文件 {} 失败: {e}", lock_path.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            loop {
                // SAFETY: file 在作用域内有效；flock 只操作 fd。
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if rc == 0 {
                    break;
                }
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(VaultError::Io(format!(
                    "锁定 {} 失败: {e}",
                    lock_path.display()
                )));
            }
        }
        Ok(Self { file })
    }
}

impl Drop for VaultLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // 关闭 fd 本身会释放 flock；显式解锁让语义更清楚。
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        #[cfg(not(unix))]
        let _ = &self.file;
    }
}

/// 读 vault 文本：限制大小、顺手把过宽的文件权限收紧到 0600。
fn read_vault_text(path: &Path) -> Result<String, VaultError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(VaultError::NotInitialized);
        },
        Err(e) => return Err(VaultError::Io(e.to_string())),
    };
    let meta = file.metadata().map_err(|e| VaultError::Io(e.to_string()))?;
    if meta.len() > MAX_FILE {
        return Err(VaultError::Corrupt("文件超过大小上限".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            // 用已打开的 fd fchmod，避免路径被替换后的 TOCTOU。
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| VaultError::Io(format!("收紧 {} 权限失败: {e}", path.display())))?;
        }
    }
    let mut text = String::new();
    file.take(MAX_FILE + 1)
        .read_to_string(&mut text)
        .map_err(|e| VaultError::Io(e.to_string()))?;
    if text.len() as u64 > MAX_FILE {
        return Err(VaultError::Corrupt("文件超过大小上限".into()));
    }
    Ok(text)
}

enum DiskState {
    Missing,
    Stale,
    Current(BTreeMap<u64, String>),
}

/// 锁内重读磁盘并用当前内存 key 解密。salt 不同说明别的进程换了主密码，
/// 当前会话不能继续写（否则会把新主密码覆盖回旧档）。
fn read_disk_entries(
    path: &Path,
    key: &[u8; KEY_LEN],
    salt: &[u8; SALT_LEN],
) -> Result<DiskState, VaultError> {
    let text = match read_vault_text(path) {
        Ok(text) => text,
        Err(VaultError::NotInitialized) => return Ok(DiskState::Missing),
        Err(e) => return Err(e),
    };
    let parsed = ParsedVault::parse(&text)?;
    if parsed.salt != *salt {
        return Ok(DiskState::Stale);
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = Nonce::from_slice(&parsed.nonce);
    let plaintext = ZeroizeVec::new(
        cipher
            .decrypt(nonce, Payload { msg: &parsed.ciphertext, aad: parsed.aad })
            .map_err(|_| {
                VaultError::Corrupt("vault 条目无法解密（文件被并发修改或已损坏）".into())
            })?,
    );
    let entries: BTreeMap<u64, String> = serde_json::from_slice(plaintext.as_slice())
        .map_err(|e| VaultError::Corrupt(format!("凭据 JSON 无效: {e}")))?;
    Ok(DiskState::Current(entries))
}

/// 序列化 + 加密 + 唯一临时文件原子写。调用方必须持有 [`VaultLock`]。
fn write_store_locked(
    path: &Path,
    key: &[u8; KEY_LEN],
    salt: &[u8; SALT_LEN],
    entries: &BTreeMap<u64, String>,
) -> Result<(), VaultError> {
    let plaintext = ZeroizeVec::new(
        serde_json::to_vec(entries)
            .map_err(|e| VaultError::Internal(format!("凭据序列化失败: {e}")))?,
    );
    let nonce_bytes = random_array::<NONCE_LEN>();
    let header = format!(
        "{MAGIC} {VERSION}\nkdf=argon2id\nm={KDF_M_COST}\nt={KDF_T_COST}\np={KDF_P_COST}\nsalt={}\nnonce={}\n",
        B64.encode(salt),
        B64.encode(nonce_bytes)
    );
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, Payload { msg: plaintext.as_slice(), aad: header.as_bytes() })
        .map_err(|_| VaultError::Internal("ChaCha20-Poly1305 加密失败".into()))?;
    let text = format!("{header}ciphertext={}\n", B64.encode(ciphertext));
    if text.len() as u64 > MAX_FILE {
        return Err(VaultError::Internal(format!(
            "vault 写入超过 {MAX_FILE} 字节上限；请减少条目"
        )));
    }
    write_atomic(path, text.as_bytes())
}

/// 唯一临时文件名：`<vault>.tmp.<pid>.<随机 64bit>`，不可预测、不共享。
fn unique_temp_path(path: &Path) -> PathBuf {
    let nonce = u64::from_le_bytes(random_array::<8>());
    sibling_path(path, &format!(".tmp.{}.{nonce:016x}", std::process::id()))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), VaultError> {
    let parent = parent_dir(path);
    fs::create_dir_all(parent)
        .map_err(|e| VaultError::Io(format!("创建目录 {} 失败: {e}", parent.display())))?;
    let tmp = unique_temp_path(path);
    let mut opts = OpenOptions::new();
    // create_new = O_CREAT|O_EXCL：不复用/不跟随别人预先放好的路径。
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let result = (|| -> Result<(), VaultError> {
        let mut file = opts
            .open(&tmp)
            .map_err(|e| VaultError::Io(format!("创建 {} 失败: {e}", tmp.display())))?;
        file.write_all(data)
            .map_err(|e| VaultError::Io(format!("写入 {} 失败: {e}", tmp.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // fd 上 fchmod：即使路径被并发替换也不会改到别的文件。
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| VaultError::Io(format!("设置权限失败: {e}")))?;
        }
        file.sync_all()
            .map_err(|e| VaultError::Io(format!("同步 {} 失败: {e}", tmp.display())))?;
        fs::rename(&tmp, path)
            .map_err(|e| VaultError::Io(format!("替换 {} 失败: {e}", path.display())))?;
        // rename 只保证目录项原子；父目录 fsync 后才保证掉电不丢新名字。
        #[cfg(unix)]
        {
            let dir = fs::File::open(parent)
                .map_err(|e| VaultError::Io(format!("打开目录 {} 失败: {e}", parent.display())))?;
            dir.sync_all().map_err(|e| {
                VaultError::Io(format!("同步目录 {} 失败: {e}", parent.display()))
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 测试探针：统计当前线程走了多少次「清零一个字符串」的路径。
/// 用 thread_local 避免并行测试互相干扰。
#[cfg(test)]
mod wipe_probe {
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn bump() {
        COUNT.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn count() -> usize {
        COUNT.with(|c| c.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtervo-vault-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn master() -> &'static str {
        "correct horse battery staple"
    }

    fn secret() -> &'static str {
        "ssh-password-123!"
    }

    #[test]
    fn oversized_vault_file_is_rejected_before_read() {
        let dir = tmpdir("oversize");
        let path = dir.join("vault.enc");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_FILE + 1).unwrap();
        let err = match prepare_unlock(&path, master()) {
            Err(error) => error,
            Ok(_) => panic!("oversized vault must be rejected"),
        };
        assert!(matches!(err, VaultError::Corrupt(_)), "{err:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn credential_is_keyed_by_profile_store_id() {
        // 连接链路：ProfileStore::add 分配 id → vault.set(id) → open_profile_tab 用
        // store 里 Profile.id 取密码。这里把这条链固定住，防止 id 错位导致取不到密码。
        use crate::profile::{Profile, ProfileStore};
        let dir = tmpdir("profile-id");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();

        let mut store = ProfileStore::default();
        let base = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "test".into(),
            auth: "password".into(),
            save_password: true,
            ..Profile::default()
        };
        let first = store.add(Profile { name: "first".into(), ..base.clone() }).unwrap();
        let second = store.add(Profile { name: "second".into(), ..base }).unwrap();
        assert_ne!(first, second, "每个 Profile 必须拿到独立 id");
        v.set(first, secret()).unwrap();
        v.set(second, "second-password").unwrap();

        let first_profile = store.get(first).unwrap();
        let second_profile = store.get(second).unwrap();
        assert_eq!(first_profile.id, first);
        assert_eq!(v.get(first_profile.id).unwrap(), secret());
        assert_eq!(v.get(second_profile.id).unwrap(), "second-password");
        assert_eq!(
            v.get(second + 1).unwrap_err(),
            VaultError::NotFound(second + 1),
            "未保存的 id 不能误取到别人的密码"
        );

        // 重新打开 vault（模拟重启）后仍按同一 id 取到同一密码。
        drop(v);
        let mut reopened = Vault::new(&path);
        reopened.unlock(master()).unwrap();
        assert_eq!(reopened.get(first_profile.id).unwrap(), secret());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_get_remove_roundtrip_survives_lock_and_reload() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        assert_eq!(v.status(), VaultStatus::Uninitialized);
        assert_eq!(v.get(1).unwrap_err(), VaultError::NotInitialized);

        v.initialize(master()).unwrap();
        assert_eq!(v.status(), VaultStatus::Unlocked);
        v.set(1, secret()).unwrap();
        v.set(42, "another").unwrap();
        assert_eq!(v.entry_count(), 2);
        assert_eq!(v.get(1).unwrap(), secret());
        assert!(v.contains(42).unwrap());

        v.lock();
        assert_eq!(v.status(), VaultStatus::Locked);
        assert_eq!(v.get(1).unwrap_err(), VaultError::Locked);
        assert_eq!(v.set(1, "x").unwrap_err(), VaultError::Locked);
        assert_eq!(v.remove(1).unwrap_err(), VaultError::Locked);
        assert_eq!(v.clear().unwrap_err(), VaultError::Locked);

        // 模拟重启：新实例默认锁定，必须重新输入主密码。
        let mut reopened = Vault::new(&path);
        assert_eq!(reopened.status(), VaultStatus::Locked);
        assert_eq!(reopened.unlock("wrong-password").unwrap_err(), VaultError::WrongPassword);
        assert_eq!(reopened.status(), VaultStatus::Locked);
        reopened.unlock(master()).unwrap();
        assert_eq!(reopened.get(1).unwrap(), secret());
        assert!(reopened.remove(1).unwrap());
        assert!(!reopened.remove(1).unwrap());
        assert_eq!(reopened.get(1).unwrap_err(), VaultError::NotFound(1));
        assert_eq!(reopened.get(42).unwrap(), "another");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_path_respects_xtervo_vault_override() {
        // 纯函数测试：不改进程环境变量，避免与并行测试竞争。
        assert_eq!(
            default_path_from(Some("/tmp/custom-vault.enc"), Some("/xdg"), Some("/home/u")),
            PathBuf::from("/tmp/custom-vault.enc")
        );
        assert_eq!(
            default_path_from(Some("   "), None, Some("/home/u")),
            PathBuf::from("/home/u/.config/xtervo/vault.enc"),
            "空 XTERVO_VAULT 应回退默认路径"
        );
        assert_eq!(
            default_path_from(None, Some("/xdg"), Some("/home/u")),
            PathBuf::from("/xdg/xtervo/vault.enc")
        );
    }

    #[test]
    fn wrong_password_and_corrupt_file_fail_cleanly() {
        let dir = tmpdir("corrupt");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set(7, secret()).unwrap();
        v.lock();

        // 错密码：报错且仍锁定。
        let mut wrong = Vault::new(&path);
        assert_eq!(wrong.unlock("bad").unwrap_err(), VaultError::WrongPassword);
        assert_eq!(wrong.status(), VaultStatus::Locked);

        // 完全不是 vault 的文件。
        let garbage = dir.join("garbage.enc");
        fs::write(&garbage, b"not a vault at all\n").unwrap();
        let mut g = Vault::new(&garbage);
        assert!(matches!(g.unlock("whatever").unwrap_err(), VaultError::Corrupt(_)));
        assert_eq!(g.status(), VaultStatus::Locked);
        assert_eq!(fs::read(&garbage).unwrap(), b"not a vault at all\n", "坏文件不能动");

        // 截断：base64/AEAD 都应报错而不是 panic。
        let before = fs::read_to_string(&path).unwrap();
        let cut = before.len() - 12;
        fs::write(&path, &before[..cut]).unwrap();
        let mut truncated = Vault::new(&path);
        assert!(truncated.unlock(master()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), &before[..cut], "损坏文件保持原样");

        // 空文件 / 只有 magic 的文件都不能 panic。
        for body in ["", "XTERVO-VAULT 1\n", "XTERVO-VAULT 999\n"] {
            let p = dir.join("edge.enc");
            fs::write(&p, body).unwrap();
            let mut edge = Vault::new(&p);
            assert!(edge.unlock(master()).is_err(), "输入 {body:?}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_never_contains_plaintext_and_permissions_are_private() {
        let dir = tmpdir("plaintext");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set(1, secret()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(secret()), "vault 文件不得含明文密码");
        assert!(!text.contains(master()), "vault 文件不得含明文主密码");
        assert!(text.starts_with(&format!("{MAGIC} {VERSION}\n")));
        assert!(text.contains("kdf=argon2id"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "vault 文件权限必须是 0600");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_master_reencrypts_and_old_password_stops_working() {
        let dir = tmpdir("change");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set(3, secret()).unwrap();
        v.change_master("new master password").unwrap();
        assert_eq!(v.get(3).unwrap(), secret());
        v.lock();

        let mut old = Vault::new(&path);
        assert_eq!(old.unlock(master()).unwrap_err(), VaultError::WrongPassword);
        let mut fresh = Vault::new(&path);
        fresh.unlock("new master password").unwrap();
        assert_eq!(fresh.get(3).unwrap(), secret());

        // 锁定时不允许改主密码。
        fresh.lock();
        assert_eq!(fresh.change_master("third password").unwrap_err(), VaultError::Locked);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn initialize_refuses_to_overwrite_and_clear_keeps_file() {
        let dir = tmpdir("init");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        assert_eq!(v.clear().unwrap_err(), VaultError::Locked);
        v.initialize(master()).unwrap();
        v.set(1, secret()).unwrap();
        v.set(2, "second").unwrap();
        assert_eq!(v.initialize("another master").unwrap_err(), VaultError::AlreadyInitialized);
        v.clear().unwrap();
        assert_eq!(v.entry_count(), 0);
        v.lock();
        let mut again = Vault::new(&path);
        again.unlock(master()).unwrap();
        assert_eq!(again.entry_count(), 0);

        // 校验输入边界。
        // 未初始化时点 Lock 不能把状态改成“已锁定”（文件仍然不存在）。
        let mut missing = Vault::new(dir.join("missing.enc"));
        missing.lock();
        assert_eq!(missing.status(), VaultStatus::Uninitialized);
        assert_eq!(missing.get(1).unwrap_err(), VaultError::NotInitialized);

        let p2 = dir.join("short.enc");
        let mut short = Vault::new(&p2);
        assert!(matches!(short.initialize("short").unwrap_err(), VaultError::Invalid(_)));
        assert_eq!(short.status(), VaultStatus::Uninitialized);
        assert!(matches!(short.initialize("").unwrap_err(), VaultError::Invalid(_)));
        short.initialize("long enough").unwrap();
        assert!(matches!(short.set(1, "").unwrap_err(), VaultError::Invalid(_)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_header_params_are_rejected_before_argon2() {
        let dir = tmpdir("params");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set(1, secret()).unwrap();
        v.lock();
        let good = fs::read_to_string(&path).unwrap();
        // 把 m 改成离谱值：结构校验应先拒绝，不能真的跑 4GB 内存的 KDF。
        let bad = good.replace("m=19456", "m=4294967295");
        let p = dir.join("huge.enc");
        fs::write(&p, bad).unwrap();
        let mut huge = Vault::new(&p);
        assert!(matches!(huge.unlock(master()).unwrap_err(), VaultError::Corrupt(_)));

        // 缺少 salt / nonce。
        for needle in ["salt=", "nonce="] {
            let broken: String = good
                .lines()
                .filter(|l| !l.starts_with(needle))
                .map(|l| format!("{l}\n"))
                .collect();
            let p = dir.join("missing.enc");
            fs::write(&p, broken).unwrap();
            let mut m = Vault::new(&p);
            assert!(matches!(m.unlock(master()).unwrap_err(), VaultError::Corrupt(_)));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- 后台 KDF 任务（P1-08）：UI 线程只轮询，不跑 Argon2 ----

    #[test]
    fn background_unlock_moves_kdf_off_caller_and_adopts_session() {
        let dir = tmpdir("bg-unlock");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set(9, secret()).unwrap();
        v.lock();

        let (wake, rx) = xtervo_core::WakeSignal::new();
        let task = VaultTask::unlock(path.clone(), master().to_string(), wake);
        // 任务在独立线程里跑：这里立即返回，不阻塞调用线程。
        rx.recv_timeout(Duration::from_secs(30)).expect("KDF 完成应唤醒 UI");
        let outcome = task.wait().expect("应拿到任务结果");
        let VaultTaskOutcome::Ok(session) = outcome else {
            panic!("后台解锁失败: {}", outcome_error(outcome));
        };
        assert_eq!(session.entry_count(), 1);
        let mut ui = Vault::new(&path);
        ui.adopt(session);
        assert_eq!(ui.status(), VaultStatus::Unlocked);
        assert_eq!(ui.get(9).unwrap(), secret());

        // 同一个 Vault 的 adopt 路径（UI 实际用法）。
        let mut second = Vault::new(&path);
        let task = VaultTask::unlock(path, master().to_string(), xtervo_core::WakeSignal::new().0);
        let VaultTaskOutcome::Ok(session) = task.wait().unwrap() else {
            panic!("第二次后台解锁失败");
        };
        second.adopt(session);
        assert_eq!(second.get(9).unwrap(), secret());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_unlock_wrong_password_keeps_ui_locked() {
        let dir = tmpdir("bg-wrong");
        let path = dir.join("vault.enc");
        Vault::new(&path).initialize(master()).unwrap();
        let task = VaultTask::unlock(
            path.clone(),
            "wrong-password".into(),
            xtervo_core::WakeSignal::new().0,
        );
        match task.wait().unwrap() {
            VaultTaskOutcome::Err(e) => assert_eq!(e, VaultError::WrongPassword),
            VaultTaskOutcome::Ok(_) => panic!("错密码不能返回会话"),
        }
        let mut v = Vault::new(&path);
        assert_eq!(v.status(), VaultStatus::Locked);
        assert_eq!(v.get(1).unwrap_err(), VaultError::Locked);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_change_master_reencrypts_without_leaking_old_key() {
        let dir = tmpdir("bg-change");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set(4, secret()).unwrap();
        let snapshot = v.change_snapshot().unwrap();
        let task = VaultTask::change_master(
            path.clone(),
            snapshot,
            "brand new master".into(),
            xtervo_core::WakeSignal::new().0,
        );
        let VaultTaskOutcome::Ok(session) = task.wait().unwrap() else {
            panic!("后台改密码失败");
        };
        v.adopt(session);
        assert_eq!(v.get(4).unwrap(), secret());

        let mut old = Vault::new(&path);
        assert_eq!(old.unlock(master()).unwrap_err(), VaultError::WrongPassword);
        let mut fresh = Vault::new(&path);
        fresh.unlock("brand new master").unwrap();
        assert_eq!(fresh.get(4).unwrap(), secret());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_initialize_state_machine_and_errors() {
        let dir = tmpdir("bg-init");
        let path = dir.join("vault.enc");
        let task = VaultTask::initialize(
            path.clone(),
            master().to_string(),
            xtervo_core::WakeSignal::new().0,
        );
        let VaultTaskOutcome::Ok(session) = task.wait().unwrap() else {
            panic!("后台初始化失败");
        };
        let mut v = Vault::new(&path);
        v.adopt(session);
        assert_eq!(v.status(), VaultStatus::Unlocked);

        // 已存在时拒绝覆盖；错误信息不含密码。
        let task = VaultTask::initialize(
            path.clone(),
            "another master".into(),
            xtervo_core::WakeSignal::new().0,
        );
        match task.wait().unwrap() {
            VaultTaskOutcome::Err(VaultError::AlreadyInitialized) => {},
            other => panic!("期望 AlreadyInitialized，得到 {}", outcome_error(other)),
        }
        assert_eq!(fs::read_to_string(&path).unwrap().matches("ciphertext=").count(), 1);

        // 锁定时 change_snapshot 立即返回 Locked（不启动后台线程）。
        let locked = Vault::new(&path);
        assert!(matches!(
            locked.change_snapshot().map(|_| ()),
            Err(VaultError::Locked)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_task_drop_does_not_block_or_leak_into_ui() {
        let dir = tmpdir("bg-drop");
        let path = dir.join("vault.enc");
        Vault::new(&path).initialize(master()).unwrap();
        // 启动后立即丢弃任务：Drop 不能 join 卡住调用线程。
        let started = std::time::Instant::now();
        let task = VaultTask::unlock(
            path.clone(),
            master().to_string(),
            xtervo_core::WakeSignal::new().0,
        );
        drop(task);
        assert!(started.elapsed() < Duration::from_secs(1), "Drop 不能阻塞 UI");
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- 并发/跨进程：persist 的 read-modify-write 必须串行化并合并 ----

    const CHILD_PATH_ENV: &str = "XTERVO_VAULT_TEST_CHILD_PATH";
    const CHILD_MASTER_ENV: &str = "XTERVO_VAULT_TEST_CHILD_MASTER";

    /// 子进程 helper：正常跑测试套件时环境变量不存在，直接返回。
    #[test]
    fn vault_child_set_helper() {
        let (Ok(path), Ok(master)) =
            (std::env::var(CHILD_PATH_ENV), std::env::var(CHILD_MASTER_ENV))
        else {
            return;
        };
        let mut v = Vault::new(path);
        v.unlock(&master).unwrap();
        v.set(2, "child-secret").unwrap();
    }

    fn run_child_set(path: &Path) {
        let exe = std::env::current_exe().expect("测试二进制路径");
        let output = std::process::Command::new(exe)
            .args(["--exact", "vault::tests::vault_child_set_helper", "--nocapture"])
            .env(CHILD_PATH_ENV, path)
            .env(CHILD_MASTER_ENV, master())
            .output()
            .expect("启动 vault 子进程");
        assert!(
            output.status.success(),
            "子进程失败: status={:?}\nstdout={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn cross_process_set_merges_stale_session() {
        let dir = tmpdir("xproc");
        let path = dir.join("vault.enc");
        let mut parent = Vault::new(&path);
        parent.initialize(master()).unwrap();
        parent.set(1, secret()).unwrap();

        // 父进程保持旧的内存快照 {1}；子进程在这个快照之外写入了 profile 2。
        parent.lock();
        parent.unlock(master()).unwrap();
        run_child_set(&path);

        // 父进程的 set 必须锁内重读磁盘并合并，不能把子进程的 2 覆盖掉。
        parent.set(3, "parent-secret").unwrap();
        assert_eq!(parent.get(2).unwrap(), "child-secret", "内存也应合并子进程写入");
        assert_eq!(parent.get(1).unwrap(), secret());

        drop(parent);
        let mut reopened = Vault::new(&path);
        reopened.unlock(master()).unwrap();
        assert_eq!(reopened.get(1).unwrap(), secret());
        assert_eq!(reopened.get(2).unwrap(), "child-secret", "跨进程更新不能被丢");
        assert_eq!(reopened.get(3).unwrap(), "parent-secret");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_passphrase_uses_separate_slot() {
        let dir = tmpdir("key-passphrase");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set_key_passphrase(7, "key-secret").unwrap();
        v.set(7, "login-secret").unwrap();
        assert_eq!(v.get_key_passphrase(7).unwrap(), "key-secret");
        assert_eq!(v.get(7).unwrap(), "login-secret");
        assert!(v.has_key_passphrase(7).unwrap());
        v.remove(7 ^ Vault::KEY_PASSPHRASE_SLOT).unwrap();
        assert!(!v.has_key_passphrase(7).unwrap());
        assert_eq!(v.get(7).unwrap(), "login-secret");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_sessions_merge_set_and_remove_updates() {
        let dir = tmpdir("two-sessions");
        let path = dir.join("vault.enc");
        let mut a = Vault::new(&path);
        a.initialize(master()).unwrap();
        a.set(1, secret()).unwrap();
        let mut b = Vault::new(&path);
        b.unlock(master()).unwrap();

        // a 先写 2；b 的内存仍是 {1}，但 b 写 3 时不能把 2 抹掉。
        a.set(2, "from-a").unwrap();
        b.set(3, "from-b").unwrap();
        assert_eq!(b.get(2).unwrap(), "from-a");

        // a 删除 1 后，b 的下一次写入仍要保持这个删除。
        a.remove(1).unwrap();
        b.set(4, "again-b").unwrap();
        assert_eq!(b.get(1).unwrap_err(), VaultError::NotFound(1));
        assert_eq!(b.get(2).unwrap(), "from-a");
        assert_eq!(b.get(3).unwrap(), "from-b");
        assert_eq!(b.get(4).unwrap(), "again-b");

        drop((a, b));
        let mut reopened = Vault::new(&path);
        reopened.unlock(master()).unwrap();
        assert_eq!(reopened.get(1).unwrap_err(), VaultError::NotFound(1));
        assert_eq!(reopened.get(2).unwrap(), "from-a");
        assert_eq!(reopened.get(3).unwrap(), "from-b");
        assert_eq!(reopened.get(4).unwrap(), "again-b");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_master_refuses_stale_entries_snapshot() {
        let dir = tmpdir("stale-change");
        let path = dir.join("vault.enc");
        let mut a = Vault::new(&path);
        a.initialize(master()).unwrap();
        a.set(1, secret()).unwrap();
        let mut b = Vault::new(&path);
        b.unlock(master()).unwrap();
        // a 在 b 取快照之后又写了一条；b 的 change_master 不能用陈旧快照覆盖。
        a.set(2, "newer-entry").unwrap();
        assert_eq!(
            b.change_master("stale new master").unwrap_err(),
            VaultError::Stale,
            "陈旧快照改主密码必须失败"
        );
        // 文件必须仍是旧主密码 + 两条完整凭据。
        let mut reopened = Vault::new(&path);
        reopened.unlock(master()).unwrap();
        assert_eq!(reopened.get(1).unwrap(), secret());
        assert_eq!(reopened.get(2).unwrap(), "newer-entry");
        let mut wrong = Vault::new(&path);
        assert_eq!(wrong.unlock("stale new master").unwrap_err(), VaultError::WrongPassword);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_master_invalidates_other_session_credentials() {
        let dir = tmpdir("rekey-stale-session");
        let path = dir.join("vault.enc");
        let mut a = Vault::new(&path);
        a.initialize(master()).unwrap();
        a.set(1, secret()).unwrap();
        let mut b = Vault::new(&path);
        b.unlock(master()).unwrap();
        a.change_master("fresh master").unwrap();

        // b 还持有旧 key：再写必须被拒绝，不能把文件改回旧主密码。
        assert_eq!(
            b.set(2, "must-not-land").unwrap_err(),
            VaultError::Stale,
            "旧会话在 rekey 后不能写"
        );
        assert_eq!(
            b.remove(1).unwrap_err(),
            VaultError::Stale,
            "旧会话在 rekey 后不能删除"
        );
        let mut fresh = Vault::new(&path);
        fresh.unlock("fresh master").unwrap();
        assert_eq!(fresh.get(1).unwrap(), secret());
        assert_eq!(fresh.get(2).unwrap_err(), VaultError::NotFound(2));
        let mut old = Vault::new(&path);
        assert_eq!(old.unlock(master()).unwrap_err(), VaultError::WrongPassword);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- 临时文件/权限 ----

    #[cfg(unix)]
    #[test]
    fn persist_does_not_follow_symlinked_legacy_tmp_file() {
        use std::os::unix::fs::symlink;
        let dir = tmpdir("tmp-symlink");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();

        let victim = dir.join("victim.txt");
        fs::write(&victim, b"victim-must-survive").unwrap();
        // 旧实现的可预测临时文件路径。
        let legacy_tmp = path.with_extension("enc.tmp");
        symlink(&victim, &legacy_tmp).unwrap();

        v.set(1, secret()).unwrap();
        assert_eq!(
            fs::read(&victim).unwrap(),
            b"victim-must-survive",
            "persist 不得跟随可预测 tmp 路径的符号链接"
        );
        assert!(
            !fs::symlink_metadata(&path).unwrap().file_type().is_symlink(),
            "vault 文件本身被替换"
        );
        let mut reopened = Vault::new(&path);
        reopened.unlock(master()).unwrap();
        assert_eq!(reopened.get(1).unwrap(), secret());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn unlock_tightens_loose_vault_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("loose-perms");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set(1, secret()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let mut reopened = Vault::new(&path);
        reopened.unlock(master()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "解锁/加载时必须把过宽权限收紧到 0600");
        assert_eq!(reopened.get(1).unwrap(), secret());
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- 清零 ----

    #[test]
    fn clear_remove_and_overwrite_wipe_displaced_secrets() {
        let dir = tmpdir("wipe-paths");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.set(1, secret()).unwrap();

        let before = wipe_probe::count();
        v.set(1, "replacement").unwrap();
        assert!(wipe_probe::count() > before, "覆盖旧凭据时必须擦除被替换的值");

        v.set(2, "second").unwrap();
        let before = wipe_probe::count();
        v.remove(2).unwrap();
        assert!(wipe_probe::count() > before, "remove 必须擦除被删除的值");

        v.set(3, "third").unwrap();
        let before = wipe_probe::count();
        v.clear().unwrap();
        assert!(wipe_probe::count() > before, "clear 必须擦除全部凭据");
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- KDF 参数上限 ----

    #[test]
    fn kdf_params_above_caps_are_rejected_before_argon2() {
        let dir = tmpdir("kdf-caps");
        let path = dir.join("vault.enc");
        let mut v = Vault::new(&path);
        v.initialize(master()).unwrap();
        v.lock();
        let good = fs::read_to_string(&path).unwrap();

        for (name, from, to) in
            [("m", "m=19456", "m=65537"), ("t", "t=2", "t=5"), ("p", "p=1", "p=5")]
        {
            let bad = good.replace(from, to);
            assert_ne!(bad, good, "{name} fixture 未替换");
            // 直接测 parse：它不会跑 Argon2，返回 Corrupt 就证明超限在派生之前被拒。
            let parsed = ParsedVault::parse(&bad);
            assert!(
                matches!(parsed, Err(VaultError::Corrupt(_))),
                "{name} 超限必须在 Argon2 之前拒绝"
            );
        }

        // 边界值仍允许（不误伤合理档位）。
        let boundary = good
            .replace("m=19456", "m=65536")
            .replace("t=2", "t=4")
            .replace("p=1", "p=4");
        assert!(ParsedVault::parse(&boundary).is_ok(), "上限边界必须允许");

        let p = dir.join("m-cap.enc");
        fs::write(&p, good.replace("m=19456", "m=65537")).unwrap();
        let mut capped = Vault::new(&p);
        assert!(
            matches!(capped.unlock(master()).unwrap_err(), VaultError::Corrupt(_)),
            "unlock 也必须快速报 Corrupt"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlocked_wipe_zeroes_key_and_entries_in_place() {
        let mut unlocked = Unlocked {
            key: [0x5a; KEY_LEN],
            salt: [0; SALT_LEN],
            entries: BTreeMap::from([(1u64, secret().to_string())]),
        };
        unlocked.wipe();
        assert_eq!(unlocked.key, [0u8; KEY_LEN], "key 必须原地清零");
        let value = unlocked.entries.get(&1).unwrap();
        assert!(value.as_bytes().iter().all(|b| *b == 0), "凭据字节必须被清零");
    }

    #[cfg(unix)]
    #[test]
    fn store_lock_excludes_second_handle_until_released() {
        use std::os::unix::io::AsRawFd;
        let dir = tmpdir("flock");
        let path = dir.join("vault.enc");
        let guard = VaultLock::acquire(&path).unwrap();
        let lock_path = sibling_path(&path, ".lock");
        let second = OpenOptions::new().read(true).write(true).open(&lock_path).unwrap();
        // 非阻塞探测：锁被持有时第二个 fd 必须拿不到。
        let rc = unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, -1, "第二个 fd 不应在锁释放前获得 flock");
        let err = std::io::Error::last_os_error();
        assert_eq!(err.raw_os_error(), Some(libc::EWOULDBLOCK), "应返回 EWOULDBLOCK: {err}");
        drop(guard);
        let rc = unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "释放后第二个 fd 应能获得 flock");
        unsafe {
            libc::flock(second.as_raw_fd(), libc::LOCK_UN);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn outcome_error(outcome: VaultTaskOutcome) -> String {
        match outcome {
            VaultTaskOutcome::Err(e) => e.to_string(),
            VaultTaskOutcome::Ok(_) => "ok".into(),
        }
    }
}
