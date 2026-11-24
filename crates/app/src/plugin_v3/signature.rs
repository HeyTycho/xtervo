// SPDX-License-Identifier: GPL-3.0-only
//! xtervo Plugin API v3 本地安装签名格式（Ed25519，无市场/无信任链）。
//!
//! 只解决“这个本地包是不是由持有对应私钥的人签过，且 plugin.toml 未被改动”。
//! 不提供 key 分发、key 轮换、证书链或在线验证；公钥来源完全由本机配置决定。
//!
//! 文件格式（全部为 UTF-8 文本，base64 为标准字母表 + padding）：
//!
//! ```text
//! plugin.toml.sig:
//!   xtervo-plugin-signature-v1
//!   <base64(64 字节 Ed25519 signature)>
//!
//! plugin-signing.pub（或 XTERVO_PLUGIN_PUBKEY 内容）:
//!   xtervo-plugin-pubkey-v1
//!   <base64(32 字节 Ed25519 public key)>
//! ```
//!
//! 也接受裸 base64 / 64 位 hex 公钥，以及 PEM SubjectPublicKeyInfo
//! （`-----BEGIN PUBLIC KEY-----`）。签名覆盖 `plugin.toml` 的**原始字节**，
//! 不规范化换行，因此签名后再格式化清单会失效——这是故意的。
//!
//! 公钥查找顺序：
//! 1. 调用方显式传入（测试 / `--pubkey` 类入口）；
//! 2. 环境变量 `XTERVO_PLUGIN_PUBKEY`（值可以是 key 内容、文件路径或 `@路径`）；
//! 3. `$XDG_CONFIG_HOME/xtervo/plugin-signing.pub`（跟随 `XTERVO_PLUGINS` 的同级目录）。
//!
//! 是否强制签名由 `XTERVO_PLUGIN_REQUIRE_SIGNATURE=1` 或设置页开关
//! （`plugin_require_signature`）决定。默认不强制；但没有公钥时，带
//! `plugin.toml.sig` 的包会被拒绝（无法验证的签名不能被当成“通过”）。

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

/// 安装包内的签名文件名（与 plugin.toml 同目录）。
pub const SIG_FILE_NAME: &str = "plugin.toml.sig";
/// 签名文件格式头。
pub const SIG_HEADER: &str = "xtervo-plugin-signature-v1";
/// 公钥文件格式头。
pub const PUBKEY_HEADER: &str = "xtervo-plugin-pubkey-v1";
/// 私钥文件格式头（只供 `xtervo plugins sign/keygen` 使用）。
pub const SIGNING_KEY_HEADER: &str = "xtervo-plugin-signing-key-v1";
/// 默认公钥文件名（在 `<config>/xtervo/` 下）。
pub const DEFAULT_PUBKEY_FILE: &str = "plugin-signing.pub";
/// 默认私钥文件名（在 `<config>/xtervo/` 下；只有 keygen/sign 使用）。
pub const DEFAULT_SIGNING_KEY_FILE: &str = "plugin-signing.key";

pub const PUBKEY_ENV: &str = "XTERVO_PLUGIN_PUBKEY";
pub const REQUIRE_SIGNATURE_ENV: &str = "XTERVO_PLUGIN_REQUIRE_SIGNATURE";

/// 签名文件读取上限：64 字节签名 + 头，留足注释空间但拒绝滥用。
pub const MAX_SIG_FILE_BYTES: u64 = 4 * 1024;
/// 公钥/私钥文件读取上限。
pub const MAX_KEY_FILE_BYTES: u64 = 16 * 1024;

/// 已安装插件/待安装包的签名状态（用于 UI 展示）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureStatus {
    /// 签名存在且用当前公钥验证通过。
    Verified { key_id: String },
    /// 没有 `plugin.toml.sig`。
    Unsigned,
    /// 有签名但本机没有公钥，无法验证。
    Unverifiable { reason: String },
    /// 签名文件/格式/签名值无效。
    Invalid { reason: String },
}

impl Default for SignatureStatus {
    fn default() -> Self {
        Self::Unsigned
    }
}

impl SignatureStatus {
    pub fn verified(&self) -> bool {
        matches!(self, SignatureStatus::Verified { .. })
    }

