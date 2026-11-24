// SPDX-License-Identifier: GPL-3.0-only
//! Login Scripts（SSH-013）的最小模型：连接 shell 建立后按序发送命令。
//!
//! Profile 里保存的是**原始文本行**（每行一条脚本），写盘/UI 编辑都不改写
//! 用户的输入；连接时才解析成 [`LoginStep`]。这样格式升级不会静默重写
//! `profiles.toml`，解析失败也能在保存表单时直接给出具体行号。
//!
//! 单行格式（字段用 `;` 分隔，命令里的字面量 `;` 写成 `\;`）：
//!
//! ```text
//! <command> ; delay=500 ; wait=PROMPT> ; timeout=5000 ; on_error=stop
//! ```
//!
//! - `delay`：命令发送后等待/匹配提示符之后再额外等待的毫秒数（默认 0）。
//! - `wait`：等待远端输出包含该文本；空 = 不等待。
//! - `timeout`：等待提示符的超时毫秒数（默认 5000，必须 > 0）。
//! - `on_error`：等待超时后的处理，`stop`（默认，停止后续脚本）或
//!   `continue`（记录警告后继续下一条）。
//!
//! 这里只解析/描述要发给**远端 shell** 的输入；任何调用方都不得把 command
//! 交给本地 shell 执行。

use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// 一条已解析的登录脚本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginStep {
    /// 要发送给远端 shell 的命令（不含结尾换行）。
    pub command: String,
    /// 等待提示符成功后额外等待的时间。
    pub delay: Duration,
    /// 等待远端输出包含的文本；`None` = 发送后不等待。
    pub wait_for: Option<String>,
    /// 等待 `wait_for` 的超时。
    pub timeout: Duration,
    /// 等待超时后是否继续执行后续脚本。
    pub continue_on_error: bool,
}

impl LoginStep {
    /// 默认策略：无延时、不等待、5s 超时、超时即停止。
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            delay: Duration::ZERO,
            wait_for: None,
            timeout: DEFAULT_TIMEOUT,
            continue_on_error: false,
        }
    }
}

/// 解析 Profile 中的脚本行；空行/全空白行忽略。
///
/// 错误信息带 1-based 行号，直接显示在 Profile 表单里。
pub fn parse_lines(lines: &[String]) -> Result<Vec<LoginStep>, String> {
    let mut steps = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        match parse_line(line) {
            Ok(step) => steps.push(step),
            Err(error) => return Err(format!("登录脚本第 {} 行: {error}", index + 1)),
        }
    }
    Ok(steps)
}

/// 解析单行脚本；空行返回错误（调用方应先过滤空行）。
pub fn parse_line(line: &str) -> Result<LoginStep, String> {
    let fields = split_fields(line);
    let command = fields.first().map(|s| s.trim()).unwrap_or_default();
    if command.is_empty() {
        return Err("命令不能为空".into());
    }
    let mut step = LoginStep::new(command);
    for field in fields.iter().skip(1) {
        let field = field.trim();
        if field.is_empty() {
            return Err("空字段（检查是否多写了 `;`）".into());
        }
        let Some((key, value)) = field.split_once('=') else {
            return Err(format!("字段应为 key=value，实际: {field}"));
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "delay" => {
                step.delay = Duration::from_millis(parse_millis(value, "delay")?);
            }
            "wait" => {
                step.wait_for = (!value.is_empty()).then(|| value.to_string());
            }
            "timeout" => {
                let ms = parse_millis(value, "timeout")?;
                if ms == 0 {
                    return Err("timeout 必须大于 0 毫秒".into());
                }
                step.timeout = Duration::from_millis(ms);
            }
            "on_error" => match value.to_ascii_lowercase().as_str() {
                "stop" => step.continue_on_error = false,
                "continue" => step.continue_on_error = true,
                other => {
                    return Err(format!("on_error 只能是 stop/continue，实际: {other}"));
                }
            },
            other => return Err(format!("未知字段: {other}")),
        }
    }
    Ok(step)
}

fn parse_millis(value: &str, key: &str) -> Result<u64, String> {
    let ms = value
        .parse::<u64>()
        .map_err(|_| format!("{key} 应为毫秒整数，实际: {value}"))?;
    if ms > 24 * 60 * 60 * 1000 {
        return Err(format!("{key} 不能超过 24 小时，实际: {ms}ms"));
    }
    Ok(ms)
}

/// 按未转义的 `;` 切分；`\;` 和 `\\` 还原为字面字符。
fn split_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(';') => current.push(';'),
                Some('\\') => current.push('\\'),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => current.push('\\'),
            },
            ';' => {
                fields.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    fields.push(current);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_only() {
        let step = parse_line("cd /tmp").unwrap();
        assert_eq!(step.command, "cd /tmp");
        assert_eq!(step.delay, Duration::ZERO);
        assert_eq!(step.wait_for, None);
        assert_eq!(step.timeout, DEFAULT_TIMEOUT);
        assert!(!step.continue_on_error);
    }

    #[test]
    fn parses_all_fields_and_escaped_semicolon() {
        let step =
            parse_line("echo a\\;b ; delay=250 ; wait=PROMPT> ; timeout=1500 ; on_error=continue")
                .unwrap();
        assert_eq!(step.command, "echo a;b");
        assert_eq!(step.delay, Duration::from_millis(250));
        assert_eq!(step.wait_for.as_deref(), Some("PROMPT>"));
        assert_eq!(step.timeout, Duration::from_millis(1500));
        assert!(step.continue_on_error);
    }

    #[test]
    fn rejects_bad_fields_with_line_number() {
        let err =
            parse_lines(&["echo ok".into(), "".into(), "echo x ; nope=1".into()]).unwrap_err();
        assert!(err.contains("第 3 行"), "{err}");
        assert!(err.contains("未知字段"), "{err}");
        assert!(parse_line("; wait=x").is_err());
        assert!(parse_line("echo x ; timeout=0").is_err());
        assert!(parse_line("echo x ; on_error=maybe").is_err());
        assert!(parse_line("echo x ; delay").is_err());
    }

    #[test]
    fn blank_lines_are_ignored() {
        let steps = parse_lines(&["".into(), "   ".into(), "whoami".into()]).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].command, "whoami");
    }
}
