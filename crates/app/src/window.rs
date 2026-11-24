// SPDX-License-Identifier: GPL-3.0-only
//! Legacy 多进程窗口 fallback。默认路径已经是同进程 WindowRegistry：
//! `xtervo` 的 secondary 转发 `open_window`，`window.new`/Ctrl+Shift+N 在 UI
//! 线程新建 MainWindow，不再 spawn 子进程。
//!
//! 只有显式 `XTERVO_MULTI_WINDOW_INPROC=0` / `XTERVO_MULTI_WINDOW_LEGACY=1`
//! 才走这里。子进程带 `XTERVO_SINGLE_INSTANCE=0` 绕过 primary 锁，否则它会
//! 被当成 secondary 转发回父进程、不会开窗。配置并发仍是“最后写入者生效”。

use std::path::Path;
use std::process::{Command, Stdio};

/// 子窗口进程标记：只用于日志区分，不触发任何自动新建窗口逻辑。
pub const WINDOW_FLAG: &str = "--window";

/// legacy 子进程必须绕过单实例锁，否则 `--window` 会被转发回父进程。
pub const LEGACY_BYPASS_ENV: &str = "XTERVO_SINGLE_INSTANCE";

/// 构造“新窗口”命令行。纯逻辑，便于单测（测试不真正 spawn）。
pub fn new_window_command(exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.arg(WINDOW_FLAG);
    cmd.env(LEGACY_BYPASS_ENV, "0");
    // 新进程启动后会建立自己的日志文件；stdout/stderr 不能继承父进程——
    // 父进程的 stderr 在 logging::init 后是 log tee 的管道，继承会混淆两份日志。
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // 独立会话/进程组：父窗口关闭、终端 Ctrl+C 都不会连带新窗口。
            libc::setsid();
            Ok(())
        });
    }
    cmd
}

/// 启动新窗口进程，立即返回 pid（不 wait、不阻塞当前事件循环）。
///
/// 用一个后台线程 `wait` 子进程，避免父进程长期运行时留下僵尸进程；
/// 线程起不来也不影响窗口本身（最坏情况只是留下一个 defunct 条目）。
pub fn spawn_new_window() -> std::io::Result<u32> {
    let exe = std::env::current_exe()?;
    let mut child = new_window_command(&exe).spawn()?;
    let pid = child.id();
    let reaper = std::thread::Builder::new()
        .name(format!("window-{pid}"))
        .spawn(move || {
            let _ = child.wait();
        });
    if let Err(e) = reaper {
        eprintln!("[ui] new window reaper thread failed pid={pid}: {e}");
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_self_with_only_window_marker() {
        let exe = Path::new("/opt/xtervo/bin/xtervo");
        let cmd = new_window_command(exe);
        assert_eq!(cmd.get_program(), exe.as_os_str());
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        // 只带标记：父进程的 --restore/--shell 等一次性参数不能漏给新窗口。
        assert_eq!(args, vec![std::ffi::OsStr::new(WINDOW_FLAG)]);
    }

    #[test]
    fn legacy_child_bypasses_single_instance_lock() {
        let cmd = new_window_command(Path::new("/opt/xtervo/bin/xtervo"));
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            envs.iter()
                .any(|(key, value)| { key == LEGACY_BYPASS_ENV && value.as_deref() == Some("0") }),
            "legacy child must not become secondary: {envs:?}"
        );
    }

    #[test]
    fn marker_matches_parsed_flag() {
        // 命令行常量与 parse_args 的匹配字符串必须一致（见 main.rs 的
        // window_flag_marks_child_process_without_parent_args 测试）。
        assert_eq!(WINDOW_FLAG, "--window");
        assert!(!WINDOW_FLAG.is_empty());
    }
}
