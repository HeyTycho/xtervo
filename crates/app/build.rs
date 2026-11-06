// SPDX-License-Identifier: GPL-3.0-only
//! Slint UI 编译 + 构建元信息（`tervo --version` 的 commit / 构建时间 / 目标平台）。
//!
//! 可复现构建：设置 `SOURCE_DATE_EPOCH` 时用它作为 `built` 时间；
//! `scripts/package.sh` 默认填 HEAD 的提交时间，因此同一 commit 重复打包
//! 二进制与包的字节一致。未设置时依次使用 `TERVO_BUILD_TIME` / 当前 UTC。
//!
//! 打包脚本可显式传 `TERVO_GIT_COMMIT` / `TERVO_GIT_DIRTY`，避免
//! “先构建、后提交”时二进制里记录旧 commit。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 单行化，避免外部环境变量里的换行破坏 `cargo:rustc-env`。
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Unix 秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC，不引入 chrono 依赖）。
fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);

    // Howard Hinnant 的 civil_from_days：era/年/月/日互转。
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

/// 让提交/切分支后 cargo 重新运行 build.rs，否则 `--version` 会一直显示旧 commit。
fn rerun_on_git_change(root: &Path) {
    let Some(git_dir) = git(root, &["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let git_dir = PathBuf::from(git_dir);
    for name in ["HEAD", "packed-refs"] {
        let path = git_dir.join(name);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(reference) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
        let path = git_dir.join(&reference);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn main() {
    slint_build::compile("ui/main.slint").expect("Slint UI 编译失败");

    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/app 应位于 workspace 根下")
        .to_path_buf();

    let commit = std::env::var("TERVO_GIT_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| one_line(&s))
        .or_else(|| git(&root, &["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = match std::env::var("TERVO_GIT_DIRTY") {
        Ok(v) => matches!(v.trim(), "1" | "true" | "yes"),
        Err(_) => git(&root, &["status", "--porcelain", "--untracked-files=no"]).is_some(),
    };
    let source_date = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let built = if let Some(epoch) = source_date {
        format_utc(epoch)
    } else {
        std::env::var("TERVO_BUILD_TIME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| one_line(&s))
            .unwrap_or_else(now_utc)
    };
    let target = std::env::var("TARGET").unwrap_or_else(|_| {
        format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
    });

    println!("cargo:rustc-env=TERVO_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=TERVO_GIT_DIRTY={}", u8::from(dirty));
    println!("cargo:rustc-env=TERVO_BUILD_TIME={built}");
    println!("cargo:rustc-env=TERVO_BUILD_TARGET={target}");

    for key in [
        "SOURCE_DATE_EPOCH",
        "TERVO_BUILD_TIME",
        "TERVO_GIT_COMMIT",
        "TERVO_GIT_DIRTY",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    rerun_on_git_change(&root);
}
