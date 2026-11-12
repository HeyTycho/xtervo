// SPDX-License-Identifier: GPL-3.0-only
//! Plugin API v3 本地安装 / 更新 / 回滚（不执行插件代码，不做市场）。
//!
//! 流程固定：
//!
//! ```text
//! 本地目录 / zip / tar(.gz)
//!   -> canonicalize 源
//!   -> 解到 <plugins>/.staging/<unique>（拒绝 ..、绝对路径、symlink、特殊文件）
//!   -> 校验 manifest（api=3/host_abi=3/id/version/entry）+ Ed25519 签名
//!   -> 写 <staging>/.tervo-install.toml
//!   -> Linux renameat2(RENAME_EXCHANGE) 原子交换；不支持时两段 rename 并可恢复
//!   -> 旧版本移到 <plugins>/.backup/<id>/<epoch-ms>-<version>
//!   -> 只保留最近 N 个备份（默认 3）
//! ```
//!
//! 安全边界：只复制文件；不 spawn、不 source、不执行任何插件代码；解压目标
//! 固定为 staging，所有路径先规范化再写；安装过程不触碰插件目录之外；
//! 权限确认仍由 v3 host 在启用时负责。安装器不是 OS 沙箱。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::manifest::{self, Manifest};
use super::signature::{self, SignatureConfig, SignatureStatus};

/// 默认保留的旧版本数量（更新/回滚共用）。
pub const DEFAULT_BACKUP_KEEP: usize = 3;
/// 备份数量硬上限，避免 CLI `--keep` 把磁盘吃满。
pub const MAX_BACKUP_KEEP: usize = 10;
/// staging 目录名（位于插件根目录下；发现插件时会跳过点目录）。
pub const STAGING_DIR: &str = ".staging";
/// 备份目录名。
pub const BACKUP_DIR: &str = ".backup";
/// 安装元数据文件名（放在每个插件目录内，签名文件只覆盖 plugin.toml）。
pub const META_FILE: &str = ".tervo-install.toml";
/// 安装锁文件名。
pub const LOCK_FILE: &str = ".install.lock";

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 安装/解包限制。测试可以缩小这些值来验证拒绝路径。
#[derive(Clone, Debug)]
pub struct InstallLimits {
    pub max_files: usize,
    pub max_entries: usize,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_depth: usize,
    pub max_path_bytes: usize,
}

impl Default for InstallLimits {
    fn default() -> Self {
        Self {
            // 一个插件通常只有脚本 + 主题 + 资源；上限留足但拒绝“无限包”。
            max_files: 2_048,
            max_entries: 4_096,
            max_total_bytes: 64 * 1024 * 1024,
            max_file_bytes: 16 * 1024 * 1024,
            max_depth: 16,
            max_path_bytes: 512,
        }
    }
}

/// 安装选项。`pubkey` 显式覆盖环境变量，测试用它避免并发修改进程环境。
#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub require_signature: bool,
    pub pubkey: Option<String>,
    pub backup_keep: usize,
    pub update: bool,
    pub force: bool,
    pub limits: InstallLimits,
    /// 测试/嵌入方的显式签名策略；None = 按环境变量与默认公钥文件解析。
    pub signature_config: Option<SignatureConfig>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            require_signature: false,
            pubkey: None,
            backup_keep: DEFAULT_BACKUP_KEEP,
            update: false,
            force: false,
            limits: InstallLimits::default(),
            signature_config: None,
        }
    }
}

/// 回滚选项。
#[derive(Clone, Debug)]
pub struct RollbackOptions {
    pub backup_keep: usize,
    pub require_signature: bool,
    pub pubkey: Option<String>,
    /// 测试/嵌入方的显式签名策略；None = 按环境变量与默认公钥文件解析。
    pub signature_config: Option<SignatureConfig>,
}

impl Default for RollbackOptions {
    fn default() -> Self {
        Self {
            backup_keep: DEFAULT_BACKUP_KEEP,
            require_signature: false,
            pubkey: None,
            signature_config: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Dir,
    Zip,
    Tar,
    TarGz,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Dir => "dir",
            SourceKind::Zip => "zip",
            SourceKind::Tar => "tar",
            SourceKind::TarGz => "tar.gz",
        }
    }
}

/// 成功安装/更新后的报告。
#[derive(Clone, Debug)]
pub struct InstallReport {
    pub id: String,
    pub name: String,
    pub version: String,
    pub previous_version: Option<String>,
    pub source: PathBuf,
    pub source_kind: SourceKind,
    pub signature: SignatureStatus,
    pub plugin_dir: PathBuf,
    pub backup_dir: Option<PathBuf>,
    pub files: u64,
    pub bytes: u64,
    pub warnings: Vec<String>,
}

/// 回滚报告。
#[derive(Clone, Debug)]
pub struct RollbackReport {
    pub id: String,
    pub from_version: String,
    pub to_version: String,
    pub plugin_dir: PathBuf,
    pub backup_dir: PathBuf,
    pub warnings: Vec<String>,
}

/// 只校验、不安装时返回的包信息。
#[derive(Clone, Debug)]
pub struct PackageInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: PathBuf,
    pub source_kind: SourceKind,
    pub signature: SignatureStatus,
    pub entry: String,
    pub files: u64,
    pub bytes: u64,
}

/// 备份条目。`dir_name` 是 `.backup/<id>/` 下的目录名。
#[derive(Clone, Debug)]
pub struct BackupInfo {
    pub path: PathBuf,
    pub dir_name: String,
    pub version: String,
    pub installed_at_unix: Option<u64>,
}

/// 安装元数据；只用于设置页展示（source/signature/version），不作为信任依据。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub source_kind: String,
    pub installed_at_unix: u64,
    /// "verified" | "unsigned"（安装器拒绝的包不会落盘）。
    pub signature: String,
    pub signature_key_id: String,
    pub signature_required: bool,
    pub files: u64,
    pub bytes: u64,
}

