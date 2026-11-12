// SPDX-License-Identifier: GPL-3.0-only
//! Local plugin install/update/rollback/signature tests.
//!
//! These tests never execute plugin code. They build real directory/zip/tar
//! packages, real Ed25519 signatures, and use isolated temp plugin roots.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use ed25519_dalek::{SigningKey, VerifyingKey};

use super::discover_plugins;
use super::install::{
    self, force_exchange_fallback, inspect_package, install_package, list_backups,
    rollback_package, update_package, InstallLimits, InstallOptions, RollbackOptions,
};
use super::signature::{self, SignatureConfig, SignatureStatus};

fn tmp(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("tervo-plugin-install-{tag}-"))
        .tempdir()
        .expect("tempdir")
}

fn manifest_text(id: &str, version: &str) -> String {
    format!(
        r#"api = 3
host_abi = 3
id = "{id}"
name = "Test {id}"
version = "{version}"
runtime = "process"
entry = "main.sh"
"#
    )
}

fn write_plugin_dir(dir: &Path, id: &str, version: &str, marker: Option<&Path>) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("plugin.toml"), manifest_text(id, version)).unwrap();
    let body = match marker {
        Some(marker) => format!("#!/bin/sh\ntouch {:?}\nexit 0\n", marker),
        None => "#!/bin/sh\nexit 0\n".to_string(),
    };
    write_executable(&dir.join("main.sh"), &body);
    dir.to_path_buf()
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
}

fn unsigned_options(limits: InstallLimits) -> InstallOptions {
    InstallOptions {
        limits,
        signature_config: Some(SignatureConfig::explicit(false, None)),
        ..InstallOptions::default()
    }
}

fn signature_options(required: bool, key: Option<&VerifyingKey>) -> InstallOptions {
    InstallOptions {
        signature_config: Some(SignatureConfig::explicit(required, key.copied())),
        ..InstallOptions::default()
    }
}

fn test_key() -> SigningKey {
    SigningKey::from_bytes(&[9u8; 32])
}

fn sign_plugin_dir(dir: &Path, key: &SigningKey) {
    let manifest = fs::read(dir.join("plugin.toml")).unwrap();
    fs::write(
        dir.join(signature::SIG_FILE_NAME),
        signature::sign_manifest(&manifest, key),
    )
    .unwrap();
}

fn zip_plugin(path: &Path, id: &str, version: &str, wrapper: Option<&str>) {
    let file = File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let exec_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);
    let prefix = wrapper
        .map(|wrapper| format!("{wrapper}/"))
        .unwrap_or_default();
    zip.start_file(format!("{prefix}plugin.toml"), options)
        .unwrap();
    zip.write_all(manifest_text(id, version).as_bytes())
        .unwrap();
    zip.start_file(format!("{prefix}main.sh"), exec_options)
        .unwrap();
    zip.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
    zip.finish().unwrap();
}

fn zip_with_entries(path: &Path, entries: &[(&str, &[u8])]) {
    let file = File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

struct TarEntry<'a> {
    name: &'a str,
    data: &'a [u8],
    mode: u32,
    kind: tar::EntryType,
    link: Option<&'a str>,
}

fn write_tar(path: &Path, entries: &[TarEntry<'_>]) {
    let file = File::create(path).unwrap();
    let mut builder = tar::Builder::new(file);
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry.kind);
        header.set_mode(entry.mode);
        header.set_size(entry.data.len() as u64);
        header.set_mtime(0);
        if let Some(link) = entry.link {
            header.set_link_name(link).unwrap();
        }
        let name = entry.name.as_bytes();
        assert!(name.len() <= 100);
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        builder.append(&header, entry.data).unwrap();
    }
    builder.finish().unwrap();
}

fn report_version(plugins: &Path, id: &str) -> String {
    let path = plugins.join(id).join("plugin.toml");
    let text = fs::read_to_string(path).unwrap();
    let value: toml::Value = toml::from_str(&text).unwrap();
    value.get("version").unwrap().as_str().unwrap().to_string()
}

