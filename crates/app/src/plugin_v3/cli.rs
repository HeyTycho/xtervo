// SPDX-License-Identifier: GPL-3.0-only
//! `tervo plugins ...`：无 UI 的本地安装/更新/回滚/校验/签名工具。
//!
//! 不启动窗口、不初始化 Slint、不执行插件代码。方便打包/CI/无桌面环境使用；
//! 设置页调用的是同一套 [`super::install`] / [`super::signature`] 代码。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use rand_core::OsRng;

use super::install::{self, InstallOptions, RollbackOptions, DEFAULT_BACKUP_KEEP};
use super::manifest;
use super::signature;
use super::PluginStore;

/// 退出码：0 = 成功；1 = 校验/安装失败；2 = 用法错误。
pub fn run_cli(args: &[String]) -> i32 {
    match run_cli_inner(args) {
        Ok(code) => code,
        Err(CliError::Usage(message)) => {
            eprintln!("tervo plugins: {message}");
            eprintln!();
            eprintln!("{}", usage());
            2
        }
        Err(CliError::Failed(message)) => {
            eprintln!("tervo plugins: {message}");
            1
        }
    }
}

enum CliError {
    Usage(String),
    Failed(String),
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

type CliResult<T> = Result<T, CliError>;

fn run_cli_inner(args: &[String]) -> CliResult<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(CliError::Usage("缺少子命令".into()));
    };
    match command {
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(0)
        }
        "install" => cmd_install(&args[1..], false),
        "update" => cmd_install(&args[1..], true),
        "verify" => cmd_verify(&args[1..]),
        "rollback" => cmd_rollback(&args[1..]),
        "list" | "ls" => {
            no_extra_args(&args[1..], "list")?;
            cmd_list()
        }
        "backups" => cmd_backups(&args[1..]),
        "keygen" => cmd_keygen(&args[1..]),
        "sign" => cmd_sign(&args[1..]),
        other => Err(CliError::Usage(format!("未知子命令 {other:?}"))),
    }
}

fn usage() -> String {
    [
        "usage: tervo plugins <command> [options]",
        "",
        "commands:",
        "  install <dir|zip|tar|tar.gz>   install a v3 plugin package locally",
        "  update  <dir|zip|tar|tar.gz>   update an installed plugin (same id required)",
        "  rollback <id> [--to <dir|version>] [--keep N]",
        "  list                          list installed plugins (id/version/signature/source)",
        "  backups <id>                  list local rollback backups",
        "  verify <dir|zip|tar|tar.gz>   validate package/signature without installing",
        "  keygen [--out-dir DIR] [--force]",
        "  sign <plugin-dir> [--key PATH]",
        "",
        "options:",
        "  --keep N            backups to keep (default 3, max 10)",
        "  --force             replace a target directory owned by another id",
        "  --require-signature reject unsigned packages for this command",
        "  --pubkey VALUE      explicit public key (base64/hex/PEM; mainly tests)",
        "",
        "signature config:",
        "  TERVO_PLUGIN_REQUIRE_SIGNATURE=1 or plugin_require_signature = true",
        "  TERVO_PLUGIN_PUBKEY=<base64|hex|path|@path>",
        "  default pubkey: <config>/tervo/plugin-signing.pub",
        "  docs: docs/PLUGIN_INSTALL.md",
    ]
    .join("\n")
}

fn cmd_install(args: &[String], update: bool) -> CliResult<i32> {
    let parsed = parse_flags(
        args,
        &["--keep", "--pubkey"],
        &["--force", "--require-signature"],
    )?;
    let source = parsed
        .positional
        .first()
        .ok_or_else(|| CliError::usage("install/update 需要包路径"))?;
    if parsed.positional.len() > 1 {
        return Err(CliError::usage("install/update 只接受一个包路径"));
    }
    let keep = parse_keep(parsed.value("--keep"))?;
    let require_signature = config_requires_signature() || parsed.flag("--require-signature");
    let options = InstallOptions {
        require_signature,
        pubkey: parsed.value("--pubkey").map(str::to_string),
        backup_keep: keep,
        update,
        force: parsed.flag("--force"),
        limits: Default::default(),
        signature_config: None,
    };
    let plugins_dir = PluginStore::dir();
    let report = if update {
        install::update_package(&plugins_dir, Path::new(source), &options)
    } else {
        install::install_package(&plugins_dir, Path::new(source), &options)
    }
    .map_err(CliError::failed)?;
    println!(
        "{} {} {} -> {}",
        if update { "updated" } else { "installed" },
        report.id,
        report.previous_version.as_deref().unwrap_or("-"),
        report.version
    );
    println!("  source:    {}", report.source.display());
    println!("  package:   {}", report.source_kind.as_str());
    println!("  signature: {}", report.signature.label());
    println!("  files:     {} ({} bytes)", report.files, report.bytes);
    println!("  target:    {}", report.plugin_dir.display());
    if let Some(backup) = &report.backup_dir {
        println!("  backup:    {}", backup.display());
    }
    for warning in &report.warnings {
        eprintln!("  warning: {warning}");
    }
    Ok(0)
}