    pub fn is_unsigned(&self) -> bool {
        matches!(self, SignatureStatus::Unsigned)
    }

    /// UI 一行展示；key_id 是公钥 SHA-256 的前 16 个 hex 字符。
    pub fn label(&self) -> String {
        match self {
            SignatureStatus::Verified { key_id } => format!("verified · ed25519 · key {key_id}"),
            SignatureStatus::Unsigned => "unsigned".to_string(),
            SignatureStatus::Unverifiable { reason } => {
                format!("unverifiable · {reason}")
            }
            SignatureStatus::Invalid { reason } => format!("invalid · {reason}"),
        }
    }
}

/// 一次安装尝试使用的签名策略。
#[derive(Clone, Debug)]
pub struct SignatureConfig {
    pub required: bool,
    pub key: Option<VerifyingKey>,
    pub key_id: Option<String>,
    /// 人类可读的公钥来源（环境变量/文件路径/未配置）。
    pub key_source: String,
}

impl SignatureConfig {
    /// 解析配置。`explicit_pubkey` 非空时优先于环境变量和默认文件。
    pub fn resolve(
        plugins_dir: &Path,
        require_config: bool,
        explicit_pubkey: Option<&str>,
    ) -> Result<Self, String> {
        let required = require_config || env_requires_signature()?;
        let default_path = default_pubkey_path(plugins_dir);
        let (key, key_source) = match explicit_pubkey.map(str::trim).filter(|v| !v.is_empty()) {
            Some(value) => (
                Some(parse_public_key(value)?),
                "explicit (tests/CLI override)".to_string(),
            ),
            None => match std::env::var(PUBKEY_ENV) {
                Ok(value) if !value.trim().is_empty() => (
                    Some(parse_public_key(&load_key_value(&value)?)?),
                    format!("env {PUBKEY_ENV}"),
                ),
                _ => {
                    if default_path.is_file() {
                        let bytes = read_limited_file(&default_path, MAX_KEY_FILE_BYTES)?;
                        let text = String::from_utf8(bytes).map_err(|_| {
                            format!("公钥文件不是 UTF-8: {}", default_path.display())
                        })?;
                        (
                            Some(parse_public_key(&text)?),
                            default_path.display().to_string(),
                        )
                    } else {
                        (None, "none configured".to_string())
                    }
                }
            },
        };
        if required && key.is_none() {
            return Err(format!(
                "要求插件签名，但没有可用的 Ed25519 公钥：设置 {PUBKEY_ENV} 或 {}（见 docs/PLUGIN_INSTALL.md）",
                default_path.display()
            ));
        }
        let key_id = key.as_ref().map(public_key_id);
        Ok(Self {
            required,
            key,
            key_id,
            key_source,
        })
    }

    pub fn default_pubkey_path(plugins_dir: &Path) -> PathBuf {
        default_pubkey_path(plugins_dir)
    }

    /// 显式策略（测试/嵌入方使用）：完全绕过环境变量和默认公钥文件。
    pub fn explicit(required: bool, key: Option<VerifyingKey>) -> Self {
        let key_id = key.as_ref().map(public_key_id);
        Self {
            required,
            key,
            key_id,
            key_source: "explicit".to_string(),
        }
    }

    /// 校验 manifest 原始字节。
    ///
    /// 返回 `Err` 表示这次安装必须拒绝：坏签名、有签名但无公钥、要求签名但缺失。
    /// 未配置公钥且未要求签名、包也未带签名时返回 `Ok(Unsigned)`。
    pub fn verify_manifest(
        &self,
        manifest: &[u8],
        signature_file: Option<&[u8]>,
    ) -> Result<SignatureStatus, String> {
        let Some(sig_bytes) = signature_file else {
            if self.required {
                return Err(format!(
                    "插件未签名（缺少 {SIG_FILE_NAME}），但当前配置要求签名"
                ));
            }
            return Ok(SignatureStatus::Unsigned);
        };
        let Some(key) = self.key.as_ref() else {
            return Err(format!(
                "插件带有 {SIG_FILE_NAME}，但本机没有配置 Ed25519 公钥（设置 {PUBKEY_ENV} 或默认公钥文件；无公钥时无法验证签名）"
            ));
        };
        let signature = parse_signature_file(sig_bytes)?;
        key.verify_strict(manifest, &signature)
            .map_err(|_| "Ed25519 签名校验失败（plugin.toml 被修改或签名不匹配）".to_string())?;
        Ok(SignatureStatus::Verified {
            key_id: public_key_id(key),
        })
    }

