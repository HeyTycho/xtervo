// SPDX-License-Identifier: GPL-3.0-only
//! 广播输入的破坏性命令识别（UI-PANE-011）。
//!
//! 只在“广播开启且当前 Tab 有 ≥2 个 pane”时用于二次确认；不做安全沙箱，
//! 只是把明显会破坏机器/数据的命令拦一次，避免一次回车在 N 个 pane 里同时执行。
//!
//! 判定故意保守：命令必须出现在**命令位**（行首/`;`/`&&`/`||`/`|` 之后，
//! 允许 `sudo`/`doas`/`env` 前缀）。因此 `echo "rm -rf /"` 不会弹窗，
//! 而 `echo hi; rm -rf ~` 会命中。

/// 命中时返回一句可展示的原因；`None` = 普通命令。
pub fn dangerous_reason(text: &str) -> Option<&'static str> {
    let norm = normalize(text);
    if norm.is_empty() {
        return None;
    }
    if is_fork_bomb(&norm) {
        return Some("fork bomb");
    }
    for command in commands(&norm) {
        let tokens: Vec<&str> = command.split(' ').filter(|t| !t.is_empty()).collect();
        let tokens = strip_prefixes(&tokens);
        let Some((head, args)) = tokens.split_first() else {
            continue;
        };
        let head = head.rsplit('/').next().unwrap_or(head);
        match head {
            "rm" if destructive_rm(args) => {
                return Some("recursive force delete (rm -rf)");
            }
            "mkfs" => return Some("format filesystem (mkfs)"),
            _ if head.starts_with("mkfs.") => return Some("format filesystem (mkfs)"),
            "dd" if args.iter().any(|a| a.starts_with("of=/dev/")) => {
                return Some("raw write to a block device (dd of=/dev/…)");
            }
            "fdisk" | "parted" | "wipefs" if args.iter().any(|a| a.contains("/dev/")) => {
                return Some("partition table change");
            }
            "shutdown" | "reboot" | "halt" | "poweroff" => {
                return Some("power state change");
            }
            "chmod" if recursive_world_write(args) => {
                return Some("recursive chmod 777");
            }
            "truncate" if truncates_to_zero(args) => {
                return Some("truncate to zero (truncate -s 0)");
            }
            _ => {}
        }
    }
    // 重定向覆盖整块设备：`> /dev/sda`。命令位之外也可能出现在 `sudo tee` 前，
    // 这里按整串兜底（少见，不会误伤普通 `echo > file`）。
    if norm.contains(">/dev/sd") || norm.contains("> /dev/sd") || norm.contains(">/dev/nvme") {
        return Some("overwrite a block device");
    }
    None
}

/// 统一小写、压缩行内空白，但**保留换行**作为命令分隔符。
///
/// 多行粘贴（`set -e\nmkfs.ext4 /dev/sda1`）如果把换行拍平成空格，
/// 整段会被当成一条 `set` 命令而漏检。
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.replace('\r', "\n").split('\n') {
        if !out.is_empty() {
            out.push('\n');
        }
        let collapsed: Vec<&str> = line.split_whitespace().collect();
        out.push_str(&collapsed.join(" "));
    }
    out.to_ascii_lowercase()
}

fn is_fork_bomb(norm: &str) -> bool {
    // 只在命令位（行首或 `;`/`&&`/`||`/`|` 之后）识别：`printf ':(){...'`
    // 只是打印文本，不能弹确认。
    let trimmed = norm.trim();
    for (idx, _) in trimmed.match_indices(":(){") {
        let prefix = trimmed[..idx].trim_end();
        if prefix.is_empty()
            || prefix.ends_with(';')
            || prefix.ends_with('&')
            || prefix.ends_with('|')
        {
            return true;
        }
    }
    false
}

