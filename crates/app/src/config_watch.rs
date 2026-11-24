// SPDX-License-Identifier: GPL-3.0-only
//! `config.toml` 外部修改监听。
//!
//! 不引入 notify/inotify 依赖：后台线程每 500ms 读一次配置并算 FNV-1a 内容
//! hash，同时记录 mtime/len 供日志与差分；只有 hash 变化且 TOML 解析成功才把
//! 新配置交给 UI 线程。配置只有几 KB，2 次/秒读取开销可忽略；这样即使编辑器
//! 保留 mtime（或文件系统时间戳粒度粗）也不会漏掉内容变化。文件删除/权限
//! 错误/半截 TOML 都保持现状。
//!
//! 自身写入（设置页 `Config::save_to`）在 rename 成功后记录内容 hash；轮询
//! 看到相同 hash 时直接跳过，不靠 UI 再对比一次，避免“保存 → watcher 重放”
//! 的反馈回环。即使记录晚于轮询，UI 侧还会用完整 `Config` 相等性兜底。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use crate::config::Config;

/// 轮询周期。配置只有几 KB，2 次/秒读取 + hash 的开销可忽略。
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 自身写入：(规范化路径, 内容 hash)。只保留每个路径最新的一次。
static OWN_WRITES: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());

/// 记录一次自身写入。`bytes` 必须与实际 rename 到目标文件的内容完全一致。
pub(crate) fn note_own_write(path: &Path, bytes: &[u8]) {
    let key = absolute_key(path);
    let mut writes = OWN_WRITES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writes.retain(|(p, _)| p != &key);
    writes.push((key, hash_bytes(bytes)));
}

/// 消费匹配的自身写入标记；路径不同或 hash 不同都不算。
fn consume_own_write(path: &Path, hash: u64) -> bool {
    let key = absolute_key(path);
    let mut writes = OWN_WRITES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(pos) = writes.iter().position(|(p, h)| p == &key && *h == hash) {
        writes.swap_remove(pos);
        true
    } else {
        false
    }
}

fn absolute_key(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// 小型非加密内容 hash：只用于判断“文件内容有没有变”，无需抗碰撞。
pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    // FNV-1a 64-bit。
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    mtime: Option<SystemTime>,
    len: u64,
    hash: u64,
}

/// 单文件轮询状态。拆成纯状态机，UI 不需要即可单测。
pub(crate) struct WatchState {
    path: PathBuf,
    last: Option<Fingerprint>,
    last_issue: Option<String>,
}

#[derive(Debug)]
pub(crate) enum PollOutcome {
    NoChange,
    Reload {
        config: Config,
        hash: u64,
    },
    /// 内容变了但 TOML 非法；调用方必须保留当前配置并且不要写盘。
    Invalid {
        error: String,
    },
    /// 变化来自本进程 `Config::save_to`，不应再套用一次。
    OwnWriteSkipped,
    /// 文件被删除/暂时看不到；保持当前配置。
    Missing,
    /// 权限/IO 错误；保持当前配置。
    ReadError {
        error: String,
    },
}

