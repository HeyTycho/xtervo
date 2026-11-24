//! 一次性迁移旧 `tervo` 用户数据到 `xtervo`。
//!
//! 只做**复制**，不删除旧目录：旧二进制仍能启动，新目录也不会因为复制中断
//! 而处于半迁移状态（先写临时目录，成功后 rename）。新目录已存在时完全不动，
//! 避免覆盖用户后来改过的配置。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn config_root() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|home| home.join(".config")))
}

fn state_root() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|home| home.join(".local").join("state")))
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)?;
            #[cfg(not(unix))]
            let _ = target;
        }
    }
    Ok(())
}

fn migrate_dir(root: Option<PathBuf>, old_name: &str, new_name: &str) -> Option<PathBuf> {
    let root = root?;
    let old = root.join(old_name);
    let new = root.join(new_name);
    if !old.is_dir() || new.exists() {
        return None;
    }
    let tmp = root.join(format!(".{new_name}.migrating-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    match copy_tree(&old, &tmp).and_then(|()| fs::rename(&tmp, &new)) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&new, fs::Permissions::from_mode(0o700));
            }
            eprintln!("[migrate] copied {} -> {}", old.display(), new.display());
            Some(new)
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&tmp);
            eprintln!(
                "[migrate] 无法迁移 {} -> {}: {error}",
                old.display(),
                new.display()
            );
            None
        }
    }
}

/// 迁移 config（profiles/vault/themes/history）与 state（session/workspace）。
pub fn migrate_legacy_user_data() {
    migrate_dir(config_root(), "tervo", "xtervo");
    migrate_dir(state_root(), "tervo", "xtervo");
}

/// 兼容旧进程/脚本：把 `TERVO_*` 环境变量复制成 `XTERVO_*`（新名优先）。
pub fn alias_legacy_env() {
    let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    for (key, value) in vars {
        let Some(key) = key.to_str() else { continue };
        let Some(rest) = key.strip_prefix("TERVO_") else {
            continue;
        };
        let new_key = format!("XTERVO_{rest}");
        if std::env::var_os(&new_key).is_none() {
            std::env::set_var(new_key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_tree_preserves_nested_files() {
        let base = std::env::temp_dir().join(format!(
            "xtervo-migrate-{}-{}",
            std::process::id(),
            crate::logging::now_ms()
        ));
        let src = base.join("old");
        let dst = base.join("new");
        std::fs::create_dir_all(src.join("themes")).unwrap();
        std::fs::write(src.join("config.toml"), "a = 1\n").unwrap();
        std::fs::write(src.join("themes/one.toml"), "b = 2\n").unwrap();
        copy_tree(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("config.toml")).unwrap(),
            "a = 1\n"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("themes/one.toml")).unwrap(),
            "b = 2\n"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_dir_skips_existing_destination() {
        let base = std::env::temp_dir().join(format!(
            "xtervo-migrate-existing-{}-{}",
            std::process::id(),
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(base.join("old")).unwrap();
        std::fs::write(base.join("old/a"), "old").unwrap();
        std::fs::create_dir_all(base.join("new")).unwrap();
        std::fs::write(base.join("new/a"), "new").unwrap();
        assert!(migrate_dir(Some(base.clone()), "old", "new").is_none());
        assert_eq!(std::fs::read_to_string(base.join("new/a")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&base);
    }
}