impl InstallMeta {
    pub fn signature_label(&self) -> String {
        if self.signature == "verified" {
            if self.signature_key_id.is_empty() {
                "verified · ed25519".to_string()
            } else {
                format!("verified · ed25519 · key {}", self.signature_key_id)
            }
        } else if self.signature == "unsigned" {
            if self.signature_required {
                "unsigned (required but installed before requirement?)".to_string()
            } else {
                "unsigned".to_string()
            }
        } else {
            self.signature.clone()
        }
    }
}

/// 读取插件目录里的 `.tervo-install.toml`；不存在/损坏返回 None。
pub fn installed_meta(dir: &Path) -> Option<InstallMeta> {
    let bytes = signature::read_limited_file(&dir.join(META_FILE), 64 * 1024).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    toml::from_str(text).ok()
}

/// 安装（`update = false`）或更新（`update = true`）。
pub fn install_package(
    plugins_dir: &Path,
    source: &Path,
    options: &InstallOptions,
) -> Result<InstallReport, String> {
    install_inner(plugins_dir, source, options)
}

/// 更新：要求目标 `<id>` 已存在。版本相同按重装处理（仍会备份旧目录）。
pub fn update_package(
    plugins_dir: &Path,
    source: &Path,
    options: &InstallOptions,
) -> Result<InstallReport, String> {
    let mut options = options.clone();
    options.update = true;
    install_inner(plugins_dir, source, &options)
}

fn install_inner(
    plugins_dir: &Path,
    source: &Path,
    options: &InstallOptions,
) -> Result<InstallReport, String> {
    let plugins_dir = prepare_plugins_root(plugins_dir)?;
    let _lock = InstallLock::acquire(&plugins_dir)?;
    let source = canonical_source(source)?;
    let source_kind = detect_source(&source)?;
    let staging_root = ensure_private_dir(&plugins_dir.join(STAGING_DIR))?;
    let staging = unique_child(&staging_root, "pkg")?;
    let mut guard = StagingGuard::new(staging.clone());
    let counts = extract_source(&source, source_kind, &staging, &options.limits)?;
    normalize_package_root(&staging)?;
    ensure_tree_is_regular(&staging, &options.limits)?;
    verify_staging_root(&staging, &plugins_dir)?;

    let manifest_bytes = signature::read_limited_file(
        &staging.join("plugin.toml"),
        manifest::MAX_MANIFEST_BYTES as u64,
    )?;
    let manifest = parse_and_validate_manifest(&manifest_bytes)?;
    // entry 必须是插件目录内的可执行普通文件；这里只查元数据，不执行。
    manifest::resolve_entry(&staging, &manifest.entry)?;

    let sig_path = staging.join(signature::SIG_FILE_NAME);
    let sig_bytes = if sig_path.is_file() {
        Some(signature::read_limited_file(
            &sig_path,
            signature::MAX_SIG_FILE_BYTES,
        )?)
    } else {
        None
    };
    let signature_config = match &options.signature_config {
        Some(config) => config.clone(),
        None => SignatureConfig::resolve(
            &plugins_dir,
            options.require_signature,
            options.pubkey.as_deref(),
        )?,
    };
    let signature = signature_config.verify_manifest(&manifest_bytes, sig_bytes.as_deref())?;

    let target = plugins_dir.join(&manifest.id);
    let existing = inspect_existing_target(&target)?;
    let previous_version = if let Some(existing) = &existing {
        if options.update {
            let old = read_manifest_from_dir(existing)?;
            if old.id != manifest.id {
                return Err(format!(
                    "update id 不一致：已安装 {}，新包 {}（拒绝覆盖）",
                    old.id, manifest.id
                ));
            }
        } else if !options.force {
            if let Ok(old) = read_manifest_from_dir(existing) {
                if old.id != manifest.id {
                    return Err(format!(
                        "目标目录 {} 已属于插件 {}，新包 id 是 {}；使用 --force 才会备份并覆盖",
                        target.display(),
                        old.id,
                        manifest.id
                    ));
                }
            } else if !options.force {
                return Err(format!(
                    "目标目录 {} 已存在但缺少可解析的 plugin.toml；使用 --force 才会备份并覆盖",
                    target.display()
                ));
            }
        }
        read_version_from_dir(existing)
    } else {
        if options.update {
            return Err(format!(
                "update 需要已安装的插件 id {}（目录 {} 不存在）；首次安装请用 install",
                manifest.id,
                target.display()
            ));
        }
        None
    };

    let installed_at_unix = unix_secs();
    let meta = InstallMeta {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        source: source.display().to_string(),
        source_kind: source_kind.as_str().to_string(),
        installed_at_unix,
        signature: if signature.verified() {
            "verified".to_string()
        } else {
            "unsigned".to_string()
        },
        signature_key_id: match &signature {
            SignatureStatus::Verified { key_id } => key_id.clone(),
            _ => String::new(),
        },
        signature_required: signature_config.required,
        files: counts.files,
        bytes: counts.bytes,
    };
    write_meta(&staging, &meta)?;

    let _backup_parent = ensure_private_dir(&plugins_dir.join(BACKUP_DIR))?;
    let backup_root = ensure_private_dir(&plugins_dir.join(BACKUP_DIR).join(&manifest.id))?;
    let mut warnings = Vec::new();
    let mut backup_dir = None;

    if let Some(existing) = existing.as_ref() {
        let backup_version = previous_version.as_deref().unwrap_or(&manifest.version);
        let backup = unique_backup_path(&backup_root, backup_version)?;
        match exchange_paths(&staging, existing)? {
            true => {
                // staging 现在持有旧版本；target 已经是新版本。
                guard.disarm();
                match fs::rename(&staging, &backup) {
                    Ok(()) => backup_dir = Some(backup),
                    Err(error) => {
                        // 新版本已原子就位，旧版本仍完整保留在 staging；不能删。
                        warnings.push(format!(
                            "新版本已安装，但旧版本移动到 {} 失败: {error}；旧版本仍在 {}（不会被删除）",
                            backup.display(),
                            staging.display()
                        ));
                        backup_dir = Some(staging.clone());
                    }
                }
            }
            false => {
                // 不支持 RENAME_EXCHANGE 的文件系统：两段 rename，第二段失败时恢复。
                fs::rename(existing, &backup)
                    .map_err(|e| format!("备份旧版本失败 {}: {e}", existing.display()))?;
                match fs::rename(&staging, existing) {
                    Ok(()) => {
                        guard.disarm();
                        backup_dir = Some(backup);
                    }
                    Err(error) => {
                        if let Err(restore_error) = fs::rename(&backup, existing) {
                            return Err(format!(
                                "替换插件失败: {error}；恢复旧版本也失败: {restore_error}；旧版本完整保留在 {}",
                                backup.display()
                            ));
                        }
                        return Err(format!(
                            "替换插件失败: {error}；已恢复旧版本，未改动已安装内容"
                        ));
                    }
                }
            }
        }
    } else {
        fs::rename(&staging, &target)
            .map_err(|e| format!("安装到 {} 失败: {e}", target.display()))?;
        guard.disarm();
    }

    // 新插件没有旧备份目录时也创建（rollback 列举用），失败不算安装失败。
    let _ = fs::create_dir_all(&backup_root);
    if let Err(error) = prune_backups(&backup_root, options.backup_keep) {
        warnings.push(error);
    }
    sync_dir_best_effort(&plugins_dir);

    Ok(InstallReport {
        id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        previous_version,
        source,
        source_kind,
        signature,
        plugin_dir: target,
        backup_dir,
        files: counts.files,
        bytes: counts.bytes,
        warnings,
    })
}