impl WatchState {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            last: None,
            last_issue: None,
        }
    }

    /// 启动基线：只记录当前文件，不触发一次重载（启动时 `Config::load` 已经读过）。
    ///
    /// 消费基线对应的自身写入标记：启动迁移也会 `save_to`，若不消费，之后外部
    /// 把内容改回迁移后的版本时会被旧标记误判成“自身写入”而跳过。
    pub(crate) fn seed(&mut self) -> std::io::Result<()> {
        let meta = std::fs::metadata(&self.path)?;
        let bytes = std::fs::read(&self.path)?;
        let hash = hash_bytes(&bytes);
        let _ = consume_own_write(&self.path, hash);
        self.last = Some(Fingerprint {
            mtime: meta.modified().ok(),
            len: meta.len(),
            hash,
        });
        Ok(())
    }

    fn note_issue(&mut self, issue: String) -> bool {
        if self.last_issue.as_deref() == Some(issue.as_str()) {
            false
        } else {
            self.last_issue = Some(issue);
            true
        }
    }

    /// 查一次。`last` 只在“成功读到完整内容”后更新；删除/读失败不会清除
    /// 已生效配置，也不会让下一次恢复被误当成新文件。
    pub(crate) fn poll(&mut self) -> PollOutcome {
        let meta = match std::fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let issue = format!("配置文件不存在（{}），保持当前配置", self.path.display());
                if self.note_issue(issue) {
                    return PollOutcome::Missing;
                }
                return PollOutcome::NoChange;
            }
            Err(e) => {
                let issue = format!(
                    "读取 {} 元数据失败（{e}），保持当前配置",
                    self.path.display()
                );
                if self.note_issue(issue) {
                    return PollOutcome::ReadError {
                        error: e.to_string(),
                    };
                }
                return PollOutcome::NoChange;
            }
        };

        let mtime = meta.modified().ok();
        // 内容 hash 是最终判据：必须在 metadata 相同的情况下也读文件，否则
        // 保存 mtime 的编辑器/粗粒度文件系统会让热更新永远不触发。
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let issue = format!(
                    "配置文件在读取时被删除（{}），保持当前配置",
                    self.path.display()
                );
                if self.note_issue(issue) {
                    return PollOutcome::Missing;
                }
                return PollOutcome::NoChange;
            }
            Err(e) => {
                let issue = format!("读取 {} 失败（{e}），保持当前配置", self.path.display());
                if self.note_issue(issue) {
                    return PollOutcome::ReadError {
                        error: e.to_string(),
                    };
                }
                return PollOutcome::NoChange;
            }
        };
        self.last_issue = None;

        let hash = hash_bytes(&bytes);
        if let Some(last) = self.last {
            if last.hash == hash {
                // 只是 touch/权限位变化，不重载、不重绘。同时消费同 hash 的自身
                // 写入标记，避免它残留到用户以后把文件改回这个版本时误跳过。
                let _ = consume_own_write(&self.path, hash);
                self.last = Some(Fingerprint {
                    mtime,
                    len: meta.len(),
                    hash,
                });
                return PollOutcome::NoChange;
            }
        }
        self.last = Some(Fingerprint {
            mtime,
            len: meta.len(),
            hash,
        });

        if consume_own_write(&self.path, hash) {
            return PollOutcome::OwnWriteSkipped;
        }

        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => {
                let issue = format!("{} 不是 UTF-8（{e}），保持当前配置", self.path.display());
                self.note_issue(issue);
                return PollOutcome::Invalid {
                    error: "file is not valid UTF-8".to_string(),
                };
            }
        };
        match Config::parse_text(&text) {
            Ok(cfg) => PollOutcome::Reload { config: cfg, hash },
            Err(e) => {
                let issue = format!("{} 解析失败，保留当前配置: {e}", self.path.display());
                self.note_issue(issue);
                PollOutcome::Invalid {
                    error: e.to_string(),
                }
            }
        }
    }
}