#[test]
fn installs_directory_and_writes_metadata_without_executing() {
    let temp = tmp("dir");
    let marker = temp.path().join("EXECUTED");
    let source = write_plugin_dir(
        &temp.path().join("src"),
        "dev.test.dir",
        "1.0.0",
        Some(&marker),
    );
    let plugins = temp.path().join("plugins");
    let report = install_package(
        &plugins,
        &source,
        &unsigned_options(InstallLimits::default()),
    )
    .expect("directory install");
    assert_eq!(report.id, "dev.test.dir");
    assert_eq!(report.version, "1.0.0");
    assert_eq!(report.signature, SignatureStatus::Unsigned);
    assert!(report.plugin_dir.join("plugin.toml").is_file());
    assert!(!marker.exists(), "installer must never execute plugin code");
    let meta = install::installed_meta(&report.plugin_dir).expect("metadata");
    assert_eq!(meta.id, "dev.test.dir");
    assert_eq!(meta.version, "1.0.0");
    assert_eq!(meta.signature, "unsigned");
    assert_eq!(meta.source_kind, "dir");
    assert!(meta.source.ends_with("src"));
    let entries = discover_plugins(&plugins);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].supported);
    assert_eq!(entries[0].install_meta.as_ref().unwrap().version, "1.0.0");
    // staging 目录已清理，不留下可被发现的副本。
    let staging = fs::read_dir(plugins.join(install::STAGING_DIR)).unwrap();
    assert_eq!(staging.count(), 0);
}

#[test]
fn installs_zip_with_single_wrapper_directory() {
    let temp = tmp("zip");
    let zip_path = temp.path().join("plugin.zip");
    zip_plugin(&zip_path, "dev.test.zip", "2.0.0", Some("wrapped-plugin"));
    let plugins = temp.path().join("plugins");
    let report = install_package(
        &plugins,
        &zip_path,
        &unsigned_options(InstallLimits::default()),
    )
    .expect("zip install");
    assert_eq!(report.source_kind, install::SourceKind::Zip);
    let entry = report.plugin_dir.join("main.sh");
    assert!(entry.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(&entry).unwrap().permissions().mode() & 0o111,
            0
        );
    }
}

