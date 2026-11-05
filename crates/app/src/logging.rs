// SPDX-License-Identifier: GPL-3.0-only
//! 运行日志：把 stderr（含 panic）同时写进按次启动的日志文件。
//!
//! 现有代码大量使用 `eprintln!`，这里不逐一改造，而是在启动时用 pipe + dup2
//! 把 fd 2 接到一个 tee 线程：原始 stderr 与日志文件各写一份。这样：
//! - 正常启动/崩溃/panic 的日志都能在文件里看到；
//! - 用户报问题时只要给出日志路径（`tervo --diagnose` 会打印）；
//! - `TERVO_LOG=0` 可完全关闭；`TERVO_LOG_DIR` 可改目录。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static INIT: Once = Once::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

const MAX_FILES: usize = 8;

/// 日志目录：`TERVO_LOG_DIR` > `XDG_STATE_HOME/tervo/logs` > `~/.local/state/tervo/logs`。
pub fn dir() -> PathBuf {
    if let Some(p) = std::env::var_os("TERVO_LOG_DIR") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(p).join("tervo").join("logs");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("state").join("tervo").join("logs")
}

pub fn path() -> Option<PathBuf> {
    LOG_PATH.get().cloned()
}

pub fn init() {
    INIT.call_once(|| {
        // panic 始终打印（哪怕 TERVO_LOG=0），便于终端里直接看到。
        std::panic::set_hook(Box::new(|info| {
            let bt = std::backtrace::Backtrace::force_capture();
            eprintln!("\n[panic] {info}\nbacktrace:\n{bt}");
        }));

        if std::env::var("TERVO_LOG").ok().as_deref() == Some("0") {
            return;
        }
        let dir = dir();
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("[log] 创建日志目录失败 {}: {e}", dir.display());
            return;
        }
        rotate(&dir);
        let path = dir.join(format!("tervo-{}.log", now_secs()));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(
                    f,
                    "=== Tervo {} started (epoch {}) ===",
                    env!("CARGO_PKG_VERSION"),
                    now_secs()
                );
                let _ = writeln!(f, "pid={} args={:?}", std::process::id(), std::env::args());
                for key in [
                    "DISPLAY",
                    "WAYLAND_DISPLAY",
                    "XDG_SESSION_TYPE",
                    "XDG_CURRENT_DESKTOP",
                    "SLINT_BACKEND",
                    "TERM",
                    "SHELL",
                    "TERVO_CONFIG",
                    "TERVO_PROFILES",
                ] {
                    let _ = writeln!(f, "{key}={:?}", std::env::var(key).ok());
                }
                let _ = LOG_PATH.set(path.clone());
                tee_stderr(&path);
                eprintln!("[log] 日志文件: {}", path.display());
            },
            Err(e) => eprintln!("[log] 打开日志失败 {}: {e}", path.display()),
        }
    });
}

/// 最后 `n` 行（诊断用；文件很小，直接读）。
pub fn tail(n: usize) -> Vec<String> {
    let Some(path) = path() else { return Vec::new() };
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn rotate(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut files: Vec<(u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?;
            let stamp = name.strip_prefix("tervo-")?.strip_suffix(".log")?.parse().ok()?;
            Some((stamp, p))
        })
        .collect();
    files.sort_by_key(|(t, _)| *t);
    while files.len() >= MAX_FILES {
        let (_, p) = files.remove(0);
        let _ = fs::remove_file(p);
    }
}

/// 把 fd 2 接到 pipe；后台线程把内容同时写回原 stderr 和日志文件。
fn tee_stderr(path: &Path) {
    use std::os::fd::FromRawFd;
    unsafe {
        let saved = libc::dup(2);
        if saved < 0 {
            return;
        }
        let mut fds = [0i32; 2];
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            libc::close(saved);
            return;
        }
        if libc::dup2(fds[1], 2) < 0 {
            libc::close(saved);
            libc::close(fds[0]);
            libc::close(fds[1]);
            return;
        }
        libc::close(fds[1]);
        let read_fd = fds[0];
        let log_path = path.to_path_buf();
        let spawned = std::thread::Builder::new().name("log-tee".into()).spawn(move || {
            let mut pipe = File::from_raw_fd(read_fd);
            let mut orig = File::from_raw_fd(saved);
            let mut log = OpenOptions::new().create(true).append(true).open(&log_path).ok();
            let mut buf = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut pipe, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = orig.write_all(&buf[..n]);
                        if let Some(f) = log.as_mut() {
                            let _ = f.write_all(&buf[..n]);
                        }
                    },
                }
            }
        });
        if let Err(e) = spawned {
            // 线程起不来就恢复 stderr，避免写满 pipe 后卡死。
            eprintln!("[log] tee 线程启动失败: {e}");
            let _ = libc::dup2(saved, 2);
            libc::close(saved);
        }
    }
}
