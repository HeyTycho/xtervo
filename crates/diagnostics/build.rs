// SPDX-License-Identifier: GPL-3.0-only
//! Build metadata for the `xtervo-doctor` CLI (version/commit/build/target).
//!
//! This mirrors the app's build.rs contract: `SOURCE_DATE_EPOCH` wins, then
//! `XTERVO_BUILD_TIME`, then the current UTC time. The build script only runs
//! local `git` commands; it performs no network access.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
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
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

fn rerun_on_git_change(root: &Path) {
    let Some(git_dir) = git(root, &["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let git_dir = PathBuf::from(git_dir);
    // In a linked worktree HEAD lives in the per-worktree git dir, while
    // branch refs live in the common dir. Watch both, otherwise a commit in
    // this worktree leaves a stale commit/dirty value embedded in the binary.
    let common_dir = git(root, &["rev-parse", "--git-common-dir"])
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .unwrap_or_else(|| git_dir.clone());
    for base in [&git_dir, &common_dir] {
        for name in ["HEAD", "packed-refs"] {
            let path = base.join(name);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    if let Some(reference) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
        for base in [&git_dir, &common_dir] {
            let path = base.join(&reference);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/diagnostics 应位于 workspace 根下")
        .to_path_buf();

    let commit = std::env::var("XTERVO_GIT_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| one_line(&value))
        .or_else(|| git(&root, &["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = match std::env::var("XTERVO_GIT_DIRTY") {
        Ok(value) => matches!(value.trim(), "1" | "true" | "yes"),
        Err(_) => git(&root, &["status", "--porcelain", "--untracked-files=no"]).is_some(),
    };
    let source_date = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok());
    let built = if let Some(epoch) = source_date {
        format_utc(epoch)
    } else {
        std::env::var("XTERVO_BUILD_TIME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| one_line(&value))
            .unwrap_or_else(now_utc)
    };
    let target = std::env::var("TARGET")
        .unwrap_or_else(|_| format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS));

    println!("cargo:rustc-env=XTERVO_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=XTERVO_GIT_DIRTY={}", u8::from(dirty));
    println!("cargo:rustc-env=XTERVO_BUILD_TIME={built}");
    println!("cargo:rustc-env=XTERVO_BUILD_TARGET={target}");

    for key in [
        "SOURCE_DATE_EPOCH",
        "XTERVO_BUILD_TIME",
        "XTERVO_GIT_COMMIT",
        "XTERVO_GIT_DIRTY",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    rerun_on_git_change(&root);
}