#[test]
fn installs_tar_and_tar_gz() {
    let temp = tmp("tar");
    let plugins = temp.path().join("plugins");
    let manifest = manifest_text("dev.test.tar", "3.0.0");
    let tar_path = temp.path().join("plugin.tar");
    write_tar(
        &tar_path,
        &[
            TarEntry {
                name: "plugin.toml",
                data: manifest.as_bytes(),
                mode: 0o644,
                kind: tar::EntryType::file(),
                link: None,
            },
            TarEntry {
                name: "main.sh",
                data: b"#!/bin/sh\nexit 0\n",
                mode: 0o755,
                kind: tar::EntryType::file(),
                link: None,
            },
        ],
    );
    let report = install_package(
        &plugins,
        &tar_path,
        &unsigned_options(InstallLimits::default()),
    )
    .expect("tar install");
    assert_eq!(report.source_kind, install::SourceKind::Tar);
    assert_eq!(report_version(&plugins, "dev.test.tar"), "3.0.0");

    let gz_path = temp.path().join("plugin.tar.gz");
    let gz_file = File::create(&gz_path).unwrap();
    let encoder = flate2::write::GzEncoder::new(gz_file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::file());
    header.set_mode(0o644);
    header.set_size(manifest.len() as u64);
    header.set_mtime(0);
    let name = b"plugin.toml";
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.set_cksum();
    builder.append(&header, manifest.as_bytes()).unwrap();
    let mut script_header = tar::Header::new_gnu();
    script_header.set_entry_type(tar::EntryType::file());
    script_header.set_mode(0o755);
    let script = b"#!/bin/sh\nexit 0\n";
    script_header.set_size(script.len() as u64);
    script_header.set_mtime(0);
    let script_name = b"main.sh";
    script_header.as_old_mut().name[..script_name.len()].copy_from_slice(script_name);
    script_header.set_cksum();
    builder.append(&script_header, &script[..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    let report = install_package(
        &temp.path().join("plugins-gz"),
        &gz_path,
        &unsigned_options(InstallLimits::default()),
    )
    .expect("tar.gz install");
    assert_eq!(report.source_kind, install::SourceKind::TarGz);
}

#[test]
fn rejects_bad_manifest_and_leaves_no_target() {
    let temp = tmp("bad-manifest");
    let source = temp.path().join("src");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("plugin.toml"),
        manifest_text("dev.test.bad", "1.0.0").replace("api = 3", "api = 2"),
    )
    .unwrap();
    write_executable(&source.join("main.sh"), "#!/bin/sh\nexit 0\n");
    let plugins = temp.path().join("plugins");
    let error = install_package(
        &plugins,
        &source,
        &unsigned_options(InstallLimits::default()),
    )
    .unwrap_err();
    assert!(error.contains("api"), "{error}");
    assert!(!plugins.join("dev.test.bad").exists());
    let staging = plugins.join(install::STAGING_DIR);
    assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
}

#[test]
fn rejects_zip_slip_and_absolute_paths() {
    let temp = tmp("zip-slip");
    let plugins = temp.path().join("plugins");
    let manifest = manifest_text("dev.test.slip", "1.0.0");
    let zip_path = temp.path().join("slip.zip");
    let absolute = temp.path().join("absolute-evil");
    let absolute_text = absolute.to_string_lossy().to_string();
    zip_with_entries(
        &zip_path,
        &[
            ("plugin.toml", manifest.as_bytes()),
            ("main.sh", b"#!/bin/sh\nexit 0\n"),
            ("../evil.txt", b"escaped"),
            (absolute_text.as_str(), b"escaped"),
        ],
    );
    let error = install_package(
        &plugins,
        &zip_path,
        &unsigned_options(InstallLimits::default()),
    )
    .unwrap_err();
    assert!(
        error.contains("..") || error.contains("绝对路径") || error.contains("包内路径"),
        "{error}"
    );
    assert!(!plugins.join("dev.test.slip").exists());
    assert!(!plugins.join("evil.txt").exists());
    assert!(!plugins.join(".staging").join("evil.txt").exists());
    assert!(!absolute.exists());
}

#[test]
fn rejects_tar_traversal_and_symlink() {
    let temp = tmp("tar-slip");
    let plugins = temp.path().join("plugins");
    let manifest = manifest_text("dev.test.tarslip", "1.0.0");
    let traversal = temp.path().join("traversal.tar");
    write_tar(
        &traversal,
        &[
            TarEntry {
                name: "plugin.toml",
                data: manifest.as_bytes(),
                mode: 0o644,
                kind: tar::EntryType::file(),
                link: None,
            },
            TarEntry {
                name: "../evil.txt",
                data: b"escaped",
                mode: 0o644,
                kind: tar::EntryType::file(),
                link: None,
            },
        ],
    );
    let error = install_package(
        &plugins,
        &traversal,
        &unsigned_options(InstallLimits::default()),
    )
    .unwrap_err();
    assert!(
        error.contains("..") || error.contains("包内路径"),
        "{error}"
    );
    assert!(!plugins.join("dev.test.tarslip").exists());

    let symlink_tar = temp.path().join("symlink.tar");
    write_tar(
        &symlink_tar,
        &[
            TarEntry {
                name: "plugin.toml",
                data: manifest.as_bytes(),
                mode: 0o644,
                kind: tar::EntryType::file(),
                link: None,
            },
            TarEntry {
                name: "escape",
                data: b"",
                mode: 0o777,
                kind: tar::EntryType::symlink(),
                link: Some("/etc/passwd"),
            },
        ],
    );
    let error = install_package(
        &temp.path().join("plugins-symlink"),
        &symlink_tar,
        &unsigned_options(InstallLimits::default()),
    )
    .unwrap_err();
    assert!(
        error.contains("符号链接") || error.contains("symlink"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlink_in_directory_source() {
    use std::os::unix::fs::symlink;
    let temp = tmp("dir-symlink");
    let source = write_plugin_dir(&temp.path().join("src"), "dev.test.symlink", "1.0.0", None);
    symlink("/etc/passwd", source.join("passwd-link")).unwrap();
    let plugins = temp.path().join("plugins");
    let error = install_package(
        &plugins,
        &source,
        &unsigned_options(InstallLimits::default()),
    )
    .unwrap_err();
    assert!(error.contains("符号链接"), "{error}");
    assert!(!plugins.join("dev.test.symlink").exists());
}

#[test]
fn rejects_too_many_files_and_total_size() {
    let temp = tmp("limits");
    let source = write_plugin_dir(&temp.path().join("src"), "dev.test.limits", "1.0.0", None);
    fs::write(source.join("extra.txt"), b"extra").unwrap();
    let plugins = temp.path().join("plugins");
    let mut limits = InstallLimits::default();
    limits.max_files = 1;
    let error = install_package(&plugins, &source, &unsigned_options(limits)).unwrap_err();
    assert!(error.contains("文件过多"), "{error}");
    assert!(!plugins.join("dev.test.limits").exists());

    let mut limits = InstallLimits::default();
    limits.max_total_bytes = 8;
    let error = install_package(&plugins, &source, &unsigned_options(limits)).unwrap_err();
    assert!(
        error.contains("总大小") || error.contains("上限"),
        "{error}"
    );
    assert!(!plugins.join("dev.test.limits").exists());
}

#[test]
fn failed_update_keeps_old_version() {
    let temp = tmp("update-fail");
    let plugins = temp.path().join("plugins");
    let v1 = write_plugin_dir(&temp.path().join("v1"), "dev.test.update", "1.0.0", None);
    install_package(&plugins, &v1, &unsigned_options(InstallLimits::default())).unwrap();
    assert_eq!(report_version(&plugins, "dev.test.update"), "1.0.0");

    let v2 = write_plugin_dir(&temp.path().join("v2"), "dev.test.update", "2.0.0", None);
    fs::write(
        v2.join("plugin.toml"),
        manifest_text("dev.test.update", "2.0.0").replace("host_abi = 3", "host_abi = 2"),
    )
    .unwrap();
    let error =
        update_package(&plugins, &v2, &unsigned_options(InstallLimits::default())).unwrap_err();
    assert!(error.contains("host_abi"), "{error}");
    assert_eq!(report_version(&plugins, "dev.test.update"), "1.0.0");
    assert!(plugins.join("dev.test.update").join("main.sh").is_file());
    assert!(list_backups(&plugins, "dev.test.update").is_empty());
}

#[test]
fn update_creates_backup_and_rollback_roundtrips() {
    let temp = tmp("rollback");
    let plugins = temp.path().join("plugins");
    let v1 = write_plugin_dir(&temp.path().join("v1"), "dev.test.rollback", "1.0.0", None);
    install_package(&plugins, &v1, &unsigned_options(InstallLimits::default())).unwrap();
    let v2 = write_plugin_dir(&temp.path().join("v2"), "dev.test.rollback", "2.0.0", None);
    let report =
        update_package(&plugins, &v2, &unsigned_options(InstallLimits::default())).expect("update");
    assert_eq!(report.previous_version.as_deref(), Some("1.0.0"));
    assert!(report.backup_dir.as_ref().unwrap().is_dir());
    assert_eq!(report_version(&plugins, "dev.test.rollback"), "2.0.0");
    let backups = list_backups(&plugins, "dev.test.rollback");
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].version, "1.0.0");
    assert!(
        backups[0].dir_name.contains("1.0.0"),
        "backup dir should use the old version: {}",
        backups[0].dir_name
    );

    let back = rollback_package(
        &plugins,
        "dev.test.rollback",
        None,
        &RollbackOptions {
            signature_config: Some(SignatureConfig::explicit(false, None)),
            ..RollbackOptions::default()
        },
    )
    .expect("rollback");
    assert_eq!(back.from_version, "2.0.0");
    assert_eq!(back.to_version, "1.0.0");
    assert_eq!(report_version(&plugins, "dev.test.rollback"), "1.0.0");
    let backups = list_backups(&plugins, "dev.test.rollback");
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].version, "2.0.0");

    let forward = rollback_package(
        &plugins,
        "dev.test.rollback",
        Some("2.0.0"),
        &RollbackOptions {
            signature_config: Some(SignatureConfig::explicit(false, None)),
            ..RollbackOptions::default()
        },
    )
    .expect("rollback to explicit version");
    assert_eq!(forward.to_version, "2.0.0");
    assert_eq!(report_version(&plugins, "dev.test.rollback"), "2.0.0");
}