    /// UI 用：把任何错误折叠成 `Invalid/Unverifiable`，不 panic、不上报安装成功。
    pub fn inspect_manifest(
        &self,
        manifest: &[u8],
        signature_file: Option<&[u8]>,
    ) -> SignatureStatus {
        match self.verify_manifest(manifest, signature_file) {
            Ok(status) => status,
            Err(reason) => {
                if signature_file.is_some() && self.key.is_none() {
                    SignatureStatus::Unverifiable { reason }
                } else {
                    SignatureStatus::Invalid { reason }
                }
            }
        }
    }
}

/// 读取已安装插件目录的签名状态（不做安装，不执行任何代码）。
pub fn inspect_plugin_dir(dir: &Path) -> SignatureStatus {
    let manifest_path = dir.join("plugin.toml");
    let manifest = match read_limited_file(
        &manifest_path,
        crate::plugin_v3::manifest::MAX_MANIFEST_BYTES as u64,
    ) {
        Ok(bytes) => bytes,
        Err(error) => return SignatureStatus::Invalid { reason: error },
    };
    let sig_path = dir.join(SIG_FILE_NAME);
    let sig = if sig_path.is_file() {
        match read_limited_file(&sig_path, MAX_SIG_FILE_BYTES) {
            Ok(bytes) => Some(bytes),
            Err(error) => return SignatureStatus::Invalid { reason: error },
        }
    } else {
        None
    };
    let plugins_dir = dir.parent().unwrap_or_else(|| Path::new("."));
    let config = match SignatureConfig::resolve(plugins_dir, false, None) {
        Ok(config) => config,
        Err(error) => return SignatureStatus::Invalid { reason: error },
    };
    config.inspect_manifest(&manifest, sig.as_deref())
}

/// 默认公钥路径：`<plugins_dir 的父目录>/plugin-signing.pub`。
pub fn default_pubkey_path(plugins_dir: &Path) -> PathBuf {
    plugins_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(DEFAULT_PUBKEY_FILE)
}

/// 读默认私钥路径（CLI `plugins sign` 默认使用）。
pub fn default_signing_key_path(plugins_dir: &Path) -> PathBuf {
    plugins_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(DEFAULT_SIGNING_KEY_FILE)
}

fn env_requires_signature() -> Result<bool, String> {
    let Ok(raw) = std::env::var(REQUIRE_SIGNATURE_ENV) else {
        return Ok(false);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        other => Err(format!(
            "{REQUIRE_SIGNATURE_ENV} 只接受 1/0/true/false/yes/no/on/off（当前 {other:?}）"
        )),
    }
}

/// 环境变量的值：先按内容解析；如果它像一个存在的文件路径，则读文件。
/// 显式 `@path` 强制按文件读取。
fn load_key_value(value: &str) -> Result<String, String> {
    let value = value.trim();
    if let Some(path) = value.strip_prefix('@') {
        return read_key_file(Path::new(path.trim()));
    }
    // base64 里经常含 '/'，不能只凭 unix 路径外观判断“这是文件”。
    // 先当作内联公钥解析；失败且像路径时才读文件。
    match parse_public_key(value) {
        Ok(_) => Ok(value.to_string()),
        Err(inline_error) => {
            let candidate = Path::new(value);
            if candidate.is_file() {
                read_key_file(candidate)
            } else if value.contains('/') || value.contains('\\') {
                Err(format!(
                    "{PUBKEY_ENV} 既不是合法公钥，也不是可读文件 {}（{inline_error}）",
                    candidate.display()
                ))
            } else {
                Ok(value.to_string())
            }
        }
    }
}

fn read_key_file(path: &Path) -> Result<String, String> {
    let bytes = read_limited_file(path, MAX_KEY_FILE_BYTES)?;
    String::from_utf8(bytes).map_err(|_| format!("公钥文件不是 UTF-8: {}", path.display()))
}

