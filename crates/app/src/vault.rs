// SPDX-License-Identifier: GPL-3.0-only
//! Vault：加密凭据存储（MVP）。
//!
//! 文件默认 `~/.config/tervo/vault.enc`（`TERVO_VAULT` 可覆盖）。格式是纯文本 +
//! base64 字段，便于排查损坏（不含明文秘密）：
//!
//! ```text
//! TERVO-VAULT 1
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
//! - 主密码与明文密码**不落盘、不写日志**；文件权限 0600，原子写。
//!
//! 安全限制（务必与 `docs/VAULT.md` 保持一致）：
//! - 解锁后密码在进程内存中；本 MVP 未做 `mlock`/zeroize 级别防护。
//! - 未接 Linux Secret Service/Keyring；加密文件是唯一后端。
//! - 错密码与密文损坏在 AEAD 层不可区分，统一返回「密码错误或文件损坏」。
//! - 与 Vault 配套的 SSH askpass 仍通过子进程环境变量传密码，同用户可经
//!   `/proc/<pid>/environ` 观察；后续应改为继承 fd/pipe（见 `askpass.rs`）。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

/// 文件首行的 magic（与版本分两段：`TERVO-VAULT <version>`）。
pub const MAGIC: &str = "TERVO-VAULT";
/// 当前 vault 文件版本。
pub const VERSION: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
/// Argon2id 参数（OWASP 推荐档：19 MiB / t=2 / p=1）。
const KDF_M_COST: u32 = 19_456;
const KDF_T_COST: u32 = 2;
const KDF_P_COST: u32 = 1;
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

enum State {
    Uninitialized,
    Locked,
    Unlocked(Unlocked),
}

/// 加密凭据容器。一个进程一个实例；`lock()` 后内存中的 key/密码一并丢弃。
pub struct Vault {
    path: PathBuf,
    state: State,
}

impl Vault {
    /// 默认路径：`$XDG_CONFIG_HOME/tervo/vault.enc` 或 `~/.config/tervo/vault.enc`；
    /// `TERVO_VAULT` 优先（测试/多实例）。
    pub fn default_path() -> PathBuf {
        default_path_from(
            std::env::var("TERVO_VAULT").ok().as_deref(),
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
    pub fn initialize(&mut self, master: &str) -> Result<(), VaultError> {
        if self.path.exists() {
            return Err(VaultError::AlreadyInitialized);
        }
        validate_master(master)?;
        let salt = random_array::<SALT_LEN>();
        let key = derive_key(master, &salt)?;
        let unlocked = Unlocked { key, salt, entries: BTreeMap::new() };
        persist(&self.path, &unlocked)?;
        self.state = State::Unlocked(unlocked);
        Ok(())
    }

    /// 解锁：读取文件 → 校验结构 → Argon2id 派生 → AEAD 解密。
    /// 失败时保持原状态（不会把半解密数据留在内存里）。
    pub fn unlock(&mut self, master: &str) -> Result<(), VaultError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.state = State::Uninitialized;
                return Err(VaultError::NotInitialized);
            },
            Err(e) => return Err(VaultError::Io(e.to_string())),
        };
        let parsed = ParsedVault::parse(&text)?;
        let key = derive_key_with(
            master,
            &parsed.salt,
            parsed.m_cost,
            parsed.t_cost,
            parsed.p_cost,
        )?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let nonce = Nonce::from_slice(&parsed.nonce);
        let plaintext = cipher
            .decrypt(nonce, Payload { msg: &parsed.ciphertext, aad: parsed.aad })
            .map_err(|_| VaultError::WrongPassword)?;
        let entries: BTreeMap<u64, String> = serde_json::from_slice(&plaintext)
            .map_err(|e| VaultError::Corrupt(format!("凭据 JSON 无效: {e}")))?;
        self.state = State::Unlocked(Unlocked { key, salt: parsed.salt, entries });
        Ok(())
    }

    /// 清空内存状态；之后 get/set/remove 都返回 `Locked`。
    /// 尽力覆盖 key 与密码字符串（Rust 分配器可能仍留旧副本，见文档的安全限制）。
    pub fn lock(&mut self) {
        if !matches!(self.state, State::Unlocked(_)) {
            return;
        }
        if let State::Unlocked(mut old) = std::mem::replace(&mut self.state, State::Locked) {
            old.key.fill(0);
            for (_, value) in old.entries.iter_mut() {
                // NUL 也是合法 UTF-8；覆盖后立刻释放，不暴露给 UI。
                unsafe {
                    value.as_bytes_mut().fill(0);
                }
            }
        }
    }

    /// 修改主密码：必须已解锁；用新 salt/key 重新加密全部凭据。
    pub fn change_master(&mut self, new_master: &str) -> Result<(), VaultError> {
        // 锁定优先于参数校验：锁定时一律返回 Locked。
        let State::Unlocked(cur) = &self.state else {
            return Err(VaultError::Locked);
        };
        validate_master(new_master)?;
        let salt = random_array::<SALT_LEN>();
        let key = derive_key(new_master, &salt)?;
        let next = Unlocked { key, salt, entries: cur.entries.clone() };
        persist(&self.path, &next)?;
        // 旧 key/旧密码副本尽量清零后再丢弃。
        if let State::Unlocked(mut old) =
            std::mem::replace(&mut self.state, State::Unlocked(next))
        {
            old.key.fill(0);
            for (_, value) in old.entries.iter_mut() {
                unsafe {
                    value.as_bytes_mut().fill(0);
                }
            }
        }
        Ok(())
    }

    /// 写入/覆盖一个 Profile 的密码并持久化。失败时回滚内存值。
    pub fn set(&mut self, profile_id: u64, password: &str) -> Result<(), VaultError> {
        let State::Unlocked(cur) = &mut self.state else {
            return Err(VaultError::Locked);
        };
        validate_secret(password)?;
        let previous = cur.entries.insert(profile_id, password.to_string());
        if let Err(e) = persist(&self.path, cur) {
            match previous {
                Some(old) => {
                    cur.entries.insert(profile_id, old);
                },
                None => {
                    cur.entries.remove(&profile_id);
                },
            }
            return Err(e);
        }
        Ok(())
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
        let Some(old) = cur.entries.remove(&profile_id) else {
            return Ok(false);
        };
        if let Err(e) = persist(&self.path, cur) {
            cur.entries.insert(profile_id, old);
            return Err(e);
        }
        Ok(true)
    }

    /// 清空全部凭据（保留文件与主密码），并持久化。
    pub fn clear(&mut self) -> Result<(), VaultError> {
        let State::Unlocked(cur) = &mut self.state else {
            return Err(VaultError::Locked);
        };
        let old = std::mem::take(&mut cur.entries);
        if let Err(e) = persist(&self.path, cur) {
            cur.entries = old;
            return Err(e);
        }
        Ok(())
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        // 退出/重建 AppState 时也清掉内存里的 key/密码。
        self.lock();
    }
}