/// 后台轮询线程。Drop 时停止并 join；最长等待一个轮询周期。
pub struct ConfigWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ConfigWatcher {
    pub fn start(
        path: PathBuf,
        on_reload: impl Fn(Config, u64) + Send + 'static,
    ) -> std::io::Result<Self> {
        let mut state = WatchState::new(path.clone());
        if let Err(e) = state.seed() {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[config] watcher 启动基线读取失败: {e}");
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::Builder::new()
            .name("config-watch".into())
            .spawn(move || {
                while !stop_thread.load(Ordering::Acquire) {
                    if !sleep_interruptible(&stop_thread, POLL_INTERVAL) {
                        break;
                    }
                    match state.poll() {
                        PollOutcome::NoChange => {}
                        PollOutcome::Reload { config, hash } => {
                            eprintln!("[config] 检测到外部修改，应用热更新");
                            on_reload(config, hash);
                        }
                        PollOutcome::Invalid { error } => {
                            eprintln!("[config] 外部修改解析失败，保留当前配置、不写文件: {error}");
                        }
                        PollOutcome::OwnWriteSkipped => {
                            eprintln!("[config] 检测到自身写入，跳过热更新（避免回环）");
                        }
                        PollOutcome::Missing => {
                            eprintln!("[config] 配置文件不存在，保持当前配置（等待恢复）");
                        }
                        PollOutcome::ReadError { error } => {
                            eprintln!("[config] 读取配置失败，保持当前配置: {error}");
                        }
                    }
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// UI 线程套用前再确认磁盘内容仍是这次解析的版本，避免“事件排队期间设置页
/// 又保存了一次”导致旧事件覆盖新配置。读失败返回 false（宁可跳过，下一次
/// 轮询会带来最新内容）。
pub(crate) fn file_hash_matches(path: &Path, expected: u64) -> bool {
    std::fs::read(path).is_ok_and(|bytes| hash_bytes(&bytes) == expected)
}

fn sleep_interruptible(stop: &AtomicBool, total: Duration) -> bool {
    const SLICE: Duration = Duration::from_millis(25);
    let mut left = total;
    while !left.is_zero() {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        let step = left.min(SLICE);
        thread::sleep(step);
        left = left.saturating_sub(step);
    }
    !stop.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-cfgwatch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seeded(path: &Path) -> WatchState {
        let mut state = WatchState::new(path.to_path_buf());
        state.seed().unwrap();
        state
    }

    #[test]
    fn mtime_touch_does_not_reload_but_content_change_does() {
        let dir = tmpdir("mtime");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\ntheme = \"latte\"\n").unwrap();
        let mut state = seeded(&path);

        // 只改 mtime：metadata 变了，但 hash 相同，不应产生 Reload。
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(2)),
        )
        .unwrap();
        assert!(matches!(state.poll(), PollOutcome::NoChange));

        std::fs::write(&path, "version = 1\ntheme = \"nord\"\n").unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => assert_eq!(config.theme, "nord"),
            other => panic!("expected Reload, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_hash_detects_change_when_mtime_is_preserved() {
        let dir = tmpdir("hash-fallback");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\nfont_size = 15\n").unwrap();
        let mut state = seeded(&path);
        let original = std::fs::metadata(&path).unwrap();

        // 同长度、把 mtime 改回原值：模拟保留 metadata 的写入。
        std::fs::write(&path, "version = 1\nfont_size = 19\n").unwrap();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(original.modified().unwrap()))
            .unwrap();

        match state.poll() {
            PollOutcome::Reload { config, .. } => {
                assert!((config.font_size - 19.0).abs() < f32::EPSILON)
            }
            other => panic!("content hash should catch preserved mtime, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_toml_is_rejected_once_and_next_valid_change_applies() {
        let dir = tmpdir("invalid");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\ntheme = \"latte\"\n").unwrap();
        let mut state = seeded(&path);

        std::fs::write(&path, "version = 1\ntheme = \n").unwrap();
        assert!(matches!(state.poll(), PollOutcome::Invalid { .. }));
        assert!(
            matches!(state.poll(), PollOutcome::NoChange),
            "坏内容不应每 500ms 刷日志/重试"
        );

        std::fs::write(&path, "version = 1\ntheme = \"nord\"\n").unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => assert_eq!(config.theme, "nord"),
            other => panic!("expected Reload after fix, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn own_write_is_skipped_without_feedback_loop() {
        let dir = tmpdir("own-write");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\nfont_size = 15\n").unwrap();
        let mut state = seeded(&path);

        let text = "version = 1\nfont_size = 20\n";
        std::fs::write(&path, text).unwrap();
        note_own_write(&path, text.as_bytes());
        assert!(matches!(state.poll(), PollOutcome::OwnWriteSkipped));
        // 标记已消费；之后真正的外部修改仍然生效。
        std::fs::write(&path, "version = 1\nfont_size = 21\n").unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => {
                assert!((config.font_size - 21.0).abs() < f32::EPSILON)
            }
            other => panic!("expected Reload after own-write marker consumed, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_keeps_state_and_later_recreation_reloads() {
        let dir = tmpdir("missing");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\ntheme = \"latte\"\n").unwrap();
        let mut state = seeded(&path);

        std::fs::remove_file(&path).unwrap();
        assert!(matches!(state.poll(), PollOutcome::Missing));
        assert!(
            matches!(state.poll(), PollOutcome::NoChange),
            "删除后不重复报错"
        );

        std::fs::write(&path, "version = 1\ntheme = \"nord\"\n").unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => assert_eq!(config.theme, "nord"),
            other => panic!("expected Reload after recreate, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_hash_is_stable_and_sensitive() {
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
        assert_ne!(hash_bytes(b""), hash_bytes(b"\0"));
    }

    #[test]
    fn stale_own_write_marker_does_not_swallow_later_external_revert() {
        let dir = tmpdir("stale-own");
        let path = dir.join("config.toml");
        let original = "version = 1\nfont_size = 15\n";
        std::fs::write(&path, original).unwrap();
        let mut state = seeded(&path);

        // 设置页写入与当前内容相同：watcher 看不到内容变化，但标记必须被消费。
        note_own_write(&path, original.as_bytes());
        assert!(matches!(state.poll(), PollOutcome::NoChange));

        std::fs::write(&path, "version = 1\nfont_size = 19\n").unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => {
                assert!((config.font_size - 19.0).abs() < f32::EPSILON)
            }
            other => panic!("expected Reload, got {other:?}"),
        }

        // 外部再改回原内容：不能命中已消费的旧自身写入标记。
        std::fs::write(&path, original).unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => {
                assert!((config.font_size - 15.0).abs() < f32::EPSILON)
            }
            other => panic!("expected Reload after external revert, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_error_keeps_state_and_recovers_without_panic() {
        let dir = tmpdir("read-error");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\nfont_size = 15\n").unwrap();
        let mut state = seeded(&path);

        // 用目录制造稳定的 IO 错误（EISDIR），不依赖当前用户是否 root 的权限位。
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(state.poll(), PollOutcome::ReadError { .. }));
        assert!(
            matches!(state.poll(), PollOutcome::NoChange),
            "同一错误不重复刷屏"
        );

        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "version = 1\nfont_size = 22\n").unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => {
                assert!((config.font_size - 22.0).abs() < f32::EPSILON)
            }
            other => panic!("expected Reload after IO recovery, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_consumes_own_write_marker_from_startup_migration() {
        let dir = tmpdir("seed-own");
        let path = dir.join("config.toml");
        let text = "version = 1\nfont_size = 17\n";
        std::fs::write(&path, text).unwrap();
        note_own_write(&path, text.as_bytes());
        let mut state = seeded(&path); // 启动基线消费标记

        std::fs::write(&path, "version = 1\nfont_size = 18\n").unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => {
                assert!((config.font_size - 18.0).abs() < f32::EPSILON)
            }
            other => panic!("external change after startup must reload, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watcher_thread_delivers_reload_to_callback() {
        let dir = tmpdir("thread");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\nfont_size = 15\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = ConfigWatcher::start(path.clone(), move |cfg, _hash| {
            let _ = tx.send(cfg.font_size);
        })
        .unwrap();
        std::fs::write(&path, "version = 1\nfont_size = 23\n").unwrap();
        let size = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("watcher callback");
        assert!((size - 23.0).abs() < f32::EPSILON);
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_reloads_to_defaults_without_panic() {
        let dir = tmpdir("empty");
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\n").unwrap();
        let mut state = seeded(&path);
        // 空文件仍是合法 TOML：所有字段走 serde default，不能 panic。
        std::fs::write(&path, "").unwrap();
        match state.poll() {
            PollOutcome::Reload { config, .. } => {
                assert_eq!(config.theme, "auto");
                assert!((config.font_size - 15.0).abs() < f32::EPSILON);
            }
            other => panic!("expected Reload of defaults, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