/// 只校验包（解到临时 staging 后清理），不安装、不写插件目录。
pub fn inspect_package(
    plugins_dir: &Path,
    source: &Path,
    options: &InstallOptions,
) -> Result<PackageInfo, String> {
    let plugins_dir = prepare_plugins_root(plugins_dir)?;
    let source = canonical_source(source)?;
    let source_kind = detect_source(&source)?;
    let staging_root = ensure_private_dir(&plugins_dir.join(STAGING_DIR))?;
    let staging = unique_child(&staging_root, "verify")?;
    let mut guard = StagingGuard::new(staging.clone());
    let counts = extract_source(&source, source_kind, &staging, &options.limits)?;
    normalize_package_root(&staging)?;
    ensure_tree_is_regular(&staging, &options.limits)?;
    verify_staging_root(&staging, &plugins_dir)?;
    let manifest_bytes = signature::read_limited_file(
        &staging.join("plugin.toml"),
        manifest::MAX_MANIFEST_BYTES as u64,
    )?;
    let manifest = parse_and_validate_manifest(&manifest_bytes)?;
    manifest::resolve_entry(&staging, &manifest.entry)?;
    let sig_path = staging.join(signature::SIG_FILE_NAME);
    let sig_bytes = if sig_path.is_file() {
        Some(signature::read_limited_file(
            &sig_path,
            signature::MAX_SIG_FILE_BYTES,
        )?)
    } else {
        None
    };
    let config = match &options.signature_config {
        Some(config) => config.clone(),
        None => SignatureConfig::resolve(
            &plugins_dir,
            options.require_signature,
            options.pubkey.as_deref(),
        )?,
    };
    let signature = config.verify_manifest(&manifest_bytes, sig_bytes.as_deref())?;
    guard.cleanup();
    Ok(PackageInfo {
        id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        source,
        source_kind,
        signature,
        entry: manifest.entry,
        files: counts.files,
        bytes: counts.bytes,
    })
}

/// 回滚到 `.backup/<id>/` 中的旧版本。默认选最新的、且版本与当前不同的备份。
pub fn rollback_package(
    plugins_dir: &Path,
    id: &str,
    to: Option<&str>,
    options: &RollbackOptions,
) -> Result<RollbackReport, String> {
    manifest::validate_id(id, "插件 id")?;
    let plugins_dir = prepare_plugins_root(plugins_dir)?;
    let _lock = InstallLock::acquire(&plugins_dir)?;
    let target = plugins_dir.join(id);
    let existing = inspect_existing_target(&target)?
        .ok_or_else(|| format!("插件 {id} 未安装（{} 不存在）", target.display()))?;
    let current_manifest = read_manifest_from_dir(&existing)?;
    let from_version = current_manifest.version.clone();
    let backups = list_backups(&plugins_dir, id);
    if backups.is_empty() {
        return Err(format!(
            "插件 {id} 没有可回滚的旧版本（{} 为空）",
            backup_root(&plugins_dir, id).display()
        ));
    }
    let selected = match to.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => {
            if value.contains('/') || value.contains('\\') || value == ".." {
                return Err(format!(
                    "回滚目标必须是备份目录名或版本号，不能是路径: {value:?}"
                ));
            }
            backups
                .iter()
                .find(|backup| backup.dir_name == value)
                .or_else(|| backups.iter().find(|backup| backup.version == value))
                .ok_or_else(|| format!("找不到备份 {value:?}（可用: {}）", backup_names(&backups)))?
                .clone()
        }
        None => backups
            .iter()
            .find(|backup| backup.version != from_version)
            .ok_or_else(|| {
                format!(
                    "插件 {id} 只有版本 {from_version} 的备份；没有其它可回滚版本（{}）",
                    backup_names(&backups)
                )
            })?
            .clone(),
    };

    let signature_config = match &options.signature_config {
        Some(config) => config.clone(),
        None => SignatureConfig::resolve(
            &plugins_dir,
            options.require_signature,
            options.pubkey.as_deref(),
        )?,
    };
    let selected_manifest = validate_installed_dir(&selected.path, &signature_config)?;
    let to_version = selected_manifest.version.clone();
    let backup_root = backup_root(&plugins_dir, id);
    let new_backup = unique_backup_path(&backup_root, &from_version)?;
    let mut warnings = Vec::new();

    match exchange_paths(&target, &selected.path)? {
        true => {
            // 原子交换后 target = 旧版本，selected.path = 当前版本；把当前版本归档。
            if let Err(error) = fs::rename(&selected.path, &new_backup) {
                warnings.push(format!(
                    "回滚已生效，但把回滚前版本归档到 {} 失败: {error}；目录仍在 {}",
                    new_backup.display(),
                    selected.path.display()
                ));
            }
        }
        false => {
            fs::rename(&target, &new_backup)
                .map_err(|e| format!("备份当前版本失败 {}: {e}", target.display()))?;
            if let Err(error) = fs::rename(&selected.path, &target) {
                let restore = fs::rename(&new_backup, &target);
                return Err(match restore {
                    Ok(()) => format!("回滚 rename 失败: {error}；已恢复当前版本"),
                    Err(restore_error) => format!(
                        "回滚 rename 失败: {error}；恢复当前版本失败: {restore_error}；当前版本完整保留在 {}",
                        new_backup.display()
                    ),
                });
            }
        }
    }
    if let Err(error) = prune_backups(&backup_root, options.backup_keep) {
        warnings.push(error);
    }
    sync_dir_best_effort(&plugins_dir);
    Ok(RollbackReport {
        id: id.to_string(),
        from_version,
        to_version,
        plugin_dir: target,
        backup_dir: new_backup,
        warnings,
    })
}