fn cmd_verify(args: &[String]) -> CliResult<i32> {
    let parsed = parse_flags(args, &["--pubkey"], &["--require-signature"])?;
    let source = parsed
        .positional
        .first()
        .ok_or_else(|| CliError::usage("verify 需要包路径"))?;
    if parsed.positional.len() > 1 {
        return Err(CliError::usage("verify 只接受一个包路径"));
    }
    let options = InstallOptions {
        require_signature: config_requires_signature() || parsed.flag("--require-signature"),
        pubkey: parsed.value("--pubkey").map(str::to_string),
        ..InstallOptions::default()
    };
    let info = install::inspect_package(&PluginStore::dir(), Path::new(source), &options)
        .map_err(CliError::failed)?;
    println!("ok {} {} ({})", info.id, info.version, info.name);
    println!("  source:    {}", info.source.display());
    println!("  package:   {}", info.source_kind.as_str());
    println!("  signature: {}", info.signature.label());
    println!("  entry:     {}", info.entry);
    println!("  files:     {} ({} bytes)", info.files, info.bytes);
    Ok(0)
}

fn cmd_rollback(args: &[String]) -> CliResult<i32> {
    let parsed = parse_flags(
        args,
        &["--to", "--keep", "--pubkey"],
        &["--require-signature"],
    )?;
    let id = parsed
        .positional
        .first()
        .ok_or_else(|| CliError::usage("rollback 需要插件 id"))?;
    if parsed.positional.len() > 1 {
        return Err(CliError::usage("rollback 只接受一个插件 id"));
    }
    let keep = parse_keep(parsed.value("--keep"))?;
    let options = RollbackOptions {
        backup_keep: keep,
        require_signature: config_requires_signature() || parsed.flag("--require-signature"),
        pubkey: parsed.value("--pubkey").map(str::to_string),
        signature_config: None,
    };
    let report = install::rollback_package(&PluginStore::dir(), id, parsed.value("--to"), &options)
        .map_err(CliError::failed)?;
    println!(
        "rolled back {} {} -> {}",
        report.id, report.from_version, report.to_version
    );
    println!("  target: {}", report.plugin_dir.display());
    println!("  backup: {}", report.backup_dir.display());
    for warning in &report.warnings {
        eprintln!("  warning: {warning}");
    }
    Ok(0)
}

fn cmd_list() -> CliResult<i32> {
    let dir = PluginStore::dir();
    let entries = super::discover_plugins(&dir);
    println!("plugins dir: {}", dir.display());
    if entries.is_empty() {
        println!("no plugins installed");
        return Ok(0);
    }
    for entry in entries {
        if !entry.supported {
            println!(
                "skip  {}  [{}]  {}",
                entry.name, entry.api_label, entry.reason
            );
            continue;
        }
        let source = entry
            .install_meta
            .as_ref()
            .map(|meta| meta.source.as_str())
            .unwrap_or("-");
        println!(
            "{}  {}  signature={}  source={}",
            entry.key,
            entry.version,
            entry.signature_status.label(),
            source
        );
        if let Some(meta) = &entry.install_meta {
            println!(
                "  installed_at_unix={} files={} bytes={} key={}",
                meta.installed_at_unix,
                meta.files,
                meta.bytes,
                if meta.signature_key_id.is_empty() {
                    "-"
                } else {
                    &meta.signature_key_id
                }
            );
        }
    }
    Ok(0)
}

fn cmd_backups(args: &[String]) -> CliResult<i32> {
    let parsed = parse_flags(args, &[], &[])?;
    let id = parsed
        .positional
        .first()
        .ok_or_else(|| CliError::usage("backups 需要插件 id"))?;
    if parsed.positional.len() > 1 {
        return Err(CliError::usage("backups 只接受一个插件 id"));
    }
    let backups = install::list_backups(&PluginStore::dir(), id);
    if backups.is_empty() {
        println!("plugin {id}: no backups");
        return Ok(0);
    }
    for backup in backups {
        println!(
            "{}  version={}  installed_at_unix={}",
            backup.dir_name,
            backup.version,
            backup
                .installed_at_unix
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string())
        );
    }
    Ok(0)
}

fn cmd_keygen(args: &[String]) -> CliResult<i32> {
    let parsed = parse_flags(args, &["--out-dir"], &["--force"])?;
    if !parsed.positional.is_empty() {
        return Err(CliError::usage("keygen 不接受位置参数"));
    }
    let plugins_dir = PluginStore::dir();
    let out_dir = parsed
        .value("--out-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            plugins_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| plugins_dir.clone())
        });
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| CliError::failed(format!("创建 {} 失败: {e}", out_dir.display())))?;
    let private_path = out_dir.join(signature::DEFAULT_SIGNING_KEY_FILE);
    let public_path = out_dir.join(signature::DEFAULT_PUBKEY_FILE);
    let force = parsed.flag("--force");
    if !force && (private_path.exists() || public_path.exists()) {
        return Err(CliError::failed(format!(
            "{} 或 {} 已存在；使用 --force 才覆盖",
            private_path.display(),
            public_path.display()
        )));
    }
    let mut rng = OsRng;
    let key = SigningKey::generate(&mut rng);
    write_atomic(
        &private_path,
        signature::signing_key_file_text(&key).as_bytes(),
        0o600,
    )
    .map_err(CliError::failed)?;
    write_atomic(
        &public_path,
        signature::public_key_file_text(&key.verifying_key()).as_bytes(),
        0o644,
    )
    .map_err(CliError::failed)?;
    println!("generated Ed25519 keypair");
    println!("  private: {}", private_path.display());
    println!("  public:  {}", public_path.display());
    println!(
        "  key id:  {}",
        signature::public_key_id(&key.verifying_key())
    );
    println!(
        "  next:    tervo plugins sign <plugin-dir> --key {}",
        private_path.display()
    );
    Ok(0)
}