/// 纯函数版路径解析（测试不需要改进程环境变量）。
fn default_path_from(tervo_vault: Option<&str>, xdg: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(p) = tervo_vault.filter(|p| !p.trim().is_empty()) {
        return PathBuf::from(p);
    }
    let base = xdg
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home.unwrap_or(".")).join(".config"));
    base.join("tervo").join("vault.enc")
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
    argon2
        .hash_password_into(master.as_bytes(), salt, &mut key)
        .map_err(|e| VaultError::Internal(format!("Argon2id 派生失败: {e}")))?;
    Ok(key)
}

/// 解析后的文件头 + 密文。`aad` 指向原始头部字节，解密时必须保持一致。
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
            return Err(VaultError::Corrupt("magic 不匹配，不是 Tervo vault 文件".into()));
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
        // 参数上下限：防恶意文件让 Argon2 吃掉内存/CPU。
        if !(8192..=262_144).contains(&m_cost)
            || !(1..=16).contains(&t_cost)
            || !(1..=8).contains(&p_cost)
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

/// 序列化 + 加密 + 原子写。密钥/明文只在本函数栈上短暂存在。
fn persist(path: &Path, unlocked: &Unlocked) -> Result<(), VaultError> {
    let plaintext = serde_json::to_vec(&unlocked.entries)
        .map_err(|e| VaultError::Internal(format!("凭据序列化失败: {e}")))?;
    let nonce_bytes = random_array::<NONCE_LEN>();
    let header = format!(
        "{MAGIC} {VERSION}\nkdf=argon2id\nm={KDF_M_COST}\nt={KDF_T_COST}\np={KDF_P_COST}\nsalt={}\nnonce={}\n",
        B64.encode(unlocked.salt),
        B64.encode(nonce_bytes)
    );
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&unlocked.key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, Payload { msg: &plaintext, aad: header.as_bytes() })
        .map_err(|_| VaultError::Internal("ChaCha20-Poly1305 加密失败".into()))?;
    let text = format!("{header}ciphertext={}\n", B64.encode(ciphertext));
    write_atomic(path, text.as_bytes())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), VaultError> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| {
        VaultError::Io(format!("创建目录 {} 失败: {e}", parent.display()))
    })?;
    let tmp = path.with_extension("enc.tmp");
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| -> Result<(), VaultError> {
        let mut file = opts
            .open(&tmp)
            .map_err(|e| VaultError::Io(format!("打开 {} 失败: {e}", tmp.display())))?;
        file.write_all(data)
            .map_err(|e| VaultError::Io(format!("写入 {} 失败: {e}", tmp.display())))?;
        file.sync_all()
            .map_err(|e| VaultError::Io(format!("同步 {} 失败: {e}", tmp.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
                .map_err(|e| VaultError::Io(format!("设置权限失败: {e}")))?;
        }
        fs::rename(&tmp, path)
            .map_err(|e| VaultError::Io(format!("替换 {} 失败: {e}", path.display())))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tervo-vault-{}-{tag}", std::process::id()));
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
    fn default_path_respects_tervo_vault_override() {
        // 纯函数测试：不改进程环境变量，避免与并行测试竞争。
        assert_eq!(
            default_path_from(Some("/tmp/custom-vault.enc"), Some("/xdg"), Some("/home/u")),
            PathBuf::from("/tmp/custom-vault.enc")
        );
        assert_eq!(
            default_path_from(Some("   "), None, Some("/home/u")),
            PathBuf::from("/home/u/.config/tervo/vault.enc"),
            "空 TERVO_VAULT 应回退默认路径"
        );
        assert_eq!(
            default_path_from(None, Some("/xdg"), Some("/home/u")),
            PathBuf::from("/xdg/tervo/vault.enc")
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
        for body in ["", "TERVO-VAULT 1\n", "TERVO-VAULT 999\n"] {
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
}