/// 列出某插件的备份，按目录名（epoch-ms 前缀）从新到旧。
pub fn list_backups(plugins_dir: &Path, id: &str) -> Vec<BackupInfo> {
    if manifest::validate_id(id, "id").is_err() {
        return Vec::new();
    }
    let root = backup_root(plugins_dir, id);
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut backups = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Some(dir_name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if !dir_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            continue;
        }
        let meta = installed_meta(&path);
        let version = read_version_from_dir(&path)
            .or_else(|| meta.as_ref().map(|meta| meta.version.clone()))
            .unwrap_or_else(|| version_part_of_backup_name(&dir_name));
        backups.push(BackupInfo {
            path,
            dir_name,
            version,
            installed_at_unix: meta.map(|meta| meta.installed_at_unix),
        });
    }
    backups.sort_by(|a, b| b.dir_name.cmp(&a.dir_name));
    backups
}

fn backup_names(backups: &[BackupInfo]) -> String {
    if backups.is_empty() {
        "无".to_string()
    } else {
        backups
            .iter()
            .map(|backup| format!("{} ({})", backup.version, backup.dir_name))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn backup_root(plugins_dir: &Path, id: &str) -> PathBuf {
    plugins_dir.join(BACKUP_DIR).join(id)
}

fn validate_installed_dir(
    dir: &Path,
    signature_config: &SignatureConfig,
) -> Result<Manifest, String> {
    let manifest_bytes = signature::read_limited_file(
        &dir.join("plugin.toml"),
        manifest::MAX_MANIFEST_BYTES as u64,
    )?;
    let manifest = parse_and_validate_manifest(&manifest_bytes)?;
    manifest::resolve_entry(dir, &manifest.entry)?;
    let sig_path = dir.join(signature::SIG_FILE_NAME);
    let sig_bytes = if sig_path.is_file() {
        Some(signature::read_limited_file(
            &sig_path,
            signature::MAX_SIG_FILE_BYTES,
        )?)
    } else {
        None
    };
    signature_config.verify_manifest(&manifest_bytes, sig_bytes.as_deref())?;
    Ok(manifest)
}

fn parse_and_validate_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "plugin.toml 不是 UTF-8".to_string())?;
    let manifest: Manifest =
        toml::from_str(text).map_err(|e| format!("v3 plugin.toml 解析失败: {e}"))?;
    manifest.validate()?;
    Ok(manifest)
}

fn read_manifest_from_dir(dir: &Path) -> Result<Manifest, String> {
    let bytes = signature::read_limited_file(
        &dir.join("plugin.toml"),
        manifest::MAX_MANIFEST_BYTES as u64,
    )?;
    parse_and_validate_manifest(&bytes)
}

fn read_version_from_dir(dir: &Path) -> Option<String> {
    installed_meta(dir)
        .map(|meta| meta.version)
        .filter(|version| !version.is_empty())
        .or_else(|| {
            read_manifest_from_dir(dir)
                .ok()
                .map(|manifest| manifest.version)
        })
}

fn write_meta(staging: &Path, meta: &InstallMeta) -> Result<(), String> {
    let text = toml::to_string_pretty(meta).map_err(|e| format!("安装元数据序列化失败: {e}"))?;
    let path = staging.join(META_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("写安装元数据失败 {}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("写安装元数据失败 {}: {e}", path.display()))?;
    Ok(())
}

fn inspect_existing_target(target: &Path) -> Result<Option<PathBuf>, String> {
    match fs::symlink_metadata(target) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "目标 {} 是符号链接；拒绝覆盖（先手动移走）",
                    target.display()
                ));
            }
            if !metadata.is_dir() {
                return Err(format!("目标 {} 已存在但不是目录", target.display()));
            }
            Ok(Some(target.to_path_buf()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("检查目标 {} 失败: {error}", target.display())),
    }
}

fn prepare_plugins_root(plugins_dir: &Path) -> Result<PathBuf, String> {
    if plugins_dir.as_os_str().is_empty() {
        return Err("插件目录为空".into());
    }
    match fs::symlink_metadata(plugins_dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(format!("插件目录不是目录: {}", plugins_dir.display()));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(plugins_dir)
                .map_err(|e| format!("创建插件目录失败 {}: {e}", plugins_dir.display()))?;
        }
        Err(error) => {
            return Err(format!(
                "读取插件目录失败 {}: {error}",
                plugins_dir.display()
            ))
        }
    }
    let canonical = fs::canonicalize(plugins_dir)
        .map_err(|e| format!("插件目录 canonicalize 失败 {}: {e}", plugins_dir.display()))?;
    if !canonical.is_dir() {
        return Err(format!("插件目录不是目录: {}", canonical.display()));
    }
    Ok(canonical)
}