fn cmd_sign(args: &[String]) -> CliResult<i32> {
    let parsed = parse_flags(args, &["--key"], &[])?;
    let dir = parsed
        .positional
        .first()
        .ok_or_else(|| CliError::usage("sign 需要插件目录（含 plugin.toml）"))?;
    if parsed.positional.len() > 1 {
        return Err(CliError::usage("sign 只接受一个插件目录"));
    }
    let dir = Path::new(dir);
    if !dir.is_dir() {
        return Err(CliError::failed(format!("不是目录: {}", dir.display())));
    }
    let key_path = parsed
        .value("--key")
        .map(PathBuf::from)
        .unwrap_or_else(|| signature::default_signing_key_path(&PluginStore::dir()));
    let key = signature::load_signing_key(&key_path).map_err(CliError::failed)?;
    let manifest_path = dir.join("plugin.toml");
    let manifest_bytes =
        signature::read_limited_file(&manifest_path, manifest::MAX_MANIFEST_BYTES as u64)
            .map_err(CliError::failed)?;
    let text = std::str::from_utf8(&manifest_bytes)
        .map_err(|_| CliError::failed("plugin.toml 不是 UTF-8"))?;
    let manifest: manifest::Manifest = toml::from_str(text)
        .map_err(|e| CliError::failed(format!("v3 plugin.toml 解析失败: {e}")))?;
    manifest.validate().map_err(CliError::failed)?;
    let sig_path = dir.join(signature::SIG_FILE_NAME);
    write_atomic(
        &sig_path,
        signature::sign_manifest(&manifest_bytes, &key).as_bytes(),
        0o644,
    )
    .map_err(CliError::failed)?;
    println!("signed {} {}", manifest.id, manifest.version);
    println!("  signature: {}", sig_path.display());
    println!(
        "  key id:    {}",
        signature::public_key_id(&key.verifying_key())
    );
    Ok(0)
}

fn parse_keep(raw: Option<&str>) -> CliResult<usize> {
    match raw {
        None => Ok(DEFAULT_BACKUP_KEEP),
        Some(value) => {
            let keep = value
                .parse::<usize>()
                .map_err(|_| CliError::usage(format!("--keep 需要整数（当前 {value:?}）")))?;
            Ok(keep.clamp(1, install::MAX_BACKUP_KEEP))
        }
    }
}

fn config_requires_signature() -> bool {
    crate::config::Config::load().plugin_require_signature
}

fn no_extra_args(args: &[String], command: &str) -> CliResult<()> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "{command} 不接受参数（收到 {} 个）",
            args.len()
        )))
    }
}

struct ParsedArgs {
    positional: Vec<String>,
    values: HashMap<String, String>,
    flags: HashSet<String>,
}

impl ParsedArgs {
    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
}

fn parse_flags(
    args: &[String],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> CliResult<ParsedArgs> {
    let mut parsed = ParsedArgs {
        positional: Vec::new(),
        values: HashMap::new(),
        flags: HashSet::new(),
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            parsed.positional.extend(args[index + 1..].iter().cloned());
            break;
        }
        if let Some((name, value)) = arg.split_once('=') {
            if value_flags.contains(&name) {
                parsed.values.insert(name.to_string(), value.to_string());
                index += 1;
                continue;
            }
            if bool_flags.contains(&name) {
                return Err(CliError::usage(format!("{name} 不接受值")));
            }
        }
        if value_flags.contains(&arg.as_str()) {
            let value = args
                .get(index + 1)
                .ok_or_else(|| CliError::usage(format!("{arg} 需要值")))?;
            parsed.values.insert(arg.clone(), value.clone());
            index += 2;
            continue;
        }
        if bool_flags.contains(&arg.as_str()) {
            parsed.flags.insert(arg.clone());
            index += 1;
            continue;
        }
        if arg.starts_with("--") {
            return Err(CliError::usage(format!("未知选项 {arg:?}")));
        }
        parsed.positional.push(arg.clone());
        index += 1;
    }
    Ok(parsed)
}

/// 原子写文件：临时文件同目录 + rename；权限在创建时收紧。
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("创建目录失败 {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("write"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|e| format!("创建临时文件失败 {}: {e}", tmp.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("写临时文件失败 {}: {e}", tmp.display()))?;
    file.sync_all()
        .map_err(|e| format!("同步临时文件失败 {}: {e}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|e| format!("替换文件失败 {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    Ok(())
}