/// 解析公钥文本：格式头 + base64 / 裸 base64 / 64 位 hex / PEM SPKI。
pub fn parse_public_key(text: &str) -> Result<VerifyingKey, String> {
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return Err("公钥为空".into());
    }
    if text.contains("-----BEGIN") {
        return parse_pem_public_key(text);
    }
    let payload = extract_header_payload(text, PUBKEY_HEADER)?;
    let raw = decode_key_bytes(&payload)?;
    VerifyingKey::from_bytes(&raw).map_err(|e| format!("不是合法 Ed25519 公钥: {e}"))
}

fn parse_pem_public_key(text: &str) -> Result<VerifyingKey, String> {
    let begin = "-----BEGIN PUBLIC KEY-----";
    let end = "-----END PUBLIC KEY-----";
    let start = text.find(begin).ok_or("PEM 缺少 BEGIN PUBLIC KEY")? + begin.len();
    let tail = &text[start..];
    let finish = tail.find(end).ok_or("PEM 缺少 END PUBLIC KEY")?;
    let body: String = tail[..finish].split_whitespace().collect();
    let der = STANDARD
        .decode(body.as_bytes())
        .map_err(|e| format!("PEM base64 无效: {e}"))?;
    // Ed25519 SubjectPublicKeyInfo 固定前缀 + 32 字节 key。
    const SPKI_PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let raw: [u8; 32] = if der.len() == 44 && der.starts_with(&SPKI_PREFIX) {
        der[12..].try_into().expect("32 bytes")
    } else if der.len() == 32 {
        der.as_slice().try_into().expect("32 bytes")
    } else {
        return Err(format!(
            "PEM 公钥不是 Ed25519 SubjectPublicKeyInfo（DER {} 字节）",
            der.len()
        ));
    };
    VerifyingKey::from_bytes(&raw).map_err(|e| format!("不是合法 Ed25519 公钥: {e}"))
}

fn extract_header_payload<'a>(text: &'a str, header: &str) -> Result<String, String> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(first) = lines.next() else {
        return Err("公钥内容为空".into());
    };
    if first == header {
        let payload = lines
            .next()
            .ok_or_else(|| format!("{header} 缺少 base64 内容"))?;
        if lines.next().is_some() {
            return Err("公钥文件在 base64 之后还有多余内容".into());
        }
        Ok(payload.to_string())
    } else if first.contains(' ') {
        Err(format!(
            "公钥格式无法识别（期望 {header} 或裸 base64/hex/PEM）"
        ))
    } else {
        // 允许裸 token；但多行裸内容也拒绝。
        if lines.next().is_some() {
            return Err("公钥文件有多行但缺少格式头".into());
        }
        Ok(first.to_string())
    }
}

fn decode_key_bytes(payload: &str) -> Result<[u8; 32], String> {
    let compact: String = payload.split_whitespace().collect();
    if compact.len() == 64 && compact.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut out = [0u8; 32];
        for (index, chunk) in compact.as_bytes().chunks(2).enumerate() {
            let text = std::str::from_utf8(chunk).map_err(|_| "hex 公钥不是 UTF-8")?;
            out[index] =
                u8::from_str_radix(text, 16).map_err(|_| format!("hex 公钥无效: {text:?}"))?;
        }
        return Ok(out);
    }
    let bytes = STANDARD
        .decode(compact.as_bytes())
        .map_err(|e| format!("公钥 base64 无效: {e}"))?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| format!("Ed25519 公钥必须 32 字节（当前 {}）", bytes.len()))
}