fn ensure_private_dir(path: &Path) -> Result<PathBuf, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "{} 必须是普通目录（不是符号链接/文件）",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(path)
                .map_err(|e| format!("创建目录失败 {}: {e}", path.display()))?;
        }
        Err(error) => return Err(format!("检查目录失败 {}: {error}", path.display())),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Ok(path.to_path_buf())
}

fn canonical_source(source: &Path) -> Result<PathBuf, String> {
    if source.as_os_str().is_empty() {
        return Err("安装源为空".into());
    }
    let canonical =
        fs::canonicalize(source).map_err(|e| format!("安装源不可用 {}: {e}", source.display()))?;
    let metadata = fs::metadata(&canonical)
        .map_err(|e| format!("读取安装源失败 {}: {e}", canonical.display()))?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(format!(
            "安装源必须是目录或 zip/tar 包: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn detect_source(source: &Path) -> Result<SourceKind, String> {
    let metadata =
        fs::metadata(source).map_err(|e| format!("读取安装源失败 {}: {e}", source.display()))?;
    if metadata.is_dir() {
        return Ok(SourceKind::Dir);
    }
    if !metadata.is_file() {
        return Err(format!("安装源不是普通文件/目录: {}", source.display()));
    }
    let mut file =
        File::open(source).map_err(|e| format!("打开安装包失败 {}: {e}", source.display()))?;
    let mut head = [0u8; 512];
    let read = file
        .read(&mut head)
        .map_err(|e| format!("读取安装包失败 {}: {e}", source.display()))?;
    let head = &head[..read];
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok(SourceKind::Zip);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return Ok(SourceKind::TarGz);
    }
    if head.len() >= 262 && &head[257..262] == b"ustar" {
        return Ok(SourceKind::Tar);
    }
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if name.ends_with(".zip") {
        Ok(SourceKind::Zip)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(SourceKind::TarGz)
    } else if name.ends_with(".tar") {
        Ok(SourceKind::Tar)
    } else {
        Err(format!(
            "无法识别安装包格式（只支持目录、zip、tar、tar.gz）: {}",
            source.display()
        ))
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    files: u64,
    bytes: u64,
    entries: usize,
}

struct ExtractState {
    limits: InstallLimits,
    files: HashSet<PathBuf>,
    dirs: HashSet<PathBuf>,
    counts: Counts,
}

impl ExtractState {
    fn new(limits: InstallLimits) -> Self {
        Self {
            limits,
            files: HashSet::new(),
            dirs: HashSet::new(),
            counts: Counts::default(),
        }
    }

    fn register(&mut self, rel: &Path, is_dir: bool) -> Result<(), String> {
        if rel.as_os_str().is_empty() {
            return Ok(());
        }
        let path_len = rel.to_string_lossy().len();
        if path_len > self.limits.max_path_bytes {
            return Err(format!(
                "包内路径过长（{path_len} 字节，上限 {}）: {}",
                self.limits.max_path_bytes,
                rel.display()
            ));
        }
        let depth = rel.components().count();
        if depth > self.limits.max_depth {
            return Err(format!(
                "包内路径层级过深（{depth}，上限 {}）",
                self.limits.max_depth
            ));
        }
        if self.files.contains(rel) {
            return Err(format!("包内重复文件路径: {}", rel.display()));
        }
        for ancestor in rel.ancestors().skip(1) {
            if !ancestor.as_os_str().is_empty() && self.files.contains(ancestor) {
                return Err(format!(
                    "包内路径冲突：{} 既是文件又是目录（{}）",
                    ancestor.display(),
                    rel.display()
                ));
            }
        }
        if is_dir {
            if self.files.contains(rel) {
                return Err(format!("包内路径冲突：{} 既是文件又是目录", rel.display()));
            }
            if self.dirs.contains(rel) {
                return Ok(());
            }
            self.counts.entries += 1;
            if self.counts.entries > self.limits.max_entries {
                return Err(format!(
                    "包内条目过多（超过 {} 个）",
                    self.limits.max_entries
                ));
            }
            self.dirs.insert(rel.to_path_buf());
        } else {
            if self.dirs.contains(rel) {
                return Err(format!("包内路径冲突：{} 既是目录又是文件", rel.display()));
            }
            self.counts.entries += 1;
            if self.counts.entries > self.limits.max_entries {
                return Err(format!(
                    "包内条目过多（超过 {} 个）",
                    self.limits.max_entries
                ));
            }
            self.counts.files += 1;
            if self.counts.files as usize > self.limits.max_files {
                return Err(format!(
                    "包内文件过多（{}，上限 {}）",
                    self.counts.files, self.limits.max_files
                ));
            }
            self.files.insert(rel.to_path_buf());
        }
        Ok(())
    }
}

fn extract_source(
    source: &Path,
    kind: SourceKind,
    staging: &Path,
    limits: &InstallLimits,
) -> Result<Counts, String> {
    let mut state = ExtractState::new(limits.clone());
    match kind {
        SourceKind::Dir => extract_dir(source, staging, &mut state)?,
        SourceKind::Zip => extract_zip(source, staging, &mut state)?,
        SourceKind::Tar => extract_tar(source, staging, false, &mut state)?,
        SourceKind::TarGz => extract_tar(source, staging, true, &mut state)?,
    }
    Ok(state.counts)
}

fn extract_dir(source: &Path, staging: &Path, state: &mut ExtractState) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|e| format!("读取源目录失败 {}: {e}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "源目录不可用（symlink/非目录）: {}",
            source.display()
        ));
    }
    copy_dir_recursive(source, staging, Path::new(""), 0, state)
}