/// 按 shell 命令分隔符（`;` `&&` `||` `|` 换行）拆成一条条命令。
fn commands(norm: &str) -> Vec<String> {
    norm.replace("&&", ";")
        .replace("||", ";")
        .split([';', '|', '&', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 跳过 `sudo` / `doas` / `env`（以及 sudo 的短选项），返回真正的命令 token。
fn strip_prefixes<'a>(tokens: &'a [&'a str]) -> &'a [&'a str] {
    let mut rest = tokens;
    loop {
        match rest.first().copied() {
            Some("sudo") | Some("doas") => rest = &rest[1..],
            Some("env") => rest = &rest[1..],
            // sudo -E / sudo -u root：跳过选项和它的参数（保守：只跳过 -x 单段）
            Some(t) if t.starts_with('-') => rest = &rest[1..],
            _ => return rest,
        }
    }
}

fn destructive_rm(args: &[&str]) -> bool {
    let mut recursive = false;
    let mut force = false;
    for arg in args {
        if arg.starts_with("--") {
            recursive |= *arg == "--recursive";
            force |= *arg == "--force";
        } else if arg.starts_with('-') && arg.len() > 1 {
            recursive |= arg.contains('r');
            force |= arg.contains('f');
        } else {
            // 第一个操作数之后不再有选项；但 `rm -rf`（无操作数）仍要命中。
            break;
        }
    }
    recursive && force
}

fn recursive_world_write(args: &[&str]) -> bool {
    let mut recursive = false;
    let mut world = false;
    for arg in args {
        if arg.starts_with("--") {
            recursive |= *arg == "--recursive";
        } else if arg.starts_with('-') && arg.len() > 1 {
            recursive |= arg.contains('r');
        }
        world |= *arg == "777";
    }
    recursive && world
}

fn truncates_to_zero(args: &[&str]) -> bool {
    let mut size = false;
    for arg in args {
        if *arg == "-s" || *arg == "--size" {
            size = true;
        } else if size && (*arg == "0" || *arg == "0b") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::dangerous_reason;

    #[test]
    fn flags_rm_rf_in_all_common_spellings() {
        for cmd in [
            "rm -rf /",
            "sudo rm -fr ~/project",
            "rm -r -f build",
            "rm --recursive --force /tmp/x",
            "ls; rm -Rf .",
            "cd /tmp && rm -rf *",
            "  RM -RF  /  ",
        ] {
            assert!(dangerous_reason(cmd).is_some(), "应命中: {cmd}");
        }
    }

    #[test]
    fn ordinary_rm_and_quoted_text_do_not_warn() {
        for cmd in [
            "rm notes.txt",
            "rm -r build",
            "rm -f stale.log",
            "echo 'rm -rf /'",
            "ls -la",
            "git status",
            "cargo test",
        ] {
            assert!(dangerous_reason(cmd).is_none(), "不应命中: {cmd}");
        }
    }

    #[test]
    fn flags_disk_and_power_commands() {
        for cmd in [
            "mkfs.ext4 /dev/sda1",
            "sudo mkfs -t xfs /dev/nvme0n1p2",
            "dd if=/dev/zero of=/dev/sda bs=1M",
            "fdisk /dev/sdb",
            "wipefs -a /dev/sdc",
            "shutdown -h now",
            "sudo reboot",
            "poweroff",
            "chmod -R 777 /",
            "truncate -s 0 important.log",
            ":(){ :|:& };:",
            "echo hi > /dev/sda",
        ] {
            assert!(dangerous_reason(cmd).is_some(), "应命中: {cmd}");
        }
    }

    #[test]
    fn does_not_flag_harmless_lookalikes() {
        for cmd in [
            "dd if=disk.img of=/tmp/copy.img",
            "chmod 755 script.sh",
            "chmod -R 755 assets",
            "fdisk --help",
            "truncate -s 1M log.txt",
            "grep shutdown /var/log/syslog",
            "echo 'dd if=/dev/zero of=/dev/sda'",
            "printf ':(){ :|:& };:'",
        ] {
            assert!(dangerous_reason(cmd).is_none(), "不应命中: {cmd}");
        }
    }

    #[test]
    fn empty_or_whitespace_is_not_dangerous() {
        assert!(dangerous_reason("").is_none());
        assert!(dangerous_reason("   \n\t ").is_none());
    }
}