/// 解析 `plugin.toml.sig`。
pub fn parse_signature_file(bytes: &[u8]) -> Result<Signature, String> {
    if bytes.len() as u64 > MAX_SIG_FILE_BYTES {
        return Err(format!(
            "签名文件过大（{} 字节，上限 {MAX_SIG_FILE_BYTES}）",
            bytes.len()
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "签名文件不是 UTF-8".to_string())?;
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let header = lines.next().ok_or("签名文件为空")?;
    if header != SIG_HEADER {
        return Err(format!(
            "签名文件格式头无效（期望 {SIG_HEADER}，当前 {header:?}）"
        ));
    }
    let payload = lines.next().ok_or("签名文件缺少 base64 内容")?;
    if lines.next().is_some() {
        return Err("签名文件在 base64 之后还有多余内容".into());
    }
    let raw = STANDARD
        .decode(payload.as_bytes())
        .map_err(|e| format!("签名 base64 无效: {e}"))?;
    let raw: [u8; 64] = raw
        .try_into()
        .map_err(|raw: Vec<u8>| format!("Ed25519 签名必须 64 字节（当前 {}）", raw.len()))?;
    Ok(Signature::from_bytes(&raw))
}

/// 生成签名文件文本（`xtervo plugins sign` 与测试共用）。
pub fn signature_file_text(signature: &Signature) -> String {
    format!("{SIG_HEADER}\n{}\n", STANDARD.encode(signature.to_bytes()))
}

/// 生成公钥文件文本（`xtervo plugins keygen` 使用；也接受裸 base64）。
pub fn public_key_file_text(key: &VerifyingKey) -> String {
    format!("{PUBKEY_HEADER}\n{}\n", STANDARD.encode(key.to_bytes()))
}

/// 生成私钥文件文本（只写权限 0600 的文件）。
pub fn signing_key_file_text(key: &SigningKey) -> String {
    format!(
        "{SIGNING_KEY_HEADER}\n{}\n",
        STANDARD.encode(key.to_bytes())
    )
}

/// 用私钥签 `plugin.toml` 原始字节。
pub fn sign_manifest(manifest: &[u8], key: &SigningKey) -> String {
    signature_file_text(&key.sign(manifest))
}

/// 读取私钥文件。
pub fn load_signing_key(path: &Path) -> Result<SigningKey, String> {
    let bytes = read_limited_file(path, MAX_KEY_FILE_BYTES)?;
    let text =
        String::from_utf8(bytes).map_err(|_| format!("私钥文件不是 UTF-8: {}", path.display()))?;
    let text = text.trim().trim_start_matches('\u{feff}');
    let payload = if text.starts_with(SIGNING_KEY_HEADER) {
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        let _ = lines.next();
        let payload = lines.next().ok_or("私钥文件缺少 base64 内容")?;
        if lines.next().is_some() {
            return Err("私钥文件在 base64 之后还有多余内容".into());
        }
        payload.to_string()
    } else {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let raw = STANDARD
        .decode(payload.as_bytes())
        .map_err(|e| format!("私钥 base64 无效: {e}"))?;
    let raw: [u8; 32] = raw
        .try_into()
        .map_err(|raw: Vec<u8>| format!("Ed25519 私钥 seed 必须 32 字节（当前 {}）", raw.len()))?;
    Ok(SigningKey::from_bytes(&raw))
}

/// 公钥指纹：SHA-256 前 16 个 hex 字符。
pub fn public_key_id(key: &VerifyingKey) -> String {
    let digest = Sha256::digest(key.to_bytes());
    hex_prefix(&digest, 8)
}

fn hex_prefix(bytes: &[u8], count: usize) -> String {
    let mut out = String::with_capacity(count * 2);
    for byte in bytes.iter().take(count) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 带大小上限的文件读取（先看元数据，再在读时截断，防止“元数据撒谎”）。
pub fn read_limited_file(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("读取文件元数据失败 {}: {e}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("拒绝符号链接: {}", path.display()));
    }
    if !metadata.is_file() {
        return Err(format!("不是普通文件: {}", path.display()));
    }
    if metadata.len() > max {
        return Err(format!(
            "文件过大 {}（{} 字节，上限 {max}）",
            path.display(),
            metadata.len()
        ));
    }
    let file =
        std::fs::File::open(path).map_err(|e| format!("打开文件失败 {}: {e}", path.display()))?;
    let mut out = Vec::with_capacity(metadata.len() as usize);
    file.take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("读取文件失败 {}: {e}", path.display()))?;
    if out.len() as u64 > max {
        return Err(format!("文件过大 {}（读取超过上限 {max}）", path.display()));
    }
    Ok(out)
}