#[test]
fn update_prunes_old_backups() {
    let temp = tmp("prune");
    let plugins = temp.path().join("plugins");
    for version in ["1.0.0", "2.0.0", "3.0.0", "4.0.0"] {
        let source = write_plugin_dir(
            &temp.path().join(format!("v{version}")),
            "dev.test.prune",
            version,
            None,
        );
        let options = InstallOptions {
            backup_keep: 2,
            signature_config: Some(SignatureConfig::explicit(false, None)),
            ..InstallOptions::default()
        };
        if version == "1.0.0" {
            install_package(&plugins, &source, &options).unwrap();
        } else {
            update_package(&plugins, &source, &options).unwrap();
        }
    }
    assert_eq!(report_version(&plugins, "dev.test.prune"), "4.0.0");
    let versions: Vec<String> = list_backups(&plugins, "dev.test.prune")
        .into_iter()
        .map(|backup| backup.version)
        .collect();
    assert_eq!(versions.len(), 2, "{versions:?}");
    assert!(versions.contains(&"3.0.0".to_string()));
    assert!(versions.contains(&"2.0.0".to_string()));
}

#[test]
fn update_id_mismatch_is_rejected() {
    let temp = tmp("id-mismatch");
    let plugins = temp.path().join("plugins");
    let v1 = write_plugin_dir(&temp.path().join("v1"), "dev.test.one", "1.0.0", None);
    install_package(&plugins, &v1, &unsigned_options(InstallLimits::default())).unwrap();
    let other = write_plugin_dir(&temp.path().join("other"), "dev.test.two", "2.0.0", None);
    let error = update_package(
        &plugins,
        &other,
        &unsigned_options(InstallLimits::default()),
    )
    .unwrap_err();
    assert!(
        error.contains("id 不一致") || error.contains("需要已安装") || error.contains("不存在"),
        "{error}"
    );
    assert_eq!(report_version(&plugins, "dev.test.one"), "1.0.0");
}