fn copy_dir_recursive(
    source: &Path,
    dest_root: &Path,
    rel: &Path,
    depth: usize,
    state: &mut ExtractState,
) -> Result<(), String> {
    if depth > state.limits.max_depth {
        return Err(format!("源目录层级过深（超过 {}）", state.limits.max_depth));
    }
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(source).map_err(|e| format!("读取源目录失败 {}: {e}", source.display()))?
    {
        let entry = entry.map_err(|e| format!("读取源目录条目失败 {}: {e}", source.display()))?;
        entries.push(entry.path());
    }
    entries.sort();
    for entry_path in entries {
        let name = entry_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("源目录含非 UTF-8 文件名: {}", entry_path.display()))?;
        let child_rel = rel.join(name);
        if is_reserved_package_path(&child_rel) {
            return Err(format!("包内不能包含保留文件: {}", child_rel.display()));
        }
        let metadata = fs::symlink_metadata(&entry_path)
            .map_err(|e| format!("读取源条目失败 {}: {e}", entry_path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(format!(
                "拒绝符号链接（可能逃逸插件目录）: {}",
                entry_path.display()
            ));
        }
        if file_type.is_dir() {
            state.register(&child_rel, true)?;
            let dest = dest_root.join(&child_rel);
            create_dir_checked(&dest, metadata_mode(&metadata).unwrap_or(0o755))?;
            copy_dir_recursive(&entry_path, dest_root, &child_rel, depth + 1, state)?;
        } else if file_type.is_file() {
            state.register(&child_rel, false)?;
            let dest = dest_root.join(&child_rel);
            create_parent_dirs(&dest)?;
            let mode = metadata_mode(&metadata).unwrap_or(0o644);
            let file = File::open(&entry_path)
                .map_err(|e| format!("打开源文件失败 {}: {e}", entry_path.display()))?;
            copy_reader_to_file(file, &dest, mode, Some(metadata.len()), state)?;
        } else {
            return Err(format!(
                "拒绝特殊文件（设备/FIFO/socket）: {}",
                entry_path.display()
            ));
        }
    }
    Ok(())
}

fn extract_zip(source: &Path, staging: &Path, state: &mut ExtractState) -> Result<(), String> {
    use zip::CompressionMethod;
    let file =
        File::open(source).map_err(|e| format!("打开 zip 失败 {}: {e}", source.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("解析 zip 失败 {}: {e}", source.display()))?;
    if archive.len() > state.limits.max_entries {
        return Err(format!(
            "zip 条目过多（{}，上限 {}）",
            archive.len(),
            state.limits.max_entries
        ));
    }
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("读取 zip 条目 #{index} 失败: {e}"))?;
        if entry.encrypted() {
            return Err(format!("拒绝加密 zip 条目: {}", entry.name()));
        }
        let raw_name = std::str::from_utf8(entry.name_raw())
            .map_err(|_| format!("zip 条目名不是 UTF-8: {:?}", entry.name_raw()))?;
        let rel = normalize_entry_path(raw_name)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if is_reserved_package_path(&rel) {
            return Err(format!("包内不能包含保留文件: {}", rel.display()));
        }
        let mode = entry
            .unix_mode()
            .unwrap_or(if entry.is_dir() { 0o755 } else { 0o644 });
        let file_type = mode & 0o170000;
        if file_type == 0o120000 {
            return Err(format!("拒绝 zip 符号链接条目: {raw_name}"));
        }
        if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
            return Err(format!("拒绝 zip 特殊文件条目: {raw_name}"));
        }
        if entry.is_dir() || file_type == 0o040000 {
            state.register(&rel, true)?;
            create_dir_checked(&staging.join(&rel), mode)?;
            continue;
        }
        if !matches!(
            entry.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(format!(
                "不支持的 zip 压缩算法 {:?}（只支持 store/deflate）: {raw_name}",
                entry.compression()
            ));
        }
        state.register(&rel, false)?;
        let dest = staging.join(&rel);
        create_parent_dirs(&dest)?;
        let size = entry.size();
        copy_reader_to_file(&mut entry, &dest, mode, Some(size), state)?;
    }
    Ok(())
}

fn extract_tar(
    source: &Path,
    staging: &Path,
    gzip: bool,
    state: &mut ExtractState,
) -> Result<(), String> {
    let file =
        File::open(source).map_err(|e| format!("打开 tar 失败 {}: {e}", source.display()))?;
    let reader: Box<dyn Read> = if gzip {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(false);
    let entries = archive
        .entries()
        .map_err(|e| format!("解析 tar 失败 {}: {e}", source.display()))?;
    let mut count = 0usize;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("读取 tar 条目失败: {e}"))?;
        count += 1;
        if count > state.limits.max_entries {
            return Err(format!("tar 条目过多（超过 {}）", state.limits.max_entries));
        }
        let header = entry.header();
        let entry_type = header.entry_type();
        let path_bytes = entry.path_bytes();
        let raw_name = std::str::from_utf8(&path_bytes)
            .map_err(|_| format!("tar 条目名不是 UTF-8: {:?}", path_bytes))?;
        let rel = normalize_entry_path(raw_name)?;
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(format!("拒绝 tar 符号链接/硬链接条目: {raw_name}"));
        }
        if entry_type.is_dir() {
            if rel.as_os_str().is_empty() {
                continue;
            }
            if is_reserved_package_path(&rel) {
                return Err(format!("包内不能包含保留文件: {}", rel.display()));
            }
            let mode = header.mode().unwrap_or(0o755);
            state.register(&rel, true)?;
            create_dir_checked(&staging.join(&rel), mode)?;
            continue;
        }
        if !entry_type.is_file() {
            return Err(format!("拒绝 tar 特殊条目（设备/FIFO/稀疏等）: {raw_name}"));
        }
        if rel.as_os_str().is_empty() {
            return Err("tar 文件条目路径为空".into());
        }
        if is_reserved_package_path(&rel) {
            return Err(format!("包内不能包含保留文件: {}", rel.display()));
        }
        let size = header
            .size()
            .map_err(|e| format!("读取 tar 文件大小失败 {raw_name}: {e}"))?;
        let mode = header.mode().unwrap_or(0o644);
        state.register(&rel, false)?;
        let dest = staging.join(&rel);
        create_parent_dirs(&dest)?;
        copy_reader_to_file(&mut entry, &dest, mode, Some(size), state)?;
    }
    Ok(())
}