#[test]
fn fallback_rename_path_updates_and_rolls_back() {
    force_exchange_fallback(true);
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            force_exchange_fallback(false);
        }
    }
    let _reset = Reset;
    let temp = tmp("fallback");
    let plugins = temp.path().join("plugins");
    let v1 = write_plugin_dir(&temp.path().join("v1"), "dev.test.fallback", "1.0.0", None);
    install_package(&plugins, &v1, &unsigned_options(InstallLimits::default())).unwrap();
    let v2 = write_plugin_dir(&temp.path().join("v2"), "dev.test.fallback", "2.0.0", None);
    let updated = update_package(&plugins, &v2, &unsigned_options(InstallLimits::default()))
        .expect("fallback update");
    assert_eq!(updated.previous_version.as_deref(), Some("1.0.0"));
    assert_eq!(report_version(&plugins, "dev.test.fallback"), "2.0.0");
    let backups = list_backups(&plugins, "dev.test.fallback");
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].version, "1.0.0");
    let back = rollback_package(
        &plugins,
        "dev.test.fallback",
        None,
        &RollbackOptions {
            signature_config: Some(SignatureConfig::explicit(false, None)),
            ..RollbackOptions::default()
        },
    )
    .expect("fallback rollback");
    assert_eq!(back.to_version, "1.0.0");
    assert_eq!(report_version(&plugins, "dev.test.fallback"), "1.0.0");
}

#[test]
fn verified_signature_is_installed_and_recorded() {
    let temp = tmp("sig-ok");
    let key = test_key();
    let source = write_plugin_dir(&temp.path().join("src"), "dev.test.signed", "1.0.0", None);
    sign_plugin_dir(&source, &key);
    let plugins = temp.path().join("plugins");
    let report = install_package(
        &plugins,
        &source,
        &signature_options(true, Some(&key.verifying_key())),
    )
    .expect("signed install");
    assert_eq!(
        report.signature,
        SignatureStatus::Verified {
            key_id: signature::public_key_id(&key.verifying_key())
        }
    );
    let meta = install::installed_meta(&report.plugin_dir).unwrap();
    assert_eq!(meta.signature, "verified");
    assert_eq!(
        meta.signature_key_id,
        signature::public_key_id(&key.verifying_key())
    );
    // 发现阶段按本机公钥做实时校验：写入默认公钥文件后应为 verified。
    fs::write(
        plugins
            .parent()
            .unwrap()
            .join(signature::DEFAULT_PUBKEY_FILE),
        signature::public_key_file_text(&key.verifying_key()),
    )
    .unwrap();
    let entries = discover_plugins(&plugins);
    assert!(
        entries[0].signature_status.verified(),
        "{:?}",
        entries[0].signature_status
    );
}