/// 规范化包内相对路径：拒绝绝对路径、`..`、反斜杠、Windows prefix 和过长路径。
fn normalize_entry_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("包内路径为空".into());
    }
    if raw.len() > 1024 {
        return Err(format!("包内路径过长（{} 字节）", raw.len()));
    }
    if raw.contains('\\') {
        return Err(format!("包内路径含反斜杠（拒绝 Windows 分隔符）: {raw:?}"));
    }
    if raw.contains('\0') {
        return Err("包内路径含 NUL".into());
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(format!("拒绝绝对路径: {raw:?}"));
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| format!("包内路径不是 UTF-8: {raw:?}"))?;
                if part == "." || part == ".." {
                    return Err(format!("拒绝 . / .. 路径段: {raw:?}"));
                }
                out.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!("拒绝 ..（zip slip / tar traversal）: {raw:?}"))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("拒绝绝对路径: {raw:?}"))
            }
        }
    }
    Ok(out)
}

fn is_reserved_package_path(rel: &Path) -> bool {
    rel == Path::new(META_FILE)
}

fn create_dir_checked(path: &Path, mode: u32) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!("路径已存在且不是目录: {}", path.display()));
            }
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("检查目录失败 {}: {error}", path.display())),
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode((mode & 0o777) | 0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("创建目录失败 {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode((mode & 0o777) | 0o700));
    }
    Ok(())
}

fn create_parent_dirs(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建父目录失败 {}: {e}", parent.display()))?;
        }
    }
    Ok(())
}

fn copy_reader_to_file<R: Read>(
    mut reader: R,
    dest: &Path,
    mode: u32,
    declared_size: Option<u64>,
    state: &mut ExtractState,
) -> Result<(), String> {
    if let Some(size) = declared_size {
        if size > state.limits.max_file_bytes {
            return Err(format!(
                "文件过大 {}（{size} 字节，单文件上限 {}）",
                dest.display(),
                state.limits.max_file_bytes
            ));
        }
        if state.counts.bytes.saturating_add(size) > state.limits.max_total_bytes {
            return Err(format!(
                "包总大小超限（已读 {} + 声明 {size}，上限 {}）",
                state.counts.bytes, state.limits.max_total_bytes
            ));
        }
    }
    let mode = (mode & 0o777) | 0o600;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| format!("创建文件失败 {}: {e}", dest.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut file_bytes = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("读取包内容失败 {}: {e}", dest.display()))?;
        if read == 0 {
            break;
        }
        file_bytes = file_bytes.saturating_add(read as u64);
        if file_bytes > state.limits.max_file_bytes {
            return Err(format!(
                "文件超过单文件上限 {}: {}",
                state.limits.max_file_bytes,
                dest.display()
            ));
        }
        let total = state.counts.bytes.saturating_add(file_bytes);
        if total > state.limits.max_total_bytes {
            return Err(format!(
                "包总大小超过上限 {}: {}",
                state.limits.max_total_bytes,
                dest.display()
            ));
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("写文件失败 {}: {e}", dest.display()))?;
    }
    if let Some(size) = declared_size {
        if file_bytes != size {
            return Err(format!(
                "文件实际大小与包声明不一致（{} != {}）: {}",
                file_bytes,
                size,
                dest.display()
            ));
        }
    }
    drop(file);
    state.counts.bytes = state.counts.bytes.saturating_add(file_bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dest, fs::Permissions::from_mode(mode))
            .map_err(|e| format!("设置权限失败 {}: {e}", dest.display()))?;
    }
    Ok(())
}

/// 若包只有一个顶层目录且其中有 plugin.toml，把该目录内容提升到 staging 根。
fn normalize_package_root(staging: &Path) -> Result<(), String> {
    let manifest = staging.join("plugin.toml");
    if is_regular_file(&manifest) {
        return Ok(());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(staging)
        .map_err(|e| format!("读取 staging 失败 {}: {e}", staging.display()))?
    {
        let entry = entry.map_err(|e| format!("读取 staging 条目失败: {e}"))?;
        entries.push(entry.path());
    }
    if entries.len() != 1 {
        return Err(
            "包根目录缺少 plugin.toml（支持根目录直接放 plugin.toml，或单一顶层目录包装）".into(),
        );
    }
    let wrapper = &entries[0];
    let metadata = fs::symlink_metadata(wrapper)
        .map_err(|e| format!("读取包顶层目录失败 {}: {e}", wrapper.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("包根目录缺少 plugin.toml，且顶层不是普通目录".into());
    }
    if !is_regular_file(&wrapper.join("plugin.toml")) {
        return Err("包根目录缺少 plugin.toml（仅支持单一顶层目录内包含 plugin.toml）".into());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(wrapper)
        .map_err(|e| format!("读取包顶层目录失败 {}: {e}", wrapper.display()))?
    {
        let entry = entry.map_err(|e| format!("读取包顶层条目失败: {e}"))?;
        children.push(entry.path());
    }
    for child in children {
        let name = child
            .file_name()
            .ok_or_else(|| format!("包顶层条目无文件名: {}", child.display()))?;
        let dest = staging.join(name);
        if fs::symlink_metadata(&dest).is_ok() {
            return Err(format!("包顶层目录内容与根目录重名: {}", dest.display()));
        }
        fs::rename(&child, &dest)
            .map_err(|e| format!("提升包顶层目录失败 {}: {e}", child.display()))?;
    }
    fs::remove_dir(wrapper)
        .map_err(|e| format!("删除包顶层包装目录失败 {}: {e}", wrapper.display()))?;
    Ok(())
}

/// 对 staging 根做 canonicalize，确认解包内容确实落在插件根目录内。
/// 归档条目已逐项拒绝 `..`/绝对路径/符号链接，这是额外的边界断言。
fn verify_staging_root(staging: &Path, plugins_dir: &Path) -> Result<(), String> {
    let canonical = fs::canonicalize(staging)
        .map_err(|e| format!("staging canonicalize 失败 {}: {e}", staging.display()))?;
    if !canonical.starts_with(plugins_dir) {
        return Err(format!(
            "staging 路径逃逸插件目录: {}（插件根 {}）",
            canonical.display(),
            plugins_dir.display()
        ));
    }
    Ok(())
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// 解包后再次扫描：任何 symlink/特殊文件都拒绝（防御性检查，与逐条目检查互补）。
fn ensure_tree_is_regular(root: &Path, limits: &InstallLimits) -> Result<(), String> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut entries = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if depth > limits.max_depth {
            return Err("解包后目录层级超过上限".into());
        }
        for entry in
            fs::read_dir(&dir).map_err(|e| format!("解包后扫描失败 {}: {e}", dir.display()))?
        {
            let entry = entry.map_err(|e| format!("解包后扫描条目失败: {e}"))?;
            entries += 1;
            if entries > limits.max_entries {
                return Err(format!("解包后条目超过上限 {}", limits.max_entries));
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("解包后读取元数据失败 {}: {e}", path.display()))?;
            if metadata.file_type().is_symlink() {
                return Err(format!("解包后仍存在符号链接: {}", path.display()));
            }
            if metadata.is_dir() {
                stack.push((path, depth + 1));
            } else if !metadata.is_file() {
                return Err(format!("解包后存在特殊文件: {}", path.display()));
            }
        }
    }
    Ok(())
}

fn metadata_mode(metadata: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn unique_child(parent: &Path, prefix: &str) -> Result<PathBuf, String> {
    for _ in 0..64 {
        let counter = STAGING_COUNTER.fetch_add(1, Ordering::SeqCst);
        let name = format!("{prefix}-{}-{}-{counter}", std::process::id(), unix_nanos());
        let path = parent.join(name);
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("创建 staging 失败 {}: {error}", path.display())),
        }
    }
    Err("无法创建唯一 staging 目录".into())
}

fn unique_backup_path(root: &Path, version: &str) -> Result<PathBuf, String> {
    let sanitized = sanitize_version(version);
    let base = format!("{:019}-{}", unix_millis(), sanitized);
    for suffix in 0..100 {
        let name = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        let path = root.join(name);
        if fs::symlink_metadata(&path).is_err() {
            return Ok(path);
        }
    }
    Err(format!("无法为版本 {version} 创建唯一备份目录"))
}

fn sanitize_version(version: &str) -> String {
    let mut out = String::new();
    for ch in version.chars().take(64) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

fn version_part_of_backup_name(name: &str) -> String {
    name.split_once('-')
        .map(|(_, version)| version.to_string())
        .unwrap_or_default()
}

fn prune_backups(backup_root: &Path, keep: usize) -> Result<(), String> {
    let keep = keep.clamp(1, MAX_BACKUP_KEEP);
    let Ok(entries) = fs::read_dir(backup_root) else {
        return Ok(());
    };
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            continue;
        }
        dirs.push((name, path));
    }
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (name, path) in dirs.into_iter().skip(keep) {
        if let Err(error) = fs::remove_dir_all(&path) {
            return Err(format!("删除旧备份 {name} 失败: {error}"));
        }
    }
    Ok(())
}

/// Linux 上尝试原子交换；文件系统不支持时返回 false，由调用方回退两段 rename。
fn exchange_paths(a: &Path, b: &Path) -> Result<bool, String> {
    #[cfg(test)]
    if FORCE_FALLBACK.load(Ordering::SeqCst) {
        return Ok(false);
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let a_c = CString::new(a.as_os_str().as_bytes())
            .map_err(|_| format!("路径含 NUL: {}", a.display()))?;
        let b_c = CString::new(b.as_os_str().as_bytes())
            .map_err(|_| format!("路径含 NUL: {}", b.display()))?;
        // SAFETY: 两个 CString 在调用期间有效；renameat2 只读路径，不保留指针。
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                a_c.as_ptr(),
                libc::AT_FDCWD,
                b_c.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP) => Ok(false),
            _ => Err(format!(
                "原子交换失败 {} <-> {}: {error}",
                a.display(),
                b.display()
            )),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (a, b);
        Ok(false)
    }
}

fn sync_dir_best_effort(path: &Path) {
    if let Ok(file) = File::open(path) {
        let _ = file.sync_all();
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

struct StagingGuard {
    path: PathBuf,
    keep: bool,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn disarm(&mut self) {
        self.keep = true;
    }

    fn cleanup(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        self.keep = true;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// 进程内 + flock 的安装锁：并发安装返回明确错误，不阻塞 UI 线程。
struct InstallLock {
    _file: File,
}

impl InstallLock {
    fn acquire(plugins_dir: &Path) -> Result<Self, String> {
        let path = plugins_dir.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC);
        }
        let file = options
            .open(&path)
            .map_err(|e| format!("打开安装锁失败 {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // 安装锁是跨进程互斥，但同一进程里并发的 fork/exec（插件 supervisor
            // 启动）可能短暂继承锁 fd，导致接下来的 LOCK_NB 瞬时 EWOULDBLOCK。
            // 非阻塞重试几百毫秒；真正的并发安装仍会明确失败，UI 不会被永久阻塞。
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                // SAFETY: fd 来自仍然存活的 File；flock 不保留 fd 之外的指针。
                let result =
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if result == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    if Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    return Err(format!(
                        "另一个插件安装/回滚正在进行（{}），请稍后重试",
                        path.display()
                    ));
                }
                return Err(format!("获取安装锁失败 {}: {error}", path.display()));
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
pub(crate) fn force_exchange_fallback(enabled: bool) {
    FORCE_FALLBACK.store(enabled, Ordering::SeqCst);
}

#[cfg(test)]
static FORCE_FALLBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