#[test]
fn bad_signature_is_rejected() {
    let temp = tmp("sig-bad");
    let key = test_key();
    let source = write_plugin_dir(&temp.path().join("src"), "dev.test.badsig", "1.0.0", None);
    sign_plugin_dir(&source, &key);
    // 签名后修改 manifest 的字节，模拟被篡改的包。
    let mut manifest = fs::read_to_string(source.join("plugin.toml")).unwrap();
    manifest = manifest.replace("version = \"1.0.0\"", "version = \"1.0.1\"");
    fs::write(source.join("plugin.toml"), manifest).unwrap();
    let plugins = temp.path().join("plugins");
    let error = install_package(
        &plugins,
        &source,
        &signature_options(false, Some(&key.verifying_key())),
    )
    .unwrap_err();
    assert!(error.contains("签名"), "{error}");
    assert!(!plugins.join("dev.test.badsig").exists());
}

#[test]
fn unsigned_is_rejected_when_required() {
    let temp = tmp("sig-required");
    let key = test_key();
    let source = write_plugin_dir(&temp.path().join("src"), "dev.test.unsigned", "1.0.0", None);
    let plugins = temp.path().join("plugins");
    let error = install_package(
        &plugins,
        &source,
        &signature_options(true, Some(&key.verifying_key())),
    )
    .unwrap_err();
    assert!(error.contains("未签名"), "{error}");
    assert!(!plugins.join("dev.test.unsigned").exists());
}

#[test]
fn signature_without_pubkey_is_rejected() {
    let temp = tmp("sig-nokey");
    let key = test_key();
    let source = write_plugin_dir(&temp.path().join("src"), "dev.test.nokey", "1.0.0", None);
    sign_plugin_dir(&source, &key);
    let plugins = temp.path().join("plugins");
    let error = install_package(&plugins, &source, &signature_options(false, None)).unwrap_err();
    assert!(
        error.contains("没有配置") || error.contains("公钥"),
        "{error}"
    );
    assert!(!plugins.join("dev.test.nokey").exists());
}

#[test]
fn unsigned_install_is_allowed_by_default() {
    let temp = tmp("sig-default");
    let source = write_plugin_dir(&temp.path().join("src"), "dev.test.default", "1.0.0", None);
    let plugins = temp.path().join("plugins");
    let report = install_package(
        &plugins,
        &source,
        &unsigned_options(InstallLimits::default()),
    )
    .expect("unsigned allowed by default");
    assert!(report.signature.is_unsigned());
}

#[test]
fn inspect_package_validates_without_installing() {
    let temp = tmp("inspect");
    let key = test_key();
    let source = write_plugin_dir(&temp.path().join("src"), "dev.test.inspect", "1.0.0", None);
    sign_plugin_dir(&source, &key);
    let plugins = temp.path().join("plugins");
    let info = inspect_package(
        &plugins,
        &source,
        &signature_options(true, Some(&key.verifying_key())),
    )
    .expect("inspect");
    assert_eq!(info.id, "dev.test.inspect");
    assert_eq!(
        info.signature,
        SignatureStatus::Verified {
            key_id: signature::public_key_id(&key.verifying_key())
        }
    );
    assert!(!plugins.join("dev.test.inspect").exists());
    assert_eq!(
        fs::read_dir(plugins.join(install::STAGING_DIR))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn signature_file_format_roundtrip_and_public_key_formats() {
    let key = test_key();
    let manifest = b"api = 3\n";
    let text = signature::sign_manifest(manifest, &key);
    let parsed = signature::parse_signature_file(text.as_bytes()).unwrap();
    let config = SignatureConfig::explicit(false, Some(key.verifying_key()));
    assert_eq!(
        config
            .verify_manifest(manifest, Some(text.as_bytes()))
            .unwrap(),
        SignatureStatus::Verified {
            key_id: signature::public_key_id(&key.verifying_key())
        }
    );
    assert!(signature::parse_signature_file(b"bad header\nAAAA\n").is_err());
    assert!(parsed.to_bytes().len() == 64);
    let public = key.verifying_key();
    assert!(signature::parse_public_key(&signature::public_key_file_text(&public)).is_ok());
    let hex: String = public
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert!(signature::parse_public_key(&hex).is_ok());
    let base64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        public.to_bytes(),
    );
    assert!(signature::parse_public_key(&base64).is_ok());
}
